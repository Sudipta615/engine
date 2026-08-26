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
//! `DspGraph` is **not the active hot path**. The engine routes through
//! [`crate::dsp::pipeline::DspPipeline`] for playback. `DspGraph` is used for:
//!
//! - Capability introspection (each node type implements [`DspNode::capability`])
//! - Unit testing node-level correctness (see `tests.rs`)
//! - Prototyping future DSP architecture features (reorderable chain,
//!   conditional nodes, SIMD dispatch)
//!
//! Since the Phase-1 refactor, the graph executes **compiled execution plans**:
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
//! - `construction.rs` — [`DspGraph::from_config`], [`DspGraph::apply_config`],
//!   [`DspGraph::apply_performance_mode`] (builds the arena + plans)
//! - `plan.rs` — the compiled [`PlanSet`] / [`ExecutionPlan`] / [`PlanStep`]
//!   representation and the canonical stage order
//! - `access.rs` — typed node accessors over the arena (replaces the former
//!   named fields: `graph.volume()` instead of `graph.volume`)
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

use plan::PlanSet;

use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::decode::ChannelLayout;
use crate::dsp::pipeline::{DspNodeInfo, PrecisionMode, DSP_STAGE_CAPABILITIES};
use config::{EngineConfig, LoudnessMode as ConfigLoudnessMode, PerformanceMode};

pub use context::GraphScratch;
pub use node::DspNode;
pub use nodes::*;

// ── Node arena ───────────────────────────────────────────────────────────────

/// Stable index into the graph's node arena. Plans reference stages by index
/// instead of by name, so reordering or replacing a node (Phase 2) only needs
/// to rebuild the plan, never the executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NodeIdx(usize);

/// Fixed arena slot order — MUST match the construction order in
/// [`DspGraph::from_config`].
mod node_id {
    pub const OUT_PREAMP: usize = 0;
    pub const OUT_LOUDNESS: usize = 1;
    pub const IN_PREAMP: usize = 2;
    pub const IN_LOUDNESS: usize = 3;
    pub const EQ: usize = 4;
    pub const DYNAMICS: usize = 5;
    pub const CONVOLUTION: usize = 6;
    pub const BALANCE: usize = 7;
    pub const CROSSFEED: usize = 8;
    pub const STEREO: usize = 9;
    pub const TIMESTRETCH: usize = 10;
    pub const VOLUME: usize = 11;
    pub const SEEK_FADE: usize = 12;
    pub const ROUTING: usize = 13;
    pub const RESAMPLER: usize = 14;
    pub const LIMITER: usize = 15;
    pub const DITHER: usize = 16;
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
enum GraphNode {
    OutPreamp(GainNode),
    OutLoudness(LoudnessNode),
    InPreamp(GainNode),
    InLoudness(LoudnessNode),
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
    // ── Node arena ──
    /// Uniform node storage: every graph node lives in one contiguous arena so
    /// the compiled execution plans can reference stages by stable index
    /// ([`node_id`]). Arena order is fixed by construction.
    nodes: Vec<GraphNode>,

    /// Compiled execution plans — one per execution mode and channel class
    /// (see [`plan`]). Built on the control path; the audio path only reads
    /// them. The canonical stage order lives here, not in the executor.
    plans: PlanSet,

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
