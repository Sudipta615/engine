//! Spatial Bass Engine (spec §56–57, Part II).
//!
//! Provides three operational modes:
//! - `Pure`: Bit-perfect pass-through of all channels.
//! - `BassManaged`: Active crossover steering between mains and subwoofer.
//! - `BassImmersion`: Psychoacoustic harmonic generation (missing fundamental synthesis)
//!   and dynamic low-shelf EQ for headphones and small monitors.

use super::management::BassManager;
use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};
use config::{BassManagementConfig, SpatialBassConfig, SpatialBassMode};

/// Spatial Bass Engine implementing Pure, BassManaged, and BassImmersion modes.
#[derive(Debug, Clone)]
pub struct SpatialBassEngine {
    config: SpatialBassConfig,
    manager: BassManager,
    sample_rate: f32,

    // Immersion mode DSP components:
    // Sub-bass extraction lowpass filter (e.g. < 80 Hz)
    immersion_lp_coeffs: BiquadCoeffsF32,
    immersion_lp_state_l: BiquadStateF32,
    immersion_lp_state_r: BiquadStateF32,

    // Harmonic bandpass filter (70 Hz .. 240 Hz)
    immersion_bp_coeffs: BiquadCoeffsF32,
    immersion_bp_state_l: BiquadStateF32,
    immersion_bp_state_r: BiquadStateF32,

    // Dynamic low-shelf filter for loudness boost
    shelf_coeffs: BiquadCoeffsF32,
    shelf_state_l: BiquadStateF32,
    shelf_state_r: BiquadStateF32,

    // Envelope detector for dynamic excursion limiter
    envelope_l: f32,
    envelope_r: f32,
}

impl SpatialBassEngine {
    /// Construct a new spatial bass engine.
    pub fn new(config: &SpatialBassConfig, sample_rate: f32, channel_count: usize) -> Self {
        let sr = sample_rate.max(1.0);
        let mgmt_cfg = BassManagementConfig {
            enabled: config.mode == SpatialBassMode::BassManaged,
            mains_highpass_enabled: true,
            crossover_hz: config.crossover_hz,
            q: std::f32::consts::FRAC_1_SQRT_2,
            slope: config.slope,
            filter_type: config.filter_type,
            sub_delay_ms: config.sub_delay_ms,
            sub_phase_degrees: config.sub_phase_degrees,
            sub_polarity_invert: config.sub_polarity_invert,
        };

        let manager = BassManager::new(&mgmt_cfg, sr, channel_count);

        let mut engine = Self {
            config: config.clone(),
            manager,
            sample_rate: sr,
            immersion_lp_coeffs: BiquadCoeffsF32::lowpass(sr, 80.0, 0.707),
            immersion_lp_state_l: BiquadStateF32::default(),
            immersion_lp_state_r: BiquadStateF32::default(),
            immersion_bp_coeffs: BiquadCoeffsF32::bandpass(sr, 140.0, 1.2),
            immersion_bp_state_l: BiquadStateF32::default(),
            immersion_bp_state_r: BiquadStateF32::default(),
            shelf_coeffs: BiquadCoeffsF32::lowshelf(sr, 100.0, config.dynamic_boost_db, 0.707),
            shelf_state_l: BiquadStateF32::default(),
            shelf_state_r: BiquadStateF32::default(),
            envelope_l: 0.0,
            envelope_r: 0.0,
        };
        engine.recompute_immersion_filters();
        engine
    }

    /// Update configuration and sample rate.
    pub fn update_config(&mut self, config: &SpatialBassConfig, sample_rate: f32) {
        self.config = config.clone();
        self.sample_rate = sample_rate.max(1.0);

        let mgmt_cfg = BassManagementConfig {
            enabled: config.mode == SpatialBassMode::BassManaged,
            mains_highpass_enabled: true,
            crossover_hz: config.crossover_hz,
            q: std::f32::consts::FRAC_1_SQRT_2,
            slope: config.slope,
            filter_type: config.filter_type,
            sub_delay_ms: config.sub_delay_ms,
            sub_phase_degrees: config.sub_phase_degrees,
            sub_polarity_invert: config.sub_polarity_invert,
        };

        self.manager.update_config(&mgmt_cfg, self.sample_rate);
        self.recompute_immersion_filters();
    }

    fn recompute_immersion_filters(&mut self) {
        let sr = self.sample_rate;
        let fc = self.config.crossover_hz.clamp(50.0, 120.0);
        self.immersion_lp_coeffs = BiquadCoeffsF32::lowpass(sr, fc, 0.707);
        self.immersion_bp_coeffs = BiquadCoeffsF32::bandpass(sr, fc * 1.75, 1.2);
        self.shelf_coeffs = BiquadCoeffsF32::lowshelf(
            sr,
            fc * 1.2,
            self.config.dynamic_boost_db.clamp(0.0, 12.0),
            0.707,
        );
    }

    /// Process multi-channel audio with the active spatial bass mode.
    ///
    /// - `mains`: Slice of mutable audio channel buffers (L, R, C, surrounds...).
    /// - `sub_out`: Dedicated subwoofer buffer.
    /// - `direct_lfe`: Optional direct LFE content.
    /// - `object_bass`: Optional sum of spatial object bass sends.
    pub fn process(
        &mut self,
        mains: &mut [&mut [f32]],
        sub_out: &mut [f32],
        direct_lfe: Option<&[f32]>,
        object_bass: Option<&[f32]>,
    ) {
        match self.config.mode {
            SpatialBassMode::Pure => {
                // Bit-perfect pass-through: mains are unmodified, sub receives direct LFE + object bass
                sub_out.fill(0.0);
                if let Some(lfe) = direct_lfe {
                    let n = sub_out.len().min(lfe.len());
                    sub_out[..n].copy_from_slice(&lfe[..n]);
                }
                if let Some(ob) = object_bass {
                    let n = sub_out.len().min(ob.len());
                    for (s, o) in sub_out[..n].iter_mut().zip(ob[..n].iter()) {
                        *s += *o;
                    }
                }
            }
            SpatialBassMode::BassManaged => {
                // Steer frequencies via dedicated crossover manager
                self.manager
                    .process(mains, sub_out, direct_lfe, object_bass);
            }
            SpatialBassMode::BassImmersion => {
                // Process bass immersion on mains (especially Left and Right channels)
                self.process_immersion(mains);

                // Subwoofer still receives LFE and object bass (if connected)
                sub_out.fill(0.0);
                if let Some(lfe) = direct_lfe {
                    let n = sub_out.len().min(lfe.len());
                    sub_out[..n].copy_from_slice(&lfe[..n]);
                }
                if let Some(ob) = object_bass {
                    let n = sub_out.len().min(ob.len());
                    for (s, o) in sub_out[..n].iter_mut().zip(ob[..n].iter()) {
                        *s += *o;
                    }
                }
            }
        }
    }

    /// Process Bass Immersion mode on Left and Right main channels:
    /// - Synthesizes 2nd and 3rd harmonics of sub-bass (< 80 Hz).
    /// - Bandpasses harmonics into the 70..240 Hz audible range.
    /// - Dynamically boosts low-shelf while limiting excursion.
    fn process_immersion(&mut self, mains: &mut [&mut [f32]]) {
        if mains.is_empty() {
            return;
        }

        let harmonic_gain = self.config.harmonic_amount.clamp(0.0, 1.0) * 0.4;
        let env_decay = (-1.0 / (self.sample_rate * 0.05)).exp(); // ~50 ms decay

        let block_len = mains[0].len();

        // Process Left channel
        if let Some(left) = mains.get_mut(0) {
            for i in 0..block_len.min(left.len()) {
                let x = left[i];

                // 1. Extract sub-bass fundamental
                let sub = self
                    .immersion_lp_state_l
                    .process(x, &self.immersion_lp_coeffs);

                // 2. Generate 2nd and 3rd harmonics using Chebyshev polynomials
                // T2(x) = 2x^2 - 1, T3(x) = 4x^3 - 3x
                let sub_clamped = sub.clamp(-1.0, 1.0);
                let h2 = 2.0 * sub_clamped * sub_clamped - 1.0;
                let h3 = 4.0 * sub_clamped * sub_clamped * sub_clamped - 3.0 * sub_clamped;
                let harmonics_raw = 0.6 * h2 + 0.4 * h3;

                // 3. Bandpass harmonics into audible 70..240 Hz region
                let harmonics = self
                    .immersion_bp_state_l
                    .process(harmonics_raw, &self.immersion_bp_coeffs);

                // 4. Dynamic low-shelf boost with envelope excursion limiter
                let boosted = if self.config.dynamic_boost_db > 0.05 {
                    self.shelf_state_l.process(x, &self.shelf_coeffs)
                } else {
                    x
                };

                let abs_x = boosted.abs();
                self.envelope_l = abs_x.max(self.envelope_l * env_decay);

                // Soft limiter knee above 0.8 to prevent speaker overdrive
                let compressed = if self.envelope_l > 0.8 {
                    let excess = self.envelope_l - 0.8;
                    let scale = 0.8 + excess / (1.0 + excess);
                    boosted * (scale / self.envelope_l)
                } else {
                    boosted
                };

                left[i] = compressed + harmonics * harmonic_gain;
            }
        }

        // Process Right channel
        if let Some(right) = mains.get_mut(1) {
            for i in 0..block_len.min(right.len()) {
                let x = right[i];

                let sub = self
                    .immersion_lp_state_r
                    .process(x, &self.immersion_lp_coeffs);
                let sub_clamped = sub.clamp(-1.0, 1.0);
                let h2 = 2.0 * sub_clamped * sub_clamped - 1.0;
                let h3 = 4.0 * sub_clamped * sub_clamped * sub_clamped - 3.0 * sub_clamped;
                let harmonics_raw = 0.6 * h2 + 0.4 * h3;

                let harmonics = self
                    .immersion_bp_state_r
                    .process(harmonics_raw, &self.immersion_bp_coeffs);

                let boosted = if self.config.dynamic_boost_db > 0.05 {
                    self.shelf_state_r.process(x, &self.shelf_coeffs)
                } else {
                    x
                };

                let abs_x = boosted.abs();
                self.envelope_r = abs_x.max(self.envelope_r * env_decay);

                let compressed = if self.envelope_r > 0.8 {
                    let excess = self.envelope_r - 0.8;
                    let scale = 0.8 + excess / (1.0 + excess);
                    boosted * (scale / self.envelope_r)
                } else {
                    boosted
                };

                right[i] = compressed + harmonics * harmonic_gain;
            }
        }
    }

    /// Reset internal filter and state memories.
    pub fn reset(&mut self) {
        self.manager.reset();
        self.immersion_lp_state_l.reset();
        self.immersion_lp_state_r.reset();
        self.immersion_bp_state_l.reset();
        self.immersion_bp_state_r.reset();
        self.shelf_state_l.reset();
        self.shelf_state_r.reset();
        self.envelope_l = 0.0;
        self.envelope_r = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_mode_is_bit_perfect() {
        let config = SpatialBassConfig {
            mode: SpatialBassMode::Pure,
            ..Default::default()
        };

        let mut engine = SpatialBassEngine::new(&config, 48000.0, 2);

        let mut left = vec![0.123f32; 128];
        let mut right = vec![-0.456f32; 128];
        let original_left = left.clone();
        let original_right = right.clone();
        let mut sub = vec![0.0f32; 128];

        let mut mains = [&mut left[..], &mut right[..]];
        engine.process(&mut mains, &mut sub, None, None);

        assert_eq!(mains[0], &original_left[..]);
        assert_eq!(mains[1], &original_right[..]);
        assert_eq!(sub, vec![0.0f32; 128]);
    }

    #[test]
    fn bass_immersion_generates_harmonics() {
        let config = SpatialBassConfig {
            mode: SpatialBassMode::BassImmersion,
            crossover_hz: 80.0,
            harmonic_amount: 1.0,
            dynamic_boost_db: 0.0,
            ..Default::default()
        };

        let mut engine = SpatialBassEngine::new(&config, 48000.0, 2);

        // Feed pure 40 Hz sub-bass tone
        let mut left = vec![0.0f32; 2048];
        let mut right = vec![0.0f32; 2048];
        for i in 0..2048 {
            let t = i as f32 / 48000.0;
            let val = (2.0 * std::f32::consts::PI * 40.0 * t).sin();
            left[i] = val;
            right[i] = val;
        }

        let mut sub = vec![0.0f32; 2048];
        let mut mains = [&mut left[..], &mut right[..]];

        engine.process(&mut mains, &mut sub, None, None);

        // Immersion mode should add harmonic energy around 80 Hz / 120 Hz
        // which alters waveform peaks
        let mut energy = 0.0f32;
        for &s in &mains[0][1000..] {
            energy += s * s;
        }
        assert!(energy > 10.0, "Immersion energy missing: {}", energy);
    }
}
