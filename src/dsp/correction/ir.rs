//! S2 — IR import and conditioning.
//!
//! One code path conditions every correction IR, whether it came out of the
//! S1 sweep deconvolution (real part of the fundamental window) or was
//! imported from a WAV file (REW / Dirac / manufacturer exports). The chain
//! is control-path only:
//!
//! 1. **Rate gate** — an IR at the wrong sample rate is rejected, not
//!    silently resampled; the engine integration (S5) owns rate alignment
//!    through the existing rate machinery.
//! 2. **DC/rumble high-pass** — a single 2nd-order Butterworth biquad
//!    (reusing `dsp::biquad`, Q = 1/√2) so imported IRs with DC offsets or
//!    infrasonic rumble cannot waste correction headroom.
//! 3. **Lead trim** — leading digital silence is removed to the *earliest*
//!    onset across channels (minus a small guard), preserving inter-channel
//!    delay relationships for multiway time alignment.
//! 4. **Decay-tail truncation** — the IR is cut where the cumulative energy
//!    from onset reaches a configurable fraction of total; a short fade-out
//!    avoids a truncation step.
//! 5. **Peak normalization** — the loudest channel is scaled to the
//!    reference peak so downstream rendering starts from known headroom.

use std::path::Path;

use crate::dsp::biquad::{BiquadCoeffs, BiquadState};

use super::CorrectionError;

/// An imported, unconditioned IR: one channel per `Vec<f64>`.
#[derive(Debug, Clone)]
pub struct WavIr {
    /// Per-channel samples (any channel count; no downmixing ever happens).
    pub channels: Vec<Vec<f64>>,
    /// Sample rate declared by the file (Hz).
    pub sample_rate: f64,
}

/// A conditioned IR ready for correction derivation.
#[derive(Debug, Clone)]
pub struct ConditionedIr {
    /// Per-channel samples after conditioning.
    pub channels: Vec<Vec<f64>>,
    /// Session sample rate (Hz).
    pub sample_rate: f64,
    /// Samples trimmed from the front (identical for every channel, so
    /// inter-channel alignment is preserved).
    pub lead_trimmed: usize,
}

/// Parameters of the IR conditioning chain.
#[derive(Debug, Clone)]
pub struct IrConditioner {
    /// 2nd-order high-pass corner (Hz). Kills DC and infrasonic rumble.
    pub high_pass_hz: f64,
    /// Fraction of total (post-onset) energy the kept tail must contain.
    /// `0.999` keeps everything audible; `0.95` aggressively truncates.
    pub tail_keep_fraction: f64,
    /// Onset detection threshold relative to the global peak (dB).
    pub onset_threshold_db: f64,
    /// Target peak amplitude after normalization.
    pub normalize_peak: f64,
    /// Fade-out length at the truncation point (samples).
    pub tail_fade_samples: usize,
}

impl Default for IrConditioner {
    fn default() -> Self {
        Self {
            high_pass_hz: 10.0,
            tail_keep_fraction: 0.999,
            onset_threshold_db: -60.0,
            normalize_peak: 0.5,
            tail_fade_samples: 64,
        }
    }
}

impl IrConditioner {
    /// Condition `ir` for a session at `session_rate_hz`.
    ///
    /// # Errors
    /// * [`CorrectionError::RateMismatch`] when the IR's rate differs from
    ///   the session's.
    /// * [`CorrectionError::InvalidConfig`] for out-of-range parameters or
    ///   an IR with no channels.
    pub fn condition(
        &self,
        ir: &WavIr,
        session_rate_hz: f64,
    ) -> Result<ConditionedIr, CorrectionError> {
        if ir.channels.is_empty() {
            return Err(CorrectionError::InvalidConfig {
                what: "IR channels",
                message: "IR has no channels".into(),
            });
        }
        if (ir.sample_rate - session_rate_hz).abs() > 1.0 {
            return Err(CorrectionError::RateMismatch {
                ir_hz: ir.sample_rate,
                session_hz: session_rate_hz,
            });
        }
        if !(self.high_pass_hz.is_finite() && (0.1..=1000.0).contains(&self.high_pass_hz)) {
            return Err(CorrectionError::InvalidConfig {
                what: "high-pass corner",
                message: format!("{} Hz is outside 0.1–1000 Hz", self.high_pass_hz),
            });
        }
        if !(0.0..1.0).contains(&self.tail_keep_fraction) {
            return Err(CorrectionError::InvalidConfig {
                what: "tail keep fraction",
                message: format!("{} is outside [0, 1)", self.tail_keep_fraction),
            });
        }
        if !(self.normalize_peak.is_finite()
            && self.normalize_peak > 0.0
            && self.normalize_peak <= 1.0)
        {
            return Err(CorrectionError::InvalidConfig {
                what: "normalize peak",
                message: format!("{} is outside (0, 1]", self.normalize_peak),
            });
        }

        // Earliest onset across channels (inter-channel alignment kept).
        let onset_threshold = 10.0_f64.powf(self.onset_threshold_db / 20.0);
        let mut global_peak = 0.0_f64;
        for ch in &ir.channels {
            for &x in ch {
                global_peak = global_peak.max(x.abs());
            }
        }
        let lead = if global_peak > 0.0 {
            let mut earliest = usize::MAX;
            for ch in &ir.channels {
                if let Some(i) = ch
                    .iter()
                    .position(|&x| x.abs() > onset_threshold * global_peak)
                {
                    earliest = earliest.min(i);
                }
            }
            earliest.min(ir.channels.iter().map(|c| c.len()).max().unwrap_or(0))
        } else {
            0
        };
        let lead_trimmed = lead.saturating_sub(16);

        // Rumble/DC high-pass (2nd-order Butterworth).
        let coeffs = BiquadCoeffs::<f64>::highpass(
            session_rate_hz as f32,
            self.high_pass_hz as f32,
            std::f32::consts::FRAC_1_SQRT_2,
        );

        let mut channels = Vec::with_capacity(ir.channels.len());
        for ch in &ir.channels {
            let mut state = BiquadState::<f64>::default();
            let filtered: Vec<f64> = ch[lead_trimmed.min(ch.len())..]
                .iter()
                .map(|&x| state.process(x, &coeffs))
                .collect();
            channels.push(filtered);
        }

        // Decay-tail truncation (per channel, energy percentile from onset).
        let keep = self.tail_keep_fraction;
        let fade_n = self.tail_fade_samples;
        for ch in &mut channels {
            let peak = ch.iter().fold(0.0_f64, |m, &x| m.max(x.abs()));
            if peak <= f64::EPSILON {
                continue;
            }
            let onset = ch
                .iter()
                .position(|&x| x.abs() > onset_threshold * peak)
                .unwrap_or(0);
            let total: f64 = ch[onset..].iter().map(|x| x * x).sum();
            let mut cum = 0.0_f64;
            let mut end = ch.len();
            for (i, &x) in ch[onset..].iter().enumerate() {
                cum += x * x;
                if cum >= keep * total {
                    end = onset + i + 1;
                    break;
                }
            }
            let end = (end + fade_n).min(ch.len());
            let fade_start = end.saturating_sub(fade_n);
            let fade_len = (end - fade_start).max(1);
            for (i, s) in ch[fade_start..end].iter_mut().enumerate() {
                let d = (i + 1) as f64 / fade_len as f64;
                *s *= 0.5 * (1.0 + (std::f64::consts::PI * d).cos());
            }
            ch.truncate(end);
        }

        // Peak normalization: one scale for all channels (balance kept).
        let peak = channels
            .iter()
            .flat_map(|c| c.iter())
            .fold(0.0_f64, |m, &x| m.max(x.abs()));
        if peak > f64::EPSILON {
            let scale = self.normalize_peak / peak;
            for ch in &mut channels {
                for x in ch {
                    *x *= scale;
                }
            }
        }

        Ok(ConditionedIr {
            channels,
            sample_rate: session_rate_hz,
            lead_trimmed,
        })
    }
}

/// Read an impulse response from a RIFF/WAVE file.
///
/// Supports PCM 8/16/24/32-bit integer and 32/64-bit IEEE float, mono or any
/// channel count, including `WAVE_FORMAT_EXTENSIBLE` declarations of those
/// formats. Every channel is extracted; nothing is ever downmixed.
///
/// # Errors
/// * [`CorrectionError::WavParse`] for structural damage.
/// * [`CorrectionError::WavFormat`] for unsupported encodings.
/// * [`CorrectionError::Io`] for filesystem failures.
pub fn read_wav_ir(path: &Path) -> Result<WavIr, CorrectionError> {
    let bytes = std::fs::read(path)?;
    parse_wav(&bytes, &path.to_string_lossy())
}

/// Parse WAV bytes into per-channel f64 samples.
fn parse_wav(bytes: &[u8], path: &str) -> Result<WavIr, CorrectionError> {
    let err = |message: &str| CorrectionError::WavParse {
        path: path.to_string(),
        message: message.to_string(),
    };
    let fmt_err = |message: String| CorrectionError::WavFormat {
        path: path.to_string(),
        message,
    };

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(err("not a RIFF/WAVE container"));
    }

    let mut fmt: Option<(u16, u16, u32, u16)> = None; // format, channels, rate, bits
    let mut data: Vec<u8> = Vec::new();
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(
            bytes
                .get(pos + 4..pos + 8)
                .ok_or_else(|| err("truncated chunk header"))?
                .try_into()
                .expect("4 bytes"),
        ) as usize;
        let body_start = pos + 8;
        let body_end = body_start.saturating_add(size).min(bytes.len());
        match id {
            b"fmt " => {
                let b = &bytes[body_start..body_end];
                if b.len() < 16 {
                    return Err(err("fmt chunk shorter than 16 bytes"));
                }
                let format = u16::from_le_bytes([b[0], b[1]]);
                let channels = u16::from_le_bytes([b[2], b[3]]);
                let rate = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                let bits = u16::from_le_bytes([b[14], b[15]]);
                let mut format = format;
                if format == 0xFFFE {
                    // WAVE_FORMAT_EXTENSIBLE: cbSize ≥ 22 then a GUID whose
                    // first two bytes carry the real format code.
                    if b.len() < 40 {
                        return Err(err("extensible fmt chunk too short"));
                    }
                    let cb = u16::from_le_bytes([b[16], b[17]]);
                    if cb < 22 {
                        return Err(err("extensible fmt chunk with cbSize < 22"));
                    }
                    format = u16::from_le_bytes([b[24], b[25]]);
                }
                fmt = Some((format, channels, rate, bits));
            }
            b"data" => data.extend_from_slice(&bytes[body_start..body_end]),
            _ => {}
        }
        // Chunks are word-aligned.
        pos = body_start + size + (size & 1);
    }

    let (format, channels, rate, bits) = fmt.ok_or_else(|| err("missing fmt chunk"))?;
    if channels == 0 {
        return Err(err("zero channels"));
    }
    if rate == 0 {
        return Err(err("zero sample rate"));
    }

    let bytes_per_sample = match (format, bits) {
        (1, 8) => 1,
        (1, 16) => 2,
        (1, 24) => 3,
        (1, 32) => 4,
        (3, 32) => 4,
        (3, 64) => 8,
        (f, b) => {
            return Err(fmt_err(format!(
                "format {f} with {b}-bit samples is not supported (PCM 8/16/24/32-bit or IEEE float 32/64-bit)"
            )))
        }
    };
    let block = channels as usize * bytes_per_sample;
    let usable = data.len() - data.len() % block;

    let mut channels_out: Vec<Vec<f64>> =
        vec![Vec::with_capacity(usable / block); channels as usize];
    let mut off = 0usize;
    while off + block <= usable {
        for ch in 0..channels as usize {
            let s = &data[off + ch * bytes_per_sample..off + (ch + 1) * bytes_per_sample];
            let v = match (format, bits) {
                (1, 8) => (s[0] as i32 - 128) as f64 / 128.0,
                (1, 16) => i16::from_le_bytes([s[0], s[1]]) as f64 / 32768.0,
                (1, 24) => {
                    let v = (s[0] as i32) | ((s[1] as i32) << 8) | ((s[2] as i32) << 16);
                    let v = (v << 8) >> 8; // sign-extend 24 → 32
                    v as f64 / 8388608.0
                }
                (1, 32) => i32::from_le_bytes(s.try_into().expect("4 bytes")) as f64 / 2147483648.0,
                (3, 32) => f32::from_le_bytes(s.try_into().expect("4 bytes")) as f64,
                (3, 64) => f64::from_le_bytes(s.try_into().expect("8 bytes")),
                _ => unreachable!("format validated above"),
            };
            channels_out[ch].push(v);
        }
        off += block;
    }

    if channels_out.iter().all(|c| c.is_empty()) {
        return Err(err("data chunk contains no samples"));
    }

    Ok(WavIr {
        channels: channels_out,
        sample_rate: rate as f64,
    })
}
