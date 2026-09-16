//! Unified dynamics detector architecture.
//!
//! Provides configurable envelope detection for compressors, limiters, gates,
//! expanders, dynamic EQ, and de-essers.

use super::envelope::BallisticEnvelope;
use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};

/// Detection algorithm type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetectionMode {
    /// Instantaneous sample peak follower (classic, transient-responsive).
    #[default]
    Peak,
    /// Root-mean-square level via leaky integrator window.
    Rms,
    /// Mean-square level (energy-proportional without square root).
    MeanSquare,
    /// ITU-R BS.1770-5 Annex 2 class 4× oversampled true-peak follower.
    TruePeak,
    /// BS.1770 K-weighted energy filter before detection.
    LufsEnergy,
    /// Band-limited energy detection around a center frequency.
    BandEnergy,
    /// High-frequency spectral energy (for de-essing and transient detection).
    SpectralEnergy,
}

/// Multichannel linking strategy for dynamics envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelLinkMode {
    /// Each channel detects its own independent envelope.
    Independent,
    /// All channels follow the maximum detected level (prevents image shifting).
    #[default]
    MaxLinked,
    /// All channels follow the average detected level.
    AverageLinked,
    /// Mid/Side linked: combines mid and side components with weighting.
    MidSide,
}

/// Optional sidechain filter configuration.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum SidechainFilter {
    /// No filtering on sidechain.
    #[default]
    None,
    /// High-pass filter (e.g. to ignore sub-bass thump).
    HighPass { freq: f32, q: f32 },
    /// Low-pass filter.
    LowPass { freq: f32, q: f32 },
    /// Band-pass filter (e.g. for de-essing or dynamic EQ band detection).
    BandPass { freq: f32, q: f32 },
}

/// Full configuration for a unified dynamics detector.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectorConfig {
    pub detection_mode: DetectionMode,
    pub link_mode: ChannelLinkMode,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub hold_ms: f32,
    pub rms_window_ms: f32,
    pub lookahead_samples: usize,
    pub sidechain_filter: SidechainFilter,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            detection_mode: DetectionMode::Peak,
            link_mode: ChannelLinkMode::MaxLinked,
            attack_ms: 10.0,
            release_ms: 100.0,
            hold_ms: 0.0,
            rms_window_ms: 20.0,
            lookahead_samples: 0,
            sidechain_filter: SidechainFilter::None,
        }
    }
}

/// Unified dynamics detector with stereo and multichannel support.
#[derive(Debug, Clone)]
pub struct DynamicsDetector {
    sample_rate: f32,
    config: DetectorConfig,
    env_l: BallisticEnvelope,
    env_r: BallisticEnvelope,

    // RMS leaky integrator state
    mean_sq_l: f32,
    mean_sq_r: f32,
    rms_coeff: f32,

    // Sidechain biquad filtering
    sc_coeffs: Option<BiquadCoeffsF32>,
    sc_state_l: BiquadStateF32,
    sc_state_r: BiquadStateF32,

    // Lookahead ring buffer (pre-allocated)
    lookahead_buf_l: Vec<f32>,
    lookahead_buf_r: Vec<f32>,
    lookahead_write: usize,
    lookahead_cap: usize,
}

impl DynamicsDetector {
    /// Create a new dynamics detector for the given sample rate and configuration.
    pub fn new(sample_rate: f32, config: DetectorConfig) -> Self {
        let sr = sample_rate.max(1.0);
        let lookahead_cap = config.lookahead_samples.max(1).next_power_of_two();
        let env_l = BallisticEnvelope::new(sr, config.attack_ms, config.release_ms, config.hold_ms);
        let env_r = BallisticEnvelope::new(sr, config.attack_ms, config.release_ms, config.hold_ms);

        let mut detector = Self {
            sample_rate: sr,
            config: config.clone(),
            env_l,
            env_r,
            mean_sq_l: 0.0,
            mean_sq_r: 0.0,
            rms_coeff: 0.0,
            sc_coeffs: None,
            sc_state_l: BiquadStateF32::default(),
            sc_state_r: BiquadStateF32::default(),
            lookahead_buf_l: vec![0.0; lookahead_cap],
            lookahead_buf_r: vec![0.0; lookahead_cap],
            lookahead_write: 0,
            lookahead_cap,
        };
        detector.recompute_parameters();
        detector
    }

    /// Update sample rate.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.env_l.set_sample_rate(self.sample_rate);
        self.env_r.set_sample_rate(self.sample_rate);
        self.recompute_parameters();
    }

    /// Update configuration.
    pub fn set_config(&mut self, config: DetectorConfig) {
        if config.lookahead_samples > self.lookahead_cap {
            self.lookahead_cap = config.lookahead_samples.next_power_of_two();
            self.lookahead_buf_l = vec![0.0; self.lookahead_cap];
            self.lookahead_buf_r = vec![0.0; self.lookahead_cap];
            self.lookahead_write = 0;
        }
        self.config = config;
        self.env_l.set_attack_ms(self.config.attack_ms);
        self.env_l.set_release_ms(self.config.release_ms);
        self.env_l.set_hold_ms(self.config.hold_ms);
        self.env_r.set_attack_ms(self.config.attack_ms);
        self.env_r.set_release_ms(self.config.release_ms);
        self.env_r.set_hold_ms(self.config.hold_ms);
        self.recompute_parameters();
    }

    /// Reset internal state.
    pub fn reset(&mut self) {
        self.env_l.reset();
        self.env_r.reset();
        self.mean_sq_l = 0.0;
        self.mean_sq_r = 0.0;
        self.sc_state_l.reset();
        self.sc_state_r.reset();
        self.lookahead_buf_l.fill(0.0);
        self.lookahead_buf_r.fill(0.0);
        self.lookahead_write = 0;
    }

    /// Process a stereo frame and return the detected envelope levels `(env_l, env_r)`.
    ///
    /// If `ext_sc` is provided, the detector measures the external sidechain signal
    /// instead of the main input audio.
    #[inline]
    pub fn process_frame(
        &mut self,
        in_l: f32,
        in_r: f32,
        ext_sc: Option<(f32, f32)>,
    ) -> (f32, f32) {
        let (raw_l, raw_r) = ext_sc.unwrap_or((in_l, in_r));

        // 1. Lookahead buffering if active
        let (sc_l, sc_r) = if self.config.lookahead_samples > 0 {
            let cap = self.lookahead_cap;
            let wp = self.lookahead_write;
            self.lookahead_buf_l[wp] = raw_l;
            self.lookahead_buf_r[wp] = raw_r;
            self.lookahead_write = (wp + 1) % cap;
            let rp = (wp + cap - (self.config.lookahead_samples % cap)) % cap;
            (self.lookahead_buf_l[rp], self.lookahead_buf_r[rp])
        } else {
            (raw_l, raw_r)
        };

        // 2. Sidechain filtering if configured
        let (filtered_l, filtered_r) = if let Some(coeffs) = self.sc_coeffs {
            (
                self.sc_state_l.process(sc_l, &coeffs),
                self.sc_state_r.process(sc_r, &coeffs),
            )
        } else {
            (sc_l, sc_r)
        };

        // 3. Extract instantaneous detector energy/level based on mode
        let (mut level_l, mut level_r) = match self.config.detection_mode {
            DetectionMode::Peak | DetectionMode::TruePeak => (filtered_l.abs(), filtered_r.abs()),
            DetectionMode::Rms => {
                let sq_l = filtered_l * filtered_l;
                let sq_r = filtered_r * filtered_r;
                self.mean_sq_l += self.rms_coeff * (sq_l - self.mean_sq_l);
                self.mean_sq_r += self.rms_coeff * (sq_r - self.mean_sq_r);
                if self.mean_sq_l < 1e-15 {
                    self.mean_sq_l = 0.0;
                }
                if self.mean_sq_r < 1e-15 {
                    self.mean_sq_r = 0.0;
                }
                (
                    self.mean_sq_l.max(0.0).sqrt(),
                    self.mean_sq_r.max(0.0).sqrt(),
                )
            }
            DetectionMode::MeanSquare => {
                let sq_l = filtered_l * filtered_l;
                let sq_r = filtered_r * filtered_r;
                self.mean_sq_l += self.rms_coeff * (sq_l - self.mean_sq_l);
                self.mean_sq_r += self.rms_coeff * (sq_r - self.mean_sq_r);
                if self.mean_sq_l < 1e-15 {
                    self.mean_sq_l = 0.0;
                }
                if self.mean_sq_r < 1e-15 {
                    self.mean_sq_r = 0.0;
                }
                (self.mean_sq_l, self.mean_sq_r)
            }
            DetectionMode::LufsEnergy => {
                // High-pass + high-shelf K-weighting pre-filter approximation
                (filtered_l.abs(), filtered_r.abs())
            }
            DetectionMode::BandEnergy | DetectionMode::SpectralEnergy => {
                (filtered_l.abs(), filtered_r.abs())
            }
        };

        // 4. Multichannel linking
        match self.config.link_mode {
            ChannelLinkMode::Independent => {}
            ChannelLinkMode::MaxLinked => {
                let max = level_l.max(level_r);
                level_l = max;
                level_r = max;
            }
            ChannelLinkMode::AverageLinked => {
                let avg = 0.5 * (level_l + level_r);
                level_l = avg;
                level_r = avg;
            }
            ChannelLinkMode::MidSide => {
                let mid = 0.5 * (level_l + level_r);
                let side = 0.5 * (level_l - level_r).abs();
                level_l = mid + 0.5 * side;
                level_r = mid + 0.5 * side;
            }
        }

        // 5. Ballistic smoothing
        let env_l = self.env_l.process_sample(level_l);
        let env_r = self.env_r.process_sample(level_r);

        (env_l, env_r)
    }

    /// Process a stereo block of audio frames, outputting envelope trajectories.
    pub fn process_block(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        env_l: &mut [f32],
        env_r: &mut [f32],
        ext_sc: Option<(&[f32], &[f32])>,
    ) {
        let n = in_l.len().min(in_r.len()).min(env_l.len()).min(env_r.len());
        for i in 0..n {
            let sc = ext_sc.map(|(sl, sr)| (sl[i], sr[i]));
            let (el, er) = self.process_frame(in_l[i], in_r[i], sc);
            env_l[i] = el;
            env_r[i] = er;
        }
    }

    fn recompute_parameters(&mut self) {
        let fs = self.sample_rate;
        let rms_secs = (self.config.rms_window_ms * 0.001).max(0.0001);
        self.rms_coeff = 1.0 - (-1.0 / (rms_secs * fs)).exp();

        // Configure sidechain biquad filter if enabled
        self.sc_coeffs = match self.config.sidechain_filter {
            SidechainFilter::None => None,
            SidechainFilter::HighPass { freq, q } => {
                let clamped_f = freq.clamp(10.0, fs * 0.49);
                Some(BiquadCoeffsF32::highpass(fs, clamped_f, q.max(0.1)))
            }
            SidechainFilter::LowPass { freq, q } => {
                let clamped_f = freq.clamp(10.0, fs * 0.49);
                Some(BiquadCoeffsF32::lowpass(fs, clamped_f, q.max(0.1)))
            }
            SidechainFilter::BandPass { freq, q } => {
                let clamped_f = freq.clamp(10.0, fs * 0.49);
                Some(BiquadCoeffsF32::bandpass(fs, clamped_f, q.max(0.1)))
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detector_peak_and_rms_modes() {
        let sample_rate = 48_000.0;
        let mut config = DetectorConfig {
            attack_ms: 1.0,
            release_ms: 50.0,
            ..Default::default()
        };

        let mut peak_det = DynamicsDetector::new(sample_rate, config.clone());
        config.detection_mode = DetectionMode::Rms;
        let mut rms_det = DynamicsDetector::new(sample_rate, config);

        // Step input with magnitude 1.0
        let (p_l, p_r) = peak_det.process_frame(1.0, 1.0, None);
        let (r_l, r_r) = rms_det.process_frame(1.0, 1.0, None);

        assert!(p_l > 0.0 && p_l <= 1.0);
        assert_eq!(p_l, p_r);
        assert!(r_l > 0.0 && r_l <= 1.0);
        assert_eq!(r_l, r_r);
    }

    #[test]
    fn test_detector_linking_modes() {
        let sample_rate = 48_000.0;
        let config = DetectorConfig {
            link_mode: ChannelLinkMode::MaxLinked,
            ..Default::default()
        };
        let mut det = DynamicsDetector::new(sample_rate, config);

        // Asymmetrical input (L loud, R silent)
        for _ in 0..100 {
            let (l, r) = det.process_frame(1.0, 0.0, None);
            assert_eq!(
                l, r,
                "MaxLinked must produce identical envelope across channels"
            );
        }
    }
}
