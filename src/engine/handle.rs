//! Safe, decoupled client handle for the audio engine (`EngineHandle`).
//!
//! `EngineHandle` provides a lightweight, cloneable API bridge between host
//! applications and the core audio engine. It operates completely through non-blocking
//! message passing (`crossbeam::channel::Sender<EngineCommand>`), lock-free
//! atomic telemetry reads (`ArcSwap<PlaybackInfo>`), and discrete engine events
//! (`crossbeam::channel::Receiver<EngineEvent>`), ensuring that the real-time
//! audio thread is never blocked by UI, networking, or database operations.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use crossbeam::channel::{Receiver, Sender};

use crate::buffer::{EngineCommand, PlaybackInfo, PlaybackState};
use crate::events::{EngineEvent, OutputEvent};
use crate::source::AudioSource;

/// A thread-safe, cloneable client handle to an active [`AudioEngine`].
#[derive(Clone)]
pub struct EngineHandle {
    cmd_tx: Sender<EngineCommand>,
    playback_info: Arc<ArcSwap<PlaybackInfo>>,
    event_rx: Receiver<EngineEvent>,
    /// Output device events — only present when `audio-output` is enabled.
    #[cfg(feature = "audio-output")]
    output_event_rx: Receiver<OutputEvent>,
    /// Shared real-time analyzer (levels + spectrum).
    analyzer: Arc<crate::dsp::AudioAnalyzer>,
}

impl std::fmt::Debug for EngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandle")
            .field("playback_state", &self.state())
            .field("current_source", &self.current_source())
            .finish()
    }
}

impl EngineHandle {
    /// Create a new `EngineHandle` (only called internally).
    #[allow(clippy::too_many_arguments, reason = "Internal-only constructor.")]
    pub fn new(
        cmd_tx: Sender<EngineCommand>,
        playback_info: Arc<ArcSwap<PlaybackInfo>>,
        event_rx: Receiver<EngineEvent>,
        #[cfg(feature = "audio-output")] output_event_rx: Receiver<OutputEvent>,
        analyzer: Arc<crate::dsp::AudioAnalyzer>,
    ) -> Self {
        Self {
            cmd_tx,
            playback_info,
            event_rx,
            #[cfg(feature = "audio-output")]
            output_event_rx,
            analyzer,
        }
    }

    /// Send a raw [`EngineCommand`] directly to the engine.
    #[inline]
    // The Err variant is the rejected command itself; boxing it would hide
    // the payload from callers who want to retry after a shutdown.
    #[allow(clippy::result_large_err)]
    pub fn send_command(
        &self,
        cmd: EngineCommand,
    ) -> Result<(), crossbeam::channel::SendError<EngineCommand>> {
        self.cmd_tx.send(cmd)
    }

    /// Access the underlying command sender channel.
    #[inline]
    pub fn command_sender(&self) -> &Sender<EngineCommand> {
        &self.cmd_tx
    }

    /// Access the discrete engine event receiver.
    #[inline]
    pub fn events(&self) -> &Receiver<EngineEvent> {
        &self.event_rx
    }

    /// Clone the event receiver for standalone asynchronous event listening.
    #[inline]
    pub fn clone_event_receiver(&self) -> Receiver<EngineEvent> {
        self.event_rx.clone()
    }

    // ── Transport & Source Controls ─────────────────────────────────────

    /// Open an explicit [`AudioSource`] (file, URI, or memory) for playback.
    pub fn open(&self, source: impl Into<AudioSource>) {
        let _ = self.send_command(EngineCommand::Open(source.into()));
    }

    /// Open a local file by path for playback.
    pub fn open_file(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::Open(AudioSource::File(path.into())));
    }

    /// Open a resource by URI (e.g. `"file:///path/to/song.flac"`).
    pub fn open_uri(&self, uri: impl Into<String>) {
        let _ = self.send_command(EngineCommand::Open(AudioSource::Uri(uri.into())));
    }

    /// Open in-memory byte buffer for playback with an optional format/extension hint.
    pub fn open_memory(&self, data: Vec<u8>, extension_hint: Option<String>) {
        let _ = self.send_command(EngineCommand::Open(AudioSource::Memory {
            data,
            extension_hint,
        }));
    }

    /// Pre-open the next audio source for seamless gapless / crossfade transition.
    pub fn prepare_next(&self, source: impl Into<AudioSource>) {
        let _ = self.send_command(EngineCommand::PrepareNext(source.into()));
    }

    /// Pre-open the next file by path for gapless / crossfade transition.
    pub fn prepare_next_file(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::PrepareNext(AudioSource::File(path.into())));
    }

    /// Pre-open next in-memory audio source for gapless / crossfade transition.
    pub fn prepare_next_memory(&self, data: Vec<u8>, extension_hint: Option<String>) {
        let _ = self.send_command(EngineCommand::PrepareNext(AudioSource::Memory {
            data,
            extension_hint,
        }));
    }

    // ── Playlist / Queue ────────────────────────────────────────────────

    /// Append a source to the end of the playback queue.
    pub fn enqueue(&self, source: impl Into<AudioSource>) {
        let _ = self.send_command(EngineCommand::Enqueue(source.into()));
    }

    /// Append a file to the end of the playback queue.
    pub fn enqueue_file(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::Enqueue(AudioSource::File(path.into())));
    }

    /// Remove the queue entry at `index`. Removing the current entry stops
    /// playback.
    pub fn remove_from_playlist(&self, index: usize) {
        let _ = self.send_command(EngineCommand::RemoveFromPlaylist(index));
    }

    /// Clear the playback queue (the current track keeps playing).
    pub fn clear_playlist(&self) {
        let _ = self.send_command(EngineCommand::ClearPlaylist);
    }

    /// Jump to queue entry `index` and start playing it.
    pub fn play_index(&self, index: usize) {
        let _ = self.send_command(EngineCommand::PlayIndex(index));
    }

    /// Skip to the next queue entry.
    pub fn next(&self) {
        let _ = self.send_command(EngineCommand::Next);
    }

    /// Skip to the previous queue entry.
    pub fn previous(&self) {
        let _ = self.send_command(EngineCommand::Previous);
    }

    /// Set the repeat mode (Off / All / One).
    pub fn set_repeat_mode(&self, mode: crate::playlist::RepeatMode) {
        let _ = self.send_command(EngineCommand::SetRepeatMode(mode));
    }

    /// Enable or disable shuffle.
    pub fn set_shuffle(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetShuffle(enabled));
    }

    /// Scan a file for EBU R128 / ReplayGain loudness and write the result
    /// back into its tags (requires the `tag-write` feature).
    pub fn write_loudness_tags(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::WriteLoudnessTags(path.into()));
    }

    /// Number of entries in the playback queue.
    pub fn playlist_len(&self) -> usize {
        self.playback_info.load().playlist_length
    }

    /// Index of the currently-playing queue entry, if any.
    pub fn playlist_index(&self) -> Option<usize> {
        self.playback_info.load().playlist_index
    }

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

    /// List currently available output devices for the default/active backend.
    #[cfg(feature = "audio-output")]
    pub fn available_devices(&self) -> Vec<String> {
        crate::output::cpal_devices::enumerate_devices(config::AudioBackend::default())
    }

    /// Open the active ASIO driver's manufacturer settings dialog.
    /// No-op when the current backend is not ASIO or the feature is not compiled in.
    pub fn open_asio_control_panel(&self) {
        let _ = self.send_command(EngineCommand::OpenAsioControlPanel);
    }

    /// Start capturing the system mix (WASAPI loopback on Windows) to a WAV
    /// file. `path` defaults to `capture.wav`; `device` selects the render
    /// endpoint (`None` = system default). Emits `CaptureStarted` or
    /// `CaptureError`. No-op on platforms without the `wasapi-native` feature.
    pub fn start_capture(&self, path: Option<std::path::PathBuf>, device: Option<String>) {
        let _ = self.send_command(EngineCommand::CaptureStart { path, device });
    }

    /// Stop the active system-audio capture and finalize its WAV file.
    /// Emits `CaptureStopped` (or `CaptureError` if none is active).
    pub fn stop_capture(&self) {
        let _ = self.send_command(EngineCommand::CaptureStop);
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

    /// Currently loaded audio source, if any.
    pub fn current_source(&self) -> Option<AudioSource> {
        self.playback_info.load().current_source.clone()
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

    /// Clone the output event receiver for standalone asynchronous device-event listening.
    /// Only available when the `audio-output` feature is enabled.
    #[cfg(feature = "audio-output")]
    #[inline]
    pub fn clone_output_event_receiver(&self) -> Receiver<OutputEvent> {
        self.output_event_rx.clone()
    }

    /// End-to-end audio pipeline latency in milliseconds.
    pub fn latency_ms(&self) -> f32 {
        self.playback_info.load().latency_ms
    }

    /// Shared real-time analyzer: peak/RMS meters and FFT spectrum updated
    /// continuously during playback. Poll [`crate::dsp::AudioAnalyzer::snapshot`]
    /// for the latest values.
    pub fn analyzer(&self) -> Arc<crate::dsp::AudioAnalyzer> {
        Arc::clone(&self.analyzer)
    }
}
