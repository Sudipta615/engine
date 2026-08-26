use super::*;

#[test]
fn test_k_weight_stage1_shelf_response() {
    // BS.1770-4 (DeMan) stage-1 high shelf: +0.67 dB at 1 kHz (below the
    // 1682 Hz corner), approaching +4 dB well above the corner.
    let sr = 48000.0f32;
    for (freq, expected_db, tol) in [(1000.0, 0.67, 0.4), (5000.0, 3.9, 0.6), (10000.0, 4.0, 0.4)] {
        let mut s1 = KWeightStage1::new(sr);
        let n = 48000 * 5;
        let mut sum_sq = 0.0f64;
        let mut sum_raw = 0.0f64;
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin();
            let k = s1.process(s, 0);
            sum_sq += (k as f64) * (k as f64);
            sum_raw += (s as f64) * (s as f64);
        }
        let gain_db = 10.0 * (sum_sq / sum_raw).log10();
        assert!(
            (gain_db - expected_db).abs() < tol,
            "stage-1 shelf gain at {} Hz: expected ~{} dB, got {:.2} dB",
            freq,
            expected_db,
            gain_db
        );
    }
}

#[test]
fn test_meter_channel_sum_calibration() {
    // BS.1770-4 channel-sum semantics for identical stereo input.
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);
    let n = 48000 * 5;
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        meter.process_stereo(s, s);
    }
    let m = meter.snapshot();
    assert!(
        m.integrated_lufs.is_finite(),
        "meter integrated must be finite"
    );
    // Stereo full-scale 1 kHz sine ≈ -0.02 LUFS (channel sum, not average)
    assert!(
        (m.integrated_lufs - (-0.02)).abs() < 0.8,
        "stereo full-scale 1 kHz should measure near -0.02 LUFS, got {:.2}",
        m.integrated_lufs
    );
}

#[test]
fn test_channel_sum_stereo_vs_mono() {
    // BS.1770-4 sums channel energies: identical stereo content measures
    // exactly 10*log10(2) ≈ 3.01 LU louder than mono.
    let sr = 48000.0f32;
    let mut mono = LoudnessMeter::new(sr, 1);
    let mut stereo = LoudnessMeter::new(sr, 2);
    let n = 48000 * 3;
    let mut mono_samp = Vec::with_capacity(n);
    let mut stereo_samp = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr).sin();
        mono_samp.push(s);
        stereo_samp.extend_from_slice(&[s, s]);
    }
    mono.process_interleaved(&mono_samp, 1);
    stereo.process_interleaved(&stereo_samp, 2);
    let mono_lufs = mono.snapshot().integrated_lufs;
    let stereo_lufs = stereo.snapshot().integrated_lufs;
    let delta = stereo_lufs - mono_lufs;
    assert!(
        (delta - 3.01).abs() < 0.3,
        "stereo should be ~3.01 LU louder than mono, got {:.2} ({:.2} vs {:.2})",
        delta,
        mono_lufs,
        stereo_lufs
    );
}

#[test]
fn test_multichannel_measurement_and_semantic_weights() {
    // 5.1-style 6-channel input must be measurable (filter state is kept
    // per channel, up to MAX_CHANNELS).
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 6);
    let n = 48000;
    let mut samples = Vec::with_capacity(n * 6);
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        samples.extend_from_slice(&[s, s, s, 0.0, s, s]);
    }
    meter.process_interleaved(&samples, 6);
    let m = meter.snapshot();
    assert!(m.integrated_lufs.is_finite());

    // Semantic weighting: LFE must be excluded from integration. A
    // 5.1 signal whose only energy sits in the LFE slot must measure as
    // effectively silent — no raw-index arithmetic can express this; it
    // requires knowing that slot 3 *is* the LFE channel.
    let mut lfe_only = LoudnessMeter::new(sr, 6);
    lfe_only.set_channel_layout(&ChannelLayout::FivePointOne);
    let lfe_samp: Vec<f32> = std::iter::repeat_n([0.0f32, 0.0, 0.0, 0.9, 0.0, 0.0], n)
        .flatten()
        .collect();
    lfe_only.process_interleaved(&lfe_samp, 6);
    let lfe_lufs = lfe_only.snapshot().integrated_lufs;
    assert!(
        lfe_lufs < -60.0 || !lfe_lufs.is_finite(),
        "LFE must be excluded from integration, got {lfe_lufs:.2} LUFS"
    );
}

#[test]
fn test_off_mode_passthrough() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::Off);
    let (l, r) = norm.process(0.5, 0.5);
    assert!((l - 0.5).abs() < 1e-5);
    assert!((r - 0.5).abs() < 1e-5);
}

#[test]
fn test_replay_gain_attenuation() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::TrackReplayGain);
    let meta = LoudnessMetadata {
        replaygain_track_db: Some(-5.0), // Loud track, RG says -5dB (reduce volume)
        replaygain_track_peak: Some(0.95),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    for _ in 0..10000 {
        norm.process(0.5, 0.5);
    }
    let (l, _r) = norm.process(0.5, 0.5);
    // With correct ReplayGain sign: rg + preamp = -5.0 + 0.0 = -5.0 dB (attenuation)
    // A loud track should be attenuated, so output should be less than input
    assert!(
        l < 0.5,
        "Loud track should be attenuated by ReplayGain, got {}",
        l
    );
    assert!(
        l > 0.01,
        "Should still be audible after attenuation, got {}",
        l
    );
}

#[test]
fn test_ebu_r128_normalization() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::EbuR128);
    norm.set_target_lufs(-23.0);
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-30.0), // Quiet track
        ebu_r128_peak: Some(-3.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    for _ in 0..10000 {
        norm.process(0.1, 0.1);
    }
    let (l, _r) = norm.process(0.1, 0.1);
    // Should be boosted (7dB = -23 - (-30))
    assert!(l > 0.1, "Quiet track should be boosted, got {}", l);
}

#[test]
fn test_gain_smoothing() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::EbuR128);
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-20.0),
        ebu_r128_peak: Some(-1.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let mut prev_gain = norm.current_gain_linear;
    for _ in 0..1000 {
        norm.process(0.5, 0.5);
        let delta = (norm.current_gain_linear - prev_gain).abs();
        assert!(delta < 0.1, "Gain should change smoothly");
        prev_gain = norm.current_gain_linear;
    }
}

#[test]
fn test_gain_clamps_bound_boost_and_attenuation() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::EbuR128);
    norm.set_target_lufs(-23.0);
    norm.set_gain_clamps(Some(3.0), Some(-6.0));

    // A very quiet track wants a large +12 dB boost; the clamp must cap it
    // at +3 dB.
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-35.0),
        ebu_r128_peak: Some(-30.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let boost_db = 20.0 * norm.target_gain_linear.log10();
    assert!(
        (boost_db - 3.0).abs() < 0.01,
        "boost must be clamped to +3 dB, got {boost_db:.3} dB"
    );

    // A very loud track wants a large −12 dB cut; the clamp must cap it at
    // −6 dB.
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-11.0),
        ebu_r128_peak: Some(-2.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let atten_db = 20.0 * norm.target_gain_linear.log10();
    assert!(
        (atten_db - (-6.0)).abs() < 0.01,
        "attenuation must be clamped to −6 dB, got {atten_db:.3} dB"
    );
}

#[test]
fn test_gain_clamps_unlimited_by_default() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::EbuR128);
    norm.set_target_lufs(-23.0);
    // No clamps set (None): the full gain must be applied.
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-33.0), // +10 dB boost
        ebu_r128_peak: Some(-30.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let boost_db = 20.0 * norm.target_gain_linear.log10();
    assert!(
        (boost_db - 10.0).abs() < 0.05,
        "default must be unlimited (full +10 dB), got {boost_db:.3} dB"
    );
}

#[test]
fn test_true_peak_guard() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::TrackReplayGain);
    norm.set_true_peak_guard(true, -1.0);

    let meta = LoudnessMetadata {
        replaygain_track_db: Some(10.0),
        replaygain_track_peak: Some(0.8),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let guarded_gain = norm.target_gain_linear;

    norm.set_true_peak_guard(false, -1.0);
    norm.set_track_metadata(&meta);
    let unguarded_gain = norm.target_gain_linear;

    assert!(
        guarded_gain <= unguarded_gain,
        "True peak guard should reduce gain when needed"
    );
}

// \u2500\u2500 LoudnessMeter tests \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500

#[test]
fn test_loudness_meter_silence_below_absolute_gate() {
    // EBU R128 §3.1: blocks below -70 LUFS must be excluded from integration.
    let mut meter = LoudnessMeter::new(44100.0, 2);
    // Feed 5s of silence
    let silence = vec![0.0f32; 44100 * 5 * 2];
    meter.process_interleaved(&silence, 2);
    let m = meter.snapshot();
    // Integrated loudness of silence should be -inf or extremely quiet
    assert!(
        m.integrated_lufs < -69.0 || !m.integrated_lufs.is_finite(),
        "Silence must be below absolute gate, got {}",
        m.integrated_lufs
    );
}

#[test]
fn test_loudness_meter_sine_1khz() {
    // A 1 kHz sine at amplitude 0.1 should produce a finite integrated LUFS.
    let sr = 44100.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);
    let samples: Vec<f32> = (0..44100 * 4)
        .flat_map(|i| {
            let s = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
            [s, s]
        })
        .collect();
    meter.process_interleaved(&samples, 2);
    let m = meter.snapshot();
    // Should be a finite LUFS value well below 0 LUFS
    assert!(m.integrated_lufs.is_finite(), "Should produce finite LUFS");
    assert!(m.integrated_lufs < 0.0, "Should be negative LUFS");
    assert!(
        m.integrated_lufs > -60.0,
        "0.1 amplitude not that quiet: {}",
        m.integrated_lufs
    );
}

#[test]
fn test_loudness_meter_reset() {
    let sr = 44100.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);
    let signal: Vec<f32> = (0..44100)
        .flat_map(|i| {
            let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr).sin();
            [s, s]
        })
        .collect();
    meter.process_interleaved(&signal, 2);
    meter.reset();
    let m = meter.snapshot();
    // After reset, integrated LUFS should be non-finite (no blocks accumulated)
    assert!(
        !m.integrated_lufs.is_finite() || m.integrated_lufs < -60.0,
        "After reset, integrated LUFS should be effectively silent"
    );
}
