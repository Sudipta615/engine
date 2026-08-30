//! Fidelity tests — tempo-mapped graph automation (v3.41).
//!
//! A control curve authored in **beats** drives a Gain node over time,
//! evaluated against a **tempo map** (so the sample where a landmark lands
//! follows tempo changes). The curve + tempo map are recorded in aelog and
//! replayed deterministically:
//! * `replay_events` reconstructs `outcome.tempo_map` and
//!   `outcome.gain_automation` from the log;
//! * `replay_render` attaches them to the executor, so a recorded musical
//!   session renders the exact gain sweep — byte-identical to a directly
//!   driven executor, reproducible across replays, and covered by the
//!   aelog-hash golden-render cache.

use engine::prelude::{
    log_hash, render_cached, replay_events, replay_render, Aelog, AelogCache, AelogRecorder,
    CurveBeats, ExecutionOrder, Graph2, NodeId, OfflineExecutor, PortId, TempoMap, TransportState,
};

const SR: f32 = 48_000.0;
const BLOCK: u64 = 2400; // 120 BPM → 1 beat = 24000 samples = 10 blocks

/// DC(1) → gain → sink. Gain 0 means silence; with automation the captured
/// output equals the curve's value at each sample.
fn dc_gain_graph() -> (Graph2, ExecutionOrder, NodeId, NodeId) {
    let mut g = Graph2::new();
    let buf = g.add_buffer("dc", vec![1.0], true);
    let gain = g.add_gain("vol", 0.0);
    let sink = g.add_sink("out");
    g.add_edge(buf, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    (g, order, gain, sink)
}

/// The recorded musical session: a tempo map (120 → 240 BPM at beat 4) and
/// a gain curve 0 → 1.0 over beats 0..8, run `blocks` blocks forward.
/// `node` is the graph's Gain node id (`.0`).
fn record_automation(node: u32, blocks: u64) -> (Aelog, TempoMap, CurveBeats, u32) {
    let mut map = TempoMap::new();
    map.push(0.0, 120.0);
    map.push(4.0, 240.0);
    let curve = CurveBeats::from_points(&[(0.0, 0.0), (8.0, 1.0)]).unwrap();

    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.record_tempo_map(&map);
    rec.record_gain_automation(node, &curve);
    rec.set_state(TransportState::Playing, 0);
    for _ in 0..blocks {
        rec.advance_block(BLOCK);
    }
    let log = rec.finish();
    (log, map, curve, node)
}

#[test]
fn replay_reconstructs_the_musical_automation() {
    let (log, map, curve, node) = record_automation(2, 20);
    let out = replay_events(&log).unwrap();
    assert_eq!(out.tempo_map, Some(map), "tempo map reconstructed");
    assert_eq!(
        out.gain_automation,
        vec![(node, curve)],
        "curves reconstructed"
    );

    // Two identical recordings hash identically (the hash is a pure
    // function of the commands).
    let log2 = record_automation(2, 20).0;
    assert_eq!(log_hash(&log), log_hash(&log2));
}

#[test]
fn golden_render_sweeps_the_gain_from_the_tempo_map() {
    // 8 beats under the tempo map = 4 beats @120 (96000) + 4 beats @240
    // (48000) = 144000 samples. Block 2400 → 60 blocks.
    let (g, order, gain, sink) = dc_gain_graph();
    let (log, map, curve, _node) = record_automation(gain.0, 60);

    // Reference: drive the executor directly with the same tempo map +
    // curve for the same block count.
    let mut ref_ex = OfflineExecutor::new(&g, &order, 2400usize, SR).unwrap();
    ref_ex.set_tempo_map(Some(map));
    ref_ex.set_gain_automation(gain, Some(curve));
    ref_ex.process_blocks(60).unwrap();
    let ref_cap = ref_ex.capture(sink).unwrap().to_vec();

    // Replay applies the recorded automation to its own executor — the
    // golden render must be byte-identical to the reference.
    let out = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.captured, ref_cap, "replay == directly-driven render");

    // The sweep is musical 0 → 1.0 over 8 beats: silent at 0, ~0.5 at beat
    // 4 (sample 96000), ~1.0 at the end (sample 143999).
    assert!(out.captured[0].abs() < 1e-6, "gain 0 at beat 0");
    assert!(
        (out.captured[96_000] - 0.5).abs() < 5e-4,
        "gain ~0.5 at beat 4 (sample 96000): got {}",
        out.captured[96_000]
    );
    assert!(
        (out.captured[143_999] - 1.0).abs() < 5e-4,
        "gain ~1.0 at the end: got {}",
        out.captured[143_999]
    );

    // Golden: a second replay and the aelog-hash cache are byte-identical.
    let again = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.captured, again.captured);
    let root = std::env::temp_dir().join(format!("aelog-auto-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cache = AelogCache::new(&root);
    let cached = cache.render_cached(&log, &g, &order, sink).unwrap();
    assert_eq!(cached.captured, out.captured, "cached == golden");
    let via_fn = render_cached(&log, &g, &order, sink).unwrap();
    assert_eq!(via_fn.captured, out.captured);
    let _ = std::fs::remove_dir_all(&root);
}
