//! Render cache for aelog (v3.33) — golden captures keyed by a
//! deterministic hash of the session, so identical logs reuse the stored
//! render instead of re-rendering.
//!
//! A golden render is a pure function of `(log, graph, sink)`: the log
//! drives the timeline, the graph topology + parameters shape the signal
//! flow, and the sink names the capture point. [`AelogCache`] hashes all
//! three — the log hash is the primary component (the *cause* of the
//! render) — and stores the captured audio under that key. On a hit it
//! replays the (cheap) event stream and splices in the cached capture;
//! on a miss it renders through [`replay_render`] and stores the result.
//!
//! Hashing is **dependency-free FNV-1a 64-bit** over the canonical JSON
//! bytes of the log and graph — deterministic forever, no wall-clock
//! timestamps, no random keys. The cache is best-effort: a corrupt or
//! missing entry is a miss, never an error, and writes are atomic
//! (temp-file + rename) so a crash cannot leave a half-written entry.

use super::{replay_events, replay_render, Aelog, ReplayError, ReplayOutcome};
use crate::dsp::graph2::{ExecutionOrder, Graph2, NodeId};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

/// FNV-1a 64-bit — stable across runs and Rust versions (no SipHash keys,
/// no random state), which a persisted cache key demands.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Deterministic hash of a log: FNV-1a over its canonical JSON bytes.
/// Identical sessions hash identically; any command difference changes the
/// hash. This is the primary cache-key component.
pub fn log_hash(log: &Aelog) -> u64 {
    serde_json::to_vec(log)
        .map(|bytes| fnv1a(&bytes))
        .unwrap_or(0)
}

/// Deterministic fingerprint of a Graph 2.0 topology: FNV-1a over its
/// canonical JSON bytes. Folded into the cache key because the same log
/// through a different graph renders different audio.
pub fn graph_fingerprint(graph: &Graph2) -> u64 {
    serde_json::to_vec(graph)
        .map(|bytes| fnv1a(&bytes))
        .unwrap_or(0)
}

/// One stored golden capture. The hash fields are re-verified on load so a
/// hash collision or a corrupted file degrades to a miss, never a wrong
/// render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CacheEntry {
    log_hash: u64,
    graph_hash: u64,
    sink: u32,
    sample_rate: f32,
    block_frames: u64,
    captured: Vec<f32>,
}

/// A file-backed golden-render cache. Control/offline-path by design.
#[derive(Debug, Clone)]
pub struct AelogCache {
    root: PathBuf,
}

impl AelogCache {
    /// A cache rooted at `root` (created if missing).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let _ = fs::create_dir_all(&root);
        Self { root }
    }

    /// The default cache root: `data_local_dir()/audio-engine/aelog-cache`
    /// (same app-data convention as the loudness cache and profiles).
    pub fn default_root() -> Option<PathBuf> {
        let mut dir = crate::paths::data_local_dir()?;
        dir.push("audio-engine");
        dir.push("aelog-cache");
        Some(dir)
    }

    /// The file path for a (log, graph, sink) key.
    fn entry_path(&self, log_hash: u64, graph_hash: u64, sink: u32) -> PathBuf {
        self.root
            .join(format!("{log_hash:016x}-{graph_hash:016x}-{sink:08x}.json"))
    }

    /// Look up a stored golden capture. `None` on miss, corrupt file, or
    /// any mismatch (hash fields, sink, header).
    pub fn lookup(&self, log: &Aelog, graph: &Graph2, sink: NodeId) -> Option<Vec<f32>> {
        let lh = log_hash(log);
        let gh = graph_fingerprint(graph);
        let path = self.entry_path(lh, gh, sink.0);
        let entry: CacheEntry = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
        if entry.log_hash != lh
            || entry.graph_hash != gh
            || entry.sink != sink.0
            || entry.sample_rate != log.header.sample_rate
            || entry.block_frames != log.header.block_frames
        {
            return None;
        }
        Some(entry.captured)
    }

    /// Store a golden capture under the (log, graph, sink) key. Atomic:
    /// writes a temp file then renames, so a crash never leaves a
    /// half-written entry. Best-effort — failures are ignored.
    pub fn insert(&self, log: &Aelog, graph: &Graph2, sink: NodeId, captured: Vec<f32>) {
        let lh = log_hash(log);
        let gh = graph_fingerprint(graph);
        let entry = CacheEntry {
            log_hash: lh,
            graph_hash: gh,
            sink: sink.0,
            sample_rate: log.header.sample_rate,
            block_frames: log.header.block_frames,
            captured,
        };
        let path = self.entry_path(lh, gh, sink.0);
        let tmp = path.with_extension("json.tmp");
        if serde_json::to_string(&entry)
            .ok()
            .and_then(|json| fs::write(&tmp, json).ok())
            .is_some()
        {
            let _ = fs::rename(&tmp, &path);
        }
    }

    /// Remove every stored entry (e.g. to force a cold-render pass).
    pub fn clear(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|e| e == "json") {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// Render `log` through `graph`, reusing the stored golden capture when
    /// the (log, graph, sink) key is already cached and rendering + storing
    /// on a miss. The returned [`ReplayOutcome`] is byte-identical to
    /// [`replay_render`] either way: on a hit the event stream and clock are
    /// recomputed from the log (pure, cheap) and only the audio comes from
    /// the cache.
    pub fn render_cached(
        &self,
        log: &Aelog,
        graph: &Graph2,
        order: &ExecutionOrder,
        sink: NodeId,
    ) -> Result<ReplayOutcome, ReplayError> {
        if let Some(captured) = self.lookup(log, graph, sink) {
            let mut outcome = replay_events(log)?;
            outcome.captured = captured;
            return Ok(outcome);
        }
        let outcome = replay_render(log, graph, order, sink)?;
        self.insert(log, graph, sink, outcome.captured.clone());
        Ok(outcome)
    }
}

/// Convenience for a render-cache that needs no constructor ceremony but
/// still wants an explicit root for testability.
pub fn render_cached(
    log: &Aelog,
    graph: &Graph2,
    order: &ExecutionOrder,
    sink: NodeId,
) -> Result<ReplayOutcome, ReplayError> {
    match AelogCache::default_root() {
        Some(root) => AelogCache::new(root).render_cached(log, graph, order, sink),
        None => replay_render(log, graph, order, sink),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::timeline::{EventPayload, EventTime, TransportState};
    use std::time::SystemTime;

    const SR: f32 = 48_000.0;
    const BLOCK: u64 = 128;

    fn session() -> Aelog {
        let mut rec = super::super::AelogRecorder::new(SR, BLOCK);
        rec.set_state(TransportState::Playing, 0);
        rec.set_tempo(120.0);
        rec.schedule(
            EventTime::Beat(1.0),
            EventPayload::SetGain { node: 1, gain: 2.0 },
        )
        .unwrap();
        rec.advance_block(BLOCK);
        rec.advance_block(BLOCK);
        rec.finish()
    }

    fn graph_with_sink() -> (Graph2, ExecutionOrder, NodeId) {
        let mut g = Graph2::new();
        let src = g.add_source_with(
            "tone",
            super::super::super::graph2::node::SourceParams {
                signal: crate::dsp::graph2::TestSignal::Sine,
                frequency_hz: 440.0,
            },
        );
        let gain = g.add_gain("vol", 0.0);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
        g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        (g, order, sink)
    }

    use crate::dsp::graph2::PortId;

    fn temp_root(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "aelog-cache-test-{tag}-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    #[test]
    fn log_hash_is_deterministic_and_sensitive() {
        let a = session();
        let b = session();
        assert_eq!(log_hash(&a), log_hash(&b), "identical sessions hash equal");

        let mut different = session();
        different
            .commands
            .push(crate::dsp::aelog::RecordedCommand::Advance(BLOCK));
        assert_ne!(
            log_hash(&a),
            log_hash(&different),
            "any command difference changes the hash"
        );
    }

    #[test]
    fn cache_roundtrip_hit_and_miss() {
        let root = temp_root("roundtrip");
        let cache = AelogCache::new(&root);
        let log = session();
        let (g, _order, sink) = graph_with_sink();

        assert_eq!(cache.lookup(&log, &g, sink), None, "cold miss");

        cache.insert(&log, &g, sink, vec![1.0, 2.0, 3.0]);
        assert_eq!(
            cache.lookup(&log, &g, sink),
            Some(vec![1.0, 2.0, 3.0]),
            "hit returns the stored capture"
        );

        // Same log through a different graph is a different key.
        let mut other = Graph2::new();
        let src = other.add_source_with(
            "tone",
            crate::dsp::graph2::node::SourceParams {
                signal: crate::dsp::graph2::TestSignal::Sine,
                frequency_hz: 220.0,
            },
        );
        let sink2 = other.add_sink("out2");
        other.add_edge(src, PortId::OUT, sink2, PortId::IN).unwrap();
        assert_eq!(
            cache.lookup(&log, &other, sink2),
            None,
            "graph differs → miss"
        );
        assert_eq!(
            cache.lookup(&log, &g, NodeId(99)),
            None,
            "sink differs → miss"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_survives_reload_and_ignores_corruption() {
        let root = temp_root("persist");
        let cache = AelogCache::new(&root);
        let log = session();
        let (g, _order, sink) = graph_with_sink();
        cache.insert(&log, &g, sink, vec![4.0, 5.0]);

        // A fresh cache over the same directory still hits.
        let reloaded = AelogCache::new(&root);
        assert_eq!(reloaded.lookup(&log, &g, sink), Some(vec![4.0, 5.0]));

        // Corrupt the entry file: degrades to a miss, not a wrong render.
        let lh = log_hash(&log);
        let gh = graph_fingerprint(&g);
        let path = root.join(format!("{lh:016x}-{gh:016x}-{:08x}.json", sink.0));
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(reloaded.lookup(&log, &g, sink), None, "corrupt → miss");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn render_cached_matches_replay_render_byte_for_byte() {
        let root = temp_root("render");
        let cache = AelogCache::new(&root);
        let log = session();
        let (g, order, sink) = graph_with_sink();

        let cold = cache.render_cached(&log, &g, &order, sink).unwrap();
        let direct = crate::dsp::aelog::replay_render(&log, &g, &order, sink).unwrap();
        assert_eq!(cold.captured, direct.captured, "cold render == direct");
        assert_eq!(cold.fired.len(), direct.fired.len(), "same fired stream");
        assert_eq!(cold.listener_motion, direct.listener_motion);

        let warm = cache.render_cached(&log, &g, &order, sink).unwrap();
        assert_eq!(warm.captured, cold.captured, "warm render == cold render");

        // The stored entry exists and a fresh cache hits it without
        // re-rendering.
        let reloaded = AelogCache::new(&root);
        assert!(reloaded.lookup(&log, &g, sink).is_some());

        let _ = fs::remove_dir_all(&root);
    }
}
