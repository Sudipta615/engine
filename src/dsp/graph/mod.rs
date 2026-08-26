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
//!   [`DspGraph::apply_performance_mode`]
//! - `lifecycle.rs` — sample-rate updates, resets, mode toggles, and the
//!   small getters/setters
//! - `process.rs` — the block signal-processing plans (stereo f32 / f64,
//!   multichannel, pre-mix / post-mix / front-filter helpers)
//! - `limiter.rs` — the output-domain final safety limiter
//! - `report.rs` — `graph_nodes` and `total_latency_ms` introspection

pub mod context;
pub mod node;
pub mod nodes;
#[cfg(test)]
pub mod tests;

mod construction;
mod lifecycle;
mod limiter;
mod process;
mod report;

use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::decode::ChannelLayout;
use crate::dsp::pipeline::{DspNodeInfo, PrecisionMode, DSP_STAGE_CAPABILITIES};
use config::{EngineConfig, LoudnessMode as ConfigLoudnessMode, PerformanceMode};

pub use context::GraphScratch;
pub use node::DspNode;
pub use nodes::*;

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
    // ── Pre-mix Chain ──
    pub out_preamp: GainNode,
    pub out_loudness: LoudnessNode,
    pub in_preamp: GainNode,
    pub in_loudness: LoudnessNode,

    // ── Post-mix Chain ──
    pub eq: EqNode,
    pub dynamics: DynamicsNode,
    pub convolution: ConvolutionNode,
    pub balance: BalanceNode,
    pub crossfeed: CrossfeedNode,
    pub stereo: StereoNode,
    pub timestretch: TimeStretchNode,
    pub volume: GainNode,
    pub seek_fade: SeekFadeNode,

    // ── Routing & Multichannel ──
    pub routing: RoutingNode,
    pub multichannel_layout: ChannelLayout,

    // ── Output Domain & Safety ──
    pub resampler: ResamplerNode,
    pub limiter: LimiterNode,
    pub dither: DitherNode,

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
