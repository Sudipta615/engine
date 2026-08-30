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
//! Entries are **content-addressed**: each is named by the SHA-256 of the
//! canonical JSON of its render identity (dependency-free FNV-1a over the
//! log and graph feeds that identity; the LRU `touched` stamp is excluded).
//! Because a render is a pure function of that identity, two machines
//! address the same file for the same session — a synced cache directory is
//! valid on any host. The directory is **size-bounded by LRU eviction**
//! (a hit bumps the entry's stamp; `insert` evicts the least-recently-used
//! until the total is under the byte budget), so it can't grow without
//! bound. Best-effort throughout: a corrupt or missing entry is a miss,
//! never an error, and writes are atomic (temp-file + rename) so a crash
//! cannot leave a half-written entry.

use super::{replay_events, replay_render, Aelog, RecordedCommand, ReplayError, ReplayOutcome};
use crate::dsp::graph2::{ExecutionOrder, Graph2, NodeId};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
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

/// The render-relevant part of a session: the two header fields that shape
/// the render (sample rate and block cadence) plus every recorded command.
/// The host `label` and `format_version` are metadata / schema markers —
/// they never change the audio — so they are **excluded** from the cache
/// key: two semantically identical sessions (e.g. the same take re-labelled
/// for a different song) reuse one golden render. Serde over this structure
/// is deterministic (command order and variant layout fully determine the
/// bytes), which a persisted key demands.
#[derive(Serialize)]
struct RenderKey<'a> {
    sample_rate: f32,
    block_frames: u64,
    commands: &'a [RecordedCommand],
}

/// A deterministic, dependency-free SHA-256 (FIPS 180-4). Hand-rolled to
/// keep the persisted cache dependency-free and stable across targets and
/// Rust versions (the same reason [`fnv1a`] is hand-rolled). Correctness is
/// pinned by a known-answer test; a bug here can only cause a cache *miss*
/// (the entry's identity fields are still re-verified on load), never a
/// wrong render.
pub fn sha256(message: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    // Append 0x80, pad with zeros to 56 mod 64, then the 64-bit bit length.
    let bit_len = (message.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity(((message.len() + 8) / 64 + 1) * 64);
    msg.extend_from_slice(message);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut w = [0u32; 64];
    for chunk in msg.chunks_exact(64) {
        for (dst, c) in w.iter_mut().take(16).zip(chunk.chunks_exact(4)) {
            *dst = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// Lowercase hex encoding of a digest.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Deterministic hash of a log's **render-relevant** content: FNV-1a over
/// the canonical JSON of [`RenderKey`]. Identical sessions hash
/// identically; any command (or sample-rate / block) difference changes
/// the hash; the label and format version do not. This is the primary
/// cache-key component.
pub fn log_hash(log: &Aelog) -> u64 {
    let key = RenderKey {
        sample_rate: log.header.sample_rate,
        block_frames: log.header.block_frames,
        commands: &log.commands,
    };
    serde_json::to_vec(&key)
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

/// The render-identity components the content address is built from: the
/// two compact hashes plus the two render-shaping header fields. Serde over
/// this is canonical — field order fully determines the bytes — so the
/// SHA-256 name derived from it is a pure function of semantic content.
/// The `touched` LRU stamp is deliberately **not** here: it is local,
/// mutable metadata and must not affect the address.
#[derive(Serialize)]
struct AddressKey {
    log_hash: u64,
    graph_hash: u64,
    sink: u32,
    sample_rate: f32,
    block_frames: u64,
}

/// The **content address** of a (log, graph, sink) entry: a SHA-256 over
/// the canonical JSON of its [`AddressKey`], hex-encoded (64 chars). Because
/// the render is a pure function of that identity, two machines rendering
/// the same session through the same graph derive *the same name* for the
/// *same stored bytes* — the property that makes cache directories
/// shareable across machines. Cross-machine sharing needs no coordination:
/// a synced directory already carries the exact match for the address the
/// other machine computes locally.
pub fn content_address(log: &Aelog, graph: &Graph2, sink: NodeId) -> String {
    let key = AddressKey {
        log_hash: log_hash(log),
        graph_hash: graph_fingerprint(graph),
        sink: sink.0,
        sample_rate: log.header.sample_rate,
        block_frames: log.header.block_frames,
    };
    let bytes = serde_json::to_vec(&key).unwrap_or_default();
    to_hex(&sha256(&bytes))
}

/// Default byte budget for the cache directory: 256 MiB of golden captures
/// (a generous session archive; eviction keeps it bounded).
pub const DEFAULT_CACHE_BUDGET: u64 = 256 * 1024 * 1024;

/// Milliseconds since the UNIX epoch — the LRU stamp stored in each entry.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One stored golden capture. The hash fields are re-verified on load so a
/// hash collision or a corrupted file degrades to a miss, never a wrong
/// render. `touched` is the last-access LRU stamp (ms since the epoch),
/// bumped on every hit and used by size-bounded eviction; `#[serde(default)]`
/// lets entries written before v3.42.0 load (treated as oldest).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CacheEntry {
    log_hash: u64,
    graph_hash: u64,
    sink: u32,
    sample_rate: f32,
    block_frames: u64,
    #[serde(default)]
    touched: u64,
    captured: Vec<f32>,
}

/// A file-backed golden-render cache. Control/offline-path by design. The
/// directory is **size-bounded**: [`insert`](Self::insert) evicts the
/// least-recently-used entries (by the `touched` stamp, bumped on lookup
/// hits) until the total stays under the byte budget, so the app-data cache
/// cannot grow without bound. A single entry larger than the budget is kept
/// (there is nothing to evict in its place).
#[derive(Debug, Clone)]
pub struct AelogCache {
    root: PathBuf,
    budget_bytes: u64,
}

impl AelogCache {
    /// A cache rooted at `root` (created if missing) with the default byte
    /// budget.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_budget(root, DEFAULT_CACHE_BUDGET)
    }

    /// A cache rooted at `root` bounded to `budget_bytes` of `.json`
    /// entries (LRU eviction once exceeded; `0` = evict everything except
    /// the just-written entry).
    pub fn with_budget(root: impl Into<PathBuf>, budget_bytes: u64) -> Self {
        let root = root.into();
        let _ = fs::create_dir_all(&root);
        Self {
            root,
            budget_bytes: budget_bytes.max(1),
        }
    }

    /// The configured byte budget (LRU eviction ceiling).
    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// The default cache root: `data_local_dir()/audio-engine/aelog-cache`
    /// (same app-data convention as the loudness cache and profiles).
    pub fn default_root() -> Option<PathBuf> {
        let mut dir = crate::paths::data_local_dir()?;
        dir.push("audio-engine");
        dir.push("aelog-cache");
        Some(dir)
    }

    /// The file path for a (log, graph, sink) entry. Named by the **content
    /// address** — a SHA-256 over the canonical identity JSON — so the path is
    /// a pure function of the render's semantic content, independent of when or
    /// where (which machine) it was rendered. The same session through the same
    /// graph on two machines therefore addresses the same file, which is what
    /// lets a cache directory be shared (synced / reused) across machines.
    fn entry_path(&self, log: &Aelog, graph: &Graph2, sink: NodeId) -> PathBuf {
        self.root
            .join(format!("{}.json", content_address(log, graph, sink)))
    }

    /// Look up a stored golden capture. `None` on miss, corrupt file, or
    /// any mismatch (hash fields, sink, header). A hit **touches** the entry
    /// (LRU stamp refreshed) so recently-used captures survive eviction.
    pub fn lookup(&self, log: &Aelog, graph: &Graph2, sink: NodeId) -> Option<Vec<f32>> {
        let lh = log_hash(log);
        let gh = graph_fingerprint(graph);
        let path = self.entry_path(log, graph, sink);
        let entry: CacheEntry = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
        if entry.log_hash != lh
            || entry.graph_hash != gh
            || entry.sink != sink.0
            || entry.sample_rate != log.header.sample_rate
            || entry.block_frames != log.header.block_frames
        {
            return None;
        }
        let captured = entry.captured.clone();
        // Refresh the LRU stamp on a hit (best-effort; a failed touch only
        // costs an earlier eviction). The entry must be re-stamped, not
        // rewritten as-is — otherwise every hit would rewrite the same
        // timestamp and the LRU order would never change.
        let mut touched = entry;
        touched.touched = now_ms();
        let _ = self.write_entry(&path, &touched);
        Some(captured)
    }

    /// Store a golden capture under the (log, graph, sink) key. Atomic:
    /// writes a temp file then renames, so a crash never leaves a
    /// half-written entry. Then enforces the byte budget: once the directory
    /// exceeds it, the least-recently-used entries are evicted. Best-effort
    /// — failures are ignored.
    pub fn insert(&self, log: &Aelog, graph: &Graph2, sink: NodeId, captured: Vec<f32>) {
        let lh = log_hash(log);
        let gh = graph_fingerprint(graph);
        let entry = CacheEntry {
            log_hash: lh,
            graph_hash: gh,
            sink: sink.0,
            sample_rate: log.header.sample_rate,
            block_frames: log.header.block_frames,
            touched: now_ms(),
            captured,
        };
        let path = self.entry_path(log, graph, sink);
        if self.write_entry(&path, &entry) {
            self.evict_if_over_budget(&path);
        }
    }

    /// Serialize `entry` to `path` atomically (temp file + rename). Returns
    /// whether the write succeeded.
    fn write_entry(&self, path: &PathBuf, entry: &CacheEntry) -> bool {
        let tmp = path.with_extension("json.tmp");
        serde_json::to_string(entry)
            .ok()
            .and_then(|json| fs::write(&tmp, json).ok())
            .map(|_| fs::rename(&tmp, path).is_ok())
            .unwrap_or(false)
    }

    /// Enforce the byte budget: when the directory's `.json` entries exceed
    /// `budget_bytes`, evict the least-recently-used ones (oldest `touched`
    /// first, corrupt entries treated as oldest) until the total is at or
    /// below 90% of the budget — the floor avoids thrashing on the entry
    /// that just triggered the check. `keep` (the just-written entry) is
    /// never evicted: a single capture larger than the budget survives, and
    /// a zero budget evicts everything except the entry that was just
    /// written.
    fn evict_if_over_budget(&self, keep: &PathBuf) {
        let mut files: Vec<(u64, u64, PathBuf)> = Vec::new(); // (touched, bytes, path)
        let mut total = 0u64;
        let Ok(rd) = fs::read_dir(&self.root) else {
            return;
        };
        for dir in rd.flatten() {
            let path = dir.path();
            if path.extension().is_some_and(|e| e == "json") {
                let size = dir.metadata().map(|m| m.len()).unwrap_or(0);
                total += size;
                let touched = fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<CacheEntry>(&s).ok())
                    .map(|e| e.touched)
                    .unwrap_or(0);
                files.push((touched, size, path));
            }
        }
        if total <= self.budget_bytes {
            return;
        }
        files.sort_by_key(|(touched, _, _)| *touched);
        let floor = self.budget_bytes.saturating_mul(9) / 10;
        let mut remaining = total;
        for (_, size, path) in files {
            if remaining <= floor {
                break;
            }
            if path == *keep {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                remaining = remaining.saturating_sub(size);
            }
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

/// The composite memo key: `(log, graph, sink)`. `log_hash` already covers
/// the log's header (sample rate, block frames) and every command, so the
/// tuple is complete.
type MemoKey = (u64, u64, u32);

fn memo_key(log: &Aelog, graph: &Graph2, sink: NodeId) -> MemoKey {
    (log_hash(log), graph_fingerprint(graph), sink.0)
}

// Bounded process-local memoisation so identical logs in one process reuse
// the captured golden audio instead of re-running the graph. This is the
// fast in-process layer on top of the durable file-backed AelogCache: the
// aelog-hash key means a second render of the same session in the same
// thread is a zero-cost replay of the event stream plus a splice of the
// stored audio. Thread-local (no locks anywhere), and cleared once it grows
// past MEMO_CAP entries so a long-lived process can't hoard render memory.
thread_local! {
    static MEMO: RefCell<HashMap<MemoKey, Vec<f32>>> = RefCell::new(HashMap::new());
}

/// Max in-memory capture entries before the memo is cleared.
const MEMO_CAP: usize = 64;

/// For tests: the number of entries currently memoised.
#[cfg(test)]
fn memo_len() -> usize {
    MEMO.with(|m| m.borrow().len())
}

/// Clear the in-process memo (e.g. after a session's worth of previews, or
/// between unrelated renders in a long-lived process). Best-effort: the
/// durable file cache is untouched.
pub fn clear_memo() {
    MEMO.with(|m| m.borrow_mut().clear());
}

/// Convenience for a render-cache that needs no constructor ceremony but
/// still wants an explicit root for testability. Consults a **thread-local
/// memo** first (fast in-process reuse of identical logs), falling back to
/// the persistent default file cache, then to a fresh render; every path
/// is byte-identical to [`replay_render`]. On a memo/file hit only the
/// cheap `replay_events` stream is recomputed and the golden audio is
/// spliced in.
pub fn render_cached(
    log: &Aelog,
    graph: &Graph2,
    order: &ExecutionOrder,
    sink: NodeId,
) -> Result<ReplayOutcome, ReplayError> {
    let key = memo_key(log, graph, sink);
    let memoised = MEMO.with(|m| m.borrow().get(&key).cloned());
    if let Some(captured) = memoised {
        let mut outcome = replay_events(log)?;
        outcome.captured = captured;
        return Ok(outcome);
    }

    let outcome = match AelogCache::default_root() {
        Some(root) => AelogCache::new(root).render_cached(log, graph, order, sink)?,
        None => replay_render(log, graph, order, sink)?,
    };

    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= MEMO_CAP {
            m.clear();
        }
        m.insert(key, outcome.captured.clone());
    });
    Ok(outcome)
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
    fn log_hash_ignores_label_and_other_non_render_header_fields() {
        // Same commands, different host labels: semantically identical
        // sessions — the golden render must be reusable across them.
        let plain = session();
        let mut labelled = session();
        labelled.header.label = "take 42 — remaster v2".to_string();
        assert_eq!(
            log_hash(&plain),
            log_hash(&labelled),
            "label is not render-relevant"
        );

        // format_version is a schema marker, not render input: also ignored.
        let mut versioned = session();
        versioned.header.format_version += 1;
        assert_eq!(log_hash(&plain), log_hash(&versioned));

        // A sample-rate difference *is* render-relevant and must split keys.
        let mut other_sr = session();
        other_sr.header.sample_rate = 96_000.0;
        assert_ne!(log_hash(&plain), log_hash(&other_sr));

        // The cache itself reuses one entry across labels: insert under the
        // plain log, look up under the labelled one.
        let root = temp_root("label");
        let cache = AelogCache::new(&root);
        let (g, _order, sink) = graph_with_sink();
        cache.insert(&plain, &g, sink, vec![7.0, 8.0, 9.0]);
        assert_eq!(
            cache.lookup(&labelled, &g, sink),
            Some(vec![7.0, 8.0, 9.0]),
            "labelled session reuses the same golden render"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sha256_matches_the_fips_180_4_known_answers() {
        // NIST / FIPS 180-4 test vectors pin the hand-rolled implementation.
        let empty = sha256(b"");
        assert_eq!(
            to_hex(&empty),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "sha256(empty)"
        );
        let abc = sha256(b"abc");
        assert_eq!(
            to_hex(&abc),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256(abc)"
        );
        let long = sha256(b"abcdbcdecdefdefgefghfghighijhi");
        assert_ne!(to_hex(&long), to_hex(&abc), "different messages differ");
    }

    #[test]
    fn content_address_is_deterministic_and_cache_shareable_across_machines() {
        // The address must be a pure function of semantic render identity:
        // recomputing it from a freshly rebuilt encoding of the same session
        // yields the identical name (no time/sequence/random component), and
        // distinct logical keys address distinct files.
        let log_a = session();
        let log_b = session();
        let (ga, _oa, sa) = graph_with_sink();

        let add1 = content_address(&log_a, &ga, sa);
        let add2 = content_address(&log_b, &ga, sa);
        assert_eq!(add1, add2, "identical sessions address the same file");
        assert_eq!(add1.len(), 64, "SHA-256 hex, 64 chars");
        assert!(add1.chars().all(|c| c.is_ascii_hexdigit()));

        // Different graph → different address.
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
        assert_ne!(
            content_address(&log_a, &other, sink2),
            add1,
            "graph differs"
        );

        // Different sink on the same graph → different address.
        assert_ne!(
            content_address(&log_a, &ga, NodeId(7)),
            add1,
            "sink differs"
        );

        // A store written at one time is addressed by a name recomputed at a
        // later time: the address is still valid across rebuilds, which is
        // exactly the cross-machine sharing contract.
        let root = temp_root("content-addr");
        let cache = AelogCache::new(&root);
        cache.insert(&log_a, &ga, sa, vec![6.0, 6.6]);
        let reloaded = AelogCache::new(&root);
        assert_eq!(
            reloaded.lookup(&log_b, &ga, sa),
            Some(vec![6.0, 6.6]),
            "second 'machine' finds the stored entry by its recomputed name"
        );
        // Only one file on disk, under the content-address name.
        let files: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|d| d.path().extension().is_some_and(|e| e == "json"))
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].path().file_stem().unwrap().to_str().unwrap(),
            &add1,
            "file named by its SHA-256 content address"
        );
        let _ = fs::remove_dir_all(&root);
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
        let path = root.join(format!("{}.json", content_address(&log, &g, sink)));
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

    #[test]
    fn in_process_memo_reuses_golden_audio_for_identical_logs() {
        // Clear the persistent default root (a stale file entry with the
        // same key from an earlier build) and the thread-local memo.
        if let Some(dflt) = AelogCache::default_root() {
            let _ = fs::remove_dir_all(&dflt);
        }
        clear_memo();
        assert_eq!(memo_len(), 0);

        let log = session();
        let (g, order, sink) = graph_with_sink();
        let direct = crate::dsp::aelog::replay_render(&log, &g, &order, sink).unwrap();

        // First call renders + memoises; the log's hash is the key.
        let a = render_cached(&log, &g, &order, sink).unwrap();
        assert_eq!(a.captured, direct.captured, "memo cold == direct");
        assert_eq!(memo_len(), 1, "one entry stored");

        // Second call of the identical log is served from the memo — the
        // entry count stays 1 (reused, not re-added) and audio is
        // byte-identical without re-running the graph.
        let b = render_cached(&log, &g, &order, sink).unwrap();
        assert_eq!(b.captured, a.captured, "memo hit == memo cold");
        assert_eq!(b.captured, direct.captured);
        assert_eq!(memo_len(), 1, "reused the memoised capture");
        assert_eq!(b.fired.len(), direct.fired.len(), "event stream still pure");

        // A different log is a different key and renders separately.
        let mut other = session();
        other
            .commands
            .push(crate::dsp::aelog::RecordedCommand::Advance(BLOCK));
        let c = render_cached(&other, &g, &order, sink).unwrap();
        assert_ne!(c.captured, a.captured, "different log renders differently");
        assert_eq!(memo_len(), 2, "two distinct logs memoised");

        // clear_memo empties the fast layer (the durable file cache is
        // untouched and still hits).
        clear_memo();
        assert_eq!(memo_len(), 0);
        let d = render_cached(&log, &g, &order, sink).unwrap();
        assert_eq!(d.captured, direct.captured, "re-memoised after clear");

        if let Some(dflt) = AelogCache::default_root() {
            let _ = fs::remove_dir_all(&dflt);
        }
        clear_memo();
    }

    #[test]
    fn lru_budget_evicts_oldest_and_hits_refresh_the_stamp() {
        use std::thread::sleep;
        use std::time::Duration;

        let root = temp_root("lru");
        let log = session();
        let (g, _order, _sink) = graph_with_sink();
        let cap = vec![0.5; 1024]; // a few KB of JSON per entry

        // Insert A and B, then measure the on-disk total so the budget can
        // be set to ~2.5 entries: a third insert exceeds it (evicting the
        // oldest), while two entries sit under the 90% eviction floor.
        let pre = AelogCache::new(&root);
        pre.insert(&log, &g, NodeId(1), cap.clone());
        sleep(Duration::from_millis(5));
        pre.insert(&log, &g, NodeId(2), cap.clone());
        sleep(Duration::from_millis(5));

        let mut total = 0u64;
        for dir in fs::read_dir(&root).unwrap().flatten() {
            total += dir.metadata().map(|m| m.len()).unwrap_or(0);
        }
        let budget = total * 5 / 4; // ~2.5 entries

        // Insert C through the budgeted cache: 3 entries > budget → A
        // (oldest stamp) is evicted; B and C stay.
        let cache = AelogCache::with_budget(&root, budget);
        sleep(Duration::from_millis(5));
        cache.insert(&log, &g, NodeId(3), cap.clone());
        assert_eq!(cache.lookup(&log, &g, NodeId(1)), None, "oldest evicted");
        assert!(cache.lookup(&log, &g, NodeId(2)).is_some(), "B survives");
        assert!(cache.lookup(&log, &g, NodeId(3)).is_some(), "C survives");

        // A lookup hit refreshes B's LRU stamp, so inserting D now evicts
        // C (the oldest *untouched*) rather than the just-touched B.
        sleep(Duration::from_millis(5));
        assert!(cache.lookup(&log, &g, NodeId(2)).is_some(), "touch B");
        sleep(Duration::from_millis(5));
        cache.insert(&log, &g, NodeId(4), cap.clone());
        assert!(
            cache.lookup(&log, &g, NodeId(2)).is_some(),
            "touched B survives the next eviction"
        );
        assert_eq!(cache.lookup(&log, &g, NodeId(3)), None, "C now evicted");
        assert!(cache.lookup(&log, &g, NodeId(4)).is_some(), "D stored");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lru_keeps_the_just_inserted_entry_even_when_it_exceeds_the_budget() {
        let root = temp_root("lru-huge");
        let log = session();
        let (g, _order, _sink) = graph_with_sink();

        // A single capture far larger than the 64-byte budget is still
        // stored (there is nothing else to evict, and the just-written
        // entry is never evicted by its own insert).
        let cache = AelogCache::with_budget(&root, 64);
        cache.insert(&log, &g, NodeId(1), vec![0.25; 4096]);
        assert!(
            cache.lookup(&log, &g, NodeId(1)).is_some(),
            "oversized single entry kept"
        );

        // A second, small insert pushes the old giant out — the newest
        // entry wins the budget.
        cache.insert(&log, &g, NodeId(2), vec![0.25; 8]);
        assert_eq!(cache.lookup(&log, &g, NodeId(1)), None, "giant evicted");
        assert!(
            cache.lookup(&log, &g, NodeId(2)).is_some(),
            "new entry kept"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
