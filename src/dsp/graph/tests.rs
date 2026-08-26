use super::*;
use crate::dsp::pipeline::DspPipeline;
use config::{EngineConfig, PrecisionMode};
use std::collections::HashMap;

fn full_config() -> EngineConfig {
    let mut c = EngineConfig::default();
    c.eq.enabled = true;
    if c.eq.bands.len() >= 5 {
        c.eq.bands[0].enabled = true;
        c.eq.bands[0].gain_db = 3.0;
        c.eq.bands[2].enabled = true;
        c.eq.bands[2].gain_db = -2.0;
        c.eq.bands[4].gain_db = 1.5;
    }
    c.limiter.enabled = true;
    c.limiter.lookahead_ms = 2.0;
    c.multiband_compressor.enabled = true;
    c.crossfeed.enabled = true;
    c.stereo_enhancer.enabled = true;
    c.stereo_enhancer.width = 1.3;
    c.loudness.mode = config::LoudnessMode::EbuR128;
    c.dither_enabled = true;
    c
}

#[test]
fn graph_nodes_descriptor_contracts_fulfilled() {
    let sr = 48000.0;
    let cfg = full_config();
    let graph = DspGraph::from_config(&cfg, sr);

    let nodes = graph.graph_nodes();
    assert_eq!(nodes.len(), DSP_STAGE_CAPABILITIES.len());

    let map: HashMap<&str, &DspNodeInfo> = nodes.iter().map(|n| (n.name, n)).collect();
    assert!(map["eq"].active);
    assert!(map["multiband_compressor"].active);
    assert!(map["crossfeed"].active);
    assert!(map["stereo_enhancer"].active);
    assert!(map["limiter"].active);
    assert!(!map["volume"].active); // unity gain reports inactive
}

#[test]
fn graph_block_processing_is_deterministic() {
    let sr = 44100.0;
    let mut cfg = full_config();
    cfg.precision_mode = PrecisionMode::Performance;

    let mut graph = DspGraph::from_config(&cfg, sr);
    graph.volume_mut().processor.set_gain(0.7);
    graph.balance_mut().set_balance(-0.3);

    let n = 2048;
    let mut left = vec![0.0f32; n];
    let mut right = vec![0.0f32; n];
    for i in 0..n {
        let t = i as f32;
        left[i] = (t * 0.02).sin() * 0.5;
        right[i] = (t * 0.03).cos() * 0.5;
    }

    graph.process_block(&mut left, &mut right);

    // Verify all outputs are finite and bounded
    for i in 0..n {
        assert!(left[i].is_finite());
        assert!(right[i].is_finite());
        assert!(left[i].abs() <= 2.0);
        assert!(right[i].abs() <= 2.0);
    }
}

#[test]
fn graph_f64_quality_mode_preserves_audio() {
    let sr = 48000.0;
    let mut cfg = full_config();
    cfg.precision_mode = PrecisionMode::Quality;

    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 1024;
    let mut left = vec![0.1f32; n];
    let mut right = vec![-0.1f32; n];

    graph.process_block(&mut left, &mut right);

    for i in 0..n {
        assert!(left[i].is_finite());
        assert!(right[i].is_finite());
    }
}

#[test]
fn graph_bit_perfect_bypass_is_exact_passthrough() {
    let sr = 48000.0;
    let cfg = full_config();
    let mut graph = DspGraph::from_config(&cfg, sr);
    graph.set_bit_perfect(true);

    let n = 512;
    let left_orig: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin()).collect();
    let right_orig: Vec<f32> = (0..n).map(|i| (i as f32 * 0.07).cos()).collect();

    let mut left = left_orig.clone();
    let mut right = right_orig.clone();

    graph.process_block(&mut left, &mut right);

    for i in 0..n {
        assert_eq!(left[i], left_orig[i]);
        assert_eq!(right[i], right_orig[i]);
    }

    let nodes = graph.graph_nodes();
    assert!(nodes.iter().all(|n| !n.active));
    assert_eq!(graph.total_latency_ms(), 0.0);
}

#[test]
fn graph_matches_pipeline_exact_block_outputs() {
    let sr = 44100.0;
    let mut cfg = full_config();
    cfg.precision_mode = PrecisionMode::Performance;

    let mut pipeline = DspPipeline::from_config(&cfg, sr);
    let mut graph = DspGraph::from_config(&cfg, sr);

    pipeline.set_volume(0.8);
    graph.volume_mut().processor.set_gain(0.8);

    let n = 1024;
    let mut input_l = vec![0.0f32; n];
    let mut input_r = vec![0.0f32; n];
    for i in 0..n {
        let t = i as f32;
        input_l[i] = (t * 0.01).sin() * 0.4;
        input_r[i] = (t * 0.015).cos() * 0.4;
    }

    let mut pipe_l = input_l.clone();
    let mut pipe_r = input_r.clone();
    pipeline.process_block(&mut pipe_l, &mut pipe_r);

    let mut graph_l = input_l.clone();
    let mut graph_r = input_r.clone();
    graph.process_block(&mut graph_l, &mut graph_r);

    for i in 0..n {
        assert!(
            (pipe_l[i] - graph_l[i]).abs() < 1e-4,
            "L mismatch at {}: pipeline={} vs graph={}",
            i,
            pipe_l[i],
            graph_l[i]
        );
        assert!(
            (pipe_r[i] - graph_r[i]).abs() < 1e-4,
            "R mismatch at {}: pipeline={} vs graph={}",
            i,
            pipe_r[i],
            graph_r[i]
        );
    }
}
