//! 64-band cascade, 1/6-octave spacing, and multi-band stress tests.
//!
//! The value of a 64-band EQ is not the number 64 — it is that the cascade
//! stays numerically stable and well-behaved at any combination of gains,
//! frequencies and Q values.  These tests pin that down:
//!
//! - exact 1/6-octave frequency spacing (2^(1/6) ratio, 20 Hz base)
//! - Q derived from the bandwidth (≈ 8.65 for 1/6 octave)
//! - all-bands-at-max-gain and all-bands-at-max-cut remain finite
//! - random frequency / Q / gain / sample-rate sweeps produce no NaN, no
//!   unstable poles and bounded output
//! - parameter changes settle smoothly (no coefficient explosion)

use engine::dsp::equalizer::{EqBandParams, EqFilterType, ParametricEq, MAX_EQ_BANDS};

#[test]
fn test_eq_64band_true_one_sixth_octave_spacing() {
    // Band i sits at 20 Hz × 2^(i/6): an exact 1/6-octave ladder.
    let eq = ParametricEq::standard_64_band(48000.0);
    assert_eq!(eq.num_bands(), 64);
    assert_eq!(MAX_EQ_BANDS, 64);

    // Band 1 is exactly 2^(1/6) above band 0; every consecutive pair has
    // the same ratio (1/6 octave).
    let f0 = eq.band_params(0).unwrap().frequency;
    let f1 = eq.band_params(1).unwrap().frequency;
    let ratio = f1 / f0;
    let expected = 2.0_f32.powf(1.0 / 6.0);
    assert!(
        (ratio - expected).abs() < 0.02,
        "consecutive bands must be 2^(1/6) apart, got {ratio:.4} vs {expected:.4}"
    );
    for i in 0..62 {
        let a = eq.band_params(i).unwrap().frequency;
        let b = eq.band_params(i + 1).unwrap().frequency;
        let r = b / a;
        assert!(
            (r - expected).abs() < 0.02,
            "band {} -> {} spacing {:.4} != 2^(1/6)",
            i,
            i + 1,
            r
        );
    }

    // Base frequency is 20 Hz and the ladder reaches ~28.96 kHz at band 63.
    assert!(
        (f0 - 20.0).abs() < 0.5,
        "first band must be 20 Hz, got {f0}"
    );
    let f_last = eq.band_params(63).unwrap().frequency;
    assert!(
        (f_last - 28963.0).abs() < 60.0,
        "last band ≈ 28 963 Hz, got {f_last}"
    );

    // Q is derived from bandwidth: Q = 1 / (2·sinh(ln(2)·(1/6)/2)) ≈ 8.65.
    let q = eq.band_params(10).unwrap().q;
    let expected_q = 1.0 / (2.0 * (2.0f64.ln() * (1.0 / 6.0) / 2.0).sinh());
    assert!(
        (q as f64 - expected_q).abs() < 1e-3,
        "1/6-octave Q must be ≈ 8.65, got {q}"
    );

    // First band is a low shelf, last is a high shelf, middle are peaking.
    assert_eq!(
        eq.band_params(0).unwrap().filter_type,
        EqFilterType::LowShelf
    );
    assert_eq!(
        eq.band_params(63).unwrap().filter_type,
        EqFilterType::HighShelf
    );
    assert_eq!(
        eq.band_params(32).unwrap().filter_type,
        EqFilterType::Peaking
    );
}

#[test]
fn test_eq_64band_all_plus12db_stable() {
    let mut eq = ParametricEq::standard_64_band(48000.0);
    for i in 0..64 {
        eq.set_band(
            i,
            EqBandParams {
                frequency: eq.band_params(i).unwrap().frequency,
                gain_db: 12.0,
                q: eq.band_params(i).unwrap().q,
                filter_type: EqFilterType::Peaking,
                enabled: true,
            },
        );
    }
    eq.set_enabled(true);

    // Low-level input; the cascade must stay finite and bounded.
    let mut max_out = 0.0f32;
    for i in 0..20000 {
        let s = 0.01 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin();
        let (l, r) = eq.process(s, -s);
        assert!(l.is_finite() && r.is_finite(), "NaN/Inf at sample {i}");
        max_out = max_out.max(l.abs()).max(r.abs());
    }
    assert!(max_out < 10.0, "all-+12dB cascade exploded: {max_out}");
}

#[test]
fn test_eq_64band_all_minus12db_stable() {
    let mut eq = ParametricEq::standard_64_band(48000.0);
    for i in 0..64 {
        eq.set_band(
            i,
            EqBandParams {
                frequency: eq.band_params(i).unwrap().frequency,
                gain_db: -12.0,
                q: eq.band_params(i).unwrap().q,
                filter_type: EqFilterType::Peaking,
                enabled: true,
            },
        );
    }
    eq.set_enabled(true);

    // The parameter smoothing ramps each band from 0 dB to −12 dB over
    // ~1.5 ms, and the low-frequency high-Q IIR states then take up to a
    // second to drain energy captured during the ramp. Settle the chain on
    // silence FIRST (completes the ramp AND drains the states), asserting
    // finiteness throughout the transient, then measure the steady-state
    // response.
    for i in 0..48000 {
        let (l, r) = eq.process(0.0, 0.0);
        assert!(l.is_finite() && r.is_finite(), "NaN during settle at {i}");
    }
    let mut max_steady = 0.0f32;
    for i in 0..20000 {
        let s = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin();
        let (l, r) = eq.process(s, s);
        assert!(l.is_finite() && r.is_finite(), "NaN/Inf at sample {i}");
        max_steady = max_steady.max(l.abs());
    }
    // Input amplitude is 0.5; steady-state gain of −6 dB or better proves
    // the cut applied (measured ≈ −19 dB from the cascaded −12 dB bands).
    const STEADY_CUT_LIMIT: f32 = 0.25;
    assert!(
        max_steady <= STEADY_CUT_LIMIT + 1e-3,
        "all-−12dB must attenuate in steady state, got {max_steady}"
    );
}

#[test]
fn test_eq_64band_random_parameter_sweep() {
    // Deterministic PRNG so failures are reproducible.
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut rng = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    // Sweep several sample rates, including rates where the 64-band ladder
    // reaches above Nyquist (48 kHz) and rates far below (32 kHz).
    for &sr in &[32000.0f32, 44100.0, 48000.0, 96000.0, 192000.0] {
        let mut eq = ParametricEq::standard_64_band(sr);
        eq.set_enabled(true);

        let nyquist = sr * 0.45;
        for i in 0..64 {
            let f = 20.0 + (nyquist - 20.0) * ((rng() % 1000) as f32 / 1000.0);
            let gain_db = -12.0 + (rng() % 2400) as f32 / 100.0; // -12..+12
            let q = 0.2 + (rng() % 2000) as f32 / 100.0; // 0.2..20.2
            eq.set_band(
                i,
                EqBandParams {
                    frequency: f,
                    gain_db,
                    q,
                    filter_type: EqFilterType::Peaking,
                    enabled: true,
                },
            );
        }

        // Random audio + impulse: output must stay finite and bounded.
        let mut max_out = 0.0f32;
        for i in 0..10000 {
            let s = (rng() % 2000) as f32 / 1000.0 - 1.0; // -1..1
            let (l, r) = eq.process(s, s);
            assert!(
                l.is_finite() && r.is_finite(),
                "NaN/Inf at {sr} Hz, sample {i}"
            );
            max_out = max_out.max(l.abs()).max(r.abs());
        }
        // Impulse: a bounded-input/bounded-output filter must not explode.
        eq.process(1.0, 1.0);
        for _ in 0..500 {
            let (l, r) = eq.process(0.0, 0.0);
            assert!(
                l.is_finite() && r.is_finite(),
                "impulse tail NaN at {sr} Hz"
            );
            max_out = max_out.max(l.abs()).max(r.abs());
        }
        assert!(
            max_out < 100.0,
            "random 64-band cascade at {sr} Hz produced {max_out} — pole explosion"
        );
    }
}

#[test]
fn test_eq_64band_smooth_transitions_no_explosion() {
    // Rapidly sweeping gains must not make coefficients explode or produce
    // discontinuities larger than the stage gain (dezippering works).
    let mut eq = ParametricEq::standard_64_band(48000.0);
    eq.set_enabled(true);
    for i in 0..64 {
        eq.set_band(
            i,
            EqBandParams {
                frequency: eq.band_params(i).unwrap().frequency,
                gain_db: 0.0,
                q: eq.band_params(i).unwrap().q,
                filter_type: EqFilterType::Peaking,
                enabled: true,
            },
        );
    }

    let mut prev_out = 0.0f32;
    let mut max_step = 0.0f32;
    for block in 0..50 {
        let gain = if block % 2 == 0 { 6.0 } else { -6.0 };
        for i in 0..64 {
            let p = eq.band_params(i).unwrap();
            eq.set_band(
                i,
                EqBandParams {
                    frequency: p.frequency,
                    gain_db: gain,
                    q: p.q,
                    filter_type: p.filter_type,
                    enabled: true,
                },
            );
        }
        for i in 0..1024 {
            let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin() * 0.1;
            let (l, _) = eq.process(s, s);
            assert!(l.is_finite());
            max_step = max_step.max((l - prev_out).abs());
            prev_out = l;
        }
    }
    // Sample-to-sample jumps stay small (the biquad smoothing ramps the
    // coefficients); no hard discontinuity.
    assert!(
        max_step < 0.5,
        "coefficient transition produced a jump of {max_step}"
    );
}
