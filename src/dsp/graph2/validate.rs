//! Graph 2.0 validation (v3.27).
//!
//! [`validate`] turns a topology into a [`ValidationReport`]: hard **errors**
//! (structural illegality — bad endpoints, wrong port direction, typed-bus
//! mismatches, duplicate fan-in, cycles) that must be fixed before
//! execution, and **warnings** (dangling ports) the executor tolerates by
//! treating unconnected inputs as silence and dropping unconnected outputs.
//! Cycle detection is a grey/white/black DFS that reports the **actual
//! cycle path** — a compile error the host can surface verbatim.

use std::collections::HashMap;

use super::edge::{EdgeDef, EdgeId};
use super::node::{NodeDef, NodeId, PortDirection, SignalType};

/// A structural problem that prevents a topology from compiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Graph2Error {
    /// The named node does not exist.
    UnknownNode(NodeId),
    /// The named edge does not exist.
    UnknownEdge(EdgeId),
    /// The node has no port with that id (or it has the wrong direction).
    UnknownPort(NodeId, super::node::PortId, PortDirection),
    /// The two ports carry different [`SignalType`]s (typed-bus violation).
    SignalMismatch(SignalType, SignalType),
    /// An input port already has an incoming edge (fan-in requires a Mix).
    /// Direction is structurally enforced by the builder (source endpoints
    /// are always looked up in the output list, targets in the input list),
    /// so no separate WrongDirection variant exists.
    DuplicateConnection,
    /// A node cannot be removed while edges still reference it.
    NodeHasEdges(NodeId),
    /// A node with that id already exists.
    NodeExists(NodeId),
    /// The graph contains a directed cycle; the listed nodes form it.
    Cycle(Vec<NodeId>),
}

impl std::fmt::Display for Graph2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Graph2Error::UnknownNode(id) => write!(f, "unknown node {id:?}"),
            Graph2Error::UnknownEdge(id) => write!(f, "unknown edge {id:?}"),
            Graph2Error::UnknownPort(n, p, d) => {
                write!(f, "node {n:?} has no {d:?} port {p:?}")
            }
            Graph2Error::SignalMismatch(a, b) => {
                write!(f, "typed-bus mismatch: {a:?} cannot feed {b:?}")
            }
            Graph2Error::DuplicateConnection => {
                write!(
                    f,
                    "an input port already has an incoming edge (use a Mix for fan-in)"
                )
            }
            Graph2Error::NodeHasEdges(id) => {
                write!(f, "node {id:?} still has attached edges")
            }
            Graph2Error::NodeExists(id) => {
                write!(f, "node {id:?} already exists")
            }
            Graph2Error::Cycle(path) => {
                write!(
                    f,
                    "directed cycle detected: {}",
                    path.iter()
                        .map(|n| format!("{n:?}"))
                        .collect::<Vec<_>>()
                        .join(" -> ")
                )
            }
        }
    }
}

impl std::error::Error for Graph2Error {}

/// The outcome of validating a topology.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// Structural problems that must be resolved before execution.
    pub errors: Vec<Graph2Error>,
    /// Non-fatal observations (dangling ports, unused sources).
    pub warnings: Vec<String>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn is_err(&self) -> bool {
        !self.is_ok()
    }

    /// The first error, if any.
    pub fn first_error(&self) -> Option<&Graph2Error> {
        self.errors.first()
    }
}

/// Run every structural check over `(nodes, edges)`.
///
/// Checks are cheap and allocation-free on the happy path (only diagnostics
/// allocate): endpoint existence, output→input direction, signal-type
/// equality, one-edge-per-input-port, and cycle detection.
pub fn validate(
    nodes: &std::collections::BTreeMap<NodeId, NodeDef>,
    edges: &std::collections::BTreeMap<EdgeId, EdgeDef>,
) -> ValidationReport {
    let mut report = ValidationReport::default();

    // ── Endpoint + direction + typed-bus checks ──
    for edge in edges.values() {
        let src = match nodes.get(&edge.source.node) {
            Some(n) => n,
            None => {
                report
                    .errors
                    .push(Graph2Error::UnknownNode(edge.source.node));
                continue;
            }
        };
        let dst = match nodes.get(&edge.target.node) {
            Some(n) => n,
            None => {
                report
                    .errors
                    .push(Graph2Error::UnknownNode(edge.target.node));
                continue;
            }
        };

        let src_port = match src.output(edge.source.port) {
            Some(p) => p,
            None => {
                report.errors.push(Graph2Error::UnknownPort(
                    edge.source.node,
                    edge.source.port,
                    PortDirection::Output,
                ));
                continue;
            }
        };
        let dst_port = match dst.input(edge.target.port) {
            Some(p) => p,
            None => {
                report.errors.push(Graph2Error::UnknownPort(
                    edge.target.node,
                    edge.target.port,
                    PortDirection::Input,
                ));
                continue;
            }
        };

        if src_port.signal != dst_port.signal {
            report.errors.push(Graph2Error::SignalMismatch(
                src_port.signal,
                dst_port.signal,
            ));
        }
    }

    // ── One incoming edge per input port (fan-in requires an explicit Mix) ──
    let mut incoming: HashMap<(NodeId, super::node::PortId), usize> = HashMap::new();
    for edge in edges.values() {
        let key = (edge.target.node, edge.target.port);
        let n = incoming.entry(key).or_insert(0);
        *n += 1;
    }
    for (_, count) in incoming {
        if count > 1 {
            report.errors.push(Graph2Error::DuplicateConnection);
        }
    }

    // ── Cycle detection (grey/white/black DFS over the edge set) ──
    if let Some(cycle) = find_cycle(nodes, edges) {
        report.errors.push(Graph2Error::Cycle(cycle));
    }

    // ── Warnings: dangling ports ──
    let mut has_incoming: HashMap<(NodeId, super::node::PortId), bool> = HashMap::new();
    let mut has_outgoing: HashMap<(NodeId, super::node::PortId), bool> = HashMap::new();
    for edge in edges.values() {
        has_incoming.insert((edge.target.node, edge.target.port), true);
        has_outgoing.insert((edge.source.node, edge.source.port), true);
    }
    for node in nodes.values() {
        for (i, _port) in node.inputs.iter().enumerate() {
            if !has_incoming.contains_key(&(node.id, super::node::PortId(i as u32))) {
                report.warnings.push(format!(
                    "input port {:?} of {} is unconnected (treated as silence)",
                    super::node::PortId(i as u32),
                    node.name
                ));
            }
        }
        for (i, _port) in node.outputs.iter().enumerate() {
            if !has_outgoing.contains_key(&(node.id, super::node::PortId(i as u32))) {
                report.warnings.push(format!(
                    "output port {:?} of {} is unconnected (dropped)",
                    super::node::PortId(i as u32),
                    node.name
                ));
            }
        }
    }

    report
}

/// Detect a directed cycle over the edge set, returning the node path that
/// closes the loop (`A -> B -> A`), or `None` if the graph is acyclic.
/// Grey/white/black DFS: grey = on the current recursion stack, black =
/// fully explored. Only the *first* cycle found is reported.
pub fn find_cycle(
    nodes: &std::collections::BTreeMap<NodeId, NodeDef>,
    edges: &std::collections::BTreeMap<EdgeId, EdgeDef>,
) -> Option<Vec<NodeId>> {
    // Adjacency: source node → target nodes.
    let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for edge in edges.values() {
        if nodes.contains_key(&edge.source.node) && nodes.contains_key(&edge.target.node) {
            adj.entry(edge.source.node)
                .or_default()
                .push(edge.target.node);
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Grey,
        Black,
    }

    fn dfs(
        n: NodeId,
        adj: &HashMap<NodeId, Vec<NodeId>>,
        color: &mut HashMap<NodeId, Color>,
        stack: &mut Vec<NodeId>,
    ) -> Option<Vec<NodeId>> {
        color.insert(n, Color::Grey);
        stack.push(n);
        if let Some(nexts) = adj.get(&n) {
            for &m in nexts {
                match color.get(&m).copied().unwrap_or(Color::White) {
                    Color::Grey => {
                        // m is on the stack: the cycle is stack[idx(m)..] + m.
                        let idx = stack.iter().position(|&x| x == m).unwrap_or(0);
                        let mut cycle = stack[idx..].to_vec();
                        cycle.push(m);
                        return Some(cycle);
                    }
                    Color::White => {
                        if let Some(cycle) = dfs(m, adj, color, stack) {
                            return Some(cycle);
                        }
                    }
                    Color::Black => {}
                }
            }
        }
        stack.pop();
        color.insert(n, Color::Black);
        None
    }

    let mut color: HashMap<NodeId, Color> = HashMap::new();
    let mut stack = Vec::new();
    for &n in nodes.keys() {
        if color.get(&n).copied().unwrap_or(Color::White) == Color::White {
            if let Some(cycle) = dfs(n, &adj, &mut color, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::graph2::Graph2;

    #[test]
    fn reports_typed_bus_mismatch() {
        // A host-defined node with a Control-typed input port: wiring an
        // Audio source into it must be rejected by the typed-bus rule.
        let mut g = Graph2::new();
        let src = g.add_source("s");
        let ctl = super::super::node::NodeDef {
            id: super::super::node::NodeId(1),
            name: "ctrl_in".to_string(),
            kind: super::super::node::NodeKind::Sink,
            params: super::super::node::NodeParams::Sink,
            inputs: vec![super::super::node::PortSpec::input(SignalType::Control, 1)],
            outputs: vec![],
        };
        g.add_node_raw(ctl).unwrap();
        let edge = g.add_edge(
            src,
            super::super::node::PortId::OUT,
            super::super::node::NodeId(1),
            super::super::node::PortId::IN,
        );
        assert_eq!(
            edge,
            Err(Graph2Error::SignalMismatch(
                SignalType::Audio,
                SignalType::Control
            ))
        );
    }

    #[test]
    fn detects_cycles_with_path() {
        let mut g = Graph2::new();
        // Simplest true cycle: delay(d1) → delay(d2) → delay(d1).
        let a = g.add_delay("a", 10);
        let b = g.add_delay("b", 10);
        g.add_edge(
            a,
            super::super::node::PortId::OUT,
            b,
            super::super::node::PortId::IN,
        )
        .unwrap();
        g.add_edge(
            b,
            super::super::node::PortId::OUT,
            a,
            super::super::node::PortId::IN,
        )
        .unwrap();
        let report = g.validate();
        assert!(report.is_err());
        let err = report.first_error().unwrap();
        match err {
            Graph2Error::Cycle(path) => {
                assert_eq!(path.len(), 3, "cycle path closes the loop: {path:?}");
                assert_eq!(path[0], path[2], "path returns to the start node");
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }
}
