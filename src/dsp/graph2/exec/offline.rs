//! The offline executor's node ops (v3.50 — moved from `exec.rs`).
//!
//! One `run_*` method per [`NodeKind`], dispatched from
//! [`OfflineExecutor::process_block`]. The math each op runs is the shared
//! kernels in [`super::ops`] (gain, mix, delay, source, convolution,
//! IR-taps), so the realtime executor (`crate::dsp::graph2::rt`) renders
//! Through the exact same arithmetic — the sharing contract.
//! This impl block is the **offline** form: planes are freshly allocated
//! `Vec`s (the offline contract allows growth); the realtime executor
//! preallocates its planes and calls the same kernels.

use super::super::edge::EdgeId;
use super::super::node::{HrtfSource, NodeId, NodeParams, PortId};
use super::ops::{direct_convolve, to_ir_taps, GainStep, CONVOLUTION_FFT_THRESHOLD};
use super::{
    ConvState, FftConvState, HrtfState, OfflineExecutor, ResamplerState, ACOUSTIC_HISTORY,
};
use crate::spatial::acoustic::bake::{spectral_taps, BakedScene, ACOUSTIC_IR_LEN};
use crate::spatial::hrtf::Ear;

impl OfflineExecutor {
    pub(crate) fn run_source(&mut self, id: NodeId) {
        let params = self.nodes.get(&id).map(|n| n.params.clone());
        let Some(NodeParams::Source(p)) = params else {
            return;
        };
        let st = self.sources.entry(id).or_default();
        let mut plane = vec![0.0f32; self.block];
        // The shared signal kernel (contract).
        super::ops::kernel_source(
            &mut plane,
            &p,
            self.sample_rate,
            &mut st.phase,
            &mut st.fired,
        );
        self.broadcast(id, PortId::OUT, &plane);
    }
    pub(crate) fn run_buffer(&mut self, id: NodeId) {
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

    pub(crate) fn run_sink(&mut self, id: NodeId) {
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

    pub(crate) fn run_gain(&mut self, id: NodeId) {
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
        // The shared kernel decides step/automation/static application and
        // Returns the persisted base gain ( sharing contract: the
        // realtime executor calls the same function).
        let stepped = step.is_some();
        let next = super::ops::kernel_gain(
            &in_plane,
            &mut out,
            base_gain,
            step.map(|(gain, local)| GainStep { gain, local }),
            self.automation_gain(id),
        );
        if stepped {
            // Persist the new gain so subsequent blocks keep it.
            if let Some(n) = self.nodes.get_mut(&id) {
                n.params = NodeParams::Gain { gain: next };
            }
        }
        self.broadcast(id, PortId::OUT, &out);
    }

    /// The `(start, end)` gain for `id`'s block under tempo-mapped
    /// automation, if a curve is registered for the node **and** a tempo
    /// map is attached. `start` is the curve at the block's first sample,
    /// `end` at the block's last sample.
    pub(crate) fn automation_gain(&self, id: NodeId) -> Option<(f32, f32)> {
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

    pub(crate) fn run_delay(&mut self, id: NodeId) {
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
        // The shared read-before-write ring kernel (contract).
        super::ops::kernel_delay(&in_plane, &mut out, &mut st.buf, &mut st.pos);
        self.broadcast(id, PortId::OUT, &out);
    }

    pub(crate) fn run_mix(&mut self, id: NodeId) {
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

    pub(crate) fn run_split(&mut self, id: NodeId) {
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        let outs = self.nodes.get(&id).map(|n| n.outputs.len()).unwrap_or(0);
        for i in 0..outs {
            self.broadcast(id, PortId(i as u32), &in_plane);
        }
    }

    pub(crate) fn run_acoustic(&mut self, id: NodeId) {
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
            // v3.48: thread the scene's air-absorption model so reflections
            // darken with travel distance when the scene opts in (otherwise
            // the free-form disabled default, bit-identical to before).
            st.paths = match scene_ref {
                Some(sc) => sc.spectral_taps(obj, ACOUSTIC_IR_LEN),
                None => spectral_taps(obj, ACOUSTIC_IR_LEN),
            };
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

    pub(crate) fn run_convolution(&mut self, id: NodeId) {
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
        // Long IRs go through the realtime partitioned-FFT engine (fast for
        // large kernels); short kernels keep the exact direct path. If the
        // engine can't be built we fall through to direct, never dropping a
        // render.
        if kernel.len() >= CONVOLUTION_FFT_THRESHOLD {
            if let Some(out) = self.convolve_partitioned(id, &kernel, &in_plane) {
                self.broadcast(id, PortId::OUT, &out);
                return;
            }
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

    /// Run one `Convolution` block through the partitioned-FFT engine.
    /// `None` when the engine state isn't (or couldn't be) available, so the
    /// caller can fall back to the exact direct path.
    pub(crate) fn convolve_partitioned(
        &mut self,
        id: NodeId,
        kernel: &[f32],
        in_plane: &[f32],
    ) -> Option<Vec<f32>> {
        if !self.fft_convolutions.contains_key(&id) {
            let st = FftConvState::build(self.sample_rate, kernel)?;
            self.fft_convolutions.insert(id, st);
        }
        let st = self.fft_convolutions.get_mut(&id)?;
        Some(st.render_block(in_plane))
    }

    pub(crate) fn run_hrtf(&mut self, id: NodeId) {
        let (left, right, source) = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::HRTF {
                left,
                right,
                source,
            }) => (left, right, source),
            _ => return,
        };
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        // Resolve the per-ear IRs the node actually renders with. A `Dataset`
        // source reads the **real measured** head-related impulse responses
        // from the executor's HrtfDataset — bilinearly interpolated at the
        // node's azimuth/elevation, padded/truncated to the reported `taps`
        // so `node_latency` and the rendered timing agree exactly.
        let (left, right) = match source {
            HrtfSource::Inline => (left, right),
            HrtfSource::Dataset {
                azimuth_deg,
                elevation_deg,
                taps,
            } => match &self.hrtf_dataset {
                Some(ds) => {
                    let mut ml = vec![0.0f32; ds.taps()];
                    let mut mr = vec![0.0f32; ds.taps()];
                    ds.bilinear_interpolate(azimuth_deg, elevation_deg, Ear::Left, &mut ml);
                    ds.bilinear_interpolate(azimuth_deg, elevation_deg, Ear::Right, &mut mr);
                    (to_ir_taps(ml, taps as usize), to_ir_taps(mr, taps as usize))
                }
                // No dataset attached: fall back to the inline tabs (usually
                // empty → passthrough), mirroring an unbaked Acoustic node.
                None => (left, right),
            },
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

    pub(crate) fn run_resampler(&mut self, id: NodeId) {
        let (ratio, quality) = match self.nodes.get(&id).map(|n| n.params.clone()) {
            Some(NodeParams::Resampler { ratio, quality }) => {
                (ratio.max(1.0), (quality.max(1)) as usize)
            }
            _ => return,
        };
        let in_plane = match self.read_input(id, PortId::IN) {
            Some(p) => p.to_vec(),
            None => vec![0.0; self.block],
        };
        let st = self
            .resamplers
            .entry(id)
            .or_insert_with(|| ResamplerState::new(ratio, quality));
        let out = st.render_block(&in_plane);
        self.broadcast(id, PortId::OUT, &out);
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    /// The plane of the single edge feeding `node`'s input port, if any.
    pub(crate) fn read_input(&self, node: NodeId, port: PortId) -> Option<&[f32]> {
        self.edges
            .values()
            .find(|e| e.target.node == node && e.target.port == port)
            .and_then(|e| self.edge_planes.get(&e.id))
            .map(|v| v.as_slice())
    }

    /// Write `plane` into every edge leaving `node`'s output port.
    pub(crate) fn broadcast(&mut self, node: NodeId, port: PortId, plane: &[f32]) {
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

/// A `NodeKind::Prod` step inside the generic offline executor: the
/// production stage's DSP does not live in the graph2 kernels — it executes
/// through the shared production arena (`crate::dsp::graph2::prod::arena`)
/// in the `prod`
/// shell. Inside the generic executor the node contributes **structure
/// only**: it passes its input plane through so pure-topology analysis and
/// latency walks behave (the prod shell overrides this dispatch with the
/// real arena execution).
pub(crate) fn pass_through_prod(exec: &mut OfflineExecutor, node_id: NodeId) {
    let in_plane = match exec.read_input(node_id, PortId::IN) {
        Some(p) => p.to_vec(),
        None => return,
    };
    exec.broadcast(node_id, PortId::OUT, &in_plane);
}
