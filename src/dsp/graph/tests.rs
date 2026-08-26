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

// ── Phase 3 S1: mix-bus per-input control + transition envelope ────────────

#[test]
fn mix_input_gain_balance_mute_apply_via_control_queue() {
    use crate::dsp::crossfade::MixerState;

    let sr = 48000.0;
    let mut cfg = EngineConfig::default();
    cfg.crossfade.enabled = false; // begin_crossfade degrades to a gapless switch
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;

    // Mute the primary input: PlayingCurrent must output silence.
    graph.set_input_mute(0, true);
    let mut l0 = vec![0.5f32; n];
    let mut r0 = vec![-0.5f32; n];
    let mut l1 = vec![0.0f32; n];
    let mut r1 = vec![0.0f32; n];
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    assert_eq!(l0[0], 0.0, "muted input 0 must contribute silence");
    assert_eq!(r0[0], 0.0);

    // Per-input gain + balance on input 1, then a gapless switch to it.
    graph.set_input_mute(0, false);
    graph.set_input_gain(1, 0.5);
    graph.set_input_balance(1, 0.5);
    graph.begin_crossfade(60);
    let mut l0 = vec![0.8f32; n];
    let mut r0 = vec![0.8f32; n];
    let mut l1 = vec![0.4f32; n];
    let mut r1 = vec![0.4f32; n];
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    // The user-gain ramp is exponential; snap it so the assertion is exact
    // (ramp timing itself is covered by the gain unit tests).
    graph.mix_mut().inputs[1].gain.snap();
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    assert_eq!(graph.mixer_state(), MixerState::PlayingNext);
    // L = in1 * gain * (1 - balance) = 0.4 * 0.5 * 0.5 = 0.1;
    // R = in1 * gain * 1.0 = 0.4 * 0.5 = 0.2. Input 0 must not leak.
    for i in 0..n {
        assert!(
            (l0[i] - 0.1).abs() < 1e-6,
            "L mismatch at {i}: {} (want 0.1)",
            l0[i]
        );
        assert!(
            (r0[i] - 0.2).abs() < 1e-6,
            "R mismatch at {i}: {} (want 0.2)",
            r0[i]
        );
    }
}

#[test]
fn mix_transitions_drive_envelope_states() {
    use crate::dsp::crossfade::MixerState;

    let sr = 48000.0;
    let mut cfg = EngineConfig::default();
    cfg.crossfade.enabled = true;
    cfg.crossfade.duration_ms = 50; // 2400 frames at 48 kHz
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;

    let mut l0 = vec![0.5f32; n];
    let mut r0 = vec![0.5f32; n];
    let mut l1 = vec![0.0f32; n];
    let mut r1 = vec![0.0f32; n];

    assert_eq!(graph.mixer_state(), MixerState::PlayingCurrent);

    graph.begin_crossfade(50);
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    assert_eq!(graph.mixer_state(), MixerState::Crossfading);

    // 2400 frames ≈ 10 blocks of 256 — the envelope completes and the bus
    // latches onto input 1.
    for _ in 0..10 {
        graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    }
    assert_eq!(graph.mixer_state(), MixerState::PlayingNext);

    // Back to single-stream playback.
    graph.begin_playing();
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    assert_eq!(graph.mixer_state(), MixerState::PlayingCurrent);

    // Sequential fade (fade-out → gap → fade-in) completes the same way.
    graph.begin_fade(50);
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    assert_eq!(graph.mixer_state(), MixerState::Fading);
    for _ in 0..10 {
        graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    }
    assert_eq!(graph.mixer_state(), MixerState::PlayingNext);
}

#[test]
fn mix_loudness_metadata_routes_to_inputs() {
    use crate::dsp::loudness::LoudnessMetadata;

    let sr = 48000.0;
    let mut cfg = EngineConfig::default();
    cfg.loudness.mode = config::LoudnessMode::EbuR128;
    cfg.loudness.target_lufs = -16.0;
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;

    graph.apply_loudness_metadata_outgoing(Some(LoudnessMetadata {
        ebu_r128_loudness: Some(-18.0),
        ..Default::default()
    }));
    graph.apply_loudness_metadata_incoming(Some(LoudnessMetadata {
        ebu_r128_loudness: Some(-22.0),
        ..Default::default()
    }));
    let mut l0 = vec![0.1f32; n];
    let mut r0 = vec![0.1f32; n];
    let mut l1 = vec![0.1f32; n];
    let mut r1 = vec![0.1f32; n];
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));

    // Distinct metadata per input must land on distinct chains (outgoing →
    // input 0, incoming → input 1) and normalize independently. The gains
    // are mid-ramp after one block, so only routing (non-identity) is
    // asserted here.
    let g0 = graph.mix().inputs[0].loudness.normalizer.current_gain_db();
    let g1 = graph.mix().inputs[1].loudness.normalizer.current_gain_db();
    assert!(
        (g0 - g1).abs() > 1e-3,
        "inputs must normalize independently: {g0} vs {g1}"
    );
}

// ── Phase 3 S2: stream slots (N-input bus + multi-stream entry) ────────────

#[test]
fn mix_stream_slots_sum_independently_and_detach() {
    use crate::dsp::crossfade::MixerState;

    let sr = 48000.0;
    let mut cfg = EngineConfig::default();
    cfg.crossfade.enabled = false; // begin_crossfade degrades to a gapless switch
                                   // Phase 4 S1: the slot count is a generation parameter — a 3-slot bus.
    cfg.mix_slots = 3;
    let mut graph = DspGraph::from_config(&cfg, sr);
    assert_eq!(graph.mix().inputs.len(), 3, "mix_slots must size the bus");

    graph.mix_mut().inputs[2].gain.set_gain(0.5);
    graph.mix_mut().inputs[2].gain.snap();

    let n = 256;
    let mut l0 = vec![0.5f32; n];
    let mut r0 = vec![0.5f32; n];
    let mut l1 = vec![0.25f32; n];
    let mut r1 = vec![0.25f32; n];
    let mut l2 = vec![0.125f32; n];
    let mut r2 = vec![0.125f32; n];

    // PlayingCurrent: input 0 at unity + slot 2 at its own gain (slot 1 is
    // gated by the pair envelope). out = 0.5 + 0.125*0.5 = 0.5625.
    graph.process_block_streams(
        (&mut l0, &mut r0),
        &mut [(&mut l1, &mut r1), (&mut l2, &mut r2)],
    );
    assert_eq!(graph.mixer_state(), MixerState::PlayingCurrent);
    for (i, v) in l0.iter().enumerate() {
        assert!(
            (v - 0.5625).abs() < 1e-6,
            "3-input sum mismatch at {i}: {v} (want 0.5625)"
        );
    }

    // Detach slot 2: it must vanish from the sum (and its gain must not
    // advance further — it stays at 0.5). Refill the source buffers first:
    // the primary buffer now carries the previous block's mixed output.
    l0.fill(0.5);
    r0.fill(0.5);
    l1.fill(0.25);
    r1.fill(0.25);
    l2.fill(0.125);
    r2.fill(0.125);
    graph.set_input_active(2, false);
    graph.process_block_streams(
        (&mut l0, &mut r0),
        &mut [(&mut l1, &mut r1), (&mut l2, &mut r2)],
    );
    for (i, v) in l0.iter().enumerate() {
        assert!(
            (v - 0.5).abs() < 1e-6,
            "detached slot leaked into the sum at {i}: {v} (want 0.5)"
        );
    }
    assert!(
        (graph.mix().inputs[2].gain.gain - 0.5).abs() < 1e-6,
        "detached slot must not advance its gain ramp"
    );

    // Gapless switch to the incoming stream: slot 1 at unity + slot 2 back
    // at 0.5 → out = 0.25 + 0.0625 = 0.3125.
    l0.fill(0.5);
    r0.fill(0.5);
    l1.fill(0.25);
    r1.fill(0.25);
    l2.fill(0.125);
    r2.fill(0.125);
    graph.set_input_active(2, true);
    graph.begin_crossfade(60);
    graph.process_block_streams(
        (&mut l0, &mut r0),
        &mut [(&mut l1, &mut r1), (&mut l2, &mut r2)],
    );
    assert_eq!(graph.mixer_state(), MixerState::PlayingNext);
    for (i, v) in l0.iter().enumerate() {
        assert!(
            (v - 0.3125).abs() < 1e-6,
            "PlayingNext 3-input sum mismatch at {i}: {v} (want 0.3125)"
        );
    }
}

// ── Phase 4 S1: slot-count parameter + per-slot user state ────────────────

#[test]
fn mix_slot_count_is_clamped_to_the_bus_bound() {
    let sr = 48000.0;

    // Below the minimum: a 1-slot request still yields the transition pair.
    let cfg = EngineConfig {
        mix_slots: 1,
        ..EngineConfig::default()
    };
    let g = DspGraph::from_config(&cfg, sr);
    assert_eq!(g.mix().inputs.len(), 2, "mix_slots=1 must clamp up to 2");

    // Above the maximum: clamped to MAX_MIX_SLOTS.
    let cfg = EngineConfig {
        mix_slots: 99,
        ..EngineConfig::default()
    };
    let g = DspGraph::from_config(&cfg, sr);
    assert_eq!(
        g.mix().inputs.len(),
        crate::dsp::graph::nodes::mix::MAX_MIX_SLOTS,
        "mix_slots=99 must clamp down to MAX_MIX_SLOTS"
    );

    // Default config stays at the canonical 2-slot layout.
    let g = DspGraph::from_config(&EngineConfig::default(), sr);
    assert_eq!(g.mix().inputs.len(), 2);
    assert_eq!(g.mix().inputs[0].name, "out_preamp");
    assert_eq!(g.mix().inputs[1].name, "in_preamp");
}

#[test]
fn per_slot_user_state_survives_reconfigure() {
    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 3,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);

    // Set lane 2 (slot 2) gain, balance, mute, and detach it via the queued
    // control surface, then drain (as a single-threaded caller would).
    graph.set_input_gain(2, 0.4);
    graph.set_input_balance(2, 0.25);
    graph.set_input_mute(2, true);
    graph.set_input_active(2, false);
    graph.drain_queued_control();

    // Reconfigure: the fresh generation must inherit the lane's settings.
    graph.reconfigure(&cfg);
    graph.drain_queued_control();
    let mix = graph.mix();
    let lane = &mix.inputs[2];
    // The gain is replayed as a *target* (one-pole ramp, like volume).
    assert_eq!(
        lane.gain.target_gain, 0.4,
        "lane gain target must survive reconfig"
    );
    assert_eq!(lane.balance, 0.25, "lane balance must survive reconfig");
    assert!(lane.mute, "lane mute must survive reconfig");
    assert!(!lane.active, "lane detachment must survive reconfig");

    // Slot 0 cannot be detached even if a snapshot claims otherwise.
    graph.set_input_active(0, false);
    graph.drain_queued_control();
    graph.reconfigure(&cfg);
    graph.drain_queued_control();
    assert!(graph.mix().inputs[0].active, "slot 0 is never detached");

    // A snapshot with fewer slots than the generation is fine: missing
    // entries keep defaults.
    let small = EngineConfig {
        mix_slots: 2,
        ..EngineConfig::default()
    };
    graph.reconfigure(&small);
    graph.drain_queued_control();
    assert_eq!(graph.mix().inputs.len(), 2);
    assert_eq!(graph.mix().inputs[1].gain.gain, 1.0, "default gain");
}

// ── Phase 4 S2: N-channel secondary planes + multichannel bus sum ─────────

#[test]
fn multichannel_stream_slots_sum_channel_wise() {
    use crate::decode::ChannelLayout;

    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 3,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);
    graph.set_multichannel_layout(&ChannelLayout::FivePointOne);

    let ch = 6; // 5.1
    let n = 128;
    let gain2 = 0.5f32;
    graph.mix_mut().inputs[2].gain.set_gain(gain2);
    graph.mix_mut().inputs[2].gain.snap();

    // Primary: all channels = 0.25. Secondary 1 (slot 2): all = 0.5.
    let mut primary = vec![0.25f32; n * ch];
    let mut sec2 = vec![0.5f32; n * ch];
    let mut sec1 = vec![0.125f32; n * ch]; // slot 1, gated by pair envelope in stereo but on MC summed too

    graph.process_block_multichannel_streams(
        &mut primary,
        ch,
        &mut [(&mut sec1, ch), (&mut sec2, ch)],
    );

    // On the MC path the pair envelope is stereo-only (it lives in
    // mix_stereo), so every secondary slot >= 1 is summed channel-wise at its
    // per-input gain: 0.25 (primary) + 0.125 (slot 1) + 0.5*0.5 (slot 2)
    // = 0.625, on every channel.
    for ch_idx in 0..ch {
        for i in 0..n {
            let v = primary[i * ch + ch_idx];
            assert!(
                (v - 0.625).abs() < 1e-5,
                "ch {ch_idx} frame {i}: {v} (want 0.625)"
            );
        }
    }
    assert_eq!(
        graph.mix().inputs[2].channels,
        ch,
        "slot channel count must be set by the multi-stream feed"
    );

    // Detach slot 2: only slot 1 remains summed -> 0.25 + 0.125 = 0.375.
    graph.set_input_active(2, false);
    graph.drain_queued_control();
    let mut primary = vec![0.25f32; n * ch];
    graph.process_block_multichannel_streams(
        &mut primary,
        ch,
        &mut [(&mut sec1, ch), (&mut sec2, ch)],
    );
    for ch_idx in 0..ch {
        for i in 0..n {
            let v = primary[i * ch + ch_idx];
            assert!(
                (v - 0.375).abs() < 1e-5,
                "detached slot leaked: ch {ch_idx} frame {i}: {v} (want 0.375)"
            );
        }
    }
}
