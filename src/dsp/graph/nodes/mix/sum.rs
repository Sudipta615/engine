//! The bus's mixing sums: stereo (f32/f64) pair-envelope mixes, the
//! independent-lane tail (slots ≥ 2), and the multichannel input-0 pass.
//!
//! These methods are pure moves from the pre-split `mix_node.rs` — the
//! expressions are untouched so the equivalence suite's bit-exact contract
//! holds by construction. The envelope state machine is embedded in the
//! frame loops (a mid-block phase transition must be handled per frame);
//! its curve math lives in [`super::envelope`].

use super::{balance_gains, pan_gains, MixBusNode, MAX_MIX_SLOTS};
use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::dsp::crossfade::MixerState;
use crate::dsp::graph::node::DspNode;

impl MixBusNode {
    /// Stereo mix: per-frame envelope + user gains + balance + mute, summing
    /// input 0 (in the master planes) and input 1 (its own planes). The
    /// envelope state is hoisted into locals so the frame loop can advance
    /// it while the per-input borrows stay alive.
    pub(super) fn mix_stereo(&mut self, planes: &mut [&mut [f32]]) {
        let frames = planes[0].len();
        // Duck state first (borrows `self.duck`), before the inputs are split
        // out for the frame loop. Gains are 1.0 when ducking is disabled.
        let d0 = self.duck_gain_for(0);
        let d1 = self.duck_gain_for(1);
        self.duck_tick(frames);

        let (l, r) = planes.split_at_mut(1);
        let out_l = &mut l[0][..frames];
        let out_r = &mut r[0][..frames];

        let mut state = self.state;
        let mut pos = self.crossfade_pos;
        let duration = self.crossfade_duration_frames;
        let curve = self.curve;

        let (in0, rest) = self.inputs.split_at_mut(1);
        let in0 = &mut in0[0];
        let (in1, _) = rest.split_at_mut(1);
        let in1 = &mut in1[0];
        let (in1_l, in1_r) = (&in1.planes[0][..frames], &in1.planes[1][..frames]);
        let (g0, bal0, mute0) = (&mut in0.gain, in0.balance, in0.mute);
        let (g1, bal1, mute1) = (&mut in1.gain, in1.balance, in1.mute);
        // Front-pair gains: balance, then pan. At pan = 0 the pan pair is
        // exactly (1, 1), so these products are bit-identical to the
        // pre-pan `balance_gains` values.
        let (b0l, b0r) = balance_gains(bal0);
        let (b1l, b1r) = balance_gains(bal1);
        let (p0l, p0r) = pan_gains(in0.pan, in0.pan_law);
        let (p1l, p1r) = pan_gains(in1.pan, in1.pan_law);
        let (b0l, b0r) = (b0l * p0l, b0r * p0r);
        let (b1l, b1r) = (b1l * p1l, b1r * p1r);

        for i in 0..frames {
            let u0 = g0.process_sample(1.0) * d0;
            let u1 = g1.process_sample(1.0) * d1;
            match state {
                MixerState::PlayingCurrent => {
                    if !mute0 && u0 == 1.0 && b0l == 1.0 && b0r == 1.0 {
                        // Pure pre-mix passthrough — bit-exact identity.
                    } else if mute0 {
                        out_l[i] = 0.0;
                        out_r[i] = 0.0;
                    } else {
                        out_l[i] *= u0 * b0l;
                        out_r[i] *= u0 * b0r;
                    }
                }
                MixerState::PlayingNext => {
                    if mute1 {
                        out_l[i] = 0.0;
                        out_r[i] = 0.0;
                    } else {
                        out_l[i] = in1_l[i] * (u1 * b1l);
                        out_r[i] = in1_r[i] * (u1 * b1r);
                    }
                }
                MixerState::Silent => {
                    out_l[i] = 0.0;
                    out_r[i] = 0.0;
                }
                MixerState::Crossfading | MixerState::Fading => {
                    let t = if duration > 0 {
                        pos as f32 / duration as f32
                    } else {
                        1.0
                    };
                    let (e0, e1) = Self::envelope_gains(state, t, curve);
                    let (g0l, g0r) = (e0 * u0 * b0l, e0 * u0 * b0r);
                    let (g1l, g1r) = (e1 * u1 * b1l, e1 * u1 * b1r);
                    let o0l = if mute0 { 0.0 } else { out_l[i] * g0l };
                    let o0r = if mute0 { 0.0 } else { out_r[i] * g0r };
                    let o1l = if mute1 { 0.0 } else { in1_l[i] * g1l };
                    let o1r = if mute1 { 0.0 } else { in1_r[i] * g1r };
                    // Keep the `a*g0 + b*g1` shape identical to
                    // `TrackMixer::process` so the f32 sum stays bit-exact.
                    out_l[i] = o0l + o1l;
                    out_r[i] = o0r + o1r;
                    pos += 1;
                    if pos >= duration {
                        state = MixerState::PlayingNext;
                    }
                }
            }
        }

        self.state = state;
        self.crossfade_pos = pos;

        // Phase 3 S2 stream slots: inputs >= 2 are independent streams summed
        // after the pair envelope (the envelope governs slots 0/1 only). Only
        // present when a generation carries more than two inputs — the
        // canonical Phase-3 layout is exactly two, so this tail never runs in
        // the bit-exact-equivalence domain.
        if self.inputs.len() > 2 {
            self.sum_extra_slots(out_l, out_r, frames);
        }
    }

    /// Sum the independent slots (k >= 2) into the master planes at their
    /// per-input gain / balance. Detached slots contribute nothing and their
    /// chains do not advance. The `k` loop indexes both `self.inputs` and the
    /// fixed `duck_gains`/automation tables — index-based is the clear
    /// spelling, so needless_range_loop is allowed (mirrored on the f64 twin).
    #[allow(clippy::needless_range_loop)]
    pub(super) fn sum_extra_slots(&mut self, out_l: &mut [f32], out_r: &mut [f32], frames: usize) {
        // Duck gains per slot (Phase 4 S4), computed before the inputs are
        // borrowed. 1.0 when disabled / not a target.
        let duck_gains: [f32; MAX_MIX_SLOTS] = std::array::from_fn(|k| self.duck_gain_for(k));
        self.duck_tick(frames);
        // Per-frame automation gains, reused across the slot loop (only the
        // slots carrying a track populate them; others stay at unity).
        let mut auto_l = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut auto_r = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        for k in 2..self.inputs.len() {
            let input = &mut self.inputs[k];
            if !input.active {
                continue;
            }
            let dk = duck_gains[k];
            let (b0l, b0r) = balance_gains(input.balance);
            let (p0l, p0r) = pan_gains(input.pan, input.pan_law);
            let (b0l, b0r) = (b0l * p0l, b0r * p0r);
            let has_auto = input.automation.is_some();
            if has_auto {
                // Sample the track into the per-frame arrays and advance the
                // cursor (the track's absolute position moves one block).
                for i in 0..frames {
                    let abs = input.automation.map(|a| a.pos).unwrap_or(0) + i;
                    if let Some((gl, gr)) = input.full_front_gains(abs) {
                        auto_l[i] = gl;
                        auto_r[i] = gr;
                    }
                }
                if let Some(a) = &mut input.automation {
                    a.pos += frames;
                }
            }
            let g = &mut input.gain;
            if input.mute {
                for _ in 0..frames {
                    g.process_sample(1.0);
                }
                continue;
            }
            if has_auto {
                for i in 0..frames {
                    let u = g.process_sample(1.0) * dk;
                    out_l[i] += input.planes[0][i] * (u * auto_l[i]);
                    out_r[i] += input.planes[1][i] * (u * auto_r[i]);
                }
            } else if b0l != 1.0 || b0r != 1.0 || dk != 1.0 {
                for i in 0..frames {
                    let u = g.process_sample(1.0) * dk;
                    out_l[i] += input.planes[0][i] * (u * b0l);
                    out_r[i] += input.planes[1][i] * (u * b0r);
                }
            } else {
                for i in 0..frames {
                    let u = g.process_sample(1.0);
                    out_l[i] += input.planes[0][i] * u;
                    out_r[i] += input.planes[1][i] * u;
                }
            }
        }
    }

    /// f64 variant of [`Self::mix_stereo`]. Matches `TrackMixer::process_f64`:
    /// the normalized position `t` is computed in f64, gains are computed in
    /// f32 and widened, and the sum is `out_l * out_gain + next_l * in_gain`.
    pub(super) fn mix_stereo_f64(&mut self, planes: &mut [&mut [f64]]) {
        let frames = planes[0].len();
        let d0 = self.duck_gain_for(0);
        let d1 = self.duck_gain_for(1);
        self.duck_tick(frames);

        let (l, r) = planes.split_at_mut(1);
        let out_l = &mut l[0][..frames];
        let out_r = &mut r[0][..frames];

        let mut state = self.state;
        let mut pos = self.crossfade_pos;
        let duration = self.crossfade_duration_frames;
        let curve = self.curve;

        let (in0, rest) = self.inputs.split_at_mut(1);
        let in0 = &mut in0[0];
        let (in1, _) = rest.split_at_mut(1);
        let in1 = &mut in1[0];
        let (in1_l, in1_r) = (&in1.planes[0][..frames], &in1.planes[1][..frames]);
        let (g0, bal0, mute0) = (&mut in0.gain, in0.balance, in0.mute);
        let (g1, bal1, mute1) = (&mut in1.gain, in1.balance, in1.mute);
        let (b0l, b0r) = balance_gains(bal0);
        let (b1l, b1r) = balance_gains(bal1);
        let (p0l, p0r) = pan_gains(in0.pan, in0.pan_law);
        let (p1l, p1r) = pan_gains(in1.pan, in1.pan_law);
        let (b0l, b0r) = (b0l * p0l, b0r * p0r);
        let (b1l, b1r) = (b1l * p1l, b1r * p1r);

        for i in 0..frames {
            let u0 = g0.process_sample(1.0) as f64 * d0 as f64;
            let u1 = g1.process_sample(1.0) as f64 * d1 as f64;
            match state {
                MixerState::PlayingCurrent => {
                    if !mute0 && u0 == 1.0 && b0l == 1.0 && b0r == 1.0 {
                        // Pure pre-mix passthrough.
                    } else if mute0 {
                        out_l[i] = 0.0;
                        out_r[i] = 0.0;
                    } else {
                        out_l[i] *= u0 * b0l as f64;
                        out_r[i] *= u0 * b0r as f64;
                    }
                }
                MixerState::PlayingNext => {
                    if mute1 {
                        out_l[i] = 0.0;
                        out_r[i] = 0.0;
                    } else {
                        out_l[i] = in1_l[i] as f64 * (u1 * b1l as f64);
                        out_r[i] = in1_r[i] as f64 * (u1 * b1r as f64);
                    }
                }
                MixerState::Silent => {
                    out_l[i] = 0.0;
                    out_r[i] = 0.0;
                }
                MixerState::Crossfading | MixerState::Fading => {
                    let t = if duration > 0 {
                        pos as f64 / duration as f64
                    } else {
                        1.0
                    };
                    let (e0, e1) = Self::envelope_gains(state, t as f32, curve);
                    let (g0l, g0r) = (e0 as f64 * u0 * b0l as f64, e0 as f64 * u0 * b0r as f64);
                    let (g1l, g1r) = (e1 as f64 * u1 * b1l as f64, e1 as f64 * u1 * b1r as f64);
                    let o0l = if mute0 { 0.0 } else { out_l[i] * g0l };
                    let o0r = if mute0 { 0.0 } else { out_r[i] * g0r };
                    let o1l = if mute1 { 0.0 } else { in1_l[i] as f64 * g1l };
                    let o1r = if mute1 { 0.0 } else { in1_r[i] as f64 * g1r };
                    out_l[i] = o0l + o1l;
                    out_r[i] = o0r + o1r;
                    pos += 1;
                    if pos >= duration {
                        state = MixerState::PlayingNext;
                    }
                }
            }
        }

        self.state = state;
        self.crossfade_pos = pos;

        if self.inputs.len() > 2 {
            self.sum_extra_slots_f64(out_l, out_r, frames);
        }
    }

    /// f64 variant of [`Self::sum_extra_slots`].
    #[allow(clippy::needless_range_loop)]
    pub(super) fn sum_extra_slots_f64(
        &mut self,
        out_l: &mut [f64],
        out_r: &mut [f64],
        frames: usize,
    ) {
        let duck_gains: [f32; MAX_MIX_SLOTS] = std::array::from_fn(|k| self.duck_gain_for(k));
        self.duck_tick(frames);
        let mut auto_l = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut auto_r = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        for k in 2..self.inputs.len() {
            let input = &mut self.inputs[k];
            if !input.active {
                continue;
            }
            let dk = duck_gains[k];
            let (b0l, b0r) = balance_gains(input.balance);
            let (p0l, p0r) = pan_gains(input.pan, input.pan_law);
            let (b0l, b0r) = (b0l * p0l, b0r * p0r);
            let has_auto = input.automation.is_some();
            if has_auto {
                for i in 0..frames {
                    let abs = input.automation.map(|a| a.pos).unwrap_or(0) + i;
                    if let Some((gl, gr)) = input.full_front_gains(abs) {
                        auto_l[i] = gl;
                        auto_r[i] = gr;
                    }
                }
                if let Some(a) = &mut input.automation {
                    a.pos += frames;
                }
            }
            let g = &mut input.gain;
            if input.mute {
                for _ in 0..frames {
                    g.process_sample(1.0);
                }
                continue;
            }
            if has_auto {
                for i in 0..frames {
                    let u = g.process_sample(1.0) as f64 * dk as f64;
                    out_l[i] += input.planes[0][i] as f64 * (u * auto_l[i] as f64);
                    out_r[i] += input.planes[1][i] as f64 * (u * auto_r[i] as f64);
                }
            } else if b0l != 1.0 || b0r != 1.0 || dk != 1.0 {
                for i in 0..frames {
                    let u = g.process_sample(1.0) as f64 * dk as f64;
                    out_l[i] += input.planes[0][i] as f64 * (u * b0l as f64);
                    out_r[i] += input.planes[1][i] as f64 * (u * b0r as f64);
                }
            } else {
                for i in 0..frames {
                    let u = g.process_sample(1.0) as f64;
                    out_l[i] += input.planes[0][i] as f64 * u;
                    out_r[i] += input.planes[1][i] as f64 * u;
                }
            }
        }
    }

    /// Multichannel master path (Phase 4 S2): input 0 passes through after
    /// its user gain (bit-identical to the old `OutPreamp`/`OutLoudness`
    /// steps at unity), then the secondary inputs are summed **channel-wise**
    /// into the master planes at their per-input gain / balance / mute.
    /// Detached slots contribute nothing. The pair envelope stays a stereo
    /// law (it lives in [`Self::mix_stereo`]); on the >2-channel path slots
    /// ≥1 are independent streams summed at unity envelope.
    //
    // The nested frame×channel loops index the master `planes` and can't be
    // trivially re-expressed as iterators: the per-input gain ramp must
    // advance once per frame and the same target scales every channel, and
    // both indices slice the outer `planes` array. Index-based is the clear
    // spelling, so needless_range_loop is allowed here (and mirrored on the
    // f64 twin below for identical semantics).
    #[allow(clippy::needless_range_loop)]
    pub(super) fn mix_multichannel(&mut self, planes: &mut [&mut [f32]]) {
        let channels = planes.len();
        let frames = planes[0].len();
        // Input 0: pre-mixed in place, scaled by its own gain / balance.
        {
            let input0 = &mut self.inputs[0];
            let g = &mut input0.gain;
            let (b0l, b0r) = balance_gains(input0.balance);
            let balance = input0.balance;
            if input0.mute {
                for plane in planes.iter_mut() {
                    plane.fill(0.0);
                }
                for _ in 0..frames {
                    g.process_sample(1.0);
                }
            } else if balance != 0.0 {
                for i in 0..frames {
                    let u = g.process_sample(1.0);
                    for (ch, plane) in planes.iter_mut().enumerate() {
                        plane[i] *= if ch == 0 {
                            u * b0l
                        } else if ch == 1 {
                            u * b0r
                        } else {
                            u
                        };
                    }
                }
            } else {
                for i in 0..frames {
                    let u = g.process_sample(1.0);
                    if u != 1.0 {
                        for plane in planes.iter_mut() {
                            plane[i] *= u;
                        }
                    }
                }
            }
        }
        // Secondary inputs: pre-mixed on their own planes (mix_secondary_f32
        // already ran), now summed channel-wise into the master planes.
        // Slots 1..channels of *the node* feed master channel `ch`; the
        // front L/R pair gets the slot's balance, extra channels pass through
        // at per-input gain.
        let duck_gains: [f32; MAX_MIX_SLOTS] = std::array::from_fn(|k| self.duck_gain_for(k));
        self.duck_tick(frames);
        let mut auto_l = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut auto_r = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        for k in 1..self.inputs.len() {
            let input = &mut self.inputs[k];
            if !input.active || input.mute {
                continue;
            }
            let src_channels = input.channels.min(channels);
            if src_channels == 0 {
                continue;
            }
            let dk = duck_gains[k];
            let (b0l, b0r) = balance_gains(input.balance);
            let (p0l, p0r) = pan_gains(input.pan, input.pan_law);
            let (b0l, b0r) = (b0l * p0l, b0r * p0r);
            let balance = input.balance;
            let has_auto = input.automation.is_some();
            if has_auto {
                for i in 0..frames {
                    let abs = input.automation.map(|a| a.pos).unwrap_or(0) + i;
                    if let Some((gl, gr)) = input.full_front_gains(abs) {
                        auto_l[i] = gl;
                        auto_r[i] = gr;
                    }
                }
                if let Some(a) = &mut input.automation {
                    a.pos += frames;
                }
            }
            let g = &mut input.gain;
            for i in 0..frames {
                // The gain ramp advances once per frame and the same target
                // scales every channel (mirrors the pipeline's per-frame
                // gain law); balance/pan/automation shape only the front
                // L/R pair.
                let u = g.process_sample(1.0) * dk;
                for ch in 0..src_channels {
                    let bal = if has_auto {
                        if ch == 0 {
                            auto_l[i]
                        } else if ch == 1 {
                            auto_r[i]
                        } else {
                            1.0
                        }
                    } else if balance != 0.0 {
                        if ch == 0 {
                            b0l
                        } else if ch == 1 {
                            b0r
                        } else {
                            1.0
                        }
                    } else {
                        1.0
                    };
                    planes[ch][i] += input.planes[ch][i] * (u * bal);
                }
            }
        }
    }

    /// f64 variant of [`Self::mix_multichannel`]. The secondary sum promotes
    /// the f32 secondary planes to f64 at the sum (the f64 bus path keeps
    /// secondary chains in f32).
    #[allow(clippy::needless_range_loop)]
    pub(super) fn mix_multichannel_f64(&mut self, planes: &mut [&mut [f64]]) {
        let channels = planes.len();
        let frames = planes[0].len();
        // Input 0: pre-mixed in place, scaled by its own gain / balance.
        {
            let input0 = &mut self.inputs[0];
            let g = &mut input0.gain;
            let (b0l, b0r) = balance_gains(input0.balance);
            let balance = input0.balance;
            if input0.mute {
                for plane in planes.iter_mut() {
                    plane.fill(0.0);
                }
                for _ in 0..frames {
                    g.process_sample(1.0);
                }
            } else if balance != 0.0 {
                for i in 0..frames {
                    let u = g.process_sample(1.0) as f64;
                    for (ch, plane) in planes.iter_mut().enumerate() {
                        plane[i] *= if ch == 0 {
                            u * b0l as f64
                        } else if ch == 1 {
                            u * b0r as f64
                        } else {
                            u
                        };
                    }
                }
            } else {
                for i in 0..frames {
                    let u = g.process_sample(1.0) as f64;
                    if u != 1.0 {
                        for plane in planes.iter_mut() {
                            plane[i] *= u;
                        }
                    }
                }
            }
        }
        let duck_gains: [f32; MAX_MIX_SLOTS] = std::array::from_fn(|k| self.duck_gain_for(k));
        self.duck_tick(frames);
        let mut auto_l = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut auto_r = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        for k in 1..self.inputs.len() {
            let input = &mut self.inputs[k];
            if !input.active || input.mute {
                continue;
            }
            let dk = duck_gains[k];
            let src_channels = input.channels.min(channels);
            if src_channels == 0 {
                continue;
            }
            let (b0l, b0r) = balance_gains(input.balance);
            let (p0l, p0r) = pan_gains(input.pan, input.pan_law);
            let (b0l, b0r) = (b0l * p0l, b0r * p0r);
            let balance = input.balance;
            let has_auto = input.automation.is_some();
            if has_auto {
                for i in 0..frames {
                    let abs = input.automation.map(|a| a.pos).unwrap_or(0) + i;
                    if let Some((gl, gr)) = input.full_front_gains(abs) {
                        auto_l[i] = gl;
                        auto_r[i] = gr;
                    }
                }
                if let Some(a) = &mut input.automation {
                    a.pos += frames;
                }
            }
            let g = &mut input.gain;
            for i in 0..frames {
                let u = g.process_sample(1.0) as f64 * dk as f64;
                for ch in 0..src_channels {
                    let bal = if has_auto {
                        if ch == 0 {
                            auto_l[i]
                        } else if ch == 1 {
                            auto_r[i]
                        } else {
                            1.0
                        }
                    } else if balance != 0.0 {
                        if ch == 0 {
                            b0l
                        } else if ch == 1 {
                            b0r
                        } else {
                            1.0
                        }
                    } else {
                        1.0
                    };
                    planes[ch][i] += input.planes[ch][i] as f64 * (u * bal as f64);
                }
            }
        }
    }

    /// Run the secondary inputs' pre-mix chains on their own planes (Phase 4
    /// S2: channel-major — every active channel of the slot's planes is
    /// pre-mixed, not just the stereo front pair). Detached slots are skipped
    /// entirely (their chains do not advance). Preamp and loudness are
    /// channel-agnostic gain stages, so they run per plane over up to
    /// `channels` planes; the slot's `channels` field bounds how many were
    /// fed this block.
    ///
    /// Zero-alloc: the plane views are built on a fixed stack array sized to
    /// [`MAX_CHANNELS`] (never a `Vec`), mirroring the multichannel plan's
    /// plane-view discipline.
    pub(super) fn mix_secondary_f32(&mut self, planes: &mut [&mut [f32]]) {
        let frames = planes[0].len();
        for slot in self.inputs.iter_mut().skip(1) {
            if !slot.active {
                continue;
            }
            // Planes are required to be preallocated to MAX_CHANNELS.
            debug_assert!(slot.planes.len() >= MAX_CHANNELS);
            // Build the plane views fresh per slot on a fixed stack array
            // (zero-alloc; a Vec would allocate on the audio path). The
            // sequential `iter_mut().next()` builder is the same discipline
            // the multichannel plan runner uses to avoid heap traffic.
            let mut iter = slot.planes.iter_mut();
            let mut views: [&mut [f32]; MAX_CHANNELS] = std::array::from_fn(|_| {
                let p: &mut Vec<f32> = iter.next().expect("MAX_CHANNELS planes preallocated");
                &mut p[..frames]
            });
            let channels = slot.channels.clamp(1, MAX_CHANNELS);
            slot.preamp.process_block_f32(&mut views[..channels]);
            slot.loudness.process_block_f32(&mut views[..channels]);
            let _ = &mut views;
        }
    }

    /// [`Self::mix_secondary_f32`] for the f64 bus path. Secondary pre-mix
    /// chains run in f32 and the result is promoted in the sum (the f64 bus
    /// path keeps secondary chains in f32; f64 secondary planes are a later
    /// phase of the engine migration).
    pub(super) fn mix_secondary_f64(&mut self, planes: &mut [&mut [f64]]) {
        let frames = planes[0].len();
        for slot in self.inputs.iter_mut().skip(1) {
            if !slot.active {
                continue;
            }
            debug_assert!(slot.planes.len() >= MAX_CHANNELS);
            let mut iter = slot.planes.iter_mut();
            let mut views: [&mut [f32]; MAX_CHANNELS] = std::array::from_fn(|_| {
                let p: &mut Vec<f32> = iter.next().expect("MAX_CHANNELS planes preallocated");
                &mut p[..frames]
            });
            let channels = slot.channels.clamp(1, MAX_CHANNELS);
            slot.preamp.process_block_f32(&mut views[..channels]);
            slot.loudness.process_block_f32(&mut views[..channels]);
            let _ = &mut views;
        }
    }
}
