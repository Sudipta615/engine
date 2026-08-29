//! # Graph 2.0 — a general-purpose audio graph topology (v3.27)
//!
//! The guide's v3.27 milestone: **make the graph the true center of the
//! rendering engine** by turning the fixed track/bus chain of
//! [`crate::dsp::graph::DspGraph`] into an *arbitrary topology* runtime.
//! Where `dsp::graph` is a canonical arena of stages whose order is data but
//! whose chain is implicit, [`Graph2`] is a model of **explicit structure**:
//!
//! ```text
//!  input ──┬─▶ Gain ─────────────────┐
//!          │                         ├─▶ Mix ──▶ Sink
//!          └─▶ Delay(512) ──▶ Gain ──┘
//! ```
//!
//! Every node declares **explicit input/output ports** with typed-bus
//! metadata ([`PortSpec`]: signal class + channel count). Every connection
//! is a **first-class edge** ([`EdgeDef`]) from one typed port to another.
//! The topology is *defined* by its edge set; execution order is derived,
//! never authored.
//!
//! ## Lifecycle
//!
//! 1. **Build** — [`Graph2::add_source`] / `add_gain` / `add_delay` /
//!    `add_mix` / `add_split` / `add_sink`, then wire with
//!    [`Graph2::add_edge`] (which fails fast on endpoint, direction and
//!    typed-bus violations). Mutating is always legal — `add_edge` of a
//!    cycle merely builds, never runs.
//! 2. **Validate** — [`Graph2::validate`] returns a
//!    [`ValidationReport`]: structural errors (including the *cycle path*
//!    from a grey/white/black DFS) plus dangling-port warnings.
//! 3. **Compile** — [`Graph2::compile`] runs the deterministic
//!    topological sort ([`topological_order`], Kahn's with ascending-id
//!    tie-break) and caches the [`ExecutionOrder`]. Any mutation
//!    invalidates the cache, so *dynamic graph recompilation* is just
//!    "mutate then compile again".
//! 4. **Execute** — [`OfflineExecutor`] renders the compiled order block by
//!    block through the built-in node ops.
//!
//! ## Module map
//!
//! - `node.rs` — [`NodeId`], [`PortId`], [`PortSpec`], [`NodeKind`],
//!   [`NodeCapabilities`], [`NodeParams`], [`NodeDef`]
//! - `edge.rs` — [`EdgeId`], [`EdgeEndpoint`], [`EdgeDef`]
//! - `validate.rs` — [`Graph2Error`], [`ValidationReport`], cycle detection
//! - `sort.rs` — [`ExecutionOrder`], [`topological_order`]
//! - `exec.rs` — [`OfflineExecutor`] (renders arbitrary topologies offline)
//!
//! ## Discipline
//!
//! Like the acoustic layer, Graph 2.0 is **offline-first by design**: the
//! topology, validation and executor are control/offline path and heap-happy
//! (building and compiling are exactly the expensive work an offline engine
//! can afford). The realtime `dsp::graph` hot path is untouched; a future
//! milestone lowers a compiled [`ExecutionOrder`] onto a realtime plan.
//! No allocation or lock is added to any audio thread.

pub mod edge;
pub mod exec;
pub mod latency;
pub mod node;
pub mod sort;
pub mod validate;

pub use edge::{EdgeDef, EdgeEndpoint, EdgeId};
pub use exec::OfflineExecutor;
pub use latency::{analyze, compensate, node_latency, LatencyReport};
pub use node::{
    NodeCapabilities, NodeDef, NodeId, NodeKind, NodeParams, PortDirection, PortId, PortSpec,
    SignalType, SourceParams, TestSignal,
};
pub use sort::{topological_order, ExecutionOrder};
pub use validate::{validate, Graph2Error, ValidationReport};

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The general-purpose audio graph topology (v3.27).
///
/// Owns the node and edge stores; every mutation keeps the graph *buildable*
/// (a cycle can be drawn; bad edges are rejected at draw time), while
/// [`Graph2::validate`] / [`Graph2::compile`] decide whether it is
/// *executable*. Deterministic iteration order (BTreeMap) makes
/// serialization, inspection and compiled orders reproducible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Graph2 {
    pub nodes: BTreeMap<NodeId, NodeDef>,
    pub edges: BTreeMap<EdgeId, EdgeDef>,
    next_node: u32,
    next_edge: u32,
    /// Last compiled order; invalidated by every mutation.
    #[serde(skip)]
    order: Option<ExecutionOrder>,
}

impl Graph2 {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Construction ────────────────────────────────────────────────────────

    /// Add a node with the canonical port shapes of its [`NodeKind`].
    fn add_node(&mut self, name: &str, kind: node::NodeKind, params: node::NodeParams) -> NodeId {
        let (inputs, outputs) = match kind {
            node::NodeKind::Source => (vec![], vec![PortSpec::output(SignalType::Audio, 1)]),
            node::NodeKind::Sink => (vec![PortSpec::input(SignalType::Audio, 0)], vec![]),
            node::NodeKind::Gain | node::NodeKind::Delay => (
                vec![PortSpec::input(SignalType::Audio, 1)],
                vec![PortSpec::output(SignalType::Audio, 1)],
            ),
            node::NodeKind::Mix => (vec![], vec![PortSpec::output(SignalType::Audio, 0)]),
            node::NodeKind::Split => (vec![PortSpec::input(SignalType::Audio, 0)], vec![]),
        };
        let id = NodeId(self.next_node);
        self.next_node += 1;
        self.nodes.insert(
            id,
            NodeDef {
                id,
                name: name.to_string(),
                kind,
                params,
                inputs,
                outputs,
            },
        );
        self.order = None;
        id
    }

    /// A signal generator.
    pub fn add_source(&mut self, name: &str) -> NodeId {
        self.add_node(
            name,
            NodeKind::Source,
            node::NodeParams::Source(Default::default()),
        )
    }

    /// A signal generator with an explicit test signal.
    pub fn add_source_with(&mut self, name: &str, params: SourceParams) -> NodeId {
        self.add_node(name, NodeKind::Source, node::NodeParams::Source(params))
    }

    /// A capture point (no outputs; the executor accumulates what arrives).
    pub fn add_sink(&mut self, name: &str) -> NodeId {
        self.add_node(name, NodeKind::Sink, node::NodeParams::Sink)
    }

    /// A 1:1 gain stage.
    pub fn add_gain(&mut self, name: &str, gain: f32) -> NodeId {
        self.add_node(name, NodeKind::Gain, node::NodeParams::Gain { gain })
    }

    /// A 1:1 delay of `samples` frames.
    pub fn add_delay(&mut self, name: &str, samples: u32) -> NodeId {
        self.add_node(name, NodeKind::Delay, node::NodeParams::Delay { samples })
    }

    /// A `N`-input → 1-output summing bus. Ports `0..N` are created in
    /// order, so a host wires `add_edge(a, OUT, mix, PortId(0))` etc.
    pub fn add_mix(&mut self, name: &str, inputs: usize) -> NodeId {
        let id = self.add_node(name, NodeKind::Mix, node::NodeParams::None);
        let node = self.nodes.get_mut(&id).expect("just inserted");
        node.inputs = (0..inputs)
            .map(|_| PortSpec::input(SignalType::Audio, 0))
            .collect();
        id
    }

    /// A 1-input → `N`-output broadcast. Outputs `0..N` are created in
    /// order, so a host wires `add_edge(split, PortId(k), out, IN)`.
    pub fn add_split(&mut self, name: &str, outputs: usize) -> NodeId {
        let id = self.add_node(name, NodeKind::Split, node::NodeParams::None);
        let node = self.nodes.get_mut(&id).expect("just inserted");
        node.outputs = (0..outputs)
            .map(|_| PortSpec::output(SignalType::Audio, 0))
            .collect();
        id
    }

    /// Insert a fully custom [`NodeDef`] (host-defined port shapes — e.g. a
    /// Control-typed input port to exercise typed-bus validation). The id
    /// must be fresh.
    pub fn add_node_raw(&mut self, def: NodeDef) -> Result<(), Graph2Error> {
        if self.nodes.contains_key(&def.id) {
            return Err(Graph2Error::NodeExists(def.id));
        }
        self.next_node = self.next_node.max(def.id.0 + 1);
        self.nodes.insert(def.id, def);
        self.order = None;
        Ok(())
    }

    /// Connect `source`'s output port to `target`'s input port. Fails fast
    /// on unknown endpoints, wrong direction, typed-bus mismatch, or a
    /// second edge into the same input port. Cycles are allowed to be
    /// *drawn*; `validate` reports them at compile time.
    pub fn add_edge(
        &mut self,
        source: NodeId,
        source_port: PortId,
        target: NodeId,
        target_port: PortId,
    ) -> Result<EdgeId, Graph2Error> {
        let src = self
            .nodes
            .get(&source)
            .ok_or(Graph2Error::UnknownNode(source))?;
        let src_port = src.output(source_port).ok_or(Graph2Error::UnknownPort(
            source,
            source_port,
            node::PortDirection::Output,
        ))?;
        let dst = self
            .nodes
            .get(&target)
            .ok_or(Graph2Error::UnknownNode(target))?;
        let dst_port = dst.input(target_port).ok_or(Graph2Error::UnknownPort(
            target,
            target_port,
            node::PortDirection::Input,
        ))?;
        if src_port.signal != dst_port.signal {
            return Err(Graph2Error::SignalMismatch(
                src_port.signal,
                dst_port.signal,
            ));
        }
        // One incoming edge per input port.
        let already = self
            .edges
            .values()
            .any(|e| e.target.node == target && e.target.port == target_port);
        if already {
            return Err(Graph2Error::DuplicateConnection);
        }
        let id = EdgeId(self.next_edge);
        self.next_edge += 1;
        self.edges.insert(
            id,
            EdgeDef {
                id,
                source: edge::EdgeEndpoint::new(source, source_port),
                target: edge::EdgeEndpoint::new(target, target_port),
            },
        );
        self.order = None;
        Ok(id)
    }

    /// Remove an edge by id (unknown id is a no-op).
    pub fn remove_edge(&mut self, id: EdgeId) {
        if self.edges.remove(&id).is_some() {
            self.order = None;
        }
    }

    /// Remove a node. Fails when edges still reference it (the ownership
    /// rule: a node's connections must be torn down first).
    pub fn remove_node(&mut self, id: NodeId) -> Result<(), Graph2Error> {
        if !self.nodes.contains_key(&id) {
            return Ok(());
        }
        if self
            .edges
            .values()
            .any(|e| e.source.node == id || e.target.node == id)
        {
            return Err(Graph2Error::NodeHasEdges(id));
        }
        self.nodes.remove(&id);
        self.order = None;
        Ok(())
    }

    /// Replace a node's parameters (e.g. retune a gain). Unknown node is a
    /// no-op; mutation invalidates the compiled order.
    pub fn set_params(&mut self, id: NodeId, params: node::NodeParams) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.params = params;
            self.order = None;
        }
    }

    // ── Query ───────────────────────────────────────────────────────────────

    pub fn node(&self, id: NodeId) -> Option<&NodeDef> {
        self.nodes.get(&id)
    }

    pub fn edge(&self, id: EdgeId) -> Option<&EdgeDef> {
        self.edges.get(&id)
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// The edge feeding `node`'s input port, if any.
    pub fn incoming(&self, node: NodeId, port: PortId) -> Option<&EdgeDef> {
        self.edges
            .values()
            .find(|e| e.target.node == node && e.target.port == port)
    }

    /// All edges leaving `node`'s output port.
    pub fn outgoing(&self, node: NodeId, port: PortId) -> impl Iterator<Item = &EdgeDef> {
        self.edges
            .values()
            .filter(move |e| e.source.node == node && e.source.port == port)
    }

    // ── Validation & compilation ────────────────────────────────────────────

    /// Run the full structural validation (endpoints, direction, typed
    /// buses, fan-in, cycles, dangling ports). Cheap; call any time.
    pub fn validate(&self) -> ValidationReport {
        validate(&self.nodes, &self.edges)
    }

    /// Validate and, if legal, compute and cache the deterministic
    /// topological [`ExecutionOrder`]. Mutate-then-`compile` again for
    /// dynamic graph recompilation.
    pub fn compile(&mut self) -> Result<&ExecutionOrder, Graph2Error> {
        let report = self.validate();
        if let Some(err) = report.first_error() {
            return Err(err.clone());
        }
        let order = topological_order(&self.nodes, &self.edges)?;
        self.order = Some(order);
        Ok(self.order.as_ref().expect("just set"))
    }

    /// The last compiled order, if any (does not recompile).
    pub fn order(&self) -> Option<&ExecutionOrder> {
        self.order.as_ref()
    }

    // ── Inspection & serialization ──────────────────────────────────────────

    /// Render the topology as a Graphviz `digraph` (dot) listing, for
    /// graph inspection in any dot viewer.
    pub fn to_dot(&self) -> String {
        let mut out = String::from("digraph audio_graph {\n  rankdir=LR;\n");
        for n in self.nodes.values() {
            out.push_str(&format!(
                "  n{} [label=\"{}\"];\n",
                n.id.0,
                escape_dot(&format!("{} ({})", n.name, node_kind_label(n.kind)))
            ));
        }
        for e in self.edges.values() {
            out.push_str(&format!(
                "  n{} -> n{} [label=\"p{}\"];\n",
                e.source.node.0, e.target.node.0, e.target.port.0
            ));
        }
        out.push('}');
        out
    }
}

fn node_kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Source => "src",
        NodeKind::Sink => "sink",
        NodeKind::Gain => "gain",
        NodeKind::Delay => "delay",
        NodeKind::Mix => "mix",
        NodeKind::Split => "split",
    }
}

fn escape_dot(s: &str) -> String {
    s.replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_wire_compile_roundtrip() {
        let mut g = Graph2::new();
        let src = g.add_source("tone");
        let gain = g.add_gain("vol", 0.5);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
        g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();

        let report = g.validate();
        assert!(report.is_ok(), "{report:?}");

        let order = g.compile().unwrap();
        assert_eq!(order.steps, vec![src, gain, sink]);
        assert_eq!(g.order().unwrap().len(), 3);

        // Mutating invalidates the cache.
        let sink2 = g.add_sink("out2");
        g.add_edge(gain, PortId::OUT, sink2, PortId::IN).unwrap();
        assert!(g.order().is_none());
        assert_eq!(g.compile().unwrap().len(), 4);
    }

    #[test]
    fn serialization_roundtrip() {
        let mut g = Graph2::new();
        let src = g.add_source("s");
        let d = g.add_delay("d", 10);
        let sink = g.add_sink("k");
        g.add_edge(src, PortId::OUT, d, PortId::IN).unwrap();
        g.add_edge(d, PortId::OUT, sink, PortId::IN).unwrap();

        let json = serde_json::to_string(&g).unwrap();
        let back: Graph2 = serde_json::from_str(&json).unwrap();
        assert_eq!(back.node_count(), 3);
        assert_eq!(back.edge_count(), 2);
        // Compile identically.
        let mut a = g;
        let mut b = back;
        assert_eq!(a.compile().unwrap(), b.compile().unwrap());
    }

    #[test]
    fn remove_node_requires_teardown() {
        let mut g = Graph2::new();
        let src = g.add_source("s");
        let sink = g.add_sink("k");
        g.add_edge(src, PortId::OUT, sink, PortId::IN).unwrap();
        assert_eq!(g.remove_node(src), Err(Graph2Error::NodeHasEdges(src)));
        g.remove_edge(g.incoming(sink, PortId::IN).unwrap().id);
        g.remove_node(src).unwrap();
        assert_eq!(g.node_count(), 1);
    }

    #[test]
    fn to_dot_lists_nodes_and_edges() {
        let mut g = Graph2::new();
        let src = g.add_source("s");
        let sink = g.add_sink("k");
        g.add_edge(src, PortId::OUT, sink, PortId::IN).unwrap();
        let dot = g.to_dot();
        assert!(dot.contains("digraph audio_graph"));
        assert!(dot.contains("n0 -> n1"));
    }
}
