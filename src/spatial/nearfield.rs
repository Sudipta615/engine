//! Near-field correction (spec §40).
//!
//! Very close sources behave differently than far sources: the proximity
//! effect lifts low frequencies, the inverse-square falloff of a point source
//! diverges, and a real head produces nearer-field interaural cues. This
//! module ships a bounded, independently-testable near-field model that the
//! renderers fold into the object level chain:
//!
//! - **Proximity gain** — lifts a close source toward a configured ceiling,
//!   clamping the raw distance law's `1/d` divergence so a source hugging the
//!   listener boosts rather than blowing past full scale.
//! - **Low-frequency proximity boost** — the classic "near-field LF lift",
//!   implemented as a low-shelf biquad whose boost scales with proximity.
//!
//! ## Realtime discipline
//!
//! [`NearFieldState`] is the renderer-owned per-object filter state (a
//! low-shelf biquad + a smoothed boost dB), mirroring the occlusion state:
//! coefficients are recomputed at block rate and the boost is one-pole
//! smoothed, so automated distance changes ramp instead of zippering.
//! Disabled (the default) is a bit-exact ×1.0 passthrough in both gain and
//! filter, matching the codebase's "disabled-exact" discipline.
//!
//! Near-field interaural differences (larger ITD growth, head/torso shadowing)
//! are the binaural renderer's domain; this module owns the level and spectral
//! part so it stays renderer-independent (spec §152).

use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};

/// Reference distance (m) at which near-field correction becomes active.
/// Sources closer than this start to lift.
pub const DEFAULT_NEAR_REFERENCE_M: f32 = 0.5;

/// Maximum LF proximity boost (dB) at zero distance.
pub const DEFAULT_MAX_LF_BOOST_DB: f32 = 8.0;

/// Cutoff (Hz) shared by the LF proximity shelf.
pub const NEAR_FIELD_SHELF_HZ: f32 = 220.0;

/// Near-field correction model (spec §40), disabled by default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NearField {
    /// Enable near-field gain + LF boost. Disabled = exact ×1.0 passthrough.
    pub enabled: bool,
    /// Reference distance (m) under which correction starts (`1.0` when this
    /// is non-positive).
    pub reference_distance: f32,
    /// Maximum proximity-gain lift (linear, ≥ 1.0) at zero distance.
    pub max_proximity_gain: f32,
    /// Maximum LF boost (dB) at zero distance; interpolated to 0 at the
    /// reference distance.
    pub max_lf_boost_db: f32,
}

impl Default for NearField {
    fn default() -> Self {
        Self {
            enabled: false,
            reference_distance: DEFAULT_NEAR_REFERENCE_M,
            max_proximity_gain: 2.0,
            max_lf_boost_db: DEFAULT_MAX_LF_BOOST_DB,
        }
    }
}

impl NearField {
    /// The proximity factor `f ∈ [0,1]`: `1 − d/ref`, clamped. `1.0` for a
    /// source at the listener, `0.0` at/above the reference.
    fn factor(&self, distance: f32) -> f32 {
        let ref_d = if self.reference_distance > 0.0 {
            self.reference_distance
        } else {
            1.0
        };
        (1.0 - distance.max(0.0) / ref_d).clamp(0.0, 1.0)
    }

    /// Bounded proximity gain for a listener-to-source distance `d` in
    /// metres. Returns `1.0` (no effect) when disabled. Always finite.
    pub fn proximity_gain(&self, distance: f32) -> f32 {
        if !self.enabled {
            return 1.0;
        }
        let f = self.factor(distance);
        1.0 + (self.max_proximity_gain.max(1.0) - 1.0) * f
    }

    /// Linear LF-shelf lift (≥ 1.0) at `distance`, interpolated from 0 at
    /// the reference to the max boost at zero distance. `1.0` when disabled.
    pub fn lf_boost_gain(&self, distance: f32) -> f32 {
        if !self.enabled {
            return 1.0;
        }
        let f = self.factor(distance);
        10.0_f32.powf(self.max_lf_boost_db.max(0.0) / 20.0 * f)
    }
}

/// Renderer-owned per-object near-field filter state (realtime-safe).
#[derive(Debug, Clone, Copy)]
pub struct NearFieldState {
    /// Smoothed shelf boost (dB). `0.0` = uninitialised (first block snaps).
    boost_db: f32,
    /// The biquad state applying the shelf.
    filter: BiquadStateF32,
}

impl Default for NearFieldState {
    fn default() -> Self {
        Self {
            boost_db: 0.0,
            filter: BiquadStateF32::default(),
        }
    }
}

impl NearFieldState {
    /// Advance the block-rate boost smoothing and return the LF low-shelf
    /// coefficients for `boost_db`. A ~0 boost returns the identity filter.
    pub fn shelf_coeffs(
        &mut self,
        boost_db: f32,
        sample_rate: f32,
        smooth: f32,
    ) -> BiquadCoeffsF32 {
        let target = boost_db.clamp(0.0, 24.0);
        let b = if smooth >= 1.0 || self.boost_db < 1e-6 {
            target
        } else {
            self.boost_db + smooth * (target - self.boost_db)
        };
        self.boost_db = b;
        if b < 1e-4 {
            BiquadCoeffsF32::identity()
        } else {
            BiquadCoeffsF32::lowshelf(sample_rate, NEAR_FIELD_SHELF_HZ, b, 0.707)
        }
    }

    /// Filter one sample. Identity (passthrough) while the boost is ~0.
    #[inline]
    pub fn process(&mut self, sample: f32, coeffs: &BiquadCoeffsF32) -> f32 {
        self.filter.process(sample, coeffs)
    }

    /// The current (smoothed) boost in dB.
    pub fn boost_db(&self) -> f32 {
        self.boost_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-3;

    #[test]
    fn disabled_is_unity_in_both_parts() {
        let nf = NearField::default(); // disabled
        assert_eq!(nf.proximity_gain(0.0), 1.0);
        assert_eq!(nf.proximity_gain(1.0), 1.0);
        assert_eq!(nf.lf_boost_gain(0.0), 1.0);
    }

    #[test]
    fn proximity_gain_ramps_in_below_reference() {
        let nf = NearField {
            enabled: true,
            reference_distance: 0.5,
            max_proximity_gain: 2.0,
            max_lf_boost_db: 8.0,
        };
        assert!((nf.proximity_gain(0.5) - 1.0).abs() < EPS);
        assert!((nf.proximity_gain(2.0) - 1.0).abs() < EPS);
        assert!((nf.proximity_gain(0.0) - 2.0).abs() < EPS);
        assert!(nf.proximity_gain(0.1) > nf.proximity_gain(0.3));
        for d in [0.0, 0.05, 0.2, 0.5, 3.0] {
            let g = nf.proximity_gain(d);
            assert!(g.is_finite() && g >= 1.0);
        }
    }

    #[test]
    fn lf_boost_scales_with_proximity() {
        let nf = NearField {
            enabled: true,
            reference_distance: 0.5,
            max_proximity_gain: 2.0,
            max_lf_boost_db: 10.0,
        };
        assert!((nf.lf_boost_gain(0.0) - 10.0f32.powf(10.0 / 20.0)).abs() < EPS);
        assert!((nf.lf_boost_gain(0.5) - 1.0).abs() < EPS);
        // Halfway (d = 0.25): factor 0.5 → 5 dB → √10 linear.
        assert!((nf.lf_boost_gain(0.25) - 10.0f32.powf(0.25)).abs() < EPS);
    }

    #[test]
    fn identity_passthrough_when_boost_zero() {
        let mut st = NearFieldState::default();
        let c = st.shelf_coeffs(0.0, 48_000.0, 1.0);
        let mut y = 0.0f32;
        for _ in 0..8 {
            y = st.process(1.0, &c);
        }
        assert!((y - 1.0).abs() < 1e-5, "passthrough {y}");
    }

    #[test]
    fn shelf_boosts_low_frequencies_and_stays_finite() {
        let mut st = NearFieldState::default();
        let c = st.shelf_coeffs(8.0, 48_000.0, 1.0);
        let fs = 48_000.0;
        let mut out = Vec::with_capacity(fs as usize);
        // A low-frequency (40 Hz) sine at DC-ish well below the shelf cutoff.
        for n in 0..fs as usize {
            let x = (2.0 * std::f32::consts::PI * 40.0 * n as f32 / fs).sin();
            out.push(st.process(x, &c));
        }
        let amp = out[fs as usize - 4096..]
            .iter()
            .fold(0.0f32, |m, &v| m.max(v.abs()));
        // An 8 dB shelf ≈ 2.51× — but the shelf's DC gain is 10^(8/20) ≈ 2.51,
        // and at 40 Hz (well below 220 Hz) the response is near the DC shelf.
        assert!(amp > 1.8, "low boosted: {amp}");
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn boost_smooths_bounded_steps() {
        let mut st = NearFieldState::default();
        // First call snaps to the target.
        let c = st.shelf_coeffs(8.0, 48_000.0, 0.1);
        // Second call toward 0 moves a bounded step (α=0.1 per block).
        let _ = st.shelf_coeffs(0.0, 48_000.0, 0.1);
        assert!(st.boost_db() < 8.0 && st.boost_db() > 0.0);
        let _ = c;
    }
}
