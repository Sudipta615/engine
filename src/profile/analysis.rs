//! Deterministic, bounded-memory streaming analysis producing an
//! [`AudioProfile`].
//!
//! [`ProfileAnalyzer`] is a push-based pass: feed interleaved PCM blocks with
//! [`ProfileAnalyzer::push`], then call [`ProfileAnalyzer::finish`]. All
//! accumulators are fixed-size ring/scalar statistics — memory use is bounded
//! regardless of stream length — and the output is deterministic for the same
//! input. This is an **offline / background-thread** tool; it is never part
//! of the realtime audio callback.
//!
//! The loudness numbers come from the same [`LoudnessMeter`] used by the
//! offline scanner and the playback chain, so a profile's `integrated_lufs`
//! is definitionally identical to the engine's scan result. The spectral,
//! transient, stereo, and mid/side features are computed here from standard
//! DSP (Hann-windowed FFT power averaging, windowed RMS energy deltas, and
//! running L/R + mid/side statistics).
//!
//! [`LoudnessMeter`]: crate::dsp::LoudnessMeter

use std::path::Path;

use crate::decode::{DecodeError, Decoder};
use crate::dsp::LoudnessMeter;
use crate::profile::{
    AnalysisMask, AudioProfile, ContentProfile, DynamicCharacter, DynamicProfile, LoudnessProfile,
    MaskingProfile, ProfileError, SpatialProfile, SpectralProfile, StereoProfile, TransientProfile,
    AUDIO_PROFILE_VERSION, PROFILE_FFT_SIZE,
};

/// Window (in frames) for RMS / correlation / onset statistics (~23 ms at
/// 44.1 kHz — fine enough for onsets, coarse enough to be stable).
const WINDOW_FRAMES: usize = 1024;

/// An onset is a window whose RMS rises ≥ this factor (≈ +10 dB) over the
/// previous window.
const ONSET_RISE: f32 = 3.162_277_7; // 10^(10/20)

/// Onsets below this RMS floor (−50 dBFS) are ignored (noise floor).
const ONSET_FLOOR: f32 = 0.003_162_3; // 10^(-50/20)

/// Windows below this RMS (−60 dBFS) count as near-silence (speech pauses).
const SILENCE_FLOOR: f32 = 0.001;

/// Correlation below this flags an out-of-phase window (phase risk).
const PHASE_RISK_CORR: f32 = -0.4;

/// Stability ring length (momentary-LUFS samples, one per window ≈ 23 ms →
/// ~24 s of history at 44.1 kHz).
const STABILITY_RING: usize = 1024;

/// Streaming, bounded-memory profile analyzer.
pub struct ProfileAnalyzer {
    sample_rate: u32,
    channels: usize,
    mask: AnalysisMask,
    meter: LoudnessMeter,
    frames: u64,

    // Dynamics (mono mix).
    peak_linear: f32,
    sum_m2: f64,
    // Window accumulators (flushed every WINDOW_FRAMES).
    win_frames: usize,
    win_sum_m2: f64,
    win_sum_l2: f64,
    win_sum_r2: f64,
    win_sum_lr: f64,
    prev_win_rms: Option<f32>,
    near_silent_windows: u64,
    windows: u64,
    onsets: u64,
    onset_excess_db: f64,

    // Spectral accumulator.
    mono_recent: Vec<f32>,
    rfft: Option<std::sync::Arc<dyn realfft::RealToComplex<f32>>>,
    window: Vec<f32>,
    scratch_input: Vec<f32>,
    scratch_spec: Vec<realfft::num_complex::Complex32>,
    spec_energy: Vec<f64>,
    spec_windows: u64,

    // Stereo / spatial (whole-track running sums).
    sum_l2: f64,
    sum_r2: f64,
    sum_lr: f64,
    sum_mid2: f64,
    sum_side2: f64,
    neg_corr_windows: u64,

    // Loudness stability ring.
    stability_ring: Vec<f32>,
    stability_idx: usize,
    stability_filled: usize,
    last_momentary: Option<f32>,
}

impl ProfileAnalyzer {
    /// Create an analyzer for `sample_rate` Hz / `channels` channels with
    /// every sub-profile enabled.
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self::new_with_mask(sample_rate, channels, AnalysisMask::all())
    }

    /// Create an analyzer computing only the sub-profiles in `mask`.
    pub fn new_with_mask(sample_rate: u32, channels: usize, mask: AnalysisMask) -> Self {
        let meter = LoudnessMeter::new(sample_rate as f32, channels);
        let mut planner = realfft::RealFftPlanner::<f32>::new();
        let rfft = planner.plan_fft_forward(PROFILE_FFT_SIZE);
        let window: Vec<f32> = (0..PROFILE_FFT_SIZE)
            .map(|i| {
                0.5 - 0.5
                    * ((2.0 * std::f32::consts::PI * i as f32) / PROFILE_FFT_SIZE as f32).cos()
            })
            .collect();
        Self {
            sample_rate,
            channels,
            mask,
            meter,
            frames: 0,
            peak_linear: 0.0,
            sum_m2: 0.0,
            win_frames: 0,
            win_sum_m2: 0.0,
            win_sum_l2: 0.0,
            win_sum_r2: 0.0,
            win_sum_lr: 0.0,
            prev_win_rms: None,
            near_silent_windows: 0,
            windows: 0,
            onsets: 0,
            onset_excess_db: 0.0,
            mono_recent: Vec::with_capacity(PROFILE_FFT_SIZE + WINDOW_FRAMES),
            rfft: Some(rfft),
            window,
            scratch_input: vec![0.0; PROFILE_FFT_SIZE],
            scratch_spec: vec![
                realfft::num_complex::Complex32::new(0.0, 0.0);
                PROFILE_FFT_SIZE / 2 + 1
            ],
            spec_energy: vec![0.0; PROFILE_FFT_SIZE / 2 + 1],
            spec_windows: 0,
            sum_l2: 0.0,
            sum_r2: 0.0,
            sum_lr: 0.0,
            sum_mid2: 0.0,
            sum_side2: 0.0,
            neg_corr_windows: 0,
            stability_ring: vec![0.0; STABILITY_RING],
            stability_idx: 0,
            stability_filled: 0,
            last_momentary: None,
        }
    }

    /// Set the semantic channel layout for the BS.1770-4 loudness weights
    /// (mirror [`crate::dsp::LoudnessMeter::set_channel_layout`]).
    pub fn set_channel_layout(&mut self, layout: &crate::decode::ChannelLayout) {
        self.meter.set_channel_layout(layout);
    }

    /// Feed one interleaved PCM block. `channels` ≥ 1; channel 0/1 are the
    /// L/R pair for stereo/spatial features.
    pub fn push(&mut self, samples: &[f32], channels: usize) {
        if samples.is_empty() {
            return;
        }
        let ch = channels.max(1);
        // Feed the loudness meter in 100 ms sub-blocks so the momentary
        // loudness (and the stability ring sampled from it) advances as the
        // stream progresses rather than only reflecting the block's final
        // state — the meter's BS.1770 hop is 100 ms.
        let hop_frames = ((self.sample_rate as usize) / 10).max(1);
        let hop_len = (hop_frames * ch).max(1);
        for sub in samples.chunks(hop_len) {
            self.meter.process_interleaved(sub, channels);
            self.process_frames(sub, channels);
        }
        if self.mask.spectral {
            while self.mono_recent.len() >= PROFILE_FFT_SIZE {
                self.compute_spectral_window();
            }
        }
    }

    /// Per-frame accumulators + per-window statistics for one sub-block.
    fn process_frames(&mut self, samples: &[f32], channels: usize) {
        let ch = channels.max(1);
        for frame in samples.chunks_exact(ch) {
            let l = frame[0];
            let r = if ch > 1 { frame[1] } else { l };
            let m = (l + r) * 0.5;
            let ml = m.abs();
            if ml > self.peak_linear {
                self.peak_linear = ml;
            }
            self.sum_m2 += (m as f64) * (m as f64);
            self.win_sum_m2 += (m as f64) * (m as f64);
            if ch > 1 {
                let lf = l as f64;
                let rf = r as f64;
                self.sum_l2 += lf * lf;
                self.sum_r2 += rf * rf;
                self.sum_lr += lf * rf;
                self.win_sum_l2 += lf * lf;
                self.win_sum_r2 += rf * rf;
                self.win_sum_lr += lf * rf;
                let mid = (lf + rf) * 0.5;
                let side = (lf - rf) * 0.5;
                self.sum_mid2 += mid * mid;
                self.sum_side2 += side * side;
            }
            if self.mask.spectral {
                self.mono_recent.push(m);
            }
            self.frames += 1;
            self.win_frames += 1;
            if self.win_frames >= WINDOW_FRAMES {
                self.flush_window(ch > 1);
            }
        }
    }

    /// Finish analysis and assemble the [`AudioProfile`].
    pub fn finish(mut self) -> AudioProfile {
        if self.win_frames > 0 {
            self.flush_window(self.channels > 1);
        }
        let duration_secs = if self.sample_rate > 0 {
            self.frames as f32 / self.sample_rate as f32
        } else {
            0.0
        };
        let has_audio = self.frames > 0 && duration_secs > 0.0;

        let loudness = if self.mask.loudness {
            let m = self.meter.snapshot();
            let integrated = if m.integrated_lufs.is_finite() {
                Some(m.integrated_lufs)
            } else {
                None
            };
            let short_term = if m.short_term_lufs.is_finite() {
                Some(m.short_term_lufs)
            } else {
                None
            };
            let true_peak = if m.true_peak_linear > 0.0 {
                Some(m.true_peak_dbtp())
            } else {
                None
            };
            let lra = if m.lra_valid && m.lra_lu.is_finite() && m.lra_lu > 0.0 {
                Some(m.lra_lu)
            } else {
                None
            };
            let stability = if self.stability_filled >= 8 {
                let (_, std) = ring_mean_std(&self.stability_ring, self.stability_filled);
                Some((1.0 - (std / 12.0)).clamp(0.0, 1.0))
            } else {
                None
            };
            LoudnessProfile {
                integrated_lufs: integrated,
                short_term_lufs: short_term,
                true_peak_dbtp: true_peak,
                loudness_range_lu: lra,
                stability,
            }
        } else {
            LoudnessProfile::default()
        };

        let crest_db = if self.mask.dynamics && has_audio {
            let rms = (self.sum_m2 / self.frames as f64).sqrt() as f32;
            let peak = self.peak_linear.min(1.0);
            if rms > 1e-9 {
                Some(20.0 * (peak / rms.max(1e-9)).log10().max(0.0))
            } else {
                None
            }
        } else {
            None
        };
        let dynamics = if self.mask.dynamics {
            let lra = loudness.loudness_range_lu;
            let character = match (crest_db, lra) {
                (Some(c), Some(l)) => {
                    if c >= 16.0 || l >= 14.0 {
                        DynamicCharacter::Dynamic
                    } else if c <= 8.0 || l <= 5.0 {
                        DynamicCharacter::Compressed
                    } else {
                        DynamicCharacter::Moderate
                    }
                }
                (Some(c), None) => {
                    if c >= 16.0 {
                        DynamicCharacter::Dynamic
                    } else if c <= 8.0 {
                        DynamicCharacter::Compressed
                    } else {
                        DynamicCharacter::Moderate
                    }
                }
                (None, Some(l)) => {
                    if l >= 14.0 {
                        DynamicCharacter::Dynamic
                    } else if l <= 5.0 {
                        DynamicCharacter::Compressed
                    } else {
                        DynamicCharacter::Moderate
                    }
                }
                (None, None) => DynamicCharacter::Unknown,
            };
            let compression = lra
                .map(|l| (1.0 - (l / 18.0)).clamp(0.0, 1.0))
                .or_else(|| crest_db.map(|c| (1.0 - (c / 16.0)).clamp(0.0, 1.0)));
            DynamicProfile {
                crest_factor_db: crest_db,
                dynamic_range_db: lra,
                character,
                compression,
            }
        } else {
            DynamicProfile::default()
        };

        let spectral = if self.mask.spectral && self.spec_windows > 0 {
            spectral_features(&self.spec_energy, self.spec_windows, self.sample_rate)
        } else {
            SpectralProfile::default()
        };

        let (density_per_sec, strength) = if self.mask.transient && has_audio {
            let density = self.onsets as f32 / duration_secs;
            let strength = if self.onsets > 0 {
                ((self.onset_excess_db / self.onsets as f64) / 20.0) as f32
            } else {
                0.0
            };
            (Some(density), Some(strength.clamp(0.0, 1.0)))
        } else {
            (None, None)
        };
        let transient = TransientProfile {
            density_per_sec,
            strength,
        };

        let stereo = if self.mask.stereo && self.channels > 1 && self.frames > 0 {
            let corr = if self.sum_l2 > 0.0 && self.sum_r2 > 0.0 {
                Some((self.sum_lr / (self.sum_l2 * self.sum_r2).sqrt()) as f32)
            } else {
                None
            };
            let width = corr.map(|c| (1.0 - c.abs()).clamp(0.0, 1.0));
            let balance = if self.sum_l2 > 0.0 && self.sum_r2 > 0.0 {
                Some((10.0 * (self.sum_l2 / self.sum_r2).log10()) as f32)
            } else {
                None
            };
            let phase_risk = if self.windows > 0 {
                Some((self.neg_corr_windows as f32 / self.windows as f32).clamp(0.0, 1.0))
            } else {
                None
            };
            StereoProfile {
                correlation: corr,
                width,
                balance_db: balance,
                phase_risk,
            }
        } else {
            StereoProfile::default()
        };

        let spatial = if self.mask.spatial && self.channels > 1 && self.frames > 0 {
            let total = self.sum_mid2 + self.sum_side2;
            let side_fraction = if total > 0.0 {
                Some((self.sum_side2 / total) as f32)
            } else {
                None
            };
            let ambience = if self.sum_mid2 > 0.0 {
                Some((10.0 * (self.sum_side2 / self.sum_mid2).log10()) as f32)
            } else {
                None
            };
            SpatialProfile {
                side_fraction,
                ambience_db: ambience,
            }
        } else {
            SpatialProfile::default()
        };

        let content = if self.mask.content {
            content_profile(
                spectral.flatness,
                density_per_sec,
                stereo.width,
                self.near_silent_windows as f32 / self.windows.max(1) as f32,
                spectral.centroid_hz,
                loudness.loudness_range_lu,
                crest_db,
            )
        } else {
            ContentProfile::default()
        };

        // Coverage: fraction of requested sub-profiles that produced data.
        let requested = [
            (self.mask.loudness, loudness.integrated_lufs.is_some()),
            (self.mask.dynamics, dynamics.crest_factor_db.is_some()),
            (self.mask.spectral, spectral.centroid_hz.is_some()),
            (self.mask.transient, transient.density_per_sec.is_some()),
            (self.mask.stereo, stereo.correlation.is_some()),
            (self.mask.spatial, spatial.side_fraction.is_some()),
            (self.mask.content, content.evidence),
        ];
        let (want, got) = requested
            .iter()
            .fold((0usize, 0usize), |(w, g), &(on, valid)| {
                (w + usize::from(on), g + usize::from(on && valid))
            });
        let coverage = if want > 0 {
            got as f32 / want as f32
        } else {
            0.0
        };
        let confidence = AudioProfile::compute_confidence(duration_secs, coverage);

        AudioProfile {
            version: AUDIO_PROFILE_VERSION,
            sample_rate: self.sample_rate,
            channels: self.channels.min(255) as u8,
            duration_secs,
            mask: self.mask,
            loudness,
            dynamics,
            spectral,
            transient,
            stereo,
            spatial,
            content,
            confidence,
        }
    }

    /// Flush one 1024-frame statistics window (RMS, onset, correlation,
    /// momentary-loudness stability sample).
    fn flush_window(&mut self, has_r: bool) {
        let n = self.win_frames.max(1) as f64;
        let rms = (self.win_sum_m2 / n).sqrt() as f32;
        self.windows += 1;
        if rms < SILENCE_FLOOR {
            self.near_silent_windows += 1;
        }
        if let Some(prev) = self.prev_win_rms {
            if rms > prev * ONSET_RISE && rms >= ONSET_FLOOR {
                self.onsets += 1;
                self.onset_excess_db += 20.0 * (rms / prev.max(1e-9)).log10() as f64;
            }
        }
        self.prev_win_rms = Some(rms);

        if has_r && self.win_sum_l2 > 0.0 && self.win_sum_r2 > 0.0 {
            let corr = (self.win_sum_lr / (self.win_sum_l2 * self.win_sum_r2).sqrt()) as f32;
            if corr < PHASE_RISK_CORR {
                self.neg_corr_windows += 1;
            }
        }

        // Sample momentary loudness once per hop (dedup identical values).
        let momentary = self.meter.snapshot().momentary_lufs;
        if momentary.is_finite() && self.last_momentary != Some(momentary) {
            self.last_momentary = Some(momentary);
            let idx = self.stability_idx % STABILITY_RING;
            self.stability_ring[idx] = momentary;
            self.stability_idx += 1;
            self.stability_filled = self.stability_filled.min(STABILITY_RING) + 1;
        }

        self.win_frames = 0;
        self.win_sum_m2 = 0.0;
        self.win_sum_l2 = 0.0;
        self.win_sum_r2 = 0.0;
        self.win_sum_lr = 0.0;
    }

    /// One Hann-windowed FFT of the most recent `PROFILE_FFT_SIZE` mono
    /// samples; accumulate power into `spec_energy` (50% overlap).
    fn compute_spectral_window(&mut self) {
        let Some(rfft) = self.rfft.as_mut() else {
            return;
        };
        let n = PROFILE_FFT_SIZE;
        let len = self.mono_recent.len();
        for i in 0..n {
            self.scratch_input[i] = self.mono_recent[len - n + i] * self.window[i];
        }
        if rfft
            .process(&mut self.scratch_input, &mut self.scratch_spec)
            .is_err()
        {
            return;
        }
        for (acc, c) in self.spec_energy.iter_mut().zip(self.scratch_spec.iter()) {
            *acc += c.norm_sqr() as f64;
        }
        self.spec_windows += 1;
        self.mono_recent.drain(..n / 2);
    }
}

/// Mean and population standard deviation of the first `filled` ring entries.
fn ring_mean_std(ring: &[f32], filled: usize) -> (f32, f32) {
    let n = filled.max(1) as f64;
    let mean = ring.iter().take(filled).map(|&v| v as f64).sum::<f64>() / n;
    let var = ring
        .iter()
        .take(filled)
        .map(|&v| (v as f64 - mean) * (v as f64 - mean))
        .sum::<f64>()
        / n;
    (mean as f32, var.sqrt() as f32)
}

/// Derive spectral features from an averaged power spectrum.
fn spectral_features(spec_energy: &[f64], windows: u64, sample_rate: u32) -> SpectralProfile {
    let n = spec_energy.len().saturating_sub(1);
    if n == 0 || windows == 0 || sample_rate == 0 {
        return SpectralProfile::default();
    }
    let inv = 1.0 / windows as f64;
    // Averaged power per bin.
    let p: Vec<f64> = spec_energy.iter().map(|e| (e * inv).max(1e-12)).collect();
    let fbin = sample_rate as f64 / PROFILE_FFT_SIZE as f64;

    // Centroid.
    let total: f64 = p.iter().skip(1).sum();
    let centroid = if total > 0.0 {
        let num: f64 = (1..=n).map(|i| i as f64 * fbin * p[i]).sum();
        Some((num / total) as f32)
    } else {
        None
    };

    // Rolloff (85% cumulative).
    let rolloff = if total > 0.0 {
        let mut acc = 0.0f64;
        let mut ro = None;
        for (i, &pi) in p.iter().enumerate().skip(1).take(n) {
            acc += pi;
            if acc >= 0.85 * total {
                ro = Some((i as f64 * fbin) as f32);
                break;
            }
        }
        ro
    } else {
        None
    };

    // Flatness (geometric/arithmetic mean over bins 1..=n).
    let flatness = {
        let log_sum: f64 = p.iter().skip(1).map(|&v| v.ln()).sum();
        let m = n as f64;
        let geom = (log_sum / m).exp();
        let arith = total / m;
        Some((geom / arith.max(1e-12)).clamp(0.0, 1.0) as f32)
    };

    // Brightness: 10·log10(E 2k–10k / E 20–200).
    let brightness = {
        let mut lo = 0.0f64; // 20–200 Hz
        let mut hi = 0.0f64; // 2k–10k Hz
        for (i, &pi) in p.iter().enumerate().skip(1).take(n) {
            let f = i as f64 * fbin;
            if (20.0..=200.0).contains(&f) {
                lo += pi;
            } else if (2_000.0..=10_000.0).contains(&f) {
                hi += pi;
            }
        }
        if lo > 0.0 {
            Some((10.0 * (hi / lo).log10()) as f32)
        } else {
            None
        }
    };

    // Spectral slope (dB/oct) by least-squares fit over 100 Hz–10 kHz.
    let slope = {
        let mut sx = 0.0f64;
        let mut sy = 0.0f64;
        let mut sxy = 0.0f64;
        let mut sxx = 0.0f64;
        let mut cnt = 0.0f64;
        for (i, &pi) in p.iter().enumerate().skip(1).take(n) {
            let f = i as f64 * fbin;
            if (100.0..=10_000.0).contains(&f) {
                let x = f.log2();
                let y = 10.0 * pi.log10();
                sx += x;
                sy += y;
                sxy += x * y;
                sxx += x * x;
                cnt += 1.0;
            }
        }
        if cnt >= 4.0 {
            let denom = cnt * sxx - sx * sx;
            if denom.abs() > 1e-12 {
                Some(((cnt * sxy - sx * sy) / denom) as f32)
            } else {
                None
            }
        } else {
            None
        }
    };

    SpectralProfile {
        centroid_hz: centroid,
        rolloff_hz: rolloff,
        slope_db_per_octave: slope,
        flatness,
        brightness_db: brightness,
    }
}

/// Deterministic heuristic content classification. Each evidence input votes
/// 0..1 into speech/music/ambient; votes are summed and normalized. When no
/// evidence exists the neutral ⅓/⅓/⅓ prior is returned with
/// `evidence = false`.
#[allow(clippy::too_many_arguments)]
fn content_profile(
    flatness: Option<f32>,
    density: Option<f32>,
    width: Option<f32>,
    silence_frac: f32,
    centroid_hz: Option<f32>,
    lra: Option<f32>,
    crest_db: Option<f32>,
) -> ContentProfile {
    let mut speech = 0.0f32;
    let mut music = 0.0f32;
    let mut ambient = 0.0f32;
    let mut evidence = false;

    if let Some(f) = flatness {
        evidence = true;
        ambient += f; // noise-like → ambient
        music += (1.0 - f) * 0.8; // tonal → music
    }
    if let Some(d) = density {
        evidence = true;
        if d < 0.5 {
            ambient += 0.6; // sparse onsets → ambient/room tone
        } else if d < 6.0 {
            speech += 0.5; // bursty onsets → speech-like
        } else {
            music += 0.4; // dense onsets → rhythmic music
        }
    }
    if let Some(w) = width {
        evidence = true;
        ambient += w * 0.4; // wide/decorrelated → ambience
        music += (1.0 - w) * 0.3;
        speech += (1.0 - w) * 0.2;
    }
    if silence_frac > 0.02 {
        evidence = true;
        speech += silence_frac * 1.2; // pauses → speech
    }
    if let Some(c) = centroid_hz {
        evidence = true;
        if c > 2_500.0 {
            speech += 0.3; // high centroid → speech formants
        } else if c < 1_500.0 {
            music += 0.3; // low centroid → music bass/body
        }
    }
    if let Some(l) = lra {
        evidence = true;
        if l > 3.0 && l < 12.0 {
            speech += 0.3; // moderate LRA → speech
        } else if l >= 12.0 {
            music += 0.3; // wide LRA → music
        }
    }

    let tonal_density = flatness.map(|f| (1.0 - f).clamp(0.0, 1.0));
    let dynamic_risk = crest_db.map(|c| (c / 20.0).clamp(0.0, 1.0));

    if !evidence {
        return ContentProfile {
            speech: 1.0 / 3.0,
            music: 1.0 / 3.0,
            ambient: 1.0 / 3.0,
            masking: MaskingProfile {
                tonal_density,
                dynamic_risk,
            },
            evidence: false,
        };
    }

    let total = (speech + music + ambient).max(1e-6);
    ContentProfile {
        speech: speech / total,
        music: music / total,
        ambient: ambient / total,
        masking: MaskingProfile {
            tonal_density,
            dynamic_risk,
        },
        evidence: true,
    }
}

/// Decode `decoder` end to end and profile it (offline; never the realtime
/// path). Uses the same [`LoudnessMeter`] and semantic layout handling as
/// [`crate::decode::scan_decoder`].
pub fn analyze_decoder(
    decoder: &mut Decoder,
    mask: AnalysisMask,
) -> Result<AudioProfile, ProfileError> {
    let info = decoder.info();
    let mut analyzer = ProfileAnalyzer::new_with_mask(info.sample_rate, info.channels.max(1), mask);
    const CHUNK_FRAMES: usize = 8192;
    loop {
        match decoder.decode_next(CHUNK_FRAMES) {
            Ok(chunk) => {
                if chunk.samples.is_empty() {
                    // Native-DSD transport chunks carry no PCM samples.
                    continue;
                }
                analyzer.set_channel_layout(&chunk.channel_layout);
                analyzer.push(&chunk.samples, chunk.channels.max(1));
            }
            Err(DecodeError::EndOfStream) => break,
            Err(_) => break,
        }
    }
    let profile = analyzer.finish();
    if profile.duration_secs <= 0.0 {
        return Err(ProfileError::NoAudio);
    }
    Ok(profile)
}

/// Open `path` and profile it end to end.
pub fn analyze_path(path: &Path, mask: AnalysisMask) -> Result<AudioProfile, ProfileError> {
    let mut decoder = Decoder::open(path).map_err(ProfileError::from)?;
    analyze_decoder(&mut decoder, mask)
}

/// Profile `path`, consulting the on-disk cache first (validated against the
/// file's size/mtime and the profile schema version). A cache hit avoids the
/// full decode entirely.
pub fn analyze_path_cached(path: &Path, mask: AnalysisMask) -> Result<AudioProfile, ProfileError> {
    if let Some(cached) = crate::profile::cache::lookup(path) {
        return Ok(cached);
    }
    let profile = analyze_path(path, mask)?;
    crate::profile::cache::store(path, &profile);
    Ok(profile)
}

/// Profile `path`, deduplicating across identical content via the content
/// fingerprint (`fingerprint` feature): a hit on the fingerprint key avoids
/// the decode even when the file lives at a different path or was re-tagged.
///
/// Without the `fingerprint` feature this degrades to
/// [`analyze_path_cached`] (size/mtime keyed). Computing a fingerprint is
/// itself a full decode, so prefer [`analyze_path_cached`] when you only
/// have one canonical file per track.
pub fn analyze_path_cached_by_fingerprint(
    path: &Path,
    mask: AnalysisMask,
) -> Result<AudioProfile, ProfileError> {
    #[cfg(feature = "fingerprint")]
    {
        if let Ok(fp) = crate::decode::extract_fingerprint(path) {
            let id = crate::decode::fingerprint_to_hex(&fp.data);
            if let Some(cached) = crate::profile::cache::lookup_for_id(&id) {
                return Ok(cached);
            }
            let profile = analyze_path(path, mask)?;
            crate::profile::cache::store_with_id(path, &profile, Some(&id));
            return Ok(profile);
        }
    }
    analyze_path_cached(path, mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A deterministic sine `freq` Hz at `amp` (amplitude), `n` frames,
    /// `channels` (1 = mono, 2 = stereo with same signal both channels).
    fn sine(channels: usize, sample_rate: u32, n: usize, freq: f32, amp: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(n * channels);
        for i in 0..n {
            let t = i as f32 / sample_rate as f32;
            let s = amp * (2.0 * std::f32::consts::PI * freq * t).sin();
            for _ in 0..channels {
                out.push(s);
            }
        }
        out
    }

    fn white_noise(channels: usize, n: usize) -> Vec<f32> {
        // Deterministic LCG noise, ±amp.
        let mut seed = 0x9E37_79B9u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let mut out = Vec::with_capacity(n * channels);
        for _ in 0..n {
            let v = next() * 0.3;
            for _ in 0..channels {
                out.push(v);
            }
        }
        out
    }

    #[test]
    fn sine_is_tonal_music_like() {
        let sr: u32 = 44_100;
        let n: usize = (sr as usize) * 6;
        let mut a = ProfileAnalyzer::new(sr, 2);
        let buf = sine(2, sr, n, 440.0, 0.5);
        a.push(&buf, 2);
        let p = a.finish();
        let flat = p.spectral.flatness.unwrap();
        assert!(flat < 0.05, "tone should be tonal, flatness = {flat}");
        assert!(p.content.music > p.content.speech && p.content.music > p.content.ambient);
        assert!(p.content.evidence);
        assert!(p.stereo.correlation.unwrap() > 0.99);
        assert!(p.stereo.width.unwrap() < 0.01);
        assert!(p.loudness.integrated_lufs.is_some());
        assert!(
            p.dynamics.crest_factor_db.unwrap() > 2.0,
            "sine crest ≈ 3 dB"
        );
    }

    #[test]
    fn noise_is_ambient_like_and_flat() {
        let sr: u32 = 44_100;
        let n: usize = (sr as usize) * 6;
        let mut a = ProfileAnalyzer::new(sr, 2);
        a.push(&white_noise(2, n), 2);
        let p = a.finish();
        let flat = p.spectral.flatness.unwrap();
        assert!(flat > 0.5, "noise should be flat, flatness = {flat}");
        assert!(p.content.ambient > p.content.music);
        assert!(
            p.transient.density_per_sec.unwrap() < 1.0,
            "noise has no onsets"
        );
        assert!(
            p.loudness.stability.unwrap() > 0.8,
            "steady noise is stable"
        );
    }

    #[test]
    fn decorrelated_stereo_is_wide() {
        let sr: u32 = 44_100;
        let n: usize = (sr as usize) * 6;
        // L = 440 Hz, R = 997 Hz (unrelated tones → decorrelated).
        let l: Vec<f32> = (0..n)
            .map(|i| 0.4 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin())
            .collect();
        let r: Vec<f32> = (0..n)
            .map(|i| 0.4 * (2.0 * std::f32::consts::PI * 997.0 * i as f32 / sr as f32).sin())
            .collect();
        let mut inter = Vec::with_capacity(n * 2);
        for i in 0..n {
            inter.push(l[i]);
            inter.push(r[i]);
        }
        let mut a = ProfileAnalyzer::new(sr, 2);
        a.push(&inter, 2);
        let p = a.finish();
        let corr = p.stereo.correlation.unwrap();
        assert!(
            corr.abs() < 0.3,
            "unrelated tones → low correlation, got {corr}"
        );
        assert!(p.stereo.width.unwrap() > 0.6);
        assert!(p.spatial.side_fraction.unwrap() > 0.3);
    }

    #[test]
    fn mono_input_reports_no_stereo_spatial() {
        let sr: u32 = 44_100;
        let mut a = ProfileAnalyzer::new(sr, 1);
        a.push(&sine(1, sr, (sr as usize) * 3, 220.0, 0.4), 1);
        let p = a.finish();
        assert_eq!(p.stereo.correlation, None);
        assert_eq!(p.spatial.side_fraction, None);
        assert!(p.spectral.centroid_hz.is_some());
    }

    #[test]
    fn confidence_grows_with_duration() {
        let sr: u32 = 44_100;
        let one_sec: usize = sr as usize;
        let mut short = ProfileAnalyzer::new(sr, 2);
        short.push(&sine(2, sr, one_sec, 440.0, 0.4), 2); // 1 s
        let ps = short.finish();

        let mut long = ProfileAnalyzer::new(sr, 2);
        for _ in 0..32 {
            long.push(&sine(2, sr, one_sec, 440.0, 0.4), 2); // 32 s
        }
        let pl = long.finish();
        assert!(pl.confidence > ps.confidence);
        assert_eq!(pl.confidence, 1.0, "32 s with full coverage → 1.0");
    }

    #[test]
    fn mask_skips_unrequested_profiles() {
        let sr: u32 = 44_100;
        // Everything off except loudness+spectral.
        let mask = AnalysisMask {
            loudness: true,
            spectral: true,
            dynamics: false,
            transient: false,
            stereo: false,
            spatial: false,
            content: false,
        };
        let mut a = ProfileAnalyzer::new_with_mask(sr, 2, mask);
        a.push(&sine(2, sr, (sr as usize) * 4, 440.0, 0.5), 2);
        let p = a.finish();
        assert!(p.loudness.integrated_lufs.is_some());
        assert!(p.spectral.centroid_hz.is_some());
        assert_eq!(p.dynamics.crest_factor_db, None);
        assert_eq!(p.stereo.correlation, None);
        assert_eq!(p.spatial.side_fraction, None);
        assert!(!p.content.evidence);
        assert!(p.confidence < 1.0, "coverage penalty applies");
    }

    #[test]
    fn analyze_path_round_trips_a_wav() {
        let dir = std::env::temp_dir().join("shadow_profile_test");
        std::fs::create_dir_all(&dir).ok();
        let path: std::path::PathBuf = dir.join("tone.wav");
        write_sine_wav(&path, 44_100, 2, 440.0, 0.4);
        let p = analyze_path(&path, AnalysisMask::all()).expect("profile");
        assert_eq!(p.channels, 2);
        assert!(p.spectral.centroid_hz.unwrap() > 300.0);
        assert!(p.content.music > p.content.ambient);
        let _ = std::fs::remove_file(&path);
    }

    /// Write a stereo 16-bit PCM WAV (mirrors the scanner test helper).
    fn write_sine_wav(path: &Path, sample_rate: u32, seconds: usize, freq: f32, amplitude: f32) {
        use std::io::Write;
        let n_frames = sample_rate as usize * seconds;
        let mut data = Vec::with_capacity(n_frames * 2 * 2);
        for i in 0..n_frames {
            let s = amplitude
                * (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin();
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut f = std::fs::File::create(path).unwrap();
        let riff_len = 36 + data.len() as u32;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&riff_len.to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&2u16.to_le_bytes()).unwrap(); // stereo
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&(sample_rate * 4).to_le_bytes()).unwrap(); // byte rate
        f.write_all(&4u16.to_le_bytes()).unwrap(); // block align
        f.write_all(&16u16.to_le_bytes()).unwrap(); // bits
        f.write_all(b"data").unwrap();
        f.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&data).unwrap();
    }
}
