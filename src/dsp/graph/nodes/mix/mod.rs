//! Phase 3 S1 — the mix bus node, split by concern (Phase 4 S1).
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
//! Phase 4 S1 parameterizes the bus: the **slot count** is fixed per
//! generation ([`MixBusNode::with_slots`], fed from
//! `EngineConfig::mix_slots` at construction), so a generation carrying N
//! simultaneous streams is a plain generation rebuild. The split keeps the
//! TrackMixer-compatible pair law in [`envelope`] and the stereo /
//! multichannel sums in [`sum`]; the 27-scenario equivalence suite pins the
//! 2-slot bit-exact contract.
//!
//! Realtime contract: the node owns one pair of secondary-input planes per
//! input (preallocated at construction), advances every input's state
//! exactly once per processed block, and performs no allocation on the hot
//! path. S1 sums secondary inputs into the stereo front pair only;
//! multichannel bus mixing is Phase 4 S2.

pub mod envelope;
pub mod sum;

use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::dsp::{
    crossfade::{CrossfadeCurve, MixerState},
    gain::GainProcessor,
    graph::node::DspNode,
    loudness::{LoudnessMetadata, LoudnessMode},
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

use super::{GainNode, LoudnessNode};

/// Upper bound on the number of mix-bus slots one generation can carry.
/// Kept modest so the control bus's per-slot sticky atomics and the arena's
/// per-input planes stay small; raising it only costs control-path memory.
pub const MAX_MIX_SLOTS: usize = 8;

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
    /// Secondary-input plane storage: channel-major, `MAX_CHANNELS` planes
    /// of [`MAX_AUDIO_BLOCK_FRAMES`] frames each, preallocated at
    /// construction (Phase 4 S2 makes every slot N-channel-capable; stereo
    /// sources use `planes[0]` / `planes[1]`). The graph's process driver
    /// feeds these; the node processes them. Zero allocation on the hot path.
    pub(crate) channels: usize,
    pub(crate) planes: Vec<Vec<f32>>,
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
            channels: 2,
            planes: (0..MAX_CHANNELS)
                .map(|_| vec![0.0; MAX_AUDIO_BLOCK_FRAMES])
                .collect(),
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
    /// Construct the canonical two-input bus (outgoing / incoming). The
    /// envelope starts in [`MixerState::PlayingCurrent`]: input 0 passes
    /// through (after its pre-mix) exactly as the old `OutPreamp` /
    /// `OutLoudness` steps did.
    pub fn new(
        sample_rate: f32,
        crossfade_duration_ms: u64,
        crossfade_enabled: bool,
        curve: config::CrossfadeCurve,
        preamp_ramp_ms: f32,
    ) -> Self {
        Self::with_slots(
            2,
            sample_rate,
            crossfade_duration_ms,
            crossfade_enabled,
            curve,
            preamp_ramp_ms,
        )
    }

    /// Construct a bus with `slots` inputs (clamped to `[2, MAX_MIX_SLOTS]`),
    /// the Phase 4 S1 generation parameter: a generation carrying N
    /// simultaneous streams is built with N slots. Slots 0/1 are the
    /// transition pair (`out_preamp`/`in_preamp` chains); slots ≥ 2 are
    /// independent lanes summed after the pair envelope. All slots default
    /// to unity gain, center balance, unmuted, and active.
    pub fn with_slots(
        slots: usize,
        sample_rate: f32,
        crossfade_duration_ms: u64,
        crossfade_enabled: bool,
        curve: config::CrossfadeCurve,
        preamp_ramp_ms: f32,
    ) -> Self {
        let slots = slots.clamp(2, MAX_MIX_SLOTS);
        let mut inputs = Vec::with_capacity(slots);
        for k in 0..slots {
            let (pname, lname) = match k {
                0 => ("out_preamp", "out_loudness"),
                1 => ("in_preamp", "in_loudness"),
                _ => ("lane_preamp", "lane_loudness"),
            };
            inputs.push(MixInput::new(pname, lname, preamp_ramp_ms, sample_rate));
        }
        Self {
            inputs,
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
            for plane in &mut input.planes {
                plane.fill(0.0);
            }
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
