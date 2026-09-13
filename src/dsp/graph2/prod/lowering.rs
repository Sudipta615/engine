//! Plan lowering: the compiled Graph2 production topology → the arena
//! [`PlanSet`] (Phase 46).
//!
//! [`lowered_plans`] walks the topologically compiled MC-chain order and
//! emits one [`PlanStep`] per chain node (skipping the output-domain
//! stages, exactly like the hand-authored `PlanSet::compile()`):
//!
//! - the **stereo plan** = the chain minus the routing head
//! - the **MC plan** = routing first, then the same chain
//!
//! Because the topological sort is deterministic (tie-breaks on ascending
//! node id, and the chain is linear), the lowered steps are identical to
//! `PlanSet::compile()`'s hand-authored order — pinned by the
//! `lowered_plans_match_handauthored` unit test — so a
//! `Graph2Engine` generation is bit-equivalent to a `DspGraph` generation
//! by construction: same arena, same config application, same user-state
//! replay, same step order.

use super::super::ProdStage;
use super::topology::{build_topology, ProdTopology};
use crate::dsp::graph::{PlanStep, ProdPlanSet as PlanSet, StepScope};

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

    /// The pinned invariant of Phase 46: the Graph2-lowered plan set must
    /// be **identical** to the hand-authored `PlanSet::compile()` — the
    /// lowered plans replace the authored ones as the plan source, and any
    /// divergence is a bit-exactness break.
    #[test]
    fn lowered_plans_match_handauthored() {
        let authored = PlanSet::compile();
        let lowered = lowered_plans();
        let slot = |s: &crate::dsp::graph::PlanStep| s.node.0;
        for (a, b) in authored
            .normal
            .steps
            .iter()
            .zip(lowered.normal.steps.iter())
        {
            assert_eq!(slot(a), slot(b), "stereo plan slot mismatch");
            assert_eq!(a.scope, b.scope, "stereo plan scope mismatch");
        }
        assert_eq!(
            authored.normal.steps.len(),
            lowered.normal.steps.len(),
            "stereo plan length mismatch"
        );
        for (a, b) in authored
            .normal_mc
            .steps
            .iter()
            .zip(lowered.normal_mc.steps.iter())
        {
            assert_eq!(slot(a), slot(b), "mc plan slot mismatch");
            assert_eq!(a.scope, b.scope, "mc plan scope mismatch");
        }
        assert_eq!(
            authored.normal_mc.steps.len(),
            lowered.normal_mc.steps.len(),
            "mc plan length mismatch"
        );
    }

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
}
