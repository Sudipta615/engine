//! Graph 2.0 topological scheduling (v3.27).
//!
//! Compiling a topology is exactly the guide's "make stage order data":
//! [`topological_order`] runs a deterministic Kahn's algorithm over the edge
//! set and returns the nodes in execution order — every node appears after
//! all of its producers. The result is the Graph 2.0 analogue of
//! `dsp::graph::plan::ExecutionPlan`: an ordered step list the executor
//! walks per block.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::edge::{EdgeDef, EdgeId};
use super::node::{NodeDef, NodeId};
use super::validate::Graph2Error;

/// The compiled execution order of a topology: a `Vec<NodeId>` where each
/// node appears strictly after every node that feeds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOrder {
    pub steps: Vec<NodeId>,
}

impl ExecutionOrder {
    /// True when the node is absent from the order (it cannot run).
    pub fn contains(&self, node: NodeId) -> bool {
        self.steps.contains(&node)
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// Compute the deterministic topological order, or report the cycle that
/// prevents one. Determinism comes from tie-breaking on ascending [`NodeId`]
/// among the currently-ready nodes: identical topologies always compile to
/// identical orders (and therefore identical renders).
pub fn topological_order(
    nodes: &BTreeMap<NodeId, NodeDef>,
    edges: &BTreeMap<EdgeId, EdgeDef>,
) -> Result<ExecutionOrder, Graph2Error> {
    // In-degree = number of incoming edges (fan-in rule guarantees ≤ 1 per
    // input port, but a node may still have several inputs).
    let mut indegree: HashMap<NodeId, usize> = HashMap::new();
    let mut producers: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for node in nodes.keys() {
        indegree.insert(*node, 0);
    }
    for edge in edges.values() {
        if !nodes.contains_key(&edge.source.node) || !nodes.contains_key(&edge.target.node) {
            // Left for validate(); ignore here so sorting stays total over
            // whatever subset is present.
            continue;
        }
        *indegree.entry(edge.target.node).or_insert(0) += 1;
        producers
            .entry(edge.target.node)
            .or_default()
            .push(edge.source.node);
    }

    // Ready set = zero in-degree, tie-broken by ascending NodeId.
    let mut ready: BTreeSet<NodeId> = nodes
        .keys()
        .filter(|n| indegree.get(n).copied().unwrap_or(0) == 0)
        .copied()
        .collect();

    let mut steps = Vec::with_capacity(nodes.len());
    while let Some(&n) = ready.iter().next() {
        ready.remove(&n);
        steps.push(n);
        for (target, producers_of) in producers.iter() {
            if producers_of.contains(&n) {
                let d = indegree.get_mut(target).expect("known node");
                *d -= 1;
                if *d == 0 {
                    ready.insert(*target);
                }
            }
        }
    }

    if steps.len() != nodes.len() {
        // Nodes left out form a cycle — reuse the DFS for the diagnostic path.
        if let Some(cycle) = super::validate::find_cycle(nodes, edges) {
            return Err(Graph2Error::Cycle(cycle));
        }
        return Err(Graph2Error::Cycle(Vec::new()));
    }

    Ok(ExecutionOrder { steps })
}
