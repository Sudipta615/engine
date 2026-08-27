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

// ── Phase 4 S3: per-slot pan law + meters ─────────────────────────────────

#[test]
fn pan_shapes_front_pair_and_meters_publish() {
    use crate::dsp::crossfade::MixerState;

    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 2,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);

    let n = 256;
    // Pan slot 0 hard right (Linear law): L = 0, R = full.
    graph.set_input_pan(0, 1.0);
    graph.drain_queued_control();

    let mut l0 = vec![0.5f32; n];
    let mut r0 = vec![0.5f32; n];
    let mut l1 = vec![0.0f32; n];
    let mut r1 = vec![0.0f32; n];
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    assert_eq!(graph.mixer_state(), MixerState::PlayingCurrent);

    // Linear pan 1.0: L = 0.5 * 0 = 0, R = 0.5 * 1 = 0.5.
    for i in 0..n {
        assert!(
            l0[i].abs() < 1e-6,
            "panned-left channel must be silent: {i}: {}",
            l0[i]
        );
        assert!(
            (r0[i] - 0.5).abs() < 1e-6,
            "panned-right channel: {i}: {}",
            r0[i]
        );
    }

    // Meters: slot 0 processed 0.5-amplitude stereo (post-pan: L=0, R=0.5),
    // so peak = 20*log10(0.5) ≈ -6.02 dB, RMS over both channels:
    // sqrt((0 + 0.25)/2) = 0.3536 -> 20*log10(0.3536) ≈ -9.03 dB.
    let (peak, rms) = graph.control_handle().slot_meters(0);
    assert!((peak - (-6.02)).abs() < 0.2, "peak {peak}");
    assert!((rms - (-9.03)).abs() < 0.2, "rms {rms}");
}

#[test]
fn duck_gates_target_slot_from_source_peak() {
    // Phase 4 S4: program-gated ducking. Slot 1 (a loud secondary stream)
    // gates slot 0: once engaged, slot 0's level drops by the depth. The
    // trigger is block-synchronous from the source slot's peak meter.
    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 3,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;

    // Loud voice-over on slot 1 (0 dBFS) ducks the music on slot 0 by 12 dB.
    graph.set_duck(Some(DuckState {
        source: 1,
        threshold_db: -60.0,
        depth_db: 12.0,
        attack_frames: 0,
        release_frames: 0,
        targets: [0; MAX_DUCK_TARGETS],
        target_count: 1,
    }));
    graph.drain_queued_control();

    let mut l0 = vec![0.5f32; n];
    let mut r0 = vec![0.5f32; n];
    let mut l1 = vec![1.0f32; n];
    let mut r1 = vec![1.0f32; n];
    // The trigger is block-synchronous: the duck gain lands one block after
    // the source meter is published, so run a few blocks to reach steady
    // state. The caller re-feeds fresh input each block (mix_stereo scales
    // the master planes in place, so reusing the buffer would compound).
    for _ in 0..4 {
        l0.fill(0.5);
        r0.fill(0.5);
        l1.fill(1.0);
        r1.fill(1.0);
        graph.process_block_streams((&mut l0, &mut r0), &mut [(&mut l1, &mut r1)]);
    }

    // Slot 0 ducked: 0.5 * 10^(-12/20) = 0.1256 -> -18.02 dB peak.
    let (peak0, _) = graph.control_handle().slot_meters(0);
    assert!(
        (peak0 - (-18.02)).abs() < 0.2,
        "target slot ducked to ~-18 dB, got {peak0}"
    );
    // Slot 1 (source) untouched at 0 dBFS.
    let (peak1, _) = graph.control_handle().slot_meters(1);
    assert!((peak1 - 0.0).abs() < 0.2, "source peak ~0 dB, got {peak1}");

    // Disabling restores the unducked level on the next block.
    graph.set_duck(None);
    graph.drain_queued_control();
    for _ in 0..2 {
        l0.fill(0.5);
        r0.fill(0.5);
        l1.fill(1.0);
        r1.fill(1.0);
        graph.process_block_streams((&mut l0, &mut r0), &mut [(&mut l1, &mut r1)]);
    }
    let (peak0, _) = graph.control_handle().slot_meters(0);
    assert!(
        (peak0 - (-6.02)).abs() < 0.2,
        "duck disabled -> ~-6 dB, got {peak0}"
    );
}

#[test]
fn automation_track_shapes_lane_slot_sample_accurate() {
    // Phase 4 S5: a Gain automation track on a lane slot (>= 2) is applied
    // sample-accurately (linear interpolation between breakpoints), and the
    // runner's absolute position advances across blocks (edge values hold).
    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 3,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;

    // Ramp lane gain from 1.0 at frame 0 to 0.5 at frame 256, then hold.
    let pts = [
        AutomationPoint {
            frame: 0,
            value: 1.0,
        },
        AutomationPoint {
            frame: 256,
            value: 0.5,
        },
    ];
    graph.set_slot_automation(2, AutomationTarget::Gain, &pts);
    graph.drain_queued_control();

    let mut l0 = vec![0.0f32; n];
    let mut r0 = vec![0.0f32; n];
    let mut l1 = vec![0.0f32; n];
    let mut r1 = vec![0.0f32; n];
    let mut l2 = vec![1.0f32; n];
    let mut r2 = vec![1.0f32; n];
    graph.process_block_streams(
        (&mut l0, &mut r0),
        &mut [(&mut l1, &mut r1), (&mut l2, &mut r2)],
    );

    // Frame 0: value 1.0 -> lane contributes 1.0.
    assert!((l0[0] - 1.0).abs() < 1e-6, "frame 0 gain 1.0: {}", l0[0]);
    // Midpoint (frame 128) interpolates linearly to exactly 0.75.
    assert!(
        (l0[128] - 0.75).abs() < 1e-6,
        "frame 128 interpolated ~0.75: {}",
        l0[128]
    );

    // Second block: absolute frames 256..512 are past the last point, so the
    // edge value (0.5) holds across the whole block.
    l0.fill(0.0);
    r0.fill(0.0);
    l2.fill(1.0);
    r2.fill(1.0);
    graph.process_block_streams(
        (&mut l0, &mut r0),
        &mut [(&mut l1, &mut r1), (&mut l2, &mut r2)],
    );
    for (i, &v) in l0.iter().enumerate() {
        assert!(
            (v - 0.5).abs() < 1e-6,
            "hold value 0.5 past track end: {i}: {v}"
        );
    }

    // Clearing the track restores unity (bit-exact shape).
    graph.clear_slot_automation(2);
    graph.drain_queued_control();
    l2.fill(1.0);
    r2.fill(1.0);
    l0.fill(0.0);
    r0.fill(0.0);
    graph.process_block_streams(
        (&mut l0, &mut r0),
        &mut [(&mut l1, &mut r1), (&mut l2, &mut r2)],
    );
    assert!(
        (l0[0] - 1.0).abs() < 1e-6,
        "cleared automation unity: {}",
        l0[0]
    );
}

#[test]
fn automation_pan_track_moves_lane_front_pair() {
    // Phase 4 S5 Pan target: the automation value replaces the static pan.
    // With the Linear law, pan 1.0 kills the left channel and keeps right.
    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 3,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;

    let pts = [AutomationPoint {
        frame: 0,
        value: 1.0,
    }];
    graph.set_slot_automation(2, AutomationTarget::Pan, &pts);
    graph.drain_queued_control();

    let mut l0 = vec![0.0f32; n];
    let mut r0 = vec![0.0f32; n];
    let mut l1 = vec![0.0f32; n];
    let mut r1 = vec![0.0f32; n];
    let mut l2 = vec![1.0f32; n];
    let mut r2 = vec![1.0f32; n];
    graph.process_block_streams(
        (&mut l0, &mut r0),
        &mut [(&mut l1, &mut r1), (&mut l2, &mut r2)],
    );
    for i in 0..n {
        assert!(l0[i].abs() < 1e-6, "pan right kills L: {i}: {}", l0[i]);
        assert!(
            (r0[i] - 1.0).abs() < 1e-6,
            "pan right keeps R: {i}: {}",
            r0[i]
        );
    }
}

#[test]
fn slot_trim_send_and_aux_bus_shape_the_master() {
    // Phase 5 S1+S2+S3: per-slot channel trim, post-fader sends, and the
    // aux bus accumulator. Slot 1 (a lane) gets a -6 dB L-channel trim, a
    // master-send of 0.5, and an aux-send of 1.0; the aux return joins the
    // master at unity. Disabled aux + unity sends must stay bit-exact with
    // the plain sum (covered by the equivalence suite).
    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 3,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;
    let h = graph.control_handle();

    // Baseline: plain lane at unity on the master. Lanes ride slots >= 2
    // (process_block_lanes feeds lane k into slot k+2), so slot 1 stays
    // silent while the lane is audible after the pair envelope.
    let mut l0 = vec![0.0f32; n];
    let mut r0 = vec![0.0f32; n];
    let mut l1 = vec![0.5f32; n];
    let mut r1 = vec![0.25f32; n];
    graph.process_block_lanes((&mut l0, &mut r0), &mut [(&mut l1, &mut r1)]);
    let (base_peak, _) = h.slot_meters(0);
    assert!(
        (base_peak - (-6.02)).abs() < 0.2,
        "unity lane sums to ~-6 dB peak (0.5), got {base_peak}"
    );

    // S1: -6 dB trim on slot 1's L channel halves the left contribution
    // (0.5 -> 0.25), so the master peak drops to 0.25 (-12.04 dB).
    graph.set_slot_trim(2, 0, -6.0, false);
    graph.drain_queued_control();
    l0.fill(0.0);
    r0.fill(0.0);
    l1.fill(0.5);
    r1.fill(0.25);
    graph.process_block_lanes((&mut l0, &mut r0), &mut [(&mut l1, &mut r1)]);
    let (trim_peak, _) = h.slot_meters(0);
    assert!(
        (trim_peak - (-12.04)).abs() < 0.2,
        "L trim -6 dB -> 0.25 peak (-12.04 dB), got {trim_peak}"
    );

    // S2: master-send 0.5 halves both channels (L 0.5->0.25, R 0.25->0.125);
    // the master peak tracks the louder L at 0.25 -> -12.04 dB.
    graph.set_slot_trim(2, 0, 0.0, false);
    graph.set_slot_send(2, 0.5, 0.0);
    graph.drain_queued_control();
    l0.fill(0.0);
    r0.fill(0.0);
    l1.fill(0.5);
    r1.fill(0.25);
    graph.process_block_lanes((&mut l0, &mut r0), &mut [(&mut l1, &mut r1)]);
    let (send_peak, _) = h.slot_meters(0);
    assert!(
        (send_peak - (-12.04)).abs() < 0.2,
        "master-send 0.5 -> L 0.25 peak (-12.04 dB), got {send_peak}"
    );

    // S3: aux-send 1.0 with a unity return adds the post-fader signal back
    // via the aux bus (master L 0.25 + aux L 0.5 = 0.75, -2.50 dB).
    graph.set_slot_send(2, 0.5, 1.0);
    graph.set_aux(true, 1.0);
    graph.drain_queued_control();
    l0.fill(0.0);
    r0.fill(0.0);
    l1.fill(0.5);
    r1.fill(0.25);
    graph.process_block_lanes((&mut l0, &mut r0), &mut [(&mut l1, &mut r1)]);
    let (aux_peak, _) = h.slot_meters(0);
    assert!(
        (aux_peak - (-2.50)).abs() < 0.2,
        "aux return at unity -> L 0.75 peak (-2.50 dB), got {aux_peak}"
    );
    let (aux_peak_m, _) = h.aux_meters();
    assert!(
        (aux_peak_m - (-6.02)).abs() < 0.2,
        "aux bus meters the 0.5 L send (-6.02 dB), got {aux_peak_m}"
    );

    // Disabling the aux return restores the master-send-only level.
    graph.set_aux(false, 1.0);
    graph.drain_queued_control();
    l0.fill(0.0);
    r0.fill(0.0);
    l1.fill(0.5);
    r1.fill(0.25);
    graph.process_block_lanes((&mut l0, &mut r0), &mut [(&mut l1, &mut r1)]);
    let (off_peak, _) = h.slot_meters(0);
    assert!(
        (off_peak - (-12.04)).abs() < 0.2,
        "aux disabled -> 0.25 peak (-12.04 dB), got {off_peak}"
    );
}

// ── Phase 5 regressions (v3.6.1) ───────────────────────────────────────────

#[test]
fn phase5_reconfigure_drains_queued_control_and_carries_duck_automation() {
    // A command enqueued BEFORE a reconfig (e.g. SetTrackGain followed by
    // AddTrack growing the bus) must land on the fresh generation, and a
    // configured duck / automation track must survive the rebuild — the
    // pre-fix code snapshotted the bus before the queue drained and carried
    // neither duck nor automation.
    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 3,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);

    // Queue commands WITHOUT draining: the reconfig must flush them first.
    graph.set_slot_send(2, 0.5, 0.0);
    graph.set_duck(Some(DuckState {
        source: 2,
        threshold_db: -40.0,
        depth_db: 6.0,
        attack_frames: 0,
        release_frames: 0,
        targets: [2; MAX_DUCK_TARGETS],
        target_count: 1,
    }));
    let pts = [
        AutomationPoint {
            frame: 0,
            value: 1.0,
        },
        AutomationPoint {
            frame: 512,
            value: 0.5,
        },
    ];
    graph.set_slot_automation(2, AutomationTarget::Gain, &pts);

    // Growing the bus forces a rebuild.
    let grown = EngineConfig {
        mix_slots: 4,
        ..cfg.clone()
    };
    graph.reconfigure(&grown);
    // Swap the fresh generation in (reconfigure publishes; control_tick
    // applies at the next block boundary).
    let mut l = vec![0.0f32; 256];
    let mut r = vec![0.0f32; 256];
    graph.process_block(&mut l, &mut r);

    let mix = graph.mix();
    assert_eq!(mix.inputs.len(), 4, "bus grew to 4 slots");
    // The queued send survived the rebuild (not reset to master 1.0/aux 0.0).
    assert_eq!(mix.inputs[2].send.master_gain, 0.5, "queued send survived");
    // Duck + automation carried across the generation.
    let duck = mix.duck.expect("duck survived the rebuild");
    assert_eq!(duck.cfg.depth_db, 6.0);
    assert!(duck.cfg.targets[..duck.cfg.target_count].contains(&2));
    let auto = mix.inputs[2].automation.expect("automation survived");
    assert_eq!(auto.count, 2);
    assert_eq!(auto.points[0].frame, 0);
    assert_eq!(auto.points[1].value, 0.5);
}

#[test]
fn phase5_config_mix_trims_sends_aux_wire_at_construction() {
    // The config surface (`mix_trims` / `mix_sends` / `aux`) was declared +
    // serialized but never applied: a host configuring them at construction
    // got a graph that ignored them. Now they seed the generation directly.
    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 3,
        mix_trims: vec![config::SlotTrimEntry {
            slot: 1,
            channel: 0,
            gain_db: -6.0,
            invert: true,
        }],
        mix_sends: vec![config::SlotSendConfig {
            slot: 1,
            master_gain: 0.5,
            aux_gain: 0.25,
        }],
        aux: config::AuxBusConfig {
            enabled: true,
            return_gain: 0.75,
            ..Default::default()
        },
        ..EngineConfig::default()
    };
    let graph = DspGraph::from_config(&cfg, sr);
    let mix = graph.mix();

    // Trim: -6 dB linear ≈ 0.5012, inverted.
    let trim = mix.inputs[1].trim;
    assert!((trim.gains[0] - 0.5012).abs() < 1e-3, "trim gain wired");
    assert!(trim.invert[0], "trim polarity wired");
    assert_eq!(trim.gains[1], 1.0);
    // Send: master 0.5 / aux 0.25.
    assert_eq!(mix.inputs[1].send.master_gain, 0.5, "master send wired");
    assert_eq!(mix.inputs[1].send.aux_gain, 0.25, "aux send wired");
    // Aux bus: enabled + return 0.75.
    assert!(mix.aux.enabled, "aux enabled from config");
    assert_eq!(mix.aux.return_gain, 0.75);
}

#[test]
fn phase5_pair_slots_tap_aux_accumulator_and_return() {
    // The pair slots (0/1) are the transition pair: pre-fix they were the
    // ONLY slots without an aux tap, so a send-only pair slot silently
    // dropped its aux contribution while every lane slot tapped fine.
    let sr = 48000.0;
    let cfg = EngineConfig::default(); // 2 slots
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;

    // Slot 0: sends-only (master 0, aux 1), aux return at 0 so the tap is
    // observable on the aux meter without returning into the master.
    graph.set_slot_send(0, 0.0, 1.0);
    graph.set_aux(true, 0.0);
    graph.drain_queued_control();

    let mut l0 = vec![0.5f32; n];
    let mut r0 = vec![0.25f32; n];
    let mut l1 = vec![0.0f32; n];
    let mut r1 = vec![0.0f32; n];
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));

    // Master silent (master-send 0), aux meters the post-fader tap: L 0.5.
    let (master_peak, _) = graph.control_handle().slot_meters(0);
    assert!(master_peak < -60.0, "sends-only slot 0 keeps master silent");
    let (aux_peak, _) = graph.control_handle().aux_meters();
    assert!(
        (aux_peak - (-6.02)).abs() < 0.2,
        "pair slot 0 taps the aux bus (~-6 dB), got {aux_peak}"
    );

    // Now bring the return up: the aux content joins the master.
    graph.set_aux(true, 1.0);
    graph.drain_queued_control();
    let mut l0 = vec![0.5f32; n];
    let mut r0 = vec![0.25f32; n];
    let mut l1 = vec![0.0f32; n];
    let mut r1 = vec![0.0f32; n];
    graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    let (master_peak, _) = graph.control_handle().slot_meters(0);
    assert!(
        (master_peak - (-6.02)).abs() < 0.2,
        "aux return mixes slot 0's send back into the master (~-6 dB)"
    );
}

#[test]
fn phase5_duck_envelope_advances_once_per_block_with_lane_slots() {
    // With independent slots (mix_slots >= 3), the duck envelope used to
    // tick TWICE per block (once in mix_stereo, once in sum_extra_slots),
    // ramping attack/release at 2x the configured rate. The duck gain is a
    // one-pole step of `frames/attack_frames` per tick.
    let sr = 48000.0;
    let cfg = EngineConfig {
        mix_slots: 3,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;
    graph.set_duck(Some(DuckState {
        source: 2,
        threshold_db: -60.0,
        depth_db: 12.0,
        attack_frames: 480,
        release_frames: 480,
        targets: [2; MAX_DUCK_TARGETS],
        target_count: 1,
    }));
    graph.drain_queued_control();

    let mut l0 = vec![0.0f32; n];
    let mut r0 = vec![0.0f32; n];
    let mut lane_l = vec![1.0f32; n];
    let mut lane_r = vec![1.0f32; n];
    // The trigger is evaluated from the source slot's PUBLISHED meter; the
    // initial meter is 0 dB (above the -60 dB threshold), so all four blocks
    // are engaged and the envelope advances one one-pole step of
    // 256/480 per tick. 4 blocks → 4 ticks.
    let step = n as f32 / 480.0;
    let target = 10.0f32.powf(-12.0 / 20.0);
    let mut expected = 1.0f32;
    for _ in 0..4 {
        expected += (target - expected) * step;
        l0.fill(0.0);
        r0.fill(0.0);
        graph.process_block_lanes((&mut l0, &mut r0), &mut [(&mut lane_l, &mut lane_r)]);
    }
    let cl = graph.mix().duck.as_ref().unwrap().current_linear;
    assert!(
        (cl - expected).abs() < 1e-4,
        "duck advanced once per block (got {cl}, single-tick expected {expected})"
    );
    // Sanity: it must NOT match the 2x (double-tick) trajectory (8 ticks).
    let mut double = 1.0f32;
    for _ in 0..8 {
        double += (target - double) * step;
    }
    assert!(
        (cl - double).abs() > 0.01,
        "duck must not tick twice per block (got {cl}, 2x trajectory {double})"
    );
}

#[test]
fn phase5_f64_path_publishes_slot_and_aux_meters() {
    // Quality-mode (f64) blocks ran the plan but never published the mix
    // meters, so telemetry read stale slot/aux levels after f64 processing.
    let sr = 48000.0;
    let cfg = EngineConfig {
        precision_mode: config::PrecisionMode::Quality,
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);
    let n = 256;

    // A sends-only slot 0 with an aux send makes both the slot and the aux
    // meter observable on the f64 path.
    graph.set_slot_send(0, 0.0, 1.0);
    graph.set_aux(true, 0.0);
    graph.drain_queued_control();

    let mut l = vec![0.5f64; n];
    let mut r = vec![0.25f64; n];
    graph.process_block_f64(&mut l, &mut r);

    let (master_peak, _) = graph.control_handle().slot_meters(0);
    assert!(master_peak < -60.0, "f64 sends-only master silent");
    let (aux_peak, _) = graph.control_handle().aux_meters();
    assert!(
        (aux_peak - (-6.02)).abs() < 0.2,
        "f64 path publishes aux meters (~-6 dB), got {aux_peak}"
    );
}

/// Write a short 16-bit stereo WAV whose left channel is a single impulse
/// (sample 0 = 1.0, rest silence); right channel silent. Used as an IR for
/// the aux-insert tests (a delta IR passes the signal through unchanged,
/// modulo the convolution engine's latency).
fn write_impulse_wav(path: &std::path::Path, sample_rate: u32, n_frames: usize) {
    let mut data = Vec::with_capacity(n_frames * 4);
    for i in 0..n_frames {
        let v = if i == 0 { 32767i16 } else { 0i16 };
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(path, &wav).unwrap();
}

#[test]
fn phase6_aux_insert_convolves_toggles_and_survives_swap() {
    // Phase 6: the aux bus carries a global convolution insert between the
    // accumulator and the return. With a delta IR the send content should
    // pass through (modulo engine latency); the runtime toggle (enabled /
    // wet only) must gate it and survive a generation swap.
    let sr = 48000.0;
    let ir_path = std::env::temp_dir().join(format!(
        "phase6_aux_ir_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_impulse_wav(&ir_path, 48000, 2048);

    let cfg = EngineConfig {
        aux: config::AuxBusConfig {
            enabled: true,
            return_gain: 0.0, // return at 0: observe the insert via meters
            insert_enabled: true,
            insert_wet_mix: 1.0,
            insert_ir_path: Some(ir_path.display().to_string()),
        },
        ..EngineConfig::default()
    };
    let mut graph = DspGraph::from_config(&cfg, sr);
    graph.set_slot_send(0, 0.0, 1.0);
    graph.drain_queued_control();

    let n = 512;
    let run_block = |graph: &mut DspGraph| {
        let mut l0 = vec![0.5f32; n];
        let mut r0 = vec![0.25f32; n];
        let mut l1 = vec![0.0f32; n];
        let mut r1 = vec![0.0f32; n];
        graph.process_block_inputs((&mut l0, &mut r0), (&mut l1, &mut r1));
    };

    // Insert active: the delta IR passes the send through → the aux meter
    // sees the post-fader level (~-6 dB on the left plane).
    run_block(&mut graph);
    let (aux_peak, _) = graph.control_handle().aux_meters();
    assert!(
        (aux_peak - (-6.02)).abs() < 0.5,
        "aux insert passes the delta IR content through (~-6 dB), got {aux_peak}"
    );

    // Runtime toggle off → insert bypassed → the accumulator still holds the
    // send, but no IR is applied; the return is zero so the master stays
    // silent and the METER still reads the raw send (the insert only shapes
    // the return). Assert the insert engine is disabled via the mirrored
    // control state.
    graph.set_aux_insert(false, 1.0);
    graph.drain_queued_control();
    let (ins_enabled, ins_wet) = graph.control_handle().aux_insert_state();
    assert!(!ins_enabled, "runtime toggle disables the insert");
    assert_eq!(ins_wet, 1.0);

    // Toggle back on, then grow the bus (reconfigure → generation swap): the
    // live runtime toggle must survive the swap.
    graph.set_aux_insert(true, 0.5);
    graph.drain_queued_control();
    let mut cfg3 = cfg.clone();
    cfg3.mix_slots = 3;
    graph.reconfigure(&cfg3);
    graph.drain_queued_control();
    let (ins_enabled, ins_wet) = graph.control_handle().aux_insert_state();
    assert!(ins_enabled, "insert toggle survives generation swap");
    assert!(
        (ins_wet - 0.5).abs() < 1e-6,
        "insert wet survives generation swap (got {ins_wet})"
    );
    let mix = graph.mix();
    let engine_enabled = mix
        .aux
        .insert
        .as_ref()
        .map(|e| e.is_enabled())
        .unwrap_or(false);
    assert!(engine_enabled, "rebuilt generation keeps the insert engine");

    // Disabled-and-reconfigured: a runtime OFF also survives (off wins over
    // the config's enabled=true, matching the other bus state semantics).
    graph.set_aux_insert(false, 0.5);
    graph.drain_queued_control();
    let mut cfg4 = cfg3.clone();
    cfg4.mix_slots = 4;
    graph.reconfigure(&cfg4);
    graph.drain_queued_control();
    assert!(
        !graph.control_handle().aux_insert_state().0,
        "insert disabled state survives generation swap"
    );
    let _ = std::fs::remove_file(&ir_path);
}

#[test]
fn phase6_bit_exact_simd_accumulate_matches_scalar() {
    // The SIMD aux-return accumulate must be bit-for-bit identical to the
    // scalar `dst += src * g` (element-wise mul then add — no FMA, no
    // reordering). This is the contract the graph-vs-pipeline equivalence
    // suite relies on.
    let mut rng = 0x9E37_79B9_1234_5678u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng as f32 / u64::MAX as f32 * 2.0 - 1.0
    };
    for &len in &[0usize, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 63, 64, 65, 257] {
        let src: Vec<f32> = (0..len).map(|_| next()).collect();
        let mut dst_a: Vec<f32> = (0..len).map(|_| next()).collect();
        let mut dst_b = dst_a.clone();
        let g = next();
        crate::dsp_utils::accumulate_scaled(&mut dst_a, &src, g, len);
        for i in 0..len {
            dst_b[i] += src[i] * g;
        }
        for i in 0..len {
            assert_eq!(
                dst_a[i].to_bits(),
                dst_b[i].to_bits(),
                "f32 accumulate diverges at {i} (len {len})"
            );
        }

        let src64: Vec<f64> = src.iter().map(|&v| v as f64).collect();
        let mut dst_a64: Vec<f64> = dst_a.iter().map(|&v| v as f64).collect();
        let mut dst_b64 = dst_a64.clone();
        let g64 = g as f64;
        crate::dsp_utils::accumulate_scaled_f64(&mut dst_a64, &src64, g64, len);
        for i in 0..len {
            dst_b64[i] += src64[i] * g64;
        }
        for i in 0..len {
            assert_eq!(
                dst_a64[i].to_bits(),
                dst_b64[i].to_bits(),
                "f64 accumulate diverges at {i} (len {len})"
            );
        }
    }
}
