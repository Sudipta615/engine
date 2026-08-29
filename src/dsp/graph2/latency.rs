//! Graph-wide latency and alignment (v3.30, Direction 2).
//!
//! Latency is a **graph-wide architectural concept**: a node adds a
//! fixed number of sample-taps (only `Delay` in the built-in set today —
//! convolution, HRTF, resampler and lookahead nodes plug into
//! [`node_latency`] the same way), and that latency accumulates down the
//! edge set. Parallel branches feeding a [`NodeKind::Mix`] must be aligned
//! to the *slowest* branch, so [`compensate`] inserts compensating
//! `Delay`s automatically.
//!
//! ```text
//!          Split ──┬─▶ Gain ──────────────────────┐
//!                  │                               ├─▶ Mix ──▶ Sink
//!                  └─▶ Delay(300) ────────────────┘
//!                      ↑ wet branch is 300 taps late
//!
//!          Split ──┬─▶ Gain ──▶ Delay(+300) ──────┐   ← auto-compensation
//!                  │                               ├─▶ Mix ──▶ Sink
//!                  └─▶ Delay(300) ────────────────┘
//! ```
//!
//! [`analyze`] computes the cumulative (max-path) upstream latency at every
//! node — the "latency arrives here" quantity the renderer cares about —
//! plus the graph total and per-node taps, for diagnostics.

use std::collections::BTreeMap;

use super::edge::{EdgeDef, EdgeEndpoint, EdgeId};
use super::node::{NodeDef, NodeId, NodeKind, NodeParams, PortId, PortSpec, SignalType};
use super::sort::topological_order;
use super::validate::{validate, Graph2Error};
use super::Graph2;

/// The intrinsic sample-latency a node adds to its outgoing signal.
/// [`NodeKind::Delay`] reports its samples; [`NodeKind::Convolution`]
/// reports its kernel length and [`NodeKind::HRTF`] the longer of its two
/// per-ear IRs — the pipeline delay a block-partitioned convolver pays, so
/// binaural and convolution-heavy branches report and compensate exactly
/// like `Delay`. [`NodeKind::Acoustic`] reports `0`: its direct path passes
/// through immediately, and the per-path delayed copies are a reverb
/// *tail* — the wet content is intentionally late, not a pipeline delay to
/// align.
pub fn node_latency(node: &NodeDef) -> u64 {
    match &node.params {
        NodeParams::Delay { samples } => *samples as u64,
        NodeParams::Convolution { kernel } => kernel.len() as u64,
        NodeParams::HRTF { left, right } => left.len().max(right.len()) as u64,
        _ => 0,
    }
}

/// Per-node latency diagnostics (Direction 2's "latency reporting").
#[derive(Debug, Clone, PartialEq)]
pub struct LatencyReport {
    /// Cumulative upstream latency (`samples`) at each node's output — the
    /// max over all source→node paths. A `Mix` reports the slowest of its
    /// inputs, which is exactly what must be equalised for alignment.
    pub upstream: BTreeMap<NodeId, u64>,
    /// Each node's own intrinsic tap count.
    pub taps: BTreeMap<NodeId, u64>,
    /// The graph's total latency (`samples`): the largest upstream value
    /// across the topology (the deepest path source → sink).
    pub total_samples: u64,
    /// `total_samples` converted to milliseconds at the given sample rate.
    pub total_ms: f32,
}

impl LatencyReport {
    /// The upstream (cumulative) latency arriving at `node`.
    pub fn upstream_at(&self, node: NodeId) -> u64 {
        self.upstream.get(&node).copied().unwrap_or(0)
    }

    /// The intrinsic taps a node adds.
    pub fn taps_at(&self, node: NodeId) -> u64 {
        self.taps.get(&node).copied().unwrap_or(0)
    }
}

/// Compute the cumulative upstream latency at every node by walking the
/// topological order (a child's latency is the max over its incoming edges
/// of `upstream[source] + taps[source]`). Assumes a validated, acyclic
/// graph; the order is supplied by the caller.
fn upstream_map(
    nodes: &BTreeMap<NodeId, NodeDef>,
    edges: &BTreeMap<EdgeId, EdgeDef>,
    order: &super::sort::ExecutionOrder,
) -> BTreeMap<NodeId, u64> {
    let mut up: BTreeMap<NodeId, u64> = BTreeMap::new();
    for &n in &order.steps {
        let mut best = 0u64;
        for e in edges.values() {
            if e.target.node != n {
                continue;
            }
            let source = match nodes.get(&e.source.node) {
                Some(s) => s,
                None => continue,
            };
            let cand = up.get(&e.source.node).copied().unwrap_or(0) + node_latency(source);
            best = best.max(cand);
        }
        up.insert(n, best);
    }
    up
}

/// Analyze a topology for latency: validate, topologically schedule, and
/// propagate cumulative upstream latency. Reports per-node upstream and
/// taps and the graph total.
pub fn analyze(graph: &Graph2, sample_rate: f32) -> Result<LatencyReport, Graph2Error> {
    let report = validate(&graph.nodes, &graph.edges);
    if let Some(err) = report.first_error() {
        return Err(err.clone());
    }
    let order = topological_order(&graph.nodes, &graph.edges)?;
    let up = upstream_map(&graph.nodes, &graph.edges, &order);
    let taps: BTreeMap<NodeId, u64> = graph
        .nodes
        .iter()
        .map(|(id, n)| (*id, node_latency(n)))
        .collect();
    let total_samples = up.values().copied().max().unwrap_or(0);
    let sr = if sample_rate > 0.0 { sample_rate } else { 1.0 };
    Ok(LatencyReport {
        upstream: up,
        taps,
        total_samples,
        total_ms: total_samples as f32 * 1000.0 / sr,
    })
}

/// Per-edge compensation for one target: how many samples must be added
/// along that edge so the branch reaches the target as late as the slowest
/// branch feeding it. Only meaningful for multi-input targets (a `Mix`).
fn edge_compensation(
    e: &EdgeDef,
    nodes: &BTreeMap<NodeId, NodeDef>,
    up: &BTreeMap<NodeId, u64>,
    max_at_target: u64,
) -> u64 {
    // Branch latency carried into the target via this edge.
    let branch = match nodes.get(&e.source.node) {
        Some(s) => up.get(&e.source.node).copied().unwrap_or(0) + node_latency(s),
        None => 0,
    };
    max_at_target.saturating_sub(branch)
}

/// Build an edited copy of a graph with **automatic delay compensation**:
/// for every edge feeding a node whose inputs carry unequal latency, insert
/// a compensating `Delay` in series so all branches into that node arrive
/// aligned to the slowest. Original node ids are preserved verbatim (so a
/// [`Timeline`](crate::dsp::timeline::Timeline) event addressing a node by
/// id — e.g. `SetGain` — keeps working on the compensated graph); only new
/// `Delay` nodes are added. The result is valid and must be recompiled
/// before execution.
pub fn compensate(graph: &Graph2) -> Result<Graph2, Graph2Error> {
    // Validate + schedule the ORIGINAL so compensation itself is exact.
    let report = validate(&graph.nodes, &graph.edges);
    if let Some(err) = report.first_error() {
        return Err(err.clone());
    }
    let order = topological_order(&graph.nodes, &graph.edges)?;
    let up = upstream_map(&graph.nodes, &graph.edges, &order);

    // The max *incoming branch latency* at each node — keyed by node, since
    // a `Mix` merges branches arriving on different input ports and the
    // fidelity target for compensation is the merge point, not one port.
    let mut max_at_node: BTreeMap<NodeId, u64> = BTreeMap::new();
    for e in graph.edges.values() {
        let branch = match graph.nodes.get(&e.source.node) {
            Some(s) => up.get(&e.source.node).copied().unwrap_or(0) + node_latency(s),
            None => 0,
        };
        let m = max_at_node.entry(e.target.node).or_insert(0);
        *m = (*m).max(branch);
    }

    // Build the compensated graph, preserving every original node id.
    let mut out = graph.clone();
    // Recompute fresh edge ids to keep the BTreeMap consistent.
    out.edges = graph.edges.clone();
    out.next_edge = graph.edges.keys().map(|e| e.0).max().unwrap_or(0) + 1;

    // A node only needs alignment if it has ≥ 2 incoming branches of
    // different latency — i.e. a merge point (or a single input fed by a
    // fan-out of unequal paths, which the map still catches).
    for &target in max_at_node.keys() {
        let needed = max_at_node[&target];
        let feed: Vec<EdgeId> = out
            .edges
            .values()
            .filter(|e| e.target.node == target)
            .map(|e| e.id)
            .collect();
        for eid in feed {
            // Re-fetch (out.edges mutated below).
            let e = match out.edges.get(&eid) {
                Some(e) => *e,
                None => continue,
            };
            let comp = edge_compensation(&e, &out.nodes, &up, needed);
            if comp == 0 {
                continue;
            }
            // Splice a Delay(comp) in series on this edge:
            //   old: (src, sPort) → (target, port)
            //   new: (src, sPort) → delay.IN ; delay.OUT → (target, port)
            let delay_id = NodeId(out.next_node);
            out.next_node += 1;
            let delay = NodeDef {
                id: delay_id,
                name: format!("comp-d{comp}"),
                kind: NodeKind::Delay,
                params: NodeParams::Delay {
                    samples: comp as u32,
                },
                inputs: vec![PortSpec::input(SignalType::Audio, 1)],
                outputs: vec![PortSpec::output(SignalType::Audio, 1)],
            };
            out.nodes.insert(delay_id, delay);

            let in_id = EdgeId(out.next_edge);
            let out_id = EdgeId(out.next_edge + 1);
            out.next_edge += 2;

            // Detach the old edge, attach the two compensation edges.
            out.edges.remove(&e.id);
            out.edges.insert(
                in_id,
                EdgeDef {
                    id: in_id,
                    source: e.source,
                    target: EdgeEndpoint::new(delay_id, PortId::IN),
                },
            );
            out.edges.insert(
                out_id,
                EdgeDef {
                    id: out_id,
                    source: EdgeEndpoint::new(delay_id, PortId::OUT),
                    target: e.target,
                },
            );
        }
    }

    // The edited graph's order cache is stale; validate the result.
    out.order = None;
    let vr = validate(&out.nodes, &out.edges);
    if !vr.is_ok() {
        return Err(vr
            .first_error()
            .cloned()
            .unwrap_or(Graph2Error::Cycle(Vec::new())));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::graph2::{Graph2, PortId};

    fn drywet() -> Graph2 {
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
        g
    }

    #[test]
    fn report_propagates_upstream_latency() {
        let g = drywet();
        let rep = analyze(&g, 48_000.0).unwrap();
        let mix = g
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::Mix)
            .unwrap()
            .id;
        let sink = g
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::Sink)
            .unwrap()
            .id;
        assert_eq!(rep.upstream_at(mix), 300, "mix sees the slow wet branch");
        assert_eq!(rep.upstream_at(sink), 300, "sink carries the total");
        assert_eq!(rep.taps_at(mix), 0);
        assert_eq!(rep.total_samples, 300);
        assert!((rep.total_ms - 6.25).abs() < 1e-3, "300/48000*1000");
    }

    #[test]
    fn deep_chain_sums_taps_no_compensation() {
        let mut g = Graph2::new();
        let src = g.add_source("s");
        let d1 = g.add_delay("a", 100);
        let d2 = g.add_delay("b", 200);
        let sink = g.add_sink("k");
        g.add_edge(src, PortId::OUT, d1, PortId::IN).unwrap();
        g.add_edge(d1, PortId::OUT, d2, PortId::IN).unwrap();
        g.add_edge(d2, PortId::OUT, sink, PortId::IN).unwrap();
        let rep = analyze(&g, 48_000.0).unwrap();
        assert_eq!(rep.total_samples, 300);
        // Single path: nothing to compensate.
        let c = compensate(&g).unwrap();
        assert_eq!(c.node_count(), g.node_count());
    }

    #[test]
    fn compensation_inserts_alignment_taps_preserving_ids() {
        let g = drywet();
        let orig_ids: Vec<NodeId> = g.nodes.keys().copied().collect();
        let dry = g.nodes.values().find(|n| n.name == "dry").unwrap().id;
        let mix = g
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::Mix)
            .unwrap()
            .id;

        let c = compensate(&g).unwrap();
        // All original nodes survive with their ids.
        for id in orig_ids {
            assert_eq!(
                c.node(id).map(|n| n.name.as_str()),
                g.node(id).map(|n| n.name.as_str())
            );
        }
        // Exactly one compensation Delay was added (value 300, on the dry leg).
        let comps: Vec<&NodeDef> = c
            .nodes
            .values()
            .filter(|n| n.name.starts_with("comp-"))
            .collect();
        assert_eq!(comps.len(), 1);
        assert_eq!(
            comps[0].params,
            NodeParams::Delay { samples: 300 },
            "dry branch compensated by the wet branch's 300 taps"
        );
        // The mix still has two incoming edges (now via the comp node).
        let into_mix = c.edges.values().filter(|e| e.target.node == mix).count();
        assert_eq!(into_mix, 2);
        // The dry gain still feeds something (id preserved).
        assert!(c.edges.values().any(|e| e.source.node == dry));
    }

    #[test]
    fn convolution_and_hrtf_report_taps_like_delay() {
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let conv = g.add_convolution("corr", vec![0.1; 200]);
        let hrtf = g.add_hrtf("bin", vec![0.5; 300], vec![0.25; 128]);
        let sink_c = g.add_sink("c");
        let sink_l = g.add_sink("l");
        let sink_r = g.add_sink("r");
        g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
        g.add_edge(src, PortId::OUT, hrtf, PortId::IN).unwrap();
        g.add_edge(conv, PortId::OUT, sink_c, PortId::IN).unwrap();
        g.add_edge(hrtf, PortId(0), sink_l, PortId::IN).unwrap();
        g.add_edge(hrtf, PortId(1), sink_r, PortId::IN).unwrap();

        assert_eq!(node_latency(g.node(conv).unwrap()), 200, "kernel length");
        assert_eq!(
            node_latency(g.node(hrtf).unwrap()),
            300,
            "longer of the two per-ear IRs"
        );
        assert!(g.node(conv).unwrap().capabilities().taps);
        assert!(g.node(hrtf).unwrap().capabilities().taps);

        let rep = analyze(&g, 48_000.0).unwrap();
        assert_eq!(rep.upstream_at(sink_c), 200);
        assert_eq!(rep.upstream_at(sink_l), 300, "left ear carries the max");
        assert_eq!(rep.upstream_at(sink_r), 300);
        assert_eq!(rep.taps_at(conv), 200);
        assert_eq!(rep.taps_at(hrtf), 300);
        assert_eq!(rep.total_samples, 300);
        assert!((rep.total_ms - 6.25).abs() < 1e-3);
    }

    #[test]
    fn compensation_aligns_convolution_branch_like_delay() {
        // source → split → {conv(kernel 300), gain} → mix → sink
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let split = g.add_split("s", 2);
        let conv = g.add_convolution("room", vec![0.1; 300]);
        let dry = g.add_gain("dry", 0.5);
        let mix = g.add_mix("mix", 2);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
        g.add_edge(split, PortId(0), conv, PortId::IN).unwrap();
        g.add_edge(split, PortId(1), dry, PortId::IN).unwrap();
        g.add_edge(conv, PortId::OUT, mix, PortId(0)).unwrap();
        g.add_edge(dry, PortId::OUT, mix, PortId(1)).unwrap();
        g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();

        let c = compensate(&g).unwrap();
        // Original ids preserved; exactly one comp Delay of 300 on the dry leg.
        for id in g.nodes.keys() {
            assert!(c.node(*id).is_some(), "id {id:?} preserved");
        }
        let comps: Vec<&NodeDef> = c
            .nodes
            .values()
            .filter(|n| n.name.starts_with("comp-"))
            .collect();
        assert_eq!(comps.len(), 1);
        assert_eq!(
            comps[0].params,
            NodeParams::Delay { samples: 300 },
            "convolver branch compensated like a 300-tap delay"
        );
    }

    #[test]
    fn compensation_aligns_hrtf_branch() {
        // source → split → {hrtf(L300 R128), gain} → mix → sink
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let split = g.add_split("s", 2);
        let hrtf = g.add_hrtf("bin", vec![0.5; 300], vec![0.25; 128]);
        let dry = g.add_gain("dry", 0.5);
        let mix = g.add_mix("mix", 2);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
        g.add_edge(split, PortId(0), hrtf, PortId::IN).unwrap();
        g.add_edge(split, PortId(1), dry, PortId::IN).unwrap();
        g.add_edge(hrtf, PortId(0), mix, PortId(0)).unwrap();
        g.add_edge(dry, PortId::OUT, mix, PortId(1)).unwrap();
        g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();

        let c = compensate(&g).unwrap();
        let comps: Vec<&NodeDef> = c
            .nodes
            .values()
            .filter(|n| n.name.starts_with("comp-"))
            .collect();
        assert_eq!(comps.len(), 1);
        assert_eq!(
            comps[0].params,
            NodeParams::Delay { samples: 300 },
            "binaural branch's 300 taps compensated on the dry leg"
        );
    }
}
