//! Fidelity tests — Graph-wide latency and alignment (v3.30, roadmap
//! v3.30, Direction 2).
//!
//! Evolution thresholds (`docs/EVOLUTION.md` Phase 28):
//! * [`analyze`] propagates cumulative upstream latency along the edge set:
//!   a `Mix` fed by a dry branch and a 300-tap delay branch reports 300 at
//!   its output, and the graph total equals the deepest path;
//! * [`compensate`] inserts automatic delay compensation so parallel
//!   branches arriving at a merge point are aligned to the slowest branch —
//!   the compensated render places the dry and wet copies on the **same
//!   sample** instead of 300 apart;
//! * deep single-path chains sum their taps and need no compensation;
//! * compensation **preserves original node ids**, so a
//!   [`Timeline`](engine::prelude::Timeline) event addressing a node by id
//!   (e.g. `SetGain`) still works on the compensated graph;
//! * analyze/compensate are deterministic.

use engine::prelude::{
    analyze, compensate, EventPayload, EventTime, ExecutionOrder, Graph2, NodeId, NodeKind,
    NodeParams, OfflineExecutor, PortId, Timeline, TransportState,
};

const SR: f32 = 48_000.0;
const BLOCK: usize = 256;

/// source → split(2) → { gain(0.5) dry, delay(300) wet } → mix → sink.
/// Returns (graph, sink).
fn drywet_diamond() -> (Graph2, NodeId) {
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("s", 2);
    let dry = g.add_gain("dry", 0.5);
    let wet = g.add_delay("wet", 300);
    let mix = g.add_mix("mix", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), dry, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), wet, PortId::IN).unwrap();
    g.add_edge(dry, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(wet, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    (g, sink)
}

fn render(g: &Graph2, order: &ExecutionOrder, sink: NodeId, blocks: usize) -> Vec<f32> {
    let mut ex = OfflineExecutor::new(g, order, BLOCK, SR).unwrap();
    for _ in 0..blocks {
        ex.process_block().unwrap();
    }
    ex.capture(sink).unwrap().to_vec()
}

#[test]
fn compensation_aligns_parallel_branches() {
    let (mut g, sink) = drywet_diamond();
    let order = g.compile().unwrap().clone();

    // Uncompensated: dry copy at 0, wet copy at 300.
    let raw = render(&g, &order, sink, 2);
    assert!((raw[0] - 0.5).abs() < 1e-6, "dry at 0: {}", raw[0]);
    assert!((raw[300] - 1.0).abs() < 1e-6, "wet at 300: {}", raw[300]);

    // Compensated: a 300-tap delay splices into the dry leg, so both
    // copies arrive at sample 300 and sum (0.5 + 1.0 = 1.5).
    let mut cg = compensate(&g).unwrap();
    let corder = cg.compile().unwrap().clone();
    let aligned = render(&cg, &corder, sink, 2);
    assert!(
        aligned[..300].iter().all(|s| s.abs() < 1e-6),
        "silent before 300"
    );
    assert!(
        (aligned[300] - 1.5).abs() < 1e-6,
        "branches aligned and summed at 300: {}",
        aligned[300]
    );
    assert!(aligned[301..].iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn report_propagates_and_diagnoses() {
    let (g, _sink) = drywet_diamond();
    let rep = analyze(&g, SR).unwrap();
    let mix = g
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Mix)
        .unwrap()
        .id;
    let dry = g.nodes.values().find(|n| n.name == "dry").unwrap().id;
    let wet = g.nodes.values().find(|n| n.name == "wet").unwrap().id;
    assert_eq!(rep.upstream_at(dry), 0);
    assert_eq!(
        rep.upstream_at(wet),
        0,
        "delay latency lands at its output, not input"
    );
    assert_eq!(rep.upstream_at(mix), 300, "mix reports the slowest branch");
    assert_eq!(rep.taps_at(wet), 300);
    assert_eq!(rep.taps_at(dry), 0);
    assert_eq!(rep.total_samples, 300);
    assert!((rep.total_ms - 6.25).abs() < 1e-3, "300 samples at 48 kHz");
}

#[test]
fn deep_chain_sums_taps_without_compensation() {
    let mut g = Graph2::new();
    let src = g.add_source("s");
    let d1 = g.add_delay("a", 100);
    let d2 = g.add_delay("b", 200);
    let sink = g.add_sink("k");
    g.add_edge(src, PortId::OUT, d1, PortId::IN).unwrap();
    g.add_edge(d1, PortId::OUT, d2, PortId::IN).unwrap();
    g.add_edge(d2, PortId::OUT, sink, PortId::IN).unwrap();

    let rep = analyze(&g, SR).unwrap();
    assert_eq!(rep.total_samples, 300, "taps sum along a chain");

    let mut cg = compensate(&g).unwrap();
    assert_eq!(
        cg.node_count(),
        g.node_count(),
        "single path: nothing to align"
    );
    let corder = cg.compile().unwrap().clone();
    let cap = render(&cg, &corder, sink, 2);
    assert!(
        (cap[300] - 1.0).abs() < 1e-6,
        "impulse arrives after 300 taps"
    );
}

#[test]
fn compensation_preserves_event_targets() {
    // A Timeline SetGain event addresses the dry gain node by id. After
    // compensation the id must still resolve (the gain node is untouched;
    // only new Delay nodes are added). 160 Hz sine: 300 samples = one
    // cycle, so the aligned dry (2.0) and wet (1.0) branches sum to 3.0×
    // the raw wave after the gate opens.
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        engine::prelude::SourceParams {
            signal: engine::prelude::TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let split = g.add_split("s", 2);
    let dry = g.add_gain("dry", 0.0);
    let wet = g.add_delay("wet", 300);
    let mix = g.add_mix("mix", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), dry, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), wet, PortId::IN).unwrap();
    g.add_edge(dry, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(wet, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();

    let gain_id = dry;
    let mut cg = compensate(&g).unwrap();
    let corder = cg.compile().unwrap().clone();

    // Gate the dry node 0.0 → 2.0 at beat 1 (120 BPM → sample 24000).
    let mut timeline = Timeline::new(SR);
    timeline.set_tempo(120.0);
    timeline.set_state(TransportState::Playing, 0);
    let mut ex = OfflineExecutor::new(&cg, &corder, BLOCK, SR).unwrap();
    timeline
        .schedule(
            EventTime::Beat(1.0),
            EventPayload::SetGain {
                node: gain_id.0,
                gain: 2.0,
            },
        )
        .unwrap();
    for _ in 0..(24_000 / BLOCK as u64 + 4) {
        for e in timeline.advance_block(BLOCK as u64) {
            if let EventPayload::SetGain { node, gain } = e.payload {
                ex.set_gain_step(NodeId(node), gain, e.local_index(BLOCK as u64))
                    .unwrap();
            }
        }
        ex.process_block().unwrap();
    }
    let cap = ex.capture(sink).unwrap();

    // After the gate opens (24 000) plus the aligned 300-tap travel, the
    // two branches share a phase and sum to 3× the raw sine.
    let raw = |i: usize| (2.0 * std::f32::consts::PI * 160.0 * i as f32 / SR).sin();
    let i = 24_000 + 310usize;
    assert!(
        (cap[i] - 3.0 * raw(i)).abs() < 1e-3,
        "aligned dry (2.0) + wet (1.0) at {i}: {} vs {}",
        cap[i],
        3.0 * raw(i)
    );
    // Before the gate only the wet branch was audible (dry gain 0.0).
    let j = 23_900usize;
    assert!((cap[j] - raw(j - 300)).abs() < 1e-3, "wet-only before gate");
}

#[test]
fn three_way_fanout_aligns_to_the_slowest() {
    // Three parallel branches: gain(0.5) direct, delay(100), delay(200) —
    // all must land on sample 200.
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("s", 3);
    let a = g.add_gain("a", 0.5);
    let b = g.add_delay("b", 100);
    let c = g.add_delay("c", 200);
    let mix = g.add_mix("mix", 3);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), a, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), b, PortId::IN).unwrap();
    g.add_edge(split, PortId(2), c, PortId::IN).unwrap();
    g.add_edge(a, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(b, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(c, PortId::OUT, mix, PortId(2)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();

    let rep = analyze(&g, SR).unwrap();
    assert_eq!(rep.total_samples, 200);
    // Two compensation delays: 200 (a) and 100 (b).
    let mut cg = compensate(&g).unwrap();
    let comps: Vec<u64> = cg
        .nodes
        .values()
        .filter(|n| n.name.starts_with("comp-"))
        .map(|n| match n.params {
            NodeParams::Delay { samples } => samples as u64,
            _ => 0,
        })
        .collect();
    assert_eq!(comps.len(), 2, "two faster branches get compensated");
    assert_eq!(comps.iter().sum::<u64>(), 300, "200 + 100");

    let corder = cg.compile().unwrap().clone();
    let cap = render(&cg, &corder, sink, 2);
    assert!(cap[..200].iter().all(|s| s.abs() < 1e-6));
    assert!(
        (cap[200] - (0.5 + 1.0 + 1.0)).abs() < 1e-6,
        "all three aligned at 200: {}",
        cap[200]
    );
}

#[test]
fn analyze_and_compensate_are_deterministic() {
    let (g, _sink) = drywet_diamond();
    let rep_a = analyze(&g, SR).unwrap();
    let rep_b = analyze(&g, SR).unwrap();
    assert_eq!(rep_a, rep_b);

    let ca = compensate(&g).unwrap();
    let cb = compensate(&g).unwrap();
    // Graph2 has no PartialEq derive; the editable structure must match.
    assert_eq!(ca.nodes, cb.nodes);
    assert_eq!(ca.edges, cb.edges);
}
