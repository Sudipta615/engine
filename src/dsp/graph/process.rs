//! Block signal processing via compiled execution plans: stereo f32 / f64 and
//! multichannel entry points.
//!
//! The stage order is NOT hardcoded here — it lives in the compiled
//! [`PlanSet`] (see [`plan`]). These entry points only handle block splitting,
//! precision promotion, the transport-bypass contracts, and plane
//! orchestration, then hand the planes to the plan runner.

use super::plan::{PlanId, StepScope};
use super::*;

impl DspGraph {
    /// Execute a compiled plan over a planar block in f32. The plan borrows
    /// the active generation's `plans` (immutably) while nodes borrow its
    /// `nodes` (mutably) — disjoint fields, so the hot path stays lock-free
    /// and allocation-free.
    #[inline]
    fn run_plan(&mut self, id: PlanId, planes: &mut [&mut [f32]]) {
        let plan = self.active.plans.plan(id);
        for step in &plan.steps {
            let node = &mut self.active.nodes[step.node.0];
            match step.scope {
                StepScope::AllChannels => node.process_block_f32(planes),
                StepScope::FrontPair => {
                    let (l, rest) = planes.split_at_mut(1);
                    let (r, _) = rest.split_at_mut(1);
                    let mut pair = [&mut l[0][..], &mut r[0][..]];
                    node.process_block_f32(&mut pair);
                }
            }
        }
    }

    /// f64 variant of [`Self::run_plan`] (Quality mode).
    #[inline]
    fn run_plan_f64(&mut self, id: PlanId, planes: &mut [&mut [f64]]) {
        let plan = self.active.plans.plan(id);
        for step in &plan.steps {
            let node = &mut self.active.nodes[step.node.0];
            match step.scope {
                StepScope::AllChannels => node.process_block_f64(planes),
                StepScope::FrontPair => {
                    let (l, rest) = planes.split_at_mut(1);
                    let (r, _) = rest.split_at_mut(1);
                    let mut pair = [&mut l[0][..], &mut r[0][..]];
                    node.process_block_f64(&mut pair);
                }
            }
        }
    }

    // ── Signal Processing Entry Points ─────────────────────────────────────

    /// Process a block of stereo frames in-place, dispatching on precision
    /// mode once per block. Semantics are identical to the pre-plan chain:
    /// pre-mix (preamp + loudness), then post-mix
    /// (eq → dynamics → convolution → balance → crossfeed → stereo →
    /// timestretch → volume → seek fade).
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        // Phase 2: apply queued control commands and any pending generation
        // swap once per CALLER block, before any splitting or bypass checks
        // (bypass governs signal processing, not control application).
        self.control_tick();
        self.process_block_inner(left, right);
    }

    /// Process a stereo block with a second mix-bus input (Phase 3 S1).
    /// `input0` is the primary (outgoing) stream, processed in place through
    /// the full chain; `input1` is the secondary (incoming) stream, summed
    /// by the mix bus under its transition envelope. Bit-exact against the
    /// pipeline's crossfade path (see `tests/fidelity/
    /// graph_pipeline_equivalence.rs`).
    ///
    /// The secondary stream may be shorter than the primary; the missing
    /// tail is treated as silence. Transport bypass (bit-perfect / DoP)
    /// returns before any stage, exactly like [`Self::process_block`].
    pub fn process_block_inputs(
        &mut self,
        input0: (&mut [f32], &mut [f32]),
        input1: (&mut [f32], &mut [f32]),
    ) {
        let mut secondaries = [input1];
        self.process_block_streams(input0, &mut secondaries);
    }

    /// Process a stereo block with one primary stream and any number of
    /// lane streams (Phase 4 S6). Identical to [`Self::process_block_streams`]
    /// except lane `k` feeds mix-bus slot `k + 2`, leaving slot 1 (the pair's
    /// incoming member) untouched — the lane feed for the single-stream path,
    /// where no crossfade is in progress. `process_block_inputs` still covers
    /// the crossfade path (lanes ride after the incoming stream there).
    pub fn process_block_lanes(
        &mut self,
        primary: (&mut [f32], &mut [f32]),
        lanes: &mut [(&mut [f32], &mut [f32])],
    ) {
        self.control_tick();
        let lane_count = lanes
            .len()
            .min(crate::dsp::graph::nodes::MAX_MIX_SLOTS.saturating_sub(2));
        let lanes = &mut lanes[..lane_count];
        let n = primary.0.len().min(primary.1.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                for (k, lane) in lanes.iter_mut().enumerate() {
                    self.feed_secondary_slot(k + 2, (lane.0, lane.1), start, end);
                }
                self.process_block_inner(&mut primary.0[start..end], &mut primary.1[start..end]);
                start = end;
            }
            return;
        }
        for (k, lane) in lanes.iter_mut().enumerate() {
            self.feed_secondary_slot(k + 2, (lane.0, lane.1), 0, n);
        }
        self.process_block_inner(primary.0, primary.1);
    }

    /// Process a stereo block during a crossfade with active lanes (Phase 4
    /// S6): the incoming stream feeds slot 1, lane `k` feeds slot `k + 2`
    /// (the slot-addressed lane placement). Kept as a dedicated entry so the
    /// engine never has to assemble the incoming + lanes into one contiguous
    /// array on the hot path (the old MAX_LANES+1 assembly panicked on the
    /// MAX_LANES-element scratch).
    pub fn process_block_crossfade_with_lanes(
        &mut self,
        primary: (&mut [f32], &mut [f32]),
        incoming: (&mut [f32], &mut [f32]),
        lanes: &mut [(&mut [f32], &mut [f32])],
    ) {
        self.control_tick();
        let n = primary.0.len().min(primary.1.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.feed_secondary_slot(1, (incoming.0, incoming.1), start, end);
                for (k, lane) in lanes.iter_mut().enumerate() {
                    self.feed_secondary_slot(k + 2, (lane.0, lane.1), start, end);
                }
                self.process_block_inner(&mut primary.0[start..end], &mut primary.1[start..end]);
                start = end;
            }
            return;
        }
        self.feed_secondary_slot(1, (incoming.0, incoming.1), 0, n);
        for (k, lane) in lanes.iter_mut().enumerate() {
            self.feed_secondary_slot(k + 2, (lane.0, lane.1), 0, n);
        }
        self.process_block_inner(primary.0, primary.1);
    }

    /// Process a stereo block with one primary stream and any number of
    /// secondary mix-bus streams (Phase 3 S2 stream slots). `primary` is
    /// processed in place through the full chain; secondary `k` feeds mix-bus
    /// slot `k + 1` (slots ≥ 2 are independent streams summed after the
    /// transition envelope). The secondary streams may be shorter than the
    /// primary; the missing tail is treated as silence. Transport bypass
    /// returns before any stage, exactly like [`Self::process_block`].
    pub fn process_block_streams(
        &mut self,
        primary: (&mut [f32], &mut [f32]),
        secondaries: &mut [(&mut [f32], &mut [f32])],
    ) {
        self.control_tick();
        let secondary_count = secondaries
            .len()
            .min(crate::dsp::graph::nodes::MAX_MIX_SLOTS - 1);
        let secondaries = &mut secondaries[..secondary_count];
        let n = primary.0.len().min(primary.1.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                for (k, sec) in secondaries.iter_mut().enumerate() {
                    self.feed_secondary_slot(k + 1, (sec.0, sec.1), start, end);
                }
                self.process_block_inner(&mut primary.0[start..end], &mut primary.1[start..end]);
                start = end;
            }
            return;
        }
        for (k, sec) in secondaries.iter_mut().enumerate() {
            self.feed_secondary_slot(k + 1, (sec.0, sec.1), 0, n);
        }
        self.process_block_inner(primary.0, primary.1);
    }

    /// Copy a chunk of a secondary stream into the mix bus's `slot` planes
    /// (audio-side, no allocation — the planes are preallocated). Fills the
    /// slot's channel-major planes from a stereo source (front L/R), marking
    /// the slot 2-channel. Use [`Self::feed_secondary_slot_mc`] for N-channel.
    fn feed_secondary_slot(
        &mut self,
        slot: usize,
        input: (&[f32], &[f32]),
        start: usize,
        end: usize,
    ) {
        let k = end - start;
        let mix = self.mix_mut();
        if mix.inputs.len() <= slot {
            return;
        }
        mix.inputs[slot].planes[0]
            .get_mut(..k)
            .expect("input plane capacity >= MAX_AUDIO_BLOCK_FRAMES")
            .copy_from_slice(&input.0[start..end]);
        mix.inputs[slot].planes[1]
            .get_mut(..k)
            .expect("input plane capacity >= MAX_AUDIO_BLOCK_FRAMES")
            .copy_from_slice(&input.1[start..end]);
        mix.inputs[slot].channels = 2;
    }

    /// Feed a secondary slot from an N-channel interleaved source (Phase 4
    /// S2). `frames * channels` samples are de-interleaved channel-major into
    /// the slot's preallocated planes and the slot's channel count is set, so
    /// the channel-wise MC sum includes all of them. Audio-side, no
    /// allocation.
    fn feed_secondary_slot_mc(
        &mut self,
        slot: usize,
        interleaved: &[f32],
        channels: usize,
        start: usize,
        end: usize,
    ) {
        let k = end - start;
        let mix = self.mix_mut();
        if mix.inputs.len() <= slot {
            return;
        }
        let ch = channels.min(mix.inputs[slot].planes.len());
        mix.inputs[slot].channels = ch;
        let available = interleaved.len() / channels;
        for (plane_idx, plane) in mix.inputs[slot].planes.iter_mut().take(ch).enumerate() {
            let got = plane.len().min(k).min(available);
            for dst in 0..got {
                plane[dst] = interleaved[(start + dst) * channels + plane_idx.min(channels - 1)];
            }
        }
    }

    /// Process an interleaved `channels > 2`-channel multichannel block with
    /// a primary stream and any number of N-channel secondary mix-bus streams
    /// (Phase 4 S2). The primary is processed in place through the full MC
    /// plan; each secondary is fed channel-major into its slot and summed
    /// channel-wise at the mix step. `secondaries` is `(interleaved,
    /// channels)` per slot — a secondary may carry fewer channels than the
    /// primary (surround slots feeding a 5.1 bus). Missing tails are silence.
    /// Transport bypass returns before any stage. Control tick runs once per
    /// caller block.
    pub fn process_block_multichannel_streams(
        &mut self,
        primary: &mut [f32],
        primary_channels: usize,
        secondaries: &mut [(&mut [f32], usize)],
    ) {
        self.control_tick();
        let n = primary.len() / primary_channels.max(1);
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                for (k, &mut (ref sec, sec_ch)) in secondaries.iter_mut().enumerate() {
                    self.feed_secondary_slot_mc(k + 1, sec, sec_ch, start, end);
                }
                self.process_block_multichannel_inner(
                    &mut primary[start * primary_channels..end * primary_channels],
                    primary_channels,
                );
                start = end;
            }
            return;
        }
        for (k, &mut (ref sec, sec_ch)) in secondaries.iter_mut().enumerate() {
            self.feed_secondary_slot_mc(k + 1, sec, sec_ch, 0, n);
        }
        self.process_block_multichannel_inner(primary, primary_channels);
    }

    /// Unticked inner path — shared by the public entry and the ≤2-channel
    /// multichannel delegation so the control tick runs exactly once per
    /// caller block.
    fn process_block_inner(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block_inner(&mut left[start..end], &mut right[start..end]);
                start = end;
            }
            return;
        }
        if n == 0 || self.dop_bypass || self.bit_perfect {
            // DoP bitstream and bit-perfect transport are hard bypass
            // contracts: no stage (not even software volume) touches the
            // samples.
            return;
        }

        match self.precision_mode {
            PrecisionMode::Performance => {
                let mut planes = [left as &mut [f32], right as &mut [f32]];
                self.run_plan(PlanId::Normal, &mut planes);
            }
            PrecisionMode::Quality => {
                // Promote the block to f64, run the f64 chain, demote back.
                // The scratch Vecs are moved out of `self` (O(1)) so the plan
                // runner can borrow `self` mutably without aliasing them; the
                // allocation is retained when they are put back.
                let mut l64 = std::mem::take(&mut self.scratch.scratch_f64_l);
                let mut r64 = std::mem::take(&mut self.scratch.scratch_f64_r);
                for i in 0..n {
                    l64[i] = left[i] as f64;
                    r64[i] = right[i] as f64;
                }
                {
                    let mut planes = [&mut l64[..n] as &mut [f64], &mut r64[..n] as &mut [f64]];
                    self.run_plan_f64(PlanId::Normal, &mut planes);
                }
                for i in 0..n {
                    left[i] = l64[i] as f32;
                    right[i] = r64[i] as f32;
                }
                self.scratch.scratch_f64_l = l64;
                self.scratch.scratch_f64_r = r64;
            }
        }
        self.publish_mix_meters();
    }

    /// Copy the mix bus's per-slot meters (computed during the plan run) onto
    /// the control bus atomics so telemetry can read them from any thread
    /// (Phase 4 S3). Audio-side, allocation-free, relaxed stores.
    fn publish_mix_meters(&mut self) {
        let inputs = &self.active.nodes[node_id::MIX];
        if let GraphNode::Mix(mix) = inputs {
            for (i, input) in mix.inputs.iter().enumerate() {
                self.bus
                    .publish_slot_meters(i, input.meters.peak_db, input.meters.rms_db);
            }
            // Phase 5 S3: publish the aux bus meters (post-return, once per
            // block) so telemetry can read the aux level from any thread.
            self.bus
                .publish_aux_meters(mix.aux.meters.peak_db, mix.aux.meters.rms_db);
        }
    }

    /// Process a block of stereo frames in f64 precision in place.
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        self.control_tick();
        self.process_block_f64_inner(left, right);
    }

    fn process_block_f64_inner(&mut self, left: &mut [f64], right: &mut [f64]) {
        let n = left.len().min(right.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block_f64_inner(&mut left[start..end], &mut right[start..end]);
                start = end;
            }
            return;
        }
        if n == 0 || self.dop_bypass || self.bit_perfect {
            return;
        }
        let mut planes = [left as &mut [f64], right as &mut [f64]];
        self.run_plan_f64(PlanId::Normal, &mut planes);
        // Publish the per-slot / aux meters after the plan run, mirroring the
        // f32 entry point (`process_block_inner`) so the public f64 path
        // reports live metering too.
        self.publish_mix_meters();
    }

    /// Process an interleaved block of `channels`-channel frames in place.
    ///
    /// - `channels <= 2`: identical to [`Self::process_block`] on the front
    ///   L/R pair (a mono source is duplicated to both channels), preserving
    ///   the stereo path's Quality-mode f64 promotion.
    /// - `channels > 2`: the multichannel plan runs `routing` (channel trim)
    ///   on every channel, then the stereo filter stages on the front L/R
    ///   pair only, then volume and seek-fade on every channel. Runs in f32
    ///   regardless of `PrecisionMode`, mirroring the pipeline's documented
    ///   f32 boundary for the >2-channel path.
    pub fn process_block_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        if channels == 0 || channels > MAX_CHANNELS {
            return;
        }
        self.control_tick();
        self.process_block_multichannel_inner(interleaved, channels);
    }

    fn process_block_multichannel_inner(&mut self, interleaved: &mut [f32], channels: usize) {
        let n = interleaved.len() / channels;
        if n == 0 {
            return;
        }
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block_multichannel_inner(
                    &mut interleaved[start * channels..end * channels],
                    channels,
                );
                start = end;
            }
            return;
        }
        if self.dop_bypass || self.bit_perfect {
            // Bit-perfect and DoP transport bypass every sample transform,
            // including multichannel trim and software volume.
            return;
        }

        // Take the reusable de-interleave planes out of `self` so the plan
        // runner can borrow `self` mutably without aliasing the scratch.
        let mut planes = std::mem::take(&mut self.scratch.scratch_mc);

        if channels <= 2 {
            for (i, chunk) in interleaved[..n * channels]
                .chunks_exact(channels)
                .enumerate()
            {
                planes[0][i] = chunk[0];
                planes[1][i] = if channels == 2 { chunk[1] } else { chunk[0] };
            }
            {
                let (front, rest) = planes.split_at_mut(1);
                self.process_block_inner(&mut front[0][..n], &mut rest[0][..n]);
            }
            for (i, chunk) in interleaved[..n * channels]
                .chunks_exact_mut(channels)
                .enumerate()
            {
                chunk[0] = planes[0][i];
                if channels == 2 {
                    chunk[1] = planes[1][i];
                }
            }
            self.scratch.scratch_mc = planes;
            return;
        }

        // De-interleave: channel `ch` sits at indices
        // `ch, ch + channels, ch + 2*channels, …` in the interleaved block.
        for (ch, plane) in planes.iter_mut().enumerate().take(channels) {
            for (i, s) in interleaved
                .iter()
                .skip(ch)
                .step_by(channels)
                .take(n)
                .enumerate()
            {
                plane[i] = *s;
            }
        }

        // Build stack-allocated plane views (no heap traffic on the hot
        // path) and run the multichannel plan over the channel subset.
        // `planes` is sized `MAX_CHANNELS` by construction (GraphScratch),
        // so the sequential reborrows below cannot fail, and each plane is
        // `MAX_AUDIO_BLOCK_FRAMES` long — at least `n`, which the block
        // splitting above guarantees. Views are truncated to `n` frames so
        // stateful stages advance their state over exactly the frames of
        // this block (the same `[..n]` discipline the stereo path and the
        // pipeline's `process_planes` use), never over the full scratch
        // length.
        debug_assert!(planes.len() >= MAX_CHANNELS);
        debug_assert!(n <= MAX_AUDIO_BLOCK_FRAMES);
        let mut iter = planes.iter_mut();
        let mut plane_views: [&mut [f32]; MAX_CHANNELS] = [
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
            &mut iter.next().unwrap()[..n],
        ];
        self.run_plan(PlanId::NormalMc, &mut plane_views[..channels]);

        // Re-interleave
        for ch in 0..channels {
            for i in 0..n {
                interleaved[i * channels + ch] = planes[ch][i];
            }
        }
        self.scratch.scratch_mc = planes;
        self.publish_mix_meters();
    }
}
