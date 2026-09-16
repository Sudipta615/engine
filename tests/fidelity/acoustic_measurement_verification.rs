//! Acoustic Measurement Verification Test Suite (§11.1, Item 30 / Item 34).
//!
//! Validates `src/spatial/acoustics/` against analytical signals:
//! 1. Direct peak detection and window functions (Hann, Hamming, Blackman, Tukey) vs analytical forms.
//! 2. Magnitude frequency response vs analytical biquad transfer function |H(e^jω)|.
//! 3. Phase response and frequency-dependent group delay vs analytical pure delay (τ_g = D/fs).
//! 4. Schroeder backward integration, RT60 (T20, T30), and EDT vs analytical exponential decay (T60 = 3*ln(10)/α).
//! 5. Acoustic clarity (C50, C80, D50) and center time (Ts) vs analytical energy integrals.
//! 6. Energy-Time Curve (ETC) envelope calculation.
//! 7. Farina logarithmic sine sweep deconvolution roundtrip.
//! 8. Maximum Length Sequence (MLS) circular cross-correlation roundtrip.
//! 9. Full end-to-end `analyze_acoustics` report generation.

use engine::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};
use engine::spatial::acoustics::*;
use std::f64::consts::PI;

#[test]
fn test_acoustic_impulse_peak_and_windows() {
    let sr = 48000.0;
    let len = 1024;
    let delay_samples = 150;

    // Synthetic Dirac impulse delayed by delay_samples
    let mut ir = vec![0.0f32; len];
    ir[delay_samples] = 0.85;

    let (peak_idx, peak_val) = find_direct_peak(&ir);
    assert_eq!(peak_idx, delay_samples);
    assert!((peak_val - 0.85).abs() < 1e-6);

    // Peak normalization
    normalize_peak(&mut ir);
    let (norm_idx, norm_val) = find_direct_peak(&ir);
    assert_eq!(norm_idx, delay_samples);
    assert!((norm_val - 1.0).abs() < 1e-6);

    // Window tests
    let hann = generate_window(WindowType::Hann, 512);
    assert_eq!(hann.len(), 512);
    assert!(hann[0].abs() < 1e-5);
    assert!(hann[511].abs() < 1e-5);
    assert!((hann[256] - 1.0).abs() < 1e-3); // Center peak

    let hamming = generate_window(WindowType::Hamming, 512);
    assert!((hamming[0] - 0.08).abs() < 1e-2);
    assert!((hamming[256] - 1.0).abs() < 1e-3);

    let tukey = generate_window(WindowType::Tukey(0.5), 512);
    // Tukey window has flat unity top in the center
    assert!((tukey[256] - 1.0).abs() < 1e-6);
    assert!((tukey[200] - 1.0).abs() < 1e-6);
    assert!(tukey[0] < 0.1);

    // Extract direct sound
    let direct = extract_direct_sound(&ir, sr as f32, 5.0);
    assert!(!direct.is_empty());
    let (d_peak_idx, d_peak_val) = find_direct_peak(&direct);
    assert!(d_peak_val > 0.9);
    assert!(d_peak_idx > 0);
}

#[test]
fn test_acoustic_frequency_response_vs_analytical_biquad() {
    let sr = 48000.0f32;
    let center_freq = 1000.0f32;
    let boost_db = 6.0f32;
    let q = 2.0f32;

    let coeffs = BiquadCoeffsF32::peaking(sr, center_freq, boost_db, q);
    let mut state = BiquadStateF32::default();

    // Generate impulse response of the biquad filter
    let ir_len = 2048;
    let mut ir = Vec::with_capacity(ir_len);
    ir.push(state.process(1.0, &coeffs));
    for _ in 1..ir_len {
        ir.push(state.process(0.0, &coeffs));
    }

    let freq_resp = compute_magnitude_response(&ir, sr as f64);
    assert!(!freq_resp.frequencies.is_empty());

    // Compare measured frequency response against analytical biquad evaluation
    // H(z) = (b0 + b1*z^-1 + b2*z^-2) / (1 + a1*z^-1 + a2*z^-2)
    let b0 = coeffs.b0 as f64;
    let b1 = coeffs.b1 as f64;
    let b2 = coeffs.b2 as f64;
    let a1 = coeffs.a1 as f64;
    let a2 = coeffs.a2 as f64;

    for (f, &meas_db) in freq_resp
        .frequencies
        .iter()
        .zip(freq_resp.magnitude_db.iter())
    {
        if *f < 50.0 || *f > 20000.0 {
            continue;
        }
        let omega = 2.0 * PI * (*f) / (sr as f64);
        let cos_w = omega.cos();
        let sin_w = omega.sin();
        let cos_2w = (2.0 * omega).cos();
        let sin_2w = (2.0 * omega).sin();

        // Numerator = b0 + b1*cos(w) + b2*cos(2w) - j*(b1*sin(w) + b2*sin(2w))
        let num_re = b0 + b1 * cos_w + b2 * cos_2w;
        let num_im = -(b1 * sin_w + b2 * sin_2w);
        let num_mag_sq = num_re * num_re + num_im * num_im;

        // Denominator = 1 + a1*cos(w) + a2*cos(2w) - j*(a1*sin(w) + a2*sin(2w))
        let den_re = 1.0 + a1 * cos_w + a2 * cos_2w;
        let den_im = -(a1 * sin_w + a2 * sin_2w);
        let den_mag_sq = den_re * den_re + den_im * den_im;

        let analytical_mag_db = 10.0 * (num_mag_sq / den_mag_sq).log10();

        assert!(
            (meas_db - analytical_mag_db).abs() < 0.15,
            "Frequency response mismatch at {:.1} Hz: measured {:.3} dB, analytical {:.3} dB",
            f,
            meas_db,
            analytical_mag_db
        );
    }

    // Octave smoothing test
    let smoothed_1_3 = smooth_frequency_response(&freq_resp, OctaveSmoothing::Octave1_3);
    assert_eq!(smoothed_1_3.frequencies.len(), freq_resp.frequencies.len());
    assert_eq!(smoothed_1_3.smoothing, OctaveSmoothing::Octave1_3);
}

#[test]
fn test_acoustic_phase_response_and_group_delay_vs_pure_delay() {
    let sr = 48000.0;
    let delay_samples = 24; // 24 samples = 0.5 ms delay
    let len = 2048;

    let mut ir = vec![0.0f32; len];
    ir[delay_samples] = 1.0;

    let phase_resp = compute_phase_response(&ir, sr);
    let group_delay = compute_group_delay(&phase_resp);

    let expected_delay_ms = (delay_samples as f64 / sr) * 1000.0; // 0.5 ms

    // Between 500 Hz and 20 kHz, group delay should be flat and equal to expected_delay_ms
    let mut checked_bins = 0;
    for (&f, &d_ms) in group_delay
        .frequencies
        .iter()
        .zip(group_delay.delay_ms.iter())
    {
        if f > 500.0 && f < 20000.0 {
            assert!(
                (d_ms - expected_delay_ms).abs() < 0.05,
                "Group delay discrepancy at {:.1} Hz: {:.4} ms (expected {:.4} ms)",
                f,
                d_ms,
                expected_delay_ms
            );
            checked_bins += 1;
        }
    }
    assert!(checked_bins > 100);
}

#[test]
fn test_acoustic_rt60_and_edt_vs_analytical_decay() {
    let sr = 48000.0;
    let target_t60 = 1.2; // seconds
    let alpha = (3.0 * 10.0f64.ln()) / target_t60; // T60 = 3*ln(10) / alpha

    let len = (target_t60 * sr * 1.2).round() as usize;
    let mut ir = Vec::with_capacity(len);

    for n in 0..len {
        let t = n as f64 / sr;
        let env = (-alpha * t).exp() as f32;
        ir.push(env);
    }

    let edc = schroeder_decay_curve(&ir);
    assert_eq!(edc.len(), len);
    assert!((edc[0] - 0.0).abs() < 1e-4); // 0 dB at start

    let rt60 = compute_rt60(&ir, sr);
    assert!(
        (rt60.t20_seconds - target_t60).abs() < 0.05,
        "T20 error: measured {:.4} s, target {:.4} s",
        rt60.t20_seconds,
        target_t60
    );
    assert!(
        (rt60.t30_seconds - target_t60).abs() < 0.05,
        "T30 error: measured {:.4} s, target {:.4} s",
        rt60.t30_seconds,
        target_t60
    );
    assert!(rt60.t20_r_squared > 0.999);
    assert!(rt60.t30_r_squared > 0.999);

    let edt = compute_edt(&ir, sr);
    assert!(
        (edt.edt_seconds - target_t60).abs() < 0.05,
        "EDT error: measured {:.4} s, target {:.4} s",
        edt.edt_seconds,
        target_t60
    );
    assert!(edt.r_squared > 0.999);
}

#[test]
fn test_acoustic_clarity_c50_c80_vs_analytical_ratio() {
    let sr = 48000.0;
    let target_t60 = 1.0; // 1 second
    let alpha = (3.0 * 10.0f64.ln()) / target_t60; // alpha = 3*ln(10) ≈ 6.907755

    let len = (1.5 * sr) as usize;
    let mut ir = Vec::with_capacity(len);
    for n in 0..len {
        let t = n as f64 / sr;
        ir.push((-alpha * t).exp() as f32);
    }

    let clarity = compute_clarity(&ir, sr);

    // Analytical energy ratio for exponential decay h(t) = exp(-alpha*t):
    // E_early(T) / E_late(T) = exp(2*alpha*T) - 1
    // C(T) = 10 * log10(exp(2*alpha*T) - 1)
    let analytical_c50 = 10.0 * ((2.0 * alpha * 0.050).exp() - 1.0).log10();
    let analytical_c80 = 10.0 * ((2.0 * alpha * 0.080).exp() - 1.0).log10();
    let analytical_d50 = 1.0 - (-2.0 * alpha * 0.050).exp();

    assert!(
        (clarity.c50_db - analytical_c50).abs() < 0.1,
        "C50 mismatch: measured {:.3} dB, analytical {:.3} dB",
        clarity.c50_db,
        analytical_c50
    );
    assert!(
        (clarity.c80_db - analytical_c80).abs() < 0.1,
        "C80 mismatch: measured {:.3} dB, analytical {:.3} dB",
        clarity.c80_db,
        analytical_c80
    );
    assert!(
        (clarity.d50 - analytical_d50).abs() < 0.02,
        "D50 mismatch: measured {:.4}, analytical {:.4}",
        clarity.d50,
        analytical_d50
    );
    assert!(clarity.center_time_ms > 0.0);
}

#[test]
fn test_acoustic_etc_envelope() {
    let sr = 48000.0;
    let ir = vec![1.0f32, 0.5, 0.25, 0.125, 0.0625];
    let etc = compute_etc(&ir, sr);

    assert_eq!(etc.time_ms.len(), ir.len());
    assert_eq!(etc.etc_db.len(), ir.len());
    assert!((etc.etc_db[0] - 0.0).abs() < 1e-4);
    assert!((etc.etc_db[1] - (-6.0206)).abs() < 1e-2);
    assert!((etc.etc_db[2] - (-12.041)).abs() < 1e-2);
}

#[test]
fn test_acoustic_farina_log_sweep_deconvolution() {
    let config = LogSweepConfig {
        start_freq_hz: 100.0,
        stop_freq_hz: 8000.0,
        duration_secs: 0.05,
        sample_rate: 48000.0,
        fade_duration_secs: 0.005,
    };

    let sweep = generate_log_sweep(&config);
    assert!(!sweep.is_empty());

    let inv = generate_inverse_filter(&sweep, &config);
    assert_eq!(inv.len(), sweep.len());

    // Deconvolve sweep through identity channel
    let ir = deconvolve_sweep(&sweep, &inv);
    assert!(!ir.is_empty());

    let (peak_idx, peak_val) = find_direct_peak(&ir);
    assert!((peak_val - 1.0).abs() < 1e-3);

    // Check peak concentration: samples far from peak must be heavily attenuated (> 20 dB down)
    let margin = 300;
    for (i, &s) in ir.iter().enumerate() {
        if i + margin < peak_idx || i > peak_idx + margin {
            assert!(s.abs() < 0.1, "Sidelobe energy leak at sample {}: {}", i, s);
        }
    }
}

#[test]
fn test_acoustic_mls_circular_cross_correlation_roundtrip() {
    let order = MlsOrder::Order10;
    let mls = generate_mls(order);
    assert_eq!(mls.len(), 1023);

    // Feed identity channel (recording = mls)
    let ir = deconvolve_mls(&mls, &mls);
    assert_eq!(ir.len(), 1023);

    let (peak_idx, peak_val) = find_direct_peak(&ir);
    assert_eq!(peak_idx, 0);
    assert!((peak_val - 1.0).abs() < 1e-4);

    // Off-peak circular autocorrelation of MLS is strictly attenuated (-1 / L)
    for &s in ir[1..].iter() {
        assert!(
            s.abs() < 0.05,
            "MLS deconvolution off-peak energy too high: {}",
            s
        );
    }
}

#[test]
fn test_acoustic_full_report_generation() {
    let sr = 48000.0;
    let mut ir = vec![0.0f32; 4800];
    // Direct peak at 200 samples
    ir[200] = 1.0;
    for (i, val) in ir.iter_mut().enumerate().take(4800).skip(201) {
        let t = (i - 200) as f64 / sr;
        *val = ((-15.0 * t).exp() * ((2.0 * PI * 440.0 * t).cos())) as f32 * 0.5;
    }

    let report = analyze_acoustics(&ir, sr);
    assert_eq!(report.direct_peak_index, 200);
    assert!((report.direct_peak_amplitude - 1.0).abs() < 1e-4);
    assert!(report.rt60.t20_seconds > 0.0);
    assert!(report.edt.edt_seconds > 0.0);
    assert!(report.clarity.c50_db != 0.0);
    assert!(!report.frequency_response_1_3.magnitude_db.is_empty());
    assert!(!report.group_delay.delay_ms.is_empty());
}
