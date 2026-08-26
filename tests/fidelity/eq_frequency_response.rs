//! Fidelity tests — EQ frequency response accuracy
//!
//! These integration tests verify that the `ParametricEq` + `BiquadCoeffs`
//! implementation matches the theoretical biquad transfer function to within
//! acceptable tolerances.
//!
//! Run with: `cargo test --test fidelity eq_`

use engine::dsp::biquad::{BiquadCoeffs, BiquadState};
use engine::dsp::equalizer::{EqBandParams, EqFilterType, ParametricEq};

const SR: f32 = 48000.0;

/// Generate N cycles of a sine wave at `freq` Hz, return the last 512 samples
/// (steady state only — avoids transient at filter startup).
fn sine_steady_state(freq: f32, sample_rate: f32, n_cycles: usize) -> Vec<f32> {
    let n_total = ((sample_rate / freq) * n_cycles as f32) as usize;
    (0..n_total)
        .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate).sin())
        .collect()
}

/// Measure RMS of a slice.
fn rms(samples: &[f32]) -> f32 {
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// Convert linear amplitude ratio to dB.
fn lin_to_db(ratio: f32) -> f32 {
    20.0 * ratio.log10()
}

/// Sweep a sine through the biquad filter and measure the steady-state RMS gain
/// at each frequency.  Returns (measured_db, expected_db) pairs.
#[allow(dead_code)]
fn measure_biquad_response(coeffs: &BiquadCoeffs<f32>, test_freqs: &[f32]) -> Vec<(f32, f32)> {
    let mut result = Vec::new();
    for &freq in test_freqs {
        let signal = sine_steady_state(freq, SR, 50);
        let mut state = BiquadState::<f32>::default();
        let output: Vec<f32> = signal.iter().map(|&s| state.process(s, coeffs)).collect();
        // Take last 25% as steady state
        let ss_start = output.len() * 3 / 4;
        let out_rms = rms(&output[ss_start..]);
        let in_rms = rms(&signal[ss_start..]);
        let measured_db = if in_rms > 1e-6 {
            lin_to_db(out_rms / in_rms)
        } else {
            -120.0
        };
        result.push((measured_db, 0.0_f32)); // expected filled in per-test
    }
    result
}

/// Maximum allowed gain error vs theoretical: 0.05 dB.
///
/// Tightened from ±0.15 dB: the biquads compute coefficients in f64 and the
/// f32 state path accumulates in f64, so the measured error is dominated by
/// measurement noise, not the filter. ±0.05 dB is the precision expected of
/// a reference EQ (well inside human-perceptible limits of ~0.1–0.2 dB).
const MAX_GAIN_ERROR_DB: f32 = 0.05;
/// Maximum channel mismatch for stereo EQ: 0.001 dB
const MAX_STEREO_MISMATCH_DB: f32 = 0.001;

#[test]
fn eq_lowpass_attenuation() {
    // A 1 kHz LP filter should pass DC and strongly attenuate 10 kHz.
    let lp_coeffs = BiquadCoeffs::<f32>::lowpass(SR, 1000.0, 0.707);

    // DC / near-DC should be near 0 dB
    let low_freqs = [10.0_f32, 50.0, 100.0];
    for freq in &low_freqs {
        let signal = sine_steady_state(*freq, SR, 20);
        let mut state = BiquadState::<f32>::default();
        let out: Vec<f32> = signal
            .iter()
            .map(|&s| state.process(s, &lp_coeffs))
            .collect();
        let ss = out.len() * 3 / 4;
        let gain_db = lin_to_db(rms(&out[ss..]) / rms(&signal[ss..]));
        assert!(
            gain_db > -1.0,
            "Low-pass @ {}Hz should pass low freq, got {} dB",
            freq,
            gain_db
        );
    }

    // 10× cutoff should be strongly attenuated (≥ -30 dB for 2nd order)
    let high_freq = 10000.0_f32;
    let signal = sine_steady_state(high_freq, SR, 20);
    let mut state = BiquadState::<f32>::default();
    let out: Vec<f32> = signal
        .iter()
        .map(|&s| state.process(s, &lp_coeffs))
        .collect();
    let ss = out.len() * 3 / 4;
    let gain_db = lin_to_db(rms(&out[ss..]) / rms(&signal[ss..]));
    assert!(
        gain_db < -30.0,
        "Low-pass should strongly attenuate 10 kHz, got {} dB",
        gain_db
    );
}

#[test]
fn eq_peaking_gain_accuracy() {
    // A +6 dB peaking filter at 1 kHz with Q=1.0 should boost 1 kHz by 6±0.15 dB.
    let gain_db_target = 6.0_f32;
    let center_freq = 1000.0_f32;
    let q = 1.0_f32;

    let coeffs = BiquadCoeffs::<f32>::peaking(SR, center_freq, gain_db_target, q);
    let signal = sine_steady_state(center_freq, SR, 50);
    let mut state = BiquadState::<f32>::default();
    let out: Vec<f32> = signal.iter().map(|&s| state.process(s, &coeffs)).collect();
    let ss = out.len() * 3 / 4;
    let gain_measured = lin_to_db(rms(&out[ss..]) / rms(&signal[ss..]));

    assert!(
        (gain_measured - gain_db_target).abs() < MAX_GAIN_ERROR_DB,
        "Peaking +{}dB @{}Hz: measured {}dB (error {}dB)",
        gain_db_target,
        center_freq,
        gain_measured,
        (gain_measured - gain_db_target).abs()
    );
}

#[test]
fn eq_peaking_negative_gain_accuracy() {
    // A -6 dB peaking filter at 1 kHz should cut 1 kHz by 6±0.15 dB.
    let gain_db_target = -6.0_f32;
    let center_freq = 1000.0_f32;
    let q = 1.0_f32;

    let coeffs = BiquadCoeffs::<f32>::peaking(SR, center_freq, gain_db_target, q);
    let signal = sine_steady_state(center_freq, SR, 50);
    let mut state = BiquadState::<f32>::default();
    let out: Vec<f32> = signal.iter().map(|&s| state.process(s, &coeffs)).collect();
    let ss = out.len() * 3 / 4;
    let gain_measured = lin_to_db(rms(&out[ss..]) / rms(&signal[ss..]));

    assert!(
        (gain_measured - gain_db_target).abs() < MAX_GAIN_ERROR_DB,
        "Peaking {}dB @{}Hz: measured {}dB (error {}dB)",
        gain_db_target,
        center_freq,
        gain_measured,
        (gain_measured - gain_db_target).abs()
    );
}

#[test]
fn eq_lowshelf_gain_accuracy() {
    // A +6 dB low-shelf at 100 Hz (with default Q=0.707). At 20 Hz the shelf
    // has not quite reached its asymptotic +6 dB, so the reference is the
    // filter's own theoretical response at the measured frequency — not the
    // nominal shelf gain. This pins the implementation to the transfer
    // function (reference-grade) instead of a loose "close to +6 dB" check.
    let shelf_gain_db = 6.0_f32;
    let coeffs = BiquadCoeffs::<f32>::lowshelf(SR, 100.0, shelf_gain_db, 0.707);

    let signal = sine_steady_state(20.0, SR, 50);
    let mut state = BiquadState::<f32>::default();
    let out: Vec<f32> = signal.iter().map(|&s| state.process(s, &coeffs)).collect();
    let ss = out.len() * 3 / 4;
    let gain_measured = lin_to_db(rms(&out[ss..]) / rms(&signal[ss..]));

    let gain_theoretical = lin_to_db(coeffs.evaluate_magnitude(20.0, SR) as f32);
    assert!(
        (gain_measured - gain_theoretical).abs() < MAX_GAIN_ERROR_DB,
        "Low-shelf +{}dB: measured {}dB vs theoretical {}dB at 20Hz (error {}dB)",
        shelf_gain_db,
        gain_measured,
        gain_theoretical,
        (gain_measured - gain_theoretical).abs()
    );
}

#[test]
fn eq_stereo_channel_matching() {
    // The same EQ applied to both channels should produce matching outputs.
    let mut eq = ParametricEq::default_10_band(SR);
    eq.set_enabled(true);
    eq.set_band(
        2,
        EqBandParams {
            frequency: 500.0,
            gain_db: 6.0,
            q: 1.0,
            filter_type: EqFilterType::Peaking,
            enabled: true,
        },
    );

    let freq = 500.0_f32;
    let n = ((SR / freq) * 50.0) as usize;
    let mut max_mismatch = 0.0_f32;

    for i in 0..n {
        let sample = (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
        let (l, r) = eq.process(sample, sample);
        let mismatch_db = if r.abs() > 1e-10 {
            lin_to_db((l / r).abs()).abs()
        } else {
            0.0
        };
        max_mismatch = max_mismatch.max(mismatch_db);
    }

    assert!(
        max_mismatch < MAX_STEREO_MISMATCH_DB,
        "Stereo channel mismatch: {} dB (max allowed {})",
        max_mismatch,
        MAX_STEREO_MISMATCH_DB
    );
}

#[test]
fn eq_f64_peaking_more_accurate_than_f32() {
    // At high Q, the f64 biquad should give lower accumulated error than f32.
    // We verify that both give the correct gain, and f64 has lower relative error.
    let gain_target = 12.0_f32;
    let q_high = 8.0_f32;
    let freq = 4000.0_f32;

    let coeffs_f32 = BiquadCoeffs::<f32>::peaking(SR, freq, gain_target, q_high);
    let coeffs_f64 = BiquadCoeffs::<f64>::peaking(SR, freq, gain_target, q_high);

    let n = ((SR / freq) * 100.0) as usize;
    let signal: Vec<f32> = (0..n)
        .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin())
        .collect();

    let mut state_f32 = BiquadState::<f32>::default();
    let mut state_f64 = BiquadState::<f64>::default();

    let out_f32: Vec<f32> = signal
        .iter()
        .map(|&s| state_f32.process(s, &coeffs_f32))
        .collect();
    let out_f64: Vec<f64> = signal
        .iter()
        .map(|&s| state_f64.process(s as f64, &coeffs_f64))
        .collect();

    let ss = n * 3 / 4;
    let gain_f32 = lin_to_db(rms(&out_f32[ss..]) / rms(&signal[ss..]));
    let gain_f64 = lin_to_db(
        (out_f64[ss..].iter().map(|x| x * x).sum::<f64>() / (n - ss) as f64).sqrt() as f32
            / rms(&signal[ss..]),
    );

    let err_f32 = (gain_f32 - gain_target).abs();
    let err_f64 = (gain_f64 - gain_target).abs();

    // Both should be reference-accurate: ±0.1 dB at +12 dB / Q=8 (a much
    // harder target than the ±0.05 dB used at Q=1).
    assert!(
        err_f32 < 0.1,
        "f32 peaking gain error too large: {} dB",
        err_f32
    );
    assert!(
        err_f64 < 0.1,
        "f64 peaking gain error too large: {} dB",
        err_f64
    );

    // Both should be within tolerance (f64 is at least as good)
    assert!(
        err_f64 <= err_f32 + 0.01, // tiny fudge factor for measurement noise
        "f64 ({:.4} dB) should be at least as accurate as f32 ({:.4} dB)",
        err_f64,
        err_f32
    );
}

#[test]
fn identity_biquad_unity_gain() {
    let coeffs = BiquadCoeffs::<f32>::identity();
    let mut state = BiquadState::<f32>::default();
    for i in 0..1000 {
        let s = (i as f32 * 0.01).sin();
        let out = state.process(s, &coeffs);
        assert!(
            (out - s).abs() < 1e-6,
            "Identity biquad should be passthrough, got {}",
            out
        );
    }
}
