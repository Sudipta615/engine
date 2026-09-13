//! Graph 2.0 executors (v3.50 Phase 45 — split from `exec.rs` per the
//! `dsp/pipeline/` house pattern).
//!
//! [`OfflineExecutor`] renders a **compiled** [`Graph2`] topology block by
//! block on the control/offline path: audio flows along the explicit edges
//! in topological order, with no authored chain — a dry/wet bus is just
//! `Split → {Gain, Delay} → Mix`, a broadcast is one output port feeding
//! many edges.
//!
//! Each edge owns one single-channel plane (`block` frames, zeroed at block
//! start — an unconnected input reads silence, an unconnected output is
//! dropped). State lives per node (delay lines, oscillator phase).
//!
//! ## Module map (the Phase-45 house split)
//!
//! - `mod.rs` (this file) — module wiring, the [`OfflineExecutor`] struct
//!   definition, control-surface methods (scene attach/swap, automation,
//!   external tracks) and `process_block` dispatch.
//! - `offline.rs` — the offline executor's per-node `run_*` operations,
//!   delegating the shared math to [`ops`].
//! - `ops.rs` — the **shared node-processing kernels** (Phase 45 S3): one
//!   set of per-node processing math used by *both* executors — the
//!   offline executor here and the realtime executor in
//!   `crate::dsp::graph2::rt` — so offline/realtime divergence is
//!   structurally impossible.
//! - `buffers.rs` — the per-node pipeline-state types (`AcousticState`,
//!   `ConvState`, `FftConvState`, `HrtfState`, `ResamplerState`,
//!   `DelayState`, `SourceState`) and the overlap-add / windowed-sinc
//!   math, plus scratch/buffer plumbing.
//! - `tests.rs` — the offline executor's test battery (moved from
//!   `exec.rs` verbatim).
//!
//! ## Realtime counterpart
//!
//! The realtime lowering substrate lives in `crate::dsp::graph2::rt`: it
//! builds an immutable, preallocated `RtPlan` on the control thread and
//! publishes it behind an atomic pointer (the Phase-2 generation-swap
//! discipline), so the audio thread runs the exact same kernels with zero
//! allocation. This module remains the offline/oracle path.

use std::collections::{BTreeMap, HashMap};

pub mod buffers;
pub mod offline;
pub mod ops;
#[cfg(test)]
#[cfg(test)]
pub mod tests;

pub use buffers::{
    AcousticState, ConvState, DelayState, FftConvState, HrtfState, ResamplerState, SourceState,
    ACOUSTIC_HISTORY,
};
pub use ops::CONVOLUTION_FFT_THRESHOLD;

use super::edge::{EdgeDef, EdgeId};
// Re-exported for the test battery's `use super::*` (moved from the old
// exec.rs glob) — `PortId`/`TestSignal` are used by tests.rs only.
use super::node::{NodeDef, NodeId, NodeKind, NodeParams};
#[cfg(test)]
use super::node::{PortId, TestSignal};
use super::sort::ExecutionOrder;
use super::validate::Graph2Error;
use super::Graph2;
use crate::dsp::timeline::automation::CurveBeats;
use crate::dsp::timeline::tempo::TempoMap;
use crate::spatial::acoustic::bake::BakedScene;
use crate::spatial::hrtf::HrtfDataset;
use crate::spatial::math::Vec3;

/// Renders a compiled topology over time. Snapshot semantics: construct from
/// a `(&Graph2, &ExecutionOrder)` pair; mutate the graph only through a
/// fresh compile + executor.
pub struct OfflineExecutor {
    pub(crate) block: usize,
    pub(crate) sample_rate: f32,
    pub(crate) nodes: BTreeMap<NodeId, NodeDef>,
    pub(crate) edges: BTreeMap<EdgeId, EdgeDef>,
    pub(crate) order: ExecutionOrder,
    /// One block-sized plane per edge, zeroed at each block start.
    pub(crate) edge_planes: HashMap<EdgeId, Vec<f32>>,
    /// Delay-line state per Delay node.
    pub(crate) delays: HashMap<NodeId, DelayState>,
    /// Oscillator phase / fired flag per Source node.
    pub(crate) sources: HashMap<NodeId, SourceState>,
    /// Accumulated captured audio per Sink node.
    pub(crate) captures: HashMap<NodeId, Vec<f32>>,
    /// Pending per-node gain steps `(gain, local block index)` applied by
    /// `run_gain` at the exact frame — sample-accurate parameter changes.
    pub(crate) gain_steps: HashMap<NodeId, (f32, usize)>,
    /// The acoustic world response cache consumed by `Acoustic` nodes
    /// (baked scene; see `set_baked_scene`).
    pub(crate) baked: Option<BakedScene>,
    /// Per-node tapped delay lines for `Acoustic` node room responses.
    pub(crate) acoustics: HashMap<NodeId, AcousticState>,
    /// Bumped whenever the acoustic world changes (`swap_baked_scene`,
    /// `set_listener_position`, `set_scene`/`remove_scene`) so `Acoustic`
    /// nodes recompile their per-path filter kernels even when the new
    /// world bakes the *same* source cell (the cell key alone can't tell
    /// two worlds apart). The raw-history ring is untouched, so the room
    /// keeps ringing.
    pub(crate) acoustic_epoch: u64,
    /// Monotonic total samples rendered (the executor's master clock
    /// position). Drives tempo-mapped automation: each `process_block`
    /// evaluates the registered [`CurveBeats`] curves over this span.
    pub(crate) master_sample: u64,
    /// The tempo map musical automation evaluates against (beat → sample).
    /// Without one, automation is inert (curves are meaningless without a
    /// tempo reference).
    pub(crate) tempo_map: Option<TempoMap>,
    /// Per-Gain-node tempo-mapped automation: `NodeId.0 → curve in beats`.
    /// When present (with a `tempo_map`), `run_gain` sweeps the gain
    /// smoothly across each block from the curve's value at block start to
    /// its value at block end.
    pub(crate) gain_automation: BTreeMap<u32, CurveBeats>,
    /// A live listener position: when set, every `Acoustic` node looks up
    /// the baked response at **this** position instead of its own
    /// `NodeParams::Acoustic::position` (the aelog listener-trajectory
    /// drive). When unset, each node uses its baked position unchanged.
    pub(crate) listener_position: Option<Vec3>,
    /// **Named** baked scenes keyed by id (`NodeParams::Acoustic::scene`):
    /// per-listener bakes one graph can mix — an `Acoustic` node whose
    /// params name a scene renders from here, otherwise the active scene.
    pub(crate) scenes: HashMap<String, BakedScene>,
    /// External audio-input track: when set, every *unaddressed* `Buffer`
    /// node (no clip address) plays this instead of its embedded samples
    /// (the aelog single-track replay path). Channel-major planes
    /// (`track[0]` = channel 0, …).
    pub(crate) external_input: Option<Vec<Vec<f32>>>,
    /// Per-clip external audio-input tracks, keyed by clip address: only
    /// `Buffer` nodes whose `NodeParams::Buffer::clip` matches play the
    /// matching track (the aelog multi-input replay path). Channel-major
    /// planes per track.
    pub(crate) external_clips: HashMap<String, Vec<Vec<f32>>>,
    /// Per-node playback cursor for `Buffer` nodes (sample index).
    pub(crate) buffer_cursors: HashMap<NodeId, usize>,
    /// Pipeline-delay state per short-kernel `Convolution` node: the
    /// convolved stream held back by one kernel length before emission.
    /// Long-kernel nodes (≥ [`CONVOLUTION_FFT_THRESHOLD`]) live in
    /// `fft_convolutions` instead.
    pub(crate) convolutions: HashMap<NodeId, ConvState>,
    /// Partitioned-FFT state per long-kernel `Convolution` node, backed by
    /// the realtime `dsp::convolution` engine (renders long IRs fast).
    pub(crate) fft_convolutions: HashMap<NodeId, FftConvState>,
    /// Per-ear pipeline-delay state per `HRTF` node (both ears delayed by
    /// the longer IR so the pair stays mutually aligned).
    pub(crate) hrtfs: HashMap<NodeId, HrtfState>,
    /// Streaming interpolator + reported-delay state per `Resampler` node.
    pub(crate) resamplers: HashMap<NodeId, ResamplerState>,
    /// The measured head-related impulse responses a `HrtfSource::Dataset`
    /// `/HRTF` node reads its per-ear IRs from (mirrors how a `BakedScene` is
    /// attached; the node references it by azimuth/elevation/taps).
    pub(crate) hrtf_dataset: Option<HrtfDataset>,
}

impl OfflineExecutor {
    /// Build an executor for a compiled graph. `block_frames` is the
    /// per-call processing size; `sample_rate` drives sine sources.
    pub fn new(
        graph: &Graph2,
        order: &ExecutionOrder,
        block_frames: usize,
        sample_rate: f32,
    ) -> Result<Self, Graph2Error> {
        // The order must actually cover the graph.
        if order.len() != graph.node_count() {
            return Err(Graph2Error::Cycle(Vec::new()));
        }
        let mut edge_planes = HashMap::new();
        for e in graph.edges.values() {
            edge_planes.insert(e.id, vec![0.0; block_frames]);
        }
        let mut delays = HashMap::new();
        for n in graph.nodes.values() {
            if let NodeParams::Delay { samples } = n.params {
                if samples > 0 {
                    delays.insert(
                        n.id,
                        DelayState {
                            buf: vec![0.0; samples as usize],
                            pos: 0,
                        },
                    );
                }
            }
        }
        Ok(Self {
            block: block_frames,
            sample_rate,
            nodes: graph.nodes.clone(),
            edges: graph.edges.clone(),
            order: order.clone(),
            edge_planes,
            delays,
            sources: HashMap::new(),
            captures: HashMap::new(),
            gain_steps: HashMap::new(),
            baked: None,
            acoustics: HashMap::new(),
            acoustic_epoch: 0,
            master_sample: 0,
            tempo_map: None,
            gain_automation: BTreeMap::new(),
            listener_position: None,
            scenes: HashMap::new(),
            external_input: None,
            external_clips: HashMap::new(),
            buffer_cursors: HashMap::new(),
            convolutions: HashMap::new(),
            fft_convolutions: HashMap::new(),
            hrtfs: HashMap::new(),
            resamplers: HashMap::new(),
            hrtf_dataset: None,
        })
    }

    /// Advance the graph one block. All edge planes are re-zeroed first, so
    /// stale samples never leak between blocks. Advances the master sample
    /// by one block, so tempo-mapped automation tracks the playhead.
    pub fn process_block(&mut self) -> Result<(), Graph2Error> {
        for plane in self.edge_planes.values_mut() {
            plane.fill(0.0);
        }
        let steps = self.order.steps.clone();
        for node_id in steps {
            let kind = self
                .nodes
                .get(&node_id)
                .map(|n| n.kind)
                .ok_or(Graph2Error::UnknownNode(node_id))?;
            match kind {
                NodeKind::Source => self.run_source(node_id),
                NodeKind::Sink => self.run_sink(node_id),
                NodeKind::Gain => self.run_gain(node_id),
                NodeKind::Delay => self.run_delay(node_id),
                NodeKind::Mix => self.run_mix(node_id),
                NodeKind::Split => self.run_split(node_id),
                NodeKind::Acoustic => self.run_acoustic(node_id),
                NodeKind::Buffer => self.run_buffer(node_id),
                NodeKind::Convolution => self.run_convolution(node_id),
                NodeKind::HRTF => self.run_hrtf(node_id),
                NodeKind::Resampler => self.run_resampler(node_id),
                // Production stages execute through the shared arena in the
                // `prod` shell (one node implementation); the generic offline
                // executor never lowers them itself.
                NodeKind::Prod(_) => offline::pass_through_prod(self, node_id),
            }
        }
        self.master_sample = self.master_sample.saturating_add(self.block as u64);
        Ok(())
    }

    /// Advance `count` blocks.
    pub fn process_blocks(&mut self, count: usize) -> Result<(), Graph2Error> {
        for _ in 0..count {
            self.process_block()?;
        }
        Ok(())
    }

    /// The audio accumulated at a Sink node so far.
    pub fn capture(&self, sink: NodeId) -> Option<&[f32]> {
        self.captures.get(&sink).map(|v| v.as_slice())
    }

    // ── Parametrized / scheduled changes (v3.28 timeline hook) ──────────────

    /// Set a Gain node's gain from the **next block start** (block-quantized).
    /// Control path; applies to the process following the next block.
    pub fn set_gain(&mut self, node: NodeId, gain: f32) -> Result<(), Graph2Error> {
        let n = self
            .nodes
            .get_mut(&node)
            .ok_or(Graph2Error::UnknownNode(node))?;
        if n.kind != NodeKind::Gain {
            return Err(Graph2Error::UnknownNode(node));
        }
        n.params = NodeParams::Gain { gain };
        // Drop any pending step for this node (a fresh absolute set wins).
        self.gain_steps.remove(&node);
        Ok(())
    }

    /// Attach (or detach) an external audio-input track (channel-major
    /// planes: `track[0]` = channel 0, …). When set, every *unaddressed*
    /// `Buffer` node (no clip address) plays this track (one-shot per its
    /// `loop` flag, cursors continuing across blocks) instead of its
    /// embedded samples. Mono callers pass `Some(vec![track])`.
    pub fn set_external_input(&mut self, track: Option<Vec<Vec<f32>>>) {
        self.external_input = track;
        self.buffer_cursors.clear();
    }

    /// Attach (or detach) a **per-clip** audio-input track (channel-major
    /// planes). Only `Buffer` nodes whose clip address matches `clip` play
    /// it (one-shot per their `loop` flag, cursors continuing across
    /// blocks); every other node keeps its embedded samples or the global
    /// external track. The aelog multi-input replay path registers one
    /// track per recorded clip, so each input reaches exactly the nodes
    /// bearing its address.
    pub fn set_external_clip(&mut self, clip: &str, track: Option<Vec<Vec<f32>>>) {
        match track {
            Some(t) => {
                self.external_clips.insert(clip.to_string(), t);
            }
            None => {
                self.external_clips.remove(clip);
            }
        }
        self.buffer_cursors.clear();
    }

    /// Attach (or detach) a v3.31 baked acoustic scene. `Acoustic` nodes
    /// render the room response of their configured source position from
    /// this cache; with no scene (or an unbaked position) they pass their
    /// input through unchanged (deterministic fallback). Resets the
    /// per-node tapped delay lines — the fresh-scene / session-start
    /// attach.
    pub fn set_baked_scene(&mut self, scene: Option<BakedScene>) {
        self.baked = scene;
        self.acoustics.clear();
        self.acoustic_epoch = self.acoustic_epoch.wrapping_add(1);
    }

    /// **Swap** the baked scene mid-session without resetting state — the
    /// animated-world path (and the aelog scene-swap replay). The new
    /// scene's direct gain and reflection taps apply from the next block,
    /// while each `Acoustic` node's tapped delay line keeps ringing from
    /// the shared input history, so a geometry change (a door opens, a
    /// wall turns to fabric) shifts the response seamlessly instead of
    /// cutting the room's tail.
    pub fn swap_baked_scene(&mut self, scene: BakedScene) {
        self.baked = Some(scene);
        // Kernels for the node's cell may differ (a different world can bake
        // the same source cell): bump the epoch so nodes recompile while the
        // raw-history rings keep ringing.
        self.acoustic_epoch = self.acoustic_epoch.wrapping_add(1);
    }

    /// **Drive** every `Acoustic` node's lookup position from a live
    /// listener position (the aelog listener-trajectory replay path). When
    /// set, each node renders the baked response at `position` instead of
    /// its own `NodeParams::Acoustic::position`, so a moving listener
    /// walks through the baked cells — exercising the full baked-room path
    /// while the tapped delay lines keep ringing. `None` restores the
    /// nodes' baked positions.
    pub fn set_listener_position(&mut self, position: Option<Vec3>) {
        self.listener_position = position;
        self.acoustic_epoch = self.acoustic_epoch.wrapping_add(1);
    }

    /// Register (or replace) a **named** baked scene under `name` — the
    /// store `Acoustic` nodes with `NodeParams::Acoustic::scene == Some(name)`
    /// render from. Per-listener bakes: one scene per listener, referenced
    /// by id, so a single graph renders distinct rooms and mixes them. The
    /// active (global) scene is untouched; the tapped delay lines keep
    /// ringing through a replacement.
    pub fn set_scene(&mut self, name: impl Into<String>, scene: BakedScene) {
        self.scenes.insert(name.into(), scene);
        self.acoustic_epoch = self.acoustic_epoch.wrapping_add(1);
    }

    /// Drop a named scene; nodes referencing it fall back to pass-through.
    pub fn remove_scene(&mut self, name: impl Into<String>) {
        self.scenes.remove(&name.into());
        self.acoustic_epoch = self.acoustic_epoch.wrapping_add(1);
    }

    /// Attach the **measured head-related impulse responses** every
    /// `HrtfSource::Dataset` /HRTF node renders with (or `None` to detach and
    /// make those nodes pass through). The dataset is control-path data;
    /// nodes reference it by azimuth/elevation/taps, so a graph's binaural
    /// branches use the real measured per-ear responses.
    pub fn set_hrtf_dataset(&mut self, dataset: Option<HrtfDataset>) {
        self.hrtf_dataset = dataset;
    }

    /// The attached measured HRTF dataset, if any.
    pub fn hrtf_dataset(&self) -> Option<&HrtfDataset> {
        self.hrtf_dataset.as_ref()
    }

    /// Schedule a gain step at a **local index within the current block**,
    /// applied sample-accurately: frames `[0, local)` keep the old gain,
    /// frames `[local, block)` use `gain`. A timeline firing an event at
    /// master sample `S` calls `set_gain_step(node, gain, S % block)` so
    /// the change lands on the exact sample.
    pub fn set_gain_step(
        &mut self,
        node: NodeId,
        gain: f32,
        local: usize,
    ) -> Result<(), Graph2Error> {
        if !self.nodes.contains_key(&node) {
            return Err(Graph2Error::UnknownNode(node));
        }
        self.gain_steps.insert(node, (gain, local.min(self.block)));
        Ok(())
    }

    /// Attach (or detach) a **tempo map** so registered gain automation is
    /// evaluated against musical time. Without a map (or `None`), curves are
    /// inert — gains hold at their static value.
    pub fn set_tempo_map(&mut self, map: Option<TempoMap>) {
        self.tempo_map = map;
    }

    /// Drive a Gain node's gain over time from a **tempo-mapped curve**:
    /// `curve` is authored in beats and evaluated against the attached tempo
    /// map, so `run_gain` sweeps the gain smoothly (sample-accurate linear
    /// ramp) across each block. `None`/`remove` restores the static gain.
    pub fn set_gain_automation(&mut self, node: NodeId, curve: Option<CurveBeats>) {
        match curve {
            Some(c) => {
                self.gain_automation.insert(node.0, c);
            }
            None => {
                self.gain_automation.remove(&node.0);
            }
        }
    }

    /// The current master sample (monotonic; the executor's clock). Exposed
    /// so hosts can align external automation with the playhead.
    pub fn master_sample(&self) -> u64 {
        self.master_sample
    }
}
