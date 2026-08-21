//! Graphic equaliser — a fixed-band slider layer compiled into the same
//! parametric biquad engine the PEQ uses (§9.1).
//!
//! # Model
//!
//! A [`GraphicEq`] is a *model*, not a second DSP stage: it owns a band
//! layout ([`config::GraphicEqLayout`]), one slider gain per band, a preamp,
//! and an enabled flag. [`GraphicEq::sync_into`] compiles that model into an
//! existing [`ParametricEq`] through `set_band`/`set_preamp_db`, so:
//!
//! * there is exactly one EQ stage in the signal path (the DSP graph stays
//!   explicit — the bit-perfect report's `eq_bypassed` verdict is unchanged),
//! * slider changes ride the `SmoothedBiquad` parameter smoothing already
//!   proven click-free by the PEQ tests,
//! * the compiler is *predictable*: identical layout + gains + preamp always
//!   produce identical band parameters (Gate B — "Graphic EQ compiles to
//!   predictable filters").
//!
//! # Band semantics
//!
//! The first and last bands compile to low/high shelves; interior bands are
//! peaking filters at the layout's fixed frequencies with the standard
//! bandwidth-derived Q (`Q = 1/(2·sinh(ln 2·B/2))`, B in octaves — 4.31 for
//! 1/3-octave, 8.65 for 1/6-octave). A slider at 0 dB compiles to a disabled
//! band so it costs nothing in the hot path.

use crate::dsp::equalizer::{EqBandParams, EqFilterType, ParametricEq};
use config::GraphicEqLayout;

/// Maximum slider excursion in dB. Keeps a careless drag from asking the
/// biquads for absurd gains while leaving headroom for any target curve.
pub const GRAPHIC_EQ_MAX_GAIN_DB: f32 = 24.0;
/// Slider gains below this magnitude are treated as 0 dB (band disabled).
pub const GRAPHIC_EQ_SLIDER_EPSILON_DB: f32 = 0.01;

/// Graphic EQ model. See the module docs for the compile semantics.
#[derive(Debug, Clone)]
pub struct GraphicEq {
    layout: GraphicEqLayout,
    gains_db: Vec<f32>,
    preamp_db: f32,
    enabled: bool,
}

impl Default for GraphicEq {
    fn default() -> Self {
        Self::new(GraphicEqLayout::TenBand)
    }
}

impl GraphicEq {
    /// Create a graphic EQ with the given layout and all sliders at 0 dB.
    pub fn new(layout: GraphicEqLayout) -> Self {
        let n = layout.num_bands();
        Self {
            layout,
            gains_db: vec![0.0; n],
            preamp_db: 0.0,
            enabled: false,
        }
    }

    /// Build a graphic EQ model from its persisted configuration.
    pub fn from_config(cfg: &config::GraphicEqConfig) -> Self {
        let n = cfg.layout.num_bands();
        let mut gains = cfg.gains_db.clone();
        gains.truncate(n);
        gains.resize(n, 0.0);
        Self {
            layout: cfg.layout.clone(),
            gains_db: gains,
            preamp_db: cfg.preamp_db,
            enabled: cfg.enabled,
        }
    }

    /// The active band layout.
    pub fn layout(&self) -> &GraphicEqLayout {
        &self.layout
    }

    /// Number of bands in the active layout.
    pub fn num_bands(&self) -> usize {
        self.gains_db.len()
    }

    /// The layout's fixed center frequencies in Hz.
    pub fn frequencies(&self) -> Vec<f32> {
        self.layout.frequencies()
    }

    /// The layout's bandwidth-derived Q factor.
    pub fn band_q(&self) -> f32 {
        self.layout.q()
    }

    /// Current slider gains, one per band, in dB.
    pub fn gains(&self) -> &[f32] {
        &self.gains_db
    }

    /// The current slider gain of one band in dB.
    pub fn slider_gain(&self, band: usize) -> f32 {
        self.gains_db.get(band).copied().unwrap_or(0.0)
    }

    /// Preamp gain in dB (applied before the EQ curve, as in the PEQ).
    pub fn preamp_db(&self) -> f32 {
        self.preamp_db
    }

    /// Whether the graphic EQ is active.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Replace the layout, resetting every slider to 0 dB. Band count
    /// changes take effect here (the model owns the slider vector).
    pub fn set_layout(&mut self, layout: GraphicEqLayout) {
        let n = layout.num_bands();
        self.layout = layout;
        self.gains_db = vec![0.0; n];
    }

    /// Set one slider gain in dB, clamped to ±[`GRAPHIC_EQ_MAX_GAIN_DB`].
    /// Out-of-range band indices are ignored.
    pub fn set_slider(&mut self, band: usize, gain_db: f32) {
        if !gain_db.is_finite() {
            log::warn!(
                "GraphicEq::set_slider: non-finite value {}; ignoring",
                gain_db
            );
            return;
        }
        if let Some(slot) = self.gains_db.get_mut(band) {
            *slot = gain_db.clamp(-GRAPHIC_EQ_MAX_GAIN_DB, GRAPHIC_EQ_MAX_GAIN_DB);
        }
    }

    /// Set the preamp gain in dB, clamped to ±30 dB (the PEQ's own clamp).
    pub fn set_preamp_db(&mut self, preamp_db: f32) {
        if !preamp_db.is_finite() {
            log::warn!(
                "GraphicEq::set_preamp_db: non-finite value {}; ignoring",
                preamp_db
            );
            return;
        }
        self.preamp_db = preamp_db.clamp(-30.0, 30.0);
    }

    /// Enable or disable the graphic EQ.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Compile this model into `eq` (the pipeline's parametric stage).
    ///
    /// Every band is written via `set_band` (smooth, click-free) and the
    /// preamp via `set_preamp_db`; the EQ's enabled flag is set from the
    /// model. Bands at 0 dB are written disabled so the hot path skips them.
    /// This is the deterministic compiler the acceptance tests verify.
    pub fn sync_into(&self, eq: &mut ParametricEq) {
        eq.set_preamp_db(self.preamp_db);
        let freqs = self.frequencies();
        let q = self.band_q();
        let last = freqs.len().saturating_sub(1);
        for (i, freq) in freqs.iter().enumerate() {
            let gain = self.gains_db.get(i).copied().unwrap_or(0.0);
            let filter_type = if i == 0 {
                EqFilterType::LowShelf
            } else if i == last {
                EqFilterType::HighShelf
            } else {
                EqFilterType::Peaking
            };
            eq.set_band(
                i,
                EqBandParams {
                    frequency: *freq,
                    gain_db: gain,
                    q,
                    filter_type,
                    enabled: gain.abs() > GRAPHIC_EQ_SLIDER_EPSILON_DB,
                },
            );
        }
        eq.set_enabled(self.enabled);
    }

    /// Compile this model into a fresh parametric EQ at `sample_rate`.
    ///
    /// Deterministic: the same model and sample rate always produce the same
    /// band parameters. The fresh EQ has no filter state, so this is only
    /// suitable for (re)building the EQ on a layout/band-count change — live
    /// slider moves must go through [`Self::sync_into`] to stay click-free.
    pub fn compile(&self, sample_rate: f32) -> ParametricEq {
        let mut eq = ParametricEq::new(self.num_bands().max(1), sample_rate);
        self.sync_into(&mut eq);
        eq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::GraphicEqLayout;

    fn settled_eq(eq: &mut ParametricEq, iterations: usize) {
        for _ in 0..iterations {
            eq.process(0.5, 0.5);
        }
    }

    #[test]
    fn layout_band_counts_and_ladders() {
        assert_eq!(GraphicEqLayout::TenBand.num_bands(), 10);
        assert_eq!(GraphicEqLayout::FifteenBand.num_bands(), 15);
        assert_eq!(GraphicEqLayout::ThirtyOneBand.num_bands(), 31);
        assert_eq!(GraphicEqLayout::ThirtyTwoBand.num_bands(), 32);
        assert_eq!(GraphicEqLayout::SixtyFourBand.num_bands(), 64);
        assert_eq!(
            GraphicEqLayout::Custom(vec![1000.0, 250.0, 500.0]).num_bands(),
            3
        );

        let f = GraphicEqLayout::TenBand.frequencies();
        assert!((f[0] - 31.5).abs() < 1e-3);
        assert!((f[9] - 16000.0).abs() < 1e-3);
        // 64-band ladder starts at 20 Hz and steps by exactly 2^(1/6).
        let f64 = GraphicEqLayout::SixtyFourBand.frequencies();
        assert!((f64[0] - 20.0).abs() < 1e-3);
        assert!((f64[6] - 40.0).abs() < 1e-3);
        // Custom ladder is sorted, deduped, finite-filtered.
        let c = GraphicEqLayout::Custom(vec![1000.0, 250.0, 250.0, -5.0, f32::NAN]);
        let cf = c.frequencies();
        assert_eq!(cf, vec![250.0, 1000.0]);
    }

    #[test]
    fn bandwidth_derived_q_is_standard() {
        // 1/3-octave → Q ≈ 4.31; 1/6-octave → Q ≈ 8.65 (the PEQ 64-band math).
        let q13 = GraphicEqLayout::ThirtyOneBand.q();
        assert!((q13 - 4.31).abs() < 0.05, "1/3-octave Q ≈ 4.31, got {q13}");
        let q16 = GraphicEqLayout::SixtyFourBand.q();
        assert!((q16 - 8.65).abs() < 0.1, "1/6-octave Q ≈ 8.65, got {q16}");
    }

    #[test]
    fn compile_is_deterministic_and_predictable() {
        let mut g = GraphicEq::new(GraphicEqLayout::ThirtyOneBand);
        g.set_slider(10, 6.0);
        g.set_slider(20, -4.5);
        g.set_preamp_db(-2.0);
        g.set_enabled(true);

        let eq_a = g.compile(48000.0);
        let eq_b = g.compile(48000.0);
        for i in 0..31 {
            let pa = eq_a.band_params(i).unwrap();
            let pb = eq_b.band_params(i).unwrap();
            assert_eq!(pa.frequency, pb.frequency);
            assert_eq!(pa.gain_db, pb.gain_db);
            assert_eq!(pa.q, pb.q);
            assert_eq!(pa.filter_type, pb.filter_type);
        }

        // Predictable filters: the slider lands on the layout's exact
        // frequency with the bandwidth-derived Q, shelves on the endpoints.
        let p10 = eq_a.band_params(10).unwrap();
        let f = GraphicEqLayout::ThirtyOneBand.frequencies();
        assert!((p10.frequency - f[10]).abs() < 1e-3);
        assert!((p10.q - GraphicEqLayout::ThirtyOneBand.q()).abs() < 1e-6);
        assert_eq!(p10.filter_type, EqFilterType::Peaking);
        assert_eq!(
            eq_a.band_params(0).unwrap().filter_type,
            EqFilterType::LowShelf
        );
        assert_eq!(
            eq_a.band_params(30).unwrap().filter_type,
            EqFilterType::HighShelf
        );
        // Disabled 0 dB sliders compile to disabled bands.
        assert!(!eq_a.band_params(5).unwrap().enabled);
        assert!(eq_a.band_params(10).unwrap().enabled);
        // Preamp is propagated.
        assert!((eq_a.preamp_db() - (-2.0)).abs() < 1e-6);
    }

    #[test]
    fn flat_sliders_are_unity_after_settling() {
        let mut g = GraphicEq::new(GraphicEqLayout::TenBand);
        g.set_enabled(true);
        let mut eq = g.compile(44100.0);
        settled_eq(&mut eq, 2000);
        let (l, r) = eq.process(0.5, 0.5);
        assert!((l - 0.5).abs() < 0.02, "flat curve unity, got {l}");
        assert!((r - 0.5).abs() < 0.02);
    }

    /// Measure the steady-state amplitude (dB) of a sine through `eq` after
    /// `settle_samples` of settling. Peak-detects over a full period so the
    /// sample phase never skews the reading.
    fn measure_sine_db(eq: &mut ParametricEq, freq_hz: f32, sr: f32, settle_samples: usize) -> f32 {
        let mut phase = 0.0f32;
        let mut peak = 0.0f32;
        let n = settle_samples + (sr / freq_hz).ceil() as usize * 2;
        for i in 0..n {
            let s = (phase * std::f32::consts::TAU).sin();
            phase += freq_hz / sr;
            let (l, _) = eq.process(s, s);
            if i >= settle_samples {
                peak = peak.max(l.abs());
            }
        }
        20.0 * peak.max(1e-9).log10()
    }

    #[test]
    fn boosted_band_measures_at_center_and_neutral_two_octaves_away() {
        let mut g = GraphicEq::new(GraphicEqLayout::ThirtyOneBand);
        g.set_enabled(true);
        // +6 dB on the 1 kHz band.
        let f = GraphicEqLayout::ThirtyOneBand.frequencies();
        let idx = f.iter().position(|&x| (x - 1000.0).abs() < 1.0).unwrap();
        g.set_slider(idx, 6.0);
        let mut eq = g.compile(48000.0);

        let center_db = measure_sine_db(&mut eq, 1000.0, 48000.0, 4000);
        assert!(
            (center_db - 6.0).abs() < 0.5,
            "1 kHz boost should measure ≈ +6 dB, got {center_db:.2} dB"
        );

        // Two octaves away (250 Hz) the peaking filter is ≈ unity.
        let away_db = measure_sine_db(&mut eq, 250.0, 48000.0, 4000);
        assert!(
            away_db.abs() < 1.0,
            "250 Hz should be ≈ unity, got {away_db:.2} dB"
        );
    }

    #[test]
    fn slider_sweep_is_click_free() {
        let mut g = GraphicEq::new(GraphicEqLayout::TenBand);
        g.set_enabled(true);
        let mut eq = g.compile(44100.0);
        settled_eq(&mut eq, 500);
        // Sweep the 1 kHz slider from 0 → +12 dB in 0.5 dB steps. Use a low
        // frequency (60 Hz) so the sine's own per-sample slew (~0.004 at
        // amplitude 0.5) stays far below the click threshold; a real
        // discontinuity would jump by ≫ the smoothing ramp contribution.
        let mut prev_l = 0.0f32;
        let mut max_delta = 0.0f32;
        let mut phase = 0.0f32;
        for step in 0..=24 {
            g.set_slider(5, step as f32 * 0.5);
            g.sync_into(&mut eq);
            for _ in 0..128 {
                let s = 0.5 * (phase * std::f32::consts::TAU).sin();
                phase += 60.0 / 44100.0;
                let (l, _) = eq.process(s, s);
                max_delta = max_delta.max((l - prev_l).abs());
                prev_l = l;
            }
        }
        assert!(
            max_delta < 0.02,
            "slider sweep must be click-free; max sample-to-sample delta {max_delta:.4}"
        );
    }

    #[test]
    fn slider_clamping_and_finite_guards() {
        let mut g = GraphicEq::new(GraphicEqLayout::TenBand);
        g.set_slider(0, 100.0);
        assert_eq!(g.slider_gain(0), GRAPHIC_EQ_MAX_GAIN_DB);
        g.set_slider(0, -100.0);
        assert_eq!(g.slider_gain(0), -GRAPHIC_EQ_MAX_GAIN_DB);
        g.set_slider(0, f32::NAN);
        assert_eq!(g.slider_gain(0), -GRAPHIC_EQ_MAX_GAIN_DB); // unchanged
        g.set_slider(99, 5.0); // out of range: ignored
        assert_eq!(g.num_bands(), 10);
        g.set_preamp_db(f32::INFINITY);
        assert_eq!(g.preamp_db(), 0.0); // rejected, not clamped
    }

    #[test]
    fn layout_change_resizes_sliders() {
        let mut g = GraphicEq::new(GraphicEqLayout::TenBand);
        g.set_slider(9, 3.0);
        g.set_layout(GraphicEqLayout::SixtyFourBand);
        assert_eq!(g.num_bands(), 64);
        assert_eq!(g.slider_gain(9), 0.0); // reset on layout change
        assert_eq!(g.slider_gain(63), 0.0);
    }
}
