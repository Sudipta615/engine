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
pub mod sends;
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
use sends::AuxBus;

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
    /// Per-input balance in [-1, 1] (0 = center). Shapes the front L/R pair.
    pub balance: f32,
    /// Per-input pan in [-1, 1] (Phase 4 S3): -1 hard left, 0 center,
    /// +1 hard right, shaped by [`pan_law`]. Compounds with `balance` on the
    /// front pair; pan = 0 yields a (1, 1) pair so the existing `balance`
    /// paths stay bit-exact.
    pub pan: f32,
    /// Pan law for the per-input `pan` (Phase 4 S3).
    pub pan_law: PanLaw,
    /// Peak / RMS metering accumulators for this slot (Phase 4 S3), published
    /// to the control bus once per block. Zero-alloc block scratch.
    pub(crate) meters: SlotMeters,
    /// Automation track (Phase 4 S5): generation-carried immutable
    /// breakpoints + audio-side cursor. `None` = no track, which keeps the
    /// sum bit-exact.
    pub(crate) automation: Option<SlotAutomation>,
    /// Per-channel trim (Phase 5 S1): per-channel gain / polarity applied on
    /// the slot's own planes after the pre-mix chains. All-unity = inactive
    /// = bit-exact.
    pub(crate) trim: PerChannelTrim,
    /// Send levels (Phase 5 S2): master-send + post-fader aux tap.
    pub(crate) send: SlotSend,
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
            pan: 0.0,
            pan_law: PanLaw::Linear,
            meters: SlotMeters::default(),
            automation: None,
            trim: PerChannelTrim::new(),
            send: SlotSend::new(),
            mute: false,
            active: true,
            channels: 2,
            planes: (0..MAX_CHANNELS)
                .map(|_| vec![0.0; MAX_AUDIO_BLOCK_FRAMES])
                .collect(),
        }
    }

    /// Full per-frame front-pair gains `(l, r)` for this slot's automation
    /// track at an absolute stream position, or `None` when the slot carries
    /// no (non-empty) track. For a Gain track the static balance/pan product
    /// is scaled by the automation value; for a Pan track the automation
    /// value replaces the static pan while the static balance still shapes
    /// the pair. Advancing the cursor is the caller's job (`value_at` moves
    /// it; call once per frame in stream order).
    fn full_front_gains(&mut self, absolute: usize) -> Option<(f32, f32)> {
        let auto = self.automation.as_mut()?;
        if auto.count == 0 {
            return None;
        }
        // A Send track shapes the aux tap, not the front pair — return
        // without advancing the cursor (the send sampler owns it).
        if auto.target == AutomationTarget::Send {
            return None;
        }
        let v = auto.value_at(absolute);
        match auto.target {
            AutomationTarget::Gain => {
                let (bl, br) = balance_gains(self.balance);
                let (pl, pr) = pan_gains(self.pan, self.pan_law);
                Some(((bl * pl) * v, (br * pr) * v))
            }
            AutomationTarget::Pan => {
                let (bl, br) = balance_gains(self.balance);
                let (pl, pr) = pan_gains(v, self.pan_law);
                Some((bl * pl, br * pr))
            }
            AutomationTarget::Send => unreachable!(),
        }
    }

    /// Per-frame aux-send multiplier for a `Send` automation track, or
    /// `None` when the slot carries no Send track. Only one of
    /// [`Self::full_front_gains`] / this sampler advances the cursor.
    fn automation_send_value(&mut self, absolute: usize) -> Option<f32> {
        let auto = self.automation.as_mut()?;
        if auto.count == 0 || auto.target != AutomationTarget::Send {
            return None;
        }
        Some(auto.value_at(absolute))
    }

    /// Apply this slot's per-channel trim (Phase 5 S1) to the master planes
    /// (slot 0 processes the caller's planes in place). Skipped when unity.
    fn apply_trim(&self, planes: &mut [&mut [f32]], channels: usize, frames: usize) {
        if !self.trim.is_active() {
            return;
        }
        let ch = channels.min(planes.len()).min(MAX_CHANNELS);
        for (c, plane) in planes.iter_mut().take(ch).enumerate() {
            let g = self.trim.gains[c];
            if g != 1.0 || self.trim.invert[c] {
                let sign = if self.trim.invert[c] { -g } else { g };
                for v in plane[..frames].iter_mut() {
                    *v *= sign;
                }
            }
        }
    }

    /// f64 twin of [`Self::apply_trim`]: gains are f32, cast at the multiply.
    fn apply_trim_f64(&self, planes: &mut [&mut [f64]], channels: usize, frames: usize) {
        if !self.trim.is_active() {
            return;
        }
        let ch = channels.min(planes.len()).min(MAX_CHANNELS);
        for (c, plane) in planes.iter_mut().take(ch).enumerate() {
            let g = self.trim.gains[c] as f64;
            if g != 1.0 || self.trim.invert[c] {
                let sign = if self.trim.invert[c] { -g } else { g };
                for v in plane[..frames].iter_mut() {
                    *v *= sign;
                }
            }
        }
    }
}

/// Per-input control commands (plain data; ride the SPSC control queues).
/// `SetAutomation` carries a fixed 64-point breakpoint array, so the enum is
/// large-variant by design (same `PlaybackStream` precedent).
#[derive(Clone, Copy, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum MixInputCmd {
    /// Set the linear user gain target in [0, 1].
    SetGain(f32),
    /// Set the user gain target in dB ([-60, 0]).
    SetGainDb(f32),
    /// Set the per-input balance in [-1, 1].
    SetBalance(f32),
    /// Set the per-input pan in [-1, 1] (Phase 4 S3).
    SetPan(f32),
    /// Set the per-input pan law (Phase 4 S3).
    SetPanLaw(PanLaw),
    /// Set one channel's trim (Phase 5 S1): gain in dB + polarity.
    SetSlotTrim {
        channel: usize,
        gain_db: f32,
        invert: bool,
    },
    /// Set the slot's send levels (Phase 5 S2): master-send + aux tap.
    SetSend {
        master_gain: f32,
        aux_gain: f32,
    },
    /// Replace the slot's automation track (Phase 4 S5). Fixed-array points
    /// keep this `Copy`; a track with `count == 0` is treated as cleared.
    SetAutomation {
        target: AutomationTarget,
        points: [AutomationPoint; MAX_AUTOMATION_POINTS],
        count: usize,
    },
    /// Remove the slot's automation track (Phase 4 S5).
    ClearAutomation,
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

/// Maximum number of duck targets one [`DuckState`] can address.
pub const MAX_DUCK_TARGETS: usize = 4;

/// Program-gated, block-synchronous ducking configuration (Phase 4 S4).
/// Plain `Copy` data that rides the SPSC control queues; the audio side
/// evaluates the trigger once per block from the *source* slot's peak meter
/// and ramps the duck gain toward the depth target over attack/release.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DuckState {
    /// Source slot whose peak meter gates the duck.
    pub source: usize,
    /// Source peak (dBFS) above which the duck engages.
    pub threshold_db: f32,
    /// Attenuation applied to targets (positive dB, e.g. 12.0).
    pub depth_db: f32,
    /// Ramp-down frames to full duck.
    pub attack_frames: usize,
    /// Ramp-up frames back to unity.
    pub release_frames: usize,
    /// Target slots attenuated while the duck is engaged.
    pub targets: [usize; MAX_DUCK_TARGETS],
    pub target_count: usize,
}

impl DuckState {
    /// Runtime duck gain as a linear multiplier from the current db value.
    #[inline]
    fn linear(depth_db: f32) -> f32 {
        10.0_f32.powf(-depth_db.abs() / 20.0)
    }
}

/// Maximum number of automation breakpoints a single slot track may carry.
/// The fixed array keeps `SetAutomation` `Copy` data that rides the SPSC
/// control queues without heap traffic.
pub const MAX_AUTOMATION_POINTS: usize = 64;

/// One automation breakpoint: `value` at an absolute `frame` (stream
/// samples, relative to the slot's generation start). Points must be
/// monotonically non-decreasing in `frame`; between points the value is
/// linearly interpolated, and before the first / after the last point the
/// edge value holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutomationPoint {
    pub frame: usize,
    pub value: f32,
}

/// What a slot's automation track modulates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AutomationTarget {
    /// Per-frame gain multiplier (folds multiplicatively like ducking).
    Gain,
    /// Per-frame pan position (replaces the static `pan` while the track
    /// is active; the static balance still shapes the pair).
    #[default]
    Pan,
    /// Per-frame aux-send multiplier (Phase 5 S2): modulates the slot's
    /// post-fader tap into the aux bus without touching the front pair.
    Send,
}

/// Per-slot per-channel trim (Phase 5 S1): linear gain + polarity per
/// channel on the slot's own planes, applied after the pre-mix chains and
/// before the sum. All-unity = inactive = bit-exact (the pass is skipped).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PerChannelTrim {
    pub(crate) gains: [f32; MAX_CHANNELS],
    pub(crate) invert: [bool; MAX_CHANNELS],
}

impl PerChannelTrim {
    fn new() -> Self {
        Self {
            gains: [1.0; MAX_CHANNELS],
            invert: [false; MAX_CHANNELS],
        }
    }

    /// Whether any channel deviates from unity (the pass can be skipped).
    fn is_active(&self) -> bool {
        self.gains.iter().any(|&g| g != 1.0) || self.invert.iter().any(|&b| b)
    }

    /// Apply this trim to its slot's channel-major plane views (the
    /// secondary pre-mix path, Phase 5 S1). A `PerChannelTrim` method so the
    /// caller only borrows the trim field while the views hold the planes.
    /// Skipped when unity.
    fn apply_views(&self, views: &mut [&mut [f32]], channels: usize, frames: usize) {
        if !self.is_active() {
            return;
        }
        let ch = channels.min(views.len()).min(MAX_CHANNELS);
        for (c, plane) in views.iter_mut().take(ch).enumerate() {
            let g = self.gains[c];
            if g != 1.0 || self.invert[c] {
                let sign = if self.invert[c] { -g } else { g };
                for v in plane[..frames].iter_mut() {
                    *v *= sign;
                }
            }
        }
    }

    /// Set one channel's gain (dB, clamped) and polarity.
    pub(crate) fn set_channel(&mut self, channel: usize, gain_db: f32, invert: bool) {
        if channel >= MAX_CHANNELS {
            return;
        }
        let linear = if gain_db.is_finite() {
            10.0_f32.powf(gain_db.clamp(-60.0, 24.0) / 20.0)
        } else {
            1.0
        };
        self.gains[channel] = linear;
        self.invert[channel] = invert;
    }
}

/// Per-slot send levels (Phase 5 S2): `master_gain` scales the slot's
/// contribution to the master sum (0.0 = "sends-only"); `aux_gain` is the
/// post-fader tap into the aux bus. Both at defaults (1.0 / 0.0) = bit-exact.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SlotSend {
    pub(crate) master_gain: f32,
    pub(crate) aux_gain: f32,
}

impl SlotSend {
    fn new() -> Self {
        Self {
            master_gain: 1.0,
            aux_gain: 0.0,
        }
    }
}

/// Reserved aux-bus identifier for [`DuckState`] `source` / `targets`
/// (Phase 5 S3): a source or target of `AUX_BUS_ID` addresses the aux
/// accumulator instead of a slot.
pub const AUX_BUS_ID: usize = usize::MAX;

/// A slot's automation track (Phase 4 S5): generation-carried immutable
/// breakpoints plus an audio-side cursor. The track is replaced wholesale by
/// `SetAutomation` (append-only live trims ride the same command), and the
/// runner advances the cursor monotonically as stream time moves forward.
/// Zero-alloc on the hot path; a slot with no track contributes nothing.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SlotAutomation {
    pub(crate) target: AutomationTarget,
    pub(crate) points: [AutomationPoint; MAX_AUTOMATION_POINTS],
    pub(crate) count: usize,
    /// Absolute stream position of the start of the NEXT block (advanced by
    /// the runner once per block).
    pub(crate) pos: usize,
    /// Index of the first point with `frame >= pos` (interpolation cursor).
    pub(crate) cursor: usize,
}

impl SlotAutomation {
    /// Interpolated value at an absolute stream position, advancing the
    /// cursor. Edge values hold before the first / after the last point.
    fn value_at(&mut self, absolute: usize) -> f32 {
        let n = self.count;
        if n == 0 {
            return 1.0;
        }
        let pts = &self.points[..n];
        if absolute <= pts[0].frame {
            return pts[0].value;
        }
        if absolute >= pts[n - 1].frame {
            return pts[n - 1].value;
        }
        // Monotonic scan from the cursor (stream time only moves forward).
        let mut k = self.cursor.min(n - 1);
        while k + 1 < n && pts[k + 1].frame <= absolute {
            k += 1;
        }
        while k > 0 && pts[k].frame > absolute {
            k -= 1;
        }
        self.cursor = k;
        let f0 = pts[k].frame;
        let f1 = pts[k + 1].frame;
        let v0 = pts[k].value;
        let v1 = pts[k + 1].value;
        if f1 == f0 {
            v0
        } else {
            let t = (absolute - f0) as f32 / (f1 - f0) as f32;
            v0 + (v1 - v0) * t
        }
    }
}

/// Per-slot pan law (Phase 4 S3). Affects the front L/R pair only; channels
/// ≥ 2 pass through at per-input gain.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum PanLaw {
    EqualPower,
    Linear,
    #[default]
    Center,
}

/// Per-slot peak/RMS metering accumulators (Phase 4 S3). Peak is a
/// per-channel max over the block; RMS is a one-pole envelope over the
/// per-frame channel sum. Published to the control bus once per block.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SlotMeters {
    pub(crate) peak_db: f32,
    pub(crate) rms_db: f32,
}

/// Per-slot front-pair pan gains `(l, r)` for a pan position and law. pan = 0
/// always yields `(1, 1)`, so folding this into an existing `* balance`
/// product multiplies by 1.0 (bit-exact).
#[inline]
fn pan_gains(pan: f32, law: PanLaw) -> (f32, f32) {
    if pan == 0.0 {
        return (1.0, 1.0);
    }
    let pan = pan.clamp(-1.0, 1.0);
    // Equal-power uses cos/sin of the normalized angle for a constant-power
    // drop toward center; Linear is a linear left/right taper; Center keeps
    // full gain at center and tapers both sides (a simple balance-style law).
    match law {
        PanLaw::EqualPower => {
            let t = (pan + 1.0) * 0.5; // 0..=1
            let cos_t = (std::f32::consts::FRAC_PI_2 * t).cos();
            let sin_t = (std::f32::consts::FRAC_PI_2 * t).sin();
            (cos_t, sin_t)
        }
        PanLaw::Linear => {
            if pan >= 0.0 {
                (1.0 - pan, 1.0)
            } else {
                (1.0, 1.0 + pan)
            }
        }
        PanLaw::Center => (1.0 - pan.abs(), 1.0 - pan.abs()),
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
    /// Program-gated ducking (Phase 4 S4). `None` = ducking disabled, which
    /// keeps the sum bit-exact. Runtime gain is advanced once per block by
    /// [`Self::duck_tick`] and folded into the target slots' gains.
    pub(crate) duck: Option<DuckRuntime>,
    /// Aux bus (Phase 5 S2/S3): accumulates the slots' post-fader sends and
    /// returns into the master before the post-mix chain. Disabled = bit-exact.
    pub(crate) aux: AuxBus,
}

/// Ducking configuration plus its audio-side runtime state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DuckRuntime {
    pub(crate) cfg: DuckState,
    /// Whether the trigger is currently engaged (source peak above
    /// threshold), evaluated once per block.
    pub(crate) engaged: bool,
    /// Current duck gain as a linear multiplier in [depth_linear, 1].
    pub(crate) current_linear: f32,
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
            duck: None,
            aux: AuxBus::new(),
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
            MixInputCmd::SetPan(p) => slot.pan = p.clamp(-1.0, 1.0),
            MixInputCmd::SetPanLaw(law) => slot.pan_law = law,
            MixInputCmd::SetAutomation {
                target,
                points,
                count,
            } => {
                let count = count.min(MAX_AUTOMATION_POINTS);
                slot.automation = if count == 0 {
                    None
                } else {
                    Some(SlotAutomation {
                        target,
                        points,
                        count,
                        pos: 0,
                        cursor: 0,
                    })
                };
            }
            MixInputCmd::ClearAutomation => slot.automation = None,
            MixInputCmd::SetSlotTrim {
                channel,
                gain_db,
                invert,
            } => slot.trim.set_channel(channel, gain_db, invert),
            MixInputCmd::SetSend {
                master_gain,
                aux_gain,
            } => {
                slot.send.master_gain = master_gain.clamp(0.0, 1.0);
                slot.send.aux_gain = aux_gain.clamp(0.0, 1.0);
            }
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

    /// Compute per-slot peak/RMS metering over the `frames` just processed
    /// (Phase 4 S3). Peak is the max |sample| over the slot's channels in
    /// dBFS; RMS is the block-windowed RMS of the per-frame channel sum.
    /// Slot 0 processes the caller's master planes in place (its secondary
    /// storage is never fed), so it is metered from `master`; inputs >= 1
    /// are metered from their own pre-mixed planes. Stored into each slot's
    /// `meters` for the graph shell to publish. Deterministic and zero-alloc.
    pub(super) fn compute_meters(&mut self, master: &[&mut [f32]], frames: usize) {
        if frames == 0 {
            return;
        }
        for (i, input) in self.inputs.iter_mut().enumerate() {
            let (peak, mean_sq) = if i == 0 {
                let ch = master.len().max(1);
                let mut peak = 0.0f32;
                let mut sum_sq = 0.0f32;
                for plane in master.iter().take(ch) {
                    for &v in plane.iter().take(frames) {
                        let a = v.abs();
                        if a > peak {
                            peak = a;
                        }
                        sum_sq += v * v;
                    }
                }
                (peak, sum_sq / (ch as f32 * frames as f32))
            } else {
                let ch = input.channels.clamp(1, input.planes.len());
                let mut peak = 0.0f32;
                let mut sum_sq = 0.0f32;
                for plane in input.planes.iter().take(ch) {
                    for &v in plane.iter().take(frames) {
                        let a = v.abs();
                        if a > peak {
                            peak = a;
                        }
                        sum_sq += v * v;
                    }
                }
                (peak, sum_sq / (ch as f32 * frames as f32))
            };
            let eps = 1e-12f32;
            let peak_db = 20.0 * (peak.max(eps)).log10();
            let rms_db = 20.0 * (mean_sq.max(eps).sqrt()).log10();
            input.meters = SlotMeters { peak_db, rms_db };
        }
    }

    /// f64 twin of [`Self::compute_meters`]: slot 0 is metered from the f64
    /// master planes (cast per sample), inputs >= 1 from their own f32
    /// planes. Zero-alloc on the hot path.
    pub(super) fn compute_meters_f64(&mut self, master: &[&mut [f64]], frames: usize) {
        if frames == 0 {
            return;
        }
        for (i, input) in self.inputs.iter_mut().enumerate() {
            let (peak, mean_sq) = if i == 0 {
                let ch = master.len().max(1);
                let mut peak = 0.0f32;
                let mut sum_sq = 0.0f64;
                for plane in master.iter().take(ch) {
                    for &v in plane.iter().take(frames) {
                        let a = v.abs() as f32;
                        if a > peak {
                            peak = a;
                        }
                        sum_sq += v * v;
                    }
                }
                (peak, (sum_sq / (ch as f64 * frames as f64)) as f32)
            } else {
                let ch = input.channels.clamp(1, input.planes.len());
                let mut peak = 0.0f32;
                let mut sum_sq = 0.0f32;
                for plane in input.planes.iter().take(ch) {
                    for &v in plane.iter().take(frames) {
                        let a = v.abs();
                        if a > peak {
                            peak = a;
                        }
                        sum_sq += v * v;
                    }
                }
                (peak, sum_sq / (ch as f32 * frames as f32))
            };
            let eps = 1e-12f32;
            let peak_db = 20.0 * (peak.max(eps)).log10();
            let rms_db = 20.0 * (mean_sq.max(eps).sqrt()).log10();
            input.meters = SlotMeters { peak_db, rms_db };
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

    /// Apply a ducking configuration (Phase 4 S4). `None` disables ducking.
    /// Control path (queued command); the runtime starts disengaged at unity.
    pub fn apply_duck(&mut self, cfg: Option<DuckState>) {
        self.duck = cfg.map(|cfg| DuckRuntime {
            engaged: false,
            current_linear: 1.0,
            cfg,
        });
    }

    /// Apply the aux bus config (Phase 5 S2/S3). Control path (queued
    /// command); the accumulator is only cleared/tapped/returned while
    /// `enabled`, so disabling is bit-exact.
    pub fn apply_aux(&mut self, enabled: bool, return_gain: f32) {
        self.aux.enabled = enabled;
        self.aux.return_gain = return_gain.clamp(0.0, 1.0);
    }

    /// Apply the Phase-6 aux insert (a global convolution between the
    /// accumulator and the return). Control path; the IR file load happens
    /// here. `ir_path: None` keeps the loaded IR (enabled/wet change only).
    pub fn apply_aux_insert(
        &mut self,
        enabled: bool,
        wet_mix: f32,
        sample_rate: f32,
        ir_path: Option<&str>,
    ) {
        self.aux
            .apply_insert(enabled, wet_mix, sample_rate, ir_path);
    }

    /// Runtime toggle of the Phase-6 aux insert (enabled / wet only — the
    /// IR stays as configured). Control path (queued command).
    pub fn set_aux_insert(&mut self, enabled: bool, wet_mix: f32) {
        self.aux.set_insert(enabled, wet_mix);
    }

    /// Advance the duck envelope once per block (Phase 4 S4). `source_peak_db`
    /// is the trigger slot's peak from this block's metering; the trigger is
    /// evaluated block-synchronously and the gain ramps toward the depth
    /// target over attack/release frames. Zero-alloc, disabled is a no-op.
    pub(super) fn duck_tick(&mut self, frames: usize) {
        let Some(duck) = &mut self.duck else {
            return;
        };
        let target_linear = DuckState::linear(duck.cfg.depth_db);
        let threshold = duck.cfg.threshold_db;
        // Phase 5 S3: a source of AUX_BUS_ID reads the aux accumulator's
        // meter instead of a slot's.
        let source_peak_db = if duck.cfg.source == AUX_BUS_ID {
            self.aux.meters.peak_db
        } else {
            self.inputs
                .get(duck.cfg.source)
                .map(|s| s.meters.peak_db)
                .unwrap_or(-96.0)
        };
        let engaged = source_peak_db > threshold;
        // One-pole step toward the target over the block: after
        // attack/release frames the gain reaches the target.
        let step = if engaged {
            if duck.cfg.attack_frames > 0 {
                (frames as f32 / duck.cfg.attack_frames as f32).min(1.0)
            } else {
                1.0
            }
        } else if duck.cfg.release_frames > 0 {
            (frames as f32 / duck.cfg.release_frames as f32).min(1.0)
        } else {
            1.0
        };
        let goal = if engaged { target_linear } else { 1.0 };
        duck.current_linear += (goal - duck.current_linear) * step;
        duck.engaged = engaged;
    }

    /// Duck gain (linear multiplier) for `slot`, 1.0 when ducking is disabled
    /// or the slot is not a target. Called per block, folded into the slot's
    /// gain in the sums.
    pub(super) fn duck_gain_for(&self, slot: usize) -> f32 {
        let Some(duck) = &self.duck else {
            return 1.0;
        };
        if duck.cfg.targets[..duck.cfg.target_count].contains(&slot) {
            duck.current_linear
        } else {
            1.0
        }
    }

    /// Duck gain for the aux return (Phase 5 S3): `current_linear` when the
    /// aux bus is a duck target, 1.0 otherwise.
    pub(super) fn aux_duck_gain(&self) -> f32 {
        self.duck_gain_for(AUX_BUS_ID)
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
            || self.duck.is_some()
            || self.aux.enabled
            || self.aux.insert_active()
            || self.inputs.iter().any(|i| {
                i.preamp.is_active()
                    || i.loudness.is_active()
                    || i.automation.is_some()
                    || i.trim.is_active()
                    || i.send.master_gain != 1.0
                    || i.send.aux_gain != 0.0
            })
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
        let frames = planes[0].len();
        // Input 0 pre-mix in place (exactly the old OUT_PREAMP / OUT_LOUDNESS
        // steps), then its per-channel trim (Phase 5 S1), then the secondary
        // inputs' pre-mix on their own planes.
        self.inputs[0].preamp.process_block_f32(planes);
        self.inputs[0].loudness.process_block_f32(planes);
        self.inputs[0].apply_trim(planes, channels, frames);
        if self.aux.enabled {
            self.aux.clear(frames);
        }
        if channels == 2 {
            self.mix_secondary_f32(planes);
            self.mix_stereo(planes);
        } else {
            self.mix_multichannel(planes);
        }
        // Aux return after all slot sums (the aux content joins the master
        // before the downstream post-mix chain), then the per-slot meters.
        self.aux.return_into(planes, frames, self.aux_duck_gain());
        self.compute_meters(planes, frames);
        self.aux.compute_meters(frames);
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        let frames = planes[0].len();
        self.inputs[0].preamp.process_block_f64(planes);
        self.inputs[0].loudness.process_block_f64(planes);
        self.inputs[0].apply_trim_f64(planes, channels, frames);
        if self.aux.enabled {
            self.aux.clear(frames);
        }
        if channels == 2 {
            self.mix_secondary_f64(planes);
            self.mix_stereo_f64(planes);
        } else {
            self.mix_multichannel_f64(planes);
        }
        self.aux
            .return_into_f64(planes, frames, self.aux_duck_gain());
        self.compute_meters_f64(planes, frames);
        self.aux.compute_meters(frames);
    }
}
