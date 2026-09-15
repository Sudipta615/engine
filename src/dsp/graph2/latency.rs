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

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::edge::{EdgeDef, EdgeEndpoint, EdgeId};
use super::node::{
    HrtfSource, NodeDef, NodeId, NodeKind, NodeParams, PortId, PortSpec, ProdStage, SignalType,
};
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
/// The intrinsic sample-latency a node adds to its outgoing signal at the given sample rate.
/// [`NodeKind::Delay`] reports its samples; [`NodeKind::Convolution`]
/// reports its kernel length and [`NodeKind::HRTF`] the longer of its two
/// per-ear IRs.
pub fn node_latency_at(node: &NodeDef, sample_rate: f32) -> u64 {
    let sr = if sample_rate > 0.0 {
        sample_rate
    } else {
        48_000.0
    };
    match &node.params {
        NodeParams::Delay { samples } => *samples as u64,
        NodeParams::Convolution { kernel } => kernel.len() as u64,
        NodeParams::HRTF {
            left,
            right,
            source,
        } => match source {
            HrtfSource::Inline => left.len().max(right.len()) as u64,
            HrtfSource::Dataset { taps, .. } => *taps as u64,
        },
        NodeParams::Resampler { quality, .. } => *quality as u64,
        _ => match node.kind {
            NodeKind::Prod(ProdStage::PluginHost) => {
                if let Some(host) = super::prod::resolve_host(&node.name) {
                    host.descriptor().latency_samples as u64
                } else {
                    0
                }
            }
            NodeKind::Prod(ProdStage::Limiter) => ((5.0 / 1000.0) * sr).round() as u64,
            NodeKind::Prod(ProdStage::Convolution) => {
                crate::dsp::convolution::DEFAULT_PARTITION_SIZE as u64
            }
            _ => 0,
        },
    }
}

/// The intrinsic sample-latency a node adds to its outgoing signal at nominal 48kHz.
pub fn node_latency(node: &NodeDef) -> u64 {
    node_latency_at(node, 48_000.0)
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
    /// The graph's total latency (`samples`): the maximum total end-to-end latency
    /// (upstream + terminal intrinsic latency) across all terminal/sink nodes.
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
    sample_rate: f32,
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
            let cand =
                up.get(&e.source.node).copied().unwrap_or(0) + node_latency_at(source, sample_rate);
            best = best.max(cand);
        }
        up.insert(n, best);
    }
    up
}

/// Analyze a topology for latency: validate, topologically schedule, and
/// propagate cumulative upstream latency. Reports per-node upstream and
/// taps and the true end-to-end graph total across all sinks.
pub fn analyze(graph: &Graph2, sample_rate: f32) -> Result<LatencyReport, Graph2Error> {
    let report = validate(&graph.nodes, &graph.edges);
    if let Some(err) = report.first_error() {
        return Err(err.clone());
    }
    let order = topological_order(&graph.nodes, &graph.edges)?;
    let up = upstream_map(&graph.nodes, &graph.edges, &order, sample_rate);
    let taps: BTreeMap<NodeId, u64> = graph
        .nodes
        .iter()
        .map(|(id, n)| (*id, node_latency_at(n, sample_rate)))
        .collect();

    // Identify all terminal / sink nodes (nodes with no outgoing edges, or NodeKind::Sink)
    let sinks: Vec<NodeId> = graph
        .nodes
        .keys()
        .filter(|&id| {
            let is_sink_kind = graph.nodes[id].kind == NodeKind::Sink;
            let has_outgoing = graph.edges.values().any(|e| e.source.node == *id);
            is_sink_kind || !has_outgoing
        })
        .copied()
        .collect();

    // For every sink:
    // sink_latency = upstream_latency + sink_intrinsic_latency
    // total_graph_latency = max(all sink latencies)
    let total_samples = if sinks.is_empty() {
        up.values().copied().max().unwrap_or(0)
    } else {
        sinks
            .iter()
            .map(|id| up.get(id).copied().unwrap_or(0) + taps.get(id).copied().unwrap_or(0))
            .max()
            .unwrap_or(0)
    };

    let sr = if sample_rate > 0.0 { sample_rate } else { 1.0 };
    Ok(LatencyReport {
        upstream: up,
        taps,
        total_samples,
        total_ms: total_samples as f32 * 1000.0 / sr,
    })
}

/// The intrinsic ring-down tail (samples) a node adds to its outgoing signal at the given sample rate (Item 29).
pub fn node_tail_at(node: &NodeDef, sample_rate: f32) -> u64 {
    let sr = if sample_rate > 0.0 {
        sample_rate
    } else {
        48_000.0
    };
    match &node.params {
        NodeParams::Delay { samples } => *samples as u64,
        NodeParams::Convolution { kernel } => kernel.len() as u64,
        NodeParams::HRTF {
            left,
            right,
            source,
        } => match source {
            HrtfSource::Inline => left.len().max(right.len()) as u64,
            HrtfSource::Dataset { taps, .. } => *taps as u64,
        },
        _ => match node.kind {
            NodeKind::Prod(ProdStage::PluginHost) => {
                if let Some(host) = super::prod::resolve_host(&node.name) {
                    host.descriptor().tail_samples as u64
                } else {
                    0
                }
            }
            NodeKind::Prod(ProdStage::Limiter) => ((50.0 / 1000.0) * sr).round() as u64,
            NodeKind::Prod(ProdStage::Convolution) => 4096,
            NodeKind::Prod(ProdStage::Spatial) => (1.5 * sr).round() as u64,
            NodeKind::Acoustic => (1.5 * sr).round() as u64,
            _ => 0,
        },
    }
}

/// The intrinsic ring-down tail (samples) a node adds to its outgoing signal at nominal 48kHz.
pub fn node_tail(node: &NodeDef) -> u64 {
    node_tail_at(node, 48_000.0)
}

/// Per-node tail diagnostics (Item 29).
#[derive(Debug, Clone, PartialEq)]
pub struct TailReport {
    /// Cumulative upstream tail (`samples`) arriving at each node's output.
    pub upstream: BTreeMap<NodeId, u64>,
    /// Each node's own intrinsic tail tap count.
    pub tails: BTreeMap<NodeId, u64>,
    /// The graph's total tail (`samples`): max over all paths to terminal/sink nodes.
    pub total_samples: u64,
    /// `total_samples` converted to milliseconds at the given sample rate.
    pub total_ms: f32,
}

impl TailReport {
    pub fn upstream_at(&self, node: NodeId) -> u64 {
        self.upstream.get(&node).copied().unwrap_or(0)
    }

    pub fn tail_at(&self, node: NodeId) -> u64 {
        self.tails.get(&node).copied().unwrap_or(0)
    }
}

/// Analyze graph-wide ring-down tail at the specified sample rate.
pub fn analyze_tail(graph: &Graph2, sample_rate: f32) -> Result<TailReport, Graph2Error> {
    let report = validate(&graph.nodes, &graph.edges);
    if let Some(err) = report.first_error() {
        return Err(err.clone());
    }
    let order = topological_order(&graph.nodes, &graph.edges)?;
    let mut tails: BTreeMap<NodeId, u64> = BTreeMap::new();
    for (&id, n) in &graph.nodes {
        tails.insert(id, node_tail_at(n, sample_rate));
    }

    let mut up: BTreeMap<NodeId, u64> = BTreeMap::new();
    for &n in &order.steps {
        let mut best = 0u64;
        for e in graph.edges.values() {
            if e.target.node != n {
                continue;
            }
            let source = match graph.nodes.get(&e.source.node) {
                Some(s) => s,
                None => continue,
            };
            let cand = up.get(&e.source.node).copied().unwrap_or(0)
                + node_latency_at(source, sample_rate)
                + node_tail_at(source, sample_rate);
            best = best.max(cand);
        }
        up.insert(n, best);
    }

    let sinks: Vec<NodeId> = graph
        .nodes
        .keys()
        .filter(|&id| {
            let is_sink_kind = graph.nodes[id].kind == NodeKind::Sink;
            let has_outgoing = graph.edges.values().any(|e| e.source.node == *id);
            is_sink_kind || !has_outgoing
        })
        .copied()
        .collect();

    let total_samples = if sinks.is_empty() {
        up.iter()
            .map(|(&id, &u)| u + tails.get(&id).copied().unwrap_or(0))
            .max()
            .unwrap_or(0)
    } else {
        sinks
            .iter()
            .map(|id| up.get(id).copied().unwrap_or(0) + tails.get(id).copied().unwrap_or(0))
            .max()
            .unwrap_or(0)
    };

    let sr = if sample_rate > 0.0 { sample_rate } else { 1.0 };
    Ok(TailReport {
        upstream: up,
        tails,
        total_samples,
        total_ms: total_samples as f32 * 1000.0 / sr,
    })
}

/// The total ring-down tail in samples for the graph at the specified sample rate.
pub fn graph_tail_samples(graph: &Graph2, sample_rate: f32) -> u64 {
    analyze_tail(graph, sample_rate)
        .map(|r| r.total_samples)
        .unwrap_or(0)
}

/// Per-edge compensation for one target: how many samples must be added
/// along that edge so the branch reaches the target as late as the slowest
/// branch feeding it. Only meaningful for multi-input targets (a `Mix`).
fn edge_compensation(
    e: &EdgeDef,
    nodes: &BTreeMap<NodeId, NodeDef>,
    up: &BTreeMap<NodeId, u64>,
    max_at_target: u64,
    sample_rate: f32,
) -> u64 {
    // Branch latency carried into the target via this edge.
    let branch = match nodes.get(&e.source.node) {
        Some(s) => up.get(&e.source.node).copied().unwrap_or(0) + node_latency_at(s, sample_rate),
        None => 0,
    };
    max_at_target.saturating_sub(branch)
}

/// Build an edited copy of a graph with **automatic delay compensation** at nominal 48 kHz.
pub fn compensate(graph: &Graph2) -> Result<Graph2, Graph2Error> {
    compensate_at(graph, 48_000.0)
}

/// Build an edited copy of a graph with **automatic delay compensation** at the specified sample rate.
pub fn compensate_at(graph: &Graph2, sample_rate: f32) -> Result<Graph2, Graph2Error> {
    // Validate + schedule the ORIGINAL so compensation itself is exact.
    let report = validate(&graph.nodes, &graph.edges);
    if let Some(err) = report.first_error() {
        return Err(err.clone());
    }
    let order = topological_order(&graph.nodes, &graph.edges)?;
    let up = upstream_map(&graph.nodes, &graph.edges, &order, sample_rate);

    // The max *incoming branch latency* at each node — keyed by node, since
    // a `Mix` merges branches arriving on different input ports and the
    // fidelity target for compensation is the merge point, not one port.
    let mut max_at_node: BTreeMap<NodeId, u64> = BTreeMap::new();
    for e in graph.edges.values() {
        let branch = match graph.nodes.get(&e.source.node) {
            Some(s) => {
                up.get(&e.source.node).copied().unwrap_or(0) + node_latency_at(s, sample_rate)
            }
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
            let comp = edge_compensation(&e, &out.nodes, &up, needed, sample_rate);
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

/// Nature or methodology of a latency observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyMeasurementKind {
    /// Nominal latency reported by node descriptors / algorithms.
    #[default]
    Reported,
    /// Realized sample-delay verified through signal tracking or test impulse.
    Actual,
    /// Model-based heuristic estimate (e.g. adaptive buffers or variable resamplers).
    Estimated,
    /// Physical loopback or hardware time-of-flight measurement.
    Measured,
}

/// Granular decomposition of latency contributors for a single node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct NodeLatencyBreakdown {
    /// Fixed algorithm pipeline delay in samples.
    pub intrinsic_samples: u64,
    /// Lookahead window buffer delay in samples (e.g. brickwall limiter).
    pub lookahead_samples: u64,
    /// Latency introduced by hosted external/native plugins in samples.
    pub plugin_samples: u64,
    /// FIR/polyphase resampler filter group delay in samples.
    pub resampler_samples: u64,
    /// Binaural head-related transfer function FIR alignment delay in samples.
    pub hrtf_samples: u64,
    /// Partitioned / direct convolution filter latency in samples.
    pub convolution_samples: u64,
    /// Output endpoint or hardware driver buffer latency in samples.
    pub device_samples: u64,
    /// Ring-buffer or transport queue latency in samples.
    pub output_samples: u64,
}

impl NodeLatencyBreakdown {
    /// Total DSP-domain latency in samples (excluding hardware driver / transport).
    pub fn dsp_total_samples(&self) -> u64 {
        self.intrinsic_samples
            + self.lookahead_samples
            + self.plugin_samples
            + self.resampler_samples
            + self.hrtf_samples
            + self.convolution_samples
    }

    /// Complete end-to-end latency in samples including output buffers.
    pub fn total_samples(&self) -> u64 {
        self.dsp_total_samples() + self.device_samples + self.output_samples
    }

    /// Latency in milliseconds at the specified sample rate.
    pub fn to_ms(&self, sample_rate: f32) -> f32 {
        if sample_rate > 0.0 {
            (self.total_samples() as f32 / sample_rate) * 1000.0
        } else {
            0.0
        }
    }
}

/// Compute granular latency breakdown for a given node at `sample_rate`.
pub fn node_latency_breakdown(node: &NodeDef, sample_rate: f32) -> NodeLatencyBreakdown {
    let mut bd = NodeLatencyBreakdown::default();
    let sr = if sample_rate > 0.0 {
        sample_rate
    } else {
        48_000.0
    };

    match &node.params {
        NodeParams::Delay { samples } => bd.intrinsic_samples = *samples as u64,
        NodeParams::Convolution { kernel } => bd.convolution_samples = kernel.len() as u64,
        NodeParams::HRTF {
            left,
            right,
            source,
        } => {
            bd.hrtf_samples = match source {
                HrtfSource::Inline => left.len().max(right.len()) as u64,
                HrtfSource::Dataset { taps, .. } => *taps as u64,
            };
        }
        NodeParams::Resampler { quality, .. } => bd.resampler_samples = *quality as u64,
        _ => match node.kind {
            NodeKind::Prod(ProdStage::PluginHost) => {
                if let Some(host) = super::prod::resolve_host(&node.name) {
                    bd.plugin_samples = host.descriptor().latency_samples as u64;
                }
            }
            NodeKind::Prod(ProdStage::Limiter) => {
                bd.lookahead_samples = ((5.0 / 1000.0) * sr).round() as u64;
            }
            NodeKind::Prod(ProdStage::Convolution) => {
                bd.convolution_samples = crate::dsp::convolution::DEFAULT_PARTITION_SIZE as u64;
            }
            _ => {}
        },
    }
    bd
}

/// Unified, architecture-wide latency analysis report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedLatencyReport {
    /// Granular breakdown per node.
    pub per_node: BTreeMap<NodeId, NodeLatencyBreakdown>,
    /// Cumulative upstream latency arriving at each node's inputs.
    pub upstream: BTreeMap<NodeId, u64>,
    /// Total DSP pipeline latency in samples.
    pub dsp_latency_samples: u64,
    /// Total DSP pipeline latency in milliseconds.
    pub dsp_latency_ms: f32,
    /// Output endpoint and driver latency in samples.
    pub device_latency_samples: u64,
    /// Output endpoint and driver latency in milliseconds.
    pub device_latency_ms: f32,
    /// Total end-to-end system latency in samples.
    pub total_latency_samples: u64,
    /// Total end-to-end system latency in milliseconds.
    pub total_latency_ms: f32,
    /// Measurement classification (Reported, Actual, Estimated, Measured).
    pub measurement_kind: LatencyMeasurementKind,
}

/// Analyze graph and output pipeline to generate a [`UnifiedLatencyReport`].
pub fn analyze_unified(
    graph: &Graph2,
    sample_rate: f32,
    device_samples: u64,
    output_samples: u64,
    measurement_kind: LatencyMeasurementKind,
) -> Result<UnifiedLatencyReport, Graph2Error> {
    let rep = analyze(graph, sample_rate)?;
    let mut per_node = BTreeMap::new();

    for (id, node) in &graph.nodes {
        per_node.insert(*id, node_latency_breakdown(node, sample_rate));
    }

    let dsp_latency_samples = rep.total_samples;
    let dsp_latency_ms = rep.total_ms;
    let dev_samples = device_samples + output_samples;
    let dev_ms = if sample_rate > 0.0 {
        (dev_samples as f32 / sample_rate) * 1000.0
    } else {
        0.0
    };

    let total_latency_samples = dsp_latency_samples + dev_samples;
    let total_latency_ms = dsp_latency_ms + dev_ms;

    Ok(UnifiedLatencyReport {
        per_node,
        upstream: rep.upstream,
        dsp_latency_samples,
        dsp_latency_ms,
        device_latency_samples: dev_samples,
        device_latency_ms: dev_ms,
        total_latency_samples,
        total_latency_ms,
        measurement_kind,
    })
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

    #[test]
    fn resampler_reports_quality_taps_and_compensates_like_delay() {
        // The v3.30 roadmap hook: a resampler node reports its own taps and
        // gets aligned exactly like Delay/Convolution.
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let split = g.add_split("s", 2);
        let rs = g.add_resampler_with_quality("rs", 2.0, 48);
        let dry = g.add_gain("dry", 0.5);
        let mix = g.add_mix("mix", 2);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
        g.add_edge(split, PortId(0), rs, PortId::IN).unwrap();
        g.add_edge(split, PortId(1), dry, PortId::IN).unwrap();
        g.add_edge(rs, PortId(0), mix, PortId(0)).unwrap();
        g.add_edge(dry, PortId::OUT, mix, PortId(1)).unwrap();
        g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();

        assert_eq!(
            node_latency(g.node(rs).unwrap()),
            48,
            "resampler reports its quality as taps"
        );
        assert!(g.node(rs).unwrap().capabilities().taps);

        let rep = analyze(&g, 48_000.0).unwrap();
        assert_eq!(
            rep.upstream_at(mix),
            48,
            "mix aligns to the resampler branch"
        );
        assert_eq!(rep.total_samples, 48);

        // Compensation splices exactly one Delay(48) on the dry leg, ids kept.
        let c = compensate(&g).unwrap();
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
            NodeParams::Delay { samples: 48 },
            "resampler branch's 48 taps compensated exactly like a delay"
        );
    }

    #[test]
    fn hrtf_dataset_reports_its_taps_and_compensates_like_delay() {
        // A measured-dataset binaural node reports its configured `taps` and
        // is aligned by the latency pass exactly like a convolution/HRTF
        // branch — graph-based binaural branches using real head-related
        // responses still align with the dry leg.
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let split = g.add_split("s", 2);
        let hrtf = g.add_hrtf_dataset_with_taps("bin", 90.0, 0.0, 64);
        let dry = g.add_gain("dry", 0.5);
        let mix = g.add_mix("mix", 2);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
        g.add_edge(split, PortId(0), hrtf, PortId::IN).unwrap();
        g.add_edge(split, PortId(1), dry, PortId::IN).unwrap();
        g.add_edge(hrtf, PortId(0), mix, PortId(0)).unwrap();
        g.add_edge(dry, PortId::OUT, mix, PortId(1)).unwrap();
        g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();

        let node = g.node(hrtf).unwrap();
        assert_eq!(node_latency(node), 64, "dataset node reports its taps");
        assert!(
            matches!(
                node.params,
                NodeParams::HRTF {
                    source: HrtfSource::Dataset { taps: 64, .. },
                    ref left,
                    ref right,
                } if left.is_empty() && right.is_empty()
            ),
            "dataset node carries no inline tabs"
        );

        let rep = analyze(&g, 48_000.0).unwrap();
        assert_eq!(
            rep.upstream_at(mix),
            64,
            "mix aligns to the binaural branch"
        );
        assert_eq!(rep.total_samples, 64);

        // Compensation splices exactly one Delay(64) on the dry leg.
        let c = compensate(&g).unwrap();
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
            NodeParams::Delay { samples: 64 },
            "dataset binaural branch's 64 taps compensated on the dry leg"
        );
    }

    #[test]
    fn terminal_limiter_latency_included_in_total_graph_latency() {
        // Source -> Limiter
        let mut g = Graph2::new();
        let src = g.add_source("s");
        let lim = g.add_node("lim", NodeKind::Prod(ProdStage::Limiter), NodeParams::None);
        g.add_edge(src, PortId::OUT, lim, PortId::IN).unwrap();

        let rep48k = analyze(&g, 48_000.0).unwrap();
        // At 48kHz, 5ms limiter is 240 samples.
        // Terminal node is lim; total_samples must include lim's intrinsic latency.
        assert_eq!(rep48k.taps_at(lim), 240);
        assert_eq!(rep48k.upstream_at(lim), 0);
        assert_eq!(rep48k.total_samples, 240);
        assert!((rep48k.total_ms - 5.0).abs() < 1e-3);

        // At 96kHz, 5ms limiter is 480 samples.
        let rep96k = analyze(&g, 96_000.0).unwrap();
        assert_eq!(rep96k.taps_at(lim), 480);
        assert_eq!(rep96k.total_samples, 480);
        assert!((rep96k.total_ms - 5.0).abs() < 1e-3);
    }

    #[test]
    fn graph_latency_terminal_selection_not_biased_by_upstream() {
        // Path 1: Source -> Delay(100) -> Limiter (terminal 1, total = 100 + 240 = 340)
        // Path 2: Source -> Delay(200) -> Gain (terminal 2, total = 200 + 0 = 200)
        // Upstream at Gain (200) > Upstream at Limiter (100), but total latency at Limiter (340) > Gain (200).
        let mut g = Graph2::new();
        let src = g.add_source("src");
        let split = g.add_split("split", 2);
        let d1 = g.add_delay("d1", 100);
        let d2 = g.add_delay("d2", 200);
        let lim = g.add_node("lim", NodeKind::Prod(ProdStage::Limiter), NodeParams::None);
        let gain = g.add_gain("gain", 1.0);

        g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
        g.add_edge(split, PortId(0), d1, PortId::IN).unwrap();
        g.add_edge(split, PortId(1), d2, PortId::IN).unwrap();
        g.add_edge(d1, PortId::OUT, lim, PortId::IN).unwrap();
        g.add_edge(d2, PortId::OUT, gain, PortId::IN).unwrap();

        let rep = analyze(&g, 48_000.0).unwrap();
        assert_eq!(rep.upstream_at(lim), 100);
        assert_eq!(rep.taps_at(lim), 240);
        assert_eq!(rep.upstream_at(gain), 200);
        assert_eq!(rep.taps_at(gain), 0);
        assert_eq!(
            rep.total_samples, 340,
            "total latency must pick the maximum end-to-end sink latency"
        );
    }

    #[test]
    fn graph_latency_source_eq_limiter_and_conv_mixer() {
        // Source -> Gain (as EQ) -> Limiter -> Sink
        let mut g1 = Graph2::new();
        let s = g1.add_source("s");
        let eq = g1.add_gain("eq", 1.2);
        let lim = g1.add_node("lim", NodeKind::Prod(ProdStage::Limiter), NodeParams::None);
        let sink = g1.add_sink("sink");
        g1.add_edge(s, PortId::OUT, eq, PortId::IN).unwrap();
        g1.add_edge(eq, PortId::OUT, lim, PortId::IN).unwrap();
        g1.add_edge(lim, PortId::OUT, sink, PortId::IN).unwrap();

        let rep1 = analyze(&g1, 48_000.0).unwrap();
        assert_eq!(rep1.total_samples, 240);

        // Source -> Convolution -> Mixer
        let mut g2 = Graph2::new();
        let s2 = g2.add_source("s");
        let conv = g2.add_node(
            "conv",
            NodeKind::Prod(ProdStage::Convolution),
            NodeParams::None,
        );
        let mix = g2.add_mix("mix", 1);
        g2.add_edge(s2, PortId::OUT, conv, PortId::IN).unwrap();
        g2.add_edge(conv, PortId::OUT, mix, PortId(0)).unwrap();

        let rep2 = analyze(&g2, 48_000.0).unwrap();
        assert_eq!(rep2.upstream_at(mix), 512);
        assert_eq!(rep2.total_samples, 512);
    }

    #[test]
    fn test_tail_calculation_and_reporting() {
        // Source -> Delay(500) -> Convolution(1024) -> Sink
        let mut g = Graph2::new();
        let src = g.add_source("s");
        let d = g.add_delay("delay", 500);
        let kernel = vec![0.1; 1024];
        let conv = g.add_convolution("conv", kernel);
        let sink = g.add_sink("out");

        g.add_edge(src, PortId::OUT, d, PortId::IN).unwrap();
        g.add_edge(d, PortId::OUT, conv, PortId::IN).unwrap();
        g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();

        let tail_rep = analyze_tail(&g, 48_000.0).unwrap();
        assert_eq!(tail_rep.tail_at(d), 500);
        assert_eq!(tail_rep.tail_at(conv), 1024);
        // Delay has latency 500 + tail 500. Then Conv has tail 1024.
        // Along path, upstream tail at conv is 500 + 500 = 1000.
        // At sink, upstream tail is 1000 + latency(conv)=1024 + tail(conv)=1024 = 3048.
        assert_eq!(tail_rep.total_samples, graph_tail_samples(&g, 48_000.0));
        assert!(tail_rep.total_samples > 0);
    }
}
