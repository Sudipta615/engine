//! Safe, decoupled client handle for the audio engine (`EngineHandle`).
//!
//! `EngineHandle` provides a lightweight, cloneable API bridge between UI/controller
//! modules and the core audio engine. It operates completely through non-blocking
//! message passing (`crossbeam::channel::Sender<EngineCommand>`) and lock-free
//! atomic telemetry reads (`ArcSwap<PlaybackInfo>`), ensuring that the real-time
//! audio thread is never blocked by UI or network activity.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use crossbeam::channel::Sender;

use crate::buffer::{EngineCommand, PlaybackInfo, PlaybackState};

/// A thread-safe, cloneable client handle to an active [`AudioEngine`].
#[derive(Clone)]
pub struct EngineHandle {
    cmd_tx: Sender<EngineCommand>,
    playback_info: Arc<ArcSwap<PlaybackInfo>>,
}

impl std::fmt::Debug for EngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandle")
            .field("playback_state", &self.state())
            .finish()
    }
}

impl EngineHandle {
    /// Create a new `EngineHandle` from a command transmitter and shared telemetry.
    pub fn new(cmd_tx: Sender<EngineCommand>, playback_info: Arc<ArcSwap<PlaybackInfo>>) -> Self {
        Self {
            cmd_tx,
            playback_info,
        }
    }

    /// Send a raw [`EngineCommand`] directly to the engine.
    #[inline]
    pub fn send_command(&self, cmd: EngineCommand) -> Result<(), crossbeam::channel::SendError<EngineCommand>> {
        self.cmd_tx.send(cmd)
    }

    /// Access the underlying command sender channel.
    #[inline]
    pub fn command_sender(&self) -> &Sender<EngineCommand> {
        &self.cmd_tx
    }

    // ── Transport Controls ──────────────────────────────────────────────

    /// Start or resume playback.
    pub fn play(&self) {
        let _ = self.send_command(EngineCommand::Play);
    }

    /// Pause playback.
    pub fn pause(&self) {
        let _ = self.send_command(EngineCommand::Pause);
    }

    /// Stop playback and reset playhead to beginning.
    pub fn stop(&self) {
        let _ = self.send_command(EngineCommand::Stop);
    }

    /// Seek to a target position in seconds.
    pub fn seek(&self, position_secs: f32) {
        let _ = self.send_command(EngineCommand::Seek(position_secs));
    }

    /// Advance to the next track.
    pub fn next_track(&self) {
        let _ = self.send_command(EngineCommand::NextTrack);
    }

    /// Go back to the previous track.
    pub fn prev_track(&self) {
        let _ = self.send_command(EngineCommand::PrevTrack);
    }

    /// Open a file URI (e.g. `"file:///path/to/song.flac"`).
    pub fn open_uri(&self, uri: impl Into<String>) {
        let _ = self.send_command(EngineCommand::OpenUri(uri.into()));
    }

    /// Load a track by numeric ID.
    pub fn load_track(&self, track_id: u64) {
        let _ = self.send_command(EngineCommand::LoadTrack(track_id));
    }

    /// Pre-open the next track for seamless gapless / crossfade transition.
    pub fn prepare_next_track(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::PrepareNextTrack(path.into()));
    }

    /// Gracefully shutdown the engine worker thread.
    pub fn shutdown(&self) {
        let _ = self.send_command(EngineCommand::Shutdown);
    }

    // ── Volume & Gain Controls ──────────────────────────────────────────

    /// Set linear volume `[0.0, 1.0]`.
    pub fn set_volume(&self, linear_volume: f32) {
        let _ = self.send_command(EngineCommand::SetVolume(linear_volume));
    }

    /// Set perceptual volume directly in dB `[-60.0, 0.0]`.
    pub fn set_volume_db(&self, db: f32) {
        let _ = self.send_command(EngineCommand::SetVolumeDb(db));
    }

    /// Set volume mode (Hardware endpoint vs Software DSP).
    pub fn set_volume_mode(&self, mode: config::VolumeMode) {
        let _ = self.send_command(EngineCommand::SetVolumeMode(mode));
    }

    /// Set balance control `[-1.0 (Left) .. 1.0 (Right)]`.
    pub fn set_balance(&self, balance: f32) {
        let _ = self.send_command(EngineCommand::SetBalance(balance));
    }

    /// Set preamp gain in dB.
    pub fn set_preamp(&self, db: f32) {
        let _ = self.send_command(EngineCommand::SetPreamp(db));
    }

    // ── Speed & Pitch Controls ──────────────────────────────────────────

    /// Set playback playback speed multiplier (e.g. 1.0 = normal, 1.5 = 1.5x).
    pub fn set_speed(&self, speed: f32) {
        let _ = self.send_command(EngineCommand::SetSpeed(speed));
    }

    /// Set playback speed mode (Varispeed, TimeStretch, PitchShift).
    pub fn set_speed_mode(&self, mode: config::SpeedMode) {
        let _ = self.send_command(EngineCommand::SetSpeedMode(mode));
    }

    /// Set pitch shift in semitones `[-24.0, +24.0]`.
    pub fn set_pitch(&self, semitones: f32) {
        let _ = self.send_command(EngineCommand::SetPitch(semitones));
    }

    // ── Equalizer & Audio Shaping ───────────────────────────────────────

    /// Enable or disable the parametric EQ.
    pub fn set_eq_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetEqEnabled(enabled));
    }

    /// Load a complete EQ preset (e.g. AutoEQ).
    pub fn set_eq_preset(&self, preset: config::EqPreset) {
        let _ = self.send_command(EngineCommand::SetEqPreset(preset));
    }

    /// Set specific parametric EQ band parameters.
    pub fn set_eq_band(&self, index: usize, frequency: f32, gain_db: f32, q: f32, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetEqBand {
            index,
            frequency,
            gain_db,
            q,
            enabled,
        });
    }

    /// Configure Graphic EQ layout (10, 15, or 31 bands).
    pub fn set_graphic_eq_layout(&self, layout: config::GraphicEqLayout) {
        let _ = self.send_command(EngineCommand::SetGraphicEqLayout(layout));
    }

    /// Adjust a Graphic EQ slider in dB.
    pub fn set_graphic_eq_slider(&self, band: usize, gain_db: f32) {
        let _ = self.send_command(EngineCommand::SetGraphicEqSlider { band, gain_db });
    }

    /// Set Graphic EQ layer enabled.
    pub fn set_graphic_eq_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetGraphicEqEnabled(enabled));
    }

    /// Set stereo enhancer width `[0.0 .. 2.0]`.
    pub fn set_stereo_width(&self, width: f32) {
        let _ = self.send_command(EngineCommand::SetStereoWidth(width));
    }

    // ── Spatial & Headphone Processing ──────────────────────────────────

    /// Enable or disable Headphone Crossfeed.
    pub fn set_crossfeed_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetCrossfeedEnabled(enabled));
    }

    /// Set crossfeed acoustic profile (Bauer, ChuMoy, Jmeier, Custom).
    pub fn set_crossfeed_profile(&self, profile: config::CrossfeedProfile) {
        let _ = self.send_command(EngineCommand::SetCrossfeedProfile(profile));
    }

    /// Set custom crossfeed parameters (frequency cut-off, Q, and ITD delay in ms).
    pub fn set_crossfeed_custom_params(&self, frequency_hz: f32, q: f32, delay_ms: f32) {
        let _ = self.send_command(EngineCommand::SetCrossfeedCustomParams {
            frequency_hz,
            q,
            delay_ms,
        });
    }

    // ── Multichannel & Spatial Management ───────────────────────────────

    /// Configure channel mix / upmix / downmix template or custom matrix.
    pub fn set_channel_mix(&self, config: config::ChannelMixConfig) {
        let _ = self.send_command(EngineCommand::SetChannelMix(config));
    }

    /// Configure multichannel preservation policy.
    pub fn set_channel_policy(&self, policy: config::ChannelPolicy) {
        let _ = self.send_command(EngineCommand::SetChannelPolicy(policy));
    }

    /// Configure per-channel trim (gain, fractional delay, polarity).
    pub fn set_channel_trim(&self, config: config::ChannelTrimConfig) {
        let _ = self.send_command(EngineCommand::SetChannelTrim(config));
    }

    /// Configure multichannel routing matrix.
    pub fn set_channel_routing(&self, config: config::ChannelRoutingConfig) {
        let _ = self.send_command(EngineCommand::SetChannelRouting(config));
    }

    /// Configure per-channel parametric EQ for multichannel setups.
    pub fn set_channel_eq(&self, config: config::ChannelEqConfig) {
        let _ = self.send_command(EngineCommand::SetChannelEq(config));
    }

    /// Configure LFE subwoofer channel parameters.
    pub fn set_lfe_config(&self, config: config::LfeConfig) {
        let _ = self.send_command(EngineCommand::SetLfeConfig(config));
    }

    /// Configure bass management crossover for main speakers.
    pub fn set_bass_management(&self, config: config::BassManagementConfig) {
        let _ = self.send_command(EngineCommand::SetBassManagement(config));
    }

    // ── Output, Device & Audiophile Settings ────────────────────────────

    /// Select audio backend (CPAL, ALSA exclusive, WASAPI exclusive, CoreAudio hog, ASIO).
    pub fn set_output_backend(&self, backend: config::AudioBackend) {
        let _ = self.send_command(EngineCommand::SetOutputBackend(backend));
    }

    /// Select output device by name (or `None` for default).
    pub fn set_output_device(&self, device_name: Option<String>) {
        let _ = self.send_command(EngineCommand::SetOutputDevice(device_name));
    }

    /// Set sample rate policy (TrackNative, DevicePreferred, Fixed, etc.).
    pub fn set_sample_rate_policy(&self, policy: config::SampleRatePolicy) {
        let _ = self.send_command(EngineCommand::SetSampleRatePolicy(policy));
    }

    /// Enable or disable bit-perfect mode.
    pub fn set_bit_perfect(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetBitPerfect(enabled));
    }

    /// Enable or disable TPDF dither.
    pub fn set_dither_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetDitherEnabled(enabled));
    }

    /// Set resampler quality (Fast, Balanced, High, Audiophile).
    pub fn set_resampler_quality(&self, quality: config::ResamplerQuality) {
        let _ = self.send_command(EngineCommand::SetResamplerQuality(quality));
    }

    /// Set limiter mode (Transparent vs Saturate).
    pub fn set_limiter_mode(&self, mode: crate::dsp::limiter::LimiterMode) {
        let _ = self.send_command(EngineCommand::SetLimiterMode(mode));
    }

    /// Enable or disable True-Peak FIR oversampled limiting.
    pub fn set_limiter_true_peak(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetLimiterTruePeak(enabled));
    }

    // ── Telemetry & State Inspection ────────────────────────────────────

    /// Fetch an atomic, lock-free snapshot of current [`PlaybackInfo`].
    pub fn playback_info(&self) -> PlaybackInfo {
        (**self.playback_info.load()).clone()
    }

    /// Check if audio is actively playing.
    pub fn is_playing(&self) -> bool {
        self.playback_info.load().state == PlaybackState::Playing
    }

    /// Current playback state (`Playing`, `Paused`, `Stopped`, `Buffering`).
    pub fn state(&self) -> PlaybackState {
        self.playback_info.load().state
    }

    /// Current playhead position at the decoder in seconds.
    pub fn position_secs(&self) -> f32 {
        self.playback_info.load().position_secs
    }

    /// Current latency-compensated playhead position (what is heard at DAC) in seconds.
    pub fn position_secs_compensated(&self) -> f32 {
        self.playback_info.load().position_secs_compensated
    }

    /// Total duration of the currently playing track in seconds.
    pub fn duration_secs(&self) -> f32 {
        self.playback_info.load().duration_secs
    }

    /// Current volume level `[0.0, 1.0]`.
    pub fn volume(&self) -> f32 {
        self.playback_info.load().volume
    }

    /// Current playback speed multiplier.
    pub fn speed(&self) -> f32 {
        self.playback_info.load().speed
    }

    /// End-to-end audio pipeline latency in milliseconds.
    pub fn latency_ms(&self) -> f32 {
        self.playback_info.load().latency_ms
    }
}
