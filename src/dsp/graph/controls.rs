//! Symmetric control surface for [`DspGraph`], mirroring
//! `src/dsp/pipeline/controls.rs` so the graph can be driven with the same
//! API as the production pipeline (used by the fidelity equivalence harness
//! and by hosts that want a node-based alternative). Methods delegate to the
//! typed arena accessors (see `access.rs`).
//!
//! Lifecycle / mode toggles that already live in `lifecycle.rs`
//! (`set_precision_mode`, `set_bit_perfect`, `set_dop_bypass`, `set_speed`,
//! `update_sample_rate`, `reset`, …) are intentionally not duplicated here.

use super::*;
use crate::dsp::{
    equalizer::EqBandParams,
    limiter::LimiterMode,
    loudness::{LoudnessMetadata, LoudnessMode},
};

impl DspGraph {
    // ── Volume / gain ──────────────────────────────────────────────────────

    pub fn set_volume(&mut self, volume: f32) {
        self.volume_mut().processor.set_gain(volume.clamp(0.0, 1.0));
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

    /// Set volume directly in dB ([-60.0, 0.0], full mute to unity). Values
    /// below -60 dB are clamped to -60.
    pub fn set_volume_db(&mut self, db: f32) {
        if !db.is_finite() {
            log::warn!("DspGraph::set_volume_db: non-finite value {}; ignoring", db);
            return;
        }
        let linear = Self::volume_db_to_linear(db);
        self.volume_mut().processor.set_gain(linear);
    }

    /// Current volume as dB (useful for UI display).
    pub fn volume_db(&self) -> f32 {
        Self::volume_linear_to_db(self.volume().processor.current_gain())
    }

    pub fn set_balance(&mut self, balance: f32) {
        self.balance_mut().set_balance(balance);
    }

    // ── Seek / transition fades ────────────────────────────────────────────

    pub fn begin_seek_fadeout(&mut self) {
        self.seek_fade_mut().fade.fade_out();
    }

    pub fn begin_seek_fadein(&mut self) {
        self.seek_fade_mut().fade.fade_in();
    }

    pub fn is_seek_fadeout_complete(&self) -> bool {
        self.seek_fade().fade.is_faded_out()
    }

    // ── Loudness metadata ──────────────────────────────────────────────────

    pub fn apply_loudness_metadata_outgoing(&mut self, metadata: Option<LoudnessMetadata>) {
        self.out_loudness_mut()
            .normalizer
            .set_track_metadata(&metadata.unwrap_or_default());
    }

    pub fn apply_loudness_metadata_incoming(&mut self, metadata: Option<LoudnessMetadata>) {
        self.in_loudness_mut()
            .normalizer
            .set_track_metadata(&metadata.unwrap_or_default());
    }

    // ── Limiter ────────────────────────────────────────────────────────────

    pub fn set_limiter_enabled(&mut self, enabled: bool) {
        self.limiter_mut().limiter.set_enabled(enabled);
    }

    /// Set the limiter mode (Transparent brick-wall or Saturate soft-clip).
    pub fn set_limiter_mode(&mut self, mode: LimiterMode) {
        self.limiter_mut().limiter.set_mode(mode);
    }

    pub fn set_limiter_params(
        &mut self,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool,
    ) {
        self.limiter_mut().limiter.set_lookahead(lookahead_ms);
        self.limiter_mut().limiter.set_attack(attack_ms);
        self.limiter_mut().limiter.set_release(release_ms);
        self.limiter_mut().limiter.set_ceiling_db(ceiling_db);
        self.limiter_mut().limiter.set_soft_clip(soft_clip);
    }

    /// Enable or disable true-peak (inter-sample peak) detection on the
    /// limiter. See `LookaheadLimiter::enable_true_peak` for details.
    pub fn set_limiter_true_peak(&mut self, enabled: bool) {
        self.limiter_mut().limiter.enable_true_peak(enabled);
    }

    /// Whether true-peak detection is currently active on the limiter.
    pub fn limiter_true_peak_enabled(&self) -> bool {
        self.limiter().limiter.true_peak_enabled()
    }

    /// Current limiter gain reduction in dB (≤ 0; 0 = no reduction).
    pub fn limiter_gain_reduction_db(&self) -> f32 {
        self.limiter().limiter.gain_reduction_db()
    }

    /// Maximum true-peak observed by the limiter since the last reset.
    pub fn limiter_max_true_peak_dbtp(&self) -> f32 {
        self.limiter().limiter.max_true_peak_dbtp()
    }

    // ── EQ ─────────────────────────────────────────────────────────────────

    pub fn set_preamp_db(&mut self, db: f32) {
        self.eq_mut().eq.set_preamp_db(db);
    }

    pub fn set_bass_shelf(&mut self, gain_db: f32) {
        self.eq_mut().eq.set_bass_shelf(gain_db);
    }

    pub fn set_treble_shelf(&mut self, gain_db: f32) {
        self.eq_mut().eq.set_treble_shelf(gain_db);
    }

    pub fn set_eq_enabled(&mut self, enabled: bool) {
        self.eq_mut().eq.set_enabled(enabled);
    }

    pub fn set_eq_auto_headroom(&mut self, enabled: bool) {
        self.eq_mut().eq.set_auto_headroom(enabled);
    }

    pub fn set_eq_band(&mut self, index: usize, params: EqBandParams) {
        self.eq_mut().eq.set_band(index, params);
    }

    pub fn eq_num_bands(&self) -> usize {
        self.eq().eq.num_bands()
    }

    pub fn set_midside_eq(&mut self, enabled: bool) {
        self.eq_mut().midside_enabled = enabled;
    }

    pub fn is_midside_eq(&self) -> bool {
        self.eq().midside_enabled
    }

    // ── Convolution ────────────────────────────────────────────────────────

    pub fn set_convolution_wet_mix(&mut self, mix: f32) {
        self.convolution_mut().engine.set_wet_mix(mix);
    }

    // ── Stereo image ───────────────────────────────────────────────────────

    pub fn set_stereo_width(&mut self, width: f32) {
        let normalized = if width > 2.0 { width / 100.0 } else { width };
        self.stereo_mut().enhancer.set_width(normalized);
        self.stereo_mut()
            .enhancer
            .set_enabled((normalized - 1.0).abs() > 0.001);
    }

    pub fn set_stereo_enhancer_enabled(&mut self, enabled: bool) {
        self.stereo_mut().enhancer.set_enabled(enabled);
    }

    // ── Crossfeed ──────────────────────────────────────────────────────────

    pub fn set_crossfeed_enabled(&mut self, enabled: bool) {
        self.crossfeed_mut().crossfeed.set_enabled(enabled);
    }

    pub fn set_crossfeed_profile(&mut self, profile: config::CrossfeedProfile) {
        self.crossfeed_mut().crossfeed.set_profile(profile);
    }

    pub fn set_crossfeed_custom_params(&mut self, frequency_hz: f32, q: f32, delay_ms: f32) {
        self.crossfeed_mut()
            .crossfeed
            .set_custom_params(frequency_hz, q, delay_ms);
    }

    // ── Multiband compressor ───────────────────────────────────────────────

    pub fn set_compressor_enabled(&mut self, enabled: bool) {
        self.dynamics_mut().compressor.set_enabled(enabled);
    }

    pub fn set_compressor_band_params(
        &mut self,
        band: usize,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    ) {
        self.dynamics_mut().compressor.set_band_params(
            band,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_gain_db,
        );
    }

    /// Set a compressor band's detector features: soft-knee width (dB),
    /// detector mode and stereo linking.
    pub fn set_compressor_band_features(
        &mut self,
        band: usize,
        knee_db: f32,
        detector: config::CompressorDetector,
        stereo_link: bool,
    ) {
        self.dynamics_mut()
            .compressor
            .set_band_features(band, knee_db, detector, stereo_link);
    }

    // ── Loudness normalization ─────────────────────────────────────────────

    pub fn set_loudness_mode(&mut self, mode: LoudnessMode) {
        self.out_loudness_mut().normalizer.set_mode(mode);
        self.in_loudness_mut().normalizer.set_mode(mode);
    }
}
