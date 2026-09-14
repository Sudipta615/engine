//! Compiled execution plans: the data-driven ordering of DSP stages.
//!
//! `DspGraph` executes a fixed set of node stages (see [`super::GraphNode`]).
//! Rather than hardcoding the call sequence in `process.rs`, the stages are
//! compiled into [`ExecutionPlan`]s — ordered step lists with per-step channel
//! scope — selected by the entry points. Plans are built once at construction
//! by the Graph2 lowering (control path only); the audio path only reads
//! them, so zero allocation happens during execution.
//!
//! The lowered stage order is the single source of truth for the signal
//! chain and is pinned against the frozen pipeline oracle by the
//! `tests/fidelity/graph_pipeline_equivalence.rs` suite.

use super::*;

/// Which compiled plan to execute.
///
/// Transport bypass (bit-perfect / DoP) is NOT a plan: the entry points
/// return before any stage runs, so the plans only cover the processing
/// chain. The stereo plan is reused by the ≤2-channel multichannel path
/// (which delegates to [`DspGraph::process_block`](crate::dsp::graph2::prod::arena::DspGraph)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlanId {
    /// Stereo pre/post-mix chain (no routing stage).
    Normal,
    /// Multichannel chain: routing on every channel, then pre/post-mix.
    NormalMc,
}

/// Channel scope of one plan step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StepScope {
    /// Run on every plane of the block (preamp, loudness, volume, seek_fade, routing).
    AllChannels,
    /// Run on the front L/R pair only (stereo-linked stages: eq … timestretch).
    FrontPair,
}

/// One ordered execution step: which node, on which channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlanStep {
    /// Arena slot (see [`super::node_id`]).
    pub(crate) node: NodeIdx,
    pub(crate) scope: StepScope,
}

impl PlanStep {
    /// Build a step from `(arena slot, scope)` — the lowering seam used by
    /// `graph2::prod` (control path only).
    pub(crate) fn from_parts(slot: usize, scope: StepScope) -> Self {
        Self {
            node: NodeIdx(slot),
            scope,
        }
    }
}

/// An ordered list of steps executed per block for one plan.
#[derive(Clone, Debug, Default)]
pub(crate) struct ExecutionPlan {
    /// Fixed canonical order — never mutated on the audio path.
    pub(crate) steps: Vec<PlanStep>,
}

/// The compiled plan set for every processing mode.
#[derive(Clone, Debug, Default)]
pub(crate) struct PlanSet {
    pub(crate) normal: ExecutionPlan,
    pub(crate) normal_mc: ExecutionPlan,
}

impl PlanSet {
    /// Build a plan set from lowered step lists (the `graph2::prod`
    /// seam): `normal` = the stereo chain, `normal_mc` = routing first.
    /// Since this is the **only** construction path — the plan
    /// source is exclusively the Graph2 topology lowering
    /// (`graph2::prod::lowering`). Control path only.
    pub(crate) fn from_steps(normal: Vec<PlanStep>, normal_mc: Vec<PlanStep>) -> Self {
        Self {
            normal: ExecutionPlan { steps: normal },
            normal_mc: ExecutionPlan { steps: normal_mc },
        }
    }

    pub(crate) fn plan(&self, id: PlanId) -> &ExecutionPlan {
        match id {
            PlanId::Normal => &self.normal,
            PlanId::NormalMc => &self.normal_mc,
        }
    }
}
