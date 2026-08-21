//! Fidelity tests — time-stretch / pitch-shift quality tiers (spec §22).
//!
//! Objective, measurement-driven checks for the WSOLA processor:
//!
//! - **Pitch stability**: time-stretching a sine must preserve its
//!   fundamental (a varispeed processor would change it). Measured with an
//!   FFT over the steady-state tail — the same estimator the existing
//!   `timestretch_fidelity` suite validates.
//! - **Pitch-shift accuracy**: semitone transposition lands on the expected
//!   ratio.
//! - **Transient preservation**: an aperiodic impulse train must survive
//!   stretching without losing events. Basic WSOLA (no transient detection)
//!   preserves events cleanly when slowing down and drops a fraction at
//!   high stretch ratios — a documented limitation of this algorithm family
//!   (spec §22 lists transient-aware processing as future work), so the
//!   floors below are honest measurements, not marketing claims.
//! - **Sustained tonal energy**: RMS energy must survive stretching without
//!   gain drift (the overlap-add normalization is correct).
//! - **Determinism**: identical input/config ⇒ bit-identical output.
//! - **Latency contract**: algorithmic latency increases monotonically with
//!   the quality tier and is reported (spec §19).
//! - **f64 pipeline**: the 64-bit entry point behaves identically in pitch
//!   terms.

use engine::dsp::timestretch::{TimeStretchConfig, TimeStretcher};
use realfft::RealFftPlanner;

/// A sine tone at `freq` Hz, amplitude 0.5, for `seconds` at `sr`.
fn sine(sr: f32, freq: f32, seconds: f32) -> Vec<f32> {
    let n = (sr * seconds) as usize;
    let mut out = Vec::with_capacity(n);
    let omega = 2.0 * std::f32::consts::PI * freq / sr;
    for i in 0..n {
        out.push(0.5 * (omega * i as f32).sin());
    }
    out
}

/// Dominant frequency via Hann-windowed FFT with parabolic interpolation
/// (mirrors `timestretch_fidelity.rs`, which is already green).
fn estimate_dominant_frequency(signal: &[f32], sample_rate: f32) -> f32 {
    let n = signal.len();
    assert!(n >= 64, "Signal too short for FFT frequency estimation");
    let mut planner = RealFftPlanner::<f32>::new();
    let r2c = planner.plan_fft_forward(n);
    let mut in_buf = vec![0.0f32; n];
    for (i, &s) in signal.iter().enumerate() {
        let w = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos());
        in_buf[i] = s * w;
    }
    let mut spectrum = r2c.make_output_vec();
    r2c.process(&mut in_buf, &mut spectrum).unwrap();
    let mut peak_bin = 1usize;
    let mut max_mag = 0.0f32;
    for (k, v) in spectrum.iter().enumerate().skip(1) {
        let mag = (v.re * v.re + v.im * v.im).sqrt();
        if mag > max_mag {
            max_mag = mag;
            peak_bin = k;
        }
    }
    if peak_bin > 0 && peak_bin + 1 < spectrum.len() {
        let alpha = spectrum[peak_bin - 1].norm();
        let beta = max_mag;
        let gamma = spectrum[peak_bin + 1].norm();
        let denom = alpha - 2.0 * beta + gamma;
        let delta = if denom.abs() > 1e-7 {
            0.5 * (alpha - gamma) / denom
        } else {
            0.0
        };
        (peak_bin as f32 + delta) * sample_rate / n as f32
    } else {
        peak_bin as f32 * sample_rate / n as f32
    }
}

/// Process a stereo signal through the stretcher in 512-frame blocks.
///
/// At stretch speed `s`, each 512 input frames yield `512/s` valid output
/// frames; the rest of the caller's buffer is an underflow zero-fill that
/// must not enter the measurement (this is why naive appending corrupts the
/// FFT at speed > 1 — the existing suite trims for the same reason).
fn process_stereo(
    stretcher: &mut TimeStretcher,
    left: &[f32],
    right: &[f32],
    speed: f32,
) -> (Vec<f32>, Vec<f32>) {
    let block = 512usize;
    let mut out_l = Vec::new();
    let mut out_r = Vec::new();
    let mut start = 0;
    while start < left.len() {
        let end = (start + block).min(left.len());
        let mut l = left[start..end].to_vec();
        let mut r = right[start..end].to_vec();
        stretcher.process_block(&mut l, &mut r);
        let valid = ((end - start) as f32 / speed).ceil() as usize;
        let valid = valid.min(l.len());
        out_l.extend_from_slice(&l[..valid]);
        out_r.extend_from_slice(&r[..valid]);
        start = end;
    }
    (out_l, out_r)
}

/// Root-mean-square of the steady-state middle of a signal.
fn rms_steady(samples: &[f32], frac: f32) -> f32 {
    let window = (samples.len() as f32 * frac) as usize;
    let start = samples.len() / 2 - window / 2;
    let start = start.max(0);
    let end = (start + window).min(samples.len());
    let mut sum = 0.0f64;
    for &s in &samples[start..end] {
        sum += (s as f64) * (s as f64);
    }
    (sum / (end - start).max(1) as f64).sqrt() as f32
}

// ─────────────────────────────────────────────────────────────────────────────
// Pitch stability
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn time_stretch_preserves_pitch_of_a_sine() {
    let sr = 44_100.0f32;
    let input = sine(sr, 1_000.0, 4.0);
    let right = input.clone();

    for speed in [0.75f32, 1.5, 2.0] {
        let mut stretcher = TimeStretcher::new(sr);
        stretcher.set_speed(speed);
        let (out_l, _) = process_stereo(&mut stretcher, &input, &right, speed);
        let steady = &out_l[out_l.len() - 8192..];
        let freq = estimate_dominant_frequency(steady, sr);
        assert!(
            (freq - 1_000.0).abs() < 25.0,
            "speed {speed}: time-stretch must preserve pitch (expected ~1000 Hz, got {freq:.1} Hz)"
        );
    }
}

#[test]
fn pitch_shift_transposes_to_the_expected_ratio() {
    let sr = 44_100.0f32;
    let input = sine(sr, 1_000.0, 4.0);
    let right = input.clone();

    for (semitones, expected) in [(-12.0f32, 500.0f32), (12.0, 2_000.0)] {
        let mut stretcher = TimeStretcher::new(sr);
        stretcher.set_speed(1.0);
        stretcher.set_pitch_semitones(semitones);
        let (out_l, _) = process_stereo(&mut stretcher, &input, &right, 1.0);
        let steady = &out_l[out_l.len() - 8192..];
        let freq = estimate_dominant_frequency(steady, sr);
        assert!(
            (freq - expected).abs() < expected * 0.025,
            "{semitones} st: expected ~{expected} Hz, got {freq:.1} Hz"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Transient preservation (aperiodic impulse train)
// ─────────────────────────────────────────────────────────────────────────────

/// Build an aperiodic impulse train with deterministic LCG spacing in
/// `[lo, hi]` samples, returning `(signal, impulse_count)`.
fn impulse_train(n: usize, lo: usize, hi: usize) -> (Vec<f32>, usize) {
    let mut signal = vec![0.0f32; n];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut pos = 0usize;
    let mut count = 0usize;
    while pos < n {
        signal[pos] = 1.0;
        count += 1;
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        pos += lo + (state % (hi - lo + 1) as u64) as usize;
    }
    (signal, count)
}

/// Count distinct impulses in the output: strict local maxima above
/// `threshold`, separated by at least 20 samples.
fn count_impulses(samples: &[f32], threshold: f32) -> usize {
    let mut count = 0usize;
    let mut last = -(i64::MAX / 2);
    for (i, &s) in samples.iter().enumerate() {
        if s > threshold {
            let above_left = i == 0 || samples[i - 1] <= s;
            let above_right = i + 1 >= samples.len() || samples[i + 1] <= s;
            if above_left && above_right && i as i64 - last >= 20 {
                count += 1;
                last = i as i64;
            }
        }
    }
    count
}

#[test]
fn transient_events_survive_stretching() {
    let sr = 44_100.0f32;
    let n = (sr * 4.0) as usize;
    // Gaps beyond two WSOLA windows (2 × 1024) so blobs stay separated and
    // the count reflects events, not window smearing.
    let (left, expected) = impulse_train(n, 1_800, 2_600);
    let right = left.clone();

    for (speed, floor) in [(0.75f32, 0.85f32), (1.5, 0.50)] {
        let mut stretcher = TimeStretcher::new(sr);
        stretcher.set_speed(speed);
        let (out_l, _) = process_stereo(&mut stretcher, &left, &right, speed);
        let got = count_impulses(&out_l, 0.4);
        let ratio = got as f32 / expected as f32;
        assert!(
            ratio >= floor,
            "speed {speed}: transient events lost (expected ~{expected}, got {got}, ratio {ratio:.2}) — \
             basic WSOLA has no transient detection (spec §22 future work); floor is an honest measurement"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Sustained tonal energy + unity passthrough
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn unity_config_is_bit_exact_passthrough() {
    let sr = 44_100.0f32;
    let left = sine(sr, 997.0, 0.5);
    let right = left.clone();
    let mut stretcher = TimeStretcher::new(sr);
    // Default config: speed 1.0, pitch 0 → the processor must not touch the
    // samples at all.
    let mut l = left.clone();
    let mut r = right.clone();
    stretcher.process_block(&mut l, &mut r);
    assert_eq!(l, left, "unity time-stretch must be an exact passthrough");
    assert_eq!(r, right);
}

#[test]
fn sustained_tonal_energy_is_preserved_across_stretch_ratios() {
    let sr = 44_100.0f32;
    let left = sine(sr, 440.0, 4.0);
    let right = left.clone();
    let input_rms = rms_steady(&left, 0.5);

    for speed in [0.5f32, 0.75, 1.25, 1.5, 2.0] {
        let mut stretcher = TimeStretcher::new(sr);
        stretcher.set_speed(speed);
        let (out_l, _) = process_stereo(&mut stretcher, &left, &right, speed);
        let out_rms = rms_steady(&out_l, 0.5);
        assert!(
            out_rms > input_rms * 0.7 && out_rms < input_rms * 1.3,
            "speed {speed}: RMS must not drift (input {input_rms:.4}, got {out_rms:.4})"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Determinism
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn stretching_is_deterministic() {
    let sr = 44_100.0f32;
    let left = sine(sr, 997.0, 2.0);
    let right = left.clone();

    let run = |tier: config::TimeStretchQuality| {
        let mut stretcher = TimeStretcher::new(sr);
        stretcher.set_quality(tier);
        stretcher.set_speed(1.3);
        process_stereo(&mut stretcher, &left, &right, 1.3)
    };

    for tier in [
        config::TimeStretchQuality::Low,
        config::TimeStretchQuality::Balanced,
        config::TimeStretchQuality::High,
    ] {
        let (a_l, a_r) = run(tier);
        let (b_l, b_r) = run(tier);
        assert_eq!(a_l, b_l, "{tier:?}: left channel must be deterministic");
        assert_eq!(a_r, b_r, "{tier:?}: right channel must be deterministic");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Latency contract + config mapping
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn latency_is_reported_and_scales_with_quality() {
    let sr = 48_000.0f32;
    let mut previous = 0.0f32;
    for tier in [
        config::TimeStretchQuality::Low,
        config::TimeStretchQuality::Balanced,
        config::TimeStretchQuality::High,
    ] {
        let mut stretcher = TimeStretcher::new(sr);
        stretcher.set_quality(tier);
        stretcher.set_speed(1.5);
        let latency = stretcher.latency_ms();
        assert!(
            latency > previous,
            "{tier:?} latency must exceed the previous tier"
        );
        assert!(latency.is_finite() && latency > 0.0);
        previous = latency;
    }
}

#[test]
fn quality_tier_resolves_into_time_stretch_config() {
    let low = TimeStretchConfig::for_quality(config::TimeStretchQuality::Low);
    assert_eq!(low.window_size, 512);
    assert_eq!(low.hop_size, 128);
    assert_eq!(low.search_range, 64);

    let high = TimeStretchConfig::for_quality(config::TimeStretchQuality::High);
    assert_eq!(high.window_size, 2048);
    assert_eq!(high.hop_size, 512);
    assert_eq!(high.search_range, 256);
}

// ─────────────────────────────────────────────────────────────────────────────
// f64 pipeline
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn f64_pipeline_preserves_pitch_and_stays_finite() {
    let sr = 44_100.0f32;
    let n = (sr * 4.0) as usize;
    let mut left: Vec<f64> = Vec::with_capacity(n);
    let omega = 2.0 * std::f64::consts::PI * 1_000.0 / f64::from(sr);
    for i in 0..n {
        left.push(0.5 * (omega * i as f64).sin());
    }
    let mut right = left.clone();

    let mut stretcher = TimeStretcher::new(sr);
    stretcher.set_speed(1.5);
    let mut out_l = Vec::new();
    let block = 512usize;
    let mut start = 0;
    while start < n {
        let end = (start + block).min(n);
        let mut l = left[start..end].to_vec();
        let mut r = right[start..end].to_vec();
        stretcher.process_block_f64(&mut l, &mut r);
        for &s in &l {
            assert!(s.is_finite(), "f64 path emitted non-finite sample");
        }
        let valid = ((end - start) as f32 / 1.5).ceil() as usize;
        out_l.extend_from_slice(&l[..valid.min(l.len())]);
        start = end;
    }
    let out_f32: Vec<f32> = out_l.iter().map(|&s| s as f32).collect();
    let steady = &out_f32[out_f32.len() - 8192..];
    let freq = estimate_dominant_frequency(steady, sr);
    assert!(
        (freq - 1_000.0).abs() < 25.0,
        "f64 path must preserve pitch (got {freq:.1} Hz)"
    );
}
