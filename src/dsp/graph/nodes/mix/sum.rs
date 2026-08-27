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
        // Phase 5 S2 send levels for the pair: the master-send scales the
        // slot's contribution to the master sum; the aux-send is a post-fader
        // tap into the shared send bus (the aux bus node applies the send
        // gains itself — Phase 6). Both at defaults (m = 1.0, aux inactive)
        // and/or the aux disabled → the tap branch is skipped and every
        // expression below is the original bit-exact one.
        let m0 = in0.send.master_gain;
        let m1 = in1.send.master_gain;
        let sb = self.send_bus.data();
        let tap0 = sb.enabled && sb.send_active[0];
        let tap1 = sb.enabled && sb.send_active[1];
        let tap_aux = tap0 || tap1;
        // The send-bus planes, borrowed only when a tap is active (disjoint
        // from the `self.inputs` borrows above). Slot 0's tap lands in slot
        // 0's planes (`slots[0]`/`slots[1]`), slot 1's in slot 1's
        // (`slots[2]`/`slots[3]`) — the aux node applies each slot's own
        // send automation. The post-fader signal is written WITHOUT the send
        // gain — the aux node applies it.
        let (mut a0l, mut a0r, mut a1l, mut a1r) = if tap_aux {
            let (l0, r0, l1, r1) = self.send_bus.pair_planes_mut();
            (
                Some(&mut l0[..frames]),
                Some(&mut r0[..frames]),
                Some(&mut l1[..frames]),
                Some(&mut r1[..frames]),
            )
        } else {
            (None, None, None, None)
        };
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
                    if mute0 {
                        out_l[i] = 0.0;
                        out_r[i] = 0.0;
                    } else if u0 == 1.0 && b0l == 1.0 && b0r == 1.0 && m0 == 1.0 && !tap_aux {
                        // Pure pre-mix passthrough — bit-exact identity.
                    } else {
                        let v_l = out_l[i];
                        let v_r = out_r[i];
                        out_l[i] = v_l * (u0 * b0l * m0);
                        out_r[i] = v_r * (u0 * b0r * m0);
                        // Post-fader aux tap (pre master-send, matching the
                        // lane slots' tap point in `sum_extra_slots`). The
                        // send gain is applied by the aux bus node.
                        if tap0 {
                            if let (Some(al), Some(ar)) = (&mut a0l, &mut a0r) {
                                al[i] += v_l * (u0 * b0l);
                                ar[i] += v_r * (u0 * b0r);
                            }
                        }
                    }
                }
                MixerState::PlayingNext => {
                    if mute1 {
                        out_l[i] = 0.0;
                        out_r[i] = 0.0;
                    } else {
                        out_l[i] = in1_l[i] * (u1 * b1l * m1);
                        out_r[i] = in1_r[i] * (u1 * b1r * m1);
                        if tap1 {
                            if let (Some(al), Some(ar)) = (&mut a1l, &mut a1r) {
                                al[i] += in1_l[i] * (u1 * b1l);
                                ar[i] += in1_r[i] * (u1 * b1r);
                            }
                        }
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
                    let (g0l, g0r) = (e0 * u0 * b0l * m0, e0 * u0 * b0r * m0);
                    let (g1l, g1r) = (e1 * u1 * b1l * m1, e1 * u1 * b1r * m1);
                    // Capture the pre-blend planes so the optional aux tap
                    // can use the slots' pre-envelope contributions while the
                    // blend expressions stay exactly `TrackMixer`'s.
                    let orig_l = out_l[i];
                    let orig_r = out_r[i];
                    let o0l = if mute0 { 0.0 } else { orig_l * g0l };
                    let o0r = if mute0 { 0.0 } else { orig_r * g0r };
                    let o1l = if mute1 { 0.0 } else { in1_l[i] * g1l };
                    let o1r = if mute1 { 0.0 } else { in1_r[i] * g1r };
                    // Keep the `a*g0 + b*g1` shape identical to
                    // `TrackMixer::process` so the f32 sum stays bit-exact.
                    out_l[i] = o0l + o1l;
                    out_r[i] = o0r + o1r;
                    if tap_aux {
                        if tap0 {
                            if let (Some(al), Some(ar)) = (&mut a0l, &mut a0r) {
                                if !mute0 {
                                    al[i] += orig_l * (u0 * b0l);
                                    ar[i] += orig_r * (u0 * b0r);
                                }
                            }
                        }
                        if tap1 {
                            if let (Some(al), Some(ar)) = (&mut a1l, &mut a1r) {
                                if !mute1 {
                                    al[i] += in1_l[i] * (u1 * b1l);
                                    ar[i] += in1_r[i] * (u1 * b1r);
                                }
                            }
                        }
                    }
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
        // borrowed. 1.0 when disabled / not a target. NOTE: the duck envelope
        // must advance exactly ONCE per block — the caller (`mix_stereo` /
        // `mix_multichannel`) already ran `duck_tick`; ticking again here
        // would ramp attack/release at 2x the configured rate whenever the
        // bus carries independent slots.
        let duck_gains: [f32; MAX_MIX_SLOTS] = std::array::from_fn(|k| self.duck_gain_for(k));
        // Per-frame automation gains, reused across the slot loop (only the
        // slots carrying a track populate them; others stay at unity).
        let mut auto_l = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut auto_r = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut aux_auto = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        for k in 2..self.inputs.len() {
            let input = &mut self.inputs[k];
            if !input.active {
                continue;
            }
            let dk = duck_gains[k];
            let m = input.send.master_gain;
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
                    if let Some(v) = input.automation_send_value(abs) {
                        aux_auto[i] = v;
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
            // Phase 5 S2: a slot whose master / aux sends are at defaults
            // takes the original expressions untouched (bit-exact); otherwise
            // the contribution is captured once and scaled into both
            // destinations (post-fader tap into the shared send bus — the
            // aux node applies the send gain itself).
            let sb = self.send_bus.data();
            let tap = sb.enabled && sb.send_active[k];
            if m == 1.0 && !tap {
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
                continue;
            }
            if tap {
                let (al, ar) = self.send_bus.slot_planes_mut(k);
                let al = &mut al[..frames];
                let ar = &mut ar[..frames];
                if has_auto {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) * dk;
                        let s = input.planes[0][i] * (u * auto_l[i]);
                        out_l[i] += s * m;
                        al[i] += s * aux_auto[i];
                        let s = input.planes[1][i] * (u * auto_r[i]);
                        out_r[i] += s * m;
                        ar[i] += s * aux_auto[i];
                    }
                } else if b0l != 1.0 || b0r != 1.0 || dk != 1.0 {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) * dk;
                        let s = input.planes[0][i] * (u * b0l);
                        out_l[i] += s * m;
                        al[i] += s;
                        let s = input.planes[1][i] * (u * b0r);
                        out_r[i] += s * m;
                        ar[i] += s;
                    }
                } else {
                    for i in 0..frames {
                        let u = g.process_sample(1.0);
                        let s = input.planes[0][i] * u;
                        out_l[i] += s * m;
                        al[i] += s;
                        let s = input.planes[1][i] * u;
                        out_r[i] += s * m;
                        ar[i] += s;
                    }
                }
            } else {
                // Master-only fold (m != 1.0, no aux tap).
                if has_auto {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) * dk;
                        out_l[i] += input.planes[0][i] * (u * auto_l[i]) * m;
                        out_r[i] += input.planes[1][i] * (u * auto_r[i]) * m;
                    }
                } else if b0l != 1.0 || b0r != 1.0 || dk != 1.0 {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) * dk;
                        out_l[i] += input.planes[0][i] * (u * b0l) * m;
                        out_r[i] += input.planes[1][i] * (u * b0r) * m;
                    }
                } else {
                    for i in 0..frames {
                        let u = g.process_sample(1.0);
                        out_l[i] += input.planes[0][i] * u * m;
                        out_r[i] += input.planes[1][i] * u * m;
                    }
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
        // Phase 5 S2: master-send + post-fader aux tap for the pair (see the
        // f32 twin for the disabled-exact contract). The tap is gated on the
        // shared send bus's `send_active` and writes WITHOUT the send gain —
        // the aux bus node (Phase 6) applies the per-send automation ramps.
        let m0 = in0.send.master_gain;
        let m1 = in1.send.master_gain;
        let sb = self.send_bus.data();
        let tap0 = sb.enabled && sb.send_active[0];
        let tap1 = sb.enabled && sb.send_active[1];
        let tap_aux = tap0 || tap1;
        // Per-slot send-bus planes (see the f32 twin): slot 0's tap in
        // `slots[0]`/`slots[1]`, slot 1's in `slots[2]`/`slots[3]`.
        let (mut a0l, mut a0r, mut a1l, mut a1r) = if tap_aux {
            let (l0, r0, l1, r1) = self.send_bus.pair_planes_mut();
            (
                Some(&mut l0[..frames]),
                Some(&mut r0[..frames]),
                Some(&mut l1[..frames]),
                Some(&mut r1[..frames]),
            )
        } else {
            (None, None, None, None)
        };
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
                    if mute0 {
                        out_l[i] = 0.0;
                        out_r[i] = 0.0;
                    } else if u0 == 1.0 && b0l == 1.0 && b0r == 1.0 && m0 == 1.0 && !tap_aux {
                        // Pure pre-mix passthrough.
                    } else {
                        let v_l = out_l[i];
                        let v_r = out_r[i];
                        out_l[i] = v_l * (u0 * b0l as f64 * m0 as f64);
                        out_r[i] = v_r * (u0 * b0r as f64 * m0 as f64);
                        if tap0 {
                            if let (Some(al), Some(ar)) = (&mut a0l, &mut a0r) {
                                al[i] += (v_l * (u0 * b0l as f64)) as f32;
                                ar[i] += (v_r * (u0 * b0r as f64)) as f32;
                            }
                        }
                    }
                }
                MixerState::PlayingNext => {
                    if mute1 {
                        out_l[i] = 0.0;
                        out_r[i] = 0.0;
                    } else {
                        out_l[i] = in1_l[i] as f64 * (u1 * b1l as f64 * m1 as f64);
                        out_r[i] = in1_r[i] as f64 * (u1 * b1r as f64 * m1 as f64);
                        if tap1 {
                            if let (Some(al), Some(ar)) = (&mut a1l, &mut a1r) {
                                al[i] += (in1_l[i] as f64 * (u1 * b1l as f64)) as f32;
                                ar[i] += (in1_r[i] as f64 * (u1 * b1r as f64)) as f32;
                            }
                        }
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
                    let (g0l, g0r) = (
                        e0 as f64 * u0 * b0l as f64 * m0 as f64,
                        e0 as f64 * u0 * b0r as f64 * m0 as f64,
                    );
                    let (g1l, g1r) = (
                        e1 as f64 * u1 * b1l as f64 * m1 as f64,
                        e1 as f64 * u1 * b1r as f64 * m1 as f64,
                    );
                    // Capture the pre-blend planes so the optional aux tap
                    // can use the slots' pre-envelope contributions while the
                    // blend expressions stay exactly `TrackMixer`'s.
                    let orig_l = out_l[i];
                    let orig_r = out_r[i];
                    let o0l = if mute0 { 0.0 } else { orig_l * g0l };
                    let o0r = if mute0 { 0.0 } else { orig_r * g0r };
                    let o1l = if mute1 { 0.0 } else { in1_l[i] as f64 * g1l };
                    let o1r = if mute1 { 0.0 } else { in1_r[i] as f64 * g1r };
                    out_l[i] = o0l + o1l;
                    out_r[i] = o0r + o1r;
                    if tap_aux {
                        if tap0 {
                            if let (Some(al), Some(ar)) = (&mut a0l, &mut a0r) {
                                if !mute0 {
                                    al[i] += (orig_l * (u0 * b0l as f64)) as f32;
                                    ar[i] += (orig_r * (u0 * b0r as f64)) as f32;
                                }
                            }
                        }
                        if tap1 {
                            if let (Some(al), Some(ar)) = (&mut a1l, &mut a1r) {
                                if !mute1 {
                                    al[i] += (in1_l[i] as f64 * (u1 * b1l as f64)) as f32;
                                    ar[i] += (in1_r[i] as f64 * (u1 * b1r as f64)) as f32;
                                }
                            }
                        }
                    }
                    pos += 1;
                    if pos >= duration {
                        state = MixerState::PlayingNext;
                    }
                }
            }
        }

        self.state = state;
        self.crossfade_pos = pos;
        // Phase 5 S2: the pair wrote its taps into the shared send bus (the
        // aux node sets its own `written` flag from `send_active`).

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
        let mut auto_l = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut auto_r = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut aux_auto = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        for k in 2..self.inputs.len() {
            let input = &mut self.inputs[k];
            if !input.active {
                continue;
            }
            let dk = duck_gains[k];
            let m = input.send.master_gain;
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
                    if let Some(v) = input.automation_send_value(abs) {
                        aux_auto[i] = v;
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
            // Phase 5 S2: tap gated on the shared send bus (see the f32
            // twin); the post-fader signal lands WITHOUT the send gain — the
            // aux node applies per-slot automation.
            let tap = self.send_bus.data().enabled && self.send_bus.data().send_active[k];
            if m == 1.0 && !tap {
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
                continue;
            }
            if tap {
                let (al, ar) = self.send_bus.slot_planes_mut(k);
                let al = &mut al[..frames];
                let ar = &mut ar[..frames];
                if has_auto {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) as f64 * dk as f64;
                        let s = input.planes[0][i] as f64 * (u * auto_l[i] as f64);
                        out_l[i] += s * m as f64;
                        al[i] += (s * aux_auto[i] as f64) as f32;
                        let s = input.planes[1][i] as f64 * (u * auto_r[i] as f64);
                        out_r[i] += s * m as f64;
                        ar[i] += (s * aux_auto[i] as f64) as f32;
                    }
                } else if b0l != 1.0 || b0r != 1.0 || dk != 1.0 {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) as f64 * dk as f64;
                        let s = input.planes[0][i] as f64 * (u * b0l as f64);
                        out_l[i] += s * m as f64;
                        al[i] += s as f32;
                        let s = input.planes[1][i] as f64 * (u * b0r as f64);
                        out_r[i] += s * m as f64;
                        ar[i] += s as f32;
                    }
                } else {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) as f64;
                        let s = input.planes[0][i] as f64 * u;
                        out_l[i] += s * m as f64;
                        al[i] += s as f32;
                        let s = input.planes[1][i] as f64 * u;
                        out_r[i] += s * m as f64;
                        ar[i] += s as f32;
                    }
                }
            } else {
                // Master-only fold (m != 1.0, no aux tap).
                if has_auto {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) as f64 * dk as f64;
                        out_l[i] += input.planes[0][i] as f64 * (u * auto_l[i] as f64) * m as f64;
                        out_r[i] += input.planes[1][i] as f64 * (u * auto_r[i] as f64) * m as f64;
                    }
                } else if b0l != 1.0 || b0r != 1.0 || dk != 1.0 {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) as f64 * dk as f64;
                        out_l[i] += input.planes[0][i] as f64 * (u * b0l as f64) * m as f64;
                        out_r[i] += input.planes[1][i] as f64 * (u * b0r as f64) * m as f64;
                    }
                } else {
                    for i in 0..frames {
                        let u = g.process_sample(1.0) as f64;
                        out_l[i] += input.planes[0][i] as f64 * u * m as f64;
                        out_r[i] += input.planes[1][i] as f64 * u * m as f64;
                    }
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
        // Input 0: pre-mixed in place, scaled by its own gain / balance
        // (Phase 5 S2: the master-send fold applies to the pair here too, and
        // slot 0's post-fader aux tap joins the accumulator like every other
        // slot's — pre master-send, gated on `aux_gain != 0 && aux.enabled`
        // so the default path keeps its exact expressions).
        {
            let input0 = &mut self.inputs[0];
            let g = &mut input0.gain;
            let (b0l, b0r) = balance_gains(input0.balance);
            let balance = input0.balance;
            let m0 = input0.send.master_gain;
            let tap_aux = self.send_bus.data().enabled && self.send_bus.data().send_active[0];
            let (mut aux0, mut aux1) = if tap_aux {
                let (l0, r0, _, _) = self.send_bus.pair_planes_mut();
                (Some(&mut l0[..frames]), Some(&mut r0[..frames]))
            } else {
                (None, None)
            };
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
                        let v = plane[i];
                        let gch = if ch == 0 {
                            u * b0l * m0
                        } else if ch == 1 {
                            u * b0r * m0
                        } else {
                            u * m0
                        };
                        plane[i] = v * gch;
                        if tap_aux && ch < 2 {
                            if let (Some(al), Some(ar)) = (&mut aux0, &mut aux1) {
                                let bal = if ch == 0 { b0l } else { b0r };
                                if ch == 0 {
                                    al[i] += v * u * bal;
                                } else {
                                    ar[i] += v * u * bal;
                                }
                            }
                        }
                    }
                }
            } else if m0 != 1.0 {
                for i in 0..frames {
                    let ug = g.process_sample(1.0);
                    for (ch, plane) in planes.iter_mut().enumerate() {
                        let v = plane[i];
                        plane[i] = v * (ug * m0);
                        if tap_aux && ch < 2 {
                            if let (Some(al), Some(ar)) = (&mut aux0, &mut aux1) {
                                if ch == 0 {
                                    al[i] += v * ug;
                                } else {
                                    ar[i] += v * ug;
                                }
                            }
                        }
                    }
                }
            } else {
                for i in 0..frames {
                    let u = g.process_sample(1.0);
                    if u != 1.0 || tap_aux {
                        for (ch, plane) in planes.iter_mut().enumerate() {
                            let v = plane[i];
                            if u != 1.0 {
                                plane[i] = v * u;
                            }
                            if tap_aux && ch < 2 {
                                if let (Some(al), Some(ar)) = (&mut aux0, &mut aux1) {
                                    if ch == 0 {
                                        al[i] += v * u;
                                    } else {
                                        ar[i] += v * u;
                                    }
                                }
                            }
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
        let mut aux_auto = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
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
            let m = input.send.master_gain;
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
                    if let Some(v) = input.automation_send_value(abs) {
                        aux_auto[i] = v;
                    }
                }
                if let Some(a) = &mut input.automation {
                    a.pos += frames;
                }
            }
            let g = &mut input.gain;
            // Phase 5 S2: master-send fold + aux tap on the front pair,
            // gated on the shared send bus (Phase 6: per-slot send targets).
            let tap = self.send_bus.data().enabled && self.send_bus.data().send_active[k];
            if m == 1.0 && !tap {
                for i in 0..frames {
                    // The gain ramp advances once per frame and the same
                    // target scales every channel (mirrors the pipeline's
                    // per-frame gain law); balance/pan/automation shape only
                    // the front L/R pair.
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
                continue;
            }
            if tap {
                let (al, ar) = self.send_bus.slot_planes_mut(k);
                let al = &mut al[..frames];
                let ar = &mut ar[..frames];
                for i in 0..frames {
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
                        let s = input.planes[ch][i] * (u * bal);
                        planes[ch][i] += s * m;
                        if ch == 0 {
                            al[i] += s * aux_auto[i];
                        } else if ch == 1 {
                            ar[i] += s * aux_auto[i];
                        }
                    }
                }
            } else {
                // Master-only fold (m != 1.0, no aux tap).
                for i in 0..frames {
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
                        planes[ch][i] += input.planes[ch][i] * (u * bal) * m;
                    }
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
        // Input 0: pre-mixed in place, scaled by its own gain / balance
        // (Phase 5 S2: the master-send fold applies to the pair here too, and
        // slot 0's post-fader aux tap joins the accumulator like every other
        // slot's — pre master-send, gated on `aux_gain != 0 && aux.enabled`
        // so the default path keeps its exact expressions).
        {
            let input0 = &mut self.inputs[0];
            let g = &mut input0.gain;
            let (b0l, b0r) = balance_gains(input0.balance);
            let balance = input0.balance;
            let m0 = input0.send.master_gain;
            let tap_aux = self.send_bus.data().enabled && self.send_bus.data().send_active[0];
            let (mut aux0, mut aux1) = if tap_aux {
                let (l0, r0, _, _) = self.send_bus.pair_planes_mut();
                (Some(&mut l0[..frames]), Some(&mut r0[..frames]))
            } else {
                (None, None)
            };
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
                        let v = plane[i];
                        let gch = if ch == 0 {
                            u * b0l as f64 * m0 as f64
                        } else if ch == 1 {
                            u * b0r as f64 * m0 as f64
                        } else {
                            u * m0 as f64
                        };
                        plane[i] = v * gch;
                        if tap_aux && ch < 2 {
                            if let (Some(al), Some(ar)) = (&mut aux0, &mut aux1) {
                                let bal = if ch == 0 { b0l as f64 } else { b0r as f64 };
                                if ch == 0 {
                                    al[i] += (v * (u * bal)) as f32;
                                } else {
                                    ar[i] += (v * (u * bal)) as f32;
                                }
                            }
                        }
                    }
                }
            } else if m0 != 1.0 {
                for i in 0..frames {
                    let ug = g.process_sample(1.0) as f64;
                    for (ch, plane) in planes.iter_mut().enumerate() {
                        let v = plane[i];
                        plane[i] = v * (ug * m0 as f64);
                        if tap_aux && ch < 2 {
                            if let (Some(al), Some(ar)) = (&mut aux0, &mut aux1) {
                                if ch == 0 {
                                    al[i] += (v * ug) as f32;
                                } else {
                                    ar[i] += (v * ug) as f32;
                                }
                            }
                        }
                    }
                }
            } else {
                for i in 0..frames {
                    let u = g.process_sample(1.0) as f64;
                    if u != 1.0 || tap_aux {
                        for (ch, plane) in planes.iter_mut().enumerate() {
                            let v = plane[i];
                            if u != 1.0 {
                                plane[i] = v * u;
                            }
                            if tap_aux && ch < 2 {
                                if let (Some(al), Some(ar)) = (&mut aux0, &mut aux1) {
                                    if ch == 0 {
                                        al[i] += (v * u) as f32;
                                    } else {
                                        ar[i] += (v * u) as f32;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let duck_gains: [f32; MAX_MIX_SLOTS] = std::array::from_fn(|k| self.duck_gain_for(k));
        self.duck_tick(frames);
        let mut auto_l = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut auto_r = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut aux_auto = [1.0f32; MAX_AUDIO_BLOCK_FRAMES];
        for k in 1..self.inputs.len() {
            let input = &mut self.inputs[k];
            if !input.active || input.mute {
                continue;
            }
            let dk = duck_gains[k];
            let m = input.send.master_gain;
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
                    if let Some(v) = input.automation_send_value(abs) {
                        aux_auto[i] = v;
                    }
                }
                if let Some(a) = &mut input.automation {
                    a.pos += frames;
                }
            }
            let g = &mut input.gain;
            let tap = self.send_bus.data().enabled && self.send_bus.data().send_active[k];
            if m == 1.0 && !tap {
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
                continue;
            }
            if tap {
                let (al, ar) = self.send_bus.slot_planes_mut(k);
                let al = &mut al[..frames];
                let ar = &mut ar[..frames];
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
                        let s = input.planes[ch][i] as f64 * (u * bal as f64);
                        planes[ch][i] += s * m as f64;
                        if ch == 0 {
                            al[i] += (s * aux_auto[i] as f64) as f32;
                        } else if ch == 1 {
                            ar[i] += (s * aux_auto[i] as f64) as f32;
                        }
                    }
                }
            } else {
                // Master-only fold (m != 1.0, no aux tap).
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
                        planes[ch][i] += input.planes[ch][i] as f64 * (u * bal as f64) * m as f64;
                    }
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
            slot.trim
                .apply_views(&mut views[..channels], channels, frames);
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
            slot.trim
                .apply_views(&mut views[..channels], channels, frames);
            let _ = &mut views;
        }
    }
}
