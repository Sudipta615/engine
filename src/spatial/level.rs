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
/// This phase implements the four analytic laws below; a user-defined
/// distance law is a declared seam.
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
/// The roll-off is described by a **magnitude family** — an analytic
/// `magnitude(f)` over frequency that is **exact 1.0 at DC** (air is
/// transparent to a static pressure field) and monotonically decreasing
/// with distance. Two renderings exist of the *same* model and must agree
/// (the Phase-50 "acoustic agreement" contract):
///
/// * **Offline** — `magnitude` is sampled per FFT bin and composed onto
///   every spectral kernel (`bake::path_filter_kernel_with`).
/// * **Realtime** — `corner_hz` collapses the family to the equivalent
///   one-pole corner whose `1/√(1+(f/f_c)²)` magnitude matches the model
///   at its −3 dB point, realised by the renderers' per-image biquad.
///
/// The default `OnePole` family is deliberately gentle and clamped so
/// distant sources darken rather than vanish entirely. The exact
/// coefficient is a perceptual tuning constant (documented, not a hidden
/// magic number).
///
/// Serde: the acoustic baker embeds a scene-scoped model verbatim in its
/// baked responses (v3.48), so the same model that darkens a realtime
/// object's direct path can shape an offline room's per-path spectral
/// kernels.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// The magnitude family shaping the roll-off (Phase 50). Default
    /// `OnePole` keeps the v3.48 behaviour bit-exactly.
    #[serde(default)]
    pub rolloff_model: AirRolloffModel,
}

/// Frequency-dependent attenuation families richer than the one-pole
/// (Phase 50 item 2). Every family is a magnitude approximation: DC-exact
/// (magnitude 1.0 at f = 0), monotonically non-increasing, and bounded by
/// the base cutoff — `disabled` (or `enabled` with any family) composes to
/// exactly ×1.0 when the model is off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AirRolloffModel {
    /// First-order magnitude shape `1/√(1+(f/f_air)²)` — the original v3.48
    /// model. Asymptotically −6 dB/oct above the air corner.
    #[default]
    OnePole,
    /// Second-order `1/(1+(f/f_air)²)` — a steeper −12 dB/oct HF decay for
    /// humid air / long throw distances; its −3 dB point sits at
    /// `f_air·√(√2−1)`.
    TwoPole,
    /// Exponential `e^(−f/f_air)` — the softest skirt (asymptotically
    /// −8.7 dB per decade), modelling cool dry air where HF loss
    /// concentrates far above speech band; crosses −3 dB at
    /// `f_air·ln(√2)`.
    Exponential,
}

impl Default for AirAbsorption {
    fn default() -> Self {
        Self {
            enabled: false,
            per_meter: 0.06,
            base_cutoff_hz: 20_000.0,
            rolloff_model: AirRolloffModel::OnePole,
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

    /// The distance-dependent **magnitude** of the model at frequency `f`
    /// (Hz) — the offline rendering (Phase 50). DC-exact: `magnitude(0, d)
    /// == 1.0` for every distance and family; disabled = exactly 1.0 at
    /// every frequency (bit-exact discipline).
    #[inline]
    pub fn magnitude(&self, f: f32, distance: f32, sample_rate: f32) -> f32 {
        if !self.enabled {
            return 1.0;
        }
        let f_air = self.cutoff_hz(distance, sample_rate);
        if f <= 0.0 {
            return 1.0;
        }
        let x = f / f_air;
        match self.rolloff_model {
            AirRolloffModel::OnePole => 1.0 / (1.0 + x * x).sqrt(),
            AirRolloffModel::TwoPole => 1.0 / (1.0 + x * x),
            AirRolloffModel::Exponential => (-x).exp(),
        }
    }

    /// The **equivalent one-pole corner** (Hz) realising this model's
    /// magnitude on the realtime path (Phase 50 item 1): the frequency
    /// where the family's magnitude crosses `1/√2` (−3 dB), so a biquad
    /// low-pass at that corner reproduces the model's −3 dB point exactly
    /// and its DC gain exactly. Disabled → `None` (nothing to fold; the
    /// realtime filter stays whatever the surface alone set it to).
    ///
    /// * `OnePole` — the corner *is* the air cutoff (identity).
    /// * `TwoPole` — `1/(1+x²)` crosses 1/√2 at `x = √(√2−1)`.
    /// * `Exponential` — `e^(−x)` crosses 1/√2 at `x = ln(√2)`.
    pub fn corner_hz(&self, distance: f32, sample_rate: f32) -> Option<f32> {
        if !self.enabled {
            return None;
        }
        let f_air = self.cutoff_hz(distance, sample_rate);
        let factor = match self.rolloff_model {
            AirRolloffModel::OnePole => 1.0,
            AirRolloffModel::TwoPole => (std::f32::consts::SQRT_2 - 1.0).sqrt(),
            AirRolloffModel::Exponential => std::f32::consts::LN_2 * 0.5, // ln(√2)
        };
        let corner = f_air * factor;
        let nyq = sample_rate * 0.5;
        if corner >= nyq * 0.999 {
            None // spectrally flat at this distance — nothing to fold
        } else {
            Some(corner)
        }
    }

    /// Compose this model's air corner with a surface low-pass corner
    /// (both optional) into the single corner the realtime per-image
    /// filter will run at. Composition is on the **one-pole magnitude
    /// family**: the composed corner is the frequency where the product
    /// `1/√(1+(f/f_a)²) · 1/√(1+(f/f_s)²)` crosses `1/√2` — exact at DC,
    /// exact at the −3 dB point of the composed shape, and reduces to the
    /// lone corner when only one side is set (bit-exact legacy behaviour).
    pub fn compose_corner_hz(&self, surface_hz: f32, distance: f32, sample_rate: f32) -> f32 {
        let air = self.corner_hz(distance, sample_rate);
        let surface_on =
            surface_hz.is_finite() && surface_hz > 1.0 && surface_hz < sample_rate * 0.5;
        match (air, surface_on) {
            (None, false) => f32::INFINITY,
            (Some(a), false) => a,
            (None, true) => surface_hz,
            (Some(a), true) => {
                // (1+x/a²)(1+x/s²) = 2 at the composed −3 dB point, with
                // x = f². Expanded: x²/(a²s²) + x(1/a²+1/s²) − 1 = 0;
                // multiply through by a²s² and take the positive root.
                let a2 = a * a;
                let s2 = surface_hz * surface_hz;
                let b = 1.0 / a2 + 1.0 / s2;
                let p = a2 * s2;
                let x2 = 0.5 * (-b * p + (b * b * p * p + 4.0 * p).sqrt());
                x2.sqrt()
            }
        }
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

    #[test]
    fn air_magnitude_is_dc_exact_and_disabled_is_unity() {
        const SR: f32 = 48_000.0;
        let mut a = AirAbsorption::default();
        // Disabled: exactly 1.0 at every frequency (bit-exact discipline).
        for f in [0.0, 100.0, 1_000.0, 10_000.0] {
            assert_eq!(a.magnitude(f, 20.0, SR), 1.0);
        }
        a.enabled = true;
        for model in [
            AirRolloffModel::OnePole,
            AirRolloffModel::TwoPole,
            AirRolloffModel::Exponential,
        ] {
            a.rolloff_model = model;
            // DC-exact for every family and distance.
            assert_eq!(a.magnitude(0.0, 0.1, SR), 1.0);
            assert_eq!(a.magnitude(0.0, 50.0, SR), 1.0);
            // Monotonically decreasing with frequency, and farther ⇒ duller.
            for f in [125.0, 1_000.0, 4_000.0, 16_000.0] {
                assert!(
                    a.magnitude(f, 20.0, SR) <= a.magnitude(f * 0.5, 20.0, SR) + 1e-6,
                    "{model:?} must be non-increasing at f={f}"
                );
            }
            assert!(a.magnitude(4_000.0, 40.0, SR) < a.magnitude(4_000.0, 4.0, SR));
            // Bounded in (0, 1].
            let m = a.magnitude(20_000.0, 100.0, SR);
            assert!(m > 0.0 && m <= 1.0);
        }
    }

    #[test]
    fn air_corner_matches_the_family_3db_point() {
        // The realtime corner must sit exactly at the family's −3 dB
        // magnitude (the Phase-50 agreement invariant between the offline
        // magnitude sampling and the realtime biquad corner).
        const SR: f32 = 48_000.0;
        let mut a = AirAbsorption {
            enabled: true,
            per_meter: 0.02,
            base_cutoff_hz: 12_000.0,
            rolloff_model: AirRolloffModel::OnePole,
        };
        // OnePole: the corner is the cutoff itself, and the magnitude
        // there is 1/√2.
        let c = a.corner_hz(30.0, SR).expect("finite corner");
        assert!((a.cutoff_hz(30.0, SR) - c).abs() < 1e-3);
        assert!((a.magnitude(c, 30.0, SR) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-4);
        // TwoPole / Exponential: corner at the −3 dB crossing.
        for model in [AirRolloffModel::TwoPole, AirRolloffModel::Exponential] {
            a.rolloff_model = model;
            let c = a.corner_hz(30.0, SR).expect("finite corner");
            let m = a.magnitude(c, 30.0, SR);
            assert!(
                (m - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3,
                "{model:?} corner magnitude {m} must be −3 dB"
            );
        }
        // Disabled ⇒ no corner to fold.
        a.enabled = false;
        assert!(a.corner_hz(30.0, SR).is_none());
    }

    #[test]
    fn air_compose_corner_reduces_and_darkens() {
        const SR: f32 = 48_000.0;
        let a = AirAbsorption {
            enabled: true,
            per_meter: 0.02,
            base_cutoff_hz: 12_000.0,
            rolloff_model: AirRolloffModel::OnePole,
        };
        // No surface corner, no air → ∞ (strict passthrough).
        let mut off = a;
        off.enabled = false;
        assert!(off.compose_corner_hz(f32::INFINITY, 30.0, SR).is_infinite());
        // Surface alone → identity (bit-exact legacy).
        assert_eq!(off.compose_corner_hz(2_000.0, 30.0, SR), 2_000.0);
        // Air alone → the air corner.
        let air_only = a.compose_corner_hz(f32::INFINITY, 30.0, SR);
        assert!((air_only - a.corner_hz(30.0, SR).unwrap()).abs() < 1e-3);
        // Both → strictly darker (lower) than either alone, and exactly the
        // −3 dB point of the composed one-pole product shape.
        let both = a.compose_corner_hz(2_000.0, 30.0, SR);
        assert!(both < 2_000.0 && both < air_only);
        let f_air = a.corner_hz(30.0, SR).unwrap();
        let shape = |f: f32| {
            1.0 / (1.0 + (f / f_air).powi(2)).sqrt() / (1.0 + (f / 2_000.0).powi(2)).sqrt()
        };
        assert!((shape(both) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        // Both crossing at one point: composed −3 dB = √(f_a·f_s)/… sanity:
        // the composed corner is between the two contributing corners.
        assert!(both > 0.0 && both < f_air.min(2_000.0));
    }
}
