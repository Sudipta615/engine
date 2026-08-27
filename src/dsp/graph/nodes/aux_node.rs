//! Phase 5 S2/S3 + Phase 6 — the aux bus as its own plan node.
//!
//! The aux bus lives OUTSIDE the mix node: the mix node's sum loops write
//! each slot's post-fader front-pair signal into a shared per-slot send bus
//! ([`AuxSendBus`]); this node consumes it — applying per-send automation
//! (an independent ramped gain per mix slot), running the optional global
//! insert (convolution) on the accumulator, returning into the master's
//! front pair, and metering itself independently of the mix bus.
//!
//! Realtime contract: the send bus is written by the mix step and read by
//! this step within the same block on the audio thread; control commands
//! (send targets, enabled / return gain, insert toggle) are applied at the
//! block boundary on the same thread. There is therefore exactly one
//! thread of execution — the interior mutability in [`AuxSendBus`] is
//! single-threaded by construction, with no atomics and no locks. The node
//! allocates nothing on the hot path (all planes preallocated at
//! construction). Disabled (`enabled = false`), a zero send target, or a
//! block with no sends writes nothing — bit-exact.

use std::cell::UnsafeCell;
use std::sync::Arc;

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::dsp::{
    convolution::ConvolutionEngine,
    gain::GainProcessor,
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};
use crate::dsp_utils::accumulate_scaled;

use super::mix::{SlotMeters, MAX_MIX_SLOTS};

/// Ramp duration for per-send gain automation, mirroring the preamp ramp so
/// a send target change glides instead of clicking (Phase 6: the aux bus
/// node carries per-send automation).
const AUX_SEND_RAMP_MS: f32 = 10.0;

/// The shared send bus between the mix node (writer) and the aux node
/// (reader). Both sides run on the audio thread: the mix step writes the
/// per-slot post-fader signals and the aux step consumes them, and queued
/// control (send targets, enabled) is applied at the block boundary on the
/// same thread before any processing. Interior mutability is therefore safe
/// by construction — no atomics, no locks on any hot path.
pub struct AuxSendBus {
    inner: UnsafeCell<AuxSendBusData>,
}

/// Plain data behind [`AuxSendBus`]'s interior mutability.
pub(crate) struct AuxSendBusData {
    /// Whether the aux bus is active. The mix node skips the send taps
    /// entirely when disabled — bit-exact zero cost.
    pub enabled: bool,
    /// Per-slot aux-send targets in [0, 1] (the aux node ramps toward these
    /// on its own gain processors).
    pub send_targets: [f32; MAX_MIX_SLOTS],
    /// Whether each slot's send is active (target != 0) — the mix node taps
    /// only active slots.
    pub send_active: [bool; MAX_MIX_SLOTS],
    /// Latest aux meter peak (dBFS), written by the aux node after metering
    /// and read by the mix node's duck trigger (one-block lag, same as the
    /// pre-split layout where the duck read the aux meter computed at the
    /// end of the previous block).
    pub aux_peak_db: f32,
    /// Aux duck gain for THIS block (linear), written by the mix node after
    /// its duck envelope advances and read by the aux node for the return.
    pub aux_duck_gain: f32,
    /// Per-slot stereo post-fader planes: `slots[slot * 2]` is the left
    /// plane, `+ 1` the right. Sized to [`MAX_MIX_SLOTS`] pairs of
    /// [`MAX_AUDIO_BLOCK_FRAMES`] frames, preallocated at construction.
    pub slots: Vec<Vec<f32>>,
}

// SAFETY: every access to the send bus happens on the audio thread (the
// mix step, the aux step, and the block-boundary control drain all run
// there and never concurrently). The graph may swap generations across
// threads, but the retired generation is only read by the audio thread
// that owned it. This mirrors the `DspNode` audio-thread-only contract.
unsafe impl Sync for AuxSendBus {}

impl AuxSendBus {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: UnsafeCell::new(AuxSendBusData {
                enabled: false,
                send_targets: [0.0; MAX_MIX_SLOTS],
                send_active: [false; MAX_MIX_SLOTS],
                aux_peak_db: -96.0,
                aux_duck_gain: 1.0,
                slots: (0..MAX_MIX_SLOTS * 2)
                    .map(|_| vec![0.0; MAX_AUDIO_BLOCK_FRAMES])
                    .collect(),
            }),
        })
    }

    /// Shared access (single-threaded by contract — see the module docs).
    pub(crate) fn data(&self) -> &AuxSendBusData {
        // SAFETY: every access to the send bus happens on the audio thread
        // (mix step / aux step / block-boundary control drain), which never
        // runs concurrently with itself. The `UnsafeCell` exists only to let
        // two arena nodes share one buffer without atomics or locks.
        unsafe { &*self.inner.get() }
    }

    /// Exclusive access (single-threaded by contract — see [`Self::data`]).
    #[allow(clippy::mut_from_ref)] // interior mutability: the audio-thread-only
                                   // contract (see the module docs) makes `&self -> &mut` safe by construction.
    pub(crate) fn data_mut(&self) -> &mut AuxSendBusData {
        // SAFETY: see [`Self::data`] — exclusive access on the audio thread.
        unsafe { &mut *self.inner.get() }
    }

    /// Disjoint mutable borrows of slots 0 and 1's L/R planes (the pair tap
    /// writes slot 0's contribution into slot 0's planes and slot 1's into
    /// slot 1's — per-slot automation needs them separate). Borrows are
    /// split at fixed indices, so this is safe; the slice length is capped
    /// by the caller (`frames`).
    pub(crate) fn pair_planes_mut(&self) -> (&mut [f32], &mut [f32], &mut [f32], &mut [f32]) {
        let sb = self.data_mut();
        let (s0, rest) = sb.slots.split_at_mut(2);
        let (s1, _) = rest.split_at_mut(2);
        let (l0, r0) = s0.split_at_mut(1);
        let (l1, r1) = s1.split_at_mut(1);
        (
            l0[0].as_mut_slice(),
            r0[0].as_mut_slice(),
            l1[0].as_mut_slice(),
            r1[0].as_mut_slice(),
        )
    }

    /// Disjoint mutable borrows of one slot's L/R planes (lane taps).
    pub(crate) fn slot_planes_mut(&self, slot: usize) -> (&mut [f32], &mut [f32]) {
        let sb = self.data_mut();
        let (a, b) = sb.slots.split_at_mut(slot * 2 + 1);
        let (b, _) = b.split_at_mut(1);
        (a[slot * 2].as_mut_slice(), b[0].as_mut_slice())
    }
}

/// The aux bus as a plan node: per-send automation + accumulator + insert +
/// return + independent metering.
pub struct AuxBusNode {
    /// Shared send bus written by the mix node (see [`AuxSendBus`]).
    send_bus: Arc<AuxSendBus>,
    /// Per-slot send automation: one ramped gain per mix slot (Phase 6).
    /// `send_targets` on the shared bus is the target; the ramp glides.
    sends: Vec<GainProcessor>,
    /// Linear return gain from the accumulator into the master.
    return_gain: f32,
    /// Whether any slot tapped into this block (the return is skipped when
    /// nothing was accumulated, so enabled-but-idle is still bit-exact).
    pub(crate) written: bool,
    /// Stereo accumulator planes, preallocated and zeroed once per block.
    planes: [Vec<f32>; 2],
    /// Peak / RMS metering over the accumulated send sum (Phase 5 S3),
    /// published like a slot's meters.
    pub(crate) meters: SlotMeters,
    /// Per-send peak meters (dBFS), one per mix slot (Phase 6: independent
    /// per-send metering), published for telemetry.
    pub(crate) send_peak_db: [f32; MAX_MIX_SLOTS],
    /// Whether each slot's send was engaged last block (Phase 6: the first
    /// engagement applies its target INSTANTLY — matching the pre-node
    /// instant send semantics the equivalence suite pins — while subsequent
    /// target changes glide through the per-send automation ramp).
    prev_active: [bool; MAX_MIX_SLOTS],
    /// Phase-6 insert: a global convolution (reverb / cabinet) that
    /// processes the accumulator in place before the return. `None` when no
    /// IR has been configured.
    insert: Option<ConvolutionEngine>,
    sample_rate: f32,
}

impl AuxBusNode {
    pub fn new(send_bus: Arc<AuxSendBus>, sample_rate: f32) -> Self {
        Self {
            send_bus,
            sends: (0..MAX_MIX_SLOTS)
                .map(|_| GainProcessor::with_ramp(0.0, AUX_SEND_RAMP_MS, sample_rate))
                .collect(),
            return_gain: 1.0,
            written: false,
            planes: [
                vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
                vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            ],
            meters: SlotMeters::default(),
            send_peak_db: [-96.0; MAX_MIX_SLOTS],
            prev_active: [false; MAX_MIX_SLOTS],
            insert: None,
            sample_rate,
        }
    }

    pub fn enabled(&self) -> bool {
        self.send_bus.data().enabled
    }

    pub fn return_gain(&self) -> f32 {
        self.return_gain
    }

    /// Per-slot aux-send target (mirror for introspection / tests).
    pub fn send_gain(&self, slot: usize) -> f32 {
        self.send_bus.data().send_targets[slot.min(MAX_MIX_SLOTS - 1)]
    }

    /// Apply the aux bus config (Phase 5 S2/S3): `enabled` routes the aux
    /// return into the master before the post-mix chain; `return_gain` in
    /// [0, 1] scales the return. Disabled = bit-exact.
    pub fn apply_aux(&mut self, enabled: bool, return_gain: f32) {
        self.send_bus.data_mut().enabled = enabled;
        self.return_gain = return_gain.clamp(0.0, 1.0);
    }

    /// Configure the insert convolution (Phase 6). Control path — the IR
    /// file load happens here (allocation is legal). `ir_path: None` keeps
    /// the currently loaded IR (only enabled/wet change). A missing or
    /// unreadable IR file logs a warning and leaves the insert inactive
    /// (bit-exact) rather than failing the whole bus.
    pub fn apply_aux_insert(
        &mut self,
        enabled: bool,
        wet_mix: f32,
        sample_rate: f32,
        ir_path: Option<&str>,
    ) {
        if let Some(path) = ir_path {
            let engine = self
                .insert
                .get_or_insert_with(|| ConvolutionEngine::new(sample_rate, 8192));
            engine.set_wet_mix(wet_mix);
            let loaded = engine.load_ir_from_file(std::path::Path::new(path));
            if loaded.is_err() {
                log::warn!("Aux insert: failed to load IR '{}'", path);
            }
        } else if let Some(engine) = &mut self.insert {
            engine.set_wet_mix(wet_mix);
        }
        if let Some(engine) = &mut self.insert {
            engine.set_enabled(enabled);
        }
    }

    /// Runtime toggle of the insert (Phase 6): enabled / wet only — the IR
    /// stays as configured (config-time load). No-op when no IR engine
    /// exists yet (nothing to process; stays bit-exact).
    pub fn set_aux_insert(&mut self, enabled: bool, wet_mix: f32) {
        if let Some(engine) = &mut self.insert {
            engine.set_wet_mix(wet_mix);
            engine.set_enabled(enabled);
        }
    }

    /// Current insert toggle state (enabled, wet mix), for the control-side
    /// mirror (`None` engine → defaults).
    pub fn insert_state(&self) -> (bool, f32) {
        self.insert
            .as_ref()
            .map(|e| (e.is_enabled(), e.wet_mix()))
            .unwrap_or((false, 0.5))
    }

    /// Whether the insert will process this block (enabled + IR loaded).
    pub fn insert_active(&self) -> bool {
        self.insert
            .as_ref()
            .map(|e| e.is_enabled() && e.is_ir_loaded())
            .unwrap_or(false)
    }

    /// Set one slot's aux-send target (Phase 6: per-send automation). The
    /// mix node taps the slot only while the target is non-zero; this node
    /// ramps its send gain toward the target.
    pub fn set_send(&mut self, slot: usize, gain: f32) {
        let slot = slot.min(MAX_MIX_SLOTS - 1);
        let gain = gain.clamp(0.0, 1.0);
        let sb = self.send_bus.data_mut();
        sb.send_targets[slot] = gain;
        sb.send_active[slot] = gain != 0.0;
    }

    /// Accumulate the per-slot sends into the accumulator planes with the
    /// per-send automation ramps, then meter each send. Returns whether any
    /// send wrote this block. Zero-alloc.
    fn accumulate_sends(&mut self, frames: usize) -> bool {
        let mut wrote = false;
        // Send targets are stable within a block; grab them once.
        let send_active = self.send_bus.data().send_active;
        for (slot, &target) in self.send_bus.data().send_targets.iter().enumerate() {
            if !send_active[slot] {
                continue;
            }
            let gain = &mut self.sends[slot];
            let first_engage = !self.prev_active[slot] && target != 0.0;
            self.prev_active[slot] = target != 0.0;
            if first_engage {
                // First engagement: apply the target instantly (bit-compatible
                // with the pre-node instant send); later changes glide.
                gain.set_gain(target);
                gain.snap();
            } else {
                gain.set_gain(target);
            }
            let sbl = &self.send_bus.data().slots[slot * 2][..frames];
            let sbr = &self.send_bus.data().slots[slot * 2 + 1][..frames];
            let mut peak = 0.0f32;
            for i in 0..frames {
                let g = gain.process_sample(1.0);
                let vl = sbl[i] * g;
                let vr = sbr[i] * g;
                self.planes[0][i] += vl;
                self.planes[1][i] += vr;
                let a = vl.abs();
                if a > peak {
                    peak = a;
                }
                let a = vr.abs();
                if a > peak {
                    peak = a;
                }
            }
            let eps = 1e-12f32;
            self.send_peak_db[slot] = 20.0 * (peak.max(eps)).log10();
            wrote = true;
        }
        wrote
    }

    /// Apply the aux return into the master planes' front pair (channels
    /// 0/1; an MC master receives the stereo aux on its front pair). The
    /// insert convolution processes the accumulator planes in place first
    /// (Phase 6); the return itself is a bit-exact element-wise `+=`
    /// (SIMD-accelerated, see [`accumulate_scaled`]). Skipped when disabled,
    /// idle, or the return gain is zero.
    fn return_into(&mut self, planes: &mut [&mut [f32]], frames: usize) {
        if !self.written || self.return_gain == 0.0 || planes.len() < 2 {
            return;
        }
        if self.insert_active() {
            if let Some(engine) = &mut self.insert {
                let (left, right) = self.planes.split_at_mut(1);
                engine.process_block(&mut left[0][..frames], &mut right[0][..frames]);
            }
        }
        let duck = self.send_bus.data().aux_duck_gain;
        let g = self.return_gain * duck;
        let (l, r) = planes.split_at_mut(1);
        let out_l = &mut l[0][..frames];
        let out_r = &mut r[0][..frames];
        accumulate_scaled(out_l, &self.planes[0], g, frames);
        accumulate_scaled(out_r, &self.planes[1], g, frames);
    }

    /// f64 twin of [`Self::return_into`]: the f32 aux planes are promoted at
    /// the return (the aux bus stays in f32); the insert also runs in f32.
    fn return_into_f64(&mut self, planes: &mut [&mut [f64]], frames: usize) {
        if !self.written || self.return_gain == 0.0 || planes.len() < 2 {
            return;
        }
        if self.insert_active() {
            if let Some(engine) = &mut self.insert {
                let (left, right) = self.planes.split_at_mut(1);
                engine.process_block(&mut left[0][..frames], &mut right[0][..frames]);
            }
        }
        // The f64 path keeps the scalar promotion loop (allocation-free);
        // the f32 hot path uses the SIMD accumulate above.
        let duck = self.send_bus.data().aux_duck_gain;
        let g = self.return_gain as f64 * duck as f64;
        let (l, r) = planes.split_at_mut(1);
        let out_l = &mut l[0][..frames];
        let out_r = &mut r[0][..frames];
        for i in 0..frames {
            out_l[i] += self.planes[0][i] as f64 * g;
            out_r[i] += self.planes[1][i] as f64 * g;
        }
    }

    /// Per-block peak/RMS metering over the accumulated planes (Phase 5 S3).
    fn compute_meters(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        let mut peak = 0.0f32;
        let mut sum_sq = 0.0f32;
        for plane in self.planes.iter() {
            for &v in plane.iter().take(frames) {
                let a = v.abs();
                if a > peak {
                    peak = a;
                }
                sum_sq += v * v;
            }
        }
        let eps = 1e-12f32;
        self.meters = SlotMeters {
            peak_db: 20.0 * (peak.max(eps)).log10(),
            rms_db: 20.0 * ((sum_sq / (2.0 * frames as f32)).max(eps).sqrt()).log10(),
        };
    }
}

impl DspNode for AuxBusNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "aux_bus",
            channel_support: StageChannelSupport::StereoOnly,
            position: "post-mix, pre-post-mix-chain",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.enabled() || self.insert_active()
    }

    fn latency_samples(&self) -> usize {
        if self.insert_active() {
            self.insert.as_ref().map(|e| e.block_size()).unwrap_or(0)
        } else {
            0
        }
    }

    fn tail_samples(&self) -> usize {
        if self.insert_active() {
            if let Some(e) = &self.insert {
                let ir_len = e.num_partitions() * e.block_size();
                return ir_len.saturating_sub(self.latency_samples());
            }
        }
        0
    }

    fn reset(&mut self) {
        if let Some(engine) = &mut self.insert {
            engine.reset();
        }
        self.planes[0].fill(0.0);
        self.planes[1].fill(0.0);
        self.written = false;
        self.meters = SlotMeters::default();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.sample_rate = sample_rate;
        if let Some(engine) = &mut self.insert {
            engine.set_sample_rate(sample_rate);
        }
        for send in &mut self.sends {
            *send = GainProcessor::with_ramp(0.0, AUX_SEND_RAMP_MS, sample_rate);
        }
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        let frames = planes[0].len();
        if !self.enabled() {
            return;
        }
        // Zero the accumulator for a fresh block, then accumulate the
        // per-slot sends with their automation ramps.
        self.planes[0][..frames].fill(0.0);
        self.planes[1][..frames].fill(0.0);
        self.written = self.accumulate_sends(frames);
        // The send bus is consumed; clear it so stale data never lingers
        // into a block where the mix node skipped the taps.
        for slot in 0..MAX_MIX_SLOTS {
            self.send_bus.data_mut().slots[slot * 2][..frames].fill(0.0);
            self.send_bus.data_mut().slots[slot * 2 + 1][..frames].fill(0.0);
        }
        if !self.written {
            return;
        }
        self.return_into(planes, frames);
        self.compute_meters(frames);
        self.send_bus.data_mut().aux_peak_db = self.meters.peak_db;
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        let frames = planes[0].len();
        if !self.enabled() {
            return;
        }
        self.planes[0][..frames].fill(0.0);
        self.planes[1][..frames].fill(0.0);
        self.written = self.accumulate_sends(frames);
        for slot in 0..MAX_MIX_SLOTS {
            self.send_bus.data_mut().slots[slot * 2][..frames].fill(0.0);
            self.send_bus.data_mut().slots[slot * 2 + 1][..frames].fill(0.0);
        }
        if !self.written {
            return;
        }
        self.return_into_f64(planes, frames);
        self.compute_meters(frames);
        self.send_bus.data_mut().aux_peak_db = self.meters.peak_db;
    }
}
