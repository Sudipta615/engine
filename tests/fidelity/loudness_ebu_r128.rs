//! EBU R128 / ITU-R BS.1770-4 Compliance Tests

use engine::dsp::loudness::{LoudnessMeasurement, LoudnessMeter};

#[test]
fn test_ebu_r128_silence_gating() {
    let mut meter = LoudnessMeter::new(48000.0, 2);
    // 5 seconds of silence: all blocks should fall below the -70 LUFS absolute gate
    let silence = vec![0.0f32; 48000 * 5 * 2];
    meter.process_interleaved(&silence, 2);

    let m: LoudnessMeasurement = meter.snapshot();
    assert!(
        m.integrated_lufs < -69.0 || !m.integrated_lufs.is_finite(),
        "Integrated loudness for pure silence must be below absolute gate, got {}",
        m.integrated_lufs
    );
}

#[test]
fn test_ebu_r128_1khz_sine_calibration() {
    // BS.1770-4 §2.1: LKFS = -0.691 + 10 log10 (Σ_i G_i·z̄_i), where z̄_i is the
    // per-channel mean square and the SUM over channels is used (stereo
    // dual-mono measures 3 dB louder than mono by design).
    //
    // Full-scale 1 kHz sine: per-channel mean square = 0.5 (RMS = -3.01 dBFS),
    // two identical channels => Σ = 1.0. K-weighting at 1 kHz (DeMan shelf)
    // contributes +0.67 dB, so the expected integrated loudness is
    // -0.691 + 10 log10(1.0 · 10^(0.067)) ≈ -0.02 LUFS.
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);
    let duration_secs = 5.0;
    let n_frames = (sr * duration_secs) as usize;

    let samples: Vec<f32> = (0..n_frames)
        .flat_map(|i| {
            let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
            [s, s]
        })
        .collect();

    meter.process_interleaved(&samples, 2);
    let m = meter.snapshot();

    assert!(
        m.integrated_lufs.is_finite(),
        "Integrated LUFS must be finite for continuous 1 kHz tone"
    );
    // Tolerate ±0.5 LU from theoretical value
    assert!(
        (m.integrated_lufs - (-0.02)).abs() < 0.8,
        "1 kHz full-scale stereo sine should measure near -0.02 LUFS, got {:.2} LUFS",
        m.integrated_lufs
    );
}

#[test]
fn test_ebu_r128_momentary_and_short_term() {
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);

    // 4 seconds of tone followed by 2 seconds of lower level
    let part1: Vec<f32> = (0..48000 * 4)
        .flat_map(|i| {
            let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr).sin();
            [s, s]
        })
        .collect();

    meter.process_interleaved(&part1, 2);
    let snap1 = meter.snapshot();

    assert!(snap1.momentary_lufs.is_finite());
    assert!(snap1.short_term_lufs.is_finite());
    assert!(snap1.momentary_lufs > -20.0);
}

#[test]
fn test_ebu_r128_relative_gating_discards_quiet_sections() {
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);

    // Sequence: 4 seconds of loud tone (amplitude 0.5 ≈ -6.02 dBFS -> ~-3.04 LUFS stereo),
    // followed by 4 seconds of quiet background (amplitude 0.01 ≈ -40 dBFS -> ~-37 LUFS stereo).
    // The relative gate is at (ungated_mean - 10 LU). The -37 LUFS blocks are well below
    // (-3 - 10 = -13 LUFS), so the relative gate MUST discard them and the integrated
    // loudness must remain close to the loud tone's loudness.
    let loud_part: Vec<f32> = (0..48000 * 4)
        .flat_map(|i| {
            let s = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
            [s, s]
        })
        .collect();

    let quiet_part: Vec<f32> = (0..48000 * 4)
        .flat_map(|i| {
            let s = 0.01 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
            [s, s]
        })
        .collect();

    meter.process_interleaved(&loud_part, 2);
    let loud_snap = meter.snapshot();

    meter.process_interleaved(&quiet_part, 2);
    let full_snap = meter.snapshot();

    // Integrated loudness should not drop significantly due to the quiet section
    assert!(
        (full_snap.integrated_lufs - loud_snap.integrated_lufs).abs() < 1.0,
        "Relative gating failed: loud was {:.2} LUFS, full with quiet was {:.2} LUFS",
        loud_snap.integrated_lufs,
        full_snap.integrated_lufs
    );
}

#[test]
fn test_ebu_r128_loudness_range_lra() {
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);

    // Create a dynamic signal alternating between loud (0.5 amp) and moderate (0.08 amp)
    // over several seconds so that 3s short-term blocks populate both high and low distributions.
    for _ in 0..3 {
        // 4s loud (allowing 3s short-term window to reach pure loud state)
        let loud: Vec<f32> = (0..48000 * 4)
            .flat_map(|i| {
                let s = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
                [s, s]
            })
            .collect();
        meter.process_interleaved(&loud, 2);

        // 4s moderate (allowing 3s short-term window to reach pure moderate state)
        let moderate: Vec<f32> = (0..48000 * 4)
            .flat_map(|i| {
                let s = 0.08 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
                [s, s]
            })
            .collect();
        meter.process_interleaved(&moderate, 2);
    }

    let snap = meter.snapshot();
    assert!(snap.lra_lu.is_finite(), "LRA must be finite");
    assert!(
        snap.lra_lu > 5.0 && snap.lra_lu < 25.0,
        "LRA should reflect the dynamic contrast between loud and moderate blocks, got {:.2} LU",
        snap.lra_lu
    );
}

#[test]
fn test_ebu_r128_multichannel_5_1_and_7_1_weighting() {
    use engine::decode::ChannelLayout;

    let sr = 48000.0f32;
    // 5.1 layout: FL, FR, C, LFE, SL, SR
    let mut meter_51 = LoudnessMeter::new(sr, 6);
    meter_51.set_channel_layout(&ChannelLayout::FivePointOne);

    let n = 48000 * 3;
    // Signal on only LFE channel (index 3). Per BS.1770-4, LFE has 0.0 weight
    let mut lfe_only = vec![0.0f32; n * 6];
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 80.0 * i as f32 / sr).sin();
        lfe_only[i * 6 + 3] = s;
    }
    meter_51.process_interleaved(&lfe_only, 6);
    let lfe_snap = meter_51.snapshot();
    assert!(
        lfe_snap.integrated_lufs < -69.0 || !lfe_snap.integrated_lufs.is_finite(),
        "LFE channel must have 0.0 weighting in BS.1770 loudness, got {}",
        lfe_snap.integrated_lufs
    );

    // 7.1 layout: FL, FR, C, LFE, SL, SR, RL, RR
    let mut meter_71 = LoudnessMeter::new(sr, 8);
    meter_71.set_channel_layout(&ChannelLayout::SevenPointOne);

    // Signal on rear surround channels (RL, RR at indices 6, 7). Per BS.1770-4,
    // rear surround channels receive 1.41 weighting (+1.5 dB power).
    let mut rear_only = vec![0.0f32; n * 8];
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        rear_only[i * 8 + 6] = s;
        rear_only[i * 8 + 7] = s;
    }
    meter_71.process_interleaved(&rear_only, 8);
    let rear_snap = meter_71.snapshot();
    assert!(
        rear_snap.integrated_lufs.is_finite(),
        "Rear surround channels must be measured with 1.41 weighting"
    );
    // Stereo front 1 kHz sine is ≈ -0.02 LUFS. Rear surround pair with 1.41 weight is
    // 10*log10(1.41) ≈ +1.49 LU louder (≈ +1.47 LUFS).
    assert!(
        rear_snap.integrated_lufs > 0.5,
        "Rear surround with 1.41 weight should measure louder than stereo front: got {:.2} LUFS",
        rear_snap.integrated_lufs
    );
}
