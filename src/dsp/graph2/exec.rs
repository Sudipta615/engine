//! Graph 2.0 offline executor (v3.27).
//!
//! [`OfflineExecutor`] renders a **compiled** [`Graph2`] topology block by
//! block. It is the "graph as the center" demonstration: audio flows along
//! the explicit edges in topological order, with no authored chain — a
//! dry/wet bus is just `Split → {Gain, Delay} → Mix`, a broadcast is one
//! output port feeding many edges.
//!
//! Each edge owns one single-channel plane (`block` frames, zeroed at block
//! start — an unconnected input reads silence, an unconnected output is
//! dropped). State lives per node (delay lines, oscillator phase). This is
//! deliberately an **offline** runtime: capture buffers grow, and no
//! realtime constraint is promised. Realtime lowering of a compiled order is
//! future work.

use std::collections::{BTreeMap, HashMap, VecDeque};

use super::edge::{EdgeDef, EdgeId};
use super::node::{NodeDef, NodeId, NodeKind, NodeParams, PortId, TestSignal};
use super::sort::ExecutionOrder;
use super::validate::Graph2Error;
use super::Graph2;
use crate::dsp::timeline::automation::CurveBeats;
use crate::dsp::timeline::tempo::TempoMap;
use crate::spatial::acoustic::bake::{spectral_taps, BakedScene, ACOUSTIC_IR_LEN};

/// Depth (samples) of the raw input-history ring each `Acoustic` node keeps
/// for per-path spectral filtering. Bounded below by room scale (the
/// longest image-source excess delay, several thousand samples at 48 kHz)
/// plus the filter span ([`ACOUSTIC_IR_LEN`]); fixed so the ring fills
/// continuously from session start and never drops history on a scene swap.
const ACOUSTIC_HISTORY: usize = ACOUSTIC_IR_LEN + 4096;
#[cfg(test)]
use crate::spatial::acoustic::bake::BakedObject;
#[cfg(test)]
use crate::spatial::acoustic::path::PathKind;
use crate::spatial::math::Vec3;

/// Renders a compiled topology over time. Snapshot semantics: construct from
/// a `(&Graph2, &ExecutionOrder)` pair; mutate the graph only through a
/// fresh compile + executor.
pub struct OfflineExecutor {
    block: usize,
    sample_rate: f32,
    nodes: BTreeMap<NodeId, NodeDef>,
    edges: BTreeMap<EdgeId, EdgeDef>,
    order: ExecutionOrder,
    /// One block-sized plane per edge, zeroed at each block start.
    edge_planes: HashMap<EdgeId, Vec<f32>>,
    /// Delay-line state per Delay node.
    delays: HashMap<NodeId, DelayState>,
    /// Oscillator phase / fired flag per Source node.
    sources: HashMap<NodeId, SourceState>,
    /// Accumulated captured audio per Sink node.
    captures: HashMap<NodeId, Vec<f32>>,
    /// Pending per-node gain steps `(gain, local block index)` applied by
    /// `run_gain` at the exact frame — sample-accurate parameter changes.
    gain_steps: HashMap<NodeId, (f32, usize)>,
    /// The acoustic world response cache consumed by `Acoustic` nodes
    /// (baked scene; see `set_baked_scene`).
    baked: Option<BakedScene>,
    /// Per-node tapped delay lines for `Acoustic` node room responses.
    acoustics: HashMap<NodeId, AcousticState>,
    /// Bumped whenever the acoustic world changes (`swap_baked_scene`,
    /// `set_listener_position`, `set_scene`/`remove_scene`) so `Acoustic`
    /// nodes recompile their per-path filter kernels even when the new
    /// world bakes the *same* source cell (the cell key alone can't tell
    /// two worlds apart). The raw-history ring is untouched, so the room
    /// keeps ringing.
    acoustic_epoch: u64,
    /// Monotonic total samples rendered (the executor's master clock
    /// position). Drives tempo-mapped automation: each `process_block`
    /// evaluates the registered [`CurveBeats`] curves over this span.
    master_sample: u64,
    /// The tempo map musical automation evaluates against (beat → sample).
    /// Without one, automation is inert (curves are meaningless without a
    /// tempo reference).
    tempo_map: Option<TempoMap>,
    /// Per-Gain-node tempo-mapped automation: `NodeId.0 → curve in beats`.
    /// When present (with a `tempo_map`), `run_gain` sweeps the gain
    /// smoothly across each block from the curve's value at block start to
    /// its value at block end.
    gain_automation: BTreeMap<u32, CurveBeats>,
    /// A live listener position: when set, every `Acoustic` node looks up
    /// the baked response at **this** position instead of its own
    /// `NodeParams::Acoustic::position` (the aelog listener-trajectory
    /// drive). When unset, each node uses its baked position unchanged.
    listener_position: Option<Vec3>,
    /// **Named** baked scenes keyed by id (`NodeParams::Acoustic::scene`):
    /// per-listener bakes one graph can mix — an `Acoustic` node whose
    /// params name a scene renders from here, otherwise the active scene.
    scenes: HashMap<String, BakedScene>,
    /// External audio-input track: when set, every *unaddressed* `Buffer`
    /// node (no clip address) plays this instead of its embedded samples
    /// (the aelog single-track replay path). Channel-major planes
    /// (`track[0]` = channel 0, …).
    external_input: Option<Vec<Vec<f32>>>,
    /// Per-clip external audio-input tracks, keyed by clip address: only
    /// `Buffer` nodes whose `NodeParams::Buffer::clip` matches play the
    /// matching track (the aelog multi-input replay path). Channel-major
    /// planes per track.
    external_clips: HashMap<String, Vec<Vec<f32>>>,
    /// Per-node playback cursor for `Buffer` nodes (sample index).
    buffer_cursors: HashMap<NodeId, usize>,
    /// Pipeline-delay state per `Convolution` node: the convolved stream
    /// held back by one kernel length before emission.
    convolutions: HashMap<NodeId, ConvState>,
    /// Per-ear pipeline-delay state per `HRTF` node (both ears delayed by
    /// the longer IR so the pair stays mutually aligned).
    hrtfs: HashMap<NodeId, HrtfState>,
}

/// The per-`Acoustic`-node acoustic rendering state: the object cell
/// currently compiled in (`key`), its per-path spectral filter kernels, and
/// a shared **raw input history ring**.
///
/// The ring holds the *unfiltered* input history, deliberately: delay and
/// filtering are both LTI (they commute), and the raw history is independent
/// of any cell's kernels — so when a scene swap or listener drive retargets
/// the node's cell, only the `paths` kernels change while the ring keeps
/// ringing seamlessly from the continuous session input (the animated-world
/// / golden semantics pinned by v3.35). Each path reads `kernel` convolved
/// against the raw history at its `excess` delay:
/// `out[k] += Σ_j h[j]·x[k − excess − j]`.
#[derive(Debug, Default)]
struct AcousticState {
    /// Cache key of the [`crate::spatial::acoustic::bake::BakedObject`]
    /// whose kernels are compiled in `paths`.
    key: Option<(i32, i32, i32)>,
    /// The [`OfflineExecutor::acoustic_epoch`] these kernels were compiled
    /// at; a bump forces a recompile even when `key` is unchanged (two
    /// worlds can bake the same source cell).
    epoch: u64,
    /// Per non-direct path: `(excess_delay, spectral filter kernel)`.
    paths: Vec<(i64, Vec<f32>)>,
    /// Shared raw input history ring (fixed depth so it never drops session
    /// history on a scene swap).
    raw: Vec<f32>,
    /// Ring write cursor.
    pos: usize,
}

/// The convolution pipeline: the convolved stream delayed by `delay`
/// samples before emission — the algorithmic latency a block-partitioned
/// convolver pays. Initialised with `delay` zeros so the first emitted
/// samples are the convolution at negative indices (zero), exactly
/// `output[k] = (x * h)[k - delay]`.
///
/// Streaming convolution is **overlap-add**: each block's convolved result
/// `y_b` (length `emit + overlap`) ends with `overlap = kernel.len() - 1`
/// samples that continue into the next block, and the next block's leading
/// `overlap` samples must be *added* to them (not dropped — the previous
/// block's tail is the same stream position). The `tail` accumulator
/// carries those addends; only the `emit` final samples of each block are
/// appended to the delay queue, which stays at a constant length, so the
/// pipeline delay never drifts.
#[derive(Debug, Default)]
struct ConvState {
    /// The delayed convolution stream; the head (`delay` zeros) makes the
    /// first emitted samples the convolution at negative indices (zero).
    pending: VecDeque<f32>,
    /// The `overlap`-sample addends carried from the previous block.
    tail: Vec<f32>,
}

impl ConvState {
    fn new(delay: usize) -> Self {
        let mut pending = VecDeque::with_capacity(delay + 256);
        pending.extend(std::iter::repeat_n(0.0, delay));
        Self {
            pending,
            tail: Vec::new(),
        }
    }

    /// Overlap-add block `y` (length `emit + overlap`) onto the stream,
    /// append its `emit` final samples, and emit the first `emit` samples
    /// (delayed by the initial head). `overlap` is `kernel.len() - 1` — or
    /// `0` for a pass-through ear (no convolution, no addends).
    fn push_and_emit(&mut self, mut y: Vec<f32>, overlap: usize, emit: usize) -> Vec<f32> {
        if overlap > 0 {
            let n = overlap.min(y.len());
            if !self.tail.is_empty() {
                for (v, t) in y.iter_mut().zip(self.tail.iter()).take(n) {
                    *v += t;
                }
            }
            let start = y.len().saturating_sub(overlap);
            self.tail = y[start..].to_vec();
        }
        let take = emit.min(y.len());
        self.pending.extend(y.into_iter().take(take));
        let mut out = Vec::with_capacity(emit);
        for _ in 0..emit {
            out.push(self.pending.pop_front().unwrap_or(0.0));
        }
        out
    }
}

/// The two per-ear pipeline queues of an `HRTF` node, both holding the
/// convolution stream back by the longer IR length.
#[derive(Debug, Default)]
struct HrtfState {
    left: ConvState,
    right: ConvState,
}

impl HrtfState {
    fn new(delay: usize) -> Self {
        Self {
            left: ConvState::new(delay),
            right: ConvState::new(delay),
        }
    }
}

#[derive(Debug, Default)]
struct DelayState {
    buf: Vec<f32>,
    pos: usize,
}

#[derive(Debug, Default)]
struct SourceState {
    phase: f32,
    fired: bool,
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
            hrtfs: HashMap::new(),
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

    // ── Node ops ────────────────────────────────────────────────────────────

    fn run_source(&mut self, id: NodeId) {
        let params = self.nodes.get(&id).map(|n| n.params.clone());
        let Some(NodeParams::Source(p)) = params else {
            return;
        };
        let st = self.sources.entry(id).or_default();
        let mut plane = vec![0.0f32; self.block];
        match p.signal {
            TestSignal::Impulse => {
                if !st.fired {
                    plane[0] = 1.0;
                    st.fired = true;
                }
            }
            TestSignal::Sine => {
                let step = 2.0 * std::f32::consts::PI * p.frequency_hz / self.sample_rate;
                for (i, s) in plane.iter_mut().enumerate() {
                    *s = (st.phase + step * i as f32).sin();
                }
                st.phase = (st.phase + step * self.block as f32) % (2.0 * std::f32::consts::PI);
            }
            TestSignal::Silence => {}
        }
        self.broadcast(id, PortId::OUT, &plane);
    }
    fn run_buffer(&mut self, id: NodeId) {
        let (embedded, loop_clip, clip_name) = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::Buffer {
                samples,
                looping,
                clip,
            }) => (samples, looping, clip),
            _ => return,
        };
        // Resolution order: an addressed node plays its registered clip
        // track (aelog multi-input replay); an unaddressed node plays the
        // global external track (aelog single-track replay); otherwise the
        // node's embedded clip plays. The resolved source is channel-major
        // planes; each plane feeds the matching output port (channel i of
        // a source with fewer channels reads silence, extra source
        // channels beyond the node's ports are dropped).
        let addressed: Option<&Vec<Vec<f32>>> = match &clip_name {
            Some(name) => self.external_clips.get(name),
            None => self.external_input.as_ref(),
        };
        let resolved: &Vec<Vec<f32>> = addressed.unwrap_or(&embedded);
        let max_len = resolved.iter().map(|c| c.len()).max().unwrap_or(0);
        let ports = self
            .nodes
            .get(&id)
            .map(|n| n.outputs.len())
            .unwrap_or(1)
            .max(1);
        // One shared cursor advances **per sample across all channels**,
        // so every port reads the same clip position in lockstep; planes
        // are built sample-major, then broadcast (no borrow of `self`
        // outlives the broadcast loop).
        let mut cursor = self.buffer_cursors.get(&id).copied().unwrap_or(0);
        let mut planes: Vec<Vec<f32>> = vec![vec![0.0f32; self.block]; ports];
        for s in 0..self.block {
            if cursor >= max_len {
                if loop_clip && max_len > 0 {
                    cursor = 0; // wrap the loop cursor
                } else {
                    continue; // one-shot: silence after the end
                }
            }
            for (port, src) in planes.iter_mut().zip(resolved.iter()) {
                port[s] = src.get(cursor).copied().unwrap_or(0.0);
            }
            cursor += 1;
        }
        self.buffer_cursors.insert(id, cursor);
        for (port, plane) in planes.into_iter().enumerate() {
            self.broadcast(id, PortId(port as u32), &plane);
        }
    }

    fn run_sink(&mut self, id: NodeId) {
        let inputs = self.nodes.get(&id).map(|n| n.inputs.len()).unwrap_or(0);
        // Collect owned planes first so the captures borrow is not held
        // across the read borrow (offline path — allocation is fine).
        let mut planes: Vec<Vec<f32>> = Vec::new();
        for i in 0..inputs {
            planes.push(
                self.read_input(id, PortId(i as u32))
                    .map(|p| p.to_vec())
                    .unwrap_or_else(|| vec![0.0; self.block]),
            );
        }
        let cap = self.captures.entry(id).or_default();
        for p in planes {
            cap.extend_from_slice(&p);
        }
    }

    fn run_gain(&mut self, id: NodeId) {
        let base_gain = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::Gain { gain }) => gain,
            _ => 1.0,
        };
        // Sample-accurate step: frames before `local` use the old gain,
        // frames at/after it use the stepped value. An explicit scheduled
        // step wins over tempo-mapped automation for the block.
        let step = self.gain_steps.remove(&id);
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        let mut out = vec![0.0f32; self.block];
        match step {
            Some((new_gain, local)) => {
                for (i, o) in out.iter_mut().enumerate() {
                    let g = if i >= local { new_gain } else { base_gain };
                    *o = in_plane[i] * g;
                }
                // Persist the new gain so subsequent blocks keep it.
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.params = NodeParams::Gain { gain: new_gain };
                }
            }
            None => match self.automation_gain(id) {
                // Tempo-mapped control curve: sweep gain smoothly across the
                // block between the curve's values at block start and end.
                Some((start, end)) => {
                    let ramp = end - start;
                    let denom = (self.block.saturating_sub(1)).max(1) as f32;
                    for (i, o) in out.iter_mut().enumerate() {
                        let t = i as f32 / denom;
                        *o = in_plane[i] * (start + ramp * t);
                    }
                }
                None => {
                    for (i, o) in out.iter_mut().enumerate() {
                        *o = in_plane[i] * base_gain;
                    }
                }
            },
        }
        self.broadcast(id, PortId::OUT, &out);
    }

    /// The `(start, end)` gain for `id`'s block under tempo-mapped
    /// automation, if a curve is registered for the node **and** a tempo
    /// map is attached. `start` is the curve at the block's first sample,
    /// `end` at the block's last sample.
    fn automation_gain(&self, id: NodeId) -> Option<(f32, f32)> {
        let curve = self.gain_automation.get(&id.0)?;
        let map = self.tempo_map.as_ref()?;
        if curve.is_empty() {
            return None;
        }
        let sr = self.sample_rate;
        let block_start = self.master_sample;
        let block_end = block_start + self.block.saturating_sub(1) as u64;
        let start = curve.evaluate(block_start, map, sr);
        let end = curve.evaluate(block_end, map, sr);
        Some((start, end))
    }

    fn run_delay(&mut self, id: NodeId) {
        let samples = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::Delay { samples }) => samples as usize,
            _ => 0,
        };
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        if samples == 0 {
            self.broadcast(id, PortId::OUT, &in_plane);
            return;
        }
        let mut out = vec![0.0f32; self.block];
        let st = self.delays.entry(id).or_default();
        // st.buf is a ring of `samples`; read-then-write per frame.
        for i in 0..self.block {
            out[i] = st.buf[st.pos];
            st.buf[st.pos] = in_plane[i];
            st.pos = (st.pos + 1) % st.buf.len();
        }
        self.broadcast(id, PortId::OUT, &out);
    }

    fn run_mix(&mut self, id: NodeId) {
        let node = self.nodes.get(&id).expect("node exists");
        let mut out = vec![0.0f32; self.block];
        for (i, _port) in node.inputs.iter().enumerate() {
            if let Some(plane) = self.read_input(id, PortId(i as u32)) {
                for (o, s) in out.iter_mut().zip(plane.iter()) {
                    *o += s;
                }
            }
        }
        self.broadcast(id, PortId::OUT, &out);
    }

    fn run_split(&mut self, id: NodeId) {
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        let outs = self.nodes.get(&id).map(|n| n.outputs.len()).unwrap_or(0);
        for i in 0..outs {
            self.broadcast(id, PortId(i as u32), &in_plane);
        }
    }

    fn run_acoustic(&mut self, id: NodeId) {
        let (baked_position, scene_id) = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::Acoustic { position, scene }) => (position, scene),
            _ => return,
        };
        // Which scene renders this node: a named scene if the params say so,
        // otherwise the executor's active (global) scene.
        let scene_ref: Option<&BakedScene> = match &scene_id {
            Some(name) => self.scenes.get(name),
            None => self.baked.as_ref(),
        };
        // A live listener position overrides the node's baked position
        // (the trajectory drive); otherwise the node renders its own cell.
        let position = self.listener_position.unwrap_or(baked_position);
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        // Direct path passes through (scaled by the baked direct gain).
        let mut out = in_plane.clone();
        let Some(obj) = scene_ref.and_then(|s| s.get(position)) else {
            self.broadcast(id, PortId::OUT, &out);
            return;
        };
        let direct_gain = obj.direct().map(|d| d.gain).unwrap_or(1.0);
        for s in out.iter_mut() {
            *s *= direct_gain;
        }
        // Non-direct paths: each is filtered by its own spectrum / corner
        // (v3.40 — per-path spectral filtering, replacing the single
        // collapsed broadband gain) then delayed by its excess delay and
        // added. The kernels are (re)compiled only when the node's active
        // cell changes (a scene swap or listener drive), so the streaming
        // tails stay continuous while the room's state is static; a flat
        // path reduces to a one-tap gain delta (the old behavior).
        let st = self.acoustics.entry(id).or_default();
        // First use allocates the shared raw-history ring once, sized to the
        // deepest read any cell could make (excess-delay + filter span). It
        // never resizes: the ring fills continuously from session start, so
        // a scene swap / listener drive only swaps the kernels while the
        // full historical input stays available — the golden semantics.
        if st.raw.is_empty() {
            st.raw = vec![0.0; ACOUSTIC_HISTORY];
        }
        if st.epoch != self.acoustic_epoch {
            st.epoch = self.acoustic_epoch;
            st.key = Some(obj.key);
            st.paths = spectral_taps(obj, ACOUSTIC_IR_LEN);
        }
        let ring_len = st.raw.len();
        for i in 0..self.block {
            st.raw[st.pos] = in_plane[i];
            for (excess, kernel) in &st.paths {
                // Filter the *delayed* raw history (delay ∘ filter = filter
                // ∘ delay, both LTI): a monophonic room colour reads
                // straight off the shared ring.
                let base = st.pos as i64 - *excess;
                let mut acc = 0.0f32;
                for (j, &hj) in kernel.iter().enumerate() {
                    let idx = (base - j as i64).rem_euclid(ring_len as i64) as usize;
                    acc += hj * st.raw[idx];
                }
                out[i] += acc;
            }
            st.pos = (st.pos + 1) % ring_len;
        }
        self.broadcast(id, PortId::OUT, &out);
    }

    fn run_convolution(&mut self, id: NodeId) {
        let kernel = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::Convolution { kernel }) => kernel,
            _ => return,
        };
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        if kernel.is_empty() {
            self.broadcast(id, PortId::OUT, &in_plane);
            return;
        }
        // Convolve this block (length block + kernel − 1) and emit delayed
        // by one kernel length — matching `node_latency` exactly. The
        // pipeline overlap-adds consecutive blocks so the delay never
        // drifts.
        let y = direct_convolve(&in_plane, &kernel);
        let st = self
            .convolutions
            .entry(id)
            .or_insert_with(|| ConvState::new(kernel.len()));
        let out = st.push_and_emit(y, kernel.len() - 1, self.block);
        self.broadcast(id, PortId::OUT, &out);
    }

    fn run_hrtf(&mut self, id: NodeId) {
        let (left, right) = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::HRTF { left, right }) => (left, right),
            _ => return,
        };
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        let delay = left.len().max(right.len());
        if delay == 0 {
            // No filters: both ears pass through (still mutually aligned).
            self.broadcast(id, PortId(0), &in_plane);
            self.broadcast(id, PortId(1), &in_plane);
            return;
        }
        // An empty ear IR means "no filter for that ear": pass the input
        // through (still delayed by `delay`, so the pair stays aligned).
        let y_l = if left.is_empty() {
            in_plane.clone()
        } else {
            direct_convolve(&in_plane, &left)
        };
        let y_r = if right.is_empty() {
            in_plane.clone()
        } else {
            direct_convolve(&in_plane, &right)
        };
        let st = self
            .hrtfs
            .entry(id)
            .or_insert_with(|| HrtfState::new(delay));
        // Per-ear overlap: each ear's own IR length − 1 (an empty ear passes
        // through with no overlap), while both ears share the same pipeline
        // delay so the pair stays aligned.
        let overlap_l = left.len().saturating_sub(1);
        let overlap_r = right.len().saturating_sub(1);
        let out_l = st.left.push_and_emit(y_l, overlap_l, self.block);
        let out_r = st.right.push_and_emit(y_r, overlap_r, self.block);
        self.broadcast(id, PortId(0), &out_l);
        self.broadcast(id, PortId(1), &out_r);
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    /// The plane of the single edge feeding `node`'s input port, if any.
    fn read_input(&self, node: NodeId, port: PortId) -> Option<&[f32]> {
        self.edges
            .values()
            .find(|e| e.target.node == node && e.target.port == port)
            .and_then(|e| self.edge_planes.get(&e.id))
            .map(|v| v.as_slice())
    }

    /// Write `plane` into every edge leaving `node`'s output port.
    fn broadcast(&mut self, node: NodeId, port: PortId, plane: &[f32]) {
        let targets: Vec<EdgeId> = self
            .edges
            .values()
            .filter(|e| e.source.node == node && e.source.port == port)
            .map(|e| e.id)
            .collect();
        for id in targets {
            if let Some(buf) = self.edge_planes.get_mut(&id) {
                buf.copy_from_slice(plane);
            }
        }
    }
}

/// Full linear convolution of one block with an FIR kernel
/// (`len(x) + len(h) − 1` samples). Offline path — the direct, exact
/// formulation; the realtime `dsp::convolution` engine remains the hot-path
/// partitioned counterpart.
fn direct_convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
    let mut y = vec![0.0f32; x.len() + h.len() - 1];
    for (i, &xi) in x.iter().enumerate() {
        for (j, &hj) in h.iter().enumerate() {
            y[i + j] += xi * hj;
        }
    }
    y
}

/// The renderable **broadband** taps of a baked response: `(excess_delay,
/// gain)` per non-direct path (the classic collapsed form). Retained for
/// the flat-material oracles and as the reduction a wholly flat path
/// equals; `run_acoustic` now applies true per-path spectral filters via
/// [`spectral_taps`], which collapses to these exact taps when every path
/// is flat.
#[cfg(test)]
fn acoustic_taps(obj: &BakedObject) -> Vec<(i64, f32)> {
    let direct_delay = obj.direct().map(|d| d.delay_samples).unwrap_or(0.0);
    obj.paths
        .iter()
        .filter(|p| p.kind != PathKind::Direct)
        .map(|p| {
            let excess = (p.delay_samples - direct_delay).max(0.0).round() as i64;
            (excess, p.gain)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn dry_wet_graph() -> (Graph2, ExecutionOrder, NodeId, NodeId) {
        // source → split(2) → {gain(0.5), delay(100)} → mix → sink
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let split = g.add_split("drywet", 2);
        let gain = g.add_gain("dry", 0.5);
        let delay = g.add_delay("wet", 100);
        let mix = g.add_mix("sum", 2);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
        g.add_edge(split, PortId(0), gain, PortId::IN).unwrap();
        g.add_edge(split, PortId(1), delay, PortId::IN).unwrap();
        g.add_edge(gain, PortId::OUT, mix, PortId(0)).unwrap();
        g.add_edge(delay, PortId::OUT, mix, PortId(1)).unwrap();
        g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        (g, order, sink, src)
    }

    #[test]
    fn dry_wet_split_and_mix_render() {
        let (g, order, sink, _src) = dry_wet_graph();
        let mut ex = OfflineExecutor::new(&g, &order, 128, SR).unwrap();
        ex.process_blocks(2).unwrap();
        let cap = ex.capture(sink).unwrap();
        assert_eq!(cap.len(), 256);
        // Dry impulse at frame 0 scaled by 0.5.
        assert!((cap[0] - 0.5).abs() < 1e-6, "dry: {}", cap[0]);
        // Wet impulse at frame 100, unscaled.
        assert!((cap[100] - 1.0).abs() < 1e-6, "wet: {}", cap[100]);
        // Nothing else.
        assert!(cap[1..100].iter().all(|s| s.abs() < 1e-6));
        assert!(cap[101..].iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn delay_across_block_boundary() {
        // Delay(300) with 128-frame blocks: the impulse written in block 0
        // must arrive at absolute sample 300.
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let d = g.add_delay("d", 300);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, d, PortId::IN).unwrap();
        g.add_edge(d, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 128, SR).unwrap();
        ex.process_blocks(3).unwrap();
        let cap = ex.capture(sink).unwrap();
        assert_eq!(cap.len(), 384);
        assert!(cap[..300].iter().all(|s| s.abs() < 1e-6));
        assert!((cap[300] - 1.0).abs() < 1e-6);
        assert!(cap[301..].iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn sine_source_is_continuous_across_blocks() {
        let mut g = Graph2::new();
        let src = g.add_source_with(
            "sine",
            super::super::node::SourceParams {
                signal: TestSignal::Sine,
                frequency_hz: 440.0,
            },
        );
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 128, SR).unwrap();
        ex.process_blocks(2).unwrap();
        let cap = ex.capture(sink).unwrap();
        assert_eq!(cap.len(), 256);
        // First sample starts at phase 0; sample 128 continues the sine —
        // the value at 128 must equal sin(2π·440·128/48000), i.e. the
        // continuous wave, not a restart.
        let expect = (2.0 * std::f32::consts::PI * 440.0 * 128.0 / SR).sin();
        assert!((cap[128] - expect).abs() < 1e-3, "{} vs {expect}", cap[128]);
        // And the wave is bounded.
        assert!(cap.iter().all(|s| s.abs() <= 1.0 + 1e-6));
    }

    #[test]
    fn gain_step_is_sample_accurate() {
        // sine → gain(0.0) → sink. A step to 2.0 at local frame 40 must leave
        // frames [0,40) silent and start scaling with 2.0 exactly at 40.
        let mut g = Graph2::new();
        let src = g.add_source_with(
            "sine",
            super::super::node::SourceParams {
                signal: TestSignal::Sine,
                frequency_hz: 1000.0,
            },
        );
        let gain = g.add_gain("vol", 0.0);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
        g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();

        let mut ex = OfflineExecutor::new(&g, &order, 128, SR).unwrap();
        ex.set_gain_step(gain, 2.0, 40).unwrap();
        ex.process_block().unwrap();
        let cap = ex.capture(sink).unwrap();
        assert_eq!(cap.len(), 128);
        // Frames before the step are silent (base gain 0).
        assert!(cap[..40].iter().all(|s| s.abs() < 1e-6));
        // Frame 40 onward is 2× the raw sine.
        let raw = |i: usize| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR).sin();
        assert!((cap[40] - 2.0 * raw(40)).abs() < 1e-3, "step frame 40");
        assert!((cap[100] - 2.0 * raw(100)).abs() < 1e-3, "after step");
        // The stepped gain persists to the next block (block-quantized set).
        // Block 1's frame 40 is absolute sample 128 + 40 = 168.
        ex.process_block().unwrap();
        let cap2 = ex.capture(sink).unwrap();
        let abs = 128 + 40;
        assert!(
            (cap2[abs] - 2.0 * raw(abs)).abs() < 1e-3,
            "persists into next block: {} vs {}",
            cap2[abs],
            2.0 * raw(abs)
        );
    }

    #[test]
    fn tempo_mapped_gain_automation_ramps_across_blocks() {
        // DC(1) → gain(0) → sink. A tempo-mapped curve (beat 0 → 0.0,
        // beat 2 → 1.0) at 120 BPM sweeps the gain linearly 0 → 1 over two
        // beats (48000 samples). The captured output equals the curve's
        // value at each sample — the gain is the signal.
        use crate::dsp::timeline::automation::CurveBeats;
        use crate::dsp::timeline::tempo::TempoMap;

        let block = 1200usize; // 120 BPM → 1 beat = 24000 samples = 20 blocks
        let mut g = Graph2::new();
        let src = g.add_buffer("dc", vec![1.0], true);
        let gain = g.add_gain("vol", 0.0);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
        g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();

        let mut map = TempoMap::new();
        map.push(0.0, 120.0);
        let curve = CurveBeats::from_points(&[(0.0, 0.0), (2.0, 1.0)]).unwrap();

        let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
        ex.set_tempo_map(Some(map));
        ex.set_gain_automation(gain, Some(curve));
        ex.process_blocks(40).unwrap(); // 48000 samples = 2 beats
        let cap = ex.capture(sink).unwrap();
        assert_eq!(cap.len(), 48_000);

        // Sample s → beat = s / 24000 → gain = s / 48000 (linear 0→1).
        let expect = |s: usize| s as f32 / 48_000.0;
        for s in [0, 4_799, 12_000, 23_999, 24_001, 36_000, 47_999] {
            assert!(
                (cap[s] - expect(s)).abs() < 2e-4,
                "sample {s}: got {}, want {}",
                cap[s],
                expect(s)
            );
        }
        assert!((cap[0] - 0.0).abs() < 1e-6);
        assert!((cap[47_999] - expect(47_999)).abs() < 2e-4, "reaches ~1.0");
        // The ramp is strictly non-decreasing (monotone DC → volume rises).
        assert!(cap.windows(100).all(|w| w[0] <= w[99] + 1e-5));

        // Detaching the curve (or clearing the tempo map) restores the
        // static gain 0.
        let mut ex2 = OfflineExecutor::new(&g, &order, block, SR).unwrap();
        ex2.set_tempo_map(Some(TempoMap::new()));
        ex2.set_gain_automation(
            gain,
            Some(CurveBeats::from_points(&[(0.0, 0.4), (4.0, 0.4)]).unwrap()),
        );
        ex2.set_gain_automation(gain, None);
        ex2.process_blocks(2).unwrap();
        assert!(ex2.capture(sink).unwrap().iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn a_tempo_change_shifts_where_automation_landmarks_land() {
        // Same curve, but the tempo doubles at beat 1: the value at a *beat*
        // is identical, yet the sample where the gain reaches 0.75 moves —
        // beat 1.5 = beat 1 (at 24000 samples @120) + 0.5 beat @240 (6000)
        // = sample 30000.
        use crate::dsp::timeline::automation::CurveBeats;
        use crate::dsp::timeline::tempo::TempoMap;

        let block = 100usize;
        let mut g = Graph2::new();
        let src = g.add_buffer("dc", vec![1.0], true);
        let gain = g.add_gain("vol", 0.0);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
        g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();

        let mut map = TempoMap::new();
        map.push(0.0, 120.0);
        map.push(1.0, 240.0);
        let curve = CurveBeats::from_points(&[(0.0, 0.0), (2.0, 1.0)]).unwrap();

        let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
        ex.set_tempo_map(Some(map.clone()));
        ex.set_gain_automation(gain, Some(curve));
        // Run 30100 samples = 301 blocks (1.5 beats under the changing map,
        // plus one block so sample 30000 exists in the capture).
        ex.process_blocks(301).unwrap();
        let cap = ex.capture(sink).unwrap();
        // The curve value is piecewise-linear in *beats*: 0 at beat 0, 1.0 at
        // beat 2. So value at beat 1.5 = 0.75, and beat 1.5 sits at sample
        // 30000 after the tempo-up.
        assert!(
            (cap[30_000] - 0.75).abs() < 5e-4,
            "gain 0.75 at beat 1.5 = sample 30000: got {}",
            cap[30_000]
        );
        // Before the tempo-up, at 120 BPM the midpoint (beat 1 = sample 24000)
        // is 0.5.
        assert!(
            (map.beat_at_sample(24_000.0, SR) - 1.0).abs() < 1e-9
                && (cap[24_000] - 0.5).abs() < 5e-4,
            "beat 1 (sample 24000) still 0.5"
        );
    }

    #[test]
    fn acoustic_node_reproduces_baked_room_response() {
        use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
        use crate::spatial::acoustic::geometry::AcousticRoom;
        use crate::spatial::acoustic::material::MaterialSpectrum;
        use crate::spatial::acoustic::solver::AcousticWorld;
        use crate::spatial::math::Vec3;
        use crate::spatial::room::Room;

        let room = Room {
            enabled: true,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 1,
            rt60_ms: 800.0,
            late_mix: 0.0,
            speed_of_sound: 343.0,
        };
        let world = AcousticWorld::new(
            AcousticRoom::from_render_room(
                &room,
                MaterialSpectrum::flat_reflective(room.absorption),
            ),
            SR,
        );
        let pos = Vec3::new(1.0, 5.0, 1.5);
        let lst = Vec3::new(6.0, 5.0, 1.5);
        let scene = AcousticBaker::new(world, 0.5).bake_single(pos, lst, SR, BakePolicy::default());

        // Oracle first (releases the scene borrow): direct at 0 (direct
        // gain), plus each non-direct path at its excess delay with its
        // gain — the same arithmetic the node runs.
        let obj = scene.get(pos).expect("baked object");
        let direct_delay = obj.direct().unwrap().delay_samples;
        let mut expected = vec![0.0f32; 2048];
        expected[0] = obj.direct().unwrap().gain;
        for p in obj.paths.iter().filter(|p| p.kind != PathKind::Direct) {
            let excess = (p.delay_samples - direct_delay).max(0.0).round() as usize;
            if excess < 2048 {
                expected[excess] += p.gain;
            }
        }
        assert!(
            expected.iter().skip(1).any(|s| s.abs() > 1e-6),
            "bake has reflections"
        );

        // Now the graph side: an impulse into the Acoustic node with the
        // baked scene attached must reproduce the oracle exactly.
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let room_node = g.add_acoustic("room", pos);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, room_node, PortId::IN).unwrap();
        g.add_edge(room_node, PortId::OUT, sink, PortId::IN)
            .unwrap();
        let order = g.compile().unwrap().clone();

        let mut ex = OfflineExecutor::new(&g, &order, 512, SR).unwrap();
        ex.set_baked_scene(Some(scene));
        ex.process_blocks(4).unwrap(); // 2048 frames — covers the reflection tail
        let cap = ex.capture(sink).unwrap();
        assert_eq!(cap, expected, "graph acoustic node == baked response");
    }

    #[test]
    fn swap_baked_scene_switches_taps_without_cutting_the_tail() {
        // A sine through the Acoustic node with scene A (concrete), then a
        // mid-session swap to scene B (fabric MinX wall — weaker
        // reflections). The swap must take effect from the next block while
        // the tapped delay line keeps ringing: output[k] = direct·x[k] +
        // Σ gain_e·x[k−e] with A's taps before the swap and B's after, all
        // from the same continuous input history.
        use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
        use crate::spatial::acoustic::geometry::AcousticRoom;
        use crate::spatial::acoustic::material::{MaterialKind, MaterialSpectrum};
        use crate::spatial::acoustic::solver::wall_index;
        use crate::spatial::acoustic::solver::AcousticWorld;
        use crate::spatial::math::Vec3;
        use crate::spatial::room::Room;

        let room = Room {
            enabled: true,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 1,
            rt60_ms: 800.0,
            late_mix: 0.0,
            speed_of_sound: 343.0,
        };
        let base = MaterialSpectrum::flat_reflective(room.absorption);
        let world_a = AcousticWorld::new(AcousticRoom::from_render_room(&room, base), SR);
        let mut room_b = AcousticRoom::from_render_room(&room, base);
        room_b.walls[wall_index(crate::spatial::Wall::MinX)] = MaterialKind::Fabric.spectrum();
        let world_b = AcousticWorld::new(room_b, SR);
        let pos = Vec3::new(1.0, 5.0, 1.5);
        let lst = Vec3::new(6.0, 5.0, 1.5);
        let scene_a =
            AcousticBaker::new(world_a, 0.5).bake_single(pos, lst, SR, BakePolicy::default());
        let scene_b =
            AcousticBaker::new(world_b, 0.5).bake_single(pos, lst, SR, BakePolicy::default());

        // Oracle taps per scene: per-path spectral filter kernels (excess,
        // kernel). Scene A is flat (single-tap gain kernels, matching the
        // classic broadband form); scene B's fabric wall spectrally colours
        // its reflections. Read before the scenes move into the executor.
        let taps = |scene: &crate::spatial::acoustic::bake::BakedScene| {
            spectral_taps(scene.get(pos).unwrap(), ACOUSTIC_IR_LEN)
        };
        let taps_a = taps(&scene_a);
        let taps_b = taps(&scene_b);
        let dg_a = scene_a
            .get(pos)
            .unwrap()
            .direct()
            .map(|d| d.gain)
            .unwrap_or(1.0);
        let dg_b = scene_b
            .get(pos)
            .unwrap()
            .direct()
            .map(|d| d.gain)
            .unwrap_or(1.0);
        // Fabric dampens the *broadband* reflected energy (its low-pass
        // colours rather than merely scales — the spectral point of v3.40).
        let bband = |scene: &crate::spatial::acoustic::bake::BakedScene| {
            let obj = scene.get(pos).unwrap();
            obj.paths
                .iter()
                .filter(|p| p.kind != crate::spatial::acoustic::path::PathKind::Direct)
                .map(|p| p.gain.abs())
                .sum::<f32>()
        };
        assert!(
            bband(&scene_a) > bband(&scene_b),
            "fabric weakens the reflections"
        );

        let mut g = Graph2::new();
        let src = g.add_source_with(
            "tone",
            super::super::node::SourceParams {
                signal: TestSignal::Sine,
                frequency_hz: 160.0,
            },
        );
        let room_node = g.add_acoustic("room", pos);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, room_node, PortId::IN).unwrap();
        g.add_edge(room_node, PortId::OUT, sink, PortId::IN)
            .unwrap();
        let order = g.compile().unwrap().clone();

        let block = 256usize;
        let swap_at = 4 * block; // after four blocks, mid-session
        let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
        ex.set_baked_scene(Some(scene_a));
        ex.process_blocks(4).unwrap();
        ex.swap_baked_scene(scene_b);
        ex.process_blocks(4).unwrap();
        let cap = ex.capture(sink).unwrap();

        let x = |k: isize| {
            if k < 0 {
                0.0
            } else {
                (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin()
            }
        };
        let oracle = |k: isize, t: &[(i64, Vec<f32>)], dg: f32| {
            let mut v = dg * x(k);
            for (excess, kern) in t {
                for (j, &hj) in kern.iter().enumerate() {
                    v += hj * x(k - *excess as isize - j as isize);
                }
            }
            v
        };
        let mut expected = vec![0.0f32; cap.len()];
        for (k, e) in expected.iter_mut().enumerate() {
            let (t, dg) = if (k as isize) < swap_at as isize {
                (&taps_a, dg_a)
            } else {
                (&taps_b, dg_b)
            };
            *e = oracle(k as isize, t, dg);
        }
        assert!(
            cap[swap_at + 1..].iter().any(|s| s.abs() > 0.5),
            "ringing continues"
        );
        // Tolerance, not bit-exact: the sine source accumulates phase
        // incrementally while the oracle evaluates the absolute angle, so
        // the two differ in float low bits (the aelog golden checks assert
        // byte-exactness between two runs of the *same* code path).
        for (k, (got, want)) in cap.iter().zip(expected.iter()).enumerate() {
            assert!(
                (got - want).abs() < 1e-4,
                "sample {k}: got {got}, oracle {want}"
            );
        }
        // The swap visibly changes the response (fabric weakens the taps).
        let a_only = |k: isize| oracle(k, &taps_a, dg_a);
        let b_only = |k: isize| oracle(k, &taps_b, dg_b);
        let mid = swap_at + 600;
        assert!(
            (a_only(mid as isize) - b_only(mid as isize)).abs() > 1e-3,
            "the swap changes the rendered response"
        );
    }

    #[test]
    fn listener_position_drives_the_acoustic_lookup() {
        // Bake two source cells P0 and P1 of the same room. A listener
        // drive retargets the node's lookup from P0 to P1 (and back),
        // rendering each cell's own response against the continuous sine
        // history — the moving-listener path.
        use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
        use crate::spatial::acoustic::geometry::AcousticRoom;
        use crate::spatial::acoustic::material::MaterialSpectrum;
        use crate::spatial::acoustic::solver::AcousticWorld;
        use crate::spatial::math::Vec3;
        use crate::spatial::room::Room;

        let room = Room {
            enabled: true,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 1,
            rt60_ms: 800.0,
            late_mix: 0.0,
            speed_of_sound: 343.0,
        };
        let world = AcousticWorld::new(
            AcousticRoom::from_render_room(
                &room,
                MaterialSpectrum::flat_reflective(room.absorption),
            ),
            SR,
        );
        let baker = AcousticBaker::new(world, 0.5);
        let lst = Vec3::new(6.0, 5.0, 1.5);
        let p0 = Vec3::new(1.0, 5.0, 1.5);
        let p1 = Vec3::new(1.0, 6.5, 1.5); // a different cell (y + 1.5)
        let scene = baker.bake_scene([p0, p1], lst, SR, BakePolicy::default());
        assert_ne!(
            scene.get(p0).unwrap().key,
            scene.get(p1).unwrap().key,
            "two distinct baked cells"
        );

        // Owned per-cell handles, computed before the scene moves into the
        // executor (no closure keeps a borrow on `scene`).
        let taps0 = acoustic_taps(scene.get(p0).unwrap());
        let taps1 = acoustic_taps(scene.get(p1).unwrap());
        let dg0 = scene
            .get(p0)
            .unwrap()
            .direct()
            .map(|d| d.gain)
            .unwrap_or(1.0);
        let dg1 = scene
            .get(p1)
            .unwrap()
            .direct()
            .map(|d| d.gain)
            .unwrap_or(1.0);

        let mut g = Graph2::new();
        let src = g.add_source_with(
            "tone",
            super::super::node::SourceParams {
                signal: TestSignal::Sine,
                frequency_hz: 160.0,
            },
        );
        let room_node = g.add_acoustic("room", p0);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, room_node, PortId::IN).unwrap();
        g.add_edge(room_node, PortId::OUT, sink, PortId::IN)
            .unwrap();
        let order = g.compile().unwrap().clone();

        let block = 256usize;
        let move_at = 5 * block;
        let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
        ex.set_baked_scene(Some(scene));
        ex.process_blocks(5).unwrap(); // renders p0 (the node's baked position)
        ex.set_listener_position(Some(p1)); // listener walks to p1
        ex.process_blocks(3).unwrap();
        ex.set_listener_position(Some(p0)); // and back
        ex.process_blocks(2).unwrap();
        let cap = ex.capture(sink).unwrap();

        let x = |k: isize| {
            if k < 0 {
                0.0
            } else {
                (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin()
            }
        };
        let oracle = |k: isize, t: &[(i64, f32)], d: f32| {
            let mut v = d * x(k);
            for &(excess, gain) in t {
                v += gain * x(k - excess as isize);
            }
            v
        };
        let seg_for = |k: usize| {
            if k < move_at {
                (taps0.as_slice(), dg0)
            } else if k < move_at + 3 * block {
                (taps1.as_slice(), dg1)
            } else {
                (taps0.as_slice(), dg0)
            }
        };
        for (k, got) in cap.iter().enumerate() {
            let (t, d) = seg_for(k);
            assert!(
                (got - oracle(k as isize, t, d)).abs() < 1e-4,
                "sample {k}: got {got}"
            );
        }
        // The motion changed the render: mid-segment differs from the
        // p0-only oracle.
        let mid = move_at + block + 100;
        assert!(
            (cap[mid] - oracle(mid as isize, &taps0, dg0)).abs() > 1e-3,
            "moving the listener visibly changed the response"
        );
        // Clearing the drive restores the node's own position (the p0
        // oracle at the very first sample).
        let mut ex2 = OfflineExecutor::new(&g, &order, block, SR).unwrap();
        ex2.set_baked_scene(ex.baked.clone());
        ex2.set_listener_position(Some(p1));
        ex2.set_listener_position(None);
        ex2.process_blocks(1).unwrap();
        let first = ex2.capture(sink).unwrap();
        assert!(
            (first[0] - oracle(0, &taps0, dg0)).abs() < 1e-4,
            "None restores the node's baked position"
        );
    }

    #[test]
    fn named_scenes_render_per_listener_responses() {
        // Two bakes of the same room, for two different listener positions
        // but the SAME source cell: one named scene per listener. Two
        // Acoustic nodes (same source, different scene ids) must each
        // render their own listener's response; an unregistered id falls
        // back to pass-through; replacing a named scene changes that node.
        use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
        use crate::spatial::acoustic::geometry::AcousticRoom;
        use crate::spatial::acoustic::material::MaterialSpectrum;
        use crate::spatial::acoustic::solver::AcousticWorld;
        use crate::spatial::math::Vec3;
        use crate::spatial::room::Room;

        let room = Room {
            enabled: true,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 1,
            rt60_ms: 800.0,
            late_mix: 0.0,
            speed_of_sound: 343.0,
        };
        let world = AcousticWorld::new(
            AcousticRoom::from_render_room(
                &room,
                MaterialSpectrum::flat_reflective(room.absorption),
            ),
            SR,
        );
        let baker = AcousticBaker::new(world, 0.5);
        let pos = Vec3::new(1.0, 5.0, 1.5);
        let front = baker.bake_single(pos, Vec3::new(6.0, 5.0, 1.5), SR, BakePolicy::default());
        let back = baker.bake_single(pos, Vec3::new(6.0, 2.0, 1.5), SR, BakePolicy::default());
        assert_ne!(front.get(pos).unwrap().paths, back.get(pos).unwrap().paths);

        // Oracle: the node's impulse response for one scene at `pos`.
        let response = |scene: &crate::spatial::acoustic::bake::BakedScene| {
            let obj = scene.get(pos).unwrap();
            let direct_delay = obj.direct().unwrap().delay_samples;
            let mut e = vec![0.0f32; 2048];
            e[0] = obj.direct().unwrap().gain;
            for p in obj.paths.iter().filter(|p| p.kind != PathKind::Direct) {
                let excess = (p.delay_samples - direct_delay).max(0.0).round() as usize;
                if excess < e.len() {
                    e[excess] += p.gain;
                }
            }
            e
        };
        let exp_front = response(&front);
        let exp_back = response(&back);
        assert_ne!(
            exp_front, exp_back,
            "different listeners → different responses"
        );

        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let split = g.add_split("s", 2);
        let n_front = g.add_acoustic_scene("front", pos, "front");
        let n_back = g.add_acoustic_scene("back", pos, "back");
        let sink_f = g.add_sink("f");
        let sink_b = g.add_sink("b");
        g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
        g.add_edge(split, PortId(0), n_front, PortId::IN).unwrap();
        g.add_edge(split, PortId(1), n_back, PortId::IN).unwrap();
        g.add_edge(n_front, PortId::OUT, sink_f, PortId::IN)
            .unwrap();
        g.add_edge(n_back, PortId::OUT, sink_b, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();

        let mut ex = OfflineExecutor::new(&g, &order, 512, SR).unwrap();
        ex.set_scene("front", front);
        ex.set_scene("back", back);
        ex.process_blocks(4).unwrap();
        assert_eq!(ex.capture(sink_f).unwrap(), exp_front, "front listener");
        assert_eq!(ex.capture(sink_b).unwrap(), exp_back, "back listener");

        // An unregistered scene id passes the input through unchanged.
        let mut g2 = Graph2::new();
        let s2 = g2.add_source("imp");
        let unknown = g2.add_acoustic_scene("u", pos, "missing");
        let k2 = g2.add_sink("k");
        g2.add_edge(s2, PortId::OUT, unknown, PortId::IN).unwrap();
        g2.add_edge(unknown, PortId::OUT, k2, PortId::IN).unwrap();
        let order2 = g2.compile().unwrap().clone();
        let mut ex2 = OfflineExecutor::new(&g2, &order2, 512, SR).unwrap();
        ex2.process_blocks(1).unwrap();
        let cap = ex2.capture(k2).unwrap();
        assert_eq!(cap[0], 1.0, "impulse passes through untouched");
        assert!(cap[1..].iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn buffer_plays_embedded_clip_one_shot_and_loops() {
        // One-shot: [1,2,3] then silence.
        let mut g = Graph2::new();
        let b = g.add_buffer("in", vec![1.0, 2.0, 3.0], false);
        let sink = g.add_sink("out");
        g.add_edge(b, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
        ex.process_blocks(2).unwrap();
        let cap = ex.capture(sink).unwrap();
        assert_eq!(cap, [1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0]);

        // Looping: [1,2,3] repeats.
        let mut g2 = Graph2::new();
        let b2 = g2.add_buffer("in", vec![1.0, 2.0, 3.0], true);
        let sink2 = g2.add_sink("out");
        g2.add_edge(b2, PortId::OUT, sink2, PortId::IN).unwrap();
        let order2 = g2.compile().unwrap().clone();
        let mut ex2 = OfflineExecutor::new(&g2, &order2, 4, SR).unwrap();
        ex2.process_blocks(2).unwrap();
        let cap2 = ex2.capture(sink2).unwrap();
        assert_eq!(cap2, [1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 1.0, 2.0]);
    }

    #[test]
    fn external_input_overrides_buffer_clip() {
        let mut g = Graph2::new();
        let b = g.add_buffer("in", vec![], false); // empty embedded clip
        let sink = g.add_sink("out");
        g.add_edge(b, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 3, SR).unwrap();
        ex.set_external_input(Some(vec![vec![9.0, 8.0, 7.0, 6.0]]));
        ex.process_blocks(2).unwrap();
        let cap = ex.capture(sink).unwrap();
        assert_eq!(
            cap,
            [9.0, 8.0, 7.0, 6.0, 0.0, 0.0],
            "one-shot external track"
        );
    }

    #[test]
    fn clip_addressed_external_track_feeds_only_matching_nodes() {
        // Two buffers: "mic" (clip "mic") and "aux" (unaddressed). A
        // per-clip track registered for "mic" plays only through the mic
        // node; the global external track drives only the unaddressed
        // node — the multi-input routing contract.
        let mut g = Graph2::new();
        let mic = g.add_buffer_clip("mic", "mic", vec![], false);
        let aux = g.add_buffer("aux", vec![], false);
        let sm = g.add_sink("mic-out");
        let sa = g.add_sink("aux-out");
        g.add_edge(mic, PortId::OUT, sm, PortId::IN).unwrap();
        g.add_edge(aux, PortId::OUT, sa, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();

        let mut ex = OfflineExecutor::new(&g, &order, 3, SR).unwrap();
        ex.set_external_clip("mic", Some(vec![vec![1.0, 2.0, 3.0, 4.0]]));
        ex.set_external_input(Some(vec![vec![9.0, 8.0, 7.0, 6.0]]));
        ex.process_blocks(2).unwrap();
        assert_eq!(
            ex.capture(sm).unwrap(),
            [1.0, 2.0, 3.0, 4.0, 0.0, 0.0],
            "clip track feeds the addressed node only"
        );
        assert_eq!(
            ex.capture(sa).unwrap(),
            [9.0, 8.0, 7.0, 6.0, 0.0, 0.0],
            "unaddressed node keeps the global track"
        );

        // A clip-named node with no registered track plays its embedded
        // clip — no silent cross-feeding from another clip's track.
        let mut g2 = Graph2::new();
        let b = g2.add_buffer_clip("synth", "synth", vec![5.0, 5.0], false);
        let s2 = g2.add_sink("out");
        g2.add_edge(b, PortId::OUT, s2, PortId::IN).unwrap();
        let order2 = g2.compile().unwrap().clone();
        let mut ex2 = OfflineExecutor::new(&g2, &order2, 3, SR).unwrap();
        ex2.set_external_clip("mic", Some(vec![vec![1.0, 2.0, 3.0]])); // not "synth"
        ex2.process_blocks(1).unwrap();
        assert_eq!(
            ex2.capture(s2).unwrap(),
            [5.0, 5.0, 0.0],
            "embedded clip when no track for the address"
        );
    }

    #[test]
    fn stereo_buffer_plays_each_channel_on_its_own_port() {
        // A two-channel clip: ports 0 (L) and 1 (R) each carry their own
        // plane in lockstep, looping together — the graph side of a stereo
        // aelog track.
        let mut g = Graph2::new();
        let b = g.add_buffer_channels(
            "stereo",
            vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]],
            true,
        );
        let sl = g.add_sink("l");
        let sr = g.add_sink("r");
        g.add_edge(b, PortId(0), sl, PortId::IN).unwrap();
        g.add_edge(b, PortId(1), sr, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
        ex.process_blocks(2).unwrap();
        assert_eq!(
            ex.capture(sl).unwrap(),
            [1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 1.0, 2.0],
            "left plane loops on port 0"
        );
        assert_eq!(
            ex.capture(sr).unwrap(),
            [4.0, 5.0, 6.0, 4.0, 5.0, 6.0, 4.0, 5.0],
            "right plane loops on port 1"
        );

        // A mono external track on a stereo node: channel 1 reads silence
        // (no upmix — the track is what it is); the looping cursor repeats
        // the single plane in lockstep.
        let mut ex2 = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
        ex2.set_external_input(Some(vec![vec![7.0, 8.0]]));
        ex2.process_blocks(1).unwrap();
        assert_eq!(ex2.capture(sl).unwrap(), [7.0, 8.0, 7.0, 8.0]);
        assert_eq!(ex2.capture(sr).unwrap(), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn convolution_renders_kernel_at_its_pipeline_delay() {
        // Impulse → conv(kernel [1,2,3]) → sink, block 4. The node emits
        // `output[k] = (x * h)[k - N]` with N = kernel length, so the
        // impulse response appears at offset 3 — matching node_latency.
        let kernel = vec![1.0, 2.0, 3.0];
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let conv = g.add_convolution("c", kernel.clone());
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
        g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
        ex.process_blocks(2).unwrap();
        let cap = ex.capture(sink).unwrap();
        assert_eq!(
            cap,
            [0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 0.0, 0.0],
            "IR at offset kernel.len(), tail carries across blocks"
        );
    }

    #[test]
    fn convolution_pipeline_does_not_drift_over_many_blocks() {
        // Sine → conv(identity kernel, 300 taps) → sink, block 256. The
        // output must stay exactly x[k-300] after 94 blocks — a per-block
        // overlap mishandling would grow the pipeline queue and drift (or
        // silence) the response over time.
        let mut h = vec![0.0; 300];
        h[0] = 1.0; // identity kernel: convolution = delay by 300
        let mut g = Graph2::new();
        let src = g.add_source_with(
            "tone",
            crate::dsp::graph2::node::SourceParams {
                signal: TestSignal::Sine,
                frequency_hz: 160.0,
            },
        );
        let conv = g.add_convolution("c", h);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
        g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 256, SR).unwrap();
        ex.process_blocks(94).unwrap(); // 24 064 samples, past the gate point
        let cap = ex.capture(sink).unwrap();
        let x = |k: usize| (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin();
        assert!((cap[301] - x(1)).abs() < 1e-3, "early: {}", cap[301]);
        assert!(
            (cap[10_000] - x(9_700)).abs() < 1e-3,
            "mid-run: {} vs {}",
            cap[10_000],
            x(9_700)
        );
        assert!(
            (cap[23_990] - x(23_690)).abs() < 1e-3,
            "late (no drift): {} vs {}",
            cap[23_990],
            x(23_690)
        );
    }

    #[test]
    fn convolution_kernel_longer_than_block_is_continuous() {
        // A 10-tap kernel with block 4: the response must span blocks
        // without a seam — out[k] = h[k-10] for k >= 10.
        let h: Vec<f32> = (1..=10).map(|i| i as f32).collect();
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let conv = g.add_convolution("c", h.clone());
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
        g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
        ex.process_blocks(5).unwrap(); // 20 frames ≥ 10 + 10
        let cap = ex.capture(sink).unwrap();
        for k in 0..cap.len() {
            let want = if k >= 10 { h[k - 10] } else { 0.0 };
            assert_eq!(cap[k], want, "frame {k}");
        }
    }

    #[test]
    fn hrtf_renders_stereo_pair_aligned_to_longer_ir() {
        // Left IR 5 taps, right IR 2 taps: both ears are delayed by 5 (the
        // node's reported taps), so the pair stays mutually aligned.
        let left = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let right = vec![10.0, 20.0];
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let hrtf = g.add_hrtf("bin", left.clone(), right.clone());
        let sl = g.add_sink("l");
        let sr = g.add_sink("r");
        g.add_edge(src, PortId::OUT, hrtf, PortId::IN).unwrap();
        g.add_edge(hrtf, PortId(0), sl, PortId::IN).unwrap();
        g.add_edge(hrtf, PortId(1), sr, PortId::IN).unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 8, SR).unwrap();
        ex.process_blocks(2).unwrap();

        let mut exp_l = vec![0.0; 5];
        exp_l.extend(left.iter().copied());
        exp_l.resize(16, 0.0);
        let mut exp_r = vec![0.0; 5];
        exp_r.extend(right.iter().copied());
        exp_r.resize(16, 0.0);
        assert_eq!(ex.capture(sl).unwrap(), exp_l, "left ear at offset 5");
        assert_eq!(ex.capture(sr).unwrap(), exp_r, "right ear aligned too");
    }
}
