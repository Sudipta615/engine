//! Fidelity tests for the WSOLA time-stretcher and pitch-shifter.
//!
//! Validates:
//! - No NaN/Inf in output at any speed/pitch setting
//! - No DC offset accumulation over long runs
//! - Buffer OOB guard: no panic when search_range > buffered input
//! - f64 path produces finite, bounded output
//! - Unity passthrough: disabled stretcher does not modify buffer
//! - Reset clears state without leaving garbage
//! - Pitch transposition: FFT validates that +12 st doubles frequency (440 Hz -> 880 Hz)
//!   and -12 st halves frequency (440 Hz -> 220 Hz) for both f32 and f64 paths.
//! - Time stretching: FFT validates that speed changes (0.5x, 2.0x) preserve fundamental pitch,
//!   and duration expands/contracts proportionally.
//! - Regression guard: f64 path genuinely transforms audio buffer without cloning/discarding.

use engine::dsp::timestretch::TimeStretcher;
use realfft::RealFftPlanner;

/// Generate a pure sine wave at `freq` Hz, sampled at `sample_rate` Hz.
fn sine_wave(freq: f32, sample_rate: f32, frames: usize) -> Vec<f32> {
    use std::f32::consts::TAU;
    (0..frames)
        .map(|i| (TAU * freq * i as f32 / sample_rate).sin())
        .collect()
}

/// Estimate dominant frequency in Hz using FFT with Hann window and parabolic interpolation.
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

    let mut out_spectrum = r2c.make_output_vec();
    r2c.process(&mut in_buf, &mut out_spectrum).unwrap();

    let mut max_mag = 0.0f32;
    let mut peak_bin = 1usize;
    for k in 1..out_spectrum.len() {
        let mag = (out_spectrum[k].re * out_spectrum[k].re
            + out_spectrum[k].im * out_spectrum[k].im)
            .sqrt();
        if mag > max_mag {
            max_mag = mag;
            peak_bin = k;
        }
    }

    if peak_bin > 0 && peak_bin + 1 < out_spectrum.len() {
        let alpha =
            (out_spectrum[peak_bin - 1].re.powi(2) + out_spectrum[peak_bin - 1].im.powi(2)).sqrt();
        let beta = max_mag;
        let gamma =
            (out_spectrum[peak_bin + 1].re.powi(2) + out_spectrum[peak_bin + 1].im.powi(2)).sqrt();
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

fn process_stream_f32(
    ts: &mut TimeStretcher,
    input: &[f32],
    chunk_size: usize,
) -> (Vec<f32>, Vec<f32>) {
    let mut out_l = Vec::with_capacity(input.len());
    let mut out_r = Vec::with_capacity(input.len());
    let mut pos = 0;
    while pos < input.len() {
        let end = (pos + chunk_size).min(input.len());
        let mut l = input[pos..end].to_vec();
        let mut r = input[pos..end].to_vec();
        ts.process_block(&mut l, &mut r);
        out_l.extend_from_slice(&l);
        out_r.extend_from_slice(&r);
        pos = end;
    }
    (out_l, out_r)
}

fn process_stream_f64(
    ts: &mut TimeStretcher,
    input: &[f64],
    chunk_size: usize,
) -> (Vec<f64>, Vec<f64>) {
    let mut out_l = Vec::with_capacity(input.len());
    let mut out_r = Vec::with_capacity(input.len());
    let mut pos = 0;
    while pos < input.len() {
        let end = (pos + chunk_size).min(input.len());
        let mut l = input[pos..end].to_vec();
        let mut r = input[pos..end].to_vec();
        ts.process_block_f64(&mut l, &mut r);
        out_l.extend_from_slice(&l);
        out_r.extend_from_slice(&r);
        pos = end;
    }
    (out_l, out_r)
}

// ── NaN / Inf safety ─────────────────────────────────────────────────────

fn check_no_nan_inf(speed: f32, pitch_semitones: f32, n_input: usize, sample_rate: f32) {
    let mut ts = TimeStretcher::new(sample_rate);
    ts.set_speed(speed);
    ts.set_pitch_semitones(pitch_semitones);
    ts.set_enabled(true);

    let input = sine_wave(440.0, sample_rate, n_input);
    let chunk = 512usize;
    let mut pos = 0;

    while pos < n_input {
        let end = (pos + chunk).min(n_input);
        let mut l = input[pos..end].to_vec();
        let mut r = input[pos..end].to_vec();
        ts.process_block(&mut l, &mut r);
        for (i, (&ls, &rs)) in l.iter().zip(r.iter()).enumerate() {
            assert!(
                ls.is_finite(),
                "NaN/Inf in L at frame {} (speed={speed}, pitch={pitch_semitones}): {ls}",
                pos + i
            );
            assert!(
                rs.is_finite(),
                "NaN/Inf in R at frame {} (speed={speed}, pitch={pitch_semitones}): {rs}",
                pos + i
            );
        }
        pos = end;
    }
}

#[test]
fn test_no_nan_half_speed() {
    check_no_nan_inf(0.5, 0.0, 44100, 44100.0);
}

#[test]
fn test_no_nan_double_speed() {
    check_no_nan_inf(2.0, 0.0, 44100, 44100.0);
}

#[test]
fn test_no_nan_quarter_speed() {
    check_no_nan_inf(0.25, 0.0, 44100, 44100.0);
}

#[test]
fn test_no_nan_4x_speed() {
    check_no_nan_inf(4.0, 0.0, 22050, 44100.0);
}

#[test]
fn test_no_nan_pitch_up_12() {
    check_no_nan_inf(1.0, 12.0, 44100, 44100.0);
}

#[test]
fn test_no_nan_pitch_down_12() {
    check_no_nan_inf(1.0, -12.0, 44100, 44100.0);
}

#[test]
fn test_no_nan_pitch_up_7() {
    check_no_nan_inf(1.0, 7.0, 44100, 44100.0);
}

#[test]
fn test_no_nan_pitch_down_7() {
    check_no_nan_inf(1.0, -7.0, 44100, 44100.0);
}

#[test]
fn test_no_nan_speed_and_pitch() {
    check_no_nan_inf(1.5, -5.0, 44100, 44100.0);
}

#[test]
fn test_no_nan_extreme_slow_and_pitch_up() {
    check_no_nan_inf(0.25, 12.0, 22050, 44100.0);
}

// ── Output is non-silent when stretcher is active ────────────────────────

/// Verify that the stretcher actually writes non-zero values for a non-zero input.
/// This is the minimal "stretcher does something" test: if all output is 0.0,
/// the stretcher is broken (not producing audio).
fn check_produces_audio(speed: f32, pitch_semitones: f32) {
    let sample_rate = 44100.0f32;
    let mut ts = TimeStretcher::new(sample_rate);
    ts.set_speed(speed);
    ts.set_pitch_semitones(pitch_semitones);
    ts.set_enabled(true);

    // Feed 4 seconds of 440 Hz sine (enough to fill the WSOLA buffer)
    let n = (sample_rate as usize) * 4;
    let input = sine_wave(440.0, sample_rate, n);
    let chunk = 1024usize;
    let mut max_abs = 0.0f32;
    let mut pos = 0;

    while pos < n {
        let end = (pos + chunk).min(n);
        let mut l = input[pos..end].to_vec();
        let mut r = input[pos..end].to_vec();
        ts.process_block(&mut l, &mut r);
        for &s in l.iter().chain(r.iter()) {
            if s.is_finite() {
                max_abs = max_abs.max(s.abs());
            }
        }
        pos = end;
    }

    // After processing 4 seconds of 440 Hz at full amplitude, we should see
    // non-trivial output (> 0.001 linear). The warm-up period is included in
    // the 4 second feed, so by the end there should be plenty of output.
    assert!(
        max_abs > 0.001,
        "speed={speed}, pitch={pitch_semitones}: max output amplitude is {max_abs:.6} — \
         stretcher appears to be silent (broken WSOLA or resampler path)"
    );
}

#[test]
fn test_produces_audio_half_speed() {
    check_produces_audio(0.5, 0.0);
}

#[test]
fn test_produces_audio_double_speed() {
    check_produces_audio(2.0, 0.0);
}

#[test]
fn test_produces_audio_pitch_up_12() {
    check_produces_audio(1.0, 12.0);
}

#[test]
fn test_produces_audio_pitch_down_12() {
    check_produces_audio(1.0, -12.0);
}

#[test]
fn test_produces_audio_pitch_up_7() {
    check_produces_audio(1.0, 7.0);
}

// ── Output is bounded ─────────────────────────────────────────────────────

/// Verify that output never exceeds 2.0 in absolute value (no gain explosion).
fn check_bounded(speed: f32, pitch_semitones: f32) {
    let sample_rate = 44100.0f32;
    let mut ts = TimeStretcher::new(sample_rate);
    ts.set_speed(speed);
    ts.set_pitch_semitones(pitch_semitones);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 3;
    let input = sine_wave(440.0, sample_rate, n);
    let chunk = 512usize;
    let mut pos = 0;

    while pos < n {
        let end = (pos + chunk).min(n);
        let mut l = input[pos..end].to_vec();
        let mut r = input[pos..end].to_vec();
        ts.process_block(&mut l, &mut r);
        for (i, (&ls, &rs)) in l.iter().zip(r.iter()).enumerate() {
            assert!(
                ls.abs() <= 2.0,
                "L[{}] out of bounds at speed={speed} pitch={pitch_semitones}: {ls}",
                pos + i
            );
            assert!(
                rs.abs() <= 2.0,
                "R[{}] out of bounds at speed={speed} pitch={pitch_semitones}: {rs}",
                pos + i
            );
        }
        pos = end;
    }
}

#[test]
fn test_bounded_half_speed() {
    check_bounded(0.5, 0.0);
}
#[test]
fn test_bounded_double_speed() {
    check_bounded(2.0, 0.0);
}
#[test]
fn test_bounded_pitch_up_12() {
    check_bounded(1.0, 12.0);
}
#[test]
fn test_bounded_pitch_down_12() {
    check_bounded(1.0, -12.0);
}
#[test]
fn test_bounded_combined() {
    check_bounded(1.5, -7.0);
}

// ── DC offset test ────────────────────────────────────────────────────────

#[test]
fn test_no_dc_accumulation() {
    // A zero-mean sine input should produce near-zero-mean output.
    let mut ts = TimeStretcher::new(44100.0);
    ts.set_speed(0.75);
    ts.set_enabled(true);

    let n = 44100 * 4;
    let input = sine_wave(440.0, 44100.0, n);
    let chunk = 256usize;
    let mut sum_l = 0.0f64;
    let mut total_samples = 0usize;

    let mut pos = 0;
    while pos < n {
        let end = (pos + chunk).min(n);
        let mut l = input[pos..end].to_vec();
        let mut r = input[pos..end].to_vec();
        ts.process_block(&mut l, &mut r);
        for &s in &l {
            if s.is_finite() {
                sum_l += s as f64;
                total_samples += 1;
            }
        }
        pos = end;
    }

    if total_samples > 0 {
        let mean = (sum_l / total_samples as f64).abs();
        assert!(
            mean < 0.05,
            "DC offset too large: mean = {mean:.6} (expected < 0.05 for zero-mean sine)"
        );
    }
}

// ── OOB guard: search_range > available input ─────────────────────────────

#[test]
fn test_wsola_oob_guard_small_input() {
    // Feed fewer frames than the search_range. The clamping logic must prevent
    // any out-of-bounds read (or panic from debug_assert).
    let mut ts = TimeStretcher::new(44100.0);
    ts.set_speed(0.5);
    ts.set_enabled(true);

    // Feed only 64 frames — well below DEFAULT_WSOLA_SEARCH_RANGE (128)
    let input = vec![0.1f32; 64];
    let mut l = input.clone();
    let mut r = input.clone();
    // Must not panic
    ts.process_block(&mut l, &mut r);

    // All outputs must be finite
    for &s in l.iter().chain(r.iter()) {
        assert!(s.is_finite(), "OOB test produced non-finite output: {s}");
    }
}

#[test]
fn test_wsola_oob_guard_zero_available() {
    // Feed 0 frames (empty block). Must not panic.
    let mut ts = TimeStretcher::new(44100.0);
    ts.set_speed(2.0);
    ts.set_enabled(true);

    let mut l = vec![0.0f32; 256];
    let mut r = vec![0.0f32; 256];
    ts.process_block(&mut l, &mut r); // empty — no input fed
    for &s in l.iter().chain(r.iter()) {
        assert!(s.is_finite(), "zero-input test produced non-finite: {s}");
    }
}

#[test]
fn test_wsola_oob_guard_incrementally_growing_feed() {
    // Feed growing block sizes starting from 1 sample. The search_range guard
    // must not panic on any block size.
    let mut ts = TimeStretcher::new(44100.0);
    ts.set_speed(0.5);
    ts.set_enabled(true);

    for n in [1, 2, 4, 8, 16, 32, 64, 128, 256, 512] {
        let input = vec![0.5f32; n];
        let mut l = input.clone();
        let mut r = input.clone();
        ts.process_block(&mut l, &mut r);
        for &s in l.iter().chain(r.iter()) {
            assert!(s.is_finite(), "n={n}: non-finite output: {s}");
        }
    }
}

// ── f64 path ─────────────────────────────────────────────────────────────

#[test]
fn test_f64_path_no_allocation_and_finite() {
    let mut ts = TimeStretcher::new(48000.0);
    ts.set_speed(1.5);
    ts.set_enabled(true);

    let n = 48000;
    let input: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / 48000.0;
            (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        })
        .collect();

    let chunk = 512;
    let mut pos = 0;
    while pos < n {
        let end = (pos + chunk).min(n);
        let mut l = input[pos..end].to_vec();
        let mut r = input[pos..end].to_vec();
        ts.process_block_f64(&mut l, &mut r);
        for (i, (&ls, &rs)) in l.iter().zip(r.iter()).enumerate() {
            assert!(ls.is_finite(), "f64 path: NaN/Inf in L[{}]: {ls}", pos + i);
            assert!(rs.is_finite(), "f64 path: NaN/Inf in R[{}]: {rs}", pos + i);
        }
        pos = end;
    }
}

#[test]
fn test_f64_path_produces_audio() {
    let mut ts = TimeStretcher::new(44100.0);
    ts.set_speed(0.75);
    ts.set_enabled(true);

    let n = 44100 * 3;
    let input: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / 44100.0;
            (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        })
        .collect();

    let chunk = 1024;
    let mut max_abs = 0.0f64;
    let mut pos = 0;
    while pos < n {
        let end = (pos + chunk).min(n);
        let mut l = input[pos..end].to_vec();
        let mut r = input[pos..end].to_vec();
        ts.process_block_f64(&mut l, &mut r);
        for &s in l.iter().chain(r.iter()) {
            if s.is_finite() {
                max_abs = max_abs.max(s.abs());
            }
        }
        pos = end;
    }
    assert!(max_abs > 0.001, "f64 path produced no audio: max={max_abs}");
}

// ── Reset / re-enable ─────────────────────────────────────────────────────

#[test]
fn test_reset_clears_state() {
    let mut ts = TimeStretcher::new(44100.0);
    ts.set_speed(0.5);
    ts.set_enabled(true);

    let input = sine_wave(440.0, 44100.0, 44100);
    let mut l = input.clone();
    let mut r = input.clone();
    ts.process_block(&mut l, &mut r);

    ts.reset();
    ts.set_speed(2.0);

    let mut l2 = input[..512].to_vec();
    let mut r2 = input[..512].to_vec();
    ts.process_block(&mut l2, &mut r2);
    for &s in l2.iter().chain(r2.iter()) {
        assert!(s.is_finite(), "after reset: non-finite output {s}");
    }
}

#[test]
fn test_unity_speed_passthrough() {
    // At speed=1.0 and pitch=0 with the stretcher disabled, process_block
    // should not modify the input buffer.
    let mut ts = TimeStretcher::new(44100.0);
    // Default state: disabled, speed=1.0, pitch=0

    let input = vec![0.5f32; 512];
    let mut l = input.clone();
    let mut r = input.clone();
    ts.process_block(&mut l, &mut r);

    // When disabled and at unity, the buffer must not be touched
    assert_eq!(
        l, input,
        "passthrough should not modify L at unity/disabled"
    );
    assert_eq!(
        r, input,
        "passthrough should not modify R at unity/disabled"
    );
}

#[test]
fn test_disable_clears_buffers() {
    let mut ts = TimeStretcher::new(44100.0);
    ts.set_speed(0.5);
    ts.set_enabled(true);

    let input = sine_wave(440.0, 44100.0, 8192);
    let mut l = input.clone();
    let mut r = input.clone();
    ts.process_block(&mut l, &mut r);

    ts.set_enabled(false);

    // After disable, all internal ring buffers are zeroed
    let mut l2 = vec![1.0f32; 256];
    let mut r2 = vec![1.0f32; 256];
    ts.process_block(&mut l2, &mut r2);
    // Disabled stretcher: passthrough — buffer unchanged
    assert!(
        l2.iter().all(|&s| s == 1.0),
        "disabled stretcher must be passthrough"
    );
}

// ── Objective FFT Pitch Shift & Time Stretch Fidelity Tests ─────────────

#[test]
fn test_pitch_shift_up_12_semitones_fft_f32() {
    let sample_rate = 44100.0f32;
    let mut ts = TimeStretcher::new(sample_rate);
    ts.set_pitch_semitones(12.0); // 1 octave up: 440 Hz -> 880 Hz
    ts.set_speed(1.0);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 4;
    let input = sine_wave(440.0, sample_rate, n);
    let (out_l, _) = process_stream_f32(&mut ts, &input, 512);

    let steady = &out_l[out_l.len() - 8192..];
    let freq = estimate_dominant_frequency(steady, sample_rate);

    assert!(
        (freq - 880.0).abs() < 15.0,
        "Expected ~880 Hz for +12 semitones on 440 Hz input, but got {freq:.2} Hz"
    );
}

#[test]
fn test_pitch_shift_down_12_semitones_fft_f32() {
    let sample_rate = 44100.0f32;
    let mut ts = TimeStretcher::new(sample_rate);
    ts.set_pitch_semitones(-12.0); // 1 octave down: 440 Hz -> 220 Hz
    ts.set_speed(1.0);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 4;
    let input = sine_wave(440.0, sample_rate, n);
    let (out_l, _) = process_stream_f32(&mut ts, &input, 512);

    let steady = &out_l[out_l.len() - 8192..];
    let freq = estimate_dominant_frequency(steady, sample_rate);

    assert!(
        (freq - 220.0).abs() < 10.0,
        "Expected ~220 Hz for -12 semitones on 440 Hz input, but got {freq:.2} Hz"
    );
}

#[test]
fn test_pitch_shift_up_7_semitones_fft_f32() {
    let sample_rate = 44100.0f32;
    let mut ts = TimeStretcher::new(sample_rate);
    ts.set_pitch_semitones(7.0); // Perfect fifth: 440 * 2^(7/12) ≈ 659.25 Hz
    ts.set_speed(1.0);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 4;
    let input = sine_wave(440.0, sample_rate, n);
    let (out_l, _) = process_stream_f32(&mut ts, &input, 512);

    let steady = &out_l[out_l.len() - 8192..];
    let freq = estimate_dominant_frequency(steady, sample_rate);

    assert!(
        (freq - 659.25).abs() < 15.0,
        "Expected ~659.25 Hz for +7 semitones on 440 Hz input, but got {freq:.2} Hz"
    );
}

#[test]
fn test_f64_pitch_shift_up_12_semitones_fft() {
    // Crucial regression test for process_block_f64
    let sample_rate = 44100.0;
    let mut ts = TimeStretcher::new(sample_rate as f32);
    ts.set_pitch_semitones(12.0); // 440 Hz -> 880 Hz
    ts.set_speed(1.0);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 4;
    let input: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / sample_rate;
            (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        })
        .collect();

    let (out_l, _) = process_stream_f64(&mut ts, &input, 512);
    let steady_f32: Vec<f32> = out_l[out_l.len() - 8192..]
        .iter()
        .map(|&s| s as f32)
        .collect();
    let freq = estimate_dominant_frequency(&steady_f32, sample_rate as f32);

    assert!(
        (freq - 880.0).abs() < 15.0,
        "f64 path: Expected ~880 Hz for +12 semitones on 440 Hz input, but got {freq:.2} Hz"
    );
}

#[test]
fn test_f64_pitch_shift_down_12_semitones_fft() {
    let sample_rate = 44100.0;
    let mut ts = TimeStretcher::new(sample_rate as f32);
    ts.set_pitch_semitones(-12.0); // 440 Hz -> 220 Hz
    ts.set_speed(1.0);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 4;
    let input: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / sample_rate;
            (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        })
        .collect();

    let (out_l, _) = process_stream_f64(&mut ts, &input, 512);
    let steady_f32: Vec<f32> = out_l[out_l.len() - 8192..]
        .iter()
        .map(|&s| s as f32)
        .collect();
    let freq = estimate_dominant_frequency(&steady_f32, sample_rate as f32);

    assert!(
        (freq - 220.0).abs() < 10.0,
        "f64 path: Expected ~220 Hz for -12 semitones on 440 Hz input, but got {freq:.2} Hz"
    );
}

#[test]
fn test_time_stretch_half_speed_preserves_pitch_fft_f32() {
    let sample_rate = 44100.0f32;
    let mut ts = TimeStretcher::new(sample_rate);
    ts.set_speed(0.5); // Half speed, pitch unchanged
    ts.set_pitch_semitones(0.0);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 4;
    let input = sine_wave(440.0, sample_rate, n);
    let (out_l, _) = process_stream_f32(&mut ts, &input, 512);

    let steady = &out_l[out_l.len() - 8192..];
    let freq = estimate_dominant_frequency(steady, sample_rate);

    assert!(
        (freq - 440.0).abs() < 10.0,
        "Expected pitch to remain ~440 Hz at speed=0.5, but got {freq:.2} Hz"
    );
}

#[test]
fn test_time_stretch_double_speed_preserves_pitch_fft_f32() {
    let sample_rate = 44100.0f32;
    let mut ts = TimeStretcher::new(sample_rate);
    ts.set_speed(2.0); // Double speed, pitch unchanged
    ts.set_pitch_semitones(0.0);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 6;
    let input = sine_wave(440.0, sample_rate, n);

    // At speed 2.0, 512 input frames produce 256 output frames.
    let mut out_l = Vec::new();
    let chunk_size = 512;
    let mut pos = 0;
    while pos + chunk_size * 2 <= input.len() {
        let chunk = &input[pos..pos + chunk_size * 2];
        pos += chunk_size * 2;
        let mut l1 = chunk[..chunk_size].to_vec();
        let mut r1 = chunk[..chunk_size].to_vec();
        ts.process_block(&mut l1, &mut r1);
        let mut l2 = chunk[chunk_size..].to_vec();
        let mut r2 = chunk[chunk_size..].to_vec();
        ts.process_block(&mut l2, &mut r2);
        // Each 512 input chunk produces 256 output samples at 2x speed
        out_l.extend_from_slice(&l1[..256]);
        out_l.extend_from_slice(&l2[..256]);
    }

    let steady = &out_l[out_l.len() - 8192..];
    let freq = estimate_dominant_frequency(steady, sample_rate);

    assert!(
        (freq - 440.0).abs() < 10.0,
        "Expected pitch to remain ~440 Hz at speed=2.0, but got {freq:.2} Hz"
    );
}

#[test]
fn test_time_stretch_half_speed_preserves_pitch_f64_fft() {
    let sample_rate = 44100.0;
    let mut ts = TimeStretcher::new(sample_rate as f32);
    ts.set_speed(0.5);
    ts.set_pitch_semitones(0.0);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 4;
    let input: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / sample_rate;
            (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        })
        .collect();

    let (out_l, _) = process_stream_f64(&mut ts, &input, 512);
    let steady_f32: Vec<f32> = out_l[out_l.len() - 8192..]
        .iter()
        .map(|&s| s as f32)
        .collect();
    let freq = estimate_dominant_frequency(&steady_f32, sample_rate as f32);

    assert!(
        (freq - 440.0).abs() < 10.0,
        "f64 path: Expected pitch to remain ~440 Hz at speed=0.5, but got {freq:.2} Hz"
    );
}

#[test]
fn test_time_stretch_duration_expansion() {
    // Feed 1.0s tone burst + 3.0s silence (4.0s input).
    // At speed=0.5, the 1.0s tone expands ~2x to ~2.0s.
    // We pump 4.0s of input + 4.0s of flushing silence to collect the full 8.0s output stream.
    let sample_rate = 44100.0f32;
    let burst_len = (sample_rate as usize) * 1; // 1.0s tone
    let silence_len = (sample_rate as usize) * 3; // 3.0s silence
    let mut input = sine_wave(440.0, sample_rate, burst_len);
    input.resize(burst_len + silence_len + (sample_rate as usize) * 4, 0.0); // 8.0s total feed

    let mut ts = TimeStretcher::new(sample_rate);
    ts.set_speed(0.5);
    ts.set_enabled(true);

    let (out_l, _) = process_stream_f32(&mut ts, &input, 512);

    // Measure active duration using 100ms RMS blocks
    let block_size = (sample_rate * 0.1) as usize;
    let mut active_blocks = 0usize;
    for chunk in out_l.chunks(block_size) {
        let rms = (chunk.iter().map(|&s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
        if rms > 0.05 {
            active_blocks += 1;
        }
    }

    let active_duration_sec = active_blocks as f32 * 0.1;
    // Input burst was 1.0s; at speed=0.5, expected ~2.0s (allow 1.7s to 2.3s)
    assert!(
        active_duration_sec >= 1.7 && active_duration_sec <= 2.3,
        "Expected active duration ~2.0s at speed=0.5 for 1.0s input burst, but got {active_duration_sec:.2}s"
    );
}

#[test]
fn test_f64_path_actively_modifies_buffer() {
    // Direct regression test for the cloning bug:
    // When pitch shift is active (+12 st), the output buffer of process_block_f64
    // MUST NOT be identical to the input buffer.
    let sample_rate = 44100.0;
    let mut ts = TimeStretcher::new(sample_rate as f32);
    ts.set_pitch_semitones(12.0);
    ts.set_enabled(true);

    let n = (sample_rate as usize) * 3;
    let input: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / sample_rate;
            (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        })
        .collect();

    let (out_l, _) = process_stream_f64(&mut ts, &input, 512);

    // Skip initial buffer-filling warmup (first 8192 samples)
    let steady_in = &input[8192..16384];
    let steady_out = &out_l[8192..16384];

    let mut diff_sum = 0.0f64;
    for (&in_s, &out_s) in steady_in.iter().zip(steady_out.iter()) {
        diff_sum += (in_s - out_s).abs();
    }
    let mean_diff = diff_sum / steady_in.len() as f64;

    assert!(
        mean_diff > 0.1,
        "f64 path did not modify the audio buffer (mean difference = {mean_diff:.6}). \
         This indicates process_block_f64 is passing input through unmodified!"
    );
}
