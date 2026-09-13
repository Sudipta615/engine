//! The production engine chain expressed as a Graph 2.0 topology
//! (Phase 46).
//!
//! [`build_topology`] constructs the canonical signal chain as a real
//! [`Graph2`] — one [`ProdStage`] node per arena slot, wired head-to-tail
//! with typed audio edges — then validates it and topologically compiles
//! it. The compiled order is the authoritative execution order; the
//! lowering in [`super::lowering`] maps it onto the arena's plan steps.
//!
//! The topology expresses the **multichannel chain** (the `NormalMc`
//! plan): `routing → mix → aux → correction → eq → dynamics →
//! convolution → balance → crossfeed → stereo → timestretch → volume →
//! seek_fade → spatial`. The stereo chain (the `Normal` plan) is the same
//! chain without the `routing` head — the lowering derives both from the
//! one compiled order.
//!
//! The limiter, resampler, and dither nodes are output-domain stages
//! (driven through the dedicated `process_final_limiter*` / output
//! endpoints, not the mix plans) — they appear in the topology as nodes
//! (so the full production surface is described and validated) but no
//! mix-chain edges attach them; the lowering drops them exactly like the
//! hand-authored `PlanSet::compile()` does.

use super::super::{ExecutionOrder, Graph2, NodeId as G2NodeId, PortId, ProdStage};
use crate::dsp::graph2::node::NodeKind;

/// The compiled production topology: the Graph2, its topologically
/// compiled execution order, and the arena-slot mapping.
pub struct ProdTopology {
    /// The typed-port graph (nodes + edges).
    pub graph: Graph2,
    /// The topologically compiled MC-chain order (routing first).
    pub order: ExecutionOrder,
}

impl ProdTopology {
    /// Iterate the stereo-chain steps (MC chain minus the routing head)
    /// in execution order, as `(arena slot, all-channels scope)`.
    pub fn stereo_steps(&self) -> impl Iterator<Item = (usize, bool)> + '_ {
        self.order
            .steps
            .iter()
            .filter(|&&n| self.stage_of(n) != Some(ProdStage::Routing))
            .map(|&n| self.slot_and_scope_of(n))
    }

    /// Iterate the MC-chain steps (routing head first) in execution
    /// order, as `(arena slot, all-channels scope)`.
    pub fn mc_steps(&self) -> impl Iterator<Item = (usize, bool)> + '_ {
        self.order.steps.iter().map(|&n| self.slot_and_scope_of(n))
    }

    /// The stage of a topology node, or `None` for a non-Prod node.
    pub fn stage_of(&self, node: G2NodeId) -> Option<ProdStage> {
        match self.graph.nodes.get(&node).map(|n| n.kind) {
            Some(NodeKind::Prod(stage)) => Some(stage),
            _ => None,
        }
    }

    fn slot_and_scope_of(&self, node: G2NodeId) -> (usize, bool) {
        let stage = self
            .stage_of(node)
            .expect("chain nodes are all Prod stages");
        (stage.slot(), stage.all_channels())
    }
}

/// The multichannel chain (the `NormalMc` plan order), with each stage's
/// channel scope. Must match `PlanSet::compile`'s chains (pinned by the
/// lowering parity unit test).
const MC_CHAIN: &[(ProdStage, bool)] = &[
    (ProdStage::Routing, true),
    (ProdStage::MixBus, true),
    (ProdStage::AuxBus, false),
    (ProdStage::Correction, true),
    (ProdStage::Eq, false),
    (ProdStage::Dynamics, false),
    (ProdStage::Convolution, false),
    (ProdStage::Balance, false),
    (ProdStage::Crossfeed, false),
    (ProdStage::Stereo, false),
    (ProdStage::Timestretch, false),
    (ProdStage::Volume, true),
    (ProdStage::SeekFade, true),
    (ProdStage::Spatial, true),
];

/// The output-domain stages: described + validated in the topology, not
/// wired into the mix chain (driven by the dedicated output endpoints).
const OUTPUT_DOMAIN_STAGES: &[ProdStage] =
    &[ProdStage::Limiter, ProdStage::Resampler, ProdStage::Dither];

/// Build the production topology as a Graph2, validate it, and compile
/// the execution order. Cheap (17 nodes, 14 edges); called at every
/// generation build.
pub fn build_topology() -> ProdTopology {
    let mut g = Graph2::new();

    // The mix chain, head-to-tail. `add_prod` pins the arena slot in the
    // node params and gives the node the production stage name.
    let mut prev: Option<G2NodeId> = None;
    for &(stage, _scope) in MC_CHAIN {
        let id = g.add_prod(stage.stage_name(), stage);
        if let Some(p) = prev {
            g.add_edge(p, PortId::OUT, id, PortId::IN)
                .expect("production chain edge");
        }
        prev = Some(id);
    }

    // Output-domain stages: added unwired (dangling inputs read silence,
    // outputs drop — validate only warns).
    for &stage in OUTPUT_DOMAIN_STAGES {
        g.add_prod(stage.stage_name(), stage);
    }

    let report = g.validate();
    debug_assert!(
        report.is_ok(),
        "production topology must validate: {:?}",
        report.errors
    );
    let order = g.compile().expect("production topology compiles").clone();

    ProdTopology { graph: g, order }
}
