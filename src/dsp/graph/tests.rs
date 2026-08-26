use super::*;
use crate::decode::ChannelLayout;
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

// ── Phase 2: queued control surface + live generation swap ─────────────────

#[test]
fn graph_control_commands_defer_until_next_block() {
    // Enqueuing must NOT mutate the active node: control crosses the thread
    // boundary only at the block-boundary tick.
    let sr = 48000.0;
    let cfg = full_config();
    let mut graph = DspGraph::from_config(&cfg, sr);
    let handle = graph.control_handle();

    let before = graph.volume_db();
    handle.set_volume_db(-6.0);
    assert_eq!(
        graph.volume_db(),
        before,
        "command must be deferred, not applied"
    );

    let n = 256;
    let mut l = vec![0.5f32; n];
    let mut r = vec![0.5f32; n];
    graph.process_block(&mut l, &mut r);

    let after = graph.volume_db();
    assert!(
        after < before,
        "volume must start ramping toward -6 dB after the tick (before={}, after={})",
        before,
        after
    );
    assert!(
        after > -6.5,
        "still mid-ramp after one small block: {}",
        after
    );
}

#[test]
fn graph_swap_installs_new_generation_at_block_boundary() {
    let sr = 48000.0;
    let cfg = full_config();
    let mut graph = DspGraph::from_config(&cfg, sr);
    let handle = graph.control_handle();
    assert_eq!(handle.generation(), 0);

    // Build a modified generation: limiter off (removes its latency + tail).
    let mut cfg2 = full_config();
    cfg2.limiter.enabled = false;

    let mut l = vec![0.3f32; 512];
    let mut r = vec![-0.3f32; 512];
    graph.process_block(&mut l, &mut r);
    assert_eq!(handle.generation(), 0, "no swap without a publish");
    assert!(
        graph
            .graph_nodes()
            .iter()
            .find(|n| n.name == "limiter")
            .unwrap()
            .active
    );

    // Publish from the control side; the audio thread swaps at the next block.
    let gen = GraphGeneration::from_config(&cfg2, sr, &ChannelLayout::Stereo);
    handle.publish_generation(gen);
    assert_eq!(handle.generation(), 0, "publish alone must not swap");

    graph.process_block(&mut l, &mut r);
    assert_eq!(
        handle.generation(),
        1,
        "swap performed at the block boundary"
    );
    assert!(
        !graph
            .graph_nodes()
            .iter()
            .find(|n| n.name == "limiter")
            .unwrap()
            .active,
        "new generation's config is live after the swap"
    );

    // The swapped-out generation sits in `retired` until the control side
    // reclaims it on its next publish.
    let gen = GraphGeneration::from_config(&cfg2, sr, &ChannelLayout::Stereo);
    handle.publish_generation(gen);
    assert_eq!(handle.reclaimed_count(), 1, "control side reclaimed gen0");
    graph.process_block(&mut l, &mut r);
    assert_eq!(handle.generation(), 2);

    let gen = GraphGeneration::from_config(&cfg2, sr, &ChannelLayout::Stereo);
    handle.publish_generation(gen);
    assert_eq!(
        handle.reclaimed_count(),
        2,
        "reclamation is exactly once per swap"
    );
}

#[test]
fn graph_publish_coalesces_pending_generations() {
    let sr = 48000.0;
    let cfg = full_config();
    let mut graph = DspGraph::from_config(&cfg, sr);
    let handle = graph.control_handle();

    // Two publishes with no block in between: the second replaces the first
    // while it is still pending, so exactly ONE generation survives to the
    // next swap.
    let gen_a = GraphGeneration::from_config(&cfg, sr, &ChannelLayout::Stereo);
    handle.publish_generation(gen_a);
    let gen_b = GraphGeneration::from_config(&cfg, sr, &ChannelLayout::Stereo);
    handle.publish_generation(gen_b);

    assert_eq!(handle.generation(), 0, "still nothing swapped");

    let mut l = vec![0.3f32; 256];
    let mut r = vec![0.3f32; 256];
    graph.process_block(&mut l, &mut r);
    assert_eq!(
        handle.generation(),
        1,
        "two publishes coalesce into one swap"
    );
    assert_eq!(handle.reclaimed_count(), 0);

    // The coalesced-away generation was dropped at publish time; the retired
    // one is reclaimed on the next publish.
    let gen_c = GraphGeneration::from_config(&cfg, sr, &ChannelLayout::Stereo);
    handle.publish_generation(gen_c);
    assert_eq!(handle.reclaimed_count(), 1);
}

#[test]
fn graph_user_state_survives_generation_swap() {
    // Volume is user state: a fresh generation inherits the drained volume
    // target from the control bus, so a reconfig does not snap the listener.
    let sr = 48000.0;
    let cfg = full_config();
    let mut graph = DspGraph::from_config(&cfg, sr);
    let handle = graph.control_handle();

    handle.set_volume_db(-9.0);
    let mut l = vec![0.5f32; 4096];
    let mut r = vec![0.5f32; 4096];
    graph.process_block(&mut l, &mut r); // drain + settle the ramp

    let settled = graph.volume_db();
    assert!(
        (settled + 9.0).abs() < 0.5,
        "volume settled near -9 dB: {}",
        settled
    );

    // The fresh generation must inherit the DRAINED volume: build it from a
    // live snapshot of the control bus, exactly as `reconfigure` does.
    let gen =
        GraphGeneration::build_with_state(&cfg, sr, &ChannelLayout::Stereo, graph.bus.snapshot());
    handle.publish_generation(gen);
    graph.process_block(&mut l, &mut r); // swap
    graph.process_block(&mut l, &mut r); // settle the fresh ramp

    let after = graph.volume_db();
    assert!(
        (after + 9.0).abs() < 0.5,
        "new generation inherits user volume (before={}, after={})",
        settled,
        after
    );
}

#[test]
fn graph_two_thread_control_and_audio_stress() {
    // A control thread publishes generations and enqueues volume commands
    // while the audio thread processes blocks. Assertions: no dropped
    // commands (bounded queues absorb the burst), every published generation
    // was received, swaps happened, and output stays finite.
    let sr = 44100.0;
    let cfg = full_config();
    let mut graph = DspGraph::from_config(&cfg, sr);
    let handle = graph.control_handle();

    const N_GENS: usize = 16;
    const N_VOL_CMDS: usize = 40;

    // Pre-build generations on the control side (allocation is legal there).
    let mut gens = Vec::new();
    for i in 0..N_GENS {
        let mut c = full_config();
        c.limiter.enabled = i % 2 == 0;
        c.eq.bands[2].gain_db = i as f32 * 0.5;
        gens.push(GraphGeneration::from_config(&c, sr, &ChannelLayout::Stereo));
    }

    let (tx, rx) = std::sync::mpsc::channel::<Box<GraphGeneration>>();
    let ctl_handle = handle.clone();
    let ctl = std::thread::spawn(move || {
        let mut received = 0usize;
        while let Ok(gen) = rx.recv() {
            ctl_handle.publish_generation(gen);
            received += 1;
        }
        for i in 0..N_VOL_CMDS {
            ctl_handle.set_volume_db(-((i % 10) as f32 + 1.0));
        }
        received
    });

    // Audio side: process while the control thread publishes.
    let mut l = vec![0.2f32; 512];
    let mut r = vec![-0.2f32; 512];
    for _ in 0..64 {
        graph.process_block(&mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
    }
    for gen in gens {
        tx.send(gen).unwrap();
    }
    drop(tx);

    let received = ctl.join().unwrap();
    assert_eq!(received, N_GENS, "control thread saw every generation");

    // Flush any in-flight publish, then assert the handshake discipline.
    for _ in 0..8 {
        graph.process_block(&mut l, &mut r);
    }
    assert!(
        handle.generation() >= 1,
        "at least one swap under contention"
    );
    assert_eq!(
        handle.dropped_commands(),
        0,
        "64-deep per-node queues must absorb this burst"
    );
    assert!(
        handle.reclaimed_count() + handle.generation() <= N_GENS as u64 + 1,
        "reclamation never exceeds published generations"
    );
}
