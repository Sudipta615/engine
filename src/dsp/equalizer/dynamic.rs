//! Frequency-selective dynamic equalizer.
//!
//! Extends parametric EQ with per-band dynamics detection, enabling
//! level-dependent frequency cuts and boosts with zero zipper noise.

use crate::dsp::biquad::{BiquadCoeffsF64, BiquadStateF64};
use crate::dsp::dynamics::{
    ChannelLinkMode, DetectionMode, DetectorConfig, DynamicsDetector, SidechainFilter,
};
use crate::dsp::equalizer::types::EqFilterType;

/// Maximum number of dynamic EQ bands.
pub const MAX_DYNAMIC_EQ_BANDS: usize = 8;

/// Sub-block parameter smoothing interval in frames.
const SUB_BLOCK_FRAMES: usize = 32;

/// Parameters for a single dynamic EQ band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DynamicEqBandParams {
    /// Centre/cutoff frequency in Hz.
    pub frequency: f32,
    /// Filter Q (bandwidth).
    pub q: f32,
    /// Static baseline gain in dB.
    pub static_gain_db: f32,
    /// Maximum dynamic gain change in dB (positive for expansion/boost, negative for compression/cut).
    pub dynamic_gain_db: f32,
    /// Detection threshold in dBFS.
    pub threshold_db: f32,
    /// Compression or expansion ratio (>= 1.0).
    pub ratio: f32,
    /// Attack time in milliseconds.
    pub attack_ms: f32,
    /// Release time in milliseconds.
    pub release_ms: f32,
    /// Maximum dynamic deflection limit in dB (absolute ceiling/floor on dynamic movement).
    pub range_db: f32,
    /// Filter type (Peaking, LowShelf, HighShelf).
    pub filter_type: EqFilterType,
    /// Envelope detector mode (Peak or Rms).
    pub detector_mode: DetectionMode,
    /// Whether this band is enabled.
    pub enabled: bool,
}

impl Default for DynamicEqBandParams {
    fn default() -> Self {
        Self {
            frequency: 1000.0,
            q: 1.414,
            static_gain_db: 0.0,
            dynamic_gain_db: -6.0,
            threshold_db: -20.0,
            ratio: 2.0,
            attack_ms: 10.0,
            release_ms: 80.0,
            range_db: 12.0,
            filter_type: EqFilterType::Peaking,
            detector_mode: DetectionMode::Peak,
            enabled: false,
        }
    }
}

/// A single dynamic EQ band processor with dedicated detector and smoothed biquad filters.
#[derive(Debug, Clone)]
pub struct DynamicEqBand {
    params: DynamicEqBandParams,
    detector: DynamicsDetector,
    state_l: BiquadStateF64,
    state_r: BiquadStateF64,
    coeffs: BiquadCoeffsF64,
    current_gain_db: f32,
    sample_rate: f32,
}

impl DynamicEqBand {
    /// Construct a new dynamic EQ band for the given sample rate.
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let params = DynamicEqBandParams::default();
        let det_config = DetectorConfig {
            detection_mode: params.detector_mode,
            link_mode: ChannelLinkMode::MaxLinked,
            attack_ms: params.attack_ms,
            release_ms: params.release_ms,
            hold_ms: 0.0,
            rms_window_ms: 20.0,
            lookahead_samples: 0,
            sidechain_filter: SidechainFilter::BandPass {
                freq: params.frequency,
                q: params.q,
            },
        };

        let coeffs = params.filter_type.to_filter_type().compute_coeffs(
            sr,
            params.frequency,
            params.static_gain_db,
            params.q,
        );

        let mut band = Self {
            params,
            detector: DynamicsDetector::new(sr, det_config),
            state_l: BiquadStateF64::default(),
            state_r: BiquadStateF64::default(),
            coeffs,
            current_gain_db: params.static_gain_db,
            sample_rate: sr,
        };
        band.reconfigure();
        band
    }

    /// Set parameters for this band.
    pub fn set_params(&mut self, params: DynamicEqBandParams) {
        self.params = params;
        self.reconfigure();
    }

    /// Current parameters.
    pub fn params(&self) -> &DynamicEqBandParams {
        &self.params
    }

    /// Set sample rate and recompute filter parameters.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.detector.set_sample_rate(self.sample_rate);
        self.reconfigure();
    }

    /// Reset internal filter and detector states.
    pub fn reset(&mut self) {
        self.detector.reset();
        self.state_l.reset();
        self.state_r.reset();
        self.current_gain_db = self.params.static_gain_db;
        self.recompute_coeffs(self.current_gain_db);
    }

    /// Reconfigure detector and filters from active params.
    fn reconfigure(&mut self) {
        let det_config = DetectorConfig {
            detection_mode: self.params.detector_mode,
            link_mode: ChannelLinkMode::MaxLinked,
            attack_ms: self.params.attack_ms,
            release_ms: self.params.release_ms,
            hold_ms: 0.0,
            rms_window_ms: 20.0,
            lookahead_samples: 0,
            sidechain_filter: SidechainFilter::BandPass {
                freq: self.params.frequency,
                q: self.params.q,
            },
        };
        self.detector.set_config(det_config);
        self.current_gain_db = self.params.static_gain_db;
        self.recompute_coeffs(self.current_gain_db);
    }

    fn recompute_coeffs(&mut self, gain_db: f32) {
        let effective_gain = if self.params.enabled { gain_db } else { 0.0 };
        self.coeffs = self.params.filter_type.to_filter_type().compute_coeffs(
            self.sample_rate,
            self.params.frequency,
            effective_gain,
            self.params.q,
        );
    }

    /// Process a stereo block of audio through this band.
    pub fn process_block(
        &mut self,
        left: &mut [f64],
        right: &mut [f64],
        ext_sc: Option<(&[f32], &[f32])>,
    ) {
        if !self.params.enabled {
            return;
        }

        let n = left.len().min(right.len());
        let ratio = self.params.ratio.max(1.0);
        let thresh_db = self.params.threshold_db;
        let range_db = self.params.range_db.abs();
        let dynamic_gain_db = self.params.dynamic_gain_db;

        let mut offset = 0;
        while offset < n {
            let chunk_len = (n - offset).min(SUB_BLOCK_FRAMES);

            // 1. Process detector over this chunk to find peak envelope
            let mut chunk_env_max = 0.0f32;
            for i in offset..offset + chunk_len {
                let l_sample = left[i] as f32;
                let r_sample = right[i] as f32;
                let sc = ext_sc.map(|(sl, sr)| (sl[i], sr[i]));
                let (el, er) = self.detector.process_frame(l_sample, r_sample, sc);
                chunk_env_max = chunk_env_max.max(el.max(er));
            }

            // 2. Derive dynamic gain for this sub-block
            let env_db = if chunk_env_max > 1e-6 {
                20.0 * chunk_env_max.log10()
            } else {
                -120.0
            };

            let target_gain_db = if env_db > thresh_db {
                let overshoot = env_db - thresh_db;
                let delta = overshoot * (1.0 - 1.0 / ratio);
                let clamped_delta = delta.min(range_db);
                if dynamic_gain_db < 0.0 {
                    self.params.static_gain_db - clamped_delta
                } else {
                    self.params.static_gain_db + clamped_delta
                }
            } else {
                self.params.static_gain_db
            };

            // Smooth gain transition across sub-blocks to prevent zipper noise
            let smoothing_alpha = 0.5f32;
            self.current_gain_db += smoothing_alpha * (target_gain_db - self.current_gain_db);
            self.recompute_coeffs(self.current_gain_db);

            // 3. Filter the audio samples in this sub-block
            for i in offset..offset + chunk_len {
                left[i] = self.state_l.process(left[i], &self.coeffs);
                right[i] = self.state_r.process(right[i], &self.coeffs);
            }

            offset += chunk_len;
        }
    }
}

/// Multi-band dynamic equalizer.
#[derive(Debug, Clone)]
pub struct DynamicEq {
    bands: Vec<DynamicEqBand>,
    sample_rate: f32,
    enabled: bool,
}

impl DynamicEq {
    /// Construct a new dynamic EQ processor with default bands.
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut bands = Vec::with_capacity(MAX_DYNAMIC_EQ_BANDS);
        for _ in 0..MAX_DYNAMIC_EQ_BANDS {
            bands.push(DynamicEqBand::new(sr));
        }
        Self {
            bands,
            sample_rate: sr,
            enabled: true,
        }
    }

    /// Set overall enabled state.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Whether dynamic EQ is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Access a band by index.
    pub fn band_mut(&mut self, index: usize) -> Option<&mut DynamicEqBand> {
        self.bands.get_mut(index)
    }

    /// Access a band by index (read-only).
    pub fn band(&self, index: usize) -> Option<&DynamicEqBand> {
        self.bands.get(index)
    }

    /// Total band count.
    pub fn band_count(&self) -> usize {
        self.bands.len()
    }

    /// Set sample rate across all bands.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for band in &mut self.bands {
            band.set_sample_rate(self.sample_rate);
        }
    }

    /// Reset internal filter and detector states across all bands.
    pub fn reset(&mut self) {
        for band in &mut self.bands {
            band.reset();
        }
    }

    /// Process a stereo block of audio through all active dynamic EQ bands.
    pub fn process_block(
        &mut self,
        left: &mut [f64],
        right: &mut [f64],
        ext_sc: Option<(&[f32], &[f32])>,
    ) {
        if !self.enabled {
            return;
        }
        for band in &mut self.bands {
            band.process_block(left, right, ext_sc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dynamic_eq_cut_above_threshold() {
        let sample_rate = 48_000.0;
        let mut dyn_eq = DynamicEq::new(sample_rate);

        // Configure band 0 for dynamic cut at 1 kHz
        if let Some(b) = dyn_eq.band_mut(0) {
            b.set_params(DynamicEqBandParams {
                frequency: 1000.0,
                q: 1.414,
                static_gain_db: 0.0,
                dynamic_gain_db: -12.0,
                threshold_db: -20.0,
                ratio: 4.0,
                attack_ms: 1.0,
                release_ms: 50.0,
                range_db: 12.0,
                filter_type: EqFilterType::Peaking,
                detector_mode: DetectionMode::Peak,
                enabled: true,
            });
        }

        // Test with a low-level 1 kHz signal (below threshold: -30 dBFS -> peak ~0.03)
        let mut quiet_l: Vec<f64> = (0..1024)
            .map(|i| {
                0.03 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / sample_rate as f64).sin()
            })
            .collect();
        let mut quiet_r = quiet_l.clone();
        dyn_eq.process_block(&mut quiet_l, &mut quiet_r, None);

        // Max amplitude of quiet signal should remain essentially unchanged (~0.03)
        let max_quiet = quiet_l[512..]
            .iter()
            .map(|s| s.abs())
            .fold(0.0f64, f64::max);
        assert!(
            (max_quiet - 0.03).abs() < 0.005,
            "Quiet signal should not trigger dynamic cut"
        );

        // Test with a high-level 1 kHz signal (above threshold: 0 dBFS -> peak 1.0)
        let mut loud_l: Vec<f64> = (0..2048)
            .map(|i| {
                1.0 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / sample_rate as f64).sin()
            })
            .collect();
        let mut loud_r = loud_l.clone();
        dyn_eq.process_block(&mut loud_l, &mut loud_r, None);

        // Max amplitude in steady state should be reduced (dynamic cut of ~6-12 dB)
        let max_loud = loud_l[1024..]
            .iter()
            .map(|s| s.abs())
            .fold(0.0f64, f64::max);
        assert!(
            max_loud < 0.8,
            "Dynamic cut must attenuate 1kHz signal above threshold (got {max_loud})"
        );
    }

    #[test]
    fn test_dynamic_eq_boost_above_threshold() {
        let sample_rate = 48_000.0;
        let mut dyn_eq = DynamicEq::new(sample_rate);

        // Configure band 0 for dynamic boost (+6 dB) at 1 kHz
        if let Some(b) = dyn_eq.band_mut(0) {
            b.set_params(DynamicEqBandParams {
                frequency: 1000.0,
                q: 1.414,
                static_gain_db: 0.0,
                dynamic_gain_db: 6.0,
                threshold_db: -20.0,
                ratio: 2.0,
                attack_ms: 1.0,
                release_ms: 50.0,
                range_db: 6.0,
                filter_type: EqFilterType::Peaking,
                detector_mode: DetectionMode::Peak,
                enabled: true,
            });
        }

        // Test with a signal above threshold (0.5 peak -> -6 dBFS)
        let mut loud_l: Vec<f64> = (0..2048)
            .map(|i| {
                0.5 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / sample_rate as f64).sin()
            })
            .collect();
        let mut loud_r = loud_l.clone();
        dyn_eq.process_block(&mut loud_l, &mut loud_r, None);

        let max_loud = loud_l[1024..]
            .iter()
            .map(|s| s.abs())
            .fold(0.0f64, f64::max);
        assert!(
            max_loud > 0.55,
            "Dynamic boost must increase 1kHz signal above threshold (got {max_loud})"
        );
    }

    #[test]
    fn test_dynamic_eq_disabled_is_passthrough() {
        let sample_rate = 48_000.0;
        let mut dyn_eq = DynamicEq::new(sample_rate);
        dyn_eq.set_enabled(false);

        let mut l: Vec<f64> = (0..512).map(|i| (i as f64 * 0.01).sin()).collect();
        let mut r = l.clone();
        let original_l = l.clone();
        dyn_eq.process_block(&mut l, &mut r, None);

        for (a, b) in l.iter().zip(original_l.iter()) {
            assert!((a - b).abs() < 1e-12);
        }
    }
}
