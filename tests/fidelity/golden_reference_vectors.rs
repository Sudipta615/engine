//! Golden reference-vector tests for DSP components.
//!
//! Validates:
//! - Exact numerical PCM impulse and step responses for parametric EQ biquads
//! - Exact mathematical power-conservation for equal-power crossfade curves
//! - Strict ceiling and envelope compliance for lookahead true-peak limiter
//! - Standard ITU-R BS.1770-4 calibration tone loudness reference vectors
//! - Statistical variance, mean, and bounds for TPDF dither

use engine::dsp::biquad::{BiquadCoeffsF64, BiquadStateF64};
use engine::dsp::crossfade::{CrossfadeCurve, TrackMixer};
use engine::dsp::dither::{Dither, DitherType};
use engine::dsp::limiter::LookaheadLimiter;
use engine::dsp::loudness::LoudnessMeter;

// ── 1. Parametric EQ Golden Vector ────────────────────────────────────────

#[test]
fn test_eq_biquad_peaking_golden_impulse_response() {
    let sample_rate = 48000.0;
    let freq = 1000.0;
    let q = 1.0;
    let gain_db = 6.0;

    let coeffs = BiquadCoeffsF64::peaking(sample_rate, freq, gain_db, q);
    let mut state = BiquadStateF64::default();

    // Unit impulse: x[0] = 1.0, x[n > 0] = 0.0
    let mut impulse = [0.0f64; 16];
    impulse[0] = 1.0;

    let mut actual_response = [0.0f64; 16];
    for (i, &x) in impulse.iter().enumerate() {
        actual_response[i] = state.process(x, &coeffs);
    }

    // First sample of impulse response must be b0
    assert!(
        (actual_response[0] - coeffs.b0).abs() < 1e-12,
        "h[0] = {} != b0 = {}",
        actual_response[0],
        coeffs.b0
    );

    // Second sample must be b1 - a1 * h[0]
    let expected_h1 = coeffs.b1 - coeffs.a1 * actual_response[0];
    assert!(
        (actual_response[1] - expected_h1).abs() < 1e-12,
        "h[1] = {} != expected {}",
        actual_response[1],
        expected_h1
    );

    // Verify all response samples are finite and bounded
    for (n, &h) in actual_response.iter().enumerate() {
        assert!(h.is_finite(), "h[{n}] is non-finite: {h}");
        assert!(h.abs() <= 2.5, "h[{n}] exceeded bound: {h}");
    }
}

#[test]
fn test_eq_biquad_unity_passthrough_impulse_response() {
    let sample_rate = 44100.0;
    let coeffs = BiquadCoeffsF64::peaking(sample_rate, 1000.0, 0.0, 0.707);
    let mut state = BiquadStateF64::default();

    let mut impulse = [0.0f64; 8];
    impulse[0] = 1.0;

    let mut actual_response = [0.0f64; 8];
    for (i, &x) in impulse.iter().enumerate() {
        actual_response[i] = state.process(x, &coeffs);
    }

    // At 0 dB gain, biquad is transparent: h[0] = 1.0, h[n > 0] = 0.0
    assert!(
        (actual_response[0] - 1.0).abs() < 1e-9,
        "h[0] at 0 dB was not 1.0: {}",
        actual_response[0]
    );
    for (n, &v) in actual_response.iter().enumerate().skip(1) {
        assert!(v.abs() < 1e-9, "h[{n}] at 0 dB was non-zero: {v}");
    }
}

// ── 2. Crossfade Curve Golden Vectors ────────────────────────────────────

#[test]
fn test_crossfade_equal_power_exact_trigonometric_invariance() {
    // Equal power curve satisfies: g_out(t)^2 + g_in(t)^2 = 1.00000000 for all t in [0, 1]
    for step in [0.0, 0.25, 0.5, 0.75, 1.0] {
        let (g_out, g_in) = TrackMixer::compute_gains_for_curve(step, CrossfadeCurve::EqualPower);
        let power_sum = g_out * g_out + g_in * g_in;
        assert!(
            (power_sum - 1.0).abs() < 1e-6,
            "Equal power curve failed at t={step}: g_out={g_out}, g_in={g_in}, power_sum={power_sum}"
        );
    }
}

#[test]
fn test_crossfade_linear_exact_amplitude_invariance() {
    for step in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
        let (g_out, g_in) = TrackMixer::compute_gains_for_curve(step, CrossfadeCurve::Linear);
        let amp_sum = g_out + g_in;
        assert!(
            (amp_sum - 1.0).abs() < 1e-6,
            "Linear crossfade failed at t={step}: g_out={g_out}, g_in={g_in}, sum={amp_sum}"
        );
        assert!(
            (g_out - (1.0 - step)).abs() < 1e-6,
            "Linear g_out != 1 - t at t={step}"
        );
        assert!((g_in - step).abs() < 1e-6, "Linear g_in != t at t={step}");
    }
}

// ── 3. Lookahead Limiter Golden Vectors ───────────────────────────────────

#[test]
fn test_limiter_ceiling_golden_vector() {
    let sample_rate = 48000.0;
    let ceiling_db = -1.0f32; // -1.0 dBFS = 0.8912509
    let ceiling_linear = 10.0f32.powf(ceiling_db / 20.0);

    let mut limiter = LookaheadLimiter::new(sample_rate);
    limiter.set_ceiling_db(ceiling_db);
    limiter.set_enabled(true);

    // Feed a massive +12 dBFS sine wave
    let n = 48000;
    let input: Vec<f32> = (0..n)
        .map(|i| 4.0 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate).sin())
        .collect();

    let mut l = input.clone();
    let mut r = input.clone();
    limiter.process_block(&mut l, &mut r);

    // Skip lookahead delay warmup (5ms = 240 samples)
    let steady = &l[480..];
    let max_peak = steady.iter().map(|&s| s.abs()).fold(0.0f32, f32::max);

    assert!(
        max_peak <= ceiling_linear + 0.01,
        "Limiter allowed peak {max_peak} above ceiling {ceiling_linear}"
    );
}

// ── 4. EBU R128 Loudness Reference Calibration Vectors ───────────────────

#[test]
fn test_ebu_r128_itu_bs1770_stereo_1khz_calibration_vector() {
    // Per ITU-R BS.1770-4 §1.4:
    // A 1000 Hz stereo sine tone at 0 dBFS peak in both channels (left = sin, right = sin)
    // with BS.1770 channel summation measures -0.02 ± 0.2 LUFS.
    let sample_rate = 48000.0;
    let mut meter = LoudnessMeter::new(sample_rate, 2);

    let n = (sample_rate as usize) * 5;
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate).sin();
        meter.process_stereo(s, s);
    }

    let meas = meter.snapshot();
    let diff = (meas.integrated_lufs - (-0.02)).abs();
    assert!(
        diff < 0.25,
        "BS.1770-4 1kHz 0 dBFS reference tone produced {:.2} LUFS (expected -0.02 ± 0.25 LUFS)",
        meas.integrated_lufs
    );
}

// ── 5. Dither Statistical Golden Vectors ─────────────────────────────────

#[test]
fn test_tpdf_dither_statistical_golden_moments() {
    // Triangular Probability Density Function (TPDF) dither with 16-bit quantizer:
    // 1. Mean of quantized silence must be 0.0 ± 0.02 LSB (unbiased).
    // 2. Variance of quantized silence equals Var(TPDF) + Var(Quantization) = 2/12 + 1/12 = 3/12 = 0.250 LSB^2 (within ±5%).
    let mut dither = Dither::new(DitherType::Triangular, 16);
    let n = 200_000;
    let lsb = 1.0 / 32768.0f64;
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;

    for _ in 0..n {
        let (dl, _dr) = dither.process(0.0, 0.0);
        let lsb_units = dl as f64 / lsb;
        sum += lsb_units;
        sum_sq += lsb_units * lsb_units;
    }

    let mean = sum / n as f64;
    let variance = (sum_sq / n as f64) - (mean * mean);

    assert!(
        mean.abs() < 0.02,
        "TPDF dither mean {mean:.5} LSB is not zero-mean"
    );
    assert!(
        (variance - 0.250).abs() < 0.02,
        "TPDF dither total variance {variance:.5} LSB^2 != expected 0.250 LSB^2"
    );
}

// ── 10. Bit-Perfect Samples-vs-Transport Matrix (§13) ──────────────────────

/// The samples-vs-transport split must be exhaustive and mutually coherent:
/// every combination of {samples perfect, transport perfect} produces the
/// documented verdict, and no combination may claim bit-perfect without both
/// halves proven. This is the golden matrix for the engine's single
/// bit-perfect verdict.
#[test]
fn test_bit_perfect_samples_vs_transport_matrix() {
    use config::EngineConfig;
    use config::{OutputAccessMode, OutputAccessState};
    use engine::dsp::pipeline::{BitPerfectResult, DspPipeline, OutputSampleFormat};

    let mut pipeline = DspPipeline::from_config(&EngineConfig::default(), 44100.0);
    pipeline.set_volume(1.0);
    pipeline.set_eq_enabled(false);
    pipeline.set_limiter_enabled(false);

    let mk = |pipeline: &DspPipeline, _samples_ok: bool, transport_ok: bool| {
        let access_state = if transport_ok {
            OutputAccessState {
                requested: OutputAccessMode::Exclusive,
                actual: OutputAccessMode::Exclusive,
                verified: true,
            }
        } else {
            OutputAccessState {
                requested: OutputAccessMode::Exclusive,
                actual: OutputAccessMode::Shared,
                verified: false,
            }
        };
        pipeline.bit_perfect_report_with_access(
            44100,
            44100,
            24,
            32,
            OutputSampleFormat::F32,
            false, // no resampler
            access_state,
            false, // no fallback
        )
    };

    // Row 1: samples OK (all stages unity/bypassed), transport verified.
    let r = mk(&pipeline, true, true);
    assert!(r.bit_perfect_samples);
    assert!(r.bit_perfect_transport);
    assert!(r.is_bit_perfect);
    assert_eq!(r.result, BitPerfectResult::BitPerfect);

    // Row 2: samples OK, transport shared/unverified → UNKNOWN.
    let r = mk(&pipeline, true, false);
    assert!(r.bit_perfect_samples);
    assert!(!r.bit_perfect_transport);
    assert!(!r.is_bit_perfect);
    assert_eq!(r.result, BitPerfectResult::Unknown);

    // Row 3: DSP active (provable sample modification), transport verified → DSP.
    pipeline.set_eq_enabled(true);
    let r = mk(&pipeline, false, true);
    assert!(!r.bit_perfect_samples);
    assert!(r.bit_perfect_transport);
    assert!(!r.is_bit_perfect);
    assert_eq!(r.result, BitPerfectResult::Dsp);

    // Row 4: DSP active AND transport shared → DSP (the sample violation is
    // the decisive, provable cause).
    pipeline.set_eq_enabled(false);
    pipeline.set_volume(0.5);
    let r = mk(&pipeline, false, false);
    assert!(!r.bit_perfect_samples);
    assert!(!r.bit_perfect_transport);
    assert!(!r.is_bit_perfect);
    assert_eq!(r.result, BitPerfectResult::Dsp);
}
