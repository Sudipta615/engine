//! Fidelity test suite for Professional Unified Audio Metering.

use engine::dsp::meters::ProfessionalMeters;
use std::f32::consts::PI;

#[test]
fn test_meters_peak_and_rms_accuracy() {
    let sample_rate = 48000;
    let meters = ProfessionalMeters::new(sample_rate, 2);

    // Generate 1 second of 1 kHz sine at amplitude 0.5 (-6.02 dBFS) on L, 1.0 (0.0 dBFS) on R
    let mut buf = Vec::with_capacity(sample_rate as usize * 2);
    for i in 0..sample_rate {
        let t = i as f32 / sample_rate as f32;
        let s_l = 0.5 * (2.0 * PI * 1000.0 * t).sin();
        let s_r = 1.0 * (2.0 * PI * 1000.0 * t).sin();
        buf.push(s_l);
        buf.push(s_r);
    }

    meters.process_interleaved(&buf, 2);
    let snap = meters.snapshot();

    assert_eq!(snap.channels, 2);

    // L channel: 0.5 peak -> -6.02 dBFS. RMS of sine = 0.5 / sqrt(2) ≈ 0.3535 -> -9.03 dBFS
    assert!(
        (snap.peak_db[0] - (-6.02)).abs() < 0.1,
        "L peak was {}",
        snap.peak_db[0]
    );
    assert!(
        (snap.rms_db[0] - (-9.03)).abs() < 0.2,
        "L rms was {}",
        snap.rms_db[0]
    );

    // R channel: 1.0 peak -> 0 dBFS. RMS of sine = 1.0 / sqrt(2) ≈ 0.7071 -> -3.01 dBFS
    assert!(
        (snap.peak_db[1] - 0.0).abs() < 0.05,
        "R peak was {}",
        snap.peak_db[1]
    );
    assert!(
        (snap.rms_db[1] - (-3.01)).abs() < 0.1,
        "R rms was {}",
        snap.rms_db[1]
    );

    // Crest factor (Dynamic range = Peak - RMS) for pure sine wave is ~3.01 dB
    assert!((snap.dynamic_range_db[0] - 3.01).abs() < 0.3);
    assert!((snap.dynamic_range_db[1] - 3.01).abs() < 0.3);
}

#[test]
fn test_meters_dc_offset_and_clipping() {
    let sample_rate = 44100;
    let meters = ProfessionalMeters::new(sample_rate, 2);

    let dc_l = 0.25f32;
    let dc_r = -0.15f32;

    let mut buf = Vec::with_capacity(sample_rate as usize * 2);
    for _ in 0..sample_rate {
        buf.push(dc_l);
        buf.push(dc_r);
    }

    // Add a few clipped samples
    buf[100] = 1.05;
    buf[101] = -1.1;

    meters.process_interleaved(&buf, 2);
    let snap = meters.snapshot();

    assert!(
        (snap.dc_offset[0] - dc_l).abs() < 0.01,
        "DC L was {}",
        snap.dc_offset[0]
    );
    assert!(
        (snap.dc_offset[1] - dc_r).abs() < 0.01,
        "DC R was {}",
        snap.dc_offset[1]
    );

    assert!(snap.clip_count >= 2, "Clip count should be at least 2");
}

#[test]
fn test_meters_true_peak_intersample_detection() {
    let sample_rate = 48000;
    let meters = ProfessionalMeters::new(sample_rate, 2);

    // fs/4 sine (12 kHz at 48k) with 45° phase offset:
    // samples peak at ~0.7071 (-3.01 dBFS), but continuous true peak reaches 1.0 (0.0 dBTP).
    let mut buf = Vec::new();
    for i in 0..4800 {
        let t = i as f32 / sample_rate as f32;
        let s = (2.0 * PI * 12000.0 * t + std::f32::consts::FRAC_PI_4).sin();
        buf.push(s);
        buf.push(s);
    }

    meters.process_interleaved(&buf, 2);
    let snap = meters.snapshot();

    // Sample peak is ~0.7071 (-3.01 dBFS)
    assert!(
        snap.peak_db[0] < -2.9,
        "sample peak was {}",
        snap.peak_db[0]
    );

    // True peak detects the intersample reconstruction reaching ~0 dBTP (+3 dB over sample peak)
    assert!(
        snap.true_peak_dbtp[0] > -0.2,
        "true peak should detect intersample peak near 0 dBTP: {}",
        snap.true_peak_dbtp[0]
    );
    assert!(snap.true_peak_dbtp[0] > snap.peak_db[0] + 2.5);
}

#[test]
fn test_meters_lufs_loudness_integration() {
    let sample_rate = 48000;
    let meters = ProfessionalMeters::new(sample_rate, 2);

    // 1 kHz stereo sine wave at 0.1 linear peak (-20 dBFS peak, -23 dBFS RMS per channel).
    // Sum of two coherent channels (+3 dB) gives -20 LUFS.
    let amp = 10.0f32.powf(-20.0 / 20.0);
    let mut buf = Vec::new();
    for i in 0..(sample_rate * 2) {
        let t = i as f32 / sample_rate as f32;
        let s = amp * (2.0 * PI * 1000.0 * t).sin();
        buf.push(s);
        buf.push(s);
    }

    meters.process_interleaved(&buf, 2);
    let snap = meters.snapshot();

    // Momentary LUFS for stereo should be -20.0 LUFS
    assert!(
        (snap.lufs_momentary - (-20.0)).abs() < 1.0,
        "momentary LUFS: {}",
        snap.lufs_momentary
    );
}
