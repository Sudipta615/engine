//! Fidelity tests — aelog golden-render cache (v3.33).
//!
//! A golden render is a pure function of `(log, graph, sink)`, so identical
//! logs through the same graph can reuse a stored capture instead of
//! re-rendering. This suite pins the cache contract:
//! * identical recorded sessions hash identically and the second
//!   `render_cached` call returns the stored render — byte-identical to a
//!   fresh `replay_render` — without re-rendering;
//! * the key folds in the graph fingerprint and sink, so the same log
//!   through a different graph (or sink) is a separate entry, never a
//!   wrong render;
//! * the cache persists on disk and a fresh instance hits it;
//! * the log hash is sensitive to any command difference and stable across
//!   identical sessions.

use engine::prelude::{
    graph_fingerprint, log_hash, replay_events, replay_render, Aelog, AelogCache, AelogRecorder,
    EventPayload, EventTime, ExecutionOrder, Graph2, NodeId, PortId, TransportState,
};
use std::path::PathBuf;
use std::time::SystemTime;

const SR: f32 = 48_000.0;
const BLOCK: u64 = 128;

/// A deterministic 300-block recording session: a beat-1 gain gate, tempo,
/// and a chunk of audio input — exercises the full (log, graph) surface.
fn record_session() -> Aelog {
    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    rec.set_tempo(120.0);
    rec.schedule(
        EventTime::Beat(1.0),
        EventPayload::SetGain { node: 1, gain: 1.0 },
    )
    .unwrap();
    for i in 0..300usize {
        let chunk: Vec<f32> = (0..BLOCK as usize)
            .map(|k| (i * 128 + k) as f32 * 0.001)
            .collect();
        rec.record_audio_input(&chunk);
        rec.advance_block(BLOCK);
    }
    rec.finish()
}

/// Buffer → gain(0) → sink: the gain gate at beat 1 opens the recorded
/// audio input. Returns (graph, order, gain node id, sink node id).
fn gated_graph() -> (Graph2, ExecutionOrder, NodeId, NodeId) {
    let mut g = Graph2::new();
    let b = g.add_buffer("in", vec![], false);
    let gain = g.add_gain("gate", 0.0);
    let sink = g.add_sink("out");
    g.add_edge(b, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    (g, order, gain, sink)
}

fn temp_root(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "aelog-cache-fidelity-{tag}-{}",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    dir
}

#[test]
fn identical_logs_hash_identically_and_reuse_the_render() {
    let root = temp_root("reuse");
    let cache = AelogCache::new(&root);
    let a = record_session();
    let b = record_session();
    let (g, order, _gain, sink) = gated_graph();

    // Two identical sessions: one hash, one render.
    assert_eq!(log_hash(&a), log_hash(&b), "identical sessions hash equal");

    let cold = cache.render_cached(&a, &g, &order, sink).unwrap();
    assert!(
        !cold.captured.is_empty() && cold.captured.len() >= 24_000,
        "gate opens: capture spans the beat-1 sample"
    );
    assert!(cold.captured[..24_000].iter().all(|s| s.abs() < 1e-6));
    assert!(
        cold.captured[24_000..].iter().any(|s| s.abs() > 1e-6),
        "audio audible after the gate"
    );

    // The identical second session hits the cache: byte-identical capture
    // to the cold render and to a fresh, uncached replay_render.
    let warm = cache.render_cached(&b, &g, &order, sink).unwrap();
    let direct = replay_render(&b, &g, &order, sink).unwrap();
    assert_eq!(warm.captured, cold.captured, "warm == cold");
    assert_eq!(warm.captured, direct.captured, "warm == direct");
    assert_eq!(warm.fired.len(), direct.fired.len(), "same fired stream");

    // The entry is on disk under the log-hash key.
    let lh = log_hash(&b);
    let gh = graph_fingerprint(&g);
    let path = root.join(format!("{lh:016x}-{gh:016x}-{:08x}.json", sink.0));
    assert!(path.exists(), "entry file written");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cache_is_keyed_by_graph_and_sink_too() {
    let root = temp_root("graphkey");
    let cache = AelogCache::new(&root);
    let log = record_session();
    let (g, order, _gain, sink) = gated_graph();
    let cold = cache.render_cached(&log, &g, &order, sink).unwrap();

    // Same log, different graph: the cache must not leak the other
    // graph's capture. A passthrough graph (no gate) renders the track
    // immediately — clearly different audio.
    let mut pass = Graph2::new();
    let b = pass.add_buffer("in", vec![], false);
    let sink2 = pass.add_sink("out2");
    pass.add_edge(b, PortId::OUT, sink2, PortId::IN).unwrap();
    let order2 = pass.compile().unwrap().clone();
    let through = cache.render_cached(&log, &pass, &order2, sink2).unwrap();
    assert_ne!(through.captured, cold.captured, "different graphs differ");
    assert_ne!(
        graph_fingerprint(&pass),
        graph_fingerprint(&g),
        "graph fingerprints differ"
    );

    // Same log + graph, different sink: the sink id is part of the key.
    assert_eq!(
        cache.lookup(&log, &g, NodeId(999)),
        None,
        "unknown sink id → miss"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cache_persists_across_instances() {
    let root = temp_root("persist");
    let log = record_session();
    let (g, order, _gain, sink) = gated_graph();

    let cold = AelogCache::new(&root)
        .render_cached(&log, &g, &order, sink)
        .unwrap();
    let reloaded = AelogCache::new(&root)
        .render_cached(&log, &g, &order, sink)
        .unwrap();
    assert_eq!(
        reloaded.captured, cold.captured,
        "disk cache survives reload"
    );
    assert!(AelogCache::new(&root).lookup(&log, &g, sink).is_some());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn corrupt_entry_is_a_miss_not_a_wrong_render() {
    let root = temp_root("corrupt");
    let cache = AelogCache::new(&root);
    let log = record_session();
    let (g, _order, _gain, sink) = gated_graph();

    // Write a garbage file at the exact key path: lookup must miss and the
    // next render must produce the true capture, not garbage.
    let lh = log_hash(&log);
    let gh = graph_fingerprint(&g);
    let path = root.join(format!("{lh:016x}-{gh:016x}-{:08x}.json", sink.0));
    std::fs::write(&path, "{ definitely not json").unwrap();
    assert_eq!(cache.lookup(&log, &g, sink), None, "corrupt entry → miss");

    let (g2, order2, _gain2, sink2) = gated_graph();
    let rendered = cache.render_cached(&log, &g2, &order2, sink2).unwrap();
    assert!(
        rendered.captured[24_000..].iter().any(|s| s.abs() > 1e-6),
        "true render, not the corrupt stub"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hash_is_sensitive_to_any_session_difference() {
    let a = record_session();
    let mut b = record_session();
    b.commands
        .push(engine::prelude::RecordedCommand::Advance(BLOCK));
    assert_ne!(
        log_hash(&a),
        log_hash(&b),
        "one extra advance changes the hash"
    );

    let mut c = record_session();
    if let engine::prelude::RecordedCommand::Schedule { payload, .. } = &mut c.commands[2] {
        *payload = EventPayload::SetGain { node: 1, gain: 0.5 };
    }
    assert_ne!(
        log_hash(&a),
        log_hash(&c),
        "a changed schedule changes the hash"
    );

    // And replay_events stays a pure function of the log regardless of any
    // cache state.
    let (g, order, _gain, sink) = gated_graph();
    let root = temp_root("purity");
    let cached = AelogCache::new(&root)
        .render_cached(&a, &g, &order, sink)
        .unwrap();
    assert_eq!(cached.fired.len(), replay_events(&a).unwrap().fired.len());

    let _ = std::fs::remove_dir_all(&root);
}
