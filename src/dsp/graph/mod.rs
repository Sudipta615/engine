//! # Experimental Node-Based DSP Graph
//!
//! This module provides an alternative, node-based DSP architecture through
//! the [`DspNode`] trait. Unlike the production [`crate::dsp::pipeline::DspPipeline`]
//! (which runs a fixed linear sequence of stages per stereo frame), `DspGraph`
//! composes nodes that each describe their capabilities, process planar audio
//! blocks in f32 or f64, and can be rearranged or selectively activated.
//!
//! ## Current status
//!
//! Since Phase 3 the engine routes through `DspGraph` as its production hot
//! path (the graph executes the full chain: mix bus, EQ, dynamics,
//! convolution, balance, crossfeed, stereo, timestretch, volume, seek fade,
//! routing, resampler, limiter, dither). `DspPipeline` remains as the
//! reference implementation and the bit-exact oracle for the equivalence
//! suite.
//!
//! The graph executes **compiled execution plans**:
//! all nodes live in a fixed [`GraphNode`] arena (indexed by [`node_id`]) and
//! [`plan::PlanSet::compile`] orders them into per-mode step lists. The hot
//! path iterates a plan and dispatches through the enum — stage order is data,
//! not code, which is the prerequisite for live reconfiguration (Phase 2).
//!
//! The static [`DSP_STAGE_CAPABILITIES`] table in the pipeline module is the
//! single source of truth for stage metadata; node capability implementations
//! here mirror those entries.
//!
//! ## Layout
//!
//! The [`DspGraph`] impl is split by concern, mirroring
//! [`crate::dsp::pipeline`] (struct + wiring in `mod.rs`, behavior in
//! concern-scoped files):
//!
//! - `construction.rs` — [`DspGraph::from_config`], [`DspGraph::reconfigure`],
//!   and the generation builder (builds the arena + plans)
//! - `plan.rs` — the compiled [`PlanSet`] / [`ExecutionPlan`] / [`PlanStep`]
//!   representation and the canonical stage order
//! - `swap.rs` — stable [`NodeId`] identity and the swappable
//!   [`swap::GraphGeneration`] container
//! - `access.rs` — typed node accessors over the arena (replaces the former
//!   named fields: `graph.volume()` instead of `graph.volume`)
//! - `controls.rs` — the queued control surface: per-node SPSC command
//!   queues, the publish/swap/retire handshake, and the block-boundary drain
//! - `lifecycle.rs` — sample-rate updates, resets, mode toggles, and the
//!   small getters/setters
//! - `process.rs` — block entry points (stereo f32/f64, multichannel) that
//!   split blocks, promote precision, and hand planes to the plan runner
//! - `limiter.rs` — the output-domain final safety limiter
//! - `report.rs` — `graph_nodes` and `total_latency_ms` introspection

pub mod context;
pub mod node;
pub mod nodes;
#[cfg(test)]
pub mod tests;

mod access;
mod construction;
mod controls;
mod lifecycle;
mod limiter;
mod plan;
mod process;
mod report;
mod swap;

use plan::PlanSet;
use std::sync::Arc;

use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::decode::ChannelLayout;
use crate::dsp::pipeline::{DspNodeInfo, PrecisionMode, DSP_STAGE_CAPABILITIES};
use config::{EngineConfig, LoudnessMode as ConfigLoudnessMode, PerformanceMode};

pub use context::GraphScratch;
pub use controls::GraphControlHandle;
pub use node::DspNode;
pub use nodes::*;

pub use swap::GraphGeneration;

pub(super) use controls::{ControlBus, NodeCmd};
pub(super) use swap::{NodeId, UserState};

// ── Node arena ───────────────────────────────────────────────────────────────

/// Stable index into the graph's node arena. Plans reference stages by index
/// instead of by name, so reordering or replacing a node only needs to
/// rebuild the plan, never the executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NodeIdx(usize);

/// Canonical `NodeId` values (the arena slot table): the per-node control
/// queues are addressed by [`swap::NodeId`], and in the default layout these
/// values coincide with the arena slot order, which MUST match the
/// construction order in [`DspGraph::from_config`]. The shell slot is
/// [`swap::NodeId::SHELL`].
mod node_id {
    /// The mix bus: N per-input pre-mix chains (preamp + loudness + gain +
    /// balance + mute) summed into the master chain. Replaces the former
    /// `OUT_PREAMP` / `OUT_LOUDNESS` / `IN_PREAMP` / `IN_LOUDNESS` slots
    /// (Phase 3 S1).
    pub const MIX: usize = 0;
    pub const EQ: usize = 1;
    pub const DYNAMICS: usize = 2;
    pub const CONVOLUTION: usize = 3;
    pub const BALANCE: usize = 4;
    pub const CROSSFEED: usize = 5;
    pub const STEREO: usize = 6;
    pub const TIMESTRETCH: usize = 7;
    pub const VOLUME: usize = 8;
    pub const SEEK_FADE: usize = 9;
    pub const ROUTING: usize = 10;
    pub const RESAMPLER: usize = 11;
    pub const LIMITER: usize = 12;
    pub const DITHER: usize = 13;
    /// Number of canonical node slots (also the first non-node `NodeId`).
    pub const NODE_COUNT: usize = 14;
}

/// Uniform node storage for the arena. The enum enables monomorphized (match)
/// dispatch on the hot path and keeps every node inline in one contiguous
/// allocation — no `Box<dyn DspNode>` indirection. The arena order is fixed
/// by construction and matches the [`node_id`] slot table.
///
/// The enum is deliberately large (the largest node, e.g. the limiter or
/// timestretcher, is ~13 KB, and the arena stores one slot per node kind);
/// boxing would reintroduce per-node indirection and allocation, so the lint
/// is allowed — same as `PlaybackStream` in the engine.
#[allow(clippy::large_enum_variant)]
pub(super) enum GraphNode {
    Mix(MixBusNode),
    Eq(EqNode),
    Dynamics(DynamicsNode),
    Convolution(ConvolutionNode),
    Balance(BalanceNode),
    Crossfeed(CrossfeedNode),
    Stereo(StereoNode),
    TimeStretch(TimeStretchNode),
    Volume(GainNode),
    SeekFade(SeekFadeNode),
    Routing(RoutingNode),
    Resampler(ResamplerNode),
    Limiter(LimiterNode),
    Dither(DitherNode),
}

const VOLUME_RAMP_DURATION_MS: f32 = 10.0;
const PREAMP_RAMP_DURATION_MS: f32 = VOLUME_RAMP_DURATION_MS;

/// The central DSP Graph executing the statically compiled signal processing chain.
///
/// Implements the target conceptual model:
/// ```text
/// DspGraph
///   ├── Gain (Preamp / SeekFade / Volume)
///   ├── Loudness (LoudnessNormalizer)
///   ├── EQ (ParametricEq / Mid-Side)
///   ├── Dynamics (MultibandCompressor)
///   ├── FIR/Convolution (ConvolutionEngine)
///   ├── Routing (ChannelTrimmer / Bass Management / Matrix)
///   ├── Crossfeed (Crossfeed)
///   ├── Stereo (StereoEnhancer)
///   ├── Time/Pitch (TimeStretcher)
///   ├── Volume (GainProcessor)
///   ├── Resampler (AudioResampler adapter)
///   ├── Limiter (LookaheadLimiter)
///   └── Dither/Conversion (Dither adapter)
/// ```
///
/// Features:
/// - Explicit node descriptor metadata via [`DspNode::capability`]
/// - Automated latency & tail tracking
/// - Zero dynamic allocations on the real-time audio thread
/// - Transparent bit-perfect & DoP bypass execution plans
///
/// The struct only declares the graph's fields and wiring; its behavior lives
/// in the concern-scoped impl files listed in the module docs.
pub struct DspGraph {
    // ── Active generation (audio thread owns this) ──
    /// The currently-executed graph configuration: node arena + compiled
    /// plans + stable node identities. Swapped atomically at block boundaries
    /// via the publish/swap/retire handshake (see [`controls`]).
    active: Box<swap::GraphGeneration>,

    /// The cross-thread control plane: per-node SPSC queues, the swap
    /// atomics, and sticky user state. The only part of the graph shared
    /// between the control and audio threads.
    bus: Arc<controls::ControlBus>,

    // ── Routing & Multichannel ──
    pub multichannel_layout: ChannelLayout,

    // ── Graph State & Control ──
    sample_rate: f32,
    speed: f32,
    volume_fade_ms: f32,
    precision_mode: PrecisionMode,
    performance_mode: PerformanceMode,
    bit_perfect: bool,
    dop_bypass: bool,

    // ── Pre-allocated Scratch Arena ──
    scratch: GraphScratch,
}
