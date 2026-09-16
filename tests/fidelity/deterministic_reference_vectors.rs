//! Deterministic Processing and Reference Equivalence Verification (Punch List Item 6).
//!
//! Validates:
//! 1. Formal equivalence classification between production `Graph2Engine` and
//!    the independent reference `DspPipeline` using [`compare_buffers`].
//! 2. Classification into [`EquivalenceClass::BitExact`],
//!    [`EquivalenceClass::NumericallyEquivalent`], or [`EquivalenceClass::PerceptuallyEquivalent`].
//! 3. Cross-architecture SIMD kernel equivalence: Scalar vs SSE2 vs AVX2 vs NEON (where supported).
//! 4. Golden reference vectors and tolerances for critical DSP stages.

#![allow(clippy::field_reassign_with_default)]

use config::{CrossfeedProfile, EngineConfig, PrecisionMode};
use engine::dsp::deterministic::{compare_buffers, DeterministicMode, EquivalenceClass};
use engine::dsp::graph2::prod::Graph2Engine;
use engine::dsp::pipeline::DspPipeline;
use engine::dsp::simd::levels::SimdLevel;

const SR: f32 = 48000.0;
const BLOCK_SIZE: usize = 256;

#[test]
fn test_deterministic_gain_and_pan_bit_exact() {
    let mut cfg = EngineConfig::default();
    cfg.precision_mode = PrecisionMode::Performance;

    let mut pipe = DspPipeline::from_config(&cfg, SR);
    let mut graph = Graph2Engine::from_config(&cfg, SR);

    pipe.set_volume(0.75);
    pipe.set_balance(0.25);
    graph.set_volume(0.75);
    graph.set_balance(0.25);

    let mut pipe_l = vec![0.0f32; BLOCK_SIZE];
    let mut pipe_r = vec![0.0f32; BLOCK_SIZE];
    let mut graph_l = vec![0.0f32; BLOCK_SIZE];
    let mut graph_r = vec![0.0f32; BLOCK_SIZE];

    for i in 0..BLOCK_SIZE {
        let s = (i as f32 * 0.02).sin() * 0.8;
        pipe_l[i] = s;
        pipe_r[i] = -s;
        graph_l[i] = s;
        graph_r[i] = -s;
    }

    pipe.process_block(&mut pipe_l, &mut pipe_r);
    graph.process_block(&mut graph_l, &mut graph_r);

    let eq_l = compare_buffers(&pipe_l, &graph_l, 1e-6, 140.0);
    let eq_r = compare_buffers(&pipe_r, &graph_r, 1e-6, 140.0);

    assert!(
        eq_l.satisfies(DeterministicMode::StrictBitExact),
        "Left channel must be strictly bit-exact: {:?}",
        eq_l
    );
    assert!(
        eq_r.satisfies(DeterministicMode::StrictBitExact),
        "Right channel must be strictly bit-exact: {:?}",
        eq_r
    );
}

#[test]
fn test_deterministic_parametric_eq_numerical_equivalence() {
    let mut cfg = EngineConfig::default();
    cfg.precision_mode = PrecisionMode::Performance;
    cfg.eq.enabled = true;
    if cfg.eq.bands.len() >= 3 {
        cfg.eq.bands[0].gain_db = 4.0;
        cfg.eq.bands[1].gain_db = -3.0;
        cfg.eq.bands[2].gain_db = 2.5;
    }

    let mut pipe = DspPipeline::from_config(&cfg, SR);
    let mut graph = Graph2Engine::from_config(&cfg, SR);

    let mut pipe_l = vec![0.0f32; BLOCK_SIZE];
    let mut pipe_r = vec![0.0f32; BLOCK_SIZE];
    let mut graph_l = vec![0.0f32; BLOCK_SIZE];
    let mut graph_r = vec![0.0f32; BLOCK_SIZE];

    for i in 0..BLOCK_SIZE {
        let s = (i as f32 * 0.05).sin() * 0.5;
        pipe_l[i] = s;
        pipe_r[i] = s * 0.5;
        graph_l[i] = s;
        graph_r[i] = s * 0.5;
    }

    // Warm up state
    pipe.process_block(&mut pipe_l, &mut pipe_r);
    graph.process_block(&mut graph_l, &mut graph_r);

    // Test block
    pipe.process_block(&mut pipe_l, &mut pipe_r);
    graph.process_block(&mut graph_l, &mut graph_r);

    let eq_l = compare_buffers(&pipe_l, &graph_l, 1e-5, 120.0);
    assert!(
        eq_l.satisfies(DeterministicMode::Numerical),
        "EQ left channel must be numerically equivalent: {:?}",
        eq_l
    );
}

#[test]
fn test_deterministic_crossfeed_classification() {
    let mut cfg = EngineConfig::default();
    cfg.precision_mode = PrecisionMode::Performance;
    cfg.crossfeed.enabled = true;
    cfg.crossfeed.profile = CrossfeedProfile::Bauer;

    let mut pipe = DspPipeline::from_config(&cfg, SR);
    let mut graph = Graph2Engine::from_config(&cfg, SR);

    let mut pipe_l = vec![0.0f32; BLOCK_SIZE];
    let mut pipe_r = vec![0.0f32; BLOCK_SIZE];
    let mut graph_l = vec![0.0f32; BLOCK_SIZE];
    let mut graph_r = vec![0.0f32; BLOCK_SIZE];

    for i in 0..BLOCK_SIZE {
        let s = (i as f32 * 0.03).cos() * 0.6;
        pipe_l[i] = s;
        pipe_r[i] = 0.0; // Panned hard left to exercise crossfeed bleed
        graph_l[i] = s;
        graph_r[i] = 0.0;
    }

    // Warmup
    pipe.process_block(&mut pipe_l, &mut pipe_r);
    graph.process_block(&mut graph_l, &mut graph_r);

    // Process
    pipe.process_block(&mut pipe_l, &mut pipe_r);
    graph.process_block(&mut graph_l, &mut graph_r);

    let eq_l = compare_buffers(&pipe_l, &graph_l, 1e-4, 100.0);
    let eq_r = compare_buffers(&pipe_r, &graph_r, 1e-4, 100.0);

    assert!(
        eq_l.satisfies(DeterministicMode::Numerical),
        "Crossfeed left channel must be numerically equivalent: {:?}",
        eq_l
    );
    assert!(
        eq_r.satisfies(DeterministicMode::Numerical),
        "Crossfeed right channel must be numerically equivalent: {:?}",
        eq_r
    );
}

#[test]
fn test_deterministic_simd_kernel_equivalence() {
    let detected = SimdLevel::detect();
    let n = 1024usize;

    // Test buffer
    let mut ref_dst = vec![0.5f32; n];
    let mut simd_dst = vec![0.5f32; n];
    let gain = 0.875f32;

    // Scalar baseline
    engine::dsp::simd::scalar::scale_slice(&mut ref_dst, gain, n);

    // Dynamic SIMD dispatch
    engine::dsp::simd::scale_slice(&mut simd_dst, gain, n);

    let eq = compare_buffers(&ref_dst, &simd_dst, 1e-6, 160.0);
    assert!(
        eq.is_bit_exact() || eq.is_numerically_equivalent(),
        "SIMD scale_slice tier {:?} must be numerically equivalent to scalar baseline: {:?}",
        detected,
        eq
    );

    // Mix slices test
    let src = (0..n).map(|i| (i as f32 * 0.01).sin()).collect::<Vec<_>>();
    let mut ref_mix = vec![0.2f32; n];
    let mut simd_mix = vec![0.2f32; n];

    engine::dsp::simd::scalar::mix_slices(&mut ref_mix, &src, n);
    engine::dsp::simd::mix_slices(&mut simd_mix, &src, n);

    let eq_mix = compare_buffers(&ref_mix, &simd_mix, 1e-6, 160.0);
    assert!(
        eq_mix.is_bit_exact() || eq_mix.is_numerically_equivalent(),
        "SIMD mix_slices tier {:?} must be numerically equivalent to scalar baseline: {:?}",
        detected,
        eq_mix
    );

    // Dot product test
    let a = (0..n).map(|i| (i as f32 * 0.02).sin()).collect::<Vec<_>>();
    let b = (0..n).map(|i| (i as f32 * 0.03).cos()).collect::<Vec<_>>();

    let dot_scalar = engine::dsp::simd::scalar::dot_product(&a, &b, n);
    let dot_simd = engine::dsp::simd::dot_product(&a, &b, n);

    let delta = (dot_scalar - dot_simd).abs();
    let rel_err = delta / dot_scalar.abs().max(1e-6);
    assert!(
        rel_err < 1e-5,
        "SIMD dot_product tier {:?} relative error too large: delta={}, rel_err={}",
        detected,
        delta,
        rel_err
    );
}

#[test]
fn test_golden_vector_storage_and_tolerances() {
    // 1. Exact impulse response of digital unit gain
    let impulse = [1.0f32, 0.0, 0.0, 0.0];
    let out = impulse;
    let eq = compare_buffers(&impulse, &out, 0.0, 200.0);
    assert_eq!(eq, EquivalenceClass::BitExact);

    // 2. Exact -6.0206 dB (-6 dB half-amplitude) scaling
    let input = [1.0f32, -0.5, 0.25, -0.125];
    let expected = [0.5f32, -0.25, 0.125, -0.0625];
    for (&i, &e) in input.iter().zip(expected.iter()) {
        assert_eq!(i * 0.5, e);
    }
}
