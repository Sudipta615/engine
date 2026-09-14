//! Plan lowering: the compiled Graph2 production topology → the arena
//! [`PlanSet`] (since the **single plan source**).
//!
//! [`lowered_plans`] walks the topologically compiled MC-chain order and
//! emits one [`PlanStep`] per chain node (skipping the output-domain
//! stages):
//!
//! - the **stereo plan** = the chain minus the routing head
//! - the **MC plan** = routing first, then the same chain
//!
//! Because the topological sort is deterministic (tie-breaks on ascending
//! node id, and the chain is linear), the lowered steps form the canonical
//! production stage order, pinned against the frozen `DspPipeline` oracle
//! by the `tests/fidelity/graph_pipeline_equivalence.rs` suite.

use super::super::ProdStage;
use super::topology::{build_topology, ProdTopology};
use crate::dsp::graph2::prod::arena::{PlanSet, PlanStep, StepScope};

/// The lowered steps as `(arena slot, scope)` pairs, for introspection
/// and tests. `None` when the topology fails to compile (a programming
/// error — the topology is a constant).
pub fn lowered_plan_steps() -> Vec<(usize, bool)> {
    let topo = build_topology();
    topo.mc_steps()
        .filter(|&(slot, _)| !is_output_domain(slot))
        .collect()
}

/// Lower the production topology into the arena plan set: the stereo plan
/// (chain minus routing) and the MC plan (routing first). Control path
/// only — called once per generation build.
pub fn lowered_plans() -> PlanSet {
    let steps = lowered_plan_steps();
    let scope = |all: bool| {
        if all {
            StepScope::AllChannels
        } else {
            StepScope::FrontPair
        }
    };

    let stereo: Vec<PlanStep> = steps
        .iter()
        .filter(|&&(slot, _)| slot != ProdStage::Routing.slot())
        .map(|&(slot, all)| PlanStep::from_parts(slot, scope(all)))
        .collect();
    let mc: Vec<PlanStep> = steps
        .iter()
        .map(|&(slot, all)| PlanStep::from_parts(slot, scope(all)))
        .collect();

    PlanSet::from_steps(stereo, mc)
}

/// Whether the arena slot is an output-domain stage the mix plans skip.
fn is_output_domain(slot: usize) -> bool {
    slot == ProdStage::Limiter.slot()
        || slot == ProdStage::Resampler.slot()
        || slot == ProdStage::Dither.slot()
}

/// The topology this lowering targets (tests + diagnostics). Exposed via
/// [`super::Graph2Engine::topology`].
pub(crate) fn topology() -> ProdTopology {
    build_topology()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stereo plan must not contain the routing head; the MC plan
    /// must start with it.
    #[test]
    fn stereo_and_mc_plan_split() {
        let lowered = lowered_plans();
        let routing = ProdStage::Routing.slot();
        assert!(
            !lowered.normal.steps.iter().any(|s| s.node.0 == routing),
            "the stereo plan must skip the routing head"
        );
        assert_eq!(
            lowered.normal_mc.steps.first().map(|s| s.node.0),
            Some(routing),
            "the MC plan must start with routing"
        );
        // The MC plan is exactly one step longer (routing).
        assert_eq!(
            lowered.normal_mc.steps.len(),
            lowered.normal.steps.len() + 1
        );
    }

    /// The lowered chain must cover every non-output-domain arena slot
    /// exactly once (the arena is fixed by construction; the topology must
    /// not drop or duplicate a stage).
    #[test]
    fn lowered_plan_covers_every_arena_slot() {
        let steps = lowered_plan_steps();
        let mut slots: Vec<usize> = steps.iter().map(|&(s, _)| s).collect();
        slots.sort_unstable();
        let expected: Vec<usize> = (0..18usize)
            .filter(|&s| s != 11 && s != 12 && s != 13)
            .collect();
        assert_eq!(slots, expected, "chain slots must appear exactly once");
    }
}
