//! Graph 2.0 edges (v3.27).
//!
//! Edges are **first-class** in Graph 2.0: an [`EdgeDef`] is an explicit,
//! addressable connection from one typed port to another. Unlike the fixed
//! arena chain of `dsp::graph` (where stage order is data but the chain
//! itself is implicit), the topology is *defined* by its edge set — node
//! order only exists after compilation.

use serde::{Deserialize, Serialize};

use super::node::{NodeId, PortId};

/// Stable identity of a graph edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EdgeId(pub u32);

/// One endpoint of an edge: a node and one of its ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EdgeEndpoint {
    pub node: NodeId,
    pub port: PortId,
}

impl EdgeEndpoint {
    pub fn new(node: NodeId, port: PortId) -> Self {
        Self { node, port }
    }
}

/// An explicit directed connection: `source` (an output port) feeds
/// `target` (an input port).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeDef {
    pub id: EdgeId,
    pub source: EdgeEndpoint,
    pub target: EdgeEndpoint,
}
