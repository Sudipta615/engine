//! Phase 46 (v3.51.0) — the production engine on Graph 2.0.
//!
//! This module is the seam between Graph 2.0's typed-port topology and the
//! production `dsp::graph` arena: the **one node implementation** the engine
//! runs (mix bus, aux, correction, EQ, … — the full 17-slot arena) stays
//! exactly where it is; what changes is the **plan source**. Instead of the
//! hand-authored `PlanSet::compile()`, every generation this engine builds
//! carries plans *lowered* from a real Graph2 topology:
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
//! Bit-exactness is by construction: the lowered plan is pinned equal to
//! `PlanSet::compile()`'s order by the `lowered_plans_match_handauthored`
//! unit test, and the arena, node configuration, user-state replay, control
//! queues, and swap machinery are the *same objects* `DspGraph` uses.
//!
//! ## Phase 47 (v3.52.0) — engine migration + shadow mode
//!
//! `AudioEngine.graph` is a [`Graph2Engine`] (a drop-in replacement: the
//! process entry points, control mutators, and getters mirror `DspGraph`
//! one-to-one). With the `graph2_shadow_verify` config flag on, the engine
//! keeps a **shadow twin** — a legacy `DspGraph` built with the
//! hand-authored plans — driven in lock-step (every control mutator fans
//! out; accessor mutations go through [`Graph2Engine::with_both`]) and
//! bit-compared on every processed block. A mismatch is recorded (first
//! differing sample + block counter) and the active engine's output
//! stands; the shadow is a diagnostic, never a fallback. Off by default
//! for performance; CI fidelity runs enable it.
//!
//! ## Module map (the house split)
//!
//! - `topology.rs` — the production chain as a Graph2 graph (typed ports,
//!   edges, validation, topological compile)
//! - `lowering.rs` — the compiled topology → the arena `PlanSet`
//! - `control.rs` — [`Graph2ControlHandle`]: the cloneable cross-thread
//!   surface (mirrors `GraphControlHandle`; fans commands out to shadow
//!   twins)
//! - `process.rs` — the 9 block entry points + the shadow bit-compare
//! - `controls.rs` — the `DspGraph`-mirroring control mutators (each
//!   forwards to the active graph **and** the shadow twin)
//! - `mod.rs` (this file) — the [`Graph2Engine`] struct, construction,
//!   lifecycle fan-out, and shadow state

use crate::dsp::graph::DspGraph;
use std::fmt;

mod control;
mod controls;
mod lowering;
mod process;
mod topology;

pub use control::Graph2ControlHandle;

/// The production engine on Graph 2.0 — a drop-in replacement for
/// [`DspGraph`].
///
/// The shell owns one `DspGraph` (the arena + control bus + swap machinery
/// — the single node implementation) whose generations carry
/// **Graph2-lowered** plans instead of the hand-authored
/// `PlanSet::compile()`. Everything else (process entry points, control
/// surface, accessors, reports) is the production graph's — forwarded
/// verbatim — so audio behavior is bit-identical by construction.
///
/// Read-only accessors (`volume()`, `eq()`, `graph_nodes()`, …) resolve
/// through `Deref` to the embedded graph. Mutating methods exist
/// explicitly on this type so they can fan out to the shadow twin (a
/// `Deref`ed mutation would silently bypass the shadow).
pub struct Graph2Engine {
    /// The production arena + control machinery — the single node
    /// implementation. Its generations carry Graph2-lowered plans.
    pub(crate) inner: DspGraph,
    /// Phase-47 shadow twin: a legacy `DspGraph` (hand-authored plans)
    /// kept in lock-step for bit-comparison. `None` unless
    /// `graph2_shadow_verify` is enabled.
    shadow: Option<Box<DspGraph>>,
    /// Blocks processed since the shadow attached.
    shadow_blocks: u64,
    /// Bit-compare mismatches observed (diagnostic counter; the active
    /// engine's output always stands).
    shadow_mismatches: u64,
}

impl Graph2Engine {
    /// Build the engine from config: lower the Graph2 production topology
    /// to plans and construct the initial generation with them (the exact
    /// arena/config/user-state path as `DspGraph::from_config`; only the
    /// plan source differs).
    pub fn from_config(config: &config::EngineConfig, sample_rate: f32) -> Self {
        let plans = lowering::lowered_plans();
        Self {
            inner: DspGraph::from_config_with_plans(config, sample_rate, plans),
            shadow: None,
            shadow_blocks: 0,
            shadow_mismatches: 0,
        }
    }

    /// Phase 47: attach the shadow twin — a legacy `DspGraph` built from
    /// the same config with the hand-authored plans. Every subsequent
    /// control mutation and processed block fans out to it, and outputs
    /// are bit-compared.
    pub fn enable_shadow(&mut self, config: &config::EngineConfig) {
        let sr = self.inner.sample_rate();
        self.shadow = Some(Box::new(DspGraph::from_config(config, sr)));
        self.shadow_blocks = 0;
        self.shadow_mismatches = 0;
    }

    /// Phase 47: detach the shadow twin (shadow mode off).
    pub fn disable_shadow(&mut self) {
        self.shadow = None;
    }

    /// Whether the shadow twin is attached.
    pub fn shadow_enabled(&self) -> bool {
        self.shadow.is_some()
    }

    /// Shadow diagnostics: `(blocks compared, mismatches)`.
    pub fn shadow_stats(&self) -> (u64, u64) {
        (self.shadow_blocks, self.shadow_mismatches)
    }

    /// Apply a mutation to the active graph **and** the shadow twin (when
    /// attached) — the fan-out seam for accessor-style mutations
    /// (`timestretch_mut().stretcher.set_speed(…)`, `eq_mut().eq = …`,
    /// `routing_mut().trimmer.set_config(…)`, …) that cannot be forwarded
    /// as plain method calls. The closure runs on the active graph first;
    /// its return value is the active graph's.
    pub fn with_both<R>(&mut self, f: impl Fn(&mut DspGraph) -> R) -> R {
        let r = f(&mut self.inner);
        if let Some(shadow) = &mut self.shadow {
            f(shadow);
        }
        r
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
    /// Read-only view for hosts and tests; mutations that must reach the
    /// shadow twin go through the explicit mutators or [`Self::with_both`].
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
            .field("shadow", &self.shadow.is_some())
            .finish()
    }
}

// ── Lifecycle (fan-out to the shadow) ──────────────────────────────────────

impl Graph2Engine {
    /// Live reconfiguration: the fresh generation carries Graph2-lowered
    /// plans. Mirrors `DspGraph::reconfigure` (the shadow twin
    /// reconfigures with its own hand-authored plans — the A/B covers the
    /// full rebuild path).
    pub fn reconfigure(&mut self, config: &config::EngineConfig) {
        let plans = lowering::lowered_plans();
        self.inner.reconfigure_with_plans(config, plans);
        if let Some(shadow) = &mut self.shadow {
            shadow.reconfigure(config);
        }
    }

    /// Apply a config to the active generation directly (control path).
    /// Mirrors `DspGraph::apply_config`.
    pub fn apply_config(&mut self, config: &config::EngineConfig) {
        self.inner.apply_config(config);
        if let Some(shadow) = &mut self.shadow {
            shadow.apply_config(config);
        }
    }

    /// Update the sample rate across all nodes. Mirrors
    /// `DspGraph::update_sample_rate`.
    pub fn update_sample_rate(&mut self, sample_rate: f32) {
        self.inner.update_sample_rate(sample_rate);
        if let Some(shadow) = &mut self.shadow {
            shadow.update_sample_rate(sample_rate);
        }
    }

    /// Reset internal state across all nodes. Mirrors `DspGraph::reset`.
    pub fn reset(&mut self) {
        self.inner.reset();
        if let Some(shadow) = &mut self.shadow {
            shadow.reset();
        }
    }

    /// Reset filter state only. Mirrors `DspGraph::reset_filters_only`.
    pub fn reset_filters_only(&mut self) {
        self.inner.reset_filters_only();
        if let Some(shadow) = &mut self.shadow {
            shadow.reset_filters_only();
        }
    }

    /// Drain the queued control commands at the block boundary (also swaps
    /// in a published generation). Mirrors `DspGraph::drain_queued_control`.
    pub fn drain_queued_control(&mut self) {
        self.inner.drain_queued_control();
        if let Some(shadow) = &mut self.shadow {
            shadow.drain_queued_control();
        }
    }

    /// Set the multichannel layout. Mirrors `DspGraph::set_multichannel_layout`.
    pub fn set_multichannel_layout(&mut self, layout: &crate::decode::ChannelLayout) {
        self.inner.set_multichannel_layout(layout);
        if let Some(shadow) = &mut self.shadow {
            shadow.set_multichannel_layout(layout);
        }
    }

    /// Set the precision mode. Mirrors `DspGraph::set_precision_mode`.
    pub fn set_precision_mode(&mut self, mode: crate::dsp::pipeline::PrecisionMode) {
        self.inner.set_precision_mode(mode);
        if let Some(shadow) = &mut self.shadow {
            shadow.set_precision_mode(mode);
        }
    }

    /// Toggle bit-perfect transport. Mirrors `DspGraph::set_bit_perfect`.
    pub fn set_bit_perfect(&self, enabled: bool) {
        self.inner.set_bit_perfect(enabled);
        if let Some(shadow) = &self.shadow {
            shadow.set_bit_perfect(enabled);
        }
    }

    /// Toggle DoP bypass. Mirrors `DspGraph::set_dop_bypass`.
    pub fn set_dop_bypass(&self, enabled: bool) {
        self.inner.set_dop_bypass(enabled);
        if let Some(shadow) = &self.shadow {
            shadow.set_dop_bypass(enabled);
        }
    }

    /// Set the playback speed target. Mirrors `DspGraph::set_speed`.
    pub fn set_speed(&self, speed: f32) {
        self.inner.set_speed(speed);
        if let Some(shadow) = &self.shadow {
            shadow.set_speed(speed);
        }
    }

    /// Set the volume-ramp duration. Mirrors `DspGraph::set_volume_fade_ms`.
    pub fn set_volume_fade_ms(&mut self, ms: f32) {
        self.inner.set_volume_fade_ms(ms);
        if let Some(shadow) = &mut self.shadow {
            shadow.set_volume_fade_ms(ms);
        }
    }

    /// Load a rendered correction IR set (active node + sticky mirror).
    /// Mirrors `DspGraph::load_correction_ir` (single-threaded control
    /// path). The `&self` queued variant lives on the control handle.
    pub fn load_correction_ir(
        &mut self,
        set: std::sync::Arc<crate::dsp::correction::CorrectionIrSet>,
    ) {
        self.inner.load_correction_ir(set.clone());
        if let Some(shadow) = &mut self.shadow {
            shadow.load_correction_ir(set);
        }
    }

    /// The cloneable cross-thread control surface. When the shadow twin
    /// is attached, the returned handle fans commands out to it too.
    pub fn control_handle(&self) -> Graph2ControlHandle {
        let handle = Graph2ControlHandle::new(self.inner.control_handle());
        if let Some(shadow) = &self.shadow {
            handle.attach_shadow(shadow.control_handle());
        }
        handle
    }
}
