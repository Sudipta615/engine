//! Fidelity tests — Graph 2.0 general-purpose topology (v3.27, roadmap
//! v3.27).
//!
//! Evolution thresholds (`docs/EVOLUTION.md` Phase 25):
//! * arbitrary topologies are expressible and **render correctly**: a
//!   dry/wet diamond (`Split → {Gain, Delay} → Mix`) places its dry and wet
//!   copies at the exact expected offsets and gains, and a three-way
//!   fan-out sums exactly;
//! * **cycle detection** rejects a cyclic graph at compile time with the
//!   cycle path reported;
//! * **validation** catches wrong-direction edges, duplicate fan-in, and
//!   typed-bus (Audio vs Control) mismatches as errors, while dangling
//!   input ports are warnings (executor treats them as silence);
//! * **topological scheduling** is deterministic (identical graphs →
//!   identical orders) and correct (every node after its producers);
//! * **dynamic graph recompilation** — mutate then `compile()` again — is
//!   reflected in the rendered audio;
//! * **serialization** round-trips through JSON to an identical render.

use engine::prelude::{
    ExecutionOrder, Graph2, Graph2Error, NodeId, NodeParams, OfflineExecutor, PortId, SignalType,
    SourceParams, TestSignal, ValidationReport,
};

const SR: f32 = 48_000.0;
const BLOCK: usize = 128;

/// Build the canonical dry/wet diamond and return (graph, order, sink).
fn dry_wet_diamond() -> (Graph2, ExecutionOrder, NodeId) {
    // source → split(2) → {gain(0.5) → mix.in0, delay(100) → mix.in1} → sink
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("drywet", 2);
    let dry = g.add_gain("dry", 0.5);
    let wet = g.add_delay("wet", 100);
    let mix = g.add_mix("sum", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), dry, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), wet, PortId::IN).unwrap();
    g.add_edge(dry, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(wet, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().expect("dry/wet diamond compiles").clone();
    (g, order, sink)
}

fn render(g: &Graph2, order: &ExecutionOrder, sink: NodeId, blocks: usize) -> Vec<f32> {
    let mut ex = OfflineExecutor::new(g, order, BLOCK, SR).unwrap();
    ex.process_blocks(blocks).unwrap();
    ex.capture(sink).unwrap().to_vec()
}

#[test]
fn dry_wet_diamond_renders_both_branches() {
    let (g, order, sink) = dry_wet_diamond();
    let cap = render(&g, &order, sink, 2);
    assert_eq!(cap.len(), 2 * BLOCK);
    assert!((cap[0] - 0.5).abs() < 1e-6, "dry copy scaled: {}", cap[0]);
    assert!(
        (cap[100] - 1.0).abs() < 1e-6,
        "wet copy delayed: {}",
        cap[100]
    );
    assert!(
        cap[1..100].iter().all(|s| s.abs() < 1e-6),
        "silence between copies"
    );
    assert!(cap[101..].iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn three_way_fanout_sums_exactly() {
    // source → split(3) → three gains → mix → sink: one impulse × (0.1+0.2+0.3).
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("fan", 3);
    let g1 = g.add_gain("a", 0.1);
    let g2 = g.add_gain("b", 0.2);
    let g3 = g.add_gain("c", 0.3);
    let mix = g.add_mix("sum", 3);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), g1, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), g2, PortId::IN).unwrap();
    g.add_edge(split, PortId(2), g3, PortId::IN).unwrap();
    g.add_edge(g1, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(g2, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(g3, PortId::OUT, mix, PortId(2)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let cap = render(&g, &order, sink, 1);
    assert!(
        (cap[0] - 0.6).abs() < 1e-6,
        "sum of three gains: {}",
        cap[0]
    );
    assert!(cap[1..].iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn cycle_is_rejected_at_compile_with_path() {
    let mut g = Graph2::new();
    let a = g.add_delay("a", 10);
    let b = g.add_delay("b", 10);
    g.add_edge(a, PortId::OUT, b, PortId::IN).unwrap();
    g.add_edge(b, PortId::OUT, a, PortId::IN).unwrap();
    let err = g.compile().unwrap_err();
    match err {
        Graph2Error::Cycle(path) => {
            assert_eq!(path.len(), 3, "cycle closes on itself: {path:?}");
            assert_eq!(path[0], path[2]);
        }
        other => panic!("expected Cycle, got {other:?}"),
    }
    // validate() reports it too.
    assert!(g.validate().is_err());
}

#[test]
fn validation_catches_bad_structure() {
    let mut g = Graph2::new();
    let sink = g.add_sink("k");

    // A non-existent output port is rejected at draw time.
    let a = g.add_delay("a", 5);
    let b = g.add_delay("b", 5);
    let err = g.add_edge(a, PortId(9), b, PortId::IN).unwrap_err();
    assert!(matches!(err, Graph2Error::UnknownPort(_, _, _)));

    // Duplicate fan-in into the same input port is rejected.
    let src = g.add_source("s");
    let src2 = g.add_source("t");
    g.add_edge(src, PortId::OUT, sink, PortId::IN).unwrap();
    let err = g.add_edge(src2, PortId::OUT, sink, PortId::IN).unwrap_err();
    assert_eq!(err, Graph2Error::DuplicateConnection);

    // Typed-bus mismatch: a host-defined Control input port rejects Audio.
    let ctl = engine::prelude::NodeDef {
        id: NodeId(100),
        name: "ctrl".to_string(),
        kind: engine::prelude::NodeKind::Sink,
        params: NodeParams::Sink,
        inputs: vec![engine::prelude::PortSpec::input(SignalType::Control, 1)],
        outputs: vec![],
    };
    g.add_node_raw(ctl).unwrap();
    let err = g
        .add_edge(src, PortId::OUT, NodeId(100), PortId::IN)
        .unwrap_err();
    assert!(matches!(err, Graph2Error::SignalMismatch(_, _)));

    // A cycle is structural: validate() reports it, while the dangling
    // input of the raw Control node is only a warning.
    g.add_edge(a, PortId::OUT, b, PortId::IN).unwrap();
    g.add_edge(b, PortId::OUT, a, PortId::IN).unwrap();
    let report: ValidationReport = g.validate();
    assert!(report.is_err(), "structural errors present");
    assert!(
        report
            .errors
            .iter()
            .any(|e| matches!(e, Graph2Error::Cycle(_))),
        "cycle reported: {:?}",
        report.errors
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("unconnected")),
        "dangling-port warnings reported"
    );
}

#[test]
fn topological_scheduling_is_deterministic_and_correct() {
    let (g1, o1, _) = dry_wet_diamond();
    let (_g2, o2, _) = dry_wet_diamond();

    // Identical topologies → identical orders.
    assert_eq!(o1, o2, "deterministic scheduling");

    // Correctness: every node appears after all of its producers.
    for e in g1.edges.values() {
        let i_src = o1.steps.iter().position(|&n| n == e.source.node).unwrap();
        let i_dst = o1.steps.iter().position(|&n| n == e.target.node).unwrap();
        assert!(i_src < i_dst, "producer before consumer: {e:?}");
    }
}

#[test]
fn dynamic_recompilation_changes_render() {
    let (mut g, order, sink) = dry_wet_diamond();
    let with_wet = render(&g, &order, sink, 2);

    // Tear down the wet branch: split.out1 → delay → mix.in1.
    let wet = g.nodes.values().find(|n| n.name == "wet").unwrap().id;
    let mix = g.nodes.values().find(|n| n.name == "sum").unwrap().id;
    let wet_edge = g.incoming(wet, PortId::IN).unwrap().id;
    let wet_mix_edge = g.incoming(mix, PortId(1)).unwrap().id;
    g.remove_edge(wet_edge);
    g.remove_edge(wet_mix_edge);
    g.remove_node(wet).unwrap();

    // Recompile (dynamic recompilation) and re-render: only the dry copy.
    let order2 = g.compile().unwrap().clone();
    let dry_only = render(&g, &order2, sink, 2);

    assert!(
        (with_wet[100] - 1.0).abs() < 1e-6,
        "wet present before teardown"
    );
    assert!((dry_only[0] - 0.5).abs() < 1e-6, "dry copy survives");
    assert!(dry_only[100].abs() < 1e-6, "wet copy gone after teardown");
}

#[test]
fn serialization_roundtrip_renders_identically() {
    let (g, order, sink) = dry_wet_diamond();
    let before = render(&g, &order, sink, 2);

    let json = serde_json::to_string(&g).unwrap();
    let mut back: Graph2 = serde_json::from_str(&json).unwrap();
    let order_back = back.compile().unwrap().clone();
    let after = render(&back, &order_back, sink, 2);

    assert_eq!(before, after, "JSON round-trip is render-identical");
}

#[test]
fn sine_source_drives_a_graph() {
    // source(sine 440) → gain(0.25) → sink: verify a continuous sine scaled.
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 440.0,
        },
    );
    let gain = g.add_gain("vol", 0.25);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let cap = render(&g, &order, sink, 2);
    assert_eq!(cap.len(), 2 * BLOCK);
    // Sample 0 of block 0: sin(0) = 0; sample 64 continues the wave.
    let expect = 0.25 * (2.0 * std::f32::consts::PI * 440.0 * 64.0 / SR).sin();
    assert!(
        (cap[64] - expect).abs() < 1e-3,
        "scaled sine: {} vs {expect}",
        cap[64]
    );
    assert!(cap.iter().all(|s| s.abs() <= 0.25 + 1e-6), "gain-bounded");
}
