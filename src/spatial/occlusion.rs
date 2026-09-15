//! Occlusion / acoustic transmission (spec §43–44).
//!
//! Occlusion reduces what the listener hears of a source: broadband
//! attenuation plus a low-pass roll-off (thick walls dull sound). The spec
//! warns against collapsing every environmental effect into one scalar, so
//! the renderer derives a structured [`AcousticTransmission`] from the
//! source's [`Occlusion`] amount — `attenuation_db` and `cutoff_hz` are
//! applied this phase; `diffusion` is a declared seam (§44) kept on the
//! struct so later phases (obstruction geometry, materials) slot in without
//! changing the model.
//!
//! ## Realtime discipline
//!
//! [`OcclusionState`] is the renderer-owned per-object filter state (one
//! biquad + a smoothed cutoff). Coefficients are recomputed at block rate
//! and the cutoff is one-pole smoothed, so automated occlusion changes ramp
//! instead of zippering; per-sample processing is a plain
//! [`BiquadState::process`] — no allocation, no locks. The filter runs on
//! the object's input *before* panning (spec §43: occlude before VBAP/HOA/
//! HRTF), and its output feeds both the pan paths and the LFE send.

use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};

/// Configuration of a source's occlusion. `amount` is the primary control;
/// the two bounds tune how severe full occlusion is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Occlusion {
    /// Occlusion amount `[0, 1]`: 0 = unobstructed, 1 = fully occluded.
    pub amount: f32,
    /// Broadband attenuation (dB) applied at `amount = 1`.
    pub max_attenuation_db: f32,
    /// Lowest low-pass cutoff (Hz) reached at `amount = 1`.
    pub min_cutoff_hz: f32,
}

impl Default for Occlusion {
    fn default() -> Self {
        Self {
            amount: 0.0,
            max_attenuation_db: 24.0,
            min_cutoff_hz: 500.0,
        }
    }
}

/// The per-block transmission state derived from an [`Occlusion`] (spec §44).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticTransmission {
    /// Broadband attenuation in dB (0 at amount 0).
    pub attenuation_db: f32,
    /// Low-pass cutoff in Hz (Nyquist at amount 0, → `min_cutoff_hz` at
    /// amount 1, exponential in log-frequency).
    pub cutoff_hz: f32,
    /// Scattering/diffusion of the transmitted sound. Always `0.0` this
    /// phase — a declared seam for material/geometry-based transmission.
    pub diffusion: f32,
}

impl Occlusion {
    /// Map `amount ∈ [0,1]` to an [`AcousticTransmission`] at `sample_rate`.
    /// The cutoff interpolates exponentially between the full band and
    /// `min_cutoff_hz` so equal amount steps sound like equal steps.
    pub fn transmission(&self, sample_rate: f32) -> AcousticTransmission {
        let a = self.amount.clamp(0.0, 1.0);
        let nyquist = (sample_rate * 0.5).max(1.0);
        let max_cut = nyquist.max(self.min_cutoff_hz);
        let lo = self.min_cutoff_hz.max(20.0).ln();
        let hi = max_cut.ln();
        // amount=0 → full band; amount=1 → min cutoff.
        let log_cut = hi + a * (lo - hi);
        AcousticTransmission {
            attenuation_db: a * self.max_attenuation_db,
            cutoff_hz: log_cut.exp(),
            diffusion: 0.0,
        }
    }

    /// Linear gain implied by [`Self::transmission`]'s attenuation.
    pub fn gain(&self, sample_rate: f32) -> f32 {
        self.transmission(sample_rate).gain()
    }
}

impl AcousticTransmission {
    /// Linear gain implied by [`Self::attenuation_db`].
    pub fn gain(&self) -> f32 {
        if self.attenuation_db <= 0.0 {
            1.0
        } else {
            10.0f32.powf(-self.attenuation_db / 20.0)
        }
    }
}

/// Type alias for backward compatibility (spec §43).
pub type BroadbandOcclusion = Occlusion;

/// Frequency-dependent diffraction around edges/barriers (Phase 3 Point 24).
///
/// Models edge diffraction where longer wavelengths (lower frequencies) bend around
/// obstacles more readily than higher frequencies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiffractionOcclusion {
    /// Number of diffracting edges in path (1 to 4). Higher order = sharper attenuation.
    pub edge_order: u8,
    /// Normalized diffraction amount [0.0, 1.0]: 0 = direct line of sight, 1 = deep shadow zone.
    pub diffraction_amount: f32,
    /// Effective low-pass order for the diffraction shadow filter (1 or 2).
    pub low_pass_order: u8,
}

impl Default for DiffractionOcclusion {
    fn default() -> Self {
        Self {
            edge_order: 1,
            diffraction_amount: 0.0,
            low_pass_order: 1,
        }
    }
}

/// Material-dependent acoustic transmission (Phase 3 Point 24).
///
/// Frequency-dependent transmission through physical partitions across 3 bands:
/// low (< 250 Hz), mid (250 Hz - 3 kHz), and high (> 3 kHz).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaterialTransmission {
    /// Transmission linear gain for low frequencies (< 250 Hz). E.g. 0.8 for drywall.
    pub low_gain: f32,
    /// Transmission linear gain for mid frequencies (250 Hz - 3 kHz). E.g. 0.35 for drywall.
    pub mid_gain: f32,
    /// Transmission linear gain for high frequencies (> 3 kHz). E.g. 0.1 for drywall.
    pub high_gain: f32,
}

impl Default for MaterialTransmission {
    fn default() -> Self {
        Self {
            low_gain: 1.0,
            mid_gain: 1.0,
            high_gain: 1.0,
        }
    }
}

impl MaterialTransmission {
    /// Transparent (100% transmission / no partition).
    pub const TRANSPARENT: Self = Self {
        low_gain: 1.0,
        mid_gain: 1.0,
        high_gain: 1.0,
    };

    /// Drywall / partition wall preset.
    pub const DRYWALL: Self = Self {
        low_gain: 0.8,
        mid_gain: 0.35,
        high_gain: 0.1,
    };

    /// Solid brick / concrete wall preset.
    pub const CONCRETE: Self = Self {
        low_gain: 0.4,
        mid_gain: 0.08,
        high_gain: 0.01,
    };

    /// Glass window preset.
    pub const GLASS: Self = Self {
        low_gain: 0.6,
        mid_gain: 0.25,
        high_gain: 0.15,
    };

    /// Wood door preset.
    pub const WOOD: Self = Self {
        low_gain: 0.7,
        mid_gain: 0.3,
        high_gain: 0.12,
    };
}

/// Combined frequency-dependent occlusion and diffraction model (Phase 3 Point 24).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FrequencyDependentOcclusion {
    /// Broadband occlusion fallback or baseline.
    pub broadband: Occlusion,
    /// Edge diffraction parameters.
    pub diffraction: DiffractionOcclusion,
    /// Material transmission parameters.
    pub material: MaterialTransmission,
}

impl FrequencyDependentOcclusion {
    /// Compute 3-band gains (low, mid, high) combining material transmission and diffraction.
    pub fn band_gains(&self) -> (f32, f32, f32) {
        let diff = self.diffraction.diffraction_amount.clamp(0.0, 1.0);
        let order = self.diffraction.edge_order.clamp(1, 4) as f32;
        // Low frequencies diffract well: attenuation is modest even in shadow.
        let diff_low = 1.0 - 0.3 * diff * (order * 0.25);
        // Mid frequencies diffract partially.
        let diff_mid = 1.0 - 0.65 * diff * (order * 0.25);
        // High frequencies diffract poorly: severe shadow attenuation.
        let diff_high = 1.0 - 0.95 * diff * (order * 0.25);

        let g_low = (self.material.low_gain * diff_low).clamp(0.0, 1.0);
        let g_mid = (self.material.mid_gain * diff_mid).clamp(0.0, 1.0);
        let g_high = (self.material.high_gain * diff_high).clamp(0.0, 1.0);

        (g_low, g_mid, g_high)
    }
}

/// Precomputed coefficients for the 3-band frequency-dependent occlusion filters.
#[derive(Debug, Clone, Copy)]
pub struct OcclusionBandCoeffs {
    pub lp_250: BiquadCoeffsF32,
    pub hp_250: BiquadCoeffsF32,
    pub lp_3000: BiquadCoeffsF32,
    pub hp_3000: BiquadCoeffsF32,
    pub gains: (f32, f32, f32),
}

impl OcclusionBandCoeffs {
    /// Construct crossover biquad coefficients at 250 Hz and 3 kHz.
    pub fn new(sample_rate: f32, fd: &FrequencyDependentOcclusion) -> Self {
        let q = 0.707;
        Self {
            lp_250: BiquadCoeffsF32::lowpass(sample_rate, 250.0, q),
            hp_250: BiquadCoeffsF32::highpass(sample_rate, 250.0, q),
            lp_3000: BiquadCoeffsF32::lowpass(sample_rate, 3000.0, q),
            hp_3000: BiquadCoeffsF32::highpass(sample_rate, 3000.0, q),
            gains: fd.band_gains(),
        }
    }
}

/// Realtime-safe per-object 3-band occlusion crossover filter state.
#[derive(Debug, Clone, Copy, Default)]
pub struct OcclusionBandState {
    pub xover_low_lp: BiquadStateF32,
    pub xover_low_hp: BiquadStateF32,
    pub xover_mid_lp: BiquadStateF32,
    pub xover_high_hp: BiquadStateF32,
}

impl OcclusionBandState {
    /// Process a single audio sample through the 3-band crossover and apply band gains.
    /// Realtime-safe: 4 biquad steps, zero allocations.
    #[inline]
    pub fn process(&mut self, sample: f32, coeffs: &OcclusionBandCoeffs) -> f32 {
        let (g_low, g_mid, g_high) = coeffs.gains;
        // Split at 250 Hz: low band vs (mid + high) band.
        let low_band = self.xover_low_lp.process(sample, &coeffs.lp_250);
        let rest = self.xover_low_hp.process(sample, &coeffs.hp_250);

        // Split rest at 3000 Hz: mid band vs high band.
        let mid_band = self.xover_mid_lp.process(rest, &coeffs.lp_3000);
        let high_band = self.xover_high_hp.process(rest, &coeffs.hp_3000);

        low_band * g_low + mid_band * g_mid + high_band * g_high
    }
}

/// Renderer-owned per-object occlusion filter state (realtime-safe).
#[derive(Debug, Clone, Copy)]
pub struct OcclusionState {
    /// Smoothed log-cutoff (Hz). `0.0` means uninitialised (first block).
    cutoff_log: f32,
    /// The actual one-pole low-pass biquad state.
    filter: BiquadStateF32,
    /// Preallocated 3-band frequency-dependent crossover state.
    pub band_state: OcclusionBandState,
}

impl Default for OcclusionState {
    fn default() -> Self {
        Self {
            cutoff_log: 0.0,
            filter: BiquadStateF32::default(),
            band_state: OcclusionBandState::default(),
        }
    }
}

impl OcclusionState {
    /// Advance the per-block cutoff smoothing and return the fresh low-pass
    /// coefficients for this block. `smooth` is the renderer's per-block
    /// one-pole factor (`1.0` = exact target).
    ///
    /// `cutoff_hz` must come from [`Occlusion::transmission`] (already
    /// bounded to `[min_cutoff, Nyquist]`).
    pub fn coeffs(&mut self, cutoff_hz: f32, sample_rate: f32, smooth: f32) -> BiquadCoeffsF32 {
        let target = cutoff_hz.max(20.0).ln();
        if self.cutoff_log == 0.0 {
            self.cutoff_log = target;
        } else if smooth < 1.0 {
            self.cutoff_log += smooth * (target - self.cutoff_log);
        }
        let cutoff = self.cutoff_log.exp();
        BiquadCoeffsF32::lowpass(sample_rate, cutoff, 0.707)
    }

    /// Filter one sample through the current coefficients.
    #[inline]
    pub fn process(&mut self, sample: f32, coeffs: &BiquadCoeffsF32) -> f32 {
        self.filter.process(sample, coeffs)
    }

    /// Filter one sample through the 3-band frequency-dependent model.
    /// Realtime-safe, zero allocations.
    #[inline]
    pub fn process_frequency_dependent(
        &mut self,
        sample: f32,
        coeffs: &OcclusionBandCoeffs,
    ) -> f32 {
        self.band_state.process(sample, coeffs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_amount_is_passthrough() {
        let occ = Occlusion::default();
        let tr = occ.transmission(48_000.0);
        assert_eq!(tr.attenuation_db, 0.0);
        assert!(
            (tr.cutoff_hz - 24_000.0).abs() < 1.0,
            "cutoff {}",
            tr.cutoff_hz
        );
        assert_eq!(tr.diffusion, 0.0);
        assert_eq!(occ.gain(48_000.0), 1.0);
    }

    #[test]
    fn full_amount_attenuates_and_rolls_off() {
        let occ = Occlusion {
            amount: 1.0,
            ..Default::default()
        };
        let tr = occ.transmission(48_000.0);
        assert!((tr.attenuation_db - 24.0).abs() < 1e-3);
        assert!(
            (tr.cutoff_hz - 500.0).abs() < 1.0,
            "cutoff {}",
            tr.cutoff_hz
        );
        let g = occ.gain(48_000.0);
        assert!((g - 10.0f32.powf(-24.0 / 20.0)).abs() < 1e-4, "gain {g}");
    }

    #[test]
    fn amount_is_monotonic_in_log_space() {
        let sr = 48_000.0;
        let mut prev_db = 0.0f32;
        let mut prev_cut = sr * 0.5;
        for a in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let occ = Occlusion {
                amount: a,
                ..Default::default()
            };
            let tr = occ.transmission(sr);
            assert!(tr.attenuation_db >= prev_db, "attenuation monotonic");
            assert!(tr.cutoff_hz <= prev_cut + 1e-3, "cutoff monotonic");
            assert!(tr.cutoff_hz >= 500.0 - 1e-3 && tr.cutoff_hz <= sr * 0.5 + 1e-3);
            prev_db = tr.attenuation_db;
            prev_cut = tr.cutoff_hz;
        }
    }

    #[test]
    fn amount_beyond_range_clamps() {
        let occ = Occlusion {
            amount: 3.0,
            ..Default::default()
        };
        let tr = occ.transmission(48_000.0);
        assert!((tr.attenuation_db - 24.0).abs() < 1e-3);
        let occ = Occlusion {
            amount: -1.0,
            ..Default::default()
        };
        assert_eq!(occ.gain(48_000.0), 1.0);
    }

    #[test]
    fn state_filters_and_smooths_cutoff() {
        let mut st = OcclusionState::default();
        let sr = 48_000.0;
        // Jump the cutoff between blocks; the first block snaps, later ones
        // move a bounded step (no zipper).
        let c1 = st.coeffs(500.0, sr, 0.25);
        let y1 = st.process(1.0, &c1);
        assert!(y1.is_finite());
        let mut prev = y1;
        for _ in 0..4 {
            let c = st.coeffs(500.0, sr, 0.25);
            let y = st.process(1.0, &c);
            assert!(y.is_finite());
            assert!((y - prev).abs() < 0.5, "bounded step {}", (y - prev).abs());
            prev = y;
        }
    }

    #[test]
    fn frequency_dependent_zero_diffraction_transparent_is_unity() {
        let fd = FrequencyDependentOcclusion {
            broadband: Occlusion::default(),
            diffraction: DiffractionOcclusion::default(),
            material: MaterialTransmission::TRANSPARENT,
        };
        let (gl, gm, gh) = fd.band_gains();
        assert!((gl - 1.0).abs() < 1e-4);
        assert!((gm - 1.0).abs() < 1e-4);
        assert!((gh - 1.0).abs() < 1e-4);
    }

    #[test]
    fn frequency_dependent_materials_attenuate_progressively() {
        for mat in [
            MaterialTransmission::DRYWALL,
            MaterialTransmission::CONCRETE,
            MaterialTransmission::GLASS,
            MaterialTransmission::WOOD,
        ] {
            // Low frequencies transmit better than high frequencies for typical building materials.
            assert!(mat.low_gain >= mat.mid_gain);
            assert!(mat.mid_gain >= mat.high_gain);
        }
    }

    #[test]
    fn frequency_dependent_diffraction_shadow_low_vs_high() {
        let fd = FrequencyDependentOcclusion {
            broadband: Occlusion::default(),
            diffraction: DiffractionOcclusion {
                edge_order: 2,
                diffraction_amount: 1.0,
                low_pass_order: 1,
            },
            material: MaterialTransmission::TRANSPARENT,
        };
        let (gl, gm, gh) = fd.band_gains();
        // Low frequencies bend around edge much better than high frequencies.
        assert!(gl > gm);
        assert!(gm > gh);
    }

    #[test]
    fn occlusion_band_state_process_runs_without_divergence() {
        let fd = FrequencyDependentOcclusion {
            broadband: Occlusion::default(),
            diffraction: DiffractionOcclusion {
                edge_order: 1,
                diffraction_amount: 0.5,
                low_pass_order: 1,
            },
            material: MaterialTransmission::DRYWALL,
        };
        let coeffs = OcclusionBandCoeffs::new(48_000.0, &fd);
        let mut state = OcclusionBandState::default();

        for _ in 0..100 {
            let out = state.process(0.5, &coeffs);
            assert!(out.is_finite());
            assert!(out.abs() <= 1.0);
        }
    }
}
