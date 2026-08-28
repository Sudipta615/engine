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

/// Renderer-owned per-object occlusion filter state (realtime-safe).
#[derive(Debug, Clone, Copy)]
pub struct OcclusionState {
    /// Smoothed log-cutoff (Hz). `0.0` means uninitialised (first block).
    cutoff_log: f32,
    /// The actual one-pole low-pass biquad state.
    filter: BiquadStateF32,
}

impl Default for OcclusionState {
    fn default() -> Self {
        Self {
            cutoff_log: 0.0,
            filter: BiquadStateF32::default(),
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
}
