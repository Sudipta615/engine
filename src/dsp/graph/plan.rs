//! Compiled execution plans: the data-driven ordering of DSP stages.
//!
//! `DspGraph` executes a fixed set of node stages (see [`super::GraphNode`]).
//! Rather than hardcoding the call sequence in `process.rs`, the stages are
//! compiled into [`ExecutionPlan`]s — ordered step lists with per-step channel
//! scope — selected by the entry points. Plans are built once at construction
//! and recompiled by `apply_config` (control path only); the audio path only
//! reads them, so zero allocation happens during execution.
//!
//! The stage order in [`PlanSet::compile`] is the single source of truth for
//! the signal chain and must match the pre-plan `process.rs` sequences exactly
//! (pinned by the `tests/fidelity/graph_pipeline_equivalence.rs` suite).

use super::*;

/// Which compiled plan to execute.
///
/// Transport bypass (bit-perfect / DoP) is NOT a plan: the entry points
/// return before any stage runs, so the plans only cover the processing
/// chain. The stereo plan is reused by the ≤2-channel multichannel path
/// (which delegates to [`DspGraph::process_block`](crate::dsp::graph::DspGraph)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlanId {
    /// Stereo pre/post-mix chain (no routing stage).
    Normal,
    /// Multichannel chain: routing on every channel, then pre/post-mix.
    NormalMc,
}

/// Channel scope of one plan step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StepScope {
    /// Run on every plane of the block (preamp, loudness, volume, seek_fade, routing).
    AllChannels,
    /// Run on the front L/R pair only (stereo-linked stages: eq … timestretch).
    FrontPair,
}

/// One ordered execution step: which node, on which channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PlanStep {
    /// Arena slot (see [`super::node_id`]).
    pub(super) node: NodeIdx,
    pub(super) scope: StepScope,
}

impl PlanStep {
    fn new(slot: usize, scope: StepScope) -> Self {
        Self {
            node: NodeIdx(slot),
            scope,
        }
    }
}

/// An ordered list of steps executed per block for one plan.
#[derive(Debug, Default)]
pub(super) struct ExecutionPlan {
    /// Fixed canonical order — never mutated on the audio path.
    pub(super) steps: Vec<PlanStep>,
}

impl ExecutionPlan {
    /// Build a plan from `(arena_slot, scope)` pairs. Control path only.
    fn new(steps: &[(usize, StepScope)]) -> Self {
        Self {
            steps: steps
                .iter()
                .map(|&(slot, scope)| PlanStep::new(slot, scope))
                .collect(),
        }
    }
}

/// The compiled plan set for every processing mode.
#[derive(Debug, Default)]
pub(super) struct PlanSet {
    pub(super) normal: ExecutionPlan,
    pub(super) normal_mc: ExecutionPlan,
}

impl PlanSet {
    /// Compile the canonical stage order. Fixed, and must match the pre-plan
    /// `process.rs` sequences:
    ///
    /// - stereo (`Normal`): `mix → eq → dynamics → convolution → balance →
    ///   crossfeed → stereo → timestretch → volume → seek_fade`. The mix
    ///   step applies every bus input's pre-mix (preamp + loudness) and
    ///   sums them — the Phase-3 S1 replacement for the former
    ///   `out_preamp → out_loudness` steps.
    /// - multichannel (`NormalMc`): the same chain with `routing` (channel
    ///   trim) prepended on every channel, matching the >2-channel path.
    ///
    /// Bit-perfect / DoP bypass is not compiled — the entry points return
    /// before any stage runs (pure passthrough, even on the multichannel
    /// path, matching the pipeline's transport contract).
    pub(super) fn compile() -> Self {
        use node_id::*;
        let stereo_chain = [
            (MIX, StepScope::AllChannels),
            // Phase 6: the aux bus consumes the mix node's send taps and
            // returns into the master BEFORE the post-mix chain runs (the
            // aux return lands in the master front pair, then EQ → … →
            // dither process it like any other mix contribution).
            (AUX, StepScope::FrontPair),
            (EQ, StepScope::FrontPair),
            (DYNAMICS, StepScope::FrontPair),
            (CONVOLUTION, StepScope::FrontPair),
            (BALANCE, StepScope::FrontPair),
            (CROSSFEED, StepScope::FrontPair),
            (STEREO, StepScope::FrontPair),
            (TIMESTRETCH, StepScope::FrontPair),
            (VOLUME, StepScope::AllChannels),
            (SEEK_FADE, StepScope::AllChannels),
        ];
        Self {
            normal: ExecutionPlan::new(&stereo_chain),
            normal_mc: {
                let mut steps = Vec::with_capacity(stereo_chain.len() + 1);
                steps.push(PlanStep::new(ROUTING, StepScope::AllChannels));
                steps.extend(
                    stereo_chain
                        .iter()
                        .map(|&(slot, scope)| PlanStep::new(slot, scope)),
                );
                ExecutionPlan { steps }
            },
        }
    }

    pub(super) fn plan(&self, id: PlanId) -> &ExecutionPlan {
        match id {
            PlanId::Normal => &self.normal,
            PlanId::NormalMc => &self.normal_mc,
        }
    }
}
