//! Fidelity tests — HRTF / convolver taps in the latency pass (v3.34).
//!
//! Convolution and binaural branches now report and compensate exactly
//! like `Delay` nodes (Phase 32):
//! * a [`NodeKind::Convolution`] node reports its **kernel length** as
//!   taps — the algorithmic latency a block-partitioned convolver pays —
//!   and the executor emits `output[k] = (x * h)[k - kernel.len()]`, so
//!   the reported taps and the rendered timing agree;
//! * a [`NodeKind::HRTF`] node (mono in, stereo out) reports the **longer**
//!   of its two per-ear IRs and delays both ears by that length, keeping
//!   the pair mutually aligned;
//! * [`analyze`] propagates both like any taps, and [`compensate`] splices
//!   a `Delay` onto the faster branch of a merge — a dry/wet diamond with
//!   a 300-tap convolver aligns to a single summed sample at 300, exactly
//!   like the v3.30 delay diamond;
//! * compensation preserves node ids, so timeline `SetGain` events keep
//!   landing on the compensated graph;
//! * deep convolver chains sum taps without compensation, and graphs
//!   containing kernels / HRIRs survive JSON round-trips.

use engine::prelude::{
    analyze, compensate, EventPayload, EventTime, ExecutionOrder, Graph2, NodeId, NodeKind,
    OfflineExecutor, PortId, Timeline, TransportState,
};

const SR: f32 = 48_000.0;
const BLOCK: usize = 256;

/// `kernel` of length `taps` that is a single unit impulse at tap 0 (so
/// the convolver branch emits exactly one sample at offset `taps`).
fn impulse_kernel(taps: usize) -> Vec<f32> {
    let mut h = vec![0.0; taps];
    h[0] = 1.0;
    h
}

/// source → split(2) → { convolution(kernel), gain(1.0) dry } → mix → sink.
fn conv_diamond(taps: usize) -> (Graph2, NodeId) {
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("s", 2);
    let conv = g.add_convolution("room", impulse_kernel(taps));
    let dry = g.add_gain("dry", 1.0);
    let mix = g.add_mix("mix", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), conv, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), dry, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(dry, PortId::OUT, mix, PortId(1)).unwrap();
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
fn convolution_reports_taps_and_compensates_like_delay() {
    let (mut g, sink) = conv_diamond(300);
    let order = g.compile().unwrap().clone();

    // Uncompensated: dry impulse at 0, convolver's response at 300 (the
    // kernel's pipeline delay — its reported taps).
    let raw = render(&g, &order, sink, 2);
    assert!((raw[0] - 1.0).abs() < 1e-6, "dry at 0: {}", raw[0]);
    assert!(
        (raw[300] - 1.0).abs() < 1e-6,
        "conv response at its 300 taps: {}",
        raw[300]
    );

    // Compensated: a 300-tap delay splices into the dry leg, so both
    // copies arrive at sample 300 and sum (1.0 + 1.0 = 2.0) — exactly the
    // v3.30 delay-diamond behaviour.
    let mut cg = compensate(&g).unwrap();
    let corder = cg.compile().unwrap().clone();
    let aligned = render(&cg, &corder, sink, 2);
    assert!(
        aligned[..300].iter().all(|s| s.abs() < 1e-6),
        "silent before 300"
    );
    assert!(
        (aligned[300] - 2.0).abs() < 1e-6,
        "aligned and summed at 300: {}",
        aligned[300]
    );
    assert!(aligned[301..].iter().all(|s| s.abs() < 1e-6));

    // The report sees the convolver as a 300-tap node.
    let rep = analyze(&g, SR).unwrap();
    let conv = g
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Convolution)
        .unwrap()
        .id;
    let mix = g
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Mix)
        .unwrap()
        .id;
    assert_eq!(rep.taps_at(conv), 300, "kernel length");
    assert_eq!(rep.upstream_at(mix), 300, "mix reports the slowest branch");
    assert_eq!(rep.total_samples, 300);
}

#[test]
fn hrtf_reports_longer_ear_and_aligns_stereo_pair() {
    // Left IR 300 taps (impulse at 0), right IR 128 taps. Both ears are
    // delayed by 300; the node reports 300 and the dry leg is compensated
    // by 300.
    let left = impulse_kernel(300);
    let right = impulse_kernel(128);
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("s", 2);
    let hrtf = g.add_hrtf("bin", left.clone(), right.clone());
    let dry = g.add_gain("dry", 1.0);
    let mix = g.add_mix("mix", 2);
    let sink = g.add_sink("out");
    let sl = g.add_sink("l");
    let sr = g.add_sink("r");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), hrtf, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), dry, PortId::IN).unwrap();
    g.add_edge(hrtf, PortId(0), sl, PortId::IN).unwrap();
    g.add_edge(hrtf, PortId(1), sr, PortId::IN).unwrap();
    g.add_edge(hrtf, PortId(0), mix, PortId(0)).unwrap();
    g.add_edge(dry, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();

    let rep = analyze(&g, SR).unwrap();
    let hrtf_id = g
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::HRTF)
        .unwrap()
        .id;
    assert_eq!(rep.taps_at(hrtf_id), 300, "longer of the two per-ear IRs");
    assert_eq!(rep.upstream_at(sl), 300);
    assert_eq!(rep.upstream_at(sr), 300);

    // Uncompensated render: both ears fire at 300 (mutually aligned), dry
    // at 0.
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK, SR).unwrap();
    ex.process_blocks(2).unwrap();
    assert!(
        (ex.capture(sl).unwrap()[300] - 1.0).abs() < 1e-6,
        "left at 300"
    );
    assert!(
        (ex.capture(sr).unwrap()[300] - 1.0).abs() < 1e-6,
        "right at 300"
    );
    assert!(ex.capture(sl).unwrap()[..300]
        .iter()
        .all(|s| s.abs() < 1e-6));

    // Compensated: dry joins the ears at 300 → mix sums 2.0 there.
    let mut cg = compensate(&g).unwrap();
    let corder = cg.compile().unwrap().clone();
    let aligned = render(&cg, &corder, sink, 2);
    assert!((aligned[300] - 2.0).abs() < 1e-6, "dry aligned to binaural");
    assert!(aligned[..300].iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn deep_convolver_chain_sums_taps_without_compensation() {
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let c1 = g.add_convolution("a", impulse_kernel(200));
    let c2 = g.add_convolution("b", impulse_kernel(100));
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, c1, PortId::IN).unwrap();
    g.add_edge(c1, PortId::OUT, c2, PortId::IN).unwrap();
    g.add_edge(c2, PortId::OUT, sink, PortId::IN).unwrap();

    let rep = analyze(&g, SR).unwrap();
    // The model: a node's own taps land at its *output*, so c2's upstream
    // is c1's 200 and the total (sink) is 300.
    assert_eq!(rep.upstream_at(c2), 200);
    assert_eq!(rep.upstream_at(sink), 300, "200 + 100 sum down the chain");
    assert_eq!(rep.total_samples, 300);

    // Single path: nothing to compensate.
    let c = compensate(&g).unwrap();
    assert_eq!(c.node_count(), g.node_count());

    // And the render honours the summed delay: the impulse response of
    // chain (impulse kernels) arrives at 300.
    let order = g.compile().unwrap().clone();
    let raw = render(&g, &order, sink, 2);
    assert!((raw[300] - 1.0).abs() < 1e-6, "chain response at 300");
    assert!(raw[..300].iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn compensated_graph_keeps_node_ids_for_timeline_events() {
    // Sine → split → { conv(300, impulse kernel), gain(1.0) } → mix → sink.
    // After compensation, a SetGain on the dry node id (via the timeline)
    // must still land — the alignment delay must not break the address.
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        engine::prelude::SourceParams {
            signal: engine::prelude::TestSignal::Sine,
            frequency_hz: 160.0, // 300 samples = exactly one cycle
        },
    );
    let split = g.add_split("s", 2);
    let conv = g.add_convolution("room", impulse_kernel(300));
    let dry = g.add_gain("dry", 1.0);
    let mix = g.add_mix("mix", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), conv, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), dry, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(dry, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();

    let dry_id = g.nodes.values().find(|n| n.name == "dry").unwrap().id;
    let mut cg = compensate(&g).unwrap();
    let corder = cg.compile().unwrap().clone();

    // Drive with a timeline: a SetGain to 0 on the dry node at beat 1
    // (sample 24 000). Before it, mix = x[k] + x[k-300] (dry + conv);
    // after it, only the convolved branch x[k-300].
    let mut tl = Timeline::new(SR);
    tl.set_state(TransportState::Playing, 0);
    tl.schedule(
        EventTime::Beat(1.0),
        EventPayload::SetGain {
            node: dry_id.0,
            gain: 0.0,
        },
    )
    .unwrap();

    let mut ex = OfflineExecutor::new(&cg, &corder, BLOCK, SR).unwrap();
    for _ in 0..188 {
        for e in tl.advance_block(BLOCK as u64) {
            if let EventPayload::SetGain { node, gain } = e.payload {
                ex.set_gain_step(NodeId(node), gain, e.local_index(BLOCK as u64))
                    .unwrap();
            }
        }
        ex.process_block().unwrap();
    }
    let cap = ex.capture(sink).unwrap();
    let raw = |i: usize| (2.0 * std::f32::consts::PI * 160.0 * i as f32 / SR).sin();
    // Frame 23 999: dry still on → x + x[23999-300].
    let before = raw(23_999) + raw(23_999 - 300);
    assert!(
        (cap[23_999] - before).abs() < 1e-3,
        "dry audible before the gate: {} vs {before}",
        cap[23_999]
    );
    // Frame 24 000: gate lands → only the convolved branch.
    let after = raw(24_000 - 300);
    assert!(
        (cap[24_000] - after).abs() < 1e-3,
        "dry silenced at the gate: {} vs {after}",
        cap[24_000]
    );
}

#[test]
fn graphs_with_kernels_serialize_roundtrip() {
    let (mut g, _sink) = conv_diamond(64);
    let hrtf = g.add_hrtf("bin", vec![0.5; 32], vec![0.25; 16]);
    let sink2 = g.add_sink("l");
    let src = g
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Source)
        .unwrap()
        .id;
    g.add_edge(src, PortId::OUT, hrtf, PortId::IN).unwrap();
    g.add_edge(hrtf, PortId(0), sink2, PortId::IN).unwrap();

    let json = serde_json::to_string(&g).unwrap();
    let mut back: Graph2 = serde_json::from_str(&json).unwrap();
    assert_eq!(back.node_count(), g.node_count());
    assert_eq!(back.edge_count(), g.edge_count());
    // Kernels survive: the convolver still reports its taps and the HRTF
    // its longer ear after a round-trip.
    let rep = analyze(&back, SR).unwrap();
    let conv = back
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Convolution)
        .unwrap()
        .id;
    let hrtf_id = back
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::HRTF)
        .unwrap()
        .id;
    assert_eq!(rep.taps_at(conv), 64);
    assert_eq!(rep.taps_at(hrtf_id), 32);
    assert_eq!(
        back.compile().unwrap(),
        g.compile().unwrap(),
        "same topology after round-trip"
    );
}
