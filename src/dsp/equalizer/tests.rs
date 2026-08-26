use super::super::{EqBandParams, ParametricEq};

#[test]
fn test_eq_passthrough_when_disabled() {
    let mut eq = ParametricEq::default_10_band(44100.0);
    let (l, r) = eq.process(0.5, 0.5);
    assert!((l - 0.5).abs() < 1e-5);
    assert!((r - 0.5).abs() < 1e-5);
}

#[test]
fn test_eq_enabled_zero_gain() {
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_enabled(true);
    // After settling, zero-gain EQ should pass signal through
    for _ in 0..500 {
        eq.process(0.5, 0.5);
    }
    let (l, _r) = eq.process(0.5, 0.5);
    assert!(
        (l - 0.5).abs() < 0.05,
        "Zero-gain EQ should be near passthrough"
    );
}

#[test]
fn test_eq_set_band() {
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_band(0, EqBandParams::peaking(100.0, 6.0, 1.4));
    let params = eq.band_params(0).unwrap();
    assert_eq!(params.frequency, 100.0);
    assert_eq!(params.gain_db, 6.0);
}

#[test]
fn test_stereo_imaging_preserved() {
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_enabled(true);
    eq.set_band(0, EqBandParams::peaking(1000.0, 6.0, 1.4));
    // Process same signal on both channels
    for _ in 0..200 {
        eq.process(0.5, 0.5);
    }
    let (l, r) = eq.process(0.5, 0.5);
    assert!((l - r).abs() < 0.01, "Stereo imaging should be preserved");
}

#[test]
fn test_eq_headroom_is_static_not_dynamic() {
    // Headroom is a STATIC pre-EQ attenuation: a fixed -3 dB gain applied
    // before the filters. It must NOT behave like a compressor — no
    // attack/release pumping, just a constant linear scale.
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_enabled(true);
    eq.set_headroom_db(-3.0);

    let expected = 2.0 * 10.0_f32.powf(-3.0 / 20.0); // ≈ 1.414

    // Feed a loud signal; after the filters' transient response settles,
    // the gain must be exactly the -3 dB headroom (plus unity band gain).
    for _ in 0..5000 {
        let _ = eq.process(2.0, 2.0);
    }
    let (l, r) = eq.process(2.0, 2.0);
    assert!(
        (l - expected).abs() < 0.02 && (r - expected).abs() < 0.02,
        "static headroom: expected ~{expected:.3}, got l={l:.3}, r={r:.3}"
    );

    // Constant gain regardless of input level => linear, not dynamic.
    let (l2, _) = eq.process(0.1, 0.1);
    let gain_ratio_hi = l / 2.0;
    let gain_ratio_lo = l2 / 0.1;
    assert!(
        (gain_ratio_hi - gain_ratio_lo).abs() < 0.01,
        "headroom must be level-independent (static), got {gain_ratio_hi} vs {gain_ratio_lo}"
    );

    // Default headroom is 0 dB (unity) — no unintended attenuation.
    let mut eq2 = ParametricEq::default_10_band(44100.0);
    eq2.set_enabled(true);
    for _ in 0..5000 {
        let _ = eq2.process(0.5, 0.5);
    }
    let (l3, _) = eq2.process(0.5, 0.5);
    assert!(
        (l3 - 0.5).abs() < 0.02,
        "default headroom must be unity, got {l3}"
    );
}

#[test]
fn test_eq_reset_clears_filter_state() {
    // After reset, the EQ filters should be in a clean state (no
    // ringing from prior processing). This is the property the old
    // test_headroom_resets_to_unity was really verifying — that
    // reset() returns runtime state to a known-good baseline.
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_enabled(true);

    // Run a loud signal to populate filter state.
    for _ in 0..1000 {
        eq.process(2.0, 2.0);
    }
    // After reset, processing silence should produce near-silence
    // (filter state is cleared, no ringing).
    eq.reset();
    let mut max_out: f32 = 0.0;
    for _ in 0..100 {
        let (l, r) = eq.process(0.0, 0.0);
        max_out = max_out.max(l.abs()).max(r.abs());
    }
    assert!(
        max_out < 1e-4,
        "After reset, processing silence should produce near-silence; got max={}",
        max_out
    );
}

#[test]
fn test_headroom_estimator_catches_high_q_peak() {
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_enabled(true);
    // A +12 dB, Q=50 peak is only ~20 Hz wide at 1 kHz. The previous
    // 151-point log sweep stepped ~47 Hz there and missed it entirely.
    eq.set_band(3, EqBandParams::peaking(1000.0, 12.0, 50.0));
    let boost = eq.combined_max_gain_db(44100.0);
    assert!(
        (boost - 12.0).abs() < 0.05,
        "Q=50 peak should measure ~12 dB, got {boost}"
    );
}

#[test]
fn test_headroom_estimator_extreme_q_and_low_frequency() {
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_enabled(true);
    // Q=100 is the biquad's clamp limit; at 40 Hz its -3 dB bandwidth is
    // only 0.4 Hz — the hardest practical target for the estimator.
    eq.set_band(3, EqBandParams::peaking(40.0, 9.0, 100.0));
    let boost = eq.combined_max_gain_db(44100.0);
    assert!(
        (boost - 9.0).abs() < 0.05,
        "low-frequency Q=100 peak should measure ~9 dB, got {boost}"
    );
}

#[test]
fn test_headroom_estimator_low_shelf_dc_gain() {
    // A Butterworth (Q=0.707) low shelf is monotonic, so its maximum is
    // exactly the +6 dB DC gain — which lives below the 20 Hz sweep floor
    // and must come from the analytic DC evaluation.
    let mut eq = ParametricEq::new(1, 44100.0);
    eq.set_enabled(true);
    eq.set_band(0, EqBandParams::lowshelf(100.0, 6.0, 0.707));
    let boost = eq.combined_max_gain_db(44100.0);
    assert!(
        (boost - 6.0).abs() < 0.05,
        "low-shelf boost should measure ~6 dB at DC, got {boost}"
    );
}

#[test]
fn test_auto_headroom_recomputes_on_band_change() {
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_enabled(true);
    eq.set_auto_headroom(true);

    eq.set_band(3, EqBandParams::peaking(1000.0, 8.0, 1.0));
    assert!(
        (eq.headroom_db() - (-8.0)).abs() < 0.05,
        "auto headroom should be -8 dB, got {}",
        eq.headroom_db()
    );

    eq.set_band(3, EqBandParams::peaking(1000.0, 4.0, 1.0));
    assert!(
        (eq.headroom_db() - (-4.0)).abs() < 0.05,
        "auto headroom should track the curve to -4 dB, got {}",
        eq.headroom_db()
    );

    eq.set_band(3, EqBandParams::peaking(1000.0, -4.0, 1.0));
    assert!(
        eq.headroom_db().abs() < 0.05,
        "cut-only curve should need no headroom, got {}",
        eq.headroom_db()
    );

    eq.set_auto_headroom(false);
    eq.set_headroom_db(-2.0);
    eq.set_band(3, EqBandParams::peaking(1000.0, 12.0, 1.0));
    assert!(
        (eq.headroom_db() - (-2.0)).abs() < 1e-6,
        "manual headroom must be preserved when auto headroom is off"
    );
}

#[test]
fn test_auto_headroom_disable_restores_manual() {
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_enabled(true);
    eq.set_headroom_db(-3.0);
    eq.set_auto_headroom(true);
    eq.set_band(3, EqBandParams::peaking(1000.0, 9.0, 1.0));
    assert!(
        (eq.headroom_db() - (-9.0)).abs() < 0.05,
        "auto headroom should override the manual value, got {}",
        eq.headroom_db()
    );

    eq.set_auto_headroom(false);
    assert!(
        (eq.headroom_db() - (-3.0)).abs() < 1e-6,
        "disabling auto headroom should restore the manual -3 dB, got {}",
        eq.headroom_db()
    );
}
