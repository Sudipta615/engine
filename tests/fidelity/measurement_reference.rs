//! Reference-grade measurements that were missing from the fidelity suite
//! (spec §25, §26):
//!
//! - **Convolution** — the partitioned FFT engine compared against a naive
//!   `f64` direct convolution (the offline reference), mono and stereo IR.
//! - **Resampler impulse response** — a unit impulse must yield a finite,
//!   unity-DC-gain response whose peak lands at the reported group delay.
//! - **Resampler two-tone co-existence** — an in-band tone and an out-of-band
//!   tone processed simultaneously: the in-band tone survives at unity while
//!   the out-of-band tone's alias image stays deeply suppressed.
//! - **Crossfade continuity** — every curve variant produces a continuous,
//!   bounded output with no gap, step, or NaN, and the outgoing/incoming
//!   envelopes are monotonic.
//! - **Multichannel golden vectors** — per-channel identity (no drop/reorder)
//!   and an explicit permutation routing matrix.

use config::{ChannelRoutingConfig, ChannelTrimConfig, ChannelTrimEntry, ResamplerQuality};
use engine::dsp::channel_trim::ChannelTrimmer;
use engine::dsp::convolution::ConvolutionEngine;
use engine::dsp::crossfade::{CrossfadeCurve, TrackMixer};
use engine::dsp::resampler::AudioResamplerF64;

// ─────────────────────────────────────────────────────────────────────────────
// Convolution: partitioned FFT vs naive direct convolution (§25.8)
// ─────────────────────────────────────────────────────────────────────────────

/// Deterministic but non-trivial test signal (not a pure tone, so a single
/// lag-free accidental alignment cannot hide a real mismatch).
fn test_signal(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let x = i as f64;
            (0.7 * (0.031 * x).sin() + 0.3 * (0.013 * x).cos()) * (0.999f64).powf(x)
        })
        .collect()
}

/// Naive direct convolution — the offline golden reference.
fn naive_convolution(x: &[f64], h: &[f64]) -> Vec<f64> {
    let n = x.len() + h.len() - 1;
    let mut y = vec![0.0; n];
    for i in 0..x.len() {
        for j in 0..h.len() {
            y[i + j] += x[i] * h[j];
        }
    }
    y
}

/// Run `x` through the engine (mono IR) and return the collected left output.
fn convolve_mono(x: &[f64], ir: &[f64]) -> Vec<f64> {
    let mut engine = ConvolutionEngine::new(48_000.0, 2048);
    engine
        .load_ir_from_samples_f64(&ir.iter().map(|&v| (v, v)).collect::<Vec<_>>())
        .unwrap();
    engine.set_enabled(true);
    engine.set_wet_mix(1.0);

    let mut out = Vec::with_capacity(x.len() + ir.len() + 4096);
    for &v in x {
        out.push(engine.process_f64(v, v).0);
    }
    // Flush the FIR tail (and then some) so the full convolution is emitted.
    for _ in 0..(ir.len() + 4096) {
        out.push(engine.process_f64(0.0, 0.0).0);
    }
    out
}

#[test]
fn convolution_matches_naive_direct_convolution_mono() {
    let ir: Vec<f64> = (0..100)
        .map(|i| {
            let t = i as f64;
            // Decaying, oscillating IR (a short "room response").
            (0.9f64).powf(t) * (0.05 * t).sin() * 0.8
        })
        .collect();
    // Make the IR non-trivial at DC so a unity-gain check is also meaningful.
    let ir = {
        let mut v = ir;
        v[0] += 0.25;
        v
    };

    let x = test_signal(2048);
    let out = convolve_mono(&x, &ir);
    let y = naive_convolution(&x, &ir);

    // The engine emits `block_size - 1` leading zeros (dry path before the
    // first partition is full), then exactly the direct convolution. Find the
    // alignment by sliding instead of hard-coding the partition size.
    assert!(
        out.len() >= y.len(),
        "engine emitted {} samples, expected at least {}",
        out.len(),
        y.len()
    );
    let mut found = false;
    let mut found_at = None;
    for d in 0..=(out.len() - y.len()) {
        let worst = out[d..d + y.len()]
            .iter()
            .zip(&y)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f64, f64::max);
        if worst < 1e-6 {
            found = true;
            found_at = Some(d);
            break;
        }
    }
    assert!(
        found,
        "partitioned convolution did not reproduce naive direct convolution \
         (worst match found at offset {found_at:?})"
    );
}

#[test]
fn convolution_matches_naive_direct_convolution_stereo() {
    // A genuinely stereo IR: L and R differ, so each channel convolves with
    // its own kernel.
    let ir_l: Vec<f64> = (0..80)
        .map(|i| (0.85f64).powi(i) * (0.04 * i as f64).cos())
        .collect();
    let ir_r: Vec<f64> = (0..80)
        .map(|i| (0.8f64).powi(i) * (0.03 * i as f64).sin())
        .collect();
    let ir: Vec<(f64, f64)> = ir_l.iter().zip(&ir_r).map(|(l, r)| (*l, *r)).collect();

    let mut engine = ConvolutionEngine::new(48_000.0, 2048);
    engine.load_ir_from_samples_f64(&ir).unwrap();
    engine.set_enabled(true);
    engine.set_wet_mix(1.0);

    let x = test_signal(1024);
    let mut out_l = Vec::with_capacity(x.len() + ir.len() + 4096);
    let mut out_r = Vec::with_capacity(x.len() + ir.len() + 4096);
    for &v in &x {
        let (l, r) = engine.process_f64(v, v);
        out_l.push(l);
        out_r.push(r);
    }
    for _ in 0..(ir.len() + 4096) {
        let (l, r) = engine.process_f64(0.0, 0.0);
        out_l.push(l);
        out_r.push(r);
    }

    let y_l = naive_convolution(&x, &ir_l);
    let y_r = naive_convolution(&x, &ir_r);

    let matches = |out: &[f64], y: &[f64]| -> bool {
        if out.len() < y.len() {
            return false;
        }
        (0..=(out.len() - y.len())).any(|d| {
            out[d..d + y.len()]
                .iter()
                .zip(y)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f64, f64::max)
                < 1e-6
        })
    };
    assert!(matches(&out_l, &y_l), "stereo left channel mismatch");
    assert!(matches(&out_r, &y_r), "stereo right channel mismatch");
}

// ─────────────────────────────────────────────────────────────────────────────
// Resampler impulse response (§25.2)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn resampler_impulse_response_is_finite_unity_dc_and_located_at_group_delay() {
    for (fs_in, fs_out) in [
        (44_100.0, 48_000.0),
        (48_000.0, 44_100.0),
        (44_100.0, 96_000.0),
    ] {
        let mut r = AudioResamplerF64::new(ResamplerQuality::HighQuality, fs_in, fs_out)
            .expect("resampler");
        let delay = r.latency_samples() as i64;

        r.feed(1.0, 1.0); // unit impulse
        for _ in 1..((fs_in * 0.5) as usize) {
            r.feed(0.0, 0.0);
        }
        r.flush();
        let mut out = Vec::new();
        while let Some((l, _r)) = r.read() {
            out.push(l);
        }

        assert!(!out.is_empty(), "{fs_in}→{fs_out}: no output");
        assert!(
            out.iter().all(|s| s.is_finite()),
            "{fs_in}→{fs_out}: non-finite impulse response"
        );

        // Unity amplitude gain: the continuous interpolation kernel peaks at
        // 1.0, but for a non-integer rate ratio the output grid samples the
        // kernel off-centre, so the discrete peak lands in [~0.6, 1.0]. The
        // bound below catches gross boost (>1.1) or attenuation (<0.5) while
        // tolerating the worst-case fractional phase.
        let peak_val = out.iter().map(|s| s.abs()).fold(0.0f64, f64::max);
        assert!(
            (0.5..=1.1).contains(&peak_val),
            "{fs_in}→{fs_out}: impulse peak {peak_val:.4} outside [0.5, 1.1]"
        );

        // The impulse-response sum equals the resampling ratio: for a
        // rate-changing polyphase filter the sum scales by fs_out/fs_in
        // (the energy normalization of the interpolating kernel).
        let ratio = fs_out as f64 / fs_in as f64;
        let sum: f64 = out.iter().sum();
        assert!(
            (sum - ratio).abs() < ratio * 0.05,
            "{fs_in}→{fs_out}: impulse sum {sum:.5} != ratio {ratio:.5}"
        );

        // Peak (main lobe) lands at the reported group delay, within a margin
        // that tolerates the filter's transition-band ringing asymmetry.
        let peak = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .map(|(i, _)| i as i64)
            .unwrap();
        assert!(
            (peak - delay).abs() < 128,
            "{fs_in}→{fs_out}: impulse peak at {peak}, reported delay {delay}"
        );

        // The response must decay: energy well past the main lobe is negligible.
        let tail_start = (delay + 256).max(0) as usize;
        if tail_start < out.len() {
            let tail_max = out[tail_start..]
                .iter()
                .map(|s| s.abs())
                .fold(0.0f64, f64::max);
            assert!(
                tail_max < 0.01,
                "{fs_in}→{fs_out}: impulse tail {tail_max:.5} did not decay"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Resampler two-tone co-existence (§25.2 aliasing / §14 multi-tone stress)
// ─────────────────────────────────────────────────────────────────────────────

fn tone_amplitude(samples: &[f64], rate: f64, freq: f64, start: usize, len: usize) -> f64 {
    let end = (start + len).min(samples.len());
    let mut re = 0.0;
    let mut im = 0.0;
    for (k, &x) in samples[start..end].iter().enumerate() {
        let phase = std::f64::consts::TAU * freq * (start + k) as f64 / rate;
        re += x * phase.cos();
        im += x * phase.sin();
    }
    2.0 * (re * re + im * im).sqrt() / (end - start) as f64
}

#[test]
fn resampler_two_tone_in_band_survives_out_of_band_suppressed() {
    // 48 → 44.1 kHz. The 18 kHz tone is in-band; the 23 kHz tone is above the
    // 22.05 kHz output Nyquist and must be rejected (its alias image lands at
    // 44.1 − 23 = 21.1 kHz). Both are present simultaneously, so this also
    // checks that a strong in-band tone does not leak energy into the stopband
    // image.
    let fs_in = 48_000.0f64;
    let fs_out = 44_100.0f64;
    let mut r = AudioResamplerF64::new(ResamplerQuality::Balanced, fs_in as f32, fs_out as f32)
        .expect("resampler");

    let n_in = (fs_in * 4.0) as usize;
    let mut input = Vec::with_capacity(n_in);
    for i in 0..n_in {
        let a = 0.5 * (std::f64::consts::TAU * 18_000.0 * i as f64 / fs_in).sin();
        let b = 0.5 * (std::f64::consts::TAU * 23_000.0 * i as f64 / fs_in).sin();
        r.feed(a + b, a + b);
        input.push(a + b);
    }
    r.flush();
    let mut out = Vec::new();
    while let Some((l, _r)) = r.read() {
        out.push(l);
    }

    // In-band tone survives at unity.
    let a_in = tone_amplitude(&input, fs_in, 18_000.0, 48_000, 32_768);
    let a_out = tone_amplitude(&out, fs_out, 18_000.0, 8_192, 32_768);
    let in_band_db = 20.0 * (a_out / a_in).abs().max(1e-300).log10();
    assert!(
        in_band_db.abs() < 0.5,
        "18 kHz tone gain {in_band_db:.3} dB exceeds ±0.5 dB"
    );

    // The alias image of the 23 kHz tone (at 21.1 kHz) must be deeply
    // suppressed relative to the out-of-band tone's input amplitude.
    let b_in = tone_amplitude(&input, fs_in, 23_000.0, 48_000, 32_768);
    let b_image = tone_amplitude(&out, fs_out, 21_100.0, 8_192, 32_768);
    let suppression_db = -20.0 * (b_image / b_in).abs().max(1e-300).log10();
    assert!(
        suppression_db > 60.0,
        "23 kHz tone alias image suppressed by only {suppression_db:.1} dB (< 60 dB)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Crossfade continuity for every curve (§25.5)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn crossfade_all_curves_are_continuous_and_bounded() {
    for curve in [
        CrossfadeCurve::Linear,
        CrossfadeCurve::EqualPower,
        CrossfadeCurve::Exponential,
        CrossfadeCurve::Logarithmic,
        CrossfadeCurve::SCurve,
    ] {
        let sr = 48_000.0f32;
        let duration = 1000u64;
        let frames = (duration as f32 * sr / 1000.0) as usize;

        // (a) Both sources at unity: output must stay finite and bounded, with
        // no sample-to-sample discontinuity (a gap/step would exceed this by
        // orders of magnitude).
        let mut mixer = TrackMixer::new(sr);
        mixer.set_curve(curve);
        mixer.set_duration_ms(duration, sr);
        mixer.start_crossfade();
        let mut prev = 1.0f32;
        let mut max_jump = 0.0f32;
        let mut max_val = 0.0f32;
        for _ in 0..frames {
            let (l, _r) = mixer.process(1.0, 1.0, 1.0, 1.0);
            assert!(l.is_finite(), "{curve:?}: non-finite output");
            max_jump = max_jump.max((l - prev).abs());
            max_val = max_val.max(l.abs());
            prev = l;
        }
        assert!(
            max_jump < 0.01,
            "{curve:?}: discontinuity {max_jump:.5} exceeds 0.01"
        );
        assert!(max_val <= 1.5, "{curve:?}: output peaked at {max_val:.3}");
        assert!(!mixer.is_crossfading(), "{curve:?}: did not complete");

        // (b) Outgoing-only: the outgoing envelope must be monotonic
        // non-increasing, 1.0 → 0.0.
        let mut mixer = TrackMixer::new(sr);
        mixer.set_curve(curve);
        mixer.set_duration_ms(duration, sr);
        mixer.start_crossfade();
        let mut prev = f32::INFINITY;
        for _ in 0..frames {
            let (l, _r) = mixer.process(1.0, 1.0, 0.0, 0.0);
            assert!(l <= prev + 1e-6, "{curve:?}: outgoing envelope increased");
            prev = l;
        }

        // (c) Incoming-only: the incoming envelope must be monotonic
        // non-decreasing, 0.0 → 1.0.
        let mut mixer = TrackMixer::new(sr);
        mixer.set_curve(curve);
        mixer.set_duration_ms(duration, sr);
        mixer.start_crossfade();
        let mut prev = f32::NEG_INFINITY;
        for _ in 0..frames {
            let (l, _r) = mixer.process(0.0, 0.0, 1.0, 1.0);
            assert!(l >= prev - 1e-6, "{curve:?}: incoming envelope decreased");
            prev = l;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Multichannel golden vectors (§25 / §17: no channel drop or reorder)
// ─────────────────────────────────────────────────────────────────────────────

fn planes(channels: usize, frames: usize, fill: impl Fn(usize, usize) -> f32) -> Vec<Vec<f32>> {
    (0..channels)
        .map(|c| (0..frames).map(|i| fill(c, i)).collect())
        .collect()
}

#[test]
fn multichannel_identity_preserves_every_channel() {
    let mut trim = ChannelTrimmer::new(48_000.0);
    trim.set_config(&ChannelTrimConfig::default(), 48_000.0);
    trim.set_routing(&ChannelRoutingConfig::default());

    // 7.1 layout (8 channels): each channel carries a distinct constant so any
    // drop, reorder, or cross-talk is immediately visible.
    let mut p = planes(8, 16, |c, _i| (c + 1) as f32 * 0.1);
    let before = p.clone();
    trim.process_planes(&mut p, 8, 16);
    assert_eq!(p, before, "identity passthrough altered channel data");
}

#[test]
fn multichannel_permutation_routing_reorders_exactly() {
    let mut trim = ChannelTrimmer::new(48_000.0);
    trim.set_config(&ChannelTrimConfig::default(), 48_000.0);
    // Swap channel 0 and channel 2; channel 1 stays put.
    trim.set_routing(&ChannelRoutingConfig {
        enabled: true,
        matrix: vec![
            vec![0.0, 0.0, 1.0],
            vec![0.0, 1.0, 0.0],
            vec![1.0, 0.0, 0.0],
        ],
    });

    let a = 0.1f32;
    let b = 0.2f32;
    let c = 0.3f32;
    let mut p = vec![vec![a; 4], vec![b; 4], vec![c; 4]];
    trim.process_planes(&mut p, 3, 4);

    for ((x0, x1), x2) in p[0].iter().zip(p[1].iter()).zip(p[2].iter()) {
        assert!((x0 - c).abs() < 1e-6, "out[0] should be C");
        assert!((x1 - b).abs() < 1e-6, "out[1] should be B");
        assert!((x2 - a).abs() < 1e-6, "out[2] should be A");
    }
}

#[test]
fn multichannel_trim_applies_to_every_channel() {
    // A trim entry targets the rearmost channel only; all others stay at
    // unity. This is the per-channel golden vector: one channel active/trimmed
    // at a time, every channel identifiable.
    let mut trim = ChannelTrimmer::new(48_000.0);
    trim.set_config(
        &ChannelTrimConfig {
            enabled: true,
            entries: vec![ChannelTrimEntry {
                channel: 7,
                gain_db: -6.0206, // ≈ 0.5×
                ..Default::default()
            }],
        },
        48_000.0,
    );
    trim.set_routing(&ChannelRoutingConfig::default());

    let mut p = planes(8, 8, |_c, i| (i + 1) as f32);
    trim.process_planes(&mut p, 8, 8);
    for (c, plane) in p.iter().enumerate().take(8) {
        for (i, &val) in plane.iter().enumerate().take(8) {
            let v = (i + 1) as f32;
            let expected = if c == 7 { v * 0.5 } else { v };
            assert!(
                (val - expected).abs() < 1e-3,
                "channel {c}[{i}] = {} != {expected}",
                val
            );
        }
    }
}
