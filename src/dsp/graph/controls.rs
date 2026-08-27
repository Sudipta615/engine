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
use crate::buffer::{PcmRingBuffer, MAX_CHANNELS};
use crate::dsp::{
    crossfade::MixerState,
    equalizer::EqBandParams,
    limiter::LimiterMode,
    loudness::{LoudnessMetadata, LoudnessMode},
};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering};
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
#[allow(clippy::large_enum_variant)]
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
    /// Program-gated ducking config (Phase 4 S4); `None` disables.
    SetDuck(Option<DuckState>),
    /// Aux bus config (Phase 5 S2/S3): enabled + return gain.
    SetAux {
        enabled: bool,
        return_gain: f32,
    },
    /// Phase-6 aux insert: runtime toggle of the global convolution
    /// (enabled / wet only; the IR stays as configured).
    SetAuxInsert {
        enabled: bool,
        wet_mix: f32,
    },
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
    /// Per-slot user state (Phase 4 S1): one entry per possible mix-bus slot,
    /// sized to [`MAX_MIX_SLOTS`]. The audio side mirrors each slot's
    /// gain / balance / mute / active at drain; the control side seeds a
    /// fresh generation from them so a reconfig never snaps lane settings.
    user_slot_gain: Vec<AtomicU32>,
    user_slot_balance: Vec<AtomicU32>,
    user_slot_pan: Vec<AtomicU32>,
    user_slot_mute: Vec<AtomicU8>,
    user_slot_active: Vec<AtomicU8>,
    /// Per-slot metering (Phase 4 S3): peak / RMS dBFS, audio-written once
    /// per block, control-read for telemetry.
    user_slot_peak_db: Vec<AtomicU32>,
    user_slot_rms_db: Vec<AtomicU32>,
    /// Per-slot send levels (Phase 5 S2): packed `master << 32 | aux` bit
    /// patterns, mirrored like gain/pan so sends survive a generation swap.
    user_slot_send: Vec<AtomicU64>,
    /// Per-slot per-channel trim gains (Phase 5 S1): one `AtomicU32` (bit
    /// pattern) per channel, plus a packed invert bitmask.
    user_slot_trim_gain: Vec<[AtomicU32; MAX_CHANNELS]>,
    user_slot_trim_invert: Vec<AtomicU32>,
    /// Aux bus user state (Phase 5 S2/S3): enabled flag + return gain,
    /// mirrored like the per-slot state.
    user_aux_enabled: AtomicU8,
    user_aux_return_gain: AtomicU32,
    /// Phase-6 aux insert: mirrored enabled / wet-mix so a generation swap
    /// preserves a live runtime toggle (same contract as `user_aux_*`).
    user_aux_insert_enabled: AtomicU8,
    user_aux_insert_wet_mix: AtomicU32,
    /// Aux metering (Phase 5 S3): peak / RMS dBFS of the accumulated sends.
    user_aux_peak_db: AtomicU32,
    user_aux_rms_db: AtomicU32,
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
            user_slot_gain: (0..MAX_MIX_SLOTS)
                .map(|_| AtomicU32::new(1.0f32.to_bits()))
                .collect(),
            user_slot_balance: (0..MAX_MIX_SLOTS)
                .map(|_| AtomicU32::new(0.0f32.to_bits()))
                .collect(),
            user_slot_pan: (0..MAX_MIX_SLOTS)
                .map(|_| AtomicU32::new(0.0f32.to_bits()))
                .collect(),
            user_slot_mute: (0..MAX_MIX_SLOTS).map(|_| AtomicU8::new(0)).collect(),
            user_slot_active: (0..MAX_MIX_SLOTS).map(|_| AtomicU8::new(1)).collect(),
            user_slot_peak_db: (0..MAX_MIX_SLOTS)
                .map(|_| AtomicU32::new((-96.0f32).to_bits()))
                .collect(),
            user_slot_rms_db: (0..MAX_MIX_SLOTS)
                .map(|_| AtomicU32::new((-96.0f32).to_bits()))
                .collect(),
            user_slot_send: (0..MAX_MIX_SLOTS)
                .map(|_| {
                    AtomicU64::new(((1.0f32.to_bits() as u64) << 32) | 0.0f32.to_bits() as u64)
                })
                .collect(),
            user_slot_trim_gain: (0..MAX_MIX_SLOTS)
                .map(|_| std::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits())))
                .collect(),
            user_slot_trim_invert: (0..MAX_MIX_SLOTS).map(|_| AtomicU32::new(0)).collect(),
            user_aux_enabled: AtomicU8::new(0),
            user_aux_return_gain: AtomicU32::new(1.0f32.to_bits()),
            user_aux_insert_enabled: AtomicU8::new(0),
            user_aux_insert_wet_mix: AtomicU32::new(0.5f32.to_bits()),
            user_aux_peak_db: AtomicU32::new((-96.0f32).to_bits()),
            user_aux_rms_db: AtomicU32::new((-96.0f32).to_bits()),
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

    /// Publish one slot's metering to the sticky per-slot atomics (audio
    /// side, once per block). Out-of-range slots are ignored.
    pub(super) fn publish_slot_meters(&self, slot: usize, peak_db: f32, rms_db: f32) {
        if let Some(p) = self.user_slot_peak_db.get(slot) {
            p.store(peak_db.to_bits(), Ordering::Relaxed);
        }
        if let Some(r) = self.user_slot_rms_db.get(slot) {
            r.store(rms_db.to_bits(), Ordering::Relaxed);
        }
    }

    /// Control-side read of one slot's metering (peak / RMS dBFS).
    pub(super) fn slot_meters(&self, slot: usize) -> (f32, f32) {
        let peak = self
            .user_slot_peak_db
            .get(slot)
            .map(|a| f32::from_bits(a.load(Ordering::Relaxed)))
            .unwrap_or(-96.0);
        let rms = self
            .user_slot_rms_db
            .get(slot)
            .map(|a| f32::from_bits(a.load(Ordering::Relaxed)))
            .unwrap_or(-96.0);
        (peak, rms)
    }

    /// Mirror one slot's user state onto the sticky per-slot atomics
    /// (audio side, at drain). The post-apply `MixInput` is the source of
    /// truth (dB/ramped commands land as targets); out-of-range slots are
    /// ignored.
    pub(super) fn set_slot_user_state(&self, slot: usize, input: &super::nodes::MixInput) {
        if let Some(g) = self.user_slot_gain.get(slot) {
            g.store(input.gain.target_gain.to_bits(), Ordering::Relaxed);
        }
        if let Some(b) = self.user_slot_balance.get(slot) {
            b.store(input.balance.to_bits(), Ordering::Relaxed);
        }
        if let Some(p) = self.user_slot_pan.get(slot) {
            p.store(input.pan.to_bits(), Ordering::Relaxed);
        }
        if let Some(m) = self.user_slot_mute.get(slot) {
            m.store(input.mute as u8, Ordering::Relaxed);
        }
        if let Some(a) = self.user_slot_active.get(slot) {
            a.store(input.active as u8, Ordering::Relaxed);
        }
        if let Some(s) = self.user_slot_send.get(slot) {
            let packed = ((input.send.master_gain.clamp(0.0, 1.0).to_bits() as u64) << 32)
                | input.send.aux_gain.clamp(0.0, 1.0).to_bits() as u64;
            s.store(packed, Ordering::Relaxed);
        }
        if let Some(gains) = self.user_slot_trim_gain.get(slot) {
            let mut invert_mask = 0u32;
            for (c, g) in gains.iter().enumerate().take(MAX_CHANNELS) {
                let gain = input.trim.gains.get(c).copied().unwrap_or(1.0);
                g.store(gain.to_bits(), Ordering::Relaxed);
                if input.trim.invert.get(c).copied().unwrap_or(false) {
                    invert_mask |= 1 << c;
                }
            }
            if let Some(iv) = self.user_slot_trim_invert.get(slot) {
                iv.store(invert_mask, Ordering::Relaxed);
            }
        }
    }

    /// Mirror the aux bus state (Phase 5 S2/S3), audio side at drain.
    pub(super) fn set_aux_user_state(&self, enabled: bool, return_gain: f32) {
        self.user_aux_enabled
            .store(enabled as u8, Ordering::Relaxed);
        self.user_aux_return_gain
            .store(return_gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Mirror the Phase-6 aux insert state, audio side at drain.
    pub(super) fn set_aux_insert_user_state(&self, enabled: bool, wet_mix: f32) {
        self.user_aux_insert_enabled
            .store(enabled as u8, Ordering::Relaxed);
        self.user_aux_insert_wet_mix
            .store(wet_mix.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Control-side read of the mirrored aux insert state.
    pub(super) fn user_aux_insert(&self) -> (bool, f32) {
        (
            self.user_aux_insert_enabled.load(Ordering::Relaxed) != 0,
            f32::from_bits(self.user_aux_insert_wet_mix.load(Ordering::Relaxed)),
        )
    }

    /// Control-side read of the mirrored aux state.
    pub(super) fn user_aux(&self) -> (bool, f32) {
        (
            self.user_aux_enabled.load(Ordering::Relaxed) != 0,
            f32::from_bits(self.user_aux_return_gain.load(Ordering::Relaxed)),
        )
    }

    /// Publish the aux meters (Phase 5 S3), audio side once per block.
    pub(super) fn publish_aux_meters(&self, peak_db: f32, rms_db: f32) {
        self.user_aux_peak_db
            .store(peak_db.to_bits(), Ordering::Relaxed);
        self.user_aux_rms_db
            .store(rms_db.to_bits(), Ordering::Relaxed);
    }

    /// Control-side read of the aux meters.
    pub(super) fn aux_meters(&self) -> (f32, f32) {
        (
            f32::from_bits(self.user_aux_peak_db.load(Ordering::Relaxed)),
            f32::from_bits(self.user_aux_rms_db.load(Ordering::Relaxed)),
        )
    }

    /// Atomic snapshot of the current user state, for seeding a generation
    /// build on the control side ([`GraphGeneration::build_with_state`]).
    pub(super) fn snapshot(&self) -> super::swap::UserState {
        use super::swap::SlotState;
        super::swap::UserState {
            volume: self.user_volume(),
            balance: self.user_balance(),
            speed: self.user_speed(),
            volume_fade_ms: self.user_fade_ms(),
            has_live_bus_state: true,
            aux_enabled: self.user_aux_enabled.load(Ordering::Relaxed) != 0,
            aux_return_gain: f32::from_bits(self.user_aux_return_gain.load(Ordering::Relaxed)),
            aux_insert_enabled: self.user_aux_insert_enabled.load(Ordering::Relaxed) != 0,
            aux_insert_wet_mix: f32::from_bits(
                self.user_aux_insert_wet_mix.load(Ordering::Relaxed),
            ),
            // Ducking + automation are NOT mirrored onto the bus atomics:
            // `DspGraph::reconfigure` reads them from the live generation
            // instead (single-threaded control access), so snapshots keep
            // them unset.
            duck: None,
            slot_automation: Vec::new(),
            slots: (0..MAX_MIX_SLOTS)
                .map(|i| {
                    let packed = self.user_slot_send[i].load(Ordering::Relaxed);
                    let invert_mask = self.user_slot_trim_invert[i].load(Ordering::Relaxed);
                    let mut trim_gains = [1.0f32; MAX_CHANNELS];
                    let mut trim_invert = [false; MAX_CHANNELS];
                    for (c, g) in self.user_slot_trim_gain[i].iter().enumerate() {
                        trim_gains[c] = f32::from_bits(g.load(Ordering::Relaxed));
                        trim_invert[c] = invert_mask & (1 << c) != 0;
                    }
                    SlotState {
                        gain: f32::from_bits(self.user_slot_gain[i].load(Ordering::Relaxed)),
                        balance: f32::from_bits(self.user_slot_balance[i].load(Ordering::Relaxed)),
                        pan: f32::from_bits(self.user_slot_pan[i].load(Ordering::Relaxed)),
                        mute: self.user_slot_mute[i].load(Ordering::Relaxed) != 0,
                        active: self.user_slot_active[i].load(Ordering::Relaxed) != 0,
                        send_master_gain: f32::from_bits((packed >> 32) as u32),
                        send_aux_gain: f32::from_bits((packed & 0xffff_ffff) as u32),
                        trim_gains,
                        trim_invert,
                    }
                })
                .collect(),
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

    /// Read a slot's latest metering as `(peak_db, rms_db)` (Phase 4 S3).
    /// Audio-written once per block; safe from any thread.
    pub fn slot_meters(&self, slot: usize) -> (f32, f32) {
        self.bus.slot_meters(slot)
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

    /// Set the per-input pan in [-1, 1] (Phase 4 S3). Shapes the front L/R
    /// pair through the slot's pan law; channels >= 2 pass at unity.
    pub fn set_input_pan(&self, input: u8, pan: f32) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetPan(pan),
            },
        );
    }

    /// Set the per-input pan law (Phase 4 S3).
    pub fn set_input_pan_law(&self, input: u8, law: PanLaw) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetPanLaw(law),
            },
        );
    }

    /// Set one channel's trim (Phase 5 S1): gain in dB + polarity inversion.
    /// Applied on the slot's own planes between its pre-mix chains and the
    /// sum; all-unity = inactive = bit-exact.
    pub fn set_slot_trim(&self, input: u8, channel: usize, gain_db: f32, invert: bool) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetSlotTrim {
                    channel,
                    gain_db,
                    invert,
                },
            },
        );
    }

    /// Set the slot's send levels (Phase 5 S2): post-fader master-send and
    /// aux-send gains in [0, 1]. The master-send scales the slot's
    /// contribution to the master sum; the aux-send taps the post-fader
    /// signal into the aux accumulator.
    pub fn set_slot_send(&self, input: u8, master_gain: f32, aux_gain: f32) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetSend {
                    master_gain,
                    aux_gain,
                },
            },
        );
    }

    /// Configure the aux bus (Phase 5 S2/S3): `enabled` routes the aux
    /// return into the master before the post-mix chain; `return_gain` in
    /// [0, 1] scales the return. Disabled = bit-exact.
    pub fn set_aux(&self, enabled: bool, return_gain: f32) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::SetAux {
                enabled,
                return_gain,
            },
        );
    }

    /// Runtime toggle of the Phase-6 aux insert (global convolution):
    /// `enabled` + `wet_mix` only; the IR stays as configured. No-op when
    /// no IR engine exists yet.
    pub fn set_aux_insert(&self, enabled: bool, wet_mix: f32) {
        self.enqueue(node_id::MIX, NodeCmd::SetAuxInsert { enabled, wet_mix });
    }

    /// Control-side read of the mirrored aux state (enabled, return gain).
    pub fn aux_state(&self) -> (bool, f32) {
        self.bus.user_aux()
    }

    /// Control-side read of the mirrored Phase-6 aux insert state
    /// (enabled, wet mix).
    pub fn aux_insert_state(&self) -> (bool, f32) {
        self.bus.user_aux_insert()
    }

    /// Control-side read of the aux meters (peak_db, rms_db), published once
    /// per audio block (Phase 5 S3).
    pub fn aux_meters(&self) -> (f32, f32) {
        self.bus.aux_meters()
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

    /// Configure program-gated ducking (Phase 4 S4). `None` disables and the
    /// sum returns to bit-exact. `Some` config rides the queue as `Copy` data
    /// and is applied atomically on the audio side.
    pub fn set_duck(&self, cfg: Option<DuckState>) {
        self.enqueue(node_id::MIX, NodeCmd::SetDuck(cfg));
    }

    /// Replace a slot's automation track (Phase 4 S5). `points` are clamped
    /// to [`MAX_AUTOMATION_POINTS`]; an empty slice clears the track. The
    /// points must be monotonically non-decreasing in `frame`; values are
    /// linearly interpolated on the audio side.
    pub fn set_slot_automation(
        &self,
        input: u8,
        target: AutomationTarget,
        points: &[AutomationPoint],
    ) {
        let mut buf = [AutomationPoint {
            frame: 0,
            value: 0.0,
        }; MAX_AUTOMATION_POINTS];
        let count = points.len().min(MAX_AUTOMATION_POINTS);
        buf[..count].copy_from_slice(&points[..count]);
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::SetAutomation {
                    target,
                    points: buf,
                    count,
                },
            },
        );
    }

    /// Remove a slot's automation track (Phase 4 S5).
    pub fn clear_slot_automation(&self, input: u8) {
        self.enqueue(
            node_id::MIX,
            NodeCmd::MixInput {
                input,
                cmd: MixInputCmd::ClearAutomation,
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
        // Swap a pending generation in BEFORE draining the command queues: a
        // command enqueued after a generation was published must land on the
        // NEW generation (the one it replaced is retired this block).
        // Draining first would apply it to the outgoing generation and lose
        // it across the swap. The sticky user-state mirror reads the
        // post-apply state of whichever generation is now active, so
        // snapshot seeding stays consistent.
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
        if self.bus.has_pending.swap(false, Ordering::AcqRel) {
            self.drain_control();
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
                // Phase 4 S1: mirror the slot's *post-apply* state so the
                // sticky snapshot carries the linear target even for
                // dB/ramped commands (the node state is the source of truth).
                if let NodeCmd::MixInput { input, .. } = cmd {
                    let input = *input as usize;
                    if let GraphNode::Mix(mix) = &self.active.nodes[i] {
                        if let Some(slot) = mix.inputs.get(input) {
                            // Mirror the *target* gain (the user's intended
                            // setting): the ramped current value may still be
                            // mid-slew with no audio processed yet.
                            self.bus.set_slot_user_state(input, slot);
                        }
                    }
                }
                // Phase 5 S3: mirror the aux bus state post-apply so a
                // generation swap preserves enabled / return gain.
                if let NodeCmd::SetAux { .. } = cmd {
                    if let GraphNode::Mix(mix) = &self.active.nodes[i] {
                        self.bus
                            .set_aux_user_state(mix.aux.enabled, mix.aux.return_gain);
                    }
                }
                // Phase 6: mirror the aux insert state post-apply (a live
                // runtime toggle survives generation swaps, like SetAux).
                if let NodeCmd::SetAuxInsert { .. } = cmd {
                    if let GraphNode::Mix(mix) = &self.active.nodes[i] {
                        let (enabled, wet) = mix
                            .aux
                            .insert
                            .as_ref()
                            .map(|e| (e.is_enabled(), e.wet_mix()))
                            .unwrap_or((false, 0.5));
                        self.bus.set_aux_insert_user_state(enabled, wet);
                    }
                }
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
        (GraphNode::Mix(n), NodeCmd::SetDuck(cfg)) => n.apply_duck(*cfg),
        (
            GraphNode::Mix(n),
            NodeCmd::SetAux {
                enabled,
                return_gain,
            },
        ) => n.apply_aux(*enabled, *return_gain),
        (GraphNode::Mix(n), NodeCmd::SetAuxInsert { enabled, wet_mix }) => {
            n.set_aux_insert(*enabled, *wet_mix)
        }
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

    pub fn set_input_pan(&self, input: u8, pan: f32) {
        self.control_handle().set_input_pan(input, pan);
    }

    pub fn set_input_pan_law(&self, input: u8, law: PanLaw) {
        self.control_handle().set_input_pan_law(input, law);
    }

    /// Set one channel's trim (Phase 5 S1): gain in dB + polarity.
    pub fn set_slot_trim(&self, input: u8, channel: usize, gain_db: f32, invert: bool) {
        self.control_handle()
            .set_slot_trim(input, channel, gain_db, invert);
    }

    /// Set the slot's send levels (Phase 5 S2): master-send + aux tap.
    pub fn set_slot_send(&self, input: u8, master_gain: f32, aux_gain: f32) {
        self.control_handle()
            .set_slot_send(input, master_gain, aux_gain);
    }

    /// Configure the aux bus (Phase 5 S3): enabled + return gain.
    pub fn set_aux(&self, enabled: bool, return_gain: f32) {
        self.control_handle().set_aux(enabled, return_gain);
    }

    /// Runtime toggle of the Phase-6 aux insert (enabled / wet only).
    pub fn set_aux_insert(&self, enabled: bool, wet_mix: f32) {
        self.control_handle().set_aux_insert(enabled, wet_mix);
    }

    pub fn set_input_mute(&self, input: u8, mute: bool) {
        self.control_handle().set_input_mute(input, mute);
    }

    pub fn set_input_active(&self, input: u8, active: bool) {
        self.control_handle().set_input_active(input, active);
    }

    /// Configure program-gated ducking (Phase 4 S4).
    pub fn set_duck(&self, cfg: Option<DuckState>) {
        self.control_handle().set_duck(cfg);
    }

    /// Replace a slot's automation track (Phase 4 S5).
    pub fn set_slot_automation(
        &self,
        input: u8,
        target: AutomationTarget,
        points: &[AutomationPoint],
    ) {
        self.control_handle()
            .set_slot_automation(input, target, points);
    }

    /// Remove a slot's automation track (Phase 4 S5).
    pub fn clear_slot_automation(&self, input: u8) {
        self.control_handle().clear_slot_automation(input);
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
