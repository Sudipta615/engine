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

use std::collections::{BTreeMap, HashMap};

use super::edge::{EdgeDef, EdgeId};
use super::node::{NodeDef, NodeId, NodeKind, NodeParams, PortId, TestSignal};
use super::sort::ExecutionOrder;
use super::validate::Graph2Error;
use super::Graph2;

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
}
