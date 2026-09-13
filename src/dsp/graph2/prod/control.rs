//! `Graph2ControlHandle` — the cloneable cross-thread control surface of
//! the [`Graph2Engine`], mirroring the arena's `GraphControlHandle`
//! one-to-one.
//!
//! Every command the production engine sends flows through here: the
//! handle enqueues onto the same per-node SPSC queues (`ControlBus`) the
//! embedded arena graph drains, so command semantics — block-boundary
//! application, sticky user-state mirroring, generation swaps — are the
//! production ones verbatim.

use crate::dsp::correction::CorrectionIrSet;
use crate::dsp::equalizer::EqBandParams;
use crate::dsp::graph2::prod::arena::{DuckState, GraphControlHandle, PanLaw};
use crate::dsp::limiter::LimiterMode;
use crate::dsp::loudness::{LoudnessMetadata, LoudnessMode};
use std::sync::Arc;

/// The cross-thread control surface of a [`super::Graph2Engine`]: one
/// cloneable handle per producer thread, sharing the engine's
/// `ControlBus` (per-node SPSC queues + the swap atomics).
///
/// Method-for-method mirror of `GraphControlHandle`; each call forwards
/// to the embedded production handle (the single implementation of the
/// queued control surface).
pub struct Graph2ControlHandle {
    /// The production handle (the arena's own control plane).
    pub(crate) inner: GraphControlHandle,
}

impl Graph2ControlHandle {
    pub(crate) fn new(inner: GraphControlHandle) -> Self {
        Self { inner }
    }

    // ── Meters / swap / state (audio-written atomics) ─────────────────────
    pub fn slot_meters(&self, slot: usize) -> (f32, f32) {
        self.inner.slot_meters(slot)
    }
    pub fn aux_meters(&self) -> (f32, f32) {
        self.inner.aux_meters()
    }
    pub fn aux_insert_state(&self) -> (bool, f32) {
        self.inner.aux_insert_state()
    }
    pub fn aux_state(&self) -> (bool, f32) {
        self.inner.aux_state()
    }
    pub fn correction_state(&self) -> (bool, f32) {
        self.inner.correction_state()
    }
    pub fn spatial_enabled(&self) -> bool {
        self.inner.spatial_enabled()
    }
    pub fn aux_send_peak(&self, slot: usize) -> f32 {
        self.inner.aux_send_peak(slot)
    }
    pub fn generation(&self) -> u64 {
        self.inner.generation()
    }
    pub fn reclaimed_count(&self) -> u64 {
        self.inner.reclaimed_count()
    }
    pub fn dropped_commands(&self) -> u64 {
        self.inner.dropped_commands()
    }

    // ── Queued commands ───────────────────────────────────────────────────
    pub fn set_volume(&self, volume: f32) {
        self.inner.set_volume(volume);
    }
    pub fn set_volume_db(&self, db: f32) {
        self.inner.set_volume_db(db);
    }
    pub fn set_balance(&self, balance: f32) {
        self.inner.set_balance(balance);
    }
    pub fn begin_seek_fadeout(&self) {
        self.inner.begin_seek_fadeout();
    }
    pub fn begin_seek_fadein(&self) {
        self.inner.begin_seek_fadein();
    }
    pub fn apply_loudness_metadata_outgoing(&self, metadata: Option<LoudnessMetadata>) {
        self.inner.apply_loudness_metadata_outgoing(metadata);
    }
    pub fn apply_loudness_metadata_incoming(&self, metadata: Option<LoudnessMetadata>) {
        self.inner.apply_loudness_metadata_incoming(metadata);
    }
    pub fn set_limiter_enabled(&self, enabled: bool) {
        self.inner.set_limiter_enabled(enabled);
    }
    pub fn set_limiter_mode(&self, mode: LimiterMode) {
        self.inner.set_limiter_mode(mode);
    }
    pub fn set_limiter_params(
        &self,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool,
    ) {
        self.inner
            .set_limiter_params(lookahead_ms, attack_ms, release_ms, ceiling_db, soft_clip);
    }
    pub fn set_limiter_true_peak(&self, enabled: bool) {
        self.inner.set_limiter_true_peak(enabled);
    }
    pub fn set_preamp_db(&self, db: f32) {
        self.inner.set_preamp_db(db);
    }
    pub fn set_bass_shelf(&self, gain_db: f32) {
        self.inner.set_bass_shelf(gain_db);
    }
    pub fn set_treble_shelf(&self, gain_db: f32) {
        self.inner.set_treble_shelf(gain_db);
    }
    pub fn set_eq_enabled(&self, enabled: bool) {
        self.inner.set_eq_enabled(enabled);
    }
    pub fn set_eq_auto_headroom(&self, enabled: bool) {
        self.inner.set_eq_auto_headroom(enabled);
    }
    pub fn set_eq_band(&self, index: usize, params: EqBandParams) {
        self.inner.set_eq_band(index, params);
    }
    pub fn set_midside_eq(&self, enabled: bool) {
        self.inner.set_midside_eq(enabled);
    }
    pub fn set_convolution_wet_mix(&self, mix: f32) {
        self.inner.set_convolution_wet_mix(mix);
    }
    pub fn set_stereo_width(&self, width: f32) {
        self.inner.set_stereo_width(width);
    }
    pub fn set_stereo_enhancer_enabled(&self, enabled: bool) {
        self.inner.set_stereo_enhancer_enabled(enabled);
    }
    pub fn set_crossfeed_enabled(&self, enabled: bool) {
        self.inner.set_crossfeed_enabled(enabled);
    }
    pub fn set_crossfeed_profile(&self, profile: config::CrossfeedProfile) {
        self.inner.set_crossfeed_profile(profile);
    }
    pub fn set_crossfeed_custom_params(&self, frequency_hz: f32, q: f32, delay_ms: f32) {
        self.inner
            .set_crossfeed_custom_params(frequency_hz, q, delay_ms);
    }
    pub fn set_compressor_enabled(&self, enabled: bool) {
        self.inner.set_compressor_enabled(enabled);
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
        self.inner.set_compressor_band_params(
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
        self.inner
            .set_compressor_band_features(band, knee_db, detector, stereo_link);
    }
    pub fn set_loudness_mode(&self, mode: LoudnessMode) {
        self.inner.set_loudness_mode(mode);
    }
    pub fn set_input_gain(&self, input: u8, gain: f32) {
        self.inner.set_input_gain(input, gain);
    }
    pub fn set_input_gain_db(&self, input: u8, db: f32) {
        self.inner.set_input_gain_db(input, db);
    }
    pub fn set_input_balance(&self, input: u8, balance: f32) {
        self.inner.set_input_balance(input, balance);
    }
    pub fn set_input_pan(&self, input: u8, pan: f32) {
        self.inner.set_input_pan(input, pan);
    }
    pub fn set_input_pan_law(&self, input: u8, law: PanLaw) {
        self.inner.set_input_pan_law(input, law);
    }
    pub fn set_slot_trim(&self, input: u8, channel: usize, gain_db: f32, invert: bool) {
        self.inner.set_slot_trim(input, channel, gain_db, invert);
    }
    pub fn set_slot_send(&self, input: u8, master_gain: f32, aux_gain: f32) {
        self.inner.set_slot_send(input, master_gain, aux_gain);
    }
    pub fn set_aux(&self, enabled: bool, return_gain: f32) {
        self.inner.set_aux(enabled, return_gain);
    }
    pub fn set_aux_insert(&self, enabled: bool, wet_mix: f32) {
        self.inner.set_aux_insert(enabled, wet_mix);
    }
    pub fn set_correction_enabled(&self, enabled: bool) {
        self.inner.set_correction_enabled(enabled);
    }
    pub fn set_correction_depth(&self, depth: f32) {
        self.inner.set_correction_depth(depth);
    }
    pub fn load_correction_ir(&self, set: Arc<CorrectionIrSet>) {
        self.inner.load_correction_ir(set.clone());
    }
    pub fn set_spatial_enabled(&self, enabled: bool) {
        self.inner.set_spatial_enabled(enabled);
    }
    pub fn set_spatial_screen(
        &self,
        center_azimuth_deg: f32,
        half_width_deg: f32,
        elevation_deg: f32,
        gain: f32,
    ) {
        self.inner
            .set_spatial_screen(center_azimuth_deg, half_width_deg, elevation_deg, gain);
    }

    /// Configure the room (see `GraphControlHandle::set_spatial_room`).
    #[allow(clippy::too_many_arguments)]
    pub fn set_spatial_room(
        &self,
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
        self.inner.set_spatial_room(
            enabled,
            width,
            depth,
            height,
            absorption,
            reflection_order,
            rt60_ms,
            late_mix,
            late_distance,
            wet,
        );
    }

    /// Configure the scene-wide air-absorption model (see
    /// `GraphControlHandle::set_spatial_air`).
    pub fn set_spatial_air(&self, air: crate::spatial::level::AirAbsorption) {
        self.inner.set_spatial_air(air);
    }
    pub fn set_spatial_listener(&self, yaw_deg: f32, pitch_deg: f32, roll_deg: f32) {
        self.inner
            .set_spatial_listener(yaw_deg, pitch_deg, roll_deg);
    }
    /// Phase-51 listener motion: target listener pose (orientation +
    /// position) the spatial node glides toward per block.
    pub fn set_spatial_listener_pose(
        &self,
        orientation: crate::spatial::math::Quat,
        position: crate::spatial::math::Vec3,
    ) {
        self.inner.set_spatial_listener_pose(orientation, position);
    }
    /// Phase-51 listener motion: smoothing policy for the listener glide.
    pub fn set_spatial_listener_tracking(&self, smoothing_ms: f32, max_rate_deg_s: f32) {
        self.inner
            .set_spatial_listener_tracking(smoothing_ms, max_rate_deg_s);
    }
    pub fn set_input_mute(&self, input: u8, mute: bool) {
        self.inner.set_input_mute(input, mute);
    }
    pub fn set_input_active(&self, input: u8, active: bool) {
        self.inner.set_input_active(input, active);
    }
    pub fn set_duck(&self, cfg: Option<DuckState>) {
        self.inner.set_duck(cfg);
    }
    pub fn set_slot_automation(
        &self,
        input: u8,
        target: crate::dsp::graph2::prod::arena::nodes::AutomationTarget,
        points: &[crate::dsp::graph2::prod::arena::nodes::AutomationPoint],
    ) {
        self.inner.set_slot_automation(input, target, points);
    }
    pub fn clear_slot_automation(&self, input: u8) {
        self.inner.clear_slot_automation(input);
    }
    pub fn begin_crossfade_frames(&self, duration_frames: usize) {
        self.inner.begin_crossfade_frames(duration_frames);
    }
    pub fn begin_fade_frames(&self, duration_frames: usize) {
        self.inner.begin_fade_frames(duration_frames);
    }
    pub fn begin_playing(&self) {
        self.inner.begin_playing();
    }
    pub fn set_crossfade_curve(&self, curve: config::CrossfadeCurve) {
        self.inner.set_crossfade_curve(curve);
    }
    pub fn set_crossfade_enabled(&self, enabled: bool) {
        self.inner.set_crossfade_enabled(enabled);
    }
    pub fn set_crossfade_duration_frames(&self, frames: usize) {
        self.inner.set_crossfade_duration_frames(frames);
    }

    /// Publish a generation to the active engine.
    pub fn publish_generation(&self, gen: Box<crate::dsp::graph2::prod::GraphGeneration>) {
        self.inner.publish_generation(gen);
    }

    /// Reclaim a retired generation on the control path.
    pub fn reclaim_retired(&self) {
        self.inner.reclaim_retired();
    }
}
