//! Symmetric control surface for [`DspGraph`] — now queued.
//!
//! Phase 2: control methods enqueue a plain-data [`NodeCmd`] into a per-node
//! SPSC queue ([`ControlBus`]) instead of mutating nodes directly. The audio
//! thread drains every queue at the block boundary
//! ([`DspGraph::control_tick`]), so a command applies deterministically at
//! the start of the next processed block. Heap-bearing operations (IR loads,
//! full config application) travel via the generation-swap path
//! ([`GraphControlHandle::publish_generation`]) instead — `NodeCmd` is
//! strictly `Copy` and allocation-free by construction.
//!
//! The [`ControlBus`] is the only part of the graph shared across threads
//! (SPSC queues + the publish/swap/retire atomics). A
//! [`GraphControlHandle`] clones it out for use from the engine's control
//! thread while the audio thread owns the [`DspGraph`]; in single-threaded
//! use the `DspGraph` control methods (below) delegate to the same handle.

use super::swap::NodeId;
use super::*;
use crate::buffer::PcmRingBuffer;
use crate::dsp::{
    crossfade::MixerState,
    equalizer::EqBandParams,
    limiter::LimiterMode,
    loudness::{LoudnessMetadata, LoudnessMode},
};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// Depth of each per-node control queue. Bounded, so the block-boundary
/// drain is O(QUEUE_CAPACITY) per queue — deterministic and budgetable.
const CONTROL_QUEUE_CAPACITY: usize = 64;

/// One parameter / transport command carried by the control queues.
///
/// Plain data only (`Copy`, no heap, no `Vec`/`String`): heap-bearing
/// operations belong to the generation-swap path, never to a queue.
/// Variants mirror the Phase-1 symmetric control surface one-to-one.
#[derive(Clone, Copy, Debug)]
pub(crate) enum NodeCmd {
    // ── Shell (transport / top-level) ─────────────────────────────────────
    SetBitPerfect(bool),
    SetDoPBypass(bool),
    SetSpeed(f32),

    // ── Volume / balance ──────────────────────────────────────────────────
    SetVolumeTarget(f32),
    SetBalance(f32),

    // ── Seek / transition fades ───────────────────────────────────────────
    BeginSeekFadeout,
    BeginSeekFadein,

    // ── Mix bus (per-input chains + transitions) ──────────────────────────
    MixInput {
        input: u8,
        cmd: MixInputCmd,
    },
    MixTransition(MixTransitionCmd),
    /// Runtime crossfade config (Phase 3 S3): curve / enabled / duration
    /// mirror the pipeline's `TrackMixer` setters the engine calls on
    /// `handle_set_crossfade_config`.
    SetMixCurve(config::CrossfadeCurve),
    SetMixEnabled(bool),
    SetMixDurationFrames(usize),

    // ── Limiter ───────────────────────────────────────────────────────────
    SetLimiterEnabled(bool),
    SetLimiterMode(LimiterMode),
    SetLimiterParams {
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool,
    },
    SetLimiterTruePeak(bool),

    // ── EQ ────────────────────────────────────────────────────────────────
    SetEqEnabled(bool),
    SetEqAutoHeadroom(bool),
    SetEqPreampDb(f32),
    SetEqBassShelf(f32),
    SetEqTrebleShelf(f32),
    SetEqBand {
        index: usize,
        params: EqBandParams,
    },
    SetMidsideEq(bool),

    // ── Convolution ───────────────────────────────────────────────────────
    SetConvolutionWetMix(f32),

    // ── Stereo image ──────────────────────────────────────────────────────
    SetStereoWidth(f32),
    SetStereoEnhancerEnabled(bool),

    // ── Crossfeed ─────────────────────────────────────────────────────────
    SetCrossfeedEnabled(bool),
    SetCrossfeedProfile(config::CrossfeedProfile),
    SetCrossfeedCustom(f32, f32, f32),

    // ── Multiband compressor ──────────────────────────────────────────────
    SetCompressorEnabled(bool),
    SetCompressorBand {
        band: usize,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    },
    SetCompressorBandFeatures {
        band: usize,
        knee_db: f32,
        detector: config::CompressorDetector,
        stereo_link: bool,
    },
}

impl Default for NodeCmd {
    /// Placeholder used to fill queue storage; never observed unless a
    /// producer pushes garbage (it cannot).
    fn default() -> Self {
        NodeCmd::SetVolumeTarget(1.0)
    }
}

/// The cross-thread control plane: per-node SPSC queues, the
/// publish/swap/retire atomics, and the sticky user-state snapshot.
///
/// Producer (control) writes queues + `has_pending`; consumer (audio) drains
/// them and, on drain, mirrors user-state (volume / balance / speed) onto the
/// sticky atomics so a later generation build can inherit it.
pub(crate) struct ControlBus {
    /// One SPSC ring per node slot (canonical `NodeId`s), plus the shell slot
    /// ([`NodeId::SHELL`]).
    queues: Vec<PcmRingBuffer<NodeCmd>>,
    /// Set on any enqueue; the audio side swaps it false at the block
    /// boundary so a quiet block costs one relaxed load, not 18.
    has_pending: AtomicBool,
    /// Control → audio: a fully built generation awaiting the next swap.
    pending: AtomicPtr<GraphGeneration>,
    /// Audio → control: the generation swapped out, awaiting reclamation.
    retired: AtomicPtr<GraphGeneration>,
    /// Monotonic generation counter (audio-incremented on swap).
    swap_seq: AtomicU64,
    /// Generations reclaimed by the control side.
    reclaimed: AtomicU64,
    /// Commands dropped on a full queue.
    dropped: AtomicU64,
    /// Sticky user state (audio-written at drain, control-read at build).
    user_volume: AtomicU32,
    user_balance: AtomicU32,
    user_speed: AtomicU32,
    user_fade_ms: AtomicU32,
}

impl ControlBus {
    pub(super) fn new(volume_fade_ms: f32) -> Self {
        Self {
            queues: (0..NodeId::SLOTS)
                .map(|_| PcmRingBuffer::<NodeCmd>::new(CONTROL_QUEUE_CAPACITY))
                .collect(),
            has_pending: AtomicBool::new(false),
            pending: AtomicPtr::new(std::ptr::null_mut()),
            retired: AtomicPtr::new(std::ptr::null_mut()),
            swap_seq: AtomicU64::new(0),
            reclaimed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            user_volume: AtomicU32::new(1.0f32.to_bits()),
            user_balance: AtomicU32::new(0.0f32.to_bits()),
            user_speed: AtomicU32::new(1.0f32.to_bits()),
            user_fade_ms: AtomicU32::new(volume_fade_ms.to_bits()),
        }
    }

    /// Enqueue a command into one queue slot (control path). Drops (and
    /// counts) on a full queue, mirroring the engine's fire-and-forget
    /// `send_command` posture.
    #[inline]
    fn enqueue(&self, slot: usize, cmd: NodeCmd) {
        if self.queues[slot].free_slots() == 0 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.queues[slot].push_block(std::slice::from_ref(&cmd));
        self.has_pending.store(true, Ordering::Release);
    }

    /// Control-side sticky user-state reads (generation seeding).
    pub(super) fn user_volume(&self) -> f32 {
        f32::from_bits(self.user_volume.load(Ordering::Relaxed))
    }
    pub(super) fn user_balance(&self) -> f32 {
        f32::from_bits(self.user_balance.load(Ordering::Relaxed))
    }
    pub(super) fn user_speed(&self) -> f32 {
        f32::from_bits(self.user_speed.load(Ordering::Relaxed))
    }
    pub(super) fn user_fade_ms(&self) -> f32 {
        f32::from_bits(self.user_fade_ms.load(Ordering::Relaxed))
    }

    /// Update the sticky volume-ramp duration (audio side: called by the
    /// direct `set_volume_fade_ms` setter so future generation builds honor
    /// it).
    pub(super) fn set_user_fade_ms(&self, ms: f32) {
        self.user_fade_ms.store(ms.to_bits(), Ordering::Relaxed);
    }

    /// Atomic snapshot of the current user state, for seeding a generation
    /// build on the control side ([`GraphGeneration::build_with_state`]).
    pub(super) fn snapshot(&self) -> super::swap::UserState {
        super::swap::UserState {
            volume: self.user_volume(),
            balance: self.user_balance(),
            speed: self.user_speed(),
            volume_fade_ms: self.user_fade_ms(),
        }
    }
}

/// The control side of the graph: enqueue commands and publish generations
/// from any thread that holds this handle (the engine's tick thread, or a
/// test's control thread). Clone is cheap (shares the [`ControlBus`]).
#[derive(Clone)]
pub struct GraphControlHandle {
    bus: Arc<ControlBus>,
}

impl GraphControlHandle {
    pub(super) fn enqueue(&self, slot: usize, cmd: NodeCmd) {
        self.bus.enqueue(slot, cmd);
    }

    /// Publish a fully-built generation for the audio thread to swap in at
    /// the next block boundary. Reclaims any generation returned by the
    /// audio thread first, then coalesces: an earlier pending (not yet
    /// swapped) generation is dropped in favor of this one — config is
    /// stateful, "latest wins".
    ///
    /// This is the cross-thread control path: build with
    /// [`GraphGeneration::from_config`] (allocation is fine on the control
    /// side) and publish from any thread holding this handle. The audio
    /// thread executes the swap with zero allocation. For same-thread
    /// reconfiguration, [`DspGraph::reconfigure`] wraps build + publish and
    /// preserves live user state.
    pub fn publish_generation(&self, gen: Box<GraphGeneration>) {
        self.reclaim_retired();
        let prev = self.bus.pending.swap(Box::into_raw(gen), Ordering::AcqRel);
        if !prev.is_null() {
            // Reclaim the coalesced, never-swapped generation.
            unsafe {
                drop(Box::from_raw(prev));
            }
        }
    }

    /// Reclaim (free) a generation returned by the audio thread. Called
    /// implicitly by [`Self::publish_generation`]; exposed for tests that
    /// assert the reclamation discipline without publishing.
    pub fn reclaim_retired(&self) {
        let r = self
            .bus
            .retired
            .swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !r.is_null() {
            unsafe {
                drop(Box::from_raw(r));
            }
            self.bus.reclaimed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Number of swaps the audio thread has performed (monotonic).
    pub fn generation(&self) -> u64 {
        self.bus.swap_seq.load(Ordering::Acquire)
    }

    /// Number of generations reclaimed by the control side.
    pub fn reclaimed_count(&self) -> u64 {
        self.bus.reclaimed.load(Ordering::Relaxed)
    }

    /// Number of commands dropped on full queues.
    pub fn dropped_commands(&self) -> u64 {
        self.bus.dropped.load(Ordering::Relaxed)
    }

    // ── Control surface (enqueue) ─────────────────────────────────────────

    pub fn set_volume(&self, volume: f32) {
        self.enqueue(
            node_id::VOLUME,
            NodeCmd::SetVolumeTarget(volume.clamp(0.0, 1.0)),
        );
    }

    pub fn set_volume_db(&self, db: f32) {
        if !db.is_finite() {
            log::warn!("DspGraph::set_volume_db: non-finite value {}; ignoring", db);
            return;
        }
        let linear = DspGraph::volume_db_to_linear(db);
        self.enqueue(node_id::VOLUME, NodeCmd::SetVolumeTarget(linear));
    }

    pub fn set_balance(&self, balance: f32) {
        self.enqueue(node_id::BALANCE, NodeCmd::SetBalance(balance));
    }

    pub fn begin_seek_fadeout(&self) {
        self.enqueue(node_id::SEEK_FADE, NodeCmd::BeginSeekFadeout);
    }

    pub fn begin_seek_fadein(&self) {
        self.enqueue(node_id::SEEK_FADE, NodeCmd::BeginSeekFadein);
    }

    pub fn apply_loudness_metadata_outgoing(&self, metadata: Option<LoudnessMetadata>) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input: 0,
                cmd: MixInputCmd::ApplyLoudnessMetadata(metadata.unwrap_or_default()),
            },
        );
    }

    pub fn apply_loudness_metadata_incoming(&self, metadata: Option<LoudnessMetadata>) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input: 1,
                cmd: MixInputCmd::ApplyLoudnessMetadata(metadata.unwrap_or_default()),
            },
        );
    }

    pub fn set_limiter_enabled(&self, enabled: bool) {
        self.enqueue(node_id::LIMITER, NodeCmd::SetLimiterEnabled(enabled));
    }

    pub fn set_limiter_mode(&self, mode: LimiterMode) {
        self.enqueue(node_id::LIMITER, NodeCmd::SetLimiterMode(mode));
    }

    pub fn set_limiter_params(
        &self,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool,
    ) {
        self.enqueue(
            node_id::LIMITER,
            NodeCmd::SetLimiterParams {
                lookahead_ms,
                attack_ms,
                release_ms,
                ceiling_db,
                soft_clip,
            },
        );
    }

    pub fn set_limiter_true_peak(&self, enabled: bool) {
        self.enqueue(node_id::LIMITER, NodeCmd::SetLimiterTruePeak(enabled));
    }

    pub fn set_preamp_db(&self, db: f32) {
        self.enqueue(node_id::EQ, NodeCmd::SetEqPreampDb(db));
    }

    pub fn set_bass_shelf(&self, gain_db: f32) {
        self.enqueue(node_id::EQ, NodeCmd::SetEqBassShelf(gain_db));
    }

    pub fn set_treble_shelf(&self, gain_db: f32) {
        self.enqueue(node_id::EQ, NodeCmd::SetEqTrebleShelf(gain_db));
    }

    pub fn set_eq_enabled(&self, enabled: bool) {
        self.enqueue(node_id::EQ, NodeCmd::SetEqEnabled(enabled));
    }

    pub fn set_eq_auto_headroom(&self, enabled: bool) {
        self.enqueue(node_id::EQ, NodeCmd::SetEqAutoHeadroom(enabled));
    }

    pub fn set_eq_band(&self, index: usize, params: EqBandParams) {
        self.enqueue(node_id::EQ, NodeCmd::SetEqBand { index, params });
    }

    pub fn set_midside_eq(&self, enabled: bool) {
        self.enqueue(node_id::EQ, NodeCmd::SetMidsideEq(enabled));
    }

    pub fn set_convolution_wet_mix(&self, mix: f32) {
        self.enqueue(node_id::CONVOLUTION, NodeCmd::SetConvolutionWetMix(mix));
    }

    pub fn set_stereo_width(&self, width: f32) {
        self.enqueue(node_id::STEREO, NodeCmd::SetStereoWidth(width));
    }

    pub fn set_stereo_enhancer_enabled(&self, enabled: bool) {
        self.enqueue(node_id::STEREO, NodeCmd::SetStereoEnhancerEnabled(enabled));
    }

    pub fn set_crossfeed_enabled(&self, enabled: bool) {
        self.enqueue(node_id::CROSSFEED, NodeCmd::SetCrossfeedEnabled(enabled));
    }

    pub fn set_crossfeed_profile(&self, profile: config::CrossfeedProfile) {
        self.enqueue(node_id::CROSSFEED, NodeCmd::SetCrossfeedProfile(profile));
    }

    pub fn set_crossfeed_custom_params(&self, frequency_hz: f32, q: f32, delay_ms: f32) {
        self.enqueue(
            node_id::CROSSFEED,
            NodeCmd::SetCrossfeedCustom(frequency_hz, q, delay_ms),
        );
    }

    pub fn set_compressor_enabled(&self, enabled: bool) {
        self.enqueue(node_id::DYNAMICS, NodeCmd::SetCompressorEnabled(enabled));
    }

    pub fn set_compressor_band_params(
        &self,
        band: usize,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    ) {
        self.enqueue(
            node_id::DYNAMICS,
            NodeCmd::SetCompressorBand {
                band,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_gain_db,
            },
        );
    }

    pub fn set_compressor_band_features(
        &self,
        band: usize,
        knee_db: f32,
        detector: config::CompressorDetector,
        stereo_link: bool,
    ) {
        self.enqueue(
            node_id::DYNAMICS,
            NodeCmd::SetCompressorBandFeatures {
                band,
                knee_db,
                detector,
                stereo_link,
            },
        );
    }

    pub fn set_loudness_mode(&self, mode: LoudnessMode) {
        let cmd = MixInputCmd::SetLoudnessMode(mode);
        self.enqueue(node_id::MIX, NodeCmd::MixInput { input: 0, cmd });
        self.enqueue(node_id::MIX, NodeCmd::MixInput { input: 1, cmd });
    }

    // ── Mix bus: per-input control (Phase 3 S1) ───────────────────────────

    pub fn set_input_gain(&self, input: u8, gain: f32) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetGain(gain),
            },
        );
    }

    pub fn set_input_gain_db(&self, input: u8, db: f32) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetGainDb(db),
            },
        );
    }

    pub fn set_input_balance(&self, input: u8, balance: f32) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetBalance(balance),
            },
        );
    }

    pub fn set_input_mute(&self, input: u8, mute: bool) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetMute(mute),
            },
        );
    }

    /// Detach / re-attach a mix-bus slot (Phase 3 S2 stream slots). A
    /// detached slot contributes nothing and its chains do not advance.
    /// Slot 0 (the primary stream) cannot be detached.
    pub fn set_input_active(&self, input: u8, active: bool) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetActive(active),
            },
        );
    }

    /// Begin a crossfade from input 0 to input 1 over `duration_frames`.
    /// Gated by the graph's crossfade config (mirrors `TrackMixer`).
    pub fn begin_crossfade_frames(&self, duration_frames: usize) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixTransition(MixTransitionCmd::StartCrossfade { duration_frames }),
        );
    }

    /// Begin a sequential fade (fade-out → gap → fade-in) over
    /// `duration_frames`.
    pub fn begin_fade_frames(&self, duration_frames: usize) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixTransition(MixTransitionCmd::StartFade { duration_frames }),
        );
    }

    /// Return the bus to ordinary single-stream playback (input 0 at unity).
    pub fn begin_playing(&self) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixTransition(MixTransitionCmd::StartPlaying),
        );
    }

    /// Set the transition curve at runtime (mirrors
    /// `TrackMixer::set_curve`).
    pub fn set_crossfade_curve(&self, curve: config::CrossfadeCurve) {
        self.enqueue(node_id::MIX, NodeCmd::SetMixCurve(curve));
    }

    /// Enable / disable crossfade transitions (mirrors
    /// `TrackMixer::set_enabled`).
    pub fn set_crossfade_enabled(&self, enabled: bool) {
        self.enqueue(node_id::MIX, NodeCmd::SetMixEnabled(enabled));
    }

    /// Set the transition duration in output frames (mirrors
    /// `TrackMixer::set_duration_ms` converted at the graph's sample rate).
    pub fn set_crossfade_duration_frames(&self, frames: usize) {
        self.enqueue(node_id::MIX, NodeCmd::SetMixDurationFrames(frames));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Audio side: block-boundary drain + swap, and the DspGraph control surface.
// ─────────────────────────────────────────────────────────────────────────────

/// Apply one shell-level (non-node) command to the graph's top-level state.
impl DspGraph {
    fn apply_shell_cmd(&mut self, cmd: NodeCmd) {
        match cmd {
            NodeCmd::SetBitPerfect(enabled) => {
                self.bit_perfect = enabled;
                if enabled {
                    self.volume_mut().processor.set_gain(1.0);
                    self.volume_mut().processor.snap();
                    self.seek_fade_mut().fade.reset();
                }
            }
            NodeCmd::SetDoPBypass(enabled) => self.dop_bypass = enabled,
            NodeCmd::SetSpeed(speed) => {
                let speed = speed.clamp(0.25, 4.0);
                self.speed = speed;
                self.timestretch_mut().stretcher.set_speed(speed);
            }
            _ => {}
        }
    }

    /// Drain every control queue into the active generation and handle a
    /// pending generation swap. Called once per caller block, before any
    /// signal processing and before the transport-bypass early returns, so
    /// control application is never skipped by bypass contracts.
    /// Drain queued control commands and any pending generation swap NOW.
    ///
    /// The engine is single-threaded (its tick drives both command dispatch
    /// and the decode loop), so it applies commands immediately after
    /// dispatch instead of waiting for the next audio block. Safe only when
    /// no other thread is processing audio; multi-threaded hosts must rely
    /// on the block-boundary [`Self::control_tick`] instead.
    pub fn drain_queued_control(&mut self) {
        self.control_tick();
    }

    pub(crate) fn control_tick(&mut self) {
        if self.bus.has_pending.swap(false, Ordering::AcqRel) {
            self.drain_control();
        }
        let pending = self
            .bus
            .pending
            .swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !pending.is_null() {
            let new_gen = unsafe { Box::from_raw(pending) };
            let prev = std::mem::replace(&mut self.active, new_gen);
            self.bus
                .retired
                .store(Box::into_raw(prev), Ordering::Release);
            self.bus.swap_seq.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// FIFO-per-node drain. Shell commands first, then per-node commands in
    /// arena order (a deterministic total order). Bounded work: at most
    /// [`CONTROL_QUEUE_CAPACITY`] pops per queue.
    fn drain_control(&mut self) {
        let mut buf = [NodeCmd::default(); CONTROL_QUEUE_CAPACITY];

        // Shell queue first.
        let n = self.bus.queues[NodeId::SHELL.0].pop_block(&mut buf);
        for cmd in &buf[..n] {
            self.apply_shell_cmd(*cmd);
        }

        // Per-node queues in arena order. `bus` and `active` are disjoint
        // fields, so mirroring user state and mutating the node coexist.
        for i in 0..self.active.nodes.len() {
            let id = self.active.node_ids[i];
            let n = self.bus.queues[id.0].pop_block(&mut buf);
            for cmd in &buf[..n] {
                match cmd {
                    NodeCmd::SetVolumeTarget(v) => {
                        self.bus.user_volume.store(v.to_bits(), Ordering::Relaxed)
                    }
                    NodeCmd::SetBalance(b) => {
                        self.bus.user_balance.store(b.to_bits(), Ordering::Relaxed)
                    }
                    NodeCmd::SetSpeed(s) => {
                        self.bus.user_speed.store(s.to_bits(), Ordering::Relaxed)
                    }
                    _ => {}
                }
                apply_node_cmd(&mut self.active.nodes[i], cmd);
            }
        }
    }

    /// Number of swaps performed so far (audio-side monotonic counter).
    pub fn generation(&self) -> u64 {
        self.bus.swap_seq.load(Ordering::Acquire)
    }

    /// A cloneable handle to this graph's control plane, for use from
    /// another thread (the engine's tick thread, or tests).
    pub fn control_handle(&self) -> GraphControlHandle {
        GraphControlHandle {
            bus: self.bus.clone(),
        }
    }
}

/// Dispatch one command onto its target node kind. The queue slot identifies
/// the node; a wrong-kind command for a slot is a programming error and is
/// ignored (debug-asserted).
fn apply_node_cmd(node: &mut GraphNode, cmd: &NodeCmd) {
    match (&mut *node, cmd) {
        (GraphNode::Volume(n), NodeCmd::SetVolumeTarget(v)) => n.processor.set_gain(*v),
        (GraphNode::Balance(n), NodeCmd::SetBalance(b)) => n.set_balance(*b),
        (GraphNode::SeekFade(n), NodeCmd::BeginSeekFadeout) => n.fade.fade_out(),
        (GraphNode::SeekFade(n), NodeCmd::BeginSeekFadein) => n.fade.fade_in(),
        (GraphNode::Mix(n), NodeCmd::MixInput { input, cmd }) => {
            n.apply_input(*input as usize, *cmd)
        }
        (GraphNode::Mix(n), NodeCmd::MixTransition(cmd)) => n.apply_transition(*cmd),
        (GraphNode::Mix(n), NodeCmd::SetMixCurve(c)) => n.curve = (*c).into(),
        (GraphNode::Mix(n), NodeCmd::SetMixEnabled(e)) => n.crossfade_enabled = *e,
        (GraphNode::Mix(n), NodeCmd::SetMixDurationFrames(f)) => {
            n.crossfade_duration_frames = (*f).max(1)
        }
        (GraphNode::Limiter(n), NodeCmd::SetLimiterEnabled(e)) => n.limiter.set_enabled(*e),
        (GraphNode::Limiter(n), NodeCmd::SetLimiterMode(m)) => n.limiter.set_mode(*m),
        (
            GraphNode::Limiter(n),
            NodeCmd::SetLimiterParams {
                lookahead_ms,
                attack_ms,
                release_ms,
                ceiling_db,
                soft_clip,
            },
        ) => {
            n.limiter.set_lookahead(*lookahead_ms);
            n.limiter.set_attack(*attack_ms);
            n.limiter.set_release(*release_ms);
            n.limiter.set_ceiling_db(*ceiling_db);
            n.limiter.set_soft_clip(*soft_clip);
        }
        (GraphNode::Limiter(n), NodeCmd::SetLimiterTruePeak(e)) => n.limiter.enable_true_peak(*e),
        (GraphNode::Eq(n), NodeCmd::SetEqEnabled(e)) => n.eq.set_enabled(*e),
        (GraphNode::Eq(n), NodeCmd::SetEqAutoHeadroom(e)) => n.eq.set_auto_headroom(*e),
        (GraphNode::Eq(n), NodeCmd::SetEqPreampDb(db)) => n.eq.set_preamp_db(*db),
        (GraphNode::Eq(n), NodeCmd::SetEqBassShelf(db)) => n.eq.set_bass_shelf(*db),
        (GraphNode::Eq(n), NodeCmd::SetEqTrebleShelf(db)) => n.eq.set_treble_shelf(*db),
        (GraphNode::Eq(n), NodeCmd::SetEqBand { index, params }) => n.eq.set_band(*index, *params),
        (GraphNode::Eq(n), NodeCmd::SetMidsideEq(e)) => n.midside_enabled = *e,
        (GraphNode::Convolution(n), NodeCmd::SetConvolutionWetMix(mix)) => {
            n.engine.set_wet_mix(*mix)
        }
        (GraphNode::Stereo(n), NodeCmd::SetStereoWidth(w)) => {
            let normalized = if *w > 2.0 { *w / 100.0 } else { *w };
            n.enhancer.set_width(normalized);
            n.enhancer.set_enabled((normalized - 1.0).abs() > 0.001);
        }
        (GraphNode::Stereo(n), NodeCmd::SetStereoEnhancerEnabled(e)) => n.enhancer.set_enabled(*e),
        (GraphNode::Crossfeed(n), NodeCmd::SetCrossfeedEnabled(e)) => n.crossfeed.set_enabled(*e),
        (GraphNode::Crossfeed(n), NodeCmd::SetCrossfeedProfile(p)) => n.crossfeed.set_profile(*p),
        (GraphNode::Crossfeed(n), NodeCmd::SetCrossfeedCustom(f, q, d)) => {
            n.crossfeed.set_custom_params(*f, *q, *d)
        }
        (GraphNode::Dynamics(n), NodeCmd::SetCompressorEnabled(e)) => n.compressor.set_enabled(*e),
        (
            GraphNode::Dynamics(n),
            NodeCmd::SetCompressorBand {
                band,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_gain_db,
            },
        ) => n.compressor.set_band_params(
            *band,
            *threshold_db,
            *ratio,
            *attack_ms,
            *release_ms,
            *makeup_gain_db,
        ),
        (
            GraphNode::Dynamics(n),
            NodeCmd::SetCompressorBandFeatures {
                band,
                knee_db,
                detector,
                stereo_link,
            },
        ) => n
            .compressor
            .set_band_features(*band, *knee_db, *detector, *stereo_link),
        _ => {
            debug_assert!(
                false,
                "control command {:?} applied to wrong node kind {:?}",
                cmd,
                std::mem::discriminant(node)
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DspGraph control surface — now &self enqueues (API-compatible with the
// Phase-1 surface; callers holding &mut continue to work).
// ─────────────────────────────────────────────────────────────────────────────

impl DspGraph {
    pub fn set_volume(&self, volume: f32) {
        self.control_handle().set_volume(volume);
    }

    /// Convert dB ([-60.0, 0.0]) to a linear scalar ([0.0, 1.0]).
    #[inline]
    pub fn volume_db_to_linear(db: f32) -> f32 {
        if !db.is_finite() || db <= -60.0 {
            0.0
        } else {
            10.0_f32.powf(db.clamp(-60.0, 0.0) / 20.0).clamp(0.0, 1.0)
        }
    }

    /// Convert linear scalar ([0.0, 1.0]) to dB ([-60.0, 0.0]).
    #[inline]
    pub fn volume_linear_to_db(linear: f32) -> f32 {
        if !linear.is_finite() || linear <= 1e-3 {
            -60.0
        } else {
            (20.0 * linear.clamp(0.0, 1.0).log10()).clamp(-60.0, 0.0)
        }
    }

    pub fn set_volume_db(&self, db: f32) {
        self.control_handle().set_volume_db(db);
    }

    /// Current volume as dB. Reads the active generation (post-drain truth).
    pub fn volume_db(&self) -> f32 {
        Self::volume_linear_to_db(self.volume().processor.current_gain())
    }

    pub fn set_balance(&self, balance: f32) {
        self.control_handle().set_balance(balance);
    }

    pub fn begin_seek_fadeout(&self) {
        self.control_handle().begin_seek_fadeout();
    }

    pub fn begin_seek_fadein(&self) {
        self.control_handle().begin_seek_fadein();
    }

    pub fn is_seek_fadeout_complete(&self) -> bool {
        self.seek_fade().fade.is_faded_out()
    }

    pub fn apply_loudness_metadata_outgoing(&self, metadata: Option<LoudnessMetadata>) {
        self.control_handle()
            .apply_loudness_metadata_outgoing(metadata);
    }

    pub fn apply_loudness_metadata_incoming(&self, metadata: Option<LoudnessMetadata>) {
        self.control_handle()
            .apply_loudness_metadata_incoming(metadata);
    }

    pub fn set_limiter_enabled(&self, enabled: bool) {
        self.control_handle().set_limiter_enabled(enabled);
    }

    pub fn set_limiter_mode(&self, mode: LimiterMode) {
        self.control_handle().set_limiter_mode(mode);
    }

    pub fn set_limiter_params(
        &self,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool,
    ) {
        self.control_handle().set_limiter_params(
            lookahead_ms,
            attack_ms,
            release_ms,
            ceiling_db,
            soft_clip,
        );
    }

    pub fn set_limiter_true_peak(&self, enabled: bool) {
        self.control_handle().set_limiter_true_peak(enabled);
    }

    pub fn limiter_true_peak_enabled(&self) -> bool {
        self.limiter().limiter.true_peak_enabled()
    }

    pub fn limiter_gain_reduction_db(&self) -> f32 {
        self.limiter().limiter.gain_reduction_db()
    }

    pub fn limiter_max_true_peak_dbtp(&self) -> f32 {
        self.limiter().limiter.max_true_peak_dbtp()
    }

    pub fn set_preamp_db(&self, db: f32) {
        self.control_handle().set_preamp_db(db);
    }

    pub fn set_bass_shelf(&self, gain_db: f32) {
        self.control_handle().set_bass_shelf(gain_db);
    }

    pub fn set_treble_shelf(&self, gain_db: f32) {
        self.control_handle().set_treble_shelf(gain_db);
    }

    pub fn set_eq_enabled(&self, enabled: bool) {
        self.control_handle().set_eq_enabled(enabled);
    }

    pub fn set_eq_auto_headroom(&self, enabled: bool) {
        self.control_handle().set_eq_auto_headroom(enabled);
    }

    pub fn set_eq_band(&self, index: usize, params: EqBandParams) {
        self.control_handle().set_eq_band(index, params);
    }

    pub fn eq_num_bands(&self) -> usize {
        self.eq().eq.num_bands()
    }

    pub fn set_midside_eq(&self, enabled: bool) {
        self.control_handle().set_midside_eq(enabled);
    }

    pub fn is_midside_eq(&self) -> bool {
        self.eq().midside_enabled
    }

    pub fn set_convolution_wet_mix(&self, mix: f32) {
        self.control_handle().set_convolution_wet_mix(mix);
    }

    pub fn set_stereo_width(&self, width: f32) {
        self.control_handle().set_stereo_width(width);
    }

    pub fn set_stereo_enhancer_enabled(&self, enabled: bool) {
        self.control_handle().set_stereo_enhancer_enabled(enabled);
    }

    pub fn set_crossfeed_enabled(&self, enabled: bool) {
        self.control_handle().set_crossfeed_enabled(enabled);
    }

    pub fn set_crossfeed_profile(&self, profile: config::CrossfeedProfile) {
        self.control_handle().set_crossfeed_profile(profile);
    }

    pub fn set_crossfeed_custom_params(&self, frequency_hz: f32, q: f32, delay_ms: f32) {
        self.control_handle()
            .set_crossfeed_custom_params(frequency_hz, q, delay_ms);
    }

    pub fn set_compressor_enabled(&self, enabled: bool) {
        self.control_handle().set_compressor_enabled(enabled);
    }

    pub fn set_compressor_band_params(
        &self,
        band: usize,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    ) {
        self.control_handle().set_compressor_band_params(
            band,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_gain_db,
        );
    }

    pub fn set_compressor_band_features(
        &self,
        band: usize,
        knee_db: f32,
        detector: config::CompressorDetector,
        stereo_link: bool,
    ) {
        self.control_handle()
            .set_compressor_band_features(band, knee_db, detector, stereo_link);
    }

    pub fn set_loudness_mode(&self, mode: LoudnessMode) {
        self.control_handle().set_loudness_mode(mode);
    }

    // ── Mix bus: per-input control + transitions (Phase 3 S1) ─────────────

    pub fn set_input_gain(&self, input: u8, gain: f32) {
        self.control_handle().set_input_gain(input, gain);
    }

    pub fn set_input_gain_db(&self, input: u8, db: f32) {
        self.control_handle().set_input_gain_db(input, db);
    }

    pub fn set_input_balance(&self, input: u8, balance: f32) {
        self.control_handle().set_input_balance(input, balance);
    }

    pub fn set_input_mute(&self, input: u8, mute: bool) {
        self.control_handle().set_input_mute(input, mute);
    }

    pub fn set_input_active(&self, input: u8, active: bool) {
        self.control_handle().set_input_active(input, active);
    }

    /// Begin a crossfade from the outgoing to the incoming bus input over
    /// `duration_ms`, converting to frames at the graph's sample rate.
    pub fn begin_crossfade(&self, duration_ms: u64) {
        let frames = (duration_ms as f32 * 0.001 * self.sample_rate) as usize;
        self.control_handle().begin_crossfade_frames(frames);
    }

    /// Begin a sequential fade over `duration_ms` (same frame conversion).
    pub fn begin_fade(&self, duration_ms: u64) {
        let frames = (duration_ms as f32 * 0.001 * self.sample_rate) as usize;
        self.control_handle().begin_fade_frames(frames);
    }

    /// Return the bus to single-stream playback (input 0 at unity).
    pub fn begin_playing(&self) {
        self.control_handle().begin_playing();
    }

    pub fn set_crossfade_curve(&self, curve: config::CrossfadeCurve) {
        self.control_handle().set_crossfade_curve(curve);
    }

    pub fn set_crossfade_enabled(&self, enabled: bool) {
        self.control_handle().set_crossfade_enabled(enabled);
    }

    pub fn set_crossfade_duration_ms(&self, duration_ms: u64) {
        let frames = (duration_ms as f32 * 0.001 * self.sample_rate) as usize;
        self.control_handle().set_crossfade_duration_frames(frames);
    }

    /// Current transition state of the bus (introspection).
    pub fn mixer_state(&self) -> MixerState {
        self.mix().state
    }
}
