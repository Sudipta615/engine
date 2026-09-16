//! Room and Output Correction Validation Suite (§11.2, Item 31 / Item 35).
//!
//! Validates:
//! 1. Pre vs post frequency response error reduction (>= 6 dB improvement).
//! 2. Phase and group delay linearity for linear-phase mode (flat group delay < 0.25 ms variation).
//! 3. Filter stability (finite L1 norm, no non-finites, biquad pole stability).
//! 4. Peak headroom enforcement (peak boost <= max_boost_db).
//! 5. Latency reporting accuracy (peak tap matches reported latency).
//! 6. Multichannel consistency across Left and Right channels (matched latency, lengths, and stability).
//! 7. Real-time convolution processor integration.

use engine::spatial::acoustics::frequency_response::{FrequencyResponse, OctaveSmoothing};
use engine::spatial::room_correction::*;

fn generate_synthetic_room_response(sample_rate: f64, num_bins: usize) -> FrequencyResponse {
    let bin_step = sample_rate / ((num_bins - 1) * 2) as f64;
    let mut frequencies = Vec::with_capacity(num_bins);
    let mut magnitude_db = Vec::with_capacity(num_bins);

    for bin in 0..num_bins {
        let f = bin as f64 * bin_step;
        frequencies.push(f);

        // Untreated room acoustic transfer function:
        // 1. Boundary loading below 250 Hz (+9 dB)
        // 2. Room mode resonance at 70 Hz (+5 dB)
        // 3. Acoustic cancellation null at 150 Hz (-8 dB)
        // 4. Floor reflection dip around 400 Hz (-5 dB)
        // 5. High-frequency absorption roll-off above 4 kHz (-9 dB)
        let mut db = 0.0;
        if f >= 20.0 {
            let low_shelf = 11.0 / (1.0 + (f / 200.0).powi(2));
            let hf_rolloff = -10.0 * (1.0 - 1.0 / (1.0 + (f / 4000.0).powi(2)));
            let mode = 5.0 / (1.0 + ((f - 70.0) / 25.0).powi(2));
            let null1 = -8.0 / (1.0 + ((f - 150.0) / 40.0).powi(2));
            let null2 = -5.0 / (1.0 + ((f - 400.0) / 80.0).powi(2));

            db = low_shelf + hf_rolloff + mode + null1 + null2;
        }

        magnitude_db.push(db);
    }

    FrequencyResponse {
        frequencies,
        magnitude_db,
        smoothing: OctaveSmoothing::None,
    }
}

#[test]
fn test_room_correction_error_reduction_and_headroom() {
    let sr = 48000.0;
    let num_bins = 2049; // 4096-point FFT grid
    let measured = generate_synthetic_room_response(sr, num_bins);
    let target = TargetCurve::flat();

    let config = FilterSynthConfig {
        filter_length_taps: 2048,
        max_boost_db: 10.0,
        max_cut_db: 18.0,
        mode: CorrectionMode::LinearPhase,
        low_freq_limit_hz: 20.0,
        high_freq_limit_hz: 20000.0,
    };

    let (fir, metrics) = synthesize_channel_correction(&measured, &target, &config, sr);
    assert_eq!(fir.len(), 2048);

    // Validate the generated correction filter
    let report = validate_correction_filter(&fir, &measured, &target, &config, sr);

    // Verify error reduction >= 6 dB
    assert!(
        report.error_reduction_satisfies_target,
        "Error reduction insufficient: {:.2} dB (target >= 6.0 dB, pre: {:.2} dB, post: {:.2} dB)",
        report.error_reduction_db, report.pre_correction_rms_db, report.post_correction_rms_db
    );
    assert!(report.error_reduction_db >= 6.0);

    // Verify headroom constraint (max_boost_db <= 6.0 + 0.75 dB ripple)
    assert!(
        report.headroom_satisfied,
        "Headroom limit violated: peak boost {:.2} dB exceeds max boost {:.2} dB",
        report.peak_boost_db, config.max_boost_db
    );

    // Filter stability and finite L1 norm
    assert!(report.filter_stable);
    assert!(report.l1_norm > 0.0 && report.l1_norm < 100.0);

    // Linear-phase latency matching (peak tap at 1024)
    assert!(report.latency_matches_peak);
    assert_eq!(report.latency_samples, 1024);

    // Group delay flatness across passband
    assert!(
        report.phase_linearity_satisfied,
        "Linear phase group delay variation too high: {:.4} ms",
        report.group_delay_variation_ms
    );
    assert!(report.group_delay_variation_ms < 0.25);

    // Verify synthesis metrics mirror validation metrics
    assert!((metrics.peak_boost_db - report.peak_boost_db).abs() < 1.0);
}

#[test]
fn test_room_correction_minimum_phase_mode() {
    let sr = 48000.0;
    let num_bins = 1025;
    let measured = generate_synthetic_room_response(sr, num_bins);
    let target = TargetCurve::flat();

    let config = FilterSynthConfig {
        filter_length_taps: 1024,
        max_boost_db: 6.0,
        max_cut_db: 12.0,
        mode: CorrectionMode::MinimumPhase,
        low_freq_limit_hz: 20.0,
        high_freq_limit_hz: 20000.0,
    };

    let (fir, _) = synthesize_channel_correction(&measured, &target, &config, sr);
    assert_eq!(fir.len(), 1024);

    let report = validate_correction_filter(&fir, &measured, &target, &config, sr);
    assert!(report.filter_stable);
    assert!(report.headroom_satisfied);
    assert!(report.l1_norm > 0.0);
}

#[test]
fn test_room_correction_biquad_parametric_fitting_stability() {
    let sr = 48000.0;
    let num_bins = 1025;
    let measured = generate_synthetic_room_response(sr, num_bins);
    let target = TargetCurve::flat();

    let bands = fit_parametric_eq(&measured, &target, 8);
    assert!(!bands.is_empty());
    assert!(bands.len() <= 8);

    assert!(validate_biquad_bands_stability(&bands));

    for b in &bands {
        assert!(b.gain_db.abs() <= 12.0);
        assert!(b.q > 0.5 && b.q < 5.0);
    }
}

#[test]
fn test_room_correction_multichannel_consistency() {
    let sr = 48000.0;
    let num_bins = 2049;
    let left_meas = generate_synthetic_room_response(sr, num_bins);

    // Create slightly different right channel (asymmetric room acoustics)
    let mut right_meas = left_meas.clone();
    for (i, db) in right_meas.magnitude_db.iter_mut().enumerate() {
        let f = right_meas.frequencies[i];
        if f > 200.0 && f < 800.0 {
            *db += 2.5; // slight right-wall reflection asymmetry
        }
    }

    let target = TargetCurve::flat();
    let config = FilterSynthConfig {
        filter_length_taps: 2048,
        max_boost_db: 10.0,
        max_cut_db: 18.0,
        mode: CorrectionMode::LinearPhase,
        low_freq_limit_hz: 20.0,
        high_freq_limit_hz: 20000.0,
    };

    let (left_fir, _) = synthesize_channel_correction(&left_meas, &target, &config, sr);
    let (right_fir, _) = synthesize_channel_correction(&right_meas, &target, &config, sr);

    let left_report = validate_correction_filter(&left_fir, &left_meas, &target, &config, sr);
    let right_report = validate_correction_filter(&right_fir, &right_meas, &target, &config, sr);

    assert!(left_report.error_reduction_satisfies_target);
    assert!(right_report.error_reduction_satisfies_target);

    // Multichannel consistency: matched length, matched latency, both stable
    let consistent =
        validate_multichannel_consistency(&left_fir, &right_fir, &left_report, &right_report);
    assert!(
        consistent,
        "Left and Right channels failed consistency checks"
    );
    assert_eq!(left_report.latency_samples, right_report.latency_samples);
}

#[test]
fn test_room_correction_realtime_convolution_execution() {
    let sr = 48000.0;
    let num_bins = 1025;
    let measured = generate_synthetic_room_response(sr, num_bins);
    let target = TargetCurve::flat();

    let config = FilterSynthConfig {
        filter_length_taps: 512,
        max_boost_db: 6.0,
        max_cut_db: 12.0,
        mode: CorrectionMode::LinearPhase,
        low_freq_limit_hz: 20.0,
        high_freq_limit_hz: 20000.0,
    };

    let (fir, _) = synthesize_channel_correction(&measured, &target, &config, sr);

    let filter = RoomCorrectionFilter {
        ir: fir,
        mode: CorrectionMode::LinearPhase,
        latency_samples: 256,
    };

    let mut processor = RoomCorrectionProcessor::new();
    processor
        .prepare(&filter)
        .expect("Failed to prepare room correction processor");
    assert!(processor.prepared());
    assert_eq!(processor.latency_samples(), 256);

    let block_size = 256;
    let mut stereo_buf = vec![0.5f32; block_size * 2];

    // Process blocks in real time
    for _ in 0..10 {
        processor.process_stereo_interleaved(&mut stereo_buf, block_size);
        for &s in &stereo_buf {
            assert!(s.is_finite());
        }
    }
}
