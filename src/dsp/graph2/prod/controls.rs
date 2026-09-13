//! The queued control mutators on [`Graph2Engine`]: every `set_*` /
//! `begin_*` method that exists on the arena's `DspGraph`, defined
//! explicitly on this type (a `Deref`-style forward would not carry the
//! method through `Deref` — `Deref` only forwards *field/method access*
//! on the target, and callers hold a `Graph2Engine`).
//!
//! Read-only getters resolve through `Deref<Target = DspGraph>`.
//! Accessor-style mutations (`eq_mut()`, `timestretch_mut()`,
//! `routing_mut()`, `convolution_mut()`, `spatial_mut()`, …) go through
//! [`super::Graph2Engine::with_graph`].

use super::Graph2Engine;
use crate::dsp::equalizer::EqBandParams;
use crate::dsp::graph2::prod::arena::nodes::{
    AutomationPoint, AutomationTarget, DuckState, PanLaw,
};
use crate::dsp::limiter::LimiterMode;
use crate::dsp::loudness::{LoudnessMetadata, LoudnessMode};

macro_rules! forward {
    ($(
        $(#[$meta:meta])*
        $name:ident($($arg:ident: $ty:ty),* $(,)?);
    )*) => {
        impl Graph2Engine {
            $(
                $(#[$meta])*
                pub fn $name(&self, $($arg: $ty),*) {
                    self.inner.$name($($arg),*);
                }
            )*
        }
    };
}

// The queued control surface — each method forwards to the active graph.
// Argument types are `Clone` (the macro-generated signatures match
// `DspGraph`'s exactly).
forward! {
    set_volume(volume: f32);
    set_volume_db(db: f32);
    set_balance(balance: f32);
    begin_seek_fadeout();
    begin_seek_fadein();
    apply_loudness_metadata_outgoing(metadata: Option<LoudnessMetadata>);
    apply_loudness_metadata_incoming(metadata: Option<LoudnessMetadata>);
    set_limiter_enabled(enabled: bool);
    set_limiter_mode(mode: LimiterMode);
    set_preamp_db(db: f32);
    set_bass_shelf(gain_db: f32);
    set_treble_shelf(gain_db: f32);
    set_eq_enabled(enabled: bool);
    set_eq_auto_headroom(enabled: bool);
    set_midside_eq(enabled: bool);
    set_convolution_wet_mix(mix: f32);
    set_stereo_width(width: f32);
    set_stereo_enhancer_enabled(enabled: bool);
    set_crossfeed_enabled(enabled: bool);
    set_crossfeed_profile(profile: config::CrossfeedProfile);
    set_compressor_enabled(enabled: bool);
    set_loudness_mode(mode: LoudnessMode);
    set_input_gain(input: u8, gain: f32);
    set_input_gain_db(input: u8, db: f32);
    set_input_balance(input: u8, balance: f32);
    set_input_pan(input: u8, pan: f32);
    set_input_pan_law(input: u8, law: PanLaw);
    set_slot_send(input: u8, master_gain: f32, aux_gain: f32);
    set_aux(enabled: bool, return_gain: f32);
    set_aux_insert(enabled: bool, wet_mix: f32);
    set_correction_enabled(enabled: bool);
    set_correction_depth(depth: f32);
    set_spatial_enabled(enabled: bool);
    set_spatial_listener(yaw_deg: f32, pitch_deg: f32, roll_deg: f32);
    set_input_mute(input: u8, mute: bool);
    set_input_active(input: u8, active: bool);
    set_duck(cfg: Option<DuckState>);
    begin_playing();
    set_crossfade_curve(curve: config::CrossfadeCurve);
    set_crossfade_enabled(enabled: bool);
}

impl Graph2Engine {
    // The multi-arg queued commands (too many parameters for the macro's
    // pattern to stay readable) — explicit forwards.

    /// Toggle the limiter's true-peak detector. Mirrors
    /// `DspGraph::set_limiter_true_peak`.
    pub fn set_limiter_true_peak(&self, enabled: bool) {
        self.inner.set_limiter_true_peak(enabled);
    }

    /// Set limiter params. Mirrors `DspGraph::set_limiter_params`.
    #[allow(clippy::too_many_arguments)]
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

    /// Set one EQ band. Mirrors `DspGraph::set_eq_band`.
    pub fn set_eq_band(&self, index: usize, params: EqBandParams) {
        self.inner.set_eq_band(index, params);
    }

    /// Set crossfeed custom params. Mirrors
    /// `DspGraph::set_crossfeed_custom_params`.
    pub fn set_crossfeed_custom_params(&self, frequency_hz: f32, q: f32, delay_ms: f32) {
        self.inner
            .set_crossfeed_custom_params(frequency_hz, q, delay_ms);
    }

    /// Set compressor band params. Mirrors
    /// `DspGraph::set_compressor_band_params`.
    #[allow(clippy::too_many_arguments)]
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

    /// Set compressor band features. Mirrors
    /// `DspGraph::set_compressor_band_features`.
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

    /// Set one channel's trim. Mirrors `DspGraph::set_slot_trim`.
    pub fn set_slot_trim(&self, input: u8, channel: usize, gain_db: f32, invert: bool) {
        self.inner.set_slot_trim(input, channel, gain_db, invert);
    }

    /// Set the virtual screen. Mirrors `DspGraph::set_spatial_screen`.
    #[allow(clippy::too_many_arguments)]
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

    /// Configure the room. Mirrors `DspGraph::set_spatial_room`.
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

    /// Configure the scene-wide air-absorption model. Mirrors
    /// `DspGraph::set_spatial_air`.
    pub fn set_spatial_air(&self, air: crate::spatial::level::AirAbsorption) {
        self.inner.set_spatial_air(air);
    }

    /// Replace a slot's automation track. Mirrors
    /// `DspGraph::set_slot_automation`.
    pub fn set_slot_automation(
        &self,
        input: u8,
        target: AutomationTarget,
        points: &[AutomationPoint],
    ) {
        self.inner.set_slot_automation(input, target, points);
    }

    /// Remove a slot's automation track. Mirrors
    /// `DspGraph::clear_slot_automation`.
    pub fn clear_slot_automation(&self, input: u8) {
        self.inner.clear_slot_automation(input);
    }

    /// Begin a crossfade over `duration_ms`. Mirrors
    /// `DspGraph::begin_crossfade`.
    pub fn begin_crossfade(&self, duration_ms: u64) {
        self.inner.begin_crossfade(duration_ms);
    }

    /// Begin a sequential fade over `duration_ms`. Mirrors
    /// `DspGraph::begin_fade`.
    pub fn begin_fade(&self, duration_ms: u64) {
        self.inner.begin_fade(duration_ms);
    }

    /// Set the crossfade duration. Mirrors
    /// `DspGraph::set_crossfade_duration_ms`.
    pub fn set_crossfade_duration_ms(&self, duration_ms: u64) {
        self.inner.set_crossfade_duration_ms(duration_ms);
    }

    /// Begin a crossfade by frames. Mirrors
    /// `GraphControlHandle::begin_crossfade_frames` (the `DspGraph` shell
    /// exposes only the ms form; the frames form goes through the handle).
    pub fn begin_crossfade_frames(&self, duration_frames: usize) {
        self.inner
            .control_handle()
            .begin_crossfade_frames(duration_frames);
    }

    /// Begin a fade by frames. Mirrors
    /// `GraphControlHandle::begin_fade_frames`.
    pub fn begin_fade_frames(&self, duration_frames: usize) {
        self.inner
            .control_handle()
            .begin_fade_frames(duration_frames);
    }
}
