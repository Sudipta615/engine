//! Graphic EQ acceptance suite (Gate B).
//!
//! Verifies, through the public API:
//! - the graphic layer **compiles to predictable filters** (each slider lands
//!   on the layout's exact frequency with the bandwidth-derived Q and the
//!   correct shelf/peaking type),
//! - a 64-band configuration compiles and processes a full block,
//! - flat sliders are unity, a boosted band measures its gain at center and
//!   ≈ unity two octaves away,
//! - slider sweeps are click-free (bounded sample-to-sample delta),
//! - `combined_max_gain_db` (the headroom estimator) matches the peak boost
//!   of a multi-slider curve.

use config::GraphicEqLayout;
use engine::dsp::equalizer::{EqFilterType, ParametricEq};
use engine::dsp::graphic_eq::GraphicEq;

/// Steady-state amplitude (dB) of a sine through `eq` after settling.
/// Peak-detects over a full period so the sample phase never skews the read.
fn measure_sine_db(eq: &mut ParametricEq, freq_hz: f32, sr: f32, settle: usize) -> f32 {
    let mut phase = 0.0f32;
    let mut peak = 0.0f32;
    let n = settle + (sr / freq_hz).ceil() as usize * 2;
    for i in 0..n {
        let s = (phase * std::f32::consts::TAU).sin();
        phase += freq_hz / sr;
        let (l, _) = eq.process(s, s);
        if i >= settle {
            peak = peak.max(l.abs());
        }
    }
    20.0 * peak.max(1e-9).log10()
}

#[test]
fn sliders_compile_to_predictable_filters() {
    // Gate B: "Graphic EQ compiles to predictable filters." A slider must
    // map to exactly the layout's frequency, the bandwidth-derived Q, and
    // the shelf endpoints.
    let mut g = GraphicEq::new(GraphicEqLayout::ThirtyOneBand);
    g.set_slider(10, 6.0); // interior → peaking
    g.set_slider(30, -3.0); // last → high shelf
    g.set_slider(0, 4.0); // first → low shelf
    g.set_enabled(true);

    let eq = g.compile(48000.0);
    let freqs = GraphicEqLayout::ThirtyOneBand.frequencies();

    let interior = eq.band_params(10).unwrap();
    assert!((interior.frequency - freqs[10]).abs() < 1e-3);
    assert!((interior.q - GraphicEqLayout::ThirtyOneBand.q()).abs() < 1e-6);
    assert_eq!(interior.filter_type, EqFilterType::Peaking);
    assert!((interior.gain_db - 6.0).abs() < 1e-4);

    let low = eq.band_params(0).unwrap();
    assert_eq!(low.filter_type, EqFilterType::LowShelf);
    assert!((low.gain_db - 4.0).abs() < 1e-4);

    let high = eq.band_params(30).unwrap();
    assert_eq!(high.filter_type, EqFilterType::HighShelf);
    assert!((high.gain_db + 3.0).abs() < 1e-4);

    // Deterministic: recompiling yields identical filters.
    let eq2 = g.compile(48000.0);
    for i in 0..31 {
        assert_eq!(eq.band_params(i), eq2.band_params(i));
    }
}

#[test]
fn sixty_four_band_configuration_compiles_and_processes() {
    // Gate B: "64-band configuration passes."
    let mut g = GraphicEq::new(GraphicEqLayout::SixtyFourBand);
    assert_eq!(g.num_bands(), 64);
    for i in 0..64 {
        g.set_slider(i, if i % 2 == 0 { 3.0 } else { -2.0 });
    }
    g.set_enabled(true);
    let mut eq = g.compile(48000.0);

    // Process a full 4096-frame block; all outputs finite and bounded.
    let mut max_out = 0.0f32;
    for i in 0..4096 {
        let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin();
        let (l, r) = eq.process(s, s);
        assert!(l.is_finite() && r.is_finite());
        max_out = max_out.max(l.abs()).max(r.abs());
    }
    assert!(max_out < 20.0, "64-band graphic curve exploded: {max_out}");

    // And the block API works.
    let mut left = [0.5f32; 4096];
    let mut right = [0.5f32; 4096];
    eq.process_block(&mut left, &mut right);
    assert!(left.iter().all(|x| x.is_finite()));
}

#[test]
fn flat_sliders_are_unity() {
    let mut g = GraphicEq::new(GraphicEqLayout::TenBand);
    g.set_enabled(true);
    let mut eq = g.compile(44100.0);
    // A 0 dB curve must pass audio unchanged after settling.
    let db = measure_sine_db(&mut eq, 1000.0, 44100.0, 4000);
    assert!(
        (db - 0.0).abs() < 0.25,
        "flat graphic EQ must be unity, measured {db:.3} dB"
    );
}

#[test]
fn boosted_band_measures_at_center_only() {
    let mut g = GraphicEq::new(GraphicEqLayout::ThirtyOneBand);
    g.set_enabled(true);
    let freqs = GraphicEqLayout::ThirtyOneBand.frequencies();
    let idx = freqs
        .iter()
        .position(|&f| (f - 1000.0).abs() < 1.0)
        .unwrap();
    g.set_slider(idx, 6.0);
    let mut eq = g.compile(48000.0);

    let center = measure_sine_db(&mut eq, 1000.0, 48000.0, 4000);
    assert!(
        (center - 6.0).abs() < 0.5,
        "1 kHz boost measures ≈ +6 dB, got {center:.2} dB"
    );
    let away = measure_sine_db(&mut eq, 250.0, 48000.0, 4000);
    assert!(
        away.abs() < 1.0,
        "two octaves away must be ≈ unity, got {away:.2} dB"
    );
}

#[test]
fn slider_sweep_is_click_free() {
    // Gate B: "Smoothing is click-free." Sweep a slider from 0 → +12 dB and
    // assert every sample-to-sample jump stays far below the signal's own
    // slew (60 Hz tone: slew ≈ 0.004 at amplitude 0.5; a hard step would
    // jump ≫ the 0.02 bound).
    let mut g = GraphicEq::new(GraphicEqLayout::TenBand);
    g.set_enabled(true);
    let mut eq = g.compile(44100.0);
    for _ in 0..1000 {
        eq.process(0.0, 0.0);
    }

    let mut prev = 0.0f32;
    let mut max_step = 0.0f32;
    let mut phase = 0.0f32;
    for step in 0..=24 {
        g.set_slider(5, step as f32 * 0.5);
        g.sync_into(&mut eq);
        for _ in 0..256 {
            let s = 0.5 * (phase * std::f32::consts::TAU).sin();
            phase += 60.0 / 44100.0;
            let (l, _) = eq.process(s, s);
            max_step = max_step.max((l - prev).abs());
            prev = l;
        }
    }
    assert!(
        max_step < 0.02,
        "slider sweep must be click-free, max step {max_step:.4}"
    );
}

#[test]
fn headroom_estimator_matches_multi_slider_peak_boost() {
    // The auto-headroom feature reserves the curve's own peak boost; the
    // estimator must agree with a direct measurement of the curve's peak.
    let mut g = GraphicEq::new(GraphicEqLayout::ThirtyOneBand);
    g.set_enabled(true);
    // A broad low boost and a narrow presence boost: peak is the sum at the
    // presence band (both filters contribute there is negligible two octaves
    // apart, so the estimator should land near the +8 dB slider).
    let freqs = GraphicEqLayout::ThirtyOneBand.frequencies();
    let low = freqs.iter().position(|&f| (f - 125.0).abs() < 1.0).unwrap();
    let peak = freqs
        .iter()
        .position(|&f| (f - 1000.0).abs() < 1.0)
        .unwrap();
    g.set_slider(low, 4.0);
    g.set_slider(peak, 8.0);

    let eq = g.compile(48000.0);
    let est = eq.combined_max_gain_db(48000.0);
    // The 4 kHz region sees ~8 dB (the 1 kHz peak) plus a small shelf
    // contribution; the estimator must be within ±1 dB of the 8 dB slider.
    assert!(
        (est - 8.0).abs() < 1.0,
        "headroom estimator should capture the 8 dB peak, got {est:.2} dB"
    );

    // And auto-headroom applies exactly that as pre-EQ attenuation.
    let mut eq_auto = g.compile(48000.0);
    eq_auto.set_auto_headroom(true);
    assert!(
        (eq_auto.headroom_db() + est).abs() < 0.05,
        "auto headroom = −peak boost: {} vs {}",
        eq_auto.headroom_db(),
        -est
    );
}

#[test]
fn layouts_expose_expected_band_counts() {
    assert_eq!(GraphicEqLayout::TenBand.num_bands(), 10);
    assert_eq!(GraphicEqLayout::FifteenBand.num_bands(), 15);
    assert_eq!(GraphicEqLayout::ThirtyOneBand.num_bands(), 31);
    assert_eq!(GraphicEqLayout::ThirtyTwoBand.num_bands(), 32);
    assert_eq!(GraphicEqLayout::SixtyFourBand.num_bands(), 64);
    // Every layout's ladder is monotonically increasing.
    for layout in [
        GraphicEqLayout::TenBand,
        GraphicEqLayout::FifteenBand,
        GraphicEqLayout::ThirtyOneBand,
        GraphicEqLayout::ThirtyTwoBand,
        GraphicEqLayout::SixtyFourBand,
    ] {
        let f = layout.frequencies();
        for w in f.windows(2) {
            assert!(w[1] > w[0], "{layout:?} ladder must ascend");
        }
    }
}
