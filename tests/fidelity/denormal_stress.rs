//! Denormal Stress Tests and Stall Immunity Suite (§8.3, Punch List Item 29).
//!
//! Validates:
//! 1. CPU behavior during reverb tails decaying into subnormal float ranges.
//! 2. Compressor envelope release decay into subnormal near-zero floors.
//! 3. High-Q resonant IIR biquad filter ringing and decay down to zero.
//! 4. Continuous streaming of subnormal floats ($10^{-38} \dots 10^{-45}$) without CPU stalls.
//! 5. Extended silence transitions with clean flush-to-zero / DAZ enforcement.

use std::time::Instant;

use config::EngineConfig;
use engine::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};
use engine::dsp::pipeline::DspPipeline;
use engine::dsp::safety::FloatSafetyMode;
use engine::dsp::MultibandCompressor;

#[test]
fn test_denormal_reverb_tail_decay_no_stall() {
    FloatSafetyMode::FlushToZero.apply_to_current_thread();

    let sr = 48000.0f32;
    let block_size = 256;
    let mut cfg = EngineConfig::default();
    cfg.convolution.enabled = true;
    cfg.limiter.enabled = true;

    let mut pipeline = DspPipeline::from_config(&cfg, sr);

    let mut left = vec![0.0f32; block_size];
    let mut right = vec![0.0f32; block_size];

    // Measure baseline with normal audio
    let normal_blocks = 500;
    let t0 = Instant::now();
    for i in 0..normal_blocks {
        let s = (i as f32 * 0.05).sin() * 0.4;
        left.fill(s);
        right.fill(s * 0.8);
        pipeline.process_block(&mut left, &mut right);
    }
    let normal_duration = t0.elapsed();

    // Now inject impulse followed by exponential decay into subnormal floor
    let mut impulse_decay_left = vec![0.0f32; block_size];
    let mut impulse_decay_right = vec![0.0f32; block_size];
    impulse_decay_left[0] = 1.0;
    impulse_decay_right[0] = 1.0;

    let decay_blocks = 1500;
    let t1 = Instant::now();
    for block in 0..decay_blocks {
        // Exponential decay envelope from 1.0 down to subnormals (1e-40)
        let decay_factor = (-0.02 * block as f32).exp();
        for i in 0..block_size {
            impulse_decay_left[i] *= decay_factor;
            impulse_decay_right[i] *= decay_factor;
        }

        pipeline.process_block(&mut impulse_decay_left, &mut impulse_decay_right);

        for &s in impulse_decay_left.iter().chain(impulse_decay_right.iter()) {
            assert!(s.is_finite(), "Reverb tail produced non-finite sample");
        }
    }
    let decay_duration = t1.elapsed();

    let normal_time_per_block = normal_duration.as_secs_f64() / normal_blocks as f64;
    let decay_time_per_block = decay_duration.as_secs_f64() / decay_blocks as f64;
    let ratio = decay_time_per_block / normal_time_per_block.max(1e-6);

    assert!(
        ratio < 3.0,
        "Reverb tail decay caused CPU stall! Slowdown ratio: {:.2}x (normal: {:.2} µs/blk, decay: {:.2} µs/blk)",
        ratio,
        normal_time_per_block * 1e6,
        decay_time_per_block * 1e6
    );
}

#[test]
fn test_denormal_compressor_release_decay_no_stall() {
    FloatSafetyMode::FlushToZero.apply_to_current_thread();

    let sr = 48000.0f32;
    let mut comp = MultibandCompressor::new(sr);
    comp.set_enabled(true);

    let block_size = 128;
    let mut left = vec![0.0f32; block_size];
    let mut right = vec![0.0f32; block_size];

    // Excite compressor with heavy 0 dBFS signal to engage gain reduction
    for _ in 0..100 {
        left.fill(0.95);
        right.fill(-0.95);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let (out_l, out_r) = comp.process(*l, *r);
            *l = out_l;
            *r = out_r;
        }
    }

    // Measure release time decay into subnormal floor
    let release_blocks = 1000;
    let t0 = Instant::now();
    for _ in 0..release_blocks {
        // Feed subnormal release floor: 1e-39
        left.fill(1e-39);
        right.fill(-1e-39);
        comp.process_block(&mut left, &mut right);
        for &s in left.iter().chain(right.iter()) {
            assert!(s.is_finite());
        }
    }
    let elapsed = t0.elapsed();

    // After 1000 blocks of release decay, compressor and filters must be down to floor
    for &s in left.iter().chain(right.iter()) {
        assert!(
            s.abs() < 1e-4,
            "Compressor failed to decay to near-silence after release: {}",
            s
        );
    }

    let avg_us = (elapsed.as_secs_f64() * 1e6) / release_blocks as f64;
    let budget_us = (block_size as f64 / sr as f64) * 1e6; // 2666.7 µs

    assert!(
        avg_us < budget_us * 0.25,
        "Compressor release decay took too long ({:.2} µs, budget: {:.2} µs)",
        avg_us,
        budget_us
    );
}

#[test]
fn test_denormal_biquad_resonant_decay_no_stall() {
    FloatSafetyMode::FlushToZero.apply_to_current_thread();

    let sr = 48000.0f32;
    // High-Q resonant peaking filter (rings for tens of thousands of samples)
    let coeffs = BiquadCoeffsF32::peaking(sr, 1000.0, 12.0, 25.0);
    let mut state = BiquadStateF32::default();

    // Initial impulse
    let mut s = state.process(1.0, &coeffs);
    assert!(s.is_finite());

    let frames = 20_000;
    let t0 = Instant::now();
    for _ in 0..frames {
        s = state.process(0.0, &coeffs);
        assert!(
            s.is_finite(),
            "Biquad filter produced non-finite sample during ringing"
        );
    }
    let elapsed = t0.elapsed();

    let ns_per_sample = (elapsed.as_secs_f64() * 1e9) / frames as f64;

    // Filter decay must execute in < 250 ns/sample with zero microcode exception stalls
    assert!(
        ns_per_sample < 250.0,
        "Biquad decay suffered denormal stall: {:.2} ns/sample",
        ns_per_sample
    );

    // Filter output must have decayed cleanly
    assert!(s.abs() < 1e-5, "Filter state did not decay cleanly: {}", s);
}

#[test]
fn test_denormal_near_silence_continuous_stream() {
    FloatSafetyMode::FlushToZero.apply_to_current_thread();

    let sr = 48000.0f32;
    let block_size = 256;
    let cfg = EngineConfig::default();
    let mut pipeline = DspPipeline::from_config(&cfg, sr);

    let mut left = vec![1e-42f32; block_size];
    let mut right = vec![-1e-42f32; block_size];

    let blocks = 1000;
    let t0 = Instant::now();
    for _ in 0..blocks {
        pipeline.process_block(&mut left, &mut right);
        for &s in left.iter().chain(right.iter()) {
            assert!(s.is_finite());
            assert!(
                s.abs() < 1e-4,
                "Near-silence subnormal numbers must remain at or below floor: {}",
                s
            );
        }
    }
    let elapsed = t0.elapsed();

    let us_per_block = (elapsed.as_secs_f64() * 1e6) / blocks as f64;
    let budget_us = (block_size as f64 / sr as f64) * 1e6;

    assert!(
        us_per_block < budget_us * 0.15,
        "Near-silence streaming exceeded budget fraction: {:.2} µs",
        us_per_block
    );
}

#[test]
fn test_denormal_extended_silence_clean_decay() {
    FloatSafetyMode::FlushToZero.apply_to_current_thread();

    let sr = 48000.0f32;
    let block_size = 256;
    let cfg = EngineConfig::default();
    let mut pipeline = DspPipeline::from_config(&cfg, sr);

    let mut left = vec![0.8f32; block_size];
    let mut right = vec![0.8f32; block_size];

    // Warm up with signal
    for _ in 0..50 {
        pipeline.process_block(&mut left, &mut right);
    }

    // Now transition to extended digital silence (0.0)
    left.fill(0.0);
    right.fill(0.0);

    for block in 0..500 {
        pipeline.process_block(&mut left, &mut right);
        for &s in left.iter().chain(right.iter()) {
            assert!(
                s.is_finite(),
                "Output was non-finite during silence at block {}",
                block
            );
        }
    }

    // Final block must be completely silent
    for &s in left.iter().chain(right.iter()) {
        assert_eq!(s, 0.0, "Expected pure 0.0 after extended silence");
    }
}
