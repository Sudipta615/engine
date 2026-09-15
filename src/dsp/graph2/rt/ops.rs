//! Realtime executor ops (v3.50): the zero-allocation per-node
//! renders and the plan publish / adopt / retire machinery.
//!
//! Every op here mirrors its offline counterpart
//! (`super::super::exec::offline`) exactly — same kernels, same overlap
//! discipline, same cursor semantics — writing into the plan's
//! preallocated planes and scratch instead of allocating. The bit-exact
//! equivalence suite pins the two together.
//!
//! ## Borrow discipline
//!
//! Like the production graph's `run_plan`, each op destructures the plan
//! into **disjoint field borrows** (`let RtPlan { planes, inputs, … } =
//! plan;`), so an op can read its input plane, mutate its node state and
//! write its output planes in one pass without fighting the borrow
//! checker or reaching for raw pointers. `render_block` copies only the
//! `NodeKind` (a `Copy` enum) out of the plan for dispatch — node params
//! are never cloned on the audio path.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use super::super::exec::ops::{kernel_delay, kernel_gain, kernel_source};
use super::super::node::{NodeKind, PortId};
use super::RtPlan;

/// Find the plane index feeding `idx`'s input port, if wired.
fn input_index(inputs: &[Vec<(u32, usize)>], idx: usize, port: u32) -> Option<usize> {
    inputs
        .get(idx)?
        .iter()
        .find(|(p, _)| *p == port)
        .map(|(_, plane)| *plane)
}

/// Read input plane if valid for the current epoch; otherwise fall back to silence.
#[inline]
fn read_input_plane<'a>(
    inputs: &[Vec<(u32, usize)>],
    idx: usize,
    port: u32,
    planes: &'a [Vec<f32>],
    plane_epochs: &[u64],
    current_epoch: u64,
    zero_plane: &'a [f32],
) -> &'a [f32] {
    match input_index(inputs, idx, port) {
        Some(i) => {
            if plane_epochs.get(i).copied() == Some(current_epoch) {
                &planes[i]
            } else {
                zero_plane
            }
        }
        None => zero_plane,
    }
}

/// Realtime executor: owns the active [`RtPlan`] and renders blocks.
///
/// The control thread builds a plan (`RtPlan::build`), publishes it
/// (`publish`), and the audio thread adopts it at the next block boundary
/// (inside `render_block`). At most one swap is in flight; a publish while
/// Another is pending coalesces (latest wins) — the publish /
/// swap / retire discipline, reused.
pub struct RtExecutor {
    /// The live plan. Replaced only by adopting a published plan at a
    /// block boundary; the retired plan is handed to the control thread
    /// for reclamation (the audio thread never drops a plan).
    active: Box<RtPlan>,
    /// Set by the first sink rendered in a block so a second sink sums
    /// instead of overwriting `out`. Reset every `render_block`.
    sink_already_written: bool,
    /// A published-but-not-yet-adopted plan (control → audio).
    pending: AtomicPtr<RtPlan>,
    /// A plan the audio thread retired (audio → control); reclaimed by
    /// the control thread's next `publish` or an explicit
    /// `reclaim_retired`.
    retired: AtomicPtr<RtPlan>,
    /// Monotonic count of adopted plans.
    swaps: AtomicU64,
}

impl RtExecutor {
    /// Build an executor owning `plan` as the initial active plan.
    pub fn new(plan: RtPlan) -> Self {
        Self {
            active: Box::new(plan),
            sink_already_written: false,
            pending: AtomicPtr::new(std::ptr::null_mut()),
            retired: AtomicPtr::new(std::ptr::null_mut()),
            swaps: AtomicU64::new(0),
        }
    }

    /// Control thread: publish a fresh plan for the audio thread to adopt
    /// at the next block boundary. Reclaims any previously-retired plan
    /// first, then coalesces an earlier pending publish (latest wins).
    /// The audio thread never allocates or frees — reclamation is
    /// control-side.
    pub fn publish(&self, plan: RtPlan) {
        self.reclaim_retired();
        let prev = self
            .pending
            .swap(Box::into_raw(Box::new(plan)), Ordering::AcqRel);
        if !prev.is_null() {
            // Reclaim the coalesced, never-adopted plan.
            unsafe {
                drop(Box::from_raw(prev));
            }
        }
    }

    /// Control thread: reclaim a plan the audio thread retired. Called
    /// implicitly by [`Self::publish`]; exposed for tests asserting the
    /// reclamation discipline.
    pub fn reclaim_retired(&self) {
        let r = self.retired.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !r.is_null() {
            unsafe {
                drop(Box::from_raw(r));
            }
        }
    }

    /// Number of plans adopted by the audio thread (monotonic).
    pub fn generation(&self) -> u64 {
        self.swaps.load(Ordering::Acquire)
    }

    /// Extract the active plan (control side).
    pub fn into_plan(self) -> RtPlan {
        *self.active
    }

    /// Audio thread: adopt a published plan, if any. The retired plan is
    /// handed to the control thread for reclamation; no allocation
    /// happens here. Returns `true` when a swap occurred.
    fn adopt_pending(&mut self) -> bool {
        let p = self.pending.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if p.is_null() {
            return false;
        }
        let new_plan = unsafe { Box::from_raw(p) };
        let prev = std::mem::replace(&mut self.active, new_plan);
        self.retired.store(Box::into_raw(prev), Ordering::Release);
        self.swaps.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Audio thread: render one block. Every Sink node's input planes are
    /// **summed into `out`** (the realtime reduction of the offline
    /// capture: a realtime render has no unbounded capture buffer;
    /// single-input sinks — the canonical topology — match the offline
    /// capture exactly). `out.len()` must equal the plan's block size.
    /// Performs zero heap allocation.
    ///
    /// A published plan is adopted first (the block-boundary swap), then
    /// the compiled steps run in order, enum-dispatched per node kind.
    pub fn render_block(&mut self, out: &mut [f32]) {
        let _ = self.adopt_pending();
        let block = self.active.block;
        debug_assert_eq!(
            out.len(),
            block,
            "render_block: out length must equal the plan's block size"
        );
        // The caller's output starts at silence every block: sinks *sum*
        // into it (`+=`), so a reused buffer must not carry the previous
        // block's samples.
        out.fill(0.0);
        self.sink_already_written = false;

        // Advance plan epoch: planes are overwritten on write or read as zero.
        // Unnecessary full-plane clearing across every edge is eliminated.
        self.active.current_epoch = self.active.current_epoch.wrapping_add(1);
        if self.active.current_epoch == 0 {
            for plane in self.active.planes.iter_mut() {
                plane.fill(0.0);
            }
            self.active.plane_epochs.fill(0);
            self.active.current_epoch = 1;
        }

        // Render every step in compiled order, enum-dispatched. Only the
        // NodeKind (Copy) is read out of the plan here — params are read
        // inside each op from the plan's own state tables.
        for step in 0..self.active.steps.len() {
            let node = self.active.steps[step];
            let kind = match self.active.nodes[node.0 as usize] {
                Some((k, _)) => k,
                None => continue,
            };
            let idx = node.0 as usize;
            match kind {
                NodeKind::Source => self.rt_source(idx),
                NodeKind::Sink => self.rt_sink(idx, out),
                NodeKind::Gain => self.rt_gain(idx),
                NodeKind::Delay => self.rt_delay(idx),
                NodeKind::Mix => self.rt_mix(idx),
                NodeKind::Split => self.rt_split(idx),
                NodeKind::Buffer => self.rt_buffer(idx),
                NodeKind::Convolution => self.rt_convolution(idx),
                NodeKind::HRTF => self.rt_hrtf(idx),
                NodeKind::Resampler => self.rt_resampler(idx),
                NodeKind::Acoustic => self.rt_acoustic(idx),
                // Production stages execute through the shared arena in the
                // `prod` shell; the generic RT plan never carries them (the
                // prod lowering routes them to the production executor).
                NodeKind::Prod(_) => {}
            }
        }
    }

    // ── Node ops (zero-alloc; disjoint-field borrows per op) ──────────────

    fn rt_source(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            nodes,
            sources,
            inputs,
            outputs,
            planes,
            scratch_out,
            zero_plane,
            sample_rate,
            block,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let Some((kind, params)) = &nodes[idx] else {
            return;
        };
        if *kind != NodeKind::Source {
            return;
        }
        let source_params = match params {
            super::super::node::NodeParams::Source(sp) => sp,
            _ => return,
        };
        let Some(st) = &mut sources[idx] else { return };
        let mut out = std::mem::take(scratch_out);
        kernel_source(
            &mut out,
            source_params,
            *sample_rate,
            &mut st.phase,
            &mut st.fired,
        );
        // Broadcast: copy scratch_out into every OUT plane.
        for (port, plane_list) in outputs[idx].iter() {
            if *port != PortId::OUT.0 {
                continue;
            }
            for plane in plane_list {
                planes[*plane].copy_from_slice(&out);
                plane_epochs[*plane] = *current_epoch;
            }
        }
        *scratch_out = out;
        let _ = (inputs, zero_plane, block);
    }

    fn rt_sink(&mut self, idx: usize, out: &mut [f32]) {
        // Multiple sinks share `out`: the first sink to render this block
        // copies its input plane (bit-exact single-input semantics,
        // preserving `-0.0`); every later sink and plane sums onto it.
        // `out` starts zeroed each block by `render_block`, and sinks
        // render in compiled (deterministic) order.
        let plan = &*self.active;
        let mut first_plane_of_this_sink = true;
        for (_, plane) in &plan.inputs[idx] {
            if plan.plane_epochs.get(*plane).copied() != Some(plan.current_epoch) {
                continue;
            }
            if first_plane_of_this_sink && !self.sink_already_written {
                // First plane of the first sink: copy, exactly like the
                // offline capture stores the plane.
                out.copy_from_slice(&plan.planes[*plane]);
                self.sink_already_written = true;
                first_plane_of_this_sink = false;
            } else {
                for (o, &s) in out.iter_mut().zip(plan.planes[*plane].iter()) {
                    *o += s;
                }
            }
        }
    }

    fn rt_gain(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            nodes,
            inputs,
            outputs,
            planes,
            scratch_out,
            zero_plane,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let Some((kind, params)) = &nodes[idx] else {
            return;
        };
        if *kind != NodeKind::Gain {
            return;
        }
        let gain = match params {
            super::super::node::NodeParams::Gain { gain } => *gain,
            _ => return,
        };
        let src: &[f32] = read_input_plane(
            inputs,
            idx,
            PortId::IN.0,
            planes,
            plane_epochs,
            *current_epoch,
            zero_plane,
        );
        let mut out = std::mem::take(scratch_out);
        kernel_gain(src, &mut out, gain, None, None);
        for (port, plane_list) in outputs[idx].iter() {
            if *port != PortId::OUT.0 {
                continue;
            }
            for plane in plane_list {
                planes[*plane].copy_from_slice(&out);
                plane_epochs[*plane] = *current_epoch;
            }
        }
        *scratch_out = out;
    }

    fn rt_delay(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            inputs,
            outputs,
            planes,
            delays,
            scratch_out,
            scratch_in,
            zero_plane,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let Some(st) = &mut delays[idx] else { return };
        let mut out = std::mem::take(scratch_out);
        // Copy the input into scratch first: `src` must not borrow `planes`
        // while the broadcast below writes into it.
        let mut work = std::mem::take(scratch_in);
        let src: &[f32] = read_input_plane(
            inputs,
            idx,
            PortId::IN.0,
            planes,
            plane_epochs,
            *current_epoch,
            zero_plane,
        );
        work.copy_from_slice(src);
        kernel_delay(&work, &mut out, &mut st.buf, &mut st.pos);
        for (port, plane_list) in outputs[idx].iter() {
            if *port != PortId::OUT.0 {
                continue;
            }
            for plane in plane_list {
                planes[*plane].copy_from_slice(&out);
                plane_epochs[*plane] = *current_epoch;
            }
        }
        *scratch_out = out;
        *scratch_in = work;
    }

    fn rt_mix(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            inputs,
            outputs,
            planes,
            scratch_out,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let mut out = std::mem::take(scratch_out);
        for s in out.iter_mut() {
            *s = 0.0;
        }
        for (_, plane) in &inputs[idx] {
            if plane_epochs.get(*plane).copied() == Some(*current_epoch) {
                for (o, &s) in out.iter_mut().zip(planes[*plane].iter()) {
                    *o += s;
                }
            }
        }
        for (port, plane_list) in outputs[idx].iter() {
            if *port != PortId::OUT.0 {
                continue;
            }
            for plane in plane_list {
                planes[*plane].copy_from_slice(&out);
                plane_epochs[*plane] = *current_epoch;
            }
        }
        *scratch_out = out;
    }

    fn rt_split(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            inputs,
            outputs,
            planes,
            scratch_in,
            zero_plane,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let mut work = std::mem::take(scratch_in);
        {
            let src: &[f32] = read_input_plane(
                inputs,
                idx,
                PortId::IN.0,
                planes,
                plane_epochs,
                *current_epoch,
                zero_plane,
            );
            work.copy_from_slice(src);
        }
        for (_, plane_list) in outputs[idx].iter() {
            for plane in plane_list {
                planes[*plane].copy_from_slice(&work);
                plane_epochs[*plane] = *current_epoch;
            }
        }
        *scratch_in = work;
    }

    fn rt_buffer(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            buffers,
            outputs,
            planes,
            buffer_cursors,
            block,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let Some((clip, looping)) = &buffers[idx] else {
            return;
        };
        let max_len = clip.iter().map(|c| c.len()).max().unwrap_or(0);
        // One shared cursor across all channels (lockstep planes) — the
        // offline semantics, replicated exactly: per frame, wrap the loop
        // cursor or stop (one-shot); port i reads clip channel i (silence
        // when absent).
        let mut cursor = buffer_cursors[idx];
        #[allow(clippy::needless_range_loop)]
        for s in 0..*block {
            if cursor >= max_len {
                if *looping && max_len > 0 {
                    cursor = 0;
                } else {
                    for (_, plane_list) in outputs[idx].iter() {
                        for plane in plane_list {
                            planes[*plane][s..*block].fill(0.0);
                        }
                    }
                    break; // one-shot: the rest stays silent
                }
            }
            // Frame `s` of every output plane reads the clip at the shared
            // cursor (port i reads channel i; silence when absent).
            for (port, plane_list) in outputs[idx].iter() {
                for plane in plane_list {
                    let sample = clip
                        .get(*port as usize)
                        .and_then(|c| c.get(cursor))
                        .copied()
                        .unwrap_or(0.0);
                    planes[*plane][s] = sample;
                }
            }
            cursor += 1;
        }
        buffer_cursors[idx] = cursor;
        for (_, plane_list) in outputs[idx].iter() {
            for plane in plane_list {
                plane_epochs[*plane] = *current_epoch;
            }
        }
    }

    fn rt_convolution(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            inputs,
            outputs,
            planes,
            convolutions,
            scratch_conv,
            scratch_out,
            scratch_in,
            zero_plane,
            block,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let Some((kernel, st)) = convolutions[idx].as_mut() else {
            return;
        };
        let kernel_len = kernel.len();
        let mut src_copy = std::mem::take(scratch_in);
        {
            let src: &[f32] = read_input_plane(
                inputs,
                idx,
                PortId::IN.0,
                planes,
                plane_epochs,
                *current_epoch,
                zero_plane,
            );
            src_copy.copy_from_slice(src);
        }
        let src: &[f32] = &src_copy;
        if kernel_len == 0 {
            // Pass-through (an empty kernel broadcasts the input).
            for (_, plane_list) in outputs[idx].iter() {
                for plane in plane_list {
                    planes[*plane].copy_from_slice(src);
                    plane_epochs[*plane] = *current_epoch;
                }
            }
            *scratch_in = src_copy;
            return;
        }
        // Convolve into scratch_conv (block + kernel − 1 frames), then
        // overlap-add through the pipeline state into scratch_out.
        let mut y = std::mem::take(scratch_conv);
        super::super::exec::ops::direct_convolve_into(src, kernel, &mut y);
        let mut out = std::mem::take(scratch_out);
        st.push_and_emit_into(&mut y, kernel_len - 1, *block, &mut out);
        for (port, plane_list) in outputs[idx].iter() {
            if *port != PortId::OUT.0 {
                continue;
            }
            for plane in plane_list {
                planes[*plane].copy_from_slice(&out);
                plane_epochs[*plane] = *current_epoch;
            }
        }
        *scratch_conv = y;
        *scratch_out = out;
        *scratch_in = src_copy;
    }

    fn rt_hrtf(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            inputs,
            outputs,
            planes,
            hrtfs,
            scratch_conv,
            scratch_out,
            scratch_in,
            zero_plane,
            block,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let Some((left, right, st)) = hrtfs[idx].as_mut() else {
            return;
        };
        let delay = left.len().max(right.len());
        let mut src_copy = std::mem::take(scratch_in);
        {
            let src: &[f32] = read_input_plane(
                inputs,
                idx,
                PortId::IN.0,
                planes,
                plane_epochs,
                *current_epoch,
                zero_plane,
            );
            src_copy.copy_from_slice(src);
        }
        let src: &[f32] = &src_copy;
        if delay == 0 {
            // No filters: both ears pass through, mutually aligned.
            for (_, plane_list) in outputs[idx].iter() {
                for plane in plane_list {
                    planes[*plane].copy_from_slice(src);
                    plane_epochs[*plane] = *current_epoch;
                }
            }
            *scratch_in = src_copy;
            return;
        }
        // Per-ear: convolve into scratch_conv, overlap-add through the
        // per-ear pipeline state, copy to the ear's planes. An empty ear
        // IR passes the input through (no overlap), still `delay`-aligned
        // — the exact offline semantics.
        for (ear_ir, ear_state, ear_port) in [
            (&left[..], &mut st.left, 0u32),
            (&right[..], &mut st.right, 1u32),
        ] {
            let mut y = std::mem::take(scratch_conv);
            if ear_ir.is_empty() {
                // Pass-through ear: the "convolved" block is the input
                // itself, overlap 0 (mirrors the offline clone path).
                y.clear();
                y.extend_from_slice(src);
            } else {
                super::super::exec::ops::direct_convolve_into(src, ear_ir, &mut y);
            }
            let mut out = std::mem::take(scratch_out);
            ear_state.push_and_emit_into(&mut y, ear_ir.len().saturating_sub(1), *block, &mut out);
            for (port, plane_list) in outputs[idx].iter() {
                if *port != ear_port {
                    continue;
                }
                for plane in plane_list {
                    planes[*plane].copy_from_slice(&out);
                    plane_epochs[*plane] = *current_epoch;
                }
            }
            *scratch_conv = y;
            *scratch_out = out;
        }
        *scratch_in = src_copy;
    }

    fn rt_resampler(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            inputs,
            outputs,
            planes,
            resamplers,
            scratch_out,
            zero_plane,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let Some(st) = &mut resamplers[idx] else {
            return;
        };
        let src: &[f32] = read_input_plane(
            inputs,
            idx,
            PortId::IN.0,
            planes,
            plane_epochs,
            *current_epoch,
            zero_plane,
        );
        let mut out = std::mem::take(scratch_out);
        st.render_block_into(src, &mut out);
        for (port, plane_list) in outputs[idx].iter() {
            if *port != PortId::OUT.0 {
                continue;
            }
            for plane in plane_list {
                planes[*plane].copy_from_slice(&out);
                plane_epochs[*plane] = *current_epoch;
            }
        }
        *scratch_out = out;
    }

    fn rt_acoustic(&mut self, idx: usize) {
        let plan = &mut *self.active;
        let RtPlan {
            inputs,
            outputs,
            planes,
            acoustics,
            direct_gains,
            scratch_out,
            scratch_in,
            zero_plane,
            block,
            current_epoch,
            plane_epochs,
            ..
        } = plan;
        let mut src_copy = std::mem::take(scratch_in);
        {
            let src: &[f32] = read_input_plane(
                inputs,
                idx,
                PortId::IN.0,
                planes,
                plane_epochs,
                *current_epoch,
                zero_plane,
            );
            src_copy.copy_from_slice(src);
        }
        let src: &[f32] = &src_copy;
        // Unbaked / missing scene: pass-through (the offline fallback).
        let Some(st) = &mut acoustics[idx] else {
            for (_, plane_list) in outputs[idx].iter() {
                for plane in plane_list {
                    planes[*plane].copy_from_slice(src);
                    plane_epochs[*plane] = *current_epoch;
                }
            }
            *scratch_in = src_copy;
            return;
        };
        // Direct path scales the input; each path adds its spectrally
        // filtered delayed read off the shared raw ring — the offline
        // math, accumulated into scratch_out then broadcast once.
        let direct_gain = direct_gains[idx];
        let mut out = std::mem::take(scratch_out);
        let ring_len = st.raw.len();
        for i in 0..*block {
            st.raw[st.pos] = src[i];
            out[i] = src[i] * direct_gain;
            for (excess, kernel) in &st.paths {
                let base = st.pos as i64 - *excess;
                let mut acc = 0.0f32;
                for (j, &hj) in kernel.iter().enumerate() {
                    let ring_idx = (base - j as i64).rem_euclid(ring_len as i64) as usize;
                    acc += hj * st.raw[ring_idx];
                }
                out[i] += acc;
            }
            st.pos = (st.pos + 1) % ring_len;
        }
        for (port, plane_list) in outputs[idx].iter() {
            if *port != PortId::OUT.0 {
                continue;
            }
            for plane in plane_list {
                planes[*plane].copy_from_slice(&out);
                plane_epochs[*plane] = *current_epoch;
            }
        }
        *scratch_out = out;
        *scratch_in = src_copy;
    }
}
