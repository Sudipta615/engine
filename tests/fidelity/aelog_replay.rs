//! Fidelity tests — Aelog deterministic recording & replay (v3.29, roadmap
//! v3.29).
//!
//! Evolution thresholds (`docs/EVOLUTION.md` Phase 27):
//! * a recorded session replayed against the same Graph 2.0 graph produces
//!   **byte-identical captured audio** (the golden render) and an
//!   **identical fired-event stream**;
//! * the log round-trips through JSON (string and file) to an identical
//!   replay — it is a pure function of its commands;
//! * transport and looping are recorded faithfully (pause/resume, loop
//!   wrap) and replay to the same end clock state;
//! * two identical recording sessions serialize to byte-equal logs;
//! * the sample-accurate gate (beat-1 at 24 000) survives the whole
//!   record → replay pipeline.

use engine::prelude::{
    replay_events, replay_render, Aelog, AelogRecorder, EventPayload, EventTime, ExecutionOrder,
    Graph2, NodeId, OfflineExecutor, PortId, SourceParams, TestSignal, TransportState,
};

const SR: f32 = 48_000.0;
const BLOCK: u64 = 128;

fn gated_sine() -> (Graph2, ExecutionOrder, NodeId, NodeId) {
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 440.0,
        },
    );
    let gain = g.add_gain("vol", 0.0);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    (g, order, gain, sink)
}

fn raw440(i: usize) -> f32 {
    (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SR).sin()
}

/// Drive a recorder + executor like a live session, applying SetGain events
/// sample-accurately, and return (fired events, final capture).
fn drive(
    rec: &mut AelogRecorder,
    ex: &mut OfflineExecutor,
    sink: NodeId,
    blocks: usize,
) -> (Vec<engine::prelude::ScheduledEvent>, Vec<f32>) {
    let mut fired = Vec::new();
    for _ in 0..blocks {
        for e in rec.advance_block(BLOCK) {
            if let EventPayload::SetGain { node, gain } = e.payload {
                ex.set_gain_step(NodeId(node), gain, e.local_index(BLOCK))
                    .unwrap();
            }
            fired.push(e);
        }
        if rec.clock().is_playing() {
            ex.process_block().unwrap();
        }
    }
    let cap = ex.capture(sink).map(|v| v.to_vec()).unwrap_or_default();
    (fired, cap)
}

fn record_gate_session() -> (
    Aelog,
    Vec<engine::prelude::ScheduledEvent>,
    Vec<f32>,
    Graph2,
    ExecutionOrder,
    NodeId,
) {
    let (g, order, gain, sink) = gated_sine();
    let mut rec = AelogRecorder::new(SR, BLOCK).with_label("golden");
    rec.set_tempo(120.0);
    rec.set_state(TransportState::Playing, 0);
    rec.schedule(
        EventTime::Beat(1.0),
        EventPayload::SetGain {
            node: gain.0,
            gain: 2.0,
        },
    )
    .unwrap();
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK as usize, SR).unwrap();
    let (fired, cap) = drive(&mut rec, &mut ex, sink, 200);
    let log = rec.finish();
    (log, fired, cap, g, order, sink)
}

#[test]
fn replay_reproduces_golden_render_byte_identically() {
    let (log, fired_live, cap_live, g, order, sink) = record_gate_session();

    // Event-stream replay: identical fired events + end clock state.
    let ev = replay_events(&log).unwrap();
    assert_eq!(ev.fired, fired_live, "identical fired stream");
    assert_eq!(ev.timeline.clock().master_position(), 25_600);
    assert!((ev.timeline.clock().tempo_bpm() - 120.0).abs() < 1e-6);

    // Full render replay: byte-identical captured audio (golden render).
    let rt = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(rt.captured, cap_live, "golden render byte-identical");
    assert_eq!(rt.fired, fired_live);

    // And the recorded sample-accuracy survives the pipeline.
    assert!(
        cap_live[..24_000].iter().all(|s| s.abs() < 1e-6),
        "gated before beat"
    );
    assert!(
        (cap_live[24_000] - 2.0 * raw440(24_000)).abs() < 1e-3,
        "sample-exact gate at 24000"
    );
}

#[test]
fn json_roundtrip_replays_identically() {
    let (log, _fired, _cap, g, order, sink) = record_gate_session();

    let json = log.to_json().unwrap();
    let log2 = Aelog::from_json(&json).unwrap();
    assert_eq!(log, log2);

    let a = replay_render(&log, &g, &order, sink).unwrap();
    let b = replay_render(&log2, &g, &order, sink).unwrap();
    assert_eq!(a.captured, b.captured);
    assert_eq!(a.fired, b.fired);
    assert_eq!(
        a.timeline.clock().master_position(),
        b.timeline.clock().master_position()
    );
}

#[test]
fn file_roundtrip_replays_identically() {
    let (log, _fired, _cap, g, order, sink) = record_gate_session();

    let path = std::env::temp_dir().join(format!("aelog_acceptance_{}.json", std::process::id()));
    log.save_json(&path).unwrap();
    let loaded = Aelog::load_json(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(loaded, log);
    let a = replay_render(&log, &g, &order, sink).unwrap();
    let b = replay_render(&loaded, &g, &order, sink).unwrap();
    assert_eq!(a.captured, b.captured);
}

#[test]
fn transport_and_looping_are_recorded_faithfully() {
    // Pause at sample 6400, resume at 12800, with a loop of 0..24000.
    let (g, order, _gain, _sink) = gated_sine();
    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    rec.set_loop(0, 24_000);
    rec.schedule(EventTime::Sample(12_000), EventPayload::Trigger { tag: 1 })
        .unwrap();

    // 50 blocks play → pause 50 → play 200: playing master 6400 + 25600 =
    // 32000 samples, crossing the 24000-sample loop end (playhead wraps)
    // and the trigger at 12000 exactly once.
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK as usize, SR).unwrap();
    let mut fired = Vec::new();
    for block in 0..300usize {
        if block == 50 {
            rec.set_state(TransportState::Paused, 0);
        }
        if block == 100 {
            rec.set_state(TransportState::Playing, 0);
        }
        for e in rec.advance_block(BLOCK) {
            fired.push(e);
        }
        if rec.clock().is_playing() {
            ex.process_block().unwrap();
        }
    }
    let log = rec.finish();

    let ev = replay_events(&log).unwrap();
    assert_eq!(
        ev.fired, fired,
        "paused segments don't drop or double events"
    );
    assert_eq!(
        ev.timeline.clock().master_position(),
        32_000,
        "only playing time advances"
    );
    assert_eq!(
        ev.timeline.clock().position(),
        8_000,
        "playhead wrapped by the loop"
    );
    // The trigger fired exactly once (master monotonic despite the loop).
    assert_eq!(
        fired
            .iter()
            .filter(|e| e.payload == EventPayload::Trigger { tag: 1 })
            .count(),
        1
    );
}

#[test]
fn identical_sessions_produce_byte_equal_logs() {
    let (a, ..) = record_gate_session();
    let (b, ..) = record_gate_session();
    assert_eq!(
        a.to_json().unwrap(),
        b.to_json().unwrap(),
        "deterministic logs"
    );
    assert_eq!(a.commands.len(), 203); // tempo + state + schedule + 200 advances
}

#[test]
fn replay_is_a_pure_function_of_the_log() {
    // Two independent replays of the same log agree on everything.
    let (log, _fired, _cap, g, order, sink) = record_gate_session();
    let a = replay_render(&log, &g, &order, sink).unwrap();
    let b = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(a.captured, b.captured);
    assert_eq!(a.fired, b.fired);
    assert_eq!(
        a.timeline.clock().master_position(),
        b.timeline.clock().master_position()
    );
    assert!(a.timeline.pending() == b.timeline.pending());
}

#[test]
fn cli_cache_flag_reports_miss_then_hit_and_skips_re_rendering() {
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "aelog-cli-cache-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // A full recorded session + a matching graph, both on disk.
    let (log, _fired, _cap, g, _order, _sink) = record_gate_session();
    let log_path = dir.join("recording.aelog");
    log.save_json(&log_path).unwrap();
    let graph_path = dir.join("graph.json");
    std::fs::write(&graph_path, serde_json::to_vec(&g).unwrap()).unwrap();
    let cache_dir = dir.join("cache");

    let bin = env!("CARGO_BIN_EXE_aelog_replay");
    let run = |expect: &str| -> String {
        let out = Command::new(bin)
            .arg(&log_path)
            .arg("--cache")
            .arg("--graph")
            .arg(&graph_path)
            .arg("--cache-dir")
            .arg(&cache_dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "CLI failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            stdout.contains("cache: ")
                && stdout.contains(expect)
                && stdout.contains(&format!("format v{}", log.header.format_version)),
            "expected {expect:?} in:\n{stdout}"
        );
        stdout
    };

    // First run renders and stores (a content-addressed file appears); the
    // second run finds it and splices, so it reports a HIT rather than
    // re-rendering.
    let miss = run("cache: MISS rendered & stored");
    let hit = run("cache: HIT  reused golden render"); // aligned: HIT gets two spaces
    assert_ne!(miss, hit, "the hit/miss report flips between runs");

    // The stored capture lives under the deterministic content address, and
    // the capture actually holds audio (not silence).
    let address = engine::prelude::content_address(&log, &g, engine::prelude::NodeId(_sink.0));
    assert!(
        cache_dir.join(format!("{address}.json")).exists(),
        "golden capture stored under its content address"
    );
    assert!(
        miss.contains(&format!("({} samples", _cap.len())),
        "stored capture is the golden render's size"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
