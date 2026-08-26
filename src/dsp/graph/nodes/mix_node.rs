//! Phase 3 S1 — the mix bus node.
//!
//! [`MixBusNode`] replaces the graph's four global pre-mix slots
//! (`OutPreamp` / `OutLoudness` / `InPreamp` / `InLoudness`) with a single
//! bus whose **inputs** each own a complete pre-mix chain: preamp +
//! loudness + a user gain ramp + balance + mute. The transition envelope
//! mirrors [`TrackMixer`]'s state machine exactly (`PlayingCurrent`,
//! `Crossfading`, `Fading`, `PlayingNext`, `Silent`) and reuses its static
//! gain functions, so a 2-input bus reproduces the pipeline's crossfade
//! path bit-for-bit — pinned by `tests/fidelity/graph_pipeline_equivalence`.
//!
//! Realtime contract: the node owns one pair of secondary-input planes per
//! input (preallocated at construction), advances every input's state
//! exactly once per processed block, and performs no allocation on the hot
//! path. S1 sums secondary inputs into the stereo front pair only;
//! multichannel bus mixing is Phase 4.

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::dsp::{
    crossfade::{CrossfadeCurve, MixerState, TrackMixer},
    gain::GainProcessor,
    graph::node::DspNode,
    loudness::{LoudnessMetadata, LoudnessMode},
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

use super::{GainNode, LoudnessNode};

/// One input to the mix bus: a complete per-stream pre-mix chain plus its
/// user gain / balance / mute and secondary-plane storage. Input 0 processes
/// the caller's planes in place; inputs ≥ 1 read from `planes_l`/`planes_r`.
pub struct MixInput {
    /// Introspection name of the preamp component (e.g. `"out_preamp"`).
    pub name: &'static str,
    pub preamp: GainNode,
    pub loudness: LoudnessNode,
    /// Per-input user gain (one-pole ramp; defaults to unity).
    pub gain: GainProcessor,
    /// Per-input balance in [-1, 1] (0 = center).
    pub balance: f32,
    /// Mute: the input contributes silence.
    pub mute: bool,
    /// Detached slot: the input contributes nothing and its chains do not
    /// advance (Phase 3 S2 slot lifecycle). Slot 0 is never detached.
    pub active: bool,
    /// Secondary-input plane storage (capacity [`MAX_AUDIO_BLOCK_FRAMES`]).
    /// The graph's process driver feeds these; the node processes them.
    pub(crate) planes_l: Vec<f32>,
    pub(crate) planes_r: Vec<f32>,
}

impl MixInput {
    pub fn new(
        preamp_name: &'static str,
        loudness_name: &'static str,
        preamp_ramp_ms: f32,
        sample_rate: f32,
    ) -> Self {
        Self {
            name: preamp_name,
            preamp: GainNode::new(preamp_name, "pre-mix", preamp_ramp_ms, sample_rate),
            loudness: LoudnessNode::new(loudness_name, "pre-mix", sample_rate),
            gain: GainProcessor::with_ramp(1.0, preamp_ramp_ms, sample_rate),
            balance: 0.0,
            mute: false,
            active: true,
            planes_l: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            planes_r: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
        }
    }
}

/// Per-input control commands (plain data; ride the SPSC control queues).
#[derive(Clone, Copy, Debug)]
pub enum MixInputCmd {
    /// Set the linear user gain target in [0, 1].
    SetGain(f32),
    /// Set the user gain target in dB ([-60, 0]).
    SetGainDb(f32),
    /// Set the per-input balance in [-1, 1].
    SetBalance(f32),
    SetMute(bool),
    /// Detach / re-attach the slot (Phase 3 S2 stream slots).
    SetActive(bool),
    SetLoudnessMode(LoudnessMode),
    ApplyLoudnessMetadata(LoudnessMetadata),
}

/// Node-level transition commands, mirroring [`TrackMixer`]'s start_* API.
#[derive(Clone, Copy, Debug)]
pub enum MixTransitionCmd {
    /// Input 0 at unity, secondary inputs silent.
    StartPlaying,
    /// Crossfade from input 0 to input 1 over `duration_frames`. Gated by
    /// [`MixBusNode::crossfade_enabled`]: when disabled this is a gapless
    /// switch to `PlayingNext` (mirrors `TrackMixer::start_crossfade`).
    StartCrossfade {
        duration_frames: usize,
    },
    /// Sequential fade (fade-out → gap → fade-in) over `duration_frames`.
    StartFade {
        duration_frames: usize,
    },
    Silent,
}

/// Per-input balance law gains `(l, r)` — mirrors the pipeline's balance
/// stage (`balance >= 0` attenuates L, `balance < 0` boosts R).
#[inline]
fn balance_gains(balance: f32) -> (f32, f32) {
    if balance >= 0.0 {
        (1.0 - balance, 1.0)
    } else {
        (1.0, 1.0 + balance)
    }
}

/// The mix bus: N per-input pre-mix chains summed into the master planes.
pub struct MixBusNode {
    pub inputs: Vec<MixInput>,
    /// Transition envelope state — mirrors [`TrackMixer`] for bit-exactness.
    pub state: MixerState,
    pub crossfade_pos: usize,
    pub crossfade_duration_frames: usize,
    pub curve: CrossfadeCurve,
    /// Whether crossfade transitions are enabled (`config.crossfade.enabled`).
    pub crossfade_enabled: bool,
}

impl MixBusNode {
    /// Construct a bus with two inputs (outgoing / incoming), the canonical
    /// Phase-3 layout. The envelope starts in [`MixerState::PlayingCurrent`]:
    /// input 0 passes through (after its pre-mix) exactly as the old
    /// `OutPreamp` / `OutLoudness` steps did.
    pub fn new(
        sample_rate: f32,
        crossfade_duration_ms: u64,
        crossfade_enabled: bool,
        curve: config::CrossfadeCurve,
        preamp_ramp_ms: f32,
    ) -> Self {
        Self {
            inputs: vec![
                MixInput::new("out_preamp", "out_loudness", preamp_ramp_ms, sample_rate),
                MixInput::new("in_preamp", "in_loudness", preamp_ramp_ms, sample_rate),
            ],
            state: MixerState::PlayingCurrent,
            crossfade_pos: 0,
            crossfade_duration_frames: (crossfade_duration_ms as f32 * 0.001 * sample_rate)
                as usize,
            curve: curve.into(),
            crossfade_enabled,
        }
    }

    /// Apply one per-input command. Out-of-range inputs are ignored
    /// (defensive: the generation's input count is fixed, so this only
    /// happens if a caller addresses a slot the current generation lacks).
    pub fn apply_input(&mut self, input: usize, cmd: MixInputCmd) {
        let Some(slot) = self.inputs.get_mut(input) else {
            return;
        };
        match cmd {
            MixInputCmd::SetGain(v) => slot.gain.set_gain(v.clamp(0.0, 1.0)),
            MixInputCmd::SetGainDb(db) => {
                if db.is_finite() {
                    let linear = if db <= -60.0 {
                        0.0
                    } else {
                        10.0_f32.powf(db.clamp(-60.0, 0.0) / 20.0).clamp(0.0, 1.0)
                    };
                    slot.gain.set_gain(linear);
                }
            }
            MixInputCmd::SetBalance(b) => slot.balance = b.clamp(-1.0, 1.0),
            MixInputCmd::SetMute(m) => slot.mute = m,
            MixInputCmd::SetActive(a) => {
                // Slot 0 is the caller's in-place planes; it cannot be
                // detached (the engine always plays the primary stream).
                if input != 0 {
                    slot.active = a;
                }
            }
            MixInputCmd::SetLoudnessMode(m) => slot.loudness.normalizer.set_mode(m),
            MixInputCmd::ApplyLoudnessMetadata(meta) => {
                slot.loudness.normalizer.set_track_metadata(&meta)
            }
        }
    }

    /// Rescale an in-progress transition when the device changes sample
    /// rate, preserving the normalized envelope progress (mirrors
    /// `TrackMixer::rescale_sample_rate`).
    pub(crate) fn rescale_sample_rate(&mut self, old_rate: f32, new_rate: f32) {
        if !old_rate.is_finite()
            || !new_rate.is_finite()
            || old_rate <= 0.0
            || new_rate <= 0.0
            || (old_rate - new_rate).abs() < f32::EPSILON
        {
            return;
        }
        let old_duration = self.crossfade_duration_frames.max(1);
        let progress = self.crossfade_pos as f64 / old_duration as f64;
        let new_duration =
            ((old_duration as f64 * new_rate as f64 / old_rate as f64).round() as usize).max(1);
        self.crossfade_duration_frames = new_duration;
        self.crossfade_pos = (progress * new_duration as f64)
            .round()
            .min(new_duration as f64) as usize;
    }

    /// Apply one transition command (mirrors `TrackMixer::start_*`).
    pub fn apply_transition(&mut self, cmd: MixTransitionCmd) {
        match cmd {
            MixTransitionCmd::StartPlaying => {
                self.state = MixerState::PlayingCurrent;
                self.crossfade_pos = 0;
            }
            MixTransitionCmd::StartCrossfade { duration_frames } => {
                if self.crossfade_enabled {
                    self.state = MixerState::Crossfading;
                    self.crossfade_pos = 0;
                    self.crossfade_duration_frames = duration_frames.max(1);
                } else {
                    // Crossfade disabled: gapless transition.
                    self.state = MixerState::PlayingNext;
                    self.crossfade_pos = 0;
                }
            }
            MixTransitionCmd::StartFade { duration_frames } => {
                self.state = MixerState::Fading;
                self.crossfade_pos = 0;
                self.crossfade_duration_frames = duration_frames.max(1);
            }
            MixTransitionCmd::Silent => {
                self.state = MixerState::Silent;
                self.crossfade_pos = 0;
            }
        }
    }

    /// Envelope gains `(input0, input1)` at normalized position `t`, using
    /// the exact `TrackMixer` math.
    #[inline]
    fn envelope_gains(state: MixerState, t: f32, curve: CrossfadeCurve) -> (f32, f32) {
        match state {
            MixerState::PlayingCurrent => (1.0, 0.0),
            MixerState::PlayingNext => (0.0, 1.0),
            MixerState::Silent => (0.0, 0.0),
            MixerState::Crossfading => TrackMixer::compute_gains_for_curve(t, curve),
            MixerState::Fading => TrackMixer::compute_fade_gains(t, curve),
        }
    }

    /// Stereo mix: per-frame envelope + user gains + balance + mute, summing
    /// input 0 (in the master planes) and input 1 (its own planes). The
    /// envelope state is hoisted into locals so the frame loop can advance
    /// it while the per-input borrows stay alive.
    fn mix_stereo(&mut self, planes: &mut [&mut [f32]]) {
        let frames = planes[0].len();
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
        let (in1_l, in1_r) = (&in1.planes_l[..frames], &in1.planes_r[..frames]);
        let (g0, bal0, mute0) = (&mut in0.gain, in0.balance, in0.mute);
        let (g1, bal1, mute1) = (&mut in1.gain, in1.balance, in1.mute);
        let (b0l, b0r) = balance_gains(bal0);
        let (b1l, b1r) = balance_gains(bal1);

        for i in 0..frames {
            let u0 = g0.process_sample(1.0);
            let u1 = g1.process_sample(1.0);
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
    /// chains do not advance.
    fn sum_extra_slots(&mut self, out_l: &mut [f32], out_r: &mut [f32], frames: usize) {
        for k in 2..self.inputs.len() {
            let input = &mut self.inputs[k];
            if !input.active {
                continue;
            }
            let (b0l, b0r) = balance_gains(input.balance);
            let g = &mut input.gain;
            if input.mute {
                for _ in 0..frames {
                    g.process_sample(1.0);
                }
                continue;
            }
            if input.balance != 0.0 {
                for i in 0..frames {
                    let u = g.process_sample(1.0);
                    out_l[i] += input.planes_l[i] * (u * b0l);
                    out_r[i] += input.planes_r[i] * (u * b0r);
                }
            } else {
                for i in 0..frames {
                    let u = g.process_sample(1.0);
                    out_l[i] += input.planes_l[i] * u;
                    out_r[i] += input.planes_r[i] * u;
                }
            }
        }
    }

    /// f64 variant of [`Self::mix_stereo`]. Matches `TrackMixer::process_f64`:
    /// the normalized position `t` is computed in f64, gains are computed in
    /// f32 and widened, and the sum is `out_l * out_gain + next_l * in_gain`.
    fn mix_stereo_f64(&mut self, planes: &mut [&mut [f64]]) {
        let frames = planes[0].len();
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
        let (in1_l, in1_r) = (&in1.planes_l[..frames], &in1.planes_r[..frames]);
        let (g0, bal0, mute0) = (&mut in0.gain, in0.balance, in0.mute);
        let (g1, bal1, mute1) = (&mut in1.gain, in1.balance, in1.mute);
        let (b0l, b0r) = balance_gains(bal0);
        let (b1l, b1r) = balance_gains(bal1);

        for i in 0..frames {
            let u0 = g0.process_sample(1.0) as f64;
            let u1 = g1.process_sample(1.0) as f64;
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
    fn sum_extra_slots_f64(&mut self, out_l: &mut [f64], out_r: &mut [f64], frames: usize) {
        for k in 2..self.inputs.len() {
            let input = &mut self.inputs[k];
            if !input.active {
                continue;
            }
            let (b0l, b0r) = balance_gains(input.balance);
            let g = &mut input.gain;
            if input.mute {
                for _ in 0..frames {
                    g.process_sample(1.0);
                }
                continue;
            }
            if input.balance != 0.0 {
                for i in 0..frames {
                    let u = g.process_sample(1.0) as f64;
                    out_l[i] += input.planes_l[i] as f64 * (u * b0l as f64);
                    out_r[i] += input.planes_r[i] as f64 * (u * b0r as f64);
                }
            } else {
                for i in 0..frames {
                    let u = g.process_sample(1.0) as f64;
                    out_l[i] += input.planes_l[i] as f64 * u;
                    out_r[i] += input.planes_r[i] as f64 * u;
                }
            }
        }
    }

    /// Multichannel master path (S1): input 0 only, with its user gain. The
    /// envelope and secondary inputs are stereo-only in S1 (Phase 4 lifts
    /// this), so this is a per-plane gain pass over the pre-mixed planes —
    /// bit-identical to the old `OutPreamp`/`OutLoudness` steps at unity.
    fn mix_multichannel(&mut self, planes: &mut [&mut [f32]]) {
        let frames = planes[0].len();
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
            return;
        }
        if balance != 0.0 {
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

    /// f64 variant of [`Self::mix_multichannel`].
    fn mix_multichannel_f64(&mut self, planes: &mut [&mut [f64]]) {
        let frames = planes[0].len();
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
            return;
        }
        if balance != 0.0 {
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

    /// Run the secondary inputs' pre-mix chains on their own f32 planes.
    /// Detached slots are skipped entirely (their chains do not advance).
    fn mix_secondary_f32(&mut self, planes: &mut [&mut [f32]]) {
        let frames = planes[0].len();
        for i in 1..self.inputs.len() {
            let input = &mut self.inputs[i];
            if !input.active {
                continue;
            }
            let (l, r) = (&mut input.planes_l[..frames], &mut input.planes_r[..frames]);
            let mut ps = [l as &mut [f32], r as &mut [f32]];
            input.preamp.process_block_f32(&mut ps);
            input.loudness.process_block_f32(&mut ps);
        }
    }

    /// Run the secondary inputs' pre-mix chains on their own f32 planes
    /// (S1: the f64 bus path keeps secondary chains in f32 and promotes the
    /// result in the sum; f64 secondary planes are Phase 2 of the engine
    /// migration).
    fn mix_secondary_f64(&mut self, planes: &mut [&mut [f64]]) {
        let frames = planes[0].len();
        for i in 1..self.inputs.len() {
            let input = &mut self.inputs[i];
            if !input.active {
                continue;
            }
            let (l, r) = (&mut input.planes_l[..frames], &mut input.planes_r[..frames]);
            let mut ps = [l as &mut [f32], r as &mut [f32]];
            input.preamp.process_block_f32(&mut ps);
            input.loudness.process_block_f32(&mut ps);
        }
    }
}

impl DspNode for MixBusNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "mixer",
            channel_support: StageChannelSupport::StereoOnly,
            position: "pre-post-mix boundary",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.crossfade_enabled
            || self.state != MixerState::PlayingCurrent
            || self
                .inputs
                .iter()
                .any(|i| i.preamp.is_active() || i.loudness.is_active())
    }

    fn reset(&mut self) {
        // Mirrors `TrackMixer::reset` + the pipeline's pre-mix filter reset:
        // the transition envelope is torn down (state -> Silent) and the
        // secondary planes are zeroed, exactly like the pipeline's `reset`
        // which runs `mixer.reset()` (stop / track-change). Per-input USER
        // gains and balance are persistent settings and survive.
        self.state = MixerState::Silent;
        self.crossfade_pos = 0;
        for input in &mut self.inputs {
            input.preamp.reset();
            input.loudness.reset();
            input.planes_l.fill(0.0);
            input.planes_r.fill(0.0);
        }
    }

    /// Reset pre-mix filter state only, leaving the transition envelope and
    /// secondary planes intact — mirrors `DspPipeline::reset_filters_only`,
    /// which does NOT touch the mixer (seek during an active transition).
    fn reset_filters_only(&mut self) {
        for input in &mut self.inputs {
            input.preamp.reset();
            input.loudness.reset();
        }
    }

    fn prepare(&mut self, sample_rate: f32, max_channels: usize) {
        for input in &mut self.inputs {
            input.preamp.prepare(sample_rate, max_channels);
            input.loudness.prepare(sample_rate, max_channels);
        }
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        // Input 0 pre-mix in place (exactly the old OUT_PREAMP / OUT_LOUDNESS
        // steps), then the secondary inputs' pre-mix on their own planes.
        self.inputs[0].preamp.process_block_f32(planes);
        self.inputs[0].loudness.process_block_f32(planes);
        if channels == 2 {
            self.mix_secondary_f32(planes);
            self.mix_stereo(planes);
        } else {
            self.mix_multichannel(planes);
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        self.inputs[0].preamp.process_block_f64(planes);
        self.inputs[0].loudness.process_block_f64(planes);
        if channels == 2 {
            self.mix_secondary_f64(planes);
            self.mix_stereo_f64(planes);
        } else {
            self.mix_multichannel_f64(planes);
        }
    }
}
