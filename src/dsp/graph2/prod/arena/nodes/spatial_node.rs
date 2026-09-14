//! Phase 17 — the SpatialNode: the spatial master output stage in the
//! production graph.
//!
//! The graph's canonical chain ends with a spatial stage that renders the
//! mixed master through the engine's spatial layer:
//!
//! ```text
//! … → seek_fade → spatial → (limiter/dither)
//! ```
//!
//! **What it does.** The master's front pair is treated as a two-object
//! "program" (prog-L at `center − half_width`, prog-R at
//! `center + half_width`, at `elevation`, radius 2 m), rendered by the
//! [`BinauralRenderer`] (head model) with the scene's room (early
//! reflections + late field) and listener orientation. This is the
//! "spatialize stereo" output stage: the dry image sits on the configurable
//! virtual screen, and the room wraps ambience around the listener.
//!
//! **Multichannel masters** (>2-channel blocks) pass through bit-exact:
//! spatializing the MC master is explicitly deferred to the scene-audio
//! routing work (per-object/bed inputs into the node). The node stays
//! active-looking but processes nothing, matching the "enabled-but-idle"
//! contract of the aux bus.
//!
//! **Realtime discipline.** The scene, the renderer, and every scratch
//! plane are preallocated at construction/prepare; `process_block*` copies
//! the front pair into scratch, runs the (already zero-allocation) renderer,
//! and copies the interleaved result back — no allocation, no locks. Control
//! commands (enabled / screen / room / listener) are plain-data
//! [`super::super::controls::NodeCmd`]s applied at the block boundary, like
//! every other node. Disabled (`enabled = false`, the default) the node
//! returns before touching a sample — bit-exact, so the equivalence suites
//! stay pinned.
//!
//! The scene is node-private; its two program objects carry the master's
//! front pair. The [`config::SpatialConfig`] section configures it at
//! construction/reconfig; the live control surface
//! (`GraphControlHandle::set_spatial_*`) changes it at runtime.
//!
//! ## Phase 51 — listener motion (v4.3.0)
//!
//! The listener is **runtime-movable** on the audio path: a
//! `SetSpatialListenerPose` control command sets a target pose
//! (orientation + position), and the node glides toward it every block
//! with the head-tracking conventions — shortest-arc nlerp on
//! orientation, linear one-pole on position, a configurable smoothing
//! time (0 ms = snap) and optional angular rate limit. The glide is
//! allocation-free and per-block, so a world-fixed image sweeps smoothly
//! as the listener rotates/moves (the VR seam, host-driven). The baked
//! acoustic seam re-bakes on the control thread when the listener
//! crosses a relevance bound (see [`SpatialNode::listener_rebake_due`]).

use super::super::node::DspNode;
use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::dsp::pipeline::{DspStageCapability, StageChannelSupport, StagePrecision};
use crate::spatial::{
    automation::CurveScalar,
    binaural::BinauralRenderer,
    level::DistanceModel,
    math::{Quat, Vec3},
    metering::SpatialMeterState,
    object::{ObjectId, MAX_SPATIAL_OBJECTS},
    quality::SpatialQuality,
    render::{HybridBlockInputs, SpatialRenderer},
    scene::SpatialScene,
    speaker::SpeakerLayout,
    tracking::TrackingConfig,
    voice::{BudgetCandidate, VoiceAdmission, VoiceBudget, VoicePriority},
};
use std::sync::Arc;

/// Program radius (m): the virtual screen sits 2 m in front, matching the
/// speaker presets' nominal radius.
const SCREEN_RADIUS: f32 = 2.0;

/// Phase 52: the program objects' authored parameters, snapshotted for
/// the render that carries a cue overlay and restored afterwards (plain
/// stack data — allocation-free on the audio path).
#[derive(Default)]
struct ProgramSnapshot {
    applied: bool,
    position: [Vec3; 2],
    gain: [f32; 2],
    spread: [f32; 2],
}

/// The SpatialNode (see the module docs).
pub struct SpatialNode {
    enabled: bool,
    /// The node-private scene: two program objects + room + listener.
    scene: SpatialScene,
    /// Phase 52: the scene-clock for cue evaluation (seconds, advanced
    /// per block by `frames / sample_rate`). Owned by the node so cue
    /// triggers are independent of the automation clock (which the host
    /// may drive explicitly via `set_automation_time`).
    cue_clock: f32,
    /// Phase 53: modeled per-block render cost (cost units; refreshed
    /// on the control path via `refresh_cost_diagnostics`).
    last_cost_units: f32,
    /// Phase 53: modeled budget utilization fraction.
    last_cost_utilization: f32,
    /// Phase 53: the render tail budget (blocks of headroom).
    tail_budget_blocks: f32,
    /// The head-model renderer (2-channel path).
    binaural: BinauralRenderer,
    /// Front-pair program scratch (f32 planes).
    prog_l: Vec<f32>,
    prog_r: Vec<f32>,
    /// Interleaved render scratch (`channels × MAX_AUDIO_BLOCK_FRAMES`).
    out: Vec<f32>,
    /// Virtual-screen geometry (degrees / linear).
    center_azimuth_deg: f32,
    half_width_deg: f32,
    elevation_deg: f32,
    screen_gain: f32,
    /// Listener orientation (degrees), kept for introspection.
    listener_yaw_deg: f32,
    listener_pitch_deg: f32,
    listener_roll_deg: f32,
    /// Program object ids (L/R).
    obj_l: ObjectId,
    obj_r: ObjectId,
    sample_rate: f32,
    /// Voice budget (spec §76) honored from the node's config; `None` =
    /// engine default budgeting (no cap from config).
    voice: Option<VoiceBudget>,
    /// Live voice admission counts from the last `apply_voice_budget` (the
    /// audio thread), published into `PlaybackInfo` telemetry.
    voice_active: bool,
    voice_full: usize,
    voice_degraded: usize,
    voice_dropped: usize,
    // ── Phase 51: runtime listener motion ──
    /// Smoothing policy for the listener glide (nlerp on orientation,
    /// one-pole on position). `smoothing_ms = 0` snaps.
    listener_tracking: TrackingConfig,
    /// Target listener orientation (world space; the glide goal).
    listener_target_quat: Quat,
    /// Target listener position (world space; the glide goal).
    listener_target_pos: Vec3,
    /// Whether a runtime motion target is active (the config-applied
    /// static orientation also seeds the target, so `apply_listener`
    /// and the glide converge on the same state).
    listener_motion_active: bool,
    /// Sample rate at last successful `prepare` (re-prepare on change).
    prepared_rate: f32,
    prepared: bool,
}

impl SpatialNode {
    pub fn new(sample_rate: f32) -> Self {
        let mut scene = SpatialScene::new(sample_rate.max(1.0) as u32);
        let obj_l = scene
            .create_audio_object(Vec3::new(-1.0, SCREEN_RADIUS, 0.0))
            .unwrap();
        let obj_r = scene
            .create_audio_object(Vec3::new(1.0, SCREEN_RADIUS, 0.0))
            .unwrap();
        for id in [obj_l, obj_r] {
            let obj = scene.object_mut(id).unwrap();
            // The program is the already-mixed master: no distance
            // attenuation, unity gain (screen gain is applied separately).
            obj.distance_model = DistanceModel::Linear;
            obj.gain = 1.0;
        }
        let mut node = Self {
            enabled: false,
            scene,
            cue_clock: 0.0,
            last_cost_units: 0.0,
            last_cost_utilization: 0.0,
            tail_budget_blocks: f32::INFINITY,
            binaural: BinauralRenderer::new(10.0),
            prog_l: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            prog_r: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            out: vec![0.0; MAX_CHANNELS * MAX_AUDIO_BLOCK_FRAMES],
            center_azimuth_deg: 0.0,
            half_width_deg: 30.0,
            elevation_deg: 0.0,
            screen_gain: 1.0,
            listener_yaw_deg: 0.0,
            listener_pitch_deg: 0.0,
            listener_roll_deg: 0.0,
            obj_l,
            obj_r,
            sample_rate: sample_rate.max(1.0),
            voice: None,
            voice_active: false,
            voice_full: 0,
            voice_degraded: 0,
            voice_dropped: 0,
            listener_tracking: TrackingConfig::default(),
            listener_target_quat: Quat::IDENTITY,
            listener_target_pos: Vec3::ZERO,
            listener_motion_active: false,
            prepared_rate: -1.0,
            prepared: false,
        };
        // The graph only calls `DspNode::prepare` on rate changes, so the
        // node prepares its renderer eagerly at construction (control path).
        node.prepare(sample_rate.max(1.0), 2);
        node
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Screen geometry `(center_azimuth_deg, half_width_deg,
    /// elevation_deg, gain)`.
    pub fn screen(&self) -> (f32, f32, f32, f32) {
        (
            self.center_azimuth_deg,
            self.half_width_deg,
            self.elevation_deg,
            self.screen_gain,
        )
    }

    /// Room params `(enabled, width, depth, height, absorption,
    /// reflection_order, rt60_ms, late_mix, late_distance, wet)`.
    pub fn room(&self) -> (bool, f32, f32, f32, f32, u8, f32, f32, bool, f32) {
        let r = &self.scene.room;
        let wet = self
            .scene
            .object(self.obj_l)
            .map(|o| o.room_send)
            .unwrap_or(0.0);
        (
            r.enabled,
            r.width,
            r.depth,
            r.height,
            r.absorption,
            r.reflection_order,
            r.rt60_ms,
            r.late_mix,
            r.late_distance,
            wet,
        )
    }

    /// Listener orientation `(yaw, pitch, roll)` in degrees.
    pub fn listener(&self) -> (f32, f32, f32) {
        (
            self.listener_yaw_deg,
            self.listener_pitch_deg,
            self.listener_roll_deg,
        )
    }

    /// Apply the config surface (construction / reconfig / `apply_config`).
    pub fn apply_config(&mut self, cfg: &config::SpatialConfig, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.enabled = cfg.enabled;
        // Render knobs (spec §86, §76, §70): quality tier, metering enable,
        // and the voice budget.
        self.binaural.set_quality(match cfg.quality {
            config::SpatialQuality::Low => SpatialQuality::Low,
            config::SpatialQuality::Medium => SpatialQuality::Medium,
            config::SpatialQuality::High => SpatialQuality::High,
            config::SpatialQuality::Ultra => SpatialQuality::Ultra,
        });
        self.binaural.set_metering_enabled(cfg.metering.enabled);
        self.voice = if cfg.voice.enabled {
            Some(VoiceBudget {
                capacity: cfg.voice.capacity,
                full_quality_capacity: cfg.voice.full_quality_capacity,
                policy: match cfg.voice.policy {
                    config::VoicePriority::Fixed => VoicePriority::Fixed,
                    config::VoicePriority::DistanceWeighted => VoicePriority::DistanceWeighted,
                    config::VoicePriority::GainWeighted => VoicePriority::GainWeighted,
                    config::VoicePriority::UserDefined => VoicePriority::UserDefined,
                },
            })
        } else {
            None
        };
        self.apply_screen(
            cfg.center_azimuth_deg,
            cfg.half_width_deg,
            cfg.elevation_deg,
            cfg.gain,
        );
        let r = &cfg.room;
        self.apply_room(
            r.enabled,
            r.width,
            r.depth,
            r.height,
            r.absorption,
            r.reflection_order,
            r.rt60_ms,
            r.late_mix,
            r.late_distance,
            r.wet,
        );
        self.apply_listener(
            cfg.listener_yaw_deg,
            cfg.listener_pitch_deg,
            cfg.listener_roll_deg,
        );
        // Phase 52: the declarative cue bank (control path — the runtime
        // curves are built here, then only read).
        self.set_cues(&cfg.cues);
        // Phase 53: refresh the modeled cost / tail budget for the new
        // configuration.
        self.refresh_cost_diagnostics();
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Set the renderer quality tier at runtime (spec §86).
    pub fn set_quality(&mut self, q: SpatialQuality) {
        self.binaural.set_quality(q);
    }

    /// The active quality tier (spec §86).
    pub fn quality(&self) -> SpatialQuality {
        self.binaural.quality()
    }

    /// Set / replace the voice budget at runtime (spec §76). `enabled ==
    /// false` clears it (full admission). Control path — the budget is read
    /// allocation-free at block rate.
    pub fn set_voice(
        &mut self,
        enabled: bool,
        capacity: usize,
        full_quality_capacity: usize,
        policy: VoicePriority,
    ) {
        self.voice = if enabled {
            Some(VoiceBudget {
                capacity,
                full_quality_capacity,
                policy,
            })
        } else {
            None
        };
    }

    /// Attach a scalar automation curve (gain or spread) to one program
    /// object (`object` 0 = L, 1 = R). `None` clears it. Control path: the
    /// curve is built / moved off the audio thread and then evaluated
    /// allocation-free at block rate. `kind` 0 = gain, 1 = spread.
    pub fn set_program_automation(
        &mut self,
        object: usize,
        kind: u8,
        curve: Option<Arc<CurveScalar>>,
    ) {
        let id = match object {
            0 => self.obj_l,
            1 => self.obj_r,
            _ => return,
        };
        let Some(obj) = self.scene.object_mut(id) else {
            return;
        };
        let curved = curve.map(|c| (*c).clone());
        match kind {
            0 => obj.automation.gain = curved,
            1 => obj.automation.spread = curved,
            _ => {}
        }
    }

    /// Drive program-object automation at `seconds` (spec §47).
    pub fn set_automation_time(&mut self, seconds: f32) {
        self.binaural.set_automation_time(seconds);
    }

    // ── Phase 52: scene animation cues (v4.4.0) ──

    /// Replace the node's cue bank from the scene-file model (control
    /// path — allocates on the caller's thread, then only reads).
    pub fn set_cues(&mut self, cues: &[config::SpatialCueConfig]) {
        self.scene.set_cues(cues);
    }

    /// Fire the named cue at the block boundary (audio drain; the trigger
    /// resolves the name to the bank index here — the queued command
    /// carries the index). Returns `false` when the name is unknown.
    pub fn trigger_cue(&mut self, cue_index: usize) -> bool {
        let now = self.cue_clock;
        if cue_index >= self.scene.cue_bank.len() {
            return false;
        }
        self.scene.cue_bank.trigger(cue_index, now);
        true
    }
    /// Stop the active cue on a program object (audio drain).
    pub fn stop_cue(&mut self, target: usize) {
        self.scene.cue_bank.stop(target);
    }

    /// Stop all active cues (audio drain).
    pub fn stop_all_cues(&mut self) {
        self.scene.cue_bank.stop_all();
    }

    /// The number of cues in the bank (control introspection).
    pub fn cue_count(&self) -> usize {
        self.scene.cue_bank.len()
    }

    /// The cue bank as the scene-file model (control path — the
    /// persistence / round-trip surface).
    pub fn scene_cues(&self) -> Vec<config::SpatialCueConfig> {
        self.scene.cues()
    }

    /// Whether a target currently has an active cue (control
    /// introspection / tests).
    pub fn cue_active(&self, target: usize) -> bool {
        self.scene.cue_bank.is_active(target)
    }

    /// The active cue's bank index on a target (control introspection /
    /// the sticky mirror).
    pub fn cue_active_index(&self, target: usize) -> Option<usize> {
        self.scene.cue_bank.active_index(target)
    }

    /// Resolve a cue name to its bank index (control path).
    pub fn cue_index_of(&self, name: &str) -> Option<usize> {
        self.scene.cue_bank.index_of(name)
    }

    /// Advance the cue clock one block and retire finished cues (audio
    /// path, allocation-free). Returns the block duration in seconds.
    fn step_cues(&mut self, block_secs: f32) -> f32 {
        self.cue_clock += block_secs;
        self.scene.cue_bank.step(self.cue_clock);
        block_secs
    }

    /// Apply the active cue overlays onto the two program objects'
    /// effective parameters for this block's render, snapshotting the
    /// authored values for [`Self::restore_program`] (audio path,
    /// allocation-free — plain stack data; a no-op when no cue is
    /// active, so the pre-Phase-52 render stays bit-exact).
    fn apply_cues_for_render(&mut self) -> ProgramSnapshot {
        let mut snap = ProgramSnapshot::default();
        for target in 0..2 {
            let mut overlay = crate::spatial::cue::CueOverlay::none();
            self.scene
                .cue_bank
                .evaluate(target, self.cue_clock, &mut overlay);
            if overlay.is_none() {
                continue;
            }
            let id = if target == 0 { self.obj_l } else { self.obj_r };
            let Some(obj) = self.scene.object_mut(id) else {
                continue;
            };
            snap.applied = true;
            snap.position[target] = obj.position;
            snap.gain[target] = obj.gain;
            snap.spread[target] = obj.spread;
            if let Some(p) = overlay.position {
                obj.position = p;
            }
            if let Some(g) = overlay.gain {
                obj.gain = g;
            }
            if let Some(s) = overlay.spread {
                obj.spread = s;
            }
        }
        snap
    }

    /// Restore the program objects' authored parameters after a render
    /// that carried a cue overlay (audio path, allocation-free).
    fn restore_program(&mut self, snap: &ProgramSnapshot) {
        if !snap.applied {
            return;
        }
        for (target, &id) in [self.obj_l, self.obj_r].iter().enumerate() {
            let Some(obj) = self.scene.object_mut(id) else {
                continue;
            };
            obj.position = snap.position[target];
            obj.gain = snap.gain[target];
            obj.spread = snap.spread[target];
        }
    }

    /// The output meters snapshot (control thread read; spec §70).
    pub fn meters(&self) -> &SpatialMeterState {
        self.binaural.meters()
    }

    /// The voice budget honored from config, if enabled (spec §76). A host
    /// can apply it via the spatial `VoiceBudget::plan` scheduler.
    pub fn voice_budget(&self) -> Option<&VoiceBudget> {
        self.voice.as_ref()
    }

    /// Run the voice budget over the scene's objects (spec §76) and feed the
    /// resulting admission plan into the renderer's object loop. Allocation-
    /// free (stack staging + bounded copy), so it is safe on the audio path;
    /// called once per `render_block`. When no voice budget is configured the
    /// renderer is reset to full admission (bit-exact passthrough).
    fn apply_voice_budget(&mut self) {
        let Some(budget) = &self.voice else {
            self.binaural.clear_voice_admission();
            self.voice_active = false;
            self.voice_full = 0;
            self.voice_degraded = 0;
            self.voice_dropped = 0;
            return;
        };
        let mut candidates = [BudgetCandidate {
            index: 0,
            gain: 0.0,
            distance: 0.0,
            priority: 0,
        }; MAX_SPATIAL_OBJECTS];
        let mut n = 0usize;
        for (slot, obj) in self.scene.objects.iter_enabled() {
            if n >= MAX_SPATIAL_OBJECTS {
                break;
            }
            candidates[n] = BudgetCandidate {
                index: slot,
                gain: obj.gain,
                distance: (obj.position - self.scene.listener.position).length(),
                priority: 0,
            };
            n += 1;
        }
        let n_slots = self.scene.objects.len().min(MAX_SPATIAL_OBJECTS);
        let mut scratch = [0usize; MAX_SPATIAL_OBJECTS];
        let mut admission = [VoiceAdmission::Dropped; MAX_SPATIAL_OBJECTS];
        budget.plan_into(
            &candidates[..n],
            n_slots,
            &mut scratch[..n.max(1)],
            &mut admission[..n_slots.max(1)],
        );
        self.binaural
            .set_voice_admission(&admission[..n_slots.max(1)]);
        // Record the admission plan for telemetry (spec §76).
        let (mut full, mut degraded, mut dropped) = (0usize, 0usize, 0usize);
        for &a in admission[..n_slots.max(1)].iter() {
            match a {
                VoiceAdmission::Full => full += 1,
                VoiceAdmission::Degraded => degraded += 1,
                VoiceAdmission::Dropped => dropped += 1,
            }
        }
        self.voice_active = true;
        self.voice_full = full;
        self.voice_degraded = degraded;
        self.voice_dropped = dropped;
    }

    /// Live spatial telemetry snapshot (control thread): the per-ear output
    /// meters (peak/RMS dBFS) and the voice-admission counts, published into
    /// `PlaybackInfo`. Reads the renderer's meters on the engine's single
    /// audio/control thread, then the value is atomically published via
    /// `ArcSwap<PlaybackInfo>` for lock-free host reads.
    pub fn spatial_telemetry(&self) -> crate::playback_info::SpatialTelemetry {
        use crate::playback_info::SpatialTelemetry;
        let m = self.binaural.meters().snapshot();
        let db = |lin: f32| -> f32 {
            if lin <= 1e-9 {
                -96.0
            } else {
                20.0 * lin.log10()
            }
        };
        let peak_l = m.speaker_peak.first().copied().unwrap_or(0.0);
        let peak_r = m.speaker_peak.get(1).copied().unwrap_or(0.0);
        let rms_l = m.speaker_rms.first().copied().unwrap_or(0.0);
        let rms_r = m.speaker_rms.get(1).copied().unwrap_or(0.0);
        SpatialTelemetry {
            enabled: self.enabled,
            voice_active: self.voice_active,
            voice_full_voices: self.voice_full,
            voice_degraded_voices: self.voice_degraded,
            voice_dropped_voices: self.voice_dropped,
            peak_db_l: db(peak_l),
            peak_db_r: db(peak_r),
            rms_db_l: db(rms_l),
            rms_db_r: db(rms_r),
            listener_yaw_deg: self.listener_yaw_deg,
            listener_pitch_deg: self.listener_pitch_deg,
            listener_roll_deg: self.listener_roll_deg,
            listener_position: self.scene.listener.position,
            render_cost_units: self.last_cost_units,
            cost_utilization: self.last_cost_utilization,
            tail_blocks_remaining: self.tail_budget_blocks,
        }
    }

    /// Phase 53: recompute the modeled render cost + tail budget (control
    /// path — pure model math from the diagnostics module; call after
    /// config/scene changes). The values mirror into telemetry.
    pub fn refresh_cost_diagnostics(&mut self) {
        let budget = self.voice.as_ref().map(|v| v.capacity as f32);
        let report = crate::spatial::diagnostics::build_scene_cost_report(
            &self.scene,
            self.enabled,
            self.binaural.quality(),
            budget,
        );
        self.last_cost_units = report.total_cost;
        self.last_cost_utilization = report.utilization;
        // Tail budget: the remaining budget expressed in equivalent
        // blocks at the current per-block cost (`∞` when idle, 0 when at
        // or over budget).
        self.tail_budget_blocks = if report.total_cost <= 0.0 {
            f32::INFINITY
        } else if report.utilization >= 1.0 {
            0.0
        } else {
            (report.block_budget - report.total_cost) / report.total_cost
        };
    }

    /// Phase 53: the modeled render-cost report (deterministic). The
    /// budget defaults to the voice budget's capacity when configured.
    pub fn scene_cost_report(
        &self,
        block_budget: Option<f32>,
    ) -> crate::spatial::diagnostics::SceneCostReport {
        let budget = block_budget.or_else(|| self.voice.as_ref().map(|v| v.capacity as f32));
        crate::spatial::diagnostics::build_scene_cost_report(
            &self.scene,
            self.enabled,
            self.binaural.quality(),
            budget,
        )
    }

    /// Phase 53: the configured voice budget's capacity (the cost budget
    /// when a budget exists), for the engine-level report accessor.
    pub fn voice_budget_capacity(&self) -> Option<f32> {
        self.voice.as_ref().map(|v| v.capacity as f32)
    }

    /// Whether the spatial stage is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Spatial-health snapshot (spec §103 extension): an explainable
    /// per-source status derived from the existing meters + scene + voice
    /// counts. **Control/telemetry path only** — reads the renderer's meter
    /// accumulator like [`Self::spatial_telemetry`], then the snapshot is
    /// published via `ArcSwap<PlaybackInfo>` for lock-free host reads.
    pub fn spatial_health(&self) -> crate::spatial::health::SpatialHealthSnapshot {
        use crate::spatial::health::{build_health, HrtfCoverage, SpatialHealthInputs};
        let meters = self.binaural.meters().snapshot();
        let hrtf = self
            .binaural
            .hrtf_grid()
            .map(|(a0, a1, e0, e1)| HrtfCoverage {
                azimuth_min_deg: a0,
                azimuth_max_deg: a1,
                elevation_min_deg: e0,
                elevation_max_deg: e1,
            });
        build_health(SpatialHealthInputs {
            scene: &self.scene,
            meters: &meters,
            enabled: self.enabled,
            quality: self.binaural.quality(),
            voice_active: self.voice_active,
            voice_full: self.voice_full,
            voice_degraded: self.voice_degraded,
            voice_dropped: self.voice_dropped,
            hrtf,
        })
    }

    /// Move the two program objects onto the virtual screen and set the
    /// screen gain.
    pub fn apply_screen(
        &mut self,
        center_azimuth_deg: f32,
        half_width_deg: f32,
        elevation_deg: f32,
        gain: f32,
    ) {
        self.center_azimuth_deg = center_azimuth_deg;
        self.half_width_deg = half_width_deg.clamp(0.0, 90.0);
        self.elevation_deg = elevation_deg.clamp(-90.0, 90.0);
        self.screen_gain = gain.clamp(0.0, 4.0);
        let az_l = (self.center_azimuth_deg - self.half_width_deg).to_radians();
        let az_r = (self.center_azimuth_deg + self.half_width_deg).to_radians();
        let el = self.elevation_deg.to_radians();
        let (sin_el, cos_el) = el.sin_cos();
        let pos = |az: f32| -> Vec3 {
            Vec3::new(
                SCREEN_RADIUS * cos_el * az.sin(),
                SCREEN_RADIUS * cos_el * az.cos(),
                SCREEN_RADIUS * sin_el,
            )
        };
        let pl = pos(az_l);
        let pr = pos(az_r);
        self.scene.object_mut(self.obj_l).unwrap().position = pl;
        self.scene.object_mut(self.obj_r).unwrap().position = pr;
        for id in [self.obj_l, self.obj_r] {
            self.scene.object_mut(id).unwrap().gain = self.screen_gain;
        }
    }

    /// Configure the room: geometry, reflection order, late field, the
    /// program's reflection send (`wet`), and the late-field distance
    /// roll-off (Phase 50).
    #[allow(clippy::too_many_arguments)]
    pub fn apply_room(
        &mut self,
        enabled: bool,
        width: f32,
        depth: f32,
        height: f32,
        absorption: f32,
        reflection_order: u8,
        rt60_ms: f32,
        late_mix: f32,
        late_distance: bool,
        wet: f32,
    ) {
        let r = &mut self.scene.room;
        r.enabled = enabled;
        r.width = width.max(0.1);
        r.depth = depth.max(0.1);
        r.height = height.max(0.1);
        r.absorption = absorption.clamp(0.0, 0.99);
        r.reflection_order = reflection_order.clamp(1, 2);
        r.rt60_ms = rt60_ms.max(1.0);
        r.late_mix = late_mix.clamp(0.0, 1.0);
        r.late_distance = late_distance;
        let wet = wet.clamp(0.0, 1.0);
        for id in [self.obj_l, self.obj_r] {
            self.scene.object_mut(id).unwrap().room_send = wet;
        }
    }

    /// Configure the scene-wide air-absorption model applied to live room
    /// reflections (Phase 50): each live image's surface corner is composed
    /// with the model's distance corner so realtime reflections darken
    /// with travel distance, agreeing with the offline spectral kernels.
    pub fn set_air_absorption(&mut self, air: crate::spatial::level::AirAbsorption) {
        self.binaural.set_air_absorption(air);
    }

    /// The scene-wide air-absorption model (Phase 50).
    pub fn air_absorption(&self) -> crate::spatial::level::AirAbsorption {
        self.binaural.air_absorption()
    }

    /// Set the listener orientation (yaw/pitch/roll, degrees). Snaps the
    /// listener immediately (the pre-Phase-51 semantic) and seeds the
    /// motion target so a later glide continues from here.
    pub fn apply_listener(&mut self, yaw_deg: f32, pitch_deg: f32, roll_deg: f32) {
        self.listener_yaw_deg = yaw_deg;
        self.listener_pitch_deg = pitch_deg;
        self.listener_roll_deg = roll_deg;
        let q = Quat::from_euler_rad(
            yaw_deg.to_radians(),
            pitch_deg.to_radians(),
            roll_deg.to_radians(),
        );
        self.scene.listener.set_orientation(q);
        self.listener_target_quat = q;
        self.listener_motion_active = true;
    }

    // ── Phase 51: runtime listener motion (v4.3.0) ──────────────────────

    /// The listener-motion smoothing policy (nlerp / one-pole time
    /// constant, optional angular rate limit). Control path — the policy
    /// is plain data read allocation-free per block.
    pub fn set_listener_tracking(&mut self, cfg: TrackingConfig) {
        self.listener_tracking = cfg;
    }

    /// The active listener-motion smoothing policy.
    pub fn listener_tracking(&self) -> TrackingConfig {
        self.listener_tracking
    }

    /// Set the **target listener pose** (world-space orientation +
    /// position). The listener glides toward it every processed block
    /// (shortest-arc nlerp on orientation, one-pole on position) per the
    /// tracking conventions — the runtime-editable rotation/position
    /// surface (Phase 51). With `smoothing_ms = 0` the next block snaps.
    /// The glide itself is allocation-free (audio path).
    pub fn set_listener_pose_target(&mut self, orientation: Quat, position: Vec3) {
        self.listener_target_quat = orientation;
        self.listener_target_pos = position;
        self.listener_motion_active = true;
    }

    /// Clear the motion target: the listener holds its current pose
    /// (no further gliding) until a new target arrives.
    pub fn clear_listener_motion(&mut self) {
        self.listener_motion_active = false;
        self.listener_target_quat = self.scene.listener.orientation;
        self.listener_target_pos = self.scene.listener.position;
    }

    /// The live listener pose (the post-glide state the renderers read),
    /// for telemetry and control-side introspection.
    pub fn listener_pose(&self) -> (Quat, Vec3) {
        (
            self.scene.listener.orientation,
            self.scene.listener.position,
        )
    }

    /// The target pose the listener is gliding toward (held pose when
    /// motion is inactive).
    pub fn listener_pose_target(&self) -> (Quat, Vec3) {
        (self.listener_target_quat, self.listener_target_pos)
    }

    /// Advance the listener one block-step toward its motion target
    /// (audio path, per `render_block`): shortest-arc nlerp on orientation
    /// with the optional rate limit, linear one-pole on position — the
    /// `HeadTracker` discipline inlined (the node cannot own a tracker
    /// because the tracker clocks on host timestamps; the node clocks on
    /// blocks). Allocation-free. No-op when no target is active or
    /// already converged.
    fn glide_listener(&mut self, block_secs: f32) {
        if !self.listener_motion_active {
            return;
        }
        let cfg = self.listener_tracking;
        let l = &mut self.scene.listener;
        // Orientation: one-pole nlerp toward the target, then the
        // optional angular rate clamp.
        let alpha = if cfg.smoothing_ms <= 0.0 || block_secs <= 0.0 {
            1.0
        } else {
            1.0 - (-block_secs / (cfg.smoothing_ms / 1000.0)).exp()
        };
        let mut next_q = l.orientation.nlerp(self.listener_target_quat, alpha);
        if cfg.max_angular_rate_deg_s > 0.0 && block_secs > 0.0 {
            let max_step = cfg.max_angular_rate_deg_s.to_radians() * block_secs;
            let angle = l.orientation.angle_to(next_q);
            if angle > max_step && angle > 1e-9 {
                next_q = l.orientation.nlerp(next_q, (max_step / angle).min(1.0));
            }
        }
        l.set_orientation(next_q);
        // Position: the same one-pole factor on each axis.
        let next_p = l.position.lerp(self.listener_target_pos, alpha);
        l.set_position(next_p);
        // Mirror the introspection degrees from the live quaternion so
        // `listener()` (and persistence) reflect the gliding pose.
        let (y, p, r) = next_q.to_euler_rad();
        self.listener_yaw_deg = y.to_degrees();
        self.listener_pitch_deg = p.to_degrees();
        self.listener_roll_deg = r.to_degrees();
        // Converged: stop gliding (bit-stable when the target is held).
        if next_q.angle_to(self.listener_target_quat) < 1e-6
            && (next_p - self.listener_target_pos).length_squared() < 1e-12
        {
            self.listener_motion_active = false;
        }
    }

    /// Whether a moving listener has crossed the baked-scene relevance
    /// bound and the control thread should re-bake the acoustic scene
    /// (Phase 51's smooth re-bake seam). The bound is the bake cell
    /// size (the resolution a re-bake can meaningfully change); the
    /// engine calls this on its tick and, when due, rebuilds the baked
    /// generation on the control thread and publishes it — the audio
    /// path is never interrupted (the Phase-2 swap machinery).
    pub fn listener_rebake_due(&self, cell_m: f32, last_baked_at: Vec3) -> bool {
        let cell = cell_m.max(0.05);
        let p = self.scene.listener.position;
        let d = p - last_baked_at;
        // Crossed a full cell in any axis → the baked response is stale.
        d.x.abs() >= cell || d.y.abs() >= cell || d.z.abs() >= cell
    }

    /// Render the scene into the block planes in place (f32 path).
    fn render_block(&mut self, planes: &mut [&mut [f32]], frames: usize) {
        if !self.prepared || frames == 0 || frames > MAX_AUDIO_BLOCK_FRAMES {
            return;
        }
        if planes.len() < 2 {
            return;
        }
        // Phase 51: glide the listener toward its motion target this
        // block (allocation-free; no-op when converged / inactive).
        let block_secs = frames as f32 / self.sample_rate;
        self.glide_listener(block_secs);
        // Phase 52: advance the cue clock and retire finished cues, then
        // overlay any active cue onto the program objects for this block
        // (allocation-free; no-op when the bank is idle).
        self.step_cues(block_secs);
        let snap = self.apply_cues_for_render();
        // Voice budget → admission (spec §76), allocation-free, this block.
        self.apply_voice_budget();
        self.prog_l[..frames].copy_from_slice(&planes[0][..frames]);
        self.prog_r[..frames].copy_from_slice(&planes[1][..frames]);
        let need = 2 * frames;
        let obj_refs = [
            self.prog_l[..frames].as_ref(),
            self.prog_r[..frames].as_ref(),
        ];
        let inputs = HybridBlockInputs {
            objects: &obj_refs,
            beds: &[],
            fields: &[],
        };
        let result =
            self.binaural
                .process_hybrid_block(&self.scene, &inputs, frames, &mut self.out[..need]);
        // Restore the program objects' authored parameters (cue overlays
        // apply per block only).
        self.restore_program(&snap);
        // After a successful prepare the render cannot fail; any error
        // leaves the block untouched (bit-exact passthrough) rather than
        // emitting garbage.
        if result.is_err() {
            return;
        }
        for (ch, plane) in planes.iter_mut().enumerate().take(2) {
            for f in 0..frames {
                plane[f] = self.out[f * 2 + ch];
            }
        }
    }

    /// f64 twin: demote the front pair, render in f32, promote back.
    fn render_block_f64(&mut self, planes: &mut [&mut [f64]], frames: usize) {
        if !self.prepared || frames == 0 || frames > MAX_AUDIO_BLOCK_FRAMES {
            return;
        }
        if planes.len() < 2 {
            return;
        }
        // Phase 51: glide the listener toward its motion target this
        // block (allocation-free; no-op when converged / inactive).
        let block_secs = frames as f32 / self.sample_rate;
        self.glide_listener(block_secs);
        // Phase 52: advance the cue clock and retire finished cues, then
        // overlay any active cue onto the program objects for this block
        // (allocation-free; no-op when the bank is idle).
        self.step_cues(block_secs);
        let snap = self.apply_cues_for_render();
        // Voice budget → admission (spec §76), allocation-free, this block.
        self.apply_voice_budget();
        for (f, (&l, &r)) in planes[0]
            .iter()
            .zip(planes[1].iter())
            .take(frames)
            .enumerate()
        {
            self.prog_l[f] = l as f32;
            self.prog_r[f] = r as f32;
        }
        let need = 2 * frames;
        let obj_refs = [
            self.prog_l[..frames].as_ref(),
            self.prog_r[..frames].as_ref(),
        ];
        let inputs = HybridBlockInputs {
            objects: &obj_refs,
            beds: &[],
            fields: &[],
        };
        let result =
            self.binaural
                .process_hybrid_block(&self.scene, &inputs, frames, &mut self.out[..need]);
        // Restore the program objects' authored parameters (cue overlays
        // apply per block only).
        self.restore_program(&snap);
        if result.is_err() {
            return;
        }
        for (ch, plane) in planes.iter_mut().enumerate().take(2) {
            for f in 0..frames {
                plane[f] = self.out[f * 2 + ch] as f64;
            }
        }
    }
}

impl DspNode for SpatialNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "spatial",
            channel_support: StageChannelSupport::AllChannels,
            position: "post-mix, spatial master output",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.enabled && self.prepared
    }

    fn reset(&mut self) {
        // The renderer owns per-block state; nothing to clear here (the
        // scene parameters are user state, like volume).
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.sample_rate = sample_rate.max(1.0);
        if (sample_rate - self.prepared_rate).abs() > 1.0 || !self.prepared {
            // The binaural head model (stereo/headphone path). Multichannel
            // blocks pass through bit-exact — see the module docs.
            let layout = SpeakerLayout::stereo();
            self.prepared = self
                .binaural
                .prepare(&layout, sample_rate.max(1.0) as u32)
                .is_ok();
            self.prepared_rate = sample_rate.max(1.0);
        }
        // Re-apply the geometric params so a rebuilt node carries them.
        self.apply_screen(
            self.center_azimuth_deg,
            self.half_width_deg,
            self.elevation_deg,
            self.screen_gain,
        );
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if !self.enabled || !self.prepared || planes.len() != 2 {
            return;
        }
        let frames = planes[0].len();
        self.render_block(planes, frames);
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if !self.enabled || !self.prepared || planes.len() != 2 {
            return;
        }
        let frames = planes[0].len();
        self.render_block_f64(planes, frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::EngineConfig;

    fn argmax_abs(buf: &[f32]) -> usize {
        buf.iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Woodworth ITD (samples at 48 kHz) for the given azimuth in radians.
    fn woodworth_samples(azimuth_rad: f32, sample_rate: u32) -> f32 {
        const A: f32 = 0.0875;
        const C: f32 = 343.0;
        let az = azimuth_rad.abs().min(std::f32::consts::PI);
        let t = if az <= std::f32::consts::FRAC_PI_2 {
            (A / C) * (az.sin() + az)
        } else {
            (A / C) * (std::f32::consts::PI - az + az.sin())
        };
        t * sample_rate as f32
    }

    #[test]
    fn apply_config_sets_quality_metering_and_voice_budget() {
        // The declarative config surface (spec §86, §76, §70) must reach the
        // live node's renderer and voice budget.
        let mut node = SpatialNode::new(48_000.0);
        let cfg = config::SpatialConfig {
            quality: config::SpatialQuality::High,
            metering: config::SpatialMeterConfig { enabled: true },
            voice: config::SpatialVoiceConfig {
                capacity: 24,
                full_quality_capacity: 8,
                policy: config::VoicePriority::GainWeighted,
                ..Default::default()
            },
            ..Default::default()
        };
        node.apply_config(&cfg, 48_000.0);

        let budget = node.voice_budget().expect("voice enabled");
        assert_eq!(budget.capacity, 24);
        assert_eq!(budget.full_quality_capacity, 8);
        assert!(matches!(budget.policy, VoicePriority::GainWeighted));

        // A serialized SpatialConfig round-trips the new knobs.
        let j = serde_json::to_string(&cfg).unwrap();
        let back: config::SpatialConfig = serde_json::from_str(&j).unwrap();
        assert_eq!(back.quality, config::SpatialQuality::High);
        assert!(back.metering.enabled);
        assert_eq!(back.voice.capacity, 24);
        assert_eq!(back.metering, cfg.metering);

        // Legacy object missing the new fields still deserializes (defaults).
        let legacy = r#"{"enabled":true}"#;
        let old: config::SpatialConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(old.quality, config::SpatialQuality::Medium);
        assert!(old.metering.enabled);
        assert!(!old.voice.enabled || old.voice.capacity == 48);
    }

    #[test]
    fn disabled_node_is_bit_exact_passthrough() {
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        let frames = 256;
        let l: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.001).sin()).collect();
        let r: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.002).cos()).collect();
        let mut l2 = l.clone();
        let mut r2 = r.clone();
        let mut planes: Vec<&mut [f32]> = vec![l2.as_mut_slice(), r2.as_mut_slice()];
        node.process_block_f32(&mut planes);
        assert_eq!(planes[0], l.as_slice());
        assert_eq!(planes[1], r.as_slice());
        // Enabled-but-unprepared (multichannel) is also bit-exact.
        let mut mc = SpatialNode::new(48_000.0);
        mc.set_enabled(true);
        let mut l2 = l.clone();
        let mut r2 = r.clone();
        // A 6-plane block (multichannel) passes through untouched even when
        // enabled.
        let mut extra: Vec<Vec<f32>> = (2..6).map(|ch| vec![ch as f32 * 0.01; frames]).collect();
        let mut refs: Vec<&mut [f32]> = vec![l2.as_mut_slice(), r2.as_mut_slice()];
        refs.extend(extra.iter_mut().map(|p| p.as_mut_slice()));
        mc.process_block_f32(&mut refs);
        assert_eq!(refs[0], l.as_slice());
        assert_eq!(refs[1], r.as_slice());
    }

    #[test]
    fn enabled_node_renders_binaural_image_with_itd() {
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_screen(0.0, 30.0, 0.0, 1.0);
        let frames = 1024;
        let mut l = vec![0.0f32; frames];
        l[64] = 1.0;
        let mut r = vec![0.0f32; frames];
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        // The left-only impulse at azimuth −30° reaches the right ear one
        // Woodworth ITD later (the contralateral ear carries the delay).
        let il = argmax_abs(planes[0]);
        let ir = argmax_abs(planes[1]);
        let expect = woodworth_samples(30f32.to_radians(), 48_000);
        assert!(ir > il, "right ear delayed ({ir} vs {il})");
        assert!(
            ((ir - il) as f32 - expect).abs() <= 4.0,
            "ITD {} samples vs {expect}",
            ir - il
        );
        // The ipsilateral (left) ear carries more energy.
        let e_l: f32 = planes[0].iter().map(|v| v * v).sum();
        let e_r: f32 = planes[1].iter().map(|v| v * v).sum();
        assert!(e_l > e_r, "ipsilateral ear stronger ({e_l} vs {e_r})");
    }

    #[test]
    fn listener_yaw_moves_the_image_across_the_ears() {
        // Listener yaws +90° (faces +X). The world-fixed screen (world +Y)
        // lands at local azimuth −90°: both program objects are now on the
        // listener's left, so the RIGHT ear becomes the contralateral ear
        // for both — its delay grows to itd(120°).
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_screen(0.0, 30.0, 0.0, 1.0);
        node.apply_listener(90.0, 0.0, 0.0);
        let frames = 1024;
        let mut l = vec![0.0f32; frames];
        l[64] = 1.0;
        let mut r = vec![0.0f32; frames];
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        let il = argmax_abs(planes[0]);
        let ir = argmax_abs(planes[1]);
        // Left object world −30° → local −120° → contralateral delay itd(120°).
        let expect = woodworth_samples(120f32.to_radians(), 48_000);
        assert!(ir > il, "right ear still contralateral ({ir} vs {il})");
        assert!(
            ((ir - il) as f32 - expect).abs() <= 6.0,
            "ITD {} samples vs {expect}",
            ir - il
        );
        // The image moved left: the left ear now carries most of the energy.
        let e_l: f32 = planes[0].iter().map(|v| v * v).sum();
        let e_r: f32 = planes[1].iter().map(|v| v * v).sum();
        assert!(e_l > e_r * 2.0, "image left ({e_l} vs {e_r})");
    }

    #[test]
    fn room_adds_a_decaying_tail_beyond_the_direct() {
        let mut off = SpatialNode::new(48_000.0);
        off.prepare(48_000.0, 2);
        off.set_enabled(true);
        off.apply_screen(0.0, 30.0, 0.0, 1.0);
        let mut on = SpatialNode::new(48_000.0);
        on.prepare(48_000.0, 2);
        on.set_enabled(true);
        on.apply_screen(0.0, 30.0, 0.0, 1.0);
        on.apply_room(true, 12.0, 10.0, 3.0, 0.2, 1, 800.0, 0.5, false, 0.5);
        let frames = 4096;
        let run = |node: &mut SpatialNode| -> (f32, f32) {
            let mut l = vec![0.0f32; frames];
            l[64] = 1.0;
            let mut r = vec![0.0f32; frames];
            let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
            node.process_block_f32(&mut planes);
            let direct: f32 = planes[0][60..80].iter().map(|v| v * v).sum::<f32>()
                + planes[1][60..80].iter().map(|v| v * v).sum::<f32>();
            let tail: f32 = planes[0][500..4096].iter().map(|v| v * v).sum::<f32>()
                + planes[1][500..4096].iter().map(|v| v * v).sum::<f32>();
            (direct, tail)
        };
        let (d_off, t_off) = run(&mut off);
        let (d_on, t_on) = run(&mut on);
        // The room must add substantial tail energy beyond the direct (the
        // reflections + late field land after ~280 samples), while the
        // direct stays comparable.
        assert!(d_on > 0.0, "direct present with room");
        assert!(
            t_on > t_off * 20.0,
            "room tail {} vs {} (no room)",
            t_on,
            t_off
        );
        assert!(
            (d_on - d_off).abs() / d_on < 0.5,
            "direct roughly unchanged"
        );
    }

    #[test]
    fn screen_geometry_moves_the_image() {
        // Narrow screen (half_width 0 → both objects at center): the
        // L-only impulse lands at azimuth 0 → both ears equal (no ITD).
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_screen(0.0, 0.0, 0.0, 1.0);
        let frames = 1024;
        let mut l = vec![0.0f32; frames];
        l[64] = 1.0;
        let mut r = vec![0.0f32; frames];
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        let il = argmax_abs(planes[0]);
        let ir = argmax_abs(planes[1]);
        assert!(
            (ir as isize - il as isize).unsigned_abs() <= 2,
            "centered image: no ITD ({il} vs {ir})"
        );
    }

    #[test]
    fn control_handle_commands_apply_at_drain_and_survive_reconfig() {
        let mut graph = crate::dsp::graph2::prod::arena::DspGraph::from_config(
            &EngineConfig::default(),
            48_000.0,
        );
        let handle = graph.control_handle();
        handle.set_spatial_enabled(true);
        handle.set_spatial_screen(0.0, 45.0, 5.0, 0.8);
        handle.set_spatial_room(true, 10.0, 8.0, 3.0, 0.3, 1, 600.0, 0.4, false, 0.6);
        handle.set_spatial_listener(10.0, 0.0, 0.0);
        graph.drain_queued_control();
        let s = graph.spatial();
        assert!(s.enabled());
        assert_eq!(s.screen(), (0.0, 45.0, 5.0, 0.8));
        assert!(s.room().0);
        assert_eq!(s.listener().0, 10.0);
        // The live enable survives a generation rebuild (mirrored at drain).
        let cfg = EngineConfig::default();
        graph.reconfigure(&cfg);
        graph.drain_queued_control(); // swap the pending generation in
        assert!(graph.spatial().enabled());
        // Reconfig replays the config-applied screen (the rebuild node is
        // re-seeded from config; live screen/room/listener are not carried
        // — documented).
        assert!((graph.spatial().screen().1 - 30.0).abs() < 1e-4);
    }

    #[test]
    fn multichannel_block_passes_through() {
        let mut node = SpatialNode::new(48_000.0);
        node.set_enabled(true);
        let frames = 128;
        let mut planes: Vec<Vec<f32>> = (0..6)
            .map(|ch| {
                (0..frames)
                    .map(|i| ch as f32 * 0.01 + (i as f32 * 0.001).sin())
                    .collect()
            })
            .collect();
        let before: Vec<Vec<f32>> = planes.clone();
        let mut refs: Vec<&mut [f32]> = planes.iter_mut().map(|p| p.as_mut_slice()).collect();
        node.process_block_f32(&mut refs);
        for (ch, p) in planes.iter().enumerate() {
            assert_eq!(p, &before[ch], "channel {ch} untouched");
        }
    }

    #[test]
    fn voice_budget_drops_a_program_object_end_to_end() {
        // The SpatialNode's config voice budget must reach the renderer's
        // object loop (spec §76). Both program objects share gain/screen
        // radius, so GainWeighted ties by store index: with a capacity of 1
        // the second program voice is dropped, cutting the output energy
        // roughly in half; with no budget both render (full admission).
        let frames = 1024;
        let render_energy = |voice: config::SpatialVoiceConfig| -> f64 {
            let mut node = SpatialNode::new(48_000.0);
            node.prepare(48_000.0, 2);
            node.set_enabled(true);
            let cfg = config::SpatialConfig {
                enabled: true,
                voice,
                ..Default::default()
            };
            node.apply_config(&cfg, 48_000.0);
            node.apply_screen(0.0, 30.0, 0.0, 1.0);
            let mut l: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.01).sin()).collect();
            let mut r: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.02).cos()).collect();
            let mut planes: Vec<&mut [f32]> = vec![l.as_mut_slice(), r.as_mut_slice()];
            node.process_block_f32(&mut planes);
            planes
                .iter()
                .map(|p| p.iter().map(|&v| (v as f64).powi(2)).sum::<f64>())
                .sum::<f64>()
        };
        let full = config::SpatialVoiceConfig {
            capacity: 2,
            full_quality_capacity: 2,
            policy: config::VoicePriority::GainWeighted,
            ..Default::default()
        };
        let tight = config::SpatialVoiceConfig {
            capacity: 1,
            full_quality_capacity: 1,
            policy: config::VoicePriority::GainWeighted,
            ..Default::default()
        };
        let disabled = config::SpatialVoiceConfig {
            enabled: false,
            ..Default::default()
        };
        let e_full = render_energy(full);
        let e_tight = render_energy(tight);
        let e_off = render_energy(disabled);
        // No budget == both admitted, so identical energy to a roomy budget.
        assert!((e_full - e_off).abs() < 1e-6 * e_full.max(1.0));
        // Dropping one of two equal programme voices cuts the output energy.
        assert!(
            e_tight < e_full * 0.9,
            "tight budget must drop a voice: {e_tight} vs {e_full}"
        );
        assert!(e_full.is_finite() && e_tight.is_finite());
    }

    #[test]
    fn voice_budget_drop_is_deterministic_given_same_initial_state() {
        // Renderers are stateful (rings/filter one-poles warm up), so the
        // determinism guarantee is: two fresh nodes with the same scene +
        // budget produce byte-identical output, and the result is finite.
        let frames = 512;
        let build = || -> SpatialNode {
            let mut n = SpatialNode::new(48_000.0);
            n.prepare(48_000.0, 2);
            n.set_enabled(true);
            let cfg = config::SpatialConfig {
                enabled: true,
                voice: config::SpatialVoiceConfig {
                    capacity: 1,
                    full_quality_capacity: 1,
                    policy: config::VoicePriority::GainWeighted,
                    ..Default::default()
                },
                ..Default::default()
            };
            n.apply_config(&cfg, 48_000.0);
            n.apply_screen(0.0, 30.0, 0.0, 1.0);
            n
        };
        let run = |mut node: SpatialNode| -> Vec<f32> {
            let mut l: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.01).sin()).collect();
            let mut r: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.02).cos()).collect();
            let mut planes: Vec<&mut [f32]> = vec![l.as_mut_slice(), r.as_mut_slice()];
            node.process_block_f32(&mut planes);
            let mut out = Vec::with_capacity(2 * frames);
            for (f, (&l, &rr)) in planes[0]
                .iter()
                .zip(planes[1].iter())
                .enumerate()
                .take(frames)
            {
                out.push(l);
                out.push(rr);
                let _ = f;
            }
            out
        };
        let a = run(build());
        let b = run(build());
        assert_eq!(a, b);
        assert!(a.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn spatial_telemetry_reports_voice_plan_and_meters() {
        // The SpatialNode publishes its voice admission counts and the
        // renderer's per-ear output meters into telemetry (spec §76).
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        let cfg = config::SpatialConfig {
            enabled: true,
            voice: config::SpatialVoiceConfig {
                capacity: 1,
                full_quality_capacity: 1,
                policy: config::VoicePriority::GainWeighted,
                ..Default::default()
            },
            ..Default::default()
        };
        node.apply_config(&cfg, 48_000.0);
        node.apply_screen(0.0, 30.0, 0.0, 1.0);
        let frames = 512;
        let mut l: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut r: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.02).cos()).collect();
        let mut planes: Vec<&mut [f32]> = vec![l.as_mut_slice(), r.as_mut_slice()];
        node.process_block_f32(&mut planes);

        let t = node.spatial_telemetry();
        assert!(t.enabled);
        assert!(t.voice_active);
        assert_eq!(t.voice_full_voices, 1);
        assert_eq!(t.voice_degraded_voices, 0);
        assert_eq!(t.voice_dropped_voices, 1);
        // Nonzero program audio reached the binaural ears.
        assert!(t.peak_db_l > -30.0, "L peak {}", t.peak_db_l);
        assert!(t.peak_db_r > -30.0, "R peak {}", t.peak_db_r);
        assert!(t.peak_db_l.is_finite() && t.rms_db_r.is_finite());
    }

    #[test]
    fn spatial_health_reports_explainable_per_source_status() {
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_config(
            &config::SpatialConfig {
                enabled: true,
                ..Default::default()
            },
            48_000.0,
        );
        node.apply_screen(0.0, 30.0, 0.0, 1.0);
        let frames = 256;
        // A mono-like programme (identical L/R content): the panned ±30°
        // objects stay phase-coherent, so the measured correlation reports
        // low phase risk rather than flagging the *test signal* itself.
        let mut l: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut r: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut planes: Vec<&mut [f32]> = vec![l.as_mut_slice(), r.as_mut_slice()];
        node.process_block_f32(&mut planes);

        let h = node.spatial_health();
        // Two program objects (L/R) on the virtual screen, analytic head
        // model, no room, no occlusion, no voice budget → all Good.
        assert_eq!(h.status, crate::spatial::health::HealthLevel::Good);
        assert_eq!(h.active_sources, 2);
        assert_eq!(h.per_source.len(), 2);
        assert_eq!(
            h.localization.level,
            crate::spatial::health::HealthLevel::Good
        );
        assert_eq!(h.occlusion.level, crate::spatial::health::HealthLevel::Good);
        assert_eq!(
            h.voice_pressure.level,
            crate::spatial::health::HealthLevel::Good
        );
        for s in &h.per_source {
            assert!(!s.reasons.is_empty());
        }

        // Disabled stage → Inactive report.
        node.set_enabled(false);
        let h = node.spatial_health();
        assert_eq!(h.status, crate::spatial::health::HealthLevel::Inactive);
    }

    #[test]
    fn listener_pose_target_glides_smoothly_across_blocks() {
        // Phase 51: a runtime pose target (90° yaw + 1 m step) with a 20 ms
        // one-pole glides block-by-block — bounded steps, convergence —
        // and the image tracks it (the world-fixed screen sweeps across
        // the ears as the listener yaws).
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_screen(0.0, 30.0, 0.0, 1.0);
        node.set_listener_tracking(crate::spatial::TrackingConfig {
            smoothing_ms: 20.0,
            max_angular_rate_deg_s: 0.0,
        });
        let target_q = Quat::from_euler_rad(90f32.to_radians(), 0.0, 0.0);
        let target_p = Vec3::new(0.0, 1.0, 0.0);
        node.set_listener_pose_target(target_q, target_p);
        assert_eq!(node.listener_pose_target().0, target_q);

        let frames = 512;
        let mut l = vec![0.0f32; frames];
        l[64] = 1.0;
        let mut r = vec![0.0f32; frames];
        let mut prev = node.listener_pose();
        let mut max_yaw_step = 0.0f32;
        for _ in 0..40 {
            let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
            node.process_block_f32(&mut planes);
            let (q, p) = node.listener_pose();
            let step = q.angle_to(prev.0).to_degrees();
            max_yaw_step = max_yaw_step.max(step);
            assert!((p - prev.1).length() < 1.0, "position glides, never jumps");
            prev = (q, p);
        }
        assert!(
            max_yaw_step < 45.0,
            "no single-block snap ({max_yaw_step}°)"
        );
        assert!(
            prev.0.angle_to(target_q).to_degrees() < 1.0,
            "converged to the target yaw"
        );
        assert!(
            (prev.1 - target_p).length() < 0.02,
            "converged to the target position"
        );
        // The glided pose is what telemetry reports.
        let t = node.spatial_telemetry();
        assert!((t.listener_yaw_deg - 90.0).abs() < 1.0);
        assert!((t.listener_position.y - 1.0).abs() < 0.02);
    }

    #[test]
    fn listener_pose_zero_smoothing_snaps_next_block() {
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.set_listener_tracking(crate::spatial::TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 0.0,
        });
        node.set_listener_pose_target(
            Quat::from_euler_rad(45f32.to_radians(), 0.0, 0.0),
            Vec3::new(2.0, 0.0, 0.0),
        );
        let frames = 128;
        let mut l = vec![0.5f32; frames];
        let mut r = vec![0.5f32; frames];
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        let (q, p) = node.listener_pose();
        assert!(
            q.angle_to(Quat::from_euler_rad(45f32.to_radians(), 0.0, 0.0)) < 1e-4,
            "snapped"
        );
        assert_eq!(p, Vec3::new(2.0, 0.0, 0.0));
    }

    #[test]
    fn listener_motion_rate_limit_caps_the_yaw_step() {
        // 100°/s at 48 kHz / 512-frame blocks ≈ 1.07° per block; a 90°
        // target never steps faster and catches up.
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.set_listener_tracking(crate::spatial::TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 100.0,
        });
        let target = Quat::from_euler_rad(90f32.to_radians(), 0.0, 0.0);
        node.set_listener_pose_target(target, Vec3::ZERO);
        let frames = 512;
        let mut l = vec![0.3f32; frames];
        let mut r = vec![0.3f32; frames];
        let mut prev = node.listener_pose().0;
        let mut max_step = 0.0f32;
        for _ in 0..120 {
            let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
            node.process_block_f32(&mut planes);
            let q = node.listener_pose().0;
            max_step = max_step.max(q.angle_to(prev).to_degrees());
            prev = q;
        }
        assert!(max_step <= 1.2, "per-block step ≤ cap (max {max_step}°)");
        assert!(prev.angle_to(target).to_degrees() < 1.0, "caught up");
    }

    #[test]
    fn listener_rebake_due_crosses_cell_bounds() {
        // The relevance-bound seam: a bake at the origin stays fresh while
        // the listener glides within one cell and goes stale once a full
        // cell is crossed.
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        assert!(!node.listener_rebake_due(0.5, Vec3::ZERO), "at origin");
        // Within the cell (0.4 m off) — not due.
        node.set_listener_tracking(crate::spatial::TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 0.0,
        });
        node.set_listener_pose_target(Quat::IDENTITY, Vec3::new(0.4, 0.0, 0.0));
        let frames = 64;
        let mut l = vec![0.0f32; frames];
        let mut r = vec![0.0f32; frames];
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        assert!(!node.listener_rebake_due(0.5, Vec3::ZERO), "inside cell");
        // Across the bound (0.6 m) — due.
        node.set_listener_pose_target(Quat::IDENTITY, Vec3::new(0.6, 0.0, 0.0));
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        assert!(node.listener_rebake_due(0.5, Vec3::ZERO), "crossed a cell");
    }

    #[test]
    fn listener_pose_command_applies_through_the_control_handle() {
        // The queued surface: `set_spatial_listener_pose` on the control
        // handle lands at the block-boundary drain and the node glides
        // from there. (The angle tolerance is f32 `angle_to` noise —
        // identical quats round-trip the dot product at ~1e-3.)
        let mut graph = crate::dsp::graph2::prod::arena::DspGraph::from_config(
            &EngineConfig::default(),
            48_000.0,
        );
        let handle = graph.control_handle();
        handle.set_spatial_enabled(true);
        handle.set_spatial_listener_tracking(0.0, 0.0);
        let target = Quat::from_euler_rad(30f32.to_radians(), 0.0, 0.0);
        handle.set_spatial_listener_pose(target, Vec3::new(1.0, 0.5, 0.0));
        graph.drain_queued_control();
        let (q, p) = graph.spatial().listener_pose_target();
        assert!(
            q.angle_to(target) < 1e-3,
            "target applied: {}",
            q.angle_to(target)
        );
        assert_eq!(p, Vec3::new(1.0, 0.5, 0.0));
        // And the glide policy landed too.
        let policy = graph.spatial().listener_tracking();
        assert_eq!(policy.smoothing_ms, 0.0);
        assert_eq!(policy.max_angular_rate_deg_s, 0.0);
    }

    #[test]
    fn moving_listener_rotates_the_rendered_image() {
        // End-to-end audio: a world-fixed left-impulse image moves across
        // the ears as the listener yaws +90° through the glide — the same
        // perceptual cue as the static `apply_listener` test, reached by
        // motion.
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_screen(0.0, 30.0, 0.0, 1.0);
        node.set_listener_tracking(crate::spatial::TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 0.0,
        });
        node.set_listener_pose_target(
            Quat::from_euler_rad(90f32.to_radians(), 0.0, 0.0),
            Vec3::ZERO,
        );
        let frames = 1024;
        let mut l = vec![0.0f32; frames];
        l[64] = 1.0;
        let mut r = vec![0.0f32; frames];
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        let il = argmax_abs(planes[0]);
        let ir = argmax_abs(planes[1]);
        let expect = woodworth_samples(120f32.to_radians(), 48_000);
        assert!(ir > il, "right ear contralateral ({ir} vs {il})");
        assert!(
            ((ir - il) as f32 - expect).abs() <= 6.0,
            "ITD {} samples vs {expect}",
            ir - il
        );
        let e_l: f32 = planes[0].iter().map(|v| v * v).sum();
        let e_r: f32 = planes[1].iter().map(|v| v * v).sum();
        assert!(e_l > e_r * 2.0, "image left ({e_l} vs {e_r})");
    }

    #[test]
    fn converged_listener_motion_renders_deterministically() {
        // Once converged, the motion deactivates (a settled target renders
        // bit-identically across two fresh nodes — the same determinism
        // discipline as the voice-budget suite; the renderer itself is
        // stateful *within* a node, warm-up included, so the guarantee is
        // across equal initial states).
        let frames = 256;
        let target = Quat::from_euler_rad(20f32.to_radians(), 0.0, 0.0);
        let build = || -> SpatialNode {
            let mut node = SpatialNode::new(48_000.0);
            node.prepare(48_000.0, 2);
            node.set_enabled(true);
            node.set_listener_tracking(crate::spatial::TrackingConfig {
                smoothing_ms: 0.0,
                max_angular_rate_deg_s: 0.0,
            });
            node.set_listener_pose_target(target, Vec3::new(0.5, 0.5, 0.0));
            node
        };
        let run = |mut node: SpatialNode| -> (Vec<f32>, f32) {
            let mut l: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.01).sin()).collect();
            let mut r: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.02).cos()).collect();
            let mut planes: Vec<&mut [f32]> = vec![l.as_mut_slice(), r.as_mut_slice()];
            node.process_block_f32(&mut planes); // snap + converge
            let residual = node.listener_pose().0.angle_to(target);
            let out = planes.iter().flat_map(|p| p.to_vec()).collect();
            (out, residual)
        };
        let (a, res_a) = run(build());
        let (b, res_b) = run(build());
        assert!(res_a < 1e-3 && res_b < 1e-3, "motion converged");
        assert_eq!(a, b, "equal states render bit-identically");
    }

    #[test]
    fn program_gain_automation_reduces_a_program_object() {
        // FFI/engine automation (spec §47): a gain curve attached to one
        // program object overrides its level at the set automation clock.
        let render_energy = |node: &mut SpatialNode| -> f64 {
            let frames = 512;
            let mut l: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.01).sin()).collect();
            let mut r: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.02).cos()).collect();
            let mut planes: Vec<&mut [f32]> = vec![l.as_mut_slice(), r.as_mut_slice()];
            node.process_block_f32(&mut planes);
            planes
                .iter()
                .map(|p| p.iter().map(|&v| (v as f64).powi(2)).sum::<f64>())
                .sum::<f64>()
        };
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_config(
            &config::SpatialConfig {
                enabled: true,
                ..Default::default()
            },
            48_000.0,
        );
        node.apply_screen(0.0, 30.0, 0.0, 1.0);
        // Baseline: no gain automation -> both programme voices at unity.
        node.set_automation_time(10.0);
        let full = render_energy(&mut node);
        // Zero the Left programme voice via a gain curve evaluated at t=0.
        let curve = crate::spatial::CurveScalar::from_points(&[(0.0, 0.0), (1.0, 0.0)]).unwrap();
        node.set_program_automation(0, 0, Some(Arc::new(curve)));
        node.set_automation_time(0.0);
        let reduced = render_energy(&mut node);
        assert!(
            reduced < full * 0.9,
            "gain automation must cut a programme voice: {reduced} vs {full}"
        );
        // Clearing it restores both voices (the renderer's filter/ring state
        // carries warm-up between blocks, so we assert restoration is large
        // and near the baseline rather than bit-identical).
        node.set_program_automation(0, 0, None);
        let back = render_energy(&mut node);
        assert!(back > reduced, "automation cleared restores energy");
        assert!((back - full).abs() < 0.05 * full.max(1.0));
    }
}
