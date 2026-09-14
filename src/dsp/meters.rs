//! Professional industry-grade audio metering subsystem.
//!
//! # Features
//!
//! - **Sample Peak**: Peak amplitude in dBFS per channel and max peak observed.
//! - **True Peak**: Inter-sample true-peak in dBTP via ITU-R BS.1770-4 4× FIR oversampling.
//! - **RMS**: Windowed root-mean-square level in dBFS per channel.
//! - **EBU R128 / BS.1770 LUFS**:
//!   - Momentary LUFS (400 ms sliding window).
//!   - Short-Term LUFS (3 s sliding window).
//!   - Integrated LUFS (program loudness with absolute & relative gating).
//!   - Loudness Range (LRA) in LU.
//! - **DC Offset**: Mean sample offset per channel.
//! - **Clipping Counter**: Count of samples reaching or exceeding ±1.0 (0 dBFS).
//! - **Dynamic Range / Crest Factor**: Peak-to-RMS ratio in dB.
//! - **Realtime Safe**: Rate-limited snapshots, lock-free telemetry export.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::buffer::MAX_CHANNELS;
use crate::dsp::loudness::LoudnessMeter;

/// Point-in-time snapshot of the professional metering subsystem.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProfessionalMeterSnapshot {
    /// Sample peak in dBFS for each active channel.
    pub peak_db: Vec<f32>,
    /// Maximum sample peak observed since last reset in dBFS.
    pub max_peak_db: f32,
    /// Inter-sample true-peak in dBTP for each active channel.
    pub true_peak_dbtp: Vec<f32>,
    /// Maximum true-peak observed since last reset in dBTP.
    pub max_true_peak_dbtp: f32,
    /// RMS level in dBFS for each active channel.
    pub rms_db: Vec<f32>,
    /// Dynamic range / crest factor (Peak − RMS) in dB per channel.
    pub dynamic_range_db: Vec<f32>,
    /// DC offset (linear average sample offset) per channel.
    pub dc_offset: Vec<f32>,
    /// Momentary LUFS (400 ms window).
    pub lufs_momentary: f32,
    /// Short-term LUFS (3 s window).
    pub lufs_short_term: f32,
    /// Integrated program loudness in LUFS.
    pub lufs_integrated: f32,
    /// Loudness Range (LRA) in LU.
    pub loudness_range_lu: f32,
    /// Whether `loudness_range_lu` is statistically valid.
    pub lra_valid: bool,
    /// Cumulative sample clipping count (samples reaching ±1.0).
    pub clip_count: u64,
    /// Active channel count.
    pub channels: usize,
    /// Active sample rate in Hz.
    pub sample_rate: u32,
}

struct MeterState {
    channels: usize,
    sample_rate: u32,
    loudness_meter: LoudnessMeter,
    sum_sq: [f64; MAX_CHANNELS],
    sum_samples: [f64; MAX_CHANNELS],
    peak_linear: [f32; MAX_CHANNELS],
    window_peak_linear: [f32; MAX_CHANNELS],
    window_rms_linear: [f32; MAX_CHANNELS],
    window_dc_offset: [f32; MAX_CHANNELS],
    max_peak_linear: f32,
    max_true_peak_linear: f32,
    frames_in_window: usize,
    window_size: usize,
    clip_count: u64,
}

impl MeterState {
    fn new(sample_rate: u32, channels: usize) -> Self {
        let ch = channels.clamp(1, MAX_CHANNELS);
        let sr = sample_rate.max(1);
        let window_size = (sr as f32 * 0.3) as usize; // 300 ms RMS window

        Self {
            channels: ch,
            sample_rate: sr,
            loudness_meter: LoudnessMeter::new(sr as f32, ch),
            sum_sq: [0.0; MAX_CHANNELS],
            sum_samples: [0.0; MAX_CHANNELS],
            peak_linear: [0.0; MAX_CHANNELS],
            window_peak_linear: [0.0; MAX_CHANNELS],
            window_rms_linear: [0.0; MAX_CHANNELS],
            window_dc_offset: [0.0; MAX_CHANNELS],
            max_peak_linear: 0.0,
            max_true_peak_linear: 0.0,
            frames_in_window: 0,
            window_size: window_size.max(64),
            clip_count: 0,
        }
    }

    fn reset(&mut self) {
        self.loudness_meter.reset();
        self.sum_sq = [0.0; MAX_CHANNELS];
        self.sum_samples = [0.0; MAX_CHANNELS];
        self.peak_linear = [0.0; MAX_CHANNELS];
        self.window_peak_linear = [0.0; MAX_CHANNELS];
        self.window_rms_linear = [0.0; MAX_CHANNELS];
        self.window_dc_offset = [0.0; MAX_CHANNELS];
        self.max_peak_linear = 0.0;
        self.max_true_peak_linear = 0.0;
        self.frames_in_window = 0;
        self.clip_count = 0;
    }
}

/// Unified professional metering subsystem.
pub struct ProfessionalMeters {
    enabled: AtomicBool,
    state: Mutex<MeterState>,
    latest_momentary_lufs: AtomicU32,
    latest_peak_db: AtomicU32,
    latest_true_peak_db: AtomicU32,
    total_clips: AtomicU64,
}

impl ProfessionalMeters {
    /// Create a new professional meter instance.
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            state: Mutex::new(MeterState::new(sample_rate, channels)),
            latest_momentary_lufs: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
            latest_peak_db: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
            latest_true_peak_db: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
            total_clips: AtomicU64::new(0),
        }
    }

    /// Check if professional metering is actively enabled.
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Enable or disable professional metering processing.
    #[inline]
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Reset all meters and cumulative stats.
    pub fn reset(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.reset();
        }
        self.latest_momentary_lufs
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.latest_peak_db
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.latest_true_peak_db
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.total_clips.store(0, Ordering::Relaxed);
    }

    /// Update the sample rate and channel layout when a new track loads.
    pub fn reconfigure(&self, sample_rate: u32, channels: usize) {
        if let Ok(mut state) = self.state.lock() {
            *state = MeterState::new(sample_rate, channels);
        }
    }

    /// Process a block of interleaved audio frames through the metering subsystem.
    pub fn process_interleaved(&self, interleaved: &[f32], channels: usize) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };

        let ch = channels.clamp(1, MAX_CHANNELS);
        state.channels = ch;
        let frame_count = interleaved.len() / ch;
        if frame_count == 0 {
            return;
        }

        // 1. EBU R128 loudness metering & 4x FIR true peak (BS.1770-4)
        state.loudness_meter.process_interleaved(interleaved, ch);

        // 2. Per-channel peak, RMS, DC offset, clipping
        for f in 0..frame_count {
            for c in 0..ch {
                let sample = interleaved[f * ch + c];
                let abs_sample = sample.abs();

                // Peak tracking
                if abs_sample > state.peak_linear[c] {
                    state.peak_linear[c] = abs_sample;
                }
                if abs_sample > state.max_peak_linear {
                    state.max_peak_linear = abs_sample;
                }

                // Clipping detection (sample >= 1.0)
                if abs_sample >= 1.0 {
                    state.clip_count += 1;
                    self.total_clips.fetch_add(1, Ordering::Relaxed);
                }

                // RMS and DC offset accumulators
                state.sum_sq[c] += (sample as f64) * (sample as f64);
                state.sum_samples[c] += sample as f64;
            }

            state.frames_in_window += 1;
            if state.frames_in_window >= state.window_size {
                let n = state.frames_in_window as f64;
                for c in 0..ch {
                    state.window_peak_linear[c] = state.peak_linear[c];
                    state.window_rms_linear[c] = (state.sum_sq[c] / n).max(1e-12).sqrt() as f32;
                    state.window_dc_offset[c] = (state.sum_samples[c] / n) as f32;
                    state.peak_linear[c] = 0.0;
                    state.sum_sq[c] = 0.0;
                    state.sum_samples[c] = 0.0;
                }
                state.frames_in_window = 0;
            }
        }

        // Check running true-peak maxima from the loudness meter
        let mut max_tp = state.max_true_peak_linear;
        for m in state
            .loudness_meter
            .true_peak_meters()
            .iter()
            .take(ch.min(8))
        {
            let tp = m.max_true_peak_linear() as f32;
            if tp > max_tp {
                max_tp = tp;
            }
        }
        state.max_true_peak_linear = max_tp;

        // Update atomic quick-read meters
        let max_p = state.max_peak_linear;
        let p_db = if max_p > 0.0 {
            20.0 * max_p.log10()
        } else {
            -120.0
        };
        self.latest_peak_db.store(p_db.to_bits(), Ordering::Relaxed);

        let max_tp = state.max_true_peak_linear;
        let tp_db = if max_tp > 0.0 {
            20.0 * max_tp.log10()
        } else {
            -120.0
        };
        self.latest_true_peak_db
            .store(tp_db.to_bits(), Ordering::Relaxed);
    }

    /// Capture a complete, point-in-time professional metering snapshot.
    pub fn snapshot(&self) -> ProfessionalMeterSnapshot {
        let Ok(state) = self.state.lock() else {
            return ProfessionalMeterSnapshot::default();
        };

        let ch = state.channels;

        let mut peak_db = Vec::with_capacity(ch);
        let mut true_peak_dbtp = Vec::with_capacity(ch);
        let mut rms_db = Vec::with_capacity(ch);
        let mut dynamic_range_db = Vec::with_capacity(ch);
        let mut dc_offset = Vec::with_capacity(ch);

        for c in 0..ch {
            let p = if state.frames_in_window > 0 {
                state.peak_linear[c].max(state.window_peak_linear[c])
            } else {
                state.window_peak_linear[c]
            };
            let p_db = if p > 0.0 { 20.0 * p.log10() } else { -120.0 };
            peak_db.push(p_db);

            let tp_db = if c < 8 {
                let tp = state.loudness_meter.true_peak_meters()[c].max_true_peak_linear() as f32;
                if tp > 0.0 {
                    20.0 * tp.log10()
                } else {
                    -120.0
                }
            } else {
                p_db
            };
            true_peak_dbtp.push(tp_db);

            let rms_linear = if state.frames_in_window > 0 {
                (state.sum_sq[c] / state.frames_in_window as f64)
                    .max(1e-12)
                    .sqrt() as f32
            } else {
                state.window_rms_linear[c]
            };
            let r_db = if rms_linear > 0.0 {
                20.0 * rms_linear.log10()
            } else {
                -120.0
            };
            rms_db.push(r_db);

            let dr = (p_db - r_db).max(0.0);
            dynamic_range_db.push(dr);

            let dc = if state.frames_in_window > 0 {
                (state.sum_samples[c] / state.frames_in_window as f64) as f32
            } else {
                state.window_dc_offset[c]
            };
            dc_offset.push(dc);
        }

        let max_peak_db = if state.max_peak_linear > 0.0 {
            20.0 * state.max_peak_linear.log10()
        } else {
            -120.0
        };

        let max_true_peak_dbtp = if state.max_true_peak_linear > 0.0 {
            20.0 * state.max_true_peak_linear.log10()
        } else {
            -120.0
        };

        let l_meas = state.loudness_meter.snapshot();
        let clip_count = state.clip_count;
        let sample_rate = state.sample_rate;

        ProfessionalMeterSnapshot {
            peak_db,
            max_peak_db,
            true_peak_dbtp,
            max_true_peak_dbtp,
            rms_db,
            dynamic_range_db,
            dc_offset,
            lufs_momentary: l_meas.momentary_lufs,
            lufs_short_term: l_meas.short_term_lufs,
            lufs_integrated: l_meas.integrated_lufs,
            loudness_range_lu: l_meas.lra_lu,
            lra_valid: l_meas.lra_valid,
            clip_count,
            channels: ch,
            sample_rate,
        }
    }
}

impl Default for ProfessionalMeters {
    fn default() -> Self {
        Self::new(44100, 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_professional_meters_basic() {
        let meters = ProfessionalMeters::new(44100, 2);

        // Generate 0.5 amplitude sine wave on L, 1.0 amplitude on R
        let mut buf = Vec::new();
        for i in 0..4410 {
            let t = i as f32 / 44100.0;
            let val_l = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            let val_r = 1.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            buf.push(val_l);
            buf.push(val_r);
        }

        meters.process_interleaved(&buf, 2);
        let snap = meters.snapshot();

        assert_eq!(snap.channels, 2);
        assert!(snap.peak_db[0] < -5.5 && snap.peak_db[0] > -6.5); // 0.5 ≈ -6.02 dBFS
        assert!(snap.peak_db[1] > -0.5 && snap.peak_db[1] <= 0.0); // 1.0 ≈ 0 dBFS
        assert!(snap.max_peak_db > -0.5);
        assert!(snap.rms_db[0] < snap.peak_db[0]);
    }
}
