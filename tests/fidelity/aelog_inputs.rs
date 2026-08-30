//! Fidelity tests — aelog render inputs: audio inputs & listener motion
//! (v3.32, roadmap extension after v3.29 determinism).
//!
//! The guide's event log must capture every input a render consumes, not
//! just timeline commands. This suite pins the two remaining inputs:
//! * audio fed into the graph (`Buffer` source nodes / external tracks) —
//!   recorded chunk-wise and reconstructed on replay to drive renders with
//!   the exact session audio;
//! * listener motion — timestamped `(master sample, position)` samples a
//!   spatial renderer re-applies sample-exactly.
//!
//! Combined sessions (audio + listener + a beat-timed gain gate) replay to
//! byte-identical captures, so a spatial golden render is reproducible.

use engine::prelude::{
    replay_events, replay_render, AelogRecorder, EventPayload, EventTime, ExecutionOrder, Graph2,
    NodeId, PortId, TransportState, Vec3,
};

const SR: f32 = 48_000.0;
const BLOCK: u64 = 128;

/// A graph whose Buffer source is fed by the recorded audio input:
/// buffer(in, empty clip) → sink(out).
fn input_graph() -> (Graph2, ExecutionOrder, NodeId) {
    let mut g = Graph2::new();
    let b = g.add_buffer("in", vec![], false); // empty clip — external track drives it
    let sink = g.add_sink("out");
    g.add_edge(b, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    (g, order, sink)
}

/// Like [`input_graph`] but with a gain node (initially 0) between the
/// buffer and the sink, so a beat-timed `SetGain` can gate the render.
/// Returns (graph, order, gain node id, sink node id).
fn gated_input_graph() -> (Graph2, ExecutionOrder, NodeId, NodeId) {
    let mut g = Graph2::new();
    let b = g.add_buffer("in", vec![], false);
    let gain = g.add_gain("gate", 0.0);
    let sink = g.add_sink("out");
    g.add_edge(b, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    (g, order, gain, sink)
}

fn track() -> Vec<f32> {
    // 200 blocks (25 600 samples) of a deterministic ramp — long enough to
    // span the beat-1 gate at sample 24 000.
    (0..200 * BLOCK as usize)
        .map(|i| i as f32 * 0.001)
        .collect()
}

#[test]
fn audio_input_reconstructs_exact_track() {
    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    for chunk in track().chunks(BLOCK as usize) {
        rec.record_audio_input(chunk);
        rec.advance_block(BLOCK);
    }
    let log = rec.finish();

    let out = replay_events(&log).unwrap();
    assert_eq!(
        out.audio_input,
        vec![track()],
        "mono chunks concatenate into the single channel plane"
    );
    assert!(out.listener_motion.is_empty());
}

#[test]
fn recorded_audio_drives_byte_identical_render() {
    let (g, order, sink) = input_graph();
    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    for chunk in track().chunks(BLOCK as usize) {
        rec.record_audio_input(chunk);
        rec.advance_block(BLOCK);
    }
    let log = rec.finish();

    // Replay through the same graph: the buffer reads the reconstructed
    // track, so the capture is the exact session audio.
    let out = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.captured.len(), track().len());
    for (i, s) in out.captured.iter().enumerate() {
        assert!(
            (s - track()[i]).abs() < 1e-6,
            "captured[{i}] = {s}, expected {}",
            track()[i]
        );
    }
    assert_eq!(out.audio_input, vec![track()]);
}

#[test]
fn listener_motion_replays_timestamped_trajectory() {
    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    let a = Vec3::new(1.0, 2.0, 3.0);
    let b = Vec3::new(-4.0, 5.0, -6.0);
    rec.record_listener_position(a); // at master 0
    rec.advance_block(BLOCK);
    rec.advance_block(BLOCK);
    rec.record_listener_position(b); // at master 256
    let log = rec.finish();

    let out = replay_events(&log).unwrap();
    assert_eq!(
        out.listener_motion,
        vec![(0, a), (256, b)],
        "positions stamped with the master sample at record time"
    );
    assert!(out.audio_input.is_empty());
}

#[test]
fn combined_session_replays_audio_listener_and_gate_exactly() {
    // Buffer → gain(0) → sink, plus a gain gate recorded as a beat-timed
    // SetGain on the gain node.
    let (g, order, gain_node, sink) = gated_input_graph();
    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    rec.schedule(
        EventTime::Beat(1.0),
        EventPayload::SetGain {
            node: gain_node.0,
            gain: 1.0,
        },
    )
    .unwrap();
    rec.record_listener_position(Vec3::new(0.0, 0.0, 1.0)); // at master 0

    let trk = track();
    for (i, chunk) in trk.chunks(BLOCK as usize).enumerate() {
        rec.record_audio_input(chunk);
        // Listener drifts one unit per block along +x.
        rec.record_listener_position(Vec3::new(i as f32 * 2.0, 0.0, 1.0));
        rec.advance_block(BLOCK);
    }
    let log = rec.finish();

    // The gate fires at beat 1 of 120 BPM = sample 24 000 (block 187,
    // local frame 64). Before it, the gain node is 0 → silence; after it,
    // the capture must equal the recorded track.
    let out = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.captured.len(), trk.len());
    assert!(out.captured[..24_000].iter().all(|s| s.abs() < 1e-6));
    for (i, want) in trk.iter().enumerate().skip(24_000) {
        assert!(
            (out.captured[i] - want).abs() < 1e-6,
            "captured[{i}] = {} vs {}",
            out.captured[i],
            want
        );
    }

    // Audio track and listener trajectory both reconstructed exactly: the
    // initial sample at master 0, then one sample per block — each stamped
    // with the master at record time (before that block's advance).
    assert_eq!(out.audio_input, vec![trk.clone()]);
    let n = trk.len() / BLOCK as usize; // 200 blocks
    assert_eq!(out.listener_motion.len(), 1 + n);
    // Each block's position is stamped *before* that block's advance, so
    // the first two samples (initial + block 0) both sit at master 0 and
    // sample k ≥ 2 lands at (k-1)·BLOCK.
    for (i, (at, p)) in out.listener_motion.iter().enumerate() {
        let expect_at = if i <= 1 { 0 } else { (i - 1) as u64 * BLOCK };
        assert_eq!(*at, expect_at, "trajectory sample {i} master");
        let x = if i == 0 { 0.0 } else { (i - 1) as f32 * 2.0 };
        assert_eq!(*p, Vec3::new(x, 0.0, 1.0), "trajectory sample {i} pos");
    }

    // The gate survived: exactly one SetGain fired, at the exact sample.
    let gates: Vec<_> = out
        .fired
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::SetGain { .. }))
        .collect();
    assert_eq!(gates.len(), 1, "gate fires exactly once");
    assert_eq!(gates[0].at, 24_000, "beat-1 gate lands sample-exact");
}

#[test]
fn clip_addressed_audio_replays_multi_input_mix() {
    // Two clip-addressed buffers ("mic", "synth") summed by a mix — the
    // multi-input graph. Each clip's recorded chunks reconstruct into its
    // own track and feed only its node, so the capture is the exact mix
    // (and neither clip leaks into the other's node).
    let mut g = Graph2::new();
    let mic = g.add_buffer_clip("mic-buf", "mic", vec![], false);
    let synth = g.add_buffer_clip("synth-buf", "synth", vec![], false);
    let mix = g.add_mix("sum", 2);
    let sink = g.add_sink("out");
    g.add_edge(mic, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(synth, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let mic_track: Vec<f32> = (0..200 * BLOCK as usize)
        .map(|i| (i as f32 * 0.001).sin())
        .collect();
    let synth_track: Vec<f32> = (0..200 * BLOCK as usize)
        .map(|i| (i as f32 * 0.0005).cos())
        .collect();

    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    for (m, s) in mic_track
        .chunks(BLOCK as usize)
        .zip(synth_track.chunks(BLOCK as usize))
    {
        rec.record_clip_audio("mic", m);
        rec.record_clip_audio("synth", s);
        rec.advance_block(BLOCK);
    }
    let log = rec.finish();

    // Each clip reconstructs into its own track; nothing unaddressed.
    let out = replay_events(&log).unwrap();
    assert!(out.audio_input.is_empty(), "no unaddressed track recorded");
    assert_eq!(out.clip_tracks.len(), 2, "one track per clip");
    let by_clip: std::collections::HashMap<&str, &[Vec<f32>]> = out
        .clip_tracks
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_slice()))
        .collect();
    assert_eq!(by_clip["mic"][0], mic_track.as_slice());
    assert_eq!(by_clip["synth"][0], synth_track.as_slice());

    // Replay through the multi-input graph: each clip feeds only its node,
    // so the capture is the exact per-sample mix.
    let out = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.captured.len(), mic_track.len());
    for i in 0..mic_track.len() {
        let want = mic_track[i] + synth_track[i];
        assert!(
            (out.captured[i] - want).abs() < 1e-5,
            "mixed[{i}] = {} vs {want}",
            out.captured[i]
        );
    }
}

#[test]
fn stereo_audio_replays_exact_channel_planes() {
    // A stereo session: each recorded chunk carries left + right planes.
    // Replay reconstructs a 2-plane track and the stereo buffer plays each
    // channel on its own output port — so the spatial/stereo render is
    // byte-exact per channel.
    let mut g = Graph2::new();
    let b = g.add_buffer_channels("stereo-in", vec![vec![], vec![]], false);
    let sl = g.add_sink("l");
    let sr = g.add_sink("r");
    g.add_edge(b, PortId(0), sl, PortId::IN).unwrap();
    g.add_edge(b, PortId(1), sr, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let left: Vec<f32> = (0..64 * BLOCK as usize)
        .map(|i| (i as f32 * 0.002).sin())
        .collect();
    let right: Vec<f32> = (0..64 * BLOCK as usize)
        .map(|i| (i as f32 * 0.0015).cos())
        .collect();

    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    for (l, r) in left
        .chunks(BLOCK as usize)
        .zip(right.chunks(BLOCK as usize))
    {
        rec.record_audio_input_channels(&[l.to_vec(), r.to_vec()]);
        rec.advance_block(BLOCK);
    }
    let log = rec.finish();

    // The track reconstructs as two channel planes, exactly.
    let out = replay_events(&log).unwrap();
    assert_eq!(out.audio_input.len(), 2, "two channel planes");
    assert_eq!(out.audio_input[0], left, "left plane exact");
    assert_eq!(out.audio_input[1], right, "right plane exact");

    // Replay through the stereo buffer: each port carries its channel.
    let out = replay_render(&log, &g, &order, sl).unwrap();
    assert_eq!(out.captured, left, "left port reproduces the left plane");
    let out = replay_render(&log, &g, &order, sr).unwrap();
    assert_eq!(out.captured, right, "right port reproduces the right plane");
}

#[test]
fn buffer_node_renders_embedded_clip_without_external_track() {
    // No recorded audio: the buffer's own clip plays (one-shot), and replay
    // leaves the capture silent-free as the clip.
    let mut g = Graph2::new();
    let b = g.add_buffer("in", vec![1.0, 2.0, 3.0], false);
    let sink = g.add_sink("out");
    g.add_edge(b, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    rec.advance_block(BLOCK); // one block of silence after the clip
    let log = rec.finish();

    let out = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.audio_input, Vec::<Vec<f32>>::new(), "no audio recorded");
    assert_eq!(
        &out.captured[..3],
        &[1.0, 2.0, 3.0],
        "embedded clip plays through the buffer node"
    );
    assert!(out.captured[3..].iter().all(|s| s.abs() < 1e-6));
}
