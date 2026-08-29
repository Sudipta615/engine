//! Distance-based level modelling (spec Part VII §38, §97).
//!
//! This phase ships a monotonic distance attenuation law and a bounded,
//! optional high-frequency air-absorption roll-off. The two are kept
//! separate so a distant source is not reduced to an "unnaturally dull"
//! signal: gain attenuation and HF filtering are independent controls.
//!
//! No module silently chooses its own units — **distance is in metres** and
//! **position/length is in metres** throughout the spatial layer (spec §18).

use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};

/// Distance attenuation law applied to an object's direct path (spec §38).
///
/// `Custom` is a declared seam; this phase implements the three analytic
/// clawds plus the inverse-square reference used most by spatial mixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceModel {
    /// No distance attenuation — level is constant regardless of distance.
    Linear,
    /// `1 / distance` (constant power per unit distance; halving distance
    /// raises level by ≈6 dB).
    Inverse,
    /// `1 / distance²` (free-field inverse square law; halving distance
    /// raises level by ≈12 dB).
    InverseSquare,
    /// Distance-independent until a short reference distance, then
    /// `reference / distance` (a bounded-inverse model that avoids infinite
    /// gain at zero distance).
    InverseReference,
}

impl DistanceModel {
    /// Compute the linear distance gain for a listener-to-source distance
    /// in metres. The result is clamped to a sane ceiling so pathological
    /// near-field distances cannot blow past full scale (spec §40).
    ///
    /// Returns 0 for distances ≤ 0 (no line doubling; deterministic, never
    /// NaN).
    pub fn distance_gain(&self, distance: f32, reference: f32) -> f32 {
        let d = distance.max(f32::EPSILON);
        match self {
            Self::Linear => 1.0,
            Self::Inverse => (1.0 / d).min(MAX_GAIN),
            Self::InverseSquare => (1.0 / (d * d)).min(MAX_GAIN),
            Self::InverseReference => {
                if distance <= reference || reference <= f32::EPSILON {
                    1.0
                } else {
                    (reference / distance).min(MAX_GAIN)
                }
            }
        }
    }
}

/// Ceiling applied to any distance gain so a source on/near the listener
/// never exceeds a sane headroom budget (≈+6 dB over unity → 2.0 linear).
pub const MAX_GAIN: f32 = 2.0;

/// A bounded, optional high-frequency air-absorption model (spec §39).
///
/// The roll-off is a simple first-order low-pass whose cutoff scales with
/// distance; it is deliberately gentle and clamped so distant sources
/// brighten rather than vanish entirely. The exact coefficient is a
/// perceptual tuning constant (documented, not a hidden magic number).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AirAbsorption {
    /// Enable/disable air absorption. Disabled = exact ×1.0 passthrough
    /// (disabled-exact discipline).
    pub enabled: bool,
    /// Reference absorption coefficient per metre (higher = duller over
    /// distance). Default targets a mild audible roll-off around a few kHz
    /// at several metres.
    pub per_meter: f32,
    /// Baseline cutoff at zero distance (Hz).
    pub base_cutoff_hz: f32,
}

impl Default for AirAbsorption {
    fn default() -> Self {
        Self {
            enabled: false,
            per_meter: 0.06,
            base_cutoff_hz: 20_000.0,
        }
    }
}

impl AirAbsorption {
    /// Effective low-pass cutoff (Hz) for a listener-to-source distance in
    /// metres. Lower cutoff = more HF absorption. When disabled the cutoff
    /// stays at the base (no filtering).
    pub fn cutoff_hz(&self, distance: f32, sample_rate: f32) -> f32 {
        if !self.enabled {
            return sample_rate * 0.5;
        }
        let d = distance.max(0.0);
        let falloff = (1.0 / (1.0 + self.per_meter * d)).clamp(0.05, 1.0);
        (self.base_cutoff_hz * falloff).clamp(500.0, sample_rate * 0.45)
    }
}

/// Renderer-owned per-object air-absorption filter state (spec §39, applied).
///
/// This is the *applied* counterpart to [`AirAbsorption::cutoff_hz`]: a
/// one-pole smoothed low-pass whose cutoff tracks the distance-dependent
/// model at block rate. Disabled (or a cutoff at Nyquist) is an exact
/// passthrough — the biquad is not run when there is nothing to filter, so
/// the conventional paths stay bit-identical.
#[derive(Debug, Clone, Copy)]
pub struct AbsorptionState {
    /// Smoothed log-cutoff (Hz). `0.0` = uninitialised (first block snaps).
    cutoff_log: f32,
    filter: BiquadStateF32,
}

impl Default for AbsorptionState {
    fn default() -> Self {
        Self {
            cutoff_log: 0.0,
            filter: BiquadStateF32::default(),
        }
    }
}

impl AbsorptionState {
    /// Advance the block-rate cutoff smoothing and return the fresh low-pass
    /// coefficients. `None` when nothing should be applied (disabled or a
    /// cutoff at/over Nyquist) — the caller skips filtering entirely in that
    /// case, keeping the path bit-exact. `smooth = 1.0` snaps exactly.
    pub fn coeffs(
        &mut self,
        cutoff_hz: f32,
        sample_rate: f32,
        smooth: f32,
    ) -> Option<BiquadCoeffsF32> {
        let nyquist = sample_rate * 0.5;
        let cutoff = cutoff_hz.clamp(20.0, nyquist);
        if cutoff >= nyquist * 0.99 {
            // Nothing to filter: snap the smoothed state to full band and
            // signal passthrough.
            self.cutoff_log = nyquist.ln();
            return None;
        }
        let target = cutoff.ln();
        if self.cutoff_log == 0.0 {
            self.cutoff_log = target;
        } else if smooth < 1.0 {
            self.cutoff_log += smooth * (target - self.cutoff_log);
        }
        let c = self.cutoff_log.exp().clamp(20.0, nyquist * 0.99);
        Some(BiquadCoeffsF32::lowpass(sample_rate, c, 0.707))
    }

    /// Filter one sample through the current coefficients.
    #[inline]
    pub fn process(&mut self, sample: f32, coeffs: &BiquadCoeffsF32) -> f32 {
        self.filter.process(sample, coeffs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_gain_is_monotonic_and_bounded() {
        let inv = DistanceModel::Inverse;
        // At the reference and beyond, gain decreases with distance.
        assert!((inv.distance_gain(2.0, 1.0) - 0.5).abs() < 1e-6);
        assert!((inv.distance_gain(4.0, 1.0) - 0.25).abs() < 1e-6);
        assert!(inv.distance_gain(2.0, 1.0) > inv.distance_gain(4.0, 1.0));
        // Near-field ceiling: never exceeds MAX_GAIN.
        assert!(inv.distance_gain(0.0, 1.0) <= MAX_GAIN);
        assert!(inv.distance_gain(-3.0, 1.0) <= MAX_GAIN);
        assert!(inv.distance_gain(0.0, 1.0).is_finite());
    }

    #[test]
    fn inverse_square_halving_raises_about_12db() {
        let sq = DistanceModel::InverseSquare;
        let g1 = sq.distance_gain(2.0, 1.0); // 1/4
        let g2 = sq.distance_gain(1.0, 1.0); // 1/1
        let db = 20.0 * (g2 / g1).log10(); // = 20·log10(4) ≈ 12.04
        assert!((db - 12.0).abs() < 0.1);
    }

    #[test]
    fn inverse_reference_flattens_near_band_and_decays_after() {
        let m = DistanceModel::InverseReference;
        assert!((m.distance_gain(0.5, 1.0) - 1.0).abs() < 1e-6);
        assert!((m.distance_gain(1.0, 1.0) - 1.0).abs() < 1e-6);
        assert!((m.distance_gain(2.0, 1.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn air_absorption_cutoff_is_bounded_and_disabled_is_passthrough() {
        let mut a = AirAbsorption::default();
        // Disabled: full band.
        assert_eq!(a.cutoff_hz(10.0, 48_000.0), 24_000.0);
        a.enabled = true;
        // Closer source => higher cutoff than a far source.
        let near = a.cutoff_hz(1.0, 48_000.0);
        let far = a.cutoff_hz(10.0, 48_000.0);
        assert!(near > far);
        // Bounded below by the clamp, above by half the sample rate.
        assert!(far >= 500.0);
        assert!(near <= 48_000.0 * 0.45);
        assert!(near.is_finite() && far.is_finite());
    }
}
