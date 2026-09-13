//! Phase 46 (v3.51.0) — the production engine on Graph 2.0.
//!
//! This module is the seam between Graph 2.0's typed-port topology and the
//! production node arena: the **one node implementation** the engine runs
//! (mix bus, aux, correction, EQ, … — the full 17-slot arena) lives in
//! [`arena`]; the plan source is Graph 2.0. Every generation this engine
//! builds carries plans *lowered* from a real Graph2 topology:
//!
//! ```text
//!   EngineConfig ──▶ topology() ──▶ Graph2 (typed ports, edges)
//!                        │  validate + topological compile
//!                        ▼
//!                lowered PlanSet (stereo + MC chains)
//!                        │  GraphGeneration::build_with_plans
//!                        ▼
//!           Graph2Engine (arena + control bus + publish/swap/retire)
//! ```
//!
//! Bit-exactness is by construction: the lowered plan is the single plan
//! source, and the arena, node configuration, user-state replay, control
//! queues, and swap machinery are the objects the engine has always run.
//! The frozen `DspPipeline` oracle pins the chain externally through
//! `tests/fidelity/graph_pipeline_equivalence.rs`.
//!
//! ## Phase 48 (v4.0.0) — legacy `dsp::graph` removal
//!
//! The former public `dsp::graph` module is gone: its arena, plan, nodes,
//! and handlers moved here as the crate-private [`arena`] (the single node
//! implementation, now an internal of `graph2::prod`), the hand-authored
//! plan source was deleted (Graph2 lowering is the only plan source), and
//! the Phase-47 shadow mode was removed with its `graph2_shadow_verify`
//! config flag. Hosts reach the surface through the `graph2::prod`
//! re-exports (previously `dsp::graph` exports).
//!
//! ## Module map (the house split)
//!
//! - `arena/` — the production node arena: the 17-slot `DspGraph`, its
//!   compiled-plan execution, `GraphGeneration` swaps, the per-node SPSC
//!   control queues, and the node implementations (`nodes/`)
//! - `topology.rs` — the production chain as a Graph2 graph (typed ports,
//!   edges, validation, topological compile)
//! - `lowering.rs` — the compiled topology → the arena `PlanSet`
//! - `control.rs` — [`Graph2ControlHandle`]: the cloneable cross-thread
//!   surface (mirrors the arena's queued control plane one-to-one)
//! - `process.rs` — the block entry points
//! - `controls.rs` — the queued control mutators (each forwards to the
//!   active graph)
//! - `mod.rs` (this file) — the [`Graph2Engine`] struct and construction

use std::fmt;

mod arena;
mod control;
mod controls;
mod lowering;
mod process;
mod topology;

pub use arena::nodes::{
    AutomationPoint, AutomationTarget, AuxBusNode, BalanceNode, ConvolutionNode, CorrectionNode,
    CorrectionNodeInfo, CrossfeedNode, DitherNode, DuckState, DynamicsNode, EqNode, GainNode,
    LimiterNode, LoudnessNode, MixBusNode, MixInput, MixInputCmd, MixTransitionCmd, PanLaw,
    ResamplerNode, RoutingNode, SeekFadeNode, SpatialNode, StereoNode, TimeStretchNode,
    MAX_AUTOMATION_POINTS, MAX_DUCK_TARGETS, MAX_MIX_SLOTS,
};
pub use arena::{DspGraph, DspNode, GraphControlHandle, GraphGeneration, GraphScratch};
pub use control::Graph2ControlHandle;

/// The production engine on Graph 2.0.
///
/// The shell owns one arena graph ([`DspGraph`] — the arena + control bus +
/// swap machinery, the single node implementation) whose generations carry
/// **Graph2-lowered** plans. Everything else (process entry points, control
/// surface, accessors, reports) is the arena's — forwarded verbatim — so
/// audio behavior is unchanged.
///
/// Read-only accessors (`volume()`, `eq()`, `graph_nodes()`, …) resolve
/// through `Deref` to the embedded graph. Mutating methods exist
/// explicitly on this type ([`Self::with_graph`] for accessor-style
/// mutations).
pub struct Graph2Engine {
    /// The production arena + control machinery — the single node
    /// implementation. Its generations carry Graph2-lowered plans.
    pub(crate) inner: DspGraph,
}

impl Graph2Engine {
    /// Build the engine from config: lower the Graph2 production topology
    /// to plans and construct the initial generation with them.
    pub fn from_config(config: &config::EngineConfig, sample_rate: f32) -> Self {
        let plans = lowering::lowered_plans();
        Self {
            inner: DspGraph::from_config_with_plans(config, sample_rate, plans),
        }
    }

    /// Apply a mutation to the active graph — the seam for accessor-style
    /// mutations (`timestretch_mut().stretcher.set_speed(…)`,
    /// `eq_mut().eq = …`, `routing_mut().trimmer.set_config(…)`, …).
    pub fn with_graph<R>(&mut self, f: impl Fn(&mut DspGraph) -> R) -> R {
        f(&mut self.inner)
    }

    /// The Graph2 production topology (nodes + edges + compiled order) —
    /// the plan source this engine runs on. Introspection and tests.
    pub fn topology() -> topology::ProdTopology {
        lowering::topology()
    }

    /// The lowered plan steps as `(arena slot, all-channels scope)` pairs.
    /// Introspection and tests.
    pub fn lowered_plan_steps() -> Vec<(usize, bool)> {
        lowering::lowered_plan_steps()
    }

    /// The active engine's arena graph — the single node implementation.
    /// Read-only view for hosts and tests; mutations go through
    /// [`Self::with_graph`] or the explicit mutators.
    pub fn inner(&self) -> &DspGraph {
        &self.inner
    }
}

impl std::ops::Deref for Graph2Engine {
    type Target = DspGraph;

    fn deref(&self) -> &DspGraph {
        &self.inner
    }
}

impl fmt::Debug for Graph2Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graph2Engine")
            .field("sample_rate", &self.inner.sample_rate())
            .finish()
    }
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

impl Graph2Engine {
    /// Live reconfiguration: the fresh generation carries Graph2-lowered
    /// plans.
    pub fn reconfigure(&mut self, config: &config::EngineConfig) {
        let plans = lowering::lowered_plans();
        self.inner.reconfigure_with_plans(config, plans);
    }

    /// Apply a config to the active generation directly (control path).
    pub fn apply_config(&mut self, config: &config::EngineConfig) {
        self.inner.apply_config(config);
    }

    /// Update the sample rate across all nodes.
    pub fn update_sample_rate(&mut self, sample_rate: f32) {
        self.inner.update_sample_rate(sample_rate);
    }

    /// Reset internal state across all nodes.
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    /// Reset filter state only.
    pub fn reset_filters_only(&mut self) {
        self.inner.reset_filters_only();
    }

    /// Drain the queued control commands at the block boundary (also swaps
    /// in a published generation).
    pub fn drain_queued_control(&mut self) {
        self.inner.drain_queued_control();
    }

    /// Set the multichannel layout.
    pub fn set_multichannel_layout(&mut self, layout: &crate::decode::ChannelLayout) {
        self.inner.set_multichannel_layout(layout);
    }

    /// Set the precision mode.
    pub fn set_precision_mode(&mut self, mode: crate::dsp::pipeline::PrecisionMode) {
        self.inner.set_precision_mode(mode);
    }

    /// Toggle bit-perfect transport.
    pub fn set_bit_perfect(&self, enabled: bool) {
        self.inner.set_bit_perfect(enabled);
    }

    /// Toggle DoP bypass.
    pub fn set_dop_bypass(&self, enabled: bool) {
        self.inner.set_dop_bypass(enabled);
    }

    /// Set the playback speed target.
    pub fn set_speed(&self, speed: f32) {
        self.inner.set_speed(speed);
    }

    /// Set the volume-ramp duration.
    pub fn set_volume_fade_ms(&mut self, ms: f32) {
        self.inner.set_volume_fade_ms(ms);
    }

    /// Load a rendered correction IR set (active node + sticky mirror).
    /// The `&self` queued variant lives on the control handle.
    pub fn load_correction_ir(
        &mut self,
        set: std::sync::Arc<crate::dsp::correction::CorrectionIrSet>,
    ) {
        self.inner.load_correction_ir(set);
    }

    /// The cloneable cross-thread control surface.
    pub fn control_handle(&self) -> Graph2ControlHandle {
        Graph2ControlHandle::new(self.inner.control_handle())
    }
}
