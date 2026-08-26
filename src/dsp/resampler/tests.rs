use super::super::AudioResampler;
use config::ResamplerQuality;

#[test]
fn test_resampler_creation() {
    let resampler =
        AudioResampler::<f32>::new(ResamplerQuality::Balanced, 44100.0, 48000.0).unwrap();
    assert!(!resampler.is_passthrough());
}

#[test]
fn test_resampler_f64_creation() {
    let mut resampler =
        AudioResampler::<f64>::new(ResamplerQuality::Balanced, 44100.0, 48000.0).unwrap();
    assert!(!resampler.is_passthrough());
    for i in 0..5000 {
        let sample = (i as f64 / 44100.0 * 440.0 * 2.0 * std::f64::consts::PI).sin() * 0.5;
        resampler.feed(sample, sample);
    }
    resampler.flush();
    assert!(resampler.available_output() > 0);
    let (l, r) = resampler.read().unwrap();
    assert!(l.abs() <= 1.0 && r.abs() <= 1.0);
}

#[test]
fn test_passthrough_detection() {
    let resampler =
        AudioResampler::<f32>::new(ResamplerQuality::Balanced, 44100.0, 44100.0).unwrap();
    assert!(resampler.is_passthrough());
}

#[test]
fn test_latency_reports_authoritative_group_delay() {
    // A real conversion must report rubato's nonzero filter group delay.
    let resampler =
        AudioResampler::<f32>::new(ResamplerQuality::Balanced, 44100.0, 48000.0).unwrap();
    assert!(
        resampler.latency_samples() > 0,
        "44.1->48 kHz must introduce filter delay"
    );

    // The ms value must be the frame count scaled at the OUTPUT rate.
    let expected_ms = resampler.latency_samples() as f32 / 48000.0 * 1000.0;
    assert!((resampler.latency_ms() - expected_ms).abs() < 1e-3);

    // f64 and f32 report the same group delay for the same conversion.
    let f64_resampler =
        AudioResampler::<f64>::new(ResamplerQuality::Balanced, 44100.0, 48000.0).unwrap();
    assert_eq!(resampler.latency_samples(), f64_resampler.latency_samples());
}

#[test]
fn test_passthrough_latency_is_zero() {
    let resampler = AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 44100.0).unwrap();
    assert!(resampler.is_passthrough());
    assert_eq!(resampler.latency_samples(), 0);
    assert_eq!(resampler.latency_ms(), 0.0);
}

#[test]
fn test_resampler_speed_change() {
    let mut resampler =
        AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 44100.0).unwrap();
    resampler.set_speed(1.5);
    assert!((resampler.speed() - 1.5).abs() < 0.001);
    assert!(resampler.rebuild_pending());
}

#[test]
fn test_resampler_produces_output() {
    let mut resampler =
        AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 48000.0).unwrap();
    for i in 0..5000 {
        let sample = (i as f32 / 44100.0 * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5;
        resampler.feed(sample, sample);
    }
    resampler.flush();
    assert!(
        resampler.available_output() > 0,
        "Resampler should produce output after feeding samples"
    );
}

#[test]
fn test_resampler_quality_change() {
    let mut resampler =
        AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 48000.0).unwrap();
    resampler.set_quality(ResamplerQuality::HighQuality);
    assert!(resampler.rebuild_pending());
}

#[test]
fn test_resampler_reset() {
    let mut resampler =
        AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 48000.0).unwrap();
    for _ in 0..1000 {
        resampler.feed(0.5f32, 0.5f32);
    }
    resampler.reset();
    assert_eq!(resampler.available_output(), 0);
    assert_eq!(resampler.available_output(), 0);
}

#[test]
fn test_resampler_invalid_rates() {
    let result = AudioResampler::<f32>::new(ResamplerQuality::Fast, 0.0, 48000.0);
    assert!(result.is_err());
    let result = AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 0.0);
    assert!(result.is_err());
}

#[test]
fn test_resampler_speed_2x_not_inverted() {
    let mut resampler =
        AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 44100.0).unwrap();
    resampler.set_speed(2.0);
    while resampler.rebuild_pending() || resampler.rebuild_pending() {
        resampler.feed(0.0f32, 0.0f32);
        if resampler.rebuild_pending() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    while resampler.read().is_some() {}

    let n_input: usize = 8192;
    for i in 0..n_input {
        let s = (i as f32 / 44100.0 * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5;
        resampler.feed(s, s);
    }
    resampler.flush();

    let mut n_output: usize = 0;
    while resampler.read().is_some() {
        n_output += 1;
    }
    let ratio = n_output as f32 / n_input as f32;
    assert!(
            ratio <= 1.25,
            "F#02 regression: speed=2.0 with {} input frames produced {} output (ratio {:.3}). \
             Correct ratio is ~0.5; inverted ratio is ~2.0. Got ratio > 1.25 → formula is inverted again.",
            n_input,
            n_output,
            ratio,
        );
}

#[test]
fn test_resampler_speed_half_not_inverted() {
    let mut resampler =
        AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 44100.0).unwrap();
    resampler.set_speed(0.5);
    while resampler.rebuild_pending() || resampler.rebuild_pending() {
        resampler.feed(0.0f32, 0.0f32);
        if resampler.rebuild_pending() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    while resampler.read().is_some() {}

    let n_input: usize = 4096;
    for i in 0..n_input {
        let s = (i as f32 / 44100.0 * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5;
        resampler.feed(s, s);
    }
    resampler.flush();

    let mut n_output: usize = 0;
    while resampler.read().is_some() {
        n_output += 1;
    }
    let ratio = n_output as f32 / n_input as f32;
    assert!(
            ratio >= 1.25,
            "F#02 regression: speed=0.5 with {} input frames produced {} output (ratio {:.3}). \
             Correct ratio is ~2.0; inverted ratio is ~0.5. Got ratio < 1.25 → formula is inverted again.",
            n_input,
            n_output,
            ratio,
        );
}
