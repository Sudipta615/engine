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
use crate::spatial::acoustic::bake::{BakedObject, BakedScene};
use crate::spatial::acoustic::path::PathKind;

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
    /// External audio-input track: when set, every `Buffer` node plays this
    /// instead of its embedded samples (the aelog replay path).
    external_input: Option<Vec<f32>>,
    /// Per-node playback cursor for `Buffer` nodes (sample index).
    buffer_cursors: HashMap<NodeId, usize>,
    /// Pipeline-delay state per `Convolution` node: the convolved stream
    /// held back by one kernel length before emission.
    convolutions: HashMap<NodeId, ConvState>,
    /// Per-ear pipeline-delay state per `HRTF` node (both ears delayed by
    /// the longer IR so the pair stays mutually aligned).
    hrtfs: HashMap<NodeId, HrtfState>,
}

/// Ring buffer + cursor for an `Acoustic` node's tapped delay line.
#[derive(Debug, Default)]
struct AcousticState {
    buf: Vec<f32>,
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
            external_input: None,
            buffer_cursors: HashMap::new(),
            convolutions: HashMap::new(),
            hrtfs: HashMap::new(),
        })
    }

    /// Advance the graph one block. All edge planes are re-zeroed first, so
    /// stale samples never leak between blocks.
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

    /// Attach (or detach) an external mono audio-input track. When set,
    /// every `Buffer` node plays this track (one-shot per its `loop` flag,
    /// cursors continuing across blocks) instead of its embedded samples.
    pub fn set_external_input(&mut self, track: Option<Vec<f32>>) {
        self.external_input = track;
        self.buffer_cursors.clear();
    }

    /// Attach (or detach) a v3.31 baked acoustic scene. `Acoustic` nodes
    /// render the room response of their configured source position from
    /// this cache; with no scene (or an unbaked position) they pass their
    /// input through unchanged (deterministic fallback).
    pub fn set_baked_scene(&mut self, scene: Option<BakedScene>) {
        self.baked = scene;
        self.acoustics.clear();
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
        let (embedded, loop_clip) = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::Buffer { samples, looping }) => (samples, looping),
            _ => return,
        };
        // The external track wins when attached (aelog replay); otherwise
        // the node's embedded clip plays.
        let samples: &[f32] = self.external_input.as_deref().unwrap_or(&embedded);
        let st = self.buffer_cursors.entry(id).or_insert(0);
        let mut plane = vec![0.0f32; self.block];
        for s in plane.iter_mut() {
            if *st >= samples.len() {
                if loop_clip && !samples.is_empty() {
                    *st = 0; // wrap the loop cursor
                } else {
                    continue; // one-shot: silence after the end
                }
            }
            *s = samples[*st];
            *st += 1;
        }
        self.broadcast(id, PortId::OUT, &plane);
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
        // frames at/after it use the stepped value.
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
            None => {
                for (i, o) in out.iter_mut().enumerate() {
                    *o = in_plane[i] * base_gain;
                }
            }
        }
        self.broadcast(id, PortId::OUT, &out);
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
        let position = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::Acoustic { position }) => position,
            _ => return,
        };
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        // Direct path passes through (scaled by the baked direct gain).
        let mut out = in_plane.clone();
        let Some(obj) = self.baked.as_ref().and_then(|s| s.get(position)) else {
            self.broadcast(id, PortId::OUT, &out);
            return;
        };
        let direct_gain = obj.direct().map(|d| d.gain).unwrap_or(1.0);
        for s in out.iter_mut() {
            *s *= direct_gain;
        }
        // Non-direct paths: one delayed, gain-scaled copy each (excess
        // delay relative to direct — the renderer's own convention).
        let taps = acoustic_taps(obj);
        let max_excess = taps.iter().map(|(ex, _)| *ex).max().unwrap_or(0);
        if max_excess == 0 {
            self.broadcast(id, PortId::OUT, &out);
            return;
        }
        let len = (max_excess + 1) as usize;
        let st = self.acoustics.entry(id).or_default();
        if st.buf.len() < len {
            st.buf.resize(len, 0.0);
        }
        for i in 0..self.block {
            st.buf[st.pos] = in_plane[i];
            for &(excess, gain) in &taps {
                let idx = (st.pos as i64 - excess).rem_euclid(st.buf.len() as i64) as usize;
                out[i] += gain * st.buf[idx];
            }
            st.pos = (st.pos + 1) % st.buf.len();
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

/// The renderable taps of a baked response: `(excess_delay, gain)` per
/// non-direct path, using the renderers' excess-delay convention (the
/// `listener_images` arithmetic). The direct path is the base pass-through.
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
        ex.set_external_input(Some(vec![9.0, 8.0, 7.0, 6.0]));
        ex.process_blocks(2).unwrap();
        let cap = ex.capture(sink).unwrap();
        assert_eq!(
            cap,
            [9.0, 8.0, 7.0, 6.0, 0.0, 0.0],
            "one-shot external track"
        );
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
