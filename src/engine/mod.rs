//! Core audio engine — wires decode → DSP → output pipeline
//!

mod buffers;
mod commands;
mod crossfade;
mod decode_loop;
mod dsd_state;
pub mod handle;
pub mod helpers;
mod loudness_state;
mod recovery;
mod stream;
mod telemetry;
#[cfg(test)]
mod tests;

pub use handle::EngineHandle;
pub(crate) use buffers::EngineScratch;
#[allow(unused_imports)]
pub use buffers::{
    CROSSFADE_SCRATCH_FRAMES, MAX_PENDING_MULTICHANNEL_SAMPLES,
    MAX_PENDING_OUTPUT_FRAMES, MIX_BLOCK_FRAMES,
};
pub(crate) use dsd_state::DsdTransportState;
pub(crate) use loudness_state::LoudnessScanState;
pub(crate) use recovery::RecoveryState;
pub(crate) use telemetry::EngineTelemetry;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use crossbeam::channel::{self, Receiver, Sender};
use log::{error, info, warn};
// Re-export public types from submodules so the public API is unchanged.
use config::EngineConfig;
pub use stream::EngineError;

#[cfg(feature = "resample")]
use crate::dsp::resampler::GenericResampler;
use crate::{
    buffer::{
        EngineCommand, FixedFrameBuffer, PlaybackInfo, PlaybackState, DEFAULT_SAMPLE_RATE,
        OUTPUT_BUFFER_FRAMES,
    },
    decode::{DecodeInfo, Decoder},
    dsp::pipeline::{DspPipeline, LatencyReport, OutputSampleFormat, VolumePath},
    events::EngineEvent,
    output::{create_output, DeviceMonitor, Output, OutputError},
    source::AudioSource,
};

/// Precise sample-domain playback clock — the engine's single source of
/// truth for the playhead position.
///
/// Position is tracked strictly as an integer source-frame counter and
/// converted to seconds with a single division per read, so there is no
/// floating-point accumulation drift over long playback sessions.
///
/// # Speed semantics
///
/// The clock does not store a speed value because speed is already embedded
/// in the frame counter: source frames are consumed at `source_rate * speed`
/// frames per wall-clock second, so [`AudioClock::position_secs`]
/// (`source_frames / source_sample_rate`) reports the position *within the
/// current track* regardless of speed — the same semantics in single-track
/// playback and during crossfade transitions.
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioClock {
    /// Total source frames consumed from the decoder for the current track.
    /// This is the playhead; it only ever moves forward (or is reset by
    /// [`AudioClock::reset_track`] / [`AudioClock::set_source_frames`]).
    pub source_frames: u64,
    /// Source sample rate (Hz) of the current track.
    pub source_sample_rate: u32,
}

impl AudioClock {
    pub fn new(source_rate: u32) -> Self {
        Self {
            source_frames: 0,
            source_sample_rate: source_rate.max(1),
        }
    }

    /// Advance the playhead by `frames` consumed source frames.
    pub fn advance_source(&mut self, frames: u64) {
        self.source_frames += frames;
    }

    /// Set the playhead directly (seek / stop).
    pub fn set_source_frames(&mut self, frames: u64) {
        self.source_frames = frames;
    }

    /// Reset the playhead to the start of a new track.
    pub fn reset_track(&mut self, source_rate: u32) {
        self.source_frames = 0;
        self.source_sample_rate = source_rate.max(1);
    }

    /// Exact position in seconds computed directly from the integer source
    /// frame count. Eliminates floating-point accumulation error.
    pub fn position_secs(&self) -> f32 {
        if self.source_sample_rate == 0 {
            0.0
        } else {
            (self.source_frames as f64 / self.source_sample_rate as f64) as f32
        }
    }
}

/// Dual-decoder state machine for true gapless playback and crossfading.

///
/// `Single` represents normal single-track playback. `Transitioning` holds
/// both the outgoing (fading) and incoming (rising) decoders simultaneously,
/// allowing the `TrackMixer` to receive genuinely distinct sample streams
/// and perform real overlapping gain scaling.
///
/// Defined in `mod.rs` so that private fields are accessible from all engine
/// submodules (Rust privacy rules: submodules can access parent-module items).
#[allow(clippy::large_enum_variant)]
pub enum PlaybackStream {
    /// Playing a single track with no crossfade in progress.
    Single {
        decoder: Decoder,
        #[cfg(feature = "resample")]
        resampler: Option<GenericResampler>,
        #[cfg(not(feature = "resample"))]
        resampler: Option<()>,
    },
    /// Crossfading between two tracks. The outgoing decoder provides the
    /// tail of the current track while the incoming decoder provides the
    /// head of the next. The mixer's process() method receives distinct
    /// (out_l, out_r) and (in_l, in_r) sample pairs.
    Transitioning {
        outgoing_decoder: Decoder,
        #[cfg(feature = "resample")]
        outgoing_resampler: Option<GenericResampler>,
        #[cfg(not(feature = "resample"))]
        outgoing_resampler: Option<()>,
        incoming_decoder: Decoder,
        #[cfg(feature = "resample")]
        incoming_resampler: Option<GenericResampler>,
        #[cfg(not(feature = "resample"))]
        incoming_resampler: Option<()>,
        /// Frames remaining in the crossfade transition.
        crossfade_frames_remaining: usize,
        /// Total crossfade duration in frames.
        crossfade_total_frames: usize,
    },
}

pub struct AudioEngine {
    output_buffer: Arc<FixedFrameBuffer>,
    cmd_tx: Sender<EngineCommand>,
    cmd_rx: Receiver<EngineCommand>,
    /// Playback info stored in an ArcSwap for wait-free concurrent reads.
    /// Writers use rcu() for atomic snapshot replacement; readers use load().
    /// This makes the decode hot path lock-free — no OS scheduler involvement.
    playback_info: Arc<ArcSwap<PlaybackInfo>>,
    running: Arc<AtomicBool>,
    /// The active output transport (cpal, or the native WASAPI exclusive
    /// backend on Windows with `wasapi-native`).
    audio_output: Option<Box<dyn Output>>,
    pipeline: DspPipeline,
    /// Graphic EQ model (§9.1) — the slider state compiled into
    /// `pipeline.eq`. Always present; only authoritative while enabled.
    graphic_eq: crate::dsp::GraphicEq,
    /// Explicitly selected output profile (§10). When `None`, the engine
    /// auto-selects from the built-in/user profile library by device name.
    output_profile: Option<crate::output::OutputProfile>,
    /// The dual-decoder state machine — replaces the single `decoder` field.
    stream: Option<PlaybackStream>,
    config: EngineConfig,
    duration_secs: f32,
    output_sample_rate: u32,
    speed: f32,
    /// Sample-accurate integer playback clock — the single source of truth
    /// for the playhead (position and current source sample rate).
    clock: AudioClock,
    current_source: Option<AudioSource>,
    stream_ended: bool,
    event_tx: Sender<EngineEvent>,
    event_rx: Receiver<EngineEvent>,
    device_monitor: DeviceMonitor,

    // ── Domain sub-structures ──
    pub(crate) telemetry: EngineTelemetry,
    pub(crate) dsd: DsdTransportState,
    pub(crate) loudness_scan: LoudnessScanState,
    pub(crate) recovery: RecoveryState,
    pub(crate) scratch: EngineScratch,
}

/// Build the reason text for why DoP cannot engage because the output is not
/// exclusive (used by the `load_track` fallback warning). When the user is on
/// the Auto/shared backend it names a concrete exclusive backend to switch to,
/// so the failure is actionable rather than a dead-end log line.
pub fn dop_exclusive_reason(
    out_info: &crate::output::OutputInfo,
    requested_backend: config::AudioBackend,
) -> String {
    if out_info.is_fallback {
        match &out_info.fallback_reason {
            Some(r) => format!("the exclusive backend request fell back to a shared device ({r})"),
            None => "the exclusive backend request fell back to a shared device".to_string(),
        }
    } else if requested_backend == config::AudioBackend::Auto {
        let exclusive_names = if cfg!(target_os = "linux") {
            "ExclusiveAlsa (or select a direct hw: device)".to_string()
        } else if cfg!(target_os = "windows") {
            // cpal's WASAPI backend is shared-mode only, so ASIO is the only
            // exclusive backend available on Windows in this build.
            "ExclusiveAsio (WASAPI exclusive requires a native IAudioClient \
             backend not available through cpal)"
                .to_string()
        } else {
            // cpal's CoreAudio backend does not implement hog mode.
            "a native CoreAudio hog-mode backend (not available through cpal)".to_string()
        };
        format!(
            "the backend is Auto (shared device); switch to an exclusive backend \
             ({exclusive_names})"
        )
    } else if requested_backend == config::AudioBackend::ExclusiveAsio && !cfg!(feature = "asio") {
        "the ASIO backend is not compiled in (enable the 'asio' feature)".to_string()
    } else {
        "the selected backend did not provide exclusive access".to_string()
    }
}

impl AudioEngine {
    /// Create a new audio engine.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        let output_buffer = Arc::new(
            FixedFrameBuffer::new(OUTPUT_BUFFER_FRAMES)
                .map_err(|e| EngineError::Config(format!("Output buffer: {}", e)))?,
        );
        let (cmd_tx, cmd_rx) = channel::bounded(256);
        let (event_tx, event_rx) = channel::bounded(256);
        let output_sample_rate = DEFAULT_SAMPLE_RATE;
        let pipeline = DspPipeline::from_config(&config, output_sample_rate as f32);
        let graphic_eq = crate::dsp::GraphicEq::from_config(&config.graphic_eq);
        let info = PlaybackInfo {
            sample_rate: output_sample_rate,
            ..Default::default()
        };
        let clock = AudioClock::new(DEFAULT_SAMPLE_RATE);
        let device_monitor = DeviceMonitor::new(config.output_backend, Duration::from_millis(1500));

        Ok(Self {
            output_buffer,
            cmd_tx,
            cmd_rx,
            playback_info: Arc::new(ArcSwap::new(Arc::new(info))),
            running: Arc::new(AtomicBool::new(false)),
            audio_output: None,
            pipeline,
            graphic_eq,
            output_profile: None,
            stream: None,
            config,
            duration_secs: 0.0,
            output_sample_rate,
            speed: 1.0,
            clock,
            current_source: None,
            stream_ended: false,
            event_tx,
            event_rx,
            device_monitor,

            telemetry: EngineTelemetry::default(),
            dsd: DsdTransportState::default(),
            loudness_scan: LoudnessScanState::default(),
            recovery: RecoveryState::default(),
            scratch: EngineScratch::default(),
        })
    }

    /// Convenience constructor using the default configuration.
    pub fn new_default() -> Result<Self, EngineError> {
        Self::new(EngineConfig::default())
    }

    #[allow(dead_code)]
    pub(super) fn detect_output_sample_rate() -> Option<u32> {
        #[cfg(test)]
        {
            // Avoid querying OS audio drivers during unit tests to prevent WASAPI/COM access violations on Windows CI runners.
            None
        }
        #[cfg(not(test))]
        {
            use cpal::traits::{DeviceTrait, HostTrait};
            let host = cpal::default_host();
            let device = host.default_output_device()?;
            let default_config = device.default_output_config().ok()?;
            Some(default_config.sample_rate())
        }
    }

    pub fn start(&mut self) -> Result<(), EngineError> {
        if self.running.load(Ordering::Acquire) {
            return Err(EngineError::AlreadyRunning);
        }
        // An active output profile's backend preference wins over the config
        // default when the stream is (re)created.
        let audio_backend = self.profile_backend().unwrap_or(self.config.output_backend);
        // `create_output` dispatches to the native WASAPI exclusive backend on
        // Windows (when `wasapi-native` is enabled) and falls back to cpal
        // otherwise — the engine drives whichever through the `Output` trait.
        let mut output = create_output(
            Arc::clone(&self.output_buffer),
            audio_backend,
            self.config.output_device.as_deref(),
            self.config.fallback_policy,
        )?;
        // Sync the user's dither preference to the new output. Outputs
        // default to dither enabled; if the user has it disabled in config,
        // we propagate that here so the i16/u16 callbacks don't apply dither
        // when the user explicitly asked for it off.
        output.set_dither_enabled(self.config.dither_enabled);
        match self.config.volume_mode {
            config::VolumeMode::HardwarePreferred if !output.supports_hardware_volume() => {
                // HardwarePreferred falls back to software gain instead of
                // rejecting the output (spec §12).
                warn!(
                    "VolumeMode::HardwarePreferred: endpoint volume unavailable; falling back to software gain"
                );
                self.write_playback_info(|pb| {
                    pb.volume_error = Some(
                        "HardwarePreferred: endpoint volume unavailable; using software gain"
                            .to_string(),
                    );
                });
            }
            config::VolumeMode::HardwareOnly if !output.supports_hardware_volume() => {
                // Strict mode (spec §12): never silently introduce software
                // volume. The signal stays untouched; the failure is surfaced.
                let message = "HardwareOnly: endpoint volume unavailable; \
                               software volume will NOT be applied"
                    .to_string();
                warn!("{}", message);
                self.write_playback_info(|pb| {
                    pb.volume_error = Some(message.clone());
                    pb.volume_path = None;
                });
            }
            _ => {}
        }
        self.output_sample_rate = output.sample_rate();
        output.start()?;
        self.audio_output = Some(output);
        // Apply the active output profile (or auto-select one) now that the
        // device name is known.
        self.refresh_output_profile();

        self.running.store(true, Ordering::Release);
        self.pipeline
            .update_sample_rate(self.output_sample_rate as f32);
        self.update_playback_state(PlaybackState::Stopped);
        self.recovery.stream_recovery_attempts = 0;
        self.recovery.stream_recovery_burst_start = None;
        info!(
            "Audio engine started (output rate: {} Hz)",
            self.output_sample_rate
        );
        let running = Arc::clone(&self.running);
        let cmd_tx = self.cmd_tx.clone();

        // Spawn background device monitor thread to avoid blocking the audio tick thread.
        // This polls CPAL device enumeration which can take 50-100ms on Linux (ALSA).
        //
        // On Linux with PipeWire/PulseAudio, the ALSA device is always named "default"
        // regardless of which sink is active underneath. When the user connects TWS
        // Bluetooth headphones and PipeWire switches its default sink, the CPAL/ALSA name
        // doesn't change — but the sample rate negotiated with the new sink does (e.g.
        // speakers at 48000 Hz → TWS at 44100 Hz). We therefore also track the default
        // output sample rate as a third change signal to detect Bluetooth sink switches.
        if let Err(e) = std::thread::Builder::new()
            .name("tc-device-monitor".into())
            .spawn(move || {
                use cpal::traits::{DeviceTrait, HostTrait};
                let mut last_count = 0usize;
                let mut last_name = String::new();
                let mut last_sample_rate = 0u32;
                let mut first_run = true;

                // Helper: snapshot the current default-device state without allocating.
                let snapshot = || -> (usize, String, u32) {
                    let host = cpal::default_host();
                    let count = host.output_devices().map(|d| d.count()).unwrap_or(0);
                    let dev = host.default_output_device();
                    let name = dev
                        .as_ref()
                        .and_then(|d| {
                            d.description().ok().map(|desc| desc.name().to_string())
                        })
                        .unwrap_or_default();
                    let rate = dev
                        .as_ref()
                        .and_then(|d| d.default_output_config().ok())
                        .map(|c| c.sample_rate())
                        .unwrap_or(0);
                    (count, name, rate)
                };

                while running.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_secs(5));
                    if !running.load(Ordering::Acquire) {
                        break;
                    }

                    let (current_count, current_name, current_rate) = snapshot();

                    if first_run {
                        last_count = current_count;
                        last_name = current_name;
                        last_sample_rate = current_rate;
                        first_run = false;
                        continue;
                    }

                    let mut changed = false;

                    let count_changed = current_count != last_count;
                    let name_changed = current_name != last_name;
                    let rate_changed = current_rate != last_sample_rate
                        && current_rate != 0
                        && (count_changed || name_changed);

                    if count_changed {
                        info!(
                            "Device monitor: output device count changed ({} -> {})",
                            last_count, current_count
                        );
                        last_count = current_count;
                        changed = true;
                    }
                    if name_changed {
                        info!(
                            "Device monitor: default device name changed ('{}' -> '{}')",
                            last_name, current_name
                        );
                        last_name = current_name;
                        changed = true;
                    }
                    if rate_changed {
                        info!(
                            "Device monitor: default output sample rate changed ({} Hz -> {} Hz) \
                             — Bluetooth sink switch (PipeWire/PulseAudio)",
                            last_sample_rate, current_rate
                        );
                        last_sample_rate = current_rate;
                        changed = true;
                    }

                    if changed {
                        info!("Device monitor: triggering stream recovery");
                        match cmd_tx.send_timeout(
                            EngineCommand::AutoRecoverStream,
                            std::time::Duration::from_secs(1),
                        ) {
                            Ok(()) => {}
                            Err(_) => {
                                warn!("Device monitor: channel full; recovery signal dropped");
                                continue;
                            }
                        }
                        // Sleep a bit longer after recovery to allow OS and BT stack to settle
                        std::thread::sleep(Duration::from_secs(4));

                        // Re-snapshot after the settle period so we don't loop immediately
                        let (c, n, r) = snapshot();
                        last_count = c;
                        last_name = n;
                        last_sample_rate = r;
                    }
                }
            })
        {
            warn!("Failed to spawn device monitor thread: {}", e);
        }

        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(mut output) = self.audio_output.take() {
            output.stop();
        }
        self.stream = None;
        self.scratch.crossfade_triggered = false;
        self.loudness_scan.next_track_path = None;
        self.scratch.cached_incoming_decoder = None;
        self.loudness_scan.current_track_path = None;
        self.loudness_scan.pending_loudness_metadata = None;
        self.loudness_scan.incoming_track_path = None;
        self.loudness_scan.pending_incoming_loudness_metadata = None;
        self.scratch.rs_out_buf.clear();
        self.scratch.rs_in_buf.clear();
        self.dsd.dop_active = false;
        self.dsd.dop_rate = 0;
        self.dsd.native_dsd_active = false;
        self.dsd.dsd_wire_format = None;
        self.dsd.dsd_byte_buffer = None;
        self.dsd.dsd_transport_report = crate::decode::DsdTransportReport::default();
        self.pipeline.set_dop_bypass(false);
        if let Some(ref output) = self.audio_output {
            output.set_dither_enabled(self.config.dither_enabled);
        }
        // Keep the published playhead consistent with the reset internal
        // clock (see the Stop command handler for the same fix).
        self.write_playback_info(|pb| pb.position_secs = 0.0);
        self.update_playback_state(PlaybackState::Stopped);
        info!("Audio engine stopped");
    }

    /// Spawn a background loudness scan for the currently loaded track if it
    /// lacks EBU R128 metadata.
    ///
    /// Scans are serialized (at most one in flight). A result for a track
    /// that has since been superseded is discarded by the completion handler,
    /// which then re-arms scans for anything still missing metadata.
    fn start_loudness_scan(&mut self) {
        if self.loudness_scan.loudness_scan_in_flight {
            return;
        }
        let Some(path) = self.loudness_scan.current_track_path.clone() else {
            return;
        };
        let needs_scan = self
            .loudness_scan
            .pending_loudness_metadata
            .as_ref()
            .is_none_or(|m| m.ebu_r128_loudness.is_none() || m.replaygain_track_db.is_none());
        if !needs_scan {
            return;
        }
        self.spawn_loudness_scan(path);
    }

    /// Spawn a background loudness scan for the incoming (next) track if it
    /// lacks EBU R128 / ReplayGain metadata. The path is either the pending next
    /// track (`next_track_path`) or the track currently fading in
    /// (`incoming_track_path`).
    fn start_incoming_loudness_scan(&mut self) {
        if self.loudness_scan.loudness_scan_in_flight {
            return;
        }
        let Some(path) = self
            .loudness_scan
            .incoming_track_path
            .clone()
            .or_else(|| self.loudness_scan.next_track_path.clone())
        else {
            return;
        };
        let needs_scan = self
            .loudness_scan
            .pending_incoming_loudness_metadata
            .as_ref()
            .is_none_or(|m| m.ebu_r128_loudness.is_none() || m.replaygain_track_db.is_none());
        if !needs_scan {
            return;
        }
        self.spawn_loudness_scan(path);
    }

    /// Shared scan-thread spawner. Guards on `loudness_scan_in_flight` so at
    /// most one decode thread is active at a time.
    fn spawn_loudness_scan(&mut self, path: std::path::PathBuf) {
        if self.loudness_scan.loudness_scan_in_flight {
            return;
        }
        self.loudness_scan.loudness_scan_in_flight = true;
        let cmd_tx = self.cmd_tx.clone();
        let path_display = path.display().to_string();
        match std::thread::Builder::new()
            .name("loudness-scan".into())
            .spawn(move || {
                let result = crate::decode::scan_track_loudness(&path);
                // Persist the result keyed by the file's size + mtime so an
                // unchanged track is never re-scanned on a later load.
                if let Some(ref r) = result {
                    crate::decode::loudness_cache::store(&path, r);
                }
                let _ = cmd_tx.send(EngineCommand::LoudnessScanComplete { path, result });
            }) {
            Ok(_) => {
                info!("Background loudness scan started for {}", path_display);
            }
            Err(e) => {
                self.loudness_scan.loudness_scan_in_flight = false;
                warn!("Failed to spawn loudness scan thread: {}", e);
            }
        }
    }

    pub fn send_command(&self, cmd: EngineCommand) {
        let is_critical = matches!(
            cmd,
            EngineCommand::Play
                | EngineCommand::Pause
                | EngineCommand::Stop
                | EngineCommand::Shutdown
        );
        let timeout = if is_critical {
            std::time::Duration::from_secs(5)
        } else {
            std::time::Duration::from_millis(100)
        };
        match self.cmd_tx.send_timeout(cmd, timeout) {
            Ok(()) => {}
            Err(crossbeam::channel::SendTimeoutError::Timeout(cmd)) => {
                // For idempotent state-setting commands (volume, seek, eq, balance),
                // attempt a fallback non-blocking send before giving up.
                if self.cmd_tx.try_send(cmd.clone()).is_err() {
                    warn!(
                        "Engine command channel saturated; dropped non-critical command: {:?}",
                        cmd
                    );
                }
            }
            Err(crossbeam::channel::SendTimeoutError::Disconnected(_)) => {
                warn!("Engine command channel disconnected; command dropped");
            }
        }
    }

    pub fn send_command_channel(&mut self) -> crossbeam::channel::Sender<EngineCommand> {
        self.cmd_tx.clone()
    }

    /// Create a safe, decoupled [`EngineHandle`] for host applications and controllers.
    pub fn handle(&self) -> EngineHandle {
        EngineHandle::new(
            self.cmd_tx.clone(),
            Arc::clone(&self.playback_info),
            self.event_rx.clone(),
        )
    }

    pub fn set_volume(&mut self, vol: f32) {
        let clamped = if vol.is_finite() {
            vol.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if self.pipeline.is_bit_perfect() {
            // Direct API callers must obey the same fail-closed contract as
            // EngineCommand::SetVolume: no software gain in Bit-Perfect mode.
            self.pipeline.set_volume(1.0);
            self.write_playback_info(|pb| {
                pb.volume_error = Some(
                    "Bit-Perfect mode: software volume is disabled; use hardware volume or disable Bit-Perfect mode"
                        .to_string(),
                );
                pb.volume_path = None;
            });
            return;
        }
        self.pipeline.set_volume(clamped);
        self.write_playback_info(|pb| pb.volume = clamped);
    }

    /// Set the volume in dB. Range: [-60.0, 0.0].
    /// This is the perceptually-correct API; UI percentages should be
    /// converted to dB via `DspPipeline::volume_percent_to_db` before
    /// being passed here.
    pub fn set_volume_db(&mut self, db: f32) {
        if !db.is_finite() {
            log::warn!(
                "AudioEngine::set_volume_db: non-finite value {}; ignoring",
                db
            );
            return;
        }
        if self.pipeline.is_bit_perfect() {
            self.pipeline.set_volume(1.0);
            self.write_playback_info(|pb| {
                pb.volume_error = Some(
                    "Bit-Perfect mode: software volume is disabled; use hardware volume or disable Bit-Perfect mode"
                        .to_string(),
                );
                pb.volume_path = None;
            });
            return;
        }
        self.pipeline.set_volume_db(db);
        let linear = DspPipeline::volume_db_to_linear(db);
        self.write_playback_info(|pb| pb.volume = linear);
    }

    /// Read the current volume in dB. Useful for UI display.
    /// Returns -60.0 when effectively muted.
    pub fn volume_db(&self) -> f32 {
        self.pipeline.volume_db()
    }

    /// Convert a UI percentage (0.0–100.0) to a dB value suitable for
    /// `set_volume_db`. This is the recommended way to map a volume
    /// slider position to a perceptually-even gain change.
    pub fn volume_percent_to_db(percent: f32) -> f32 {
        DspPipeline::volume_percent_to_db(percent)
    }

    /// Reset the clip and NaN counters in PlaybackInfo. The UI can call
    /// this after displaying a warning to the user, so that subsequent
    /// warnings only appear for *new* incidents.
    pub fn reset_dsp_diagnostics(&mut self) {
        self.write_playback_info(|pb| {
            pb.clip_count = 0;
            pb.nan_count = 0;
        });
    }

    /// Enable or disable true-peak (inter-sample peak) detection on the
    /// limiter. See `LookaheadLimiter::enable_true_peak` for details.
    /// This is a thin pass-through to the pipeline/limiter; the engine
    /// does not persist this setting across pipeline rebuilds (callers
    /// that want persistence should set it via the config struct's
    /// limiter section).
    pub fn set_limiter_true_peak(&mut self, enabled: bool) {
        self.pipeline.set_limiter_true_peak(enabled);
    }

    /// Whether true-peak detection is currently active on the limiter.
    pub fn limiter_true_peak_enabled(&self) -> bool {
        self.pipeline.limiter_true_peak_enabled()
    }

    pub fn load_track(&mut self, path: &std::path::Path) -> Result<DecodeInfo, EngineError> {
        let decoder = match self.scratch.cached_incoming_decoder.take() {
            Some(d) if self.loudness_scan.next_track_path.as_deref() == Some(path) => {
                info!("Using cached decoder for load_track");
                d
            }
            _ => Decoder::open(path)?,
        };

        self.load_opened_decoder(AudioSource::File(path.to_path_buf()), decoder, Some(path))
    }

    /// Load in-memory byte buffer for decoding.
    pub fn load_memory(&mut self, data: Vec<u8>, extension_hint: Option<&str>) -> Result<DecodeInfo, EngineError> {
        let decoder = Decoder::open_memory(data.clone(), extension_hint)?;
        let source = AudioSource::Memory {
            data,
            extension_hint: extension_hint.map(String::from),
        };
        self.load_opened_decoder(source, decoder, None)
    }

    /// Configure and load an opened decoder into the engine pipeline.
    pub(crate) fn load_opened_decoder(
        &mut self,
        source: AudioSource,
        mut decoder: Decoder,
        loudness_path: Option<&std::path::Path>,
    ) -> Result<DecodeInfo, EngineError> {
        // A native DSD output is a different transport, not merely a sample
        // rate. Leave it explicitly before loading any track whose requested
        // policy is PCM/DoP (including a DSD→PCM transition); otherwise a raw
        // decoder would be paired with a stale native render thread and the
        // PCM ring would never reach the DAC.
        let wants_native =
            decoder.is_dsd() && self.config.dsd_output == config::DsdOutput::NativeDsd;
        if self.dsd.native_dsd_active && !wants_native {
            let output_rate = {
                let output = self.audio_output.as_mut().ok_or_else(|| {
                    EngineError::Output(OutputError::StreamError(
                        "native DSD active without an output transport".to_string(),
                    ))
                })?;
                output.set_native_dsd(None)?;
                output.set_dither_enabled(self.config.dither_enabled);
                output.sample_rate()
            };
            self.output_sample_rate = output_rate;
            self.pipeline
                .update_sample_rate(self.output_sample_rate as f32);
            self.dsd.native_dsd_active = false;
            self.dsd.dsd_wire_format = None;
            self.dsd.dsd_byte_buffer = None;
            self.pipeline.set_dop_bypass(false);
        }

        // ── DSD output mode selection (§7) ─────────────────────────────────
        let mut native_dsd_active = false;
        let mut dsd_report = if decoder.is_dsd() {
            let requested = match self.config.dsd_output {
                config::DsdOutput::NativeDsd => crate::decode::DsdTransport::Native,
                config::DsdOutput::DoP => crate::decode::DsdTransport::Dop,
                config::DsdOutput::PcmConvert => crate::decode::DsdTransport::PcmConversion,
            };
            let mut r = crate::decode::DsdTransportReport::new(requested, requested);
            r.bit_rate = decoder.dsd_bit_rate();
            r
        } else {
            crate::decode::DsdTransportReport::default()
        };
        if self.config.dsd_output == config::DsdOutput::NativeDsd && decoder.is_dsd() {
            match self.negotiate_native_dsd(&mut decoder) {
                Ok((format, buffer)) => {
                    native_dsd_active = true;
                    dsd_report.actual = crate::decode::DsdTransport::Native;
                    dsd_report.wire_format = Some(format);
                    self.dsd.dsd_wire_format = Some(format);
                    self.dsd.dsd_byte_buffer = Some(buffer);
                    let bit_rate = dsd_report.bit_rate.unwrap_or(2_822_400);
                    self.output_sample_rate = format.frame_rate_hz(bit_rate);
                    info!(
                        "DSD native active: {} at {} (DSP structurally bypassed)",
                        format.label(),
                        format.frame_rate_hz(bit_rate)
                    );
                }
                Err(e) => {
                    dsd_report.actual = crate::decode::DsdTransport::Dop;
                    dsd_report.step(format!("native DSD unavailable ({e})"));
                    dsd_report.step("fallback: DoP");
                    warn!("Native DSD unavailable ({e}); falling back to DoP, then DSD→PCM");
                }
            }
        }
        if !native_dsd_active
            && decoder.is_dsd()
            && self.config.dsd_output != config::DsdOutput::PcmConvert
        {
            decoder.set_dop_mode(true);
        }
        let dop_rate = if native_dsd_active {
            None
        } else {
            decoder.dop_rate()
        };
        let mut dop_active = false;
        if let Some(dr) = dop_rate {
            if let Some(ref mut output) = self.audio_output {
                let out_info = output.output_info();
                let exclusive = out_info.access_state.is_bit_perfect();
                if exclusive {
                    match output.reconfigure_sample_format(dr, cpal::SampleFormat::I32) {
                        Ok(actual)
                            if actual == dr
                                && output.sample_format() == cpal::SampleFormat::I32 =>
                        {
                            dop_active = true;
                            self.output_sample_rate = dr;
                            self.pipeline.update_sample_rate(dr as f32);
                        }
                        Ok(actual) => {
                            warn!(
                                "DoP requested but output settled on {} Hz without the I32 \
                                 container (need {} Hz I32); using DSD→PCM",
                                 actual, dr
                            );
                            dsd_report.step(format!(
                                "DoP unavailable: output settled on {} Hz without the I32 \
                                 container (need {} Hz I32)",
                                actual, dr
                            ));
                        }
                        Err(e) => {
                            warn!(
                                "DoP requested but the output could not provide a {} Hz I32 \
                                 stream ({}); using DSD→PCM",
                                dr, e
                            );
                            dsd_report.step(format!("DoP unavailable ({e})"));
                        }
                    }
                } else {
                    let reason = dop_exclusive_reason(&out_info, self.config.output_backend);
                    warn!(
                        "DoP requires an exclusive output — {reason}. Using DSD→PCM for this track."
                    );
                    dsd_report.step(format!("DoP requires an exclusive output — {reason}"));
                }
            } else {
                warn!("DoP requested but no output device is active; using DSD→PCM");
                dsd_report.step("DoP unavailable: no output device is active");
            }
            if !dop_active && dsd_report.actual != crate::decode::DsdTransport::PcmConversion {
                dsd_report.actual = crate::decode::DsdTransport::PcmConversion;
                dsd_report.step("fallback: DSD→PCM conversion");
            }
        } else if decoder.is_dsd()
            && !native_dsd_active
            && dsd_report.requested != crate::decode::DsdTransport::PcmConversion
            && dsd_report.actual != crate::decode::DsdTransport::PcmConversion
        {
            dsd_report.actual = crate::decode::DsdTransport::PcmConversion;
            dsd_report.step("fallback: DSD→PCM conversion");
        }
        if !dop_active && !native_dsd_active {
            decoder.set_dop_mode(false);
        }
        if native_dsd_active {
            decoder.set_native_dsd_mode(true);
        }
        self.dsd.dop_active = dop_active;
        self.dsd.dop_rate = if dop_active { dop_rate.unwrap_or(0) } else { 0 };
        self.dsd.native_dsd_active = native_dsd_active;
        self.dsd.dsd_transport_report = dsd_report;
        if dop_active || native_dsd_active {
            self.pipeline.set_dop_bypass(true);
            if let Some(ref output) = self.audio_output {
                output.set_dither_enabled(false);
            }
            self.speed = 1.0;
            if dop_active {
                info!(
                    "DSD DoP active: {} Hz to DAC (DSP bypassed, dither off)",
                    self.dsd.dop_rate
                );
            }
        } else {
            self.pipeline.set_dop_bypass(false);
            if let Some(ref output) = self.audio_output {
                output.set_dither_enabled(self.config.dither_enabled);
            }
        }

        let info = decoder.info().clone();
        self.clock.reset_track(info.sample_rate);
        self.duration_secs = info.duration_secs;
        self.recovery.consecutive_decode_errors = 0;

        if !dop_active && !native_dsd_active {
            if let Some(ref mut output) = self.audio_output {
                let caps = output.capabilities();
                let target_rate =
                    caps.best_rate_for(info.sample_rate, &self.config.sample_rate_policy);
                if let Ok(actual_rate) = output.reconfigure_sample_rate(target_rate) {
                    self.output_sample_rate = actual_rate;
                    self.pipeline.update_sample_rate(actual_rate as f32);
                }
            }
        }

        #[cfg(feature = "resample")]
        let resample_speed = if dop_active || native_dsd_active {
            1.0
        } else {
            self.speed
        };
        #[cfg(feature = "resample")]
        let resampler = if native_dsd_active {
            None
        } else {
            recovery::build_resampler(
                self.config.resampler_quality,
                self.clock.source_sample_rate as f32,
                self.output_sample_rate as f32,
                resample_speed,
                self.config.precision_mode,
            )
        };

        #[cfg(feature = "resample")]
        if !native_dsd_active
            && resampler.is_none()
            && (self.clock.source_sample_rate != self.output_sample_rate
                || (self.speed - 1.0).abs() > 0.001)
        {
            log::error!(
                "Critical: Resampler required ({} Hz -> {} Hz) but could not be initialized!",
                self.clock.source_sample_rate,
                self.output_sample_rate
            );
            self.write_playback_info(|pb| {
                pb.resampler_disabled = true;
                pb.resampler_failed_fatal = true;
                pb.engine_error = Some(
                    "Resampler initialization failed; cannot play at requested rate/speed"
                        .to_string(),
                );
            });
            return Err(EngineError::Resampler(format!(
                "Required resampler ({} Hz -> {} Hz) could not be initialized; \
                 refusing to play at the wrong rate/speed",
                self.clock.source_sample_rate, self.output_sample_rate
            )));
        }

        #[cfg(not(feature = "resample"))]
        let resampler: Option<()> = None;

        self.stream = Some(PlaybackStream::Single { decoder, resampler });

        if let Some(ref output) = self.audio_output {
            output.reset_buffer();
        } else {
            self.output_buffer.reset();
        }
        self.scratch.pending_output_frames.clear();
        self.scratch.pending_multichannel.clear();
        self.scratch.pending_multichannel_channels = 0;
        self.scratch.pending_chunk = None;
        self.scratch.pending_incoming_chunk = None;
        self.scratch.rs_out_buf.clear();
        self.scratch.rs_in_buf.clear();
        self.pipeline.reset();

        let current_volume = self.playback_info.load().volume;
        self.pipeline.set_volume(current_volume);
        self.pipeline.volume.snap();

        let loudness_meta = if let Some(path) = loudness_path {
            let mut meta = crate::decode::extract_loudness_metadata(path);
            if meta.ebu_r128_loudness.is_none() {
                if let Some(cached) = crate::decode::loudness_cache::lookup(path) {
                    meta.ebu_r128_loudness = cached.ebu_r128_loudness;
                    meta.ebu_r128_peak = cached.ebu_r128_peak_dbtp;
                    info!("Loaded cached loudness metadata for {}", path.display());
                }
            }
            meta
        } else {
            crate::dsp::LoudnessMetadata::default()
        };

        self.loudness_scan.current_track_path = loudness_path.map(|p| p.to_path_buf());
        self.loudness_scan.pending_loudness_metadata = Some(loudness_meta);
        self.pipeline
            .apply_loudness_metadata_outgoing(Some(loudness_meta));
        if loudness_path.is_some() {
            self.start_loudness_scan();
        }

        self.pipeline.mixer_mut().start_playing();

        self.stream_ended = false;
        self.current_source = Some(source.clone());
        let current_source = self.current_source.clone();
        let speed = self.speed;
        let dop_active = self.dsd.dop_active;
        let native_dsd_active = self.dsd.native_dsd_active;
        let dsd_transport = self.dsd.dsd_transport_report.actual;
        let dsd_transport_report = self.dsd.dsd_transport_report.clone();
        self.playback_info.rcu(|old| {
            Arc::new(PlaybackInfo {
                duration_secs: info.duration_secs,
                sample_rate: info.sample_rate,
                current_source: current_source.clone(),
                speed,
                dop_active,
                native_dsd_active,
                dsd_transport,
                dsd_transport_report: dsd_transport_report.clone(),
                volume: old.volume,
                state: if old.state == PlaybackState::Stopped {
                    PlaybackState::Paused
                } else {
                    old.state
                },

                ..Default::default()
            })
        });

        self.emit_event(EngineEvent::SourceOpened {
            source,
            sample_rate: info.sample_rate,
            channels: info.channels,
            duration_secs: info.duration_secs,
        });
        self.emit_event(EngineEvent::FormatChanged {
            sample_rate: info.sample_rate,
            channels: info.channels,
        });

        info!(
            "Loaded source: {} Hz, {} ch, {:.1}s",
            info.sample_rate, info.channels, info.duration_secs
        );
        Ok(info)
    }

    /// Load an explicit [`AudioSource`] (file, URI, or memory) for playback.
    pub fn load_source(&mut self, source: &AudioSource) -> Result<DecodeInfo, EngineError> {
        match source {
            AudioSource::File(path) => self.load_track(path),
            AudioSource::Uri(uri) => {
                let path_buf = if let Some(stripped) = uri.strip_prefix("file://") {
                    helpers::percent_decode(stripped)
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| {
                            EngineError::InvalidSource(format!("Invalid file URI: {}", uri))
                        })?
                } else {
                    std::path::PathBuf::from(uri)
                };
                self.load_track(&path_buf)
            }
            AudioSource::Memory { data, extension_hint } => {
                self.load_memory(data.clone(), extension_hint.as_deref())
            }
        }
    }

    /// Pre-open the next [`AudioSource`] for seamless gapless / crossfade transition.
    pub fn prepare_next_source(&mut self, source: &AudioSource) -> Result<DecodeInfo, EngineError> {
        match source {
            AudioSource::File(path) => self.prepare_next_track(path),
            AudioSource::Uri(uri) => {
                let path_buf = if let Some(stripped) = uri.strip_prefix("file://") {
                    helpers::percent_decode(stripped)
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| {
                            EngineError::InvalidSource(format!("Invalid file URI: {}", uri))
                        })?
                } else {
                    std::path::PathBuf::from(uri)
                };
                self.prepare_next_track(&path_buf)
            }
            AudioSource::Memory { data, extension_hint } => {
                let decoder = Decoder::open_memory(data.clone(), extension_hint.as_deref())?;
                let info = decoder.info().clone();
                self.scratch.cached_incoming_decoder = Some(decoder);
                self.loudness_scan.pending_incoming_loudness_metadata =
                    Some(crate::dsp::LoudnessMetadata::default());
                Ok(info)
            }
        }
    }

    /// Negotiate native-DSD transport with the output backend (§7).
    ///
    /// Returns the negotiated wire format and the byte ring the engine feeds
    /// raw DSD bytes into, or an error describing why native DSD is
    /// unavailable on this device/backend (e.g. no DSD-capable output, or
    /// the device refused every DSD format). The caller records the failure
    /// as an explicit fallback step — never a silent downgrade.
    fn negotiate_native_dsd(
        &mut self,
        decoder: &mut Decoder,
    ) -> Result<
        (
            crate::decode::dsd::DsdWireFormat,
            std::sync::Arc<crate::buffer::DsdByteBuffer>,
        ),
        OutputError,
    > {
        let Some(output) = self.audio_output.as_mut() else {
            return Err(OutputError::StreamError(
                "no output device is active".to_string(),
            ));
        };
        // Capability candidates are typed by wire format, rate, and channel
        // constraints. The exact stream is still verified by set_native_dsd;
        // this prevents choosing DSD_U8 merely because it was first in a
        // legacy format list when a DSD128+ endpoint exposes another format.
        let caps = output.native_dsd_capability_matrix();
        let bit_rate = decoder
            .dsd_bit_rate()
            .ok_or_else(|| OutputError::StreamError("not a DSD source".to_string()))?;
        let channels = decoder.info().channels as u16;
        let wire_format = caps
            .iter()
            .find(|cap| cap.supports(bit_rate, channels))
            .map(|cap| cap.wire_format)
            .or_else(|| caps.first().map(|cap| cap.wire_format))
            .ok_or_else(|| {
                OutputError::StreamError(format!(
                    "no native DSD transport candidates on backend {:?}",
                    self.config.output_backend
                ))
            })?;
        // Byte ring sized for approximately one second of the source DSD
        // bitstream. Native U8/U16/U32 wire formats carry the same total byte
        // count per channel; the negotiated frame width only changes how the
        // render thread groups those bytes.
        let capacity = (bit_rate as usize)
            .checked_mul(channels as usize)
            .and_then(|bytes| bytes.checked_div(8))
            .unwrap_or(usize::MAX)
            .min(usize::MAX / 2);
        let buffer = std::sync::Arc::new(crate::buffer::DsdByteBuffer::new(capacity.max(65536)));
        let params = crate::output::NativeDsdParams {
            wire_format,
            bit_rate,
            channels,
            buffer: buffer.clone(),
        };
        let negotiated = output.set_native_dsd(Some(params))?;
        let format = negotiated.ok_or_else(|| {
            OutputError::StreamError("backend returned no native DSD format".to_string())
        })?;
        Ok((format, buffer))
    }

    /// Sample-accurate gapless handoff to the next track.
    ///
    /// Unlike [`Self::load_track`] — which rebuilds the resampler and resets
    /// every DSP stage — this preserves the whole pipeline state, most
    /// importantly the final safety limiter's lookahead delay line, so the
    /// outgoing track's last samples flow into the incoming track with no
    /// gap, no drop, and no click. Only the decoder, the playback clock, and
    /// the loudness metadata are swapped.
    ///
    /// When the next track's source rate matches the current one, the running
    /// resampler instance is moved into the new stream unchanged (filter
    /// state and partially-filled input block included). When the rate
    /// differs, the old resampler cannot accept the new track's samples, so
    /// its tail is flushed through the shared limiter — completing the
    /// outgoing track — and a fresh resampler is built for the new ratio.
    ///
    /// Falls back to [`Self::load_track`] only when DSD/DoP handling is
    /// involved (decoder mode selection and output-rate reconfiguration are
    /// too invasive for a state-preserving handoff).
    pub(super) fn swap_to_next_track(
        &mut self,
        path: &std::path::Path,
        #[cfg(feature = "resample")] resampler: &mut Option<GenericResampler>,
        #[cfg(not(feature = "resample"))] _resampler: &mut Option<()>,
    ) -> Result<DecodeInfo, EngineError> {
        // Reuse the decoder pre-opened by `prepare_next_track` if there is
        // one; it was opened for exactly this path.
        let decoder = match self.scratch.cached_incoming_decoder.take() {
            Some(d) => d,
            None => Decoder::open(path)?,
        };
        let info = decoder.info().clone();

        // DSD/DoP needs the full load path, as does a handoff away from an
        // active DoP or native-DSD stream (the output device must be
        // reconfigured back to PCM or re-negotiated for DSD).
        if decoder.is_dsd() || self.dsd.dop_active || self.dsd.native_dsd_active {
            return self.load_track(path);
        }

        // Build the next stream's resampler. Same rate: keep the running
        // instance. Different rate: flush its tail (through the shared
        // limiter, so the outgoing track completes) and build a fresh
        // resampler for the new ratio. Everything else — limiter, filters,
        // pending frames, ring buffer — is preserved in both cases.
        #[cfg(feature = "resample")]
        let next_resampler = if info.sample_rate == self.clock.source_sample_rate {
            resampler.take()
        } else {
            self.flush_resampler_tail(resampler);
            let r = recovery::build_resampler(
                self.config.resampler_quality,
                info.sample_rate as f32,
                self.output_sample_rate as f32,
                self.speed,
                self.config.precision_mode,
            );
            if r.is_none()
                && (info.sample_rate != self.output_sample_rate || (self.speed - 1.0).abs() > 0.001)
            {
                // Continuing the handoff without the required resampler would
                // play the next track at the wrong rate/pitch (passthrough).
                // Fail the handoff instead — the caller halts playback.
                log::error!(
                    "Critical: Resampler required ({} Hz -> {} Hz) for gapless handoff \
                     but could not be initialized!",
                    info.sample_rate,
                    self.output_sample_rate
                );
                self.write_playback_info(|pb| {
                    pb.resampler_disabled = true;
                    pb.resampler_failed_fatal = true;
                    pb.engine_error =
                        Some("Resampler initialization failed during gapless handoff".to_string());
                });
                return Err(EngineError::Resampler(format!(
                    "Required resampler ({} Hz -> {} Hz) for gapless handoff could \
                     not be initialized; refusing to play at the wrong rate/speed",
                    info.sample_rate, self.output_sample_rate
                )));
            }
            r
        };
        #[cfg(not(feature = "resample"))]
        let next_resampler: Option<()> = _resampler.take();

        self.dsd.dop_active = false;
        self.dsd.dop_rate = 0;
        self.pipeline.set_dop_bypass(false);

        self.clock.reset_track(info.sample_rate);
        self.duration_secs = info.duration_secs;
        self.recovery.consecutive_decode_errors = 0;
        self.scratch.crossfade_triggered = false;
        self.scratch.pending_chunk = None;
        self.scratch.pending_incoming_chunk = None;

        self.stream = Some(PlaybackStream::Single {
            decoder,
            resampler: next_resampler,
        });

        // Loudness for the incoming track: reuse what `prepare_next_track`
        // prepared (tags + cached scan + background scan result), falling
        // back to extracting from the file.
        let mut loudness_meta = self
            .loudness_scan
            .pending_incoming_loudness_metadata
            .take()
            .unwrap_or_else(|| crate::decode::extract_loudness_metadata(path));
        if loudness_meta.ebu_r128_loudness.is_none() {
            if let Some(cached) = crate::decode::loudness_cache::lookup(path) {
                loudness_meta.ebu_r128_loudness = cached.ebu_r128_loudness;
                loudness_meta.ebu_r128_peak = cached.ebu_r128_peak_dbtp;
                info!("Loaded cached loudness metadata for {}", path.display());
            }
        }
        self.loudness_scan.current_track_path = Some(path.to_path_buf());
        self.loudness_scan.pending_loudness_metadata = Some(loudness_meta);
        self.pipeline
            .apply_loudness_metadata_outgoing(Some(loudness_meta));
        self.start_loudness_scan();

        self.stream_ended = false;

        self.current_source = Some(AudioSource::File(path.to_path_buf()));
        let current_source = self.current_source.clone();
        let speed = self.speed;
        let dop_active = self.dsd.dop_active;
        let native_dsd_active = self.dsd.native_dsd_active;
        let dsd_transport = self.dsd.dsd_transport_report.actual;
        let dsd_transport_report = self.dsd.dsd_transport_report.clone();
        self.playback_info.rcu(|old| {
            Arc::new(PlaybackInfo {
                duration_secs: info.duration_secs,
                sample_rate: info.sample_rate,
                current_source: current_source.clone(),
                speed,
                dop_active,
                native_dsd_active,
                dsd_transport,
                dsd_transport_report: dsd_transport_report.clone(),
                // Preserve fields that survive a track load
                volume: old.volume,
                state: if old.state == PlaybackState::Stopped {
                    PlaybackState::Paused
                } else {
                    old.state
                },

                ..Default::default()
            })
        });

        self.emit_event(EngineEvent::SourceOpened {
            source: AudioSource::File(path.to_path_buf()),
            sample_rate: info.sample_rate,
            channels: info.channels,
            duration_secs: info.duration_secs,
        });
        self.emit_event(EngineEvent::FormatChanged {
            sample_rate: info.sample_rate,
            channels: info.channels,
        });

        info!(
            "Gapless handoff: {} Hz, {} ch, {:.1}s (resampler + DSP state preserved)",
            info.sample_rate, info.channels, info.duration_secs
        );
        Ok(info)
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        let initial_state = self.current_state();

        if let Some(prev) = self.telemetry.tick_start {
            let elapsed = now.duration_since(prev);
            self.telemetry.total_time += elapsed;
            if elapsed > self.telemetry.worst_tick_time {
                self.telemetry.worst_tick_time = elapsed;
            }

            // Watchdog: detect if the engine thread was starved for too long
            const TICK_DEADLINE: Duration = Duration::from_millis(50);
            if elapsed > TICK_DEADLINE && initial_state == PlaybackState::Playing {
                warn!("Audio dropout: tick delayed by {:.1}ms (deadline 50ms). CPU may be over-utilized.", elapsed.as_secs_f32() * 1000.0);
                self.write_playback_info(|pb| pb.cpu_overloads += 1);
                self.telemetry.deadline_miss_window += 1;
            }
        }
        self.telemetry.tick_start = Some(now);

        self.process_commands();
        self.poll_device_monitor();

        // Fold externally-observed hardware volume changes (OS volume
        // slider, hardware knob, programmatic sets from other processes)
        // into PlaybackInfo so the displayed volume tracks the real
        // hardware level. Only meaningful when hardware owns the level
        // (HardwarePreferred with a volume-capable endpoint): in SoftwareOnly
        // mode the app owns the gain, so external hardware
        // changes must not touch the UI volume. Native backends report
        // these via the endpoint's change-notification callback; the
        // cpal backends return None (trait default).
        if self.volume_uses_hardware() {
            if let Some(level) = self
                .audio_output
                .as_ref()
                .and_then(|o| o.take_external_volume_change())
            {
                self.pipeline.set_volume(1.0);
                self.write_playback_info(|pb| {
                    pb.volume = level;
                    pb.volume_error = None;
                });
            }
        }

        let state = self.current_state();

        if state == PlaybackState::Playing {
            let dsp_start = Instant::now();

            // Check for crossfade trigger before decoding.
            self.check_crossfade_trigger();

            self.decode_and_process();
            if self.audio_output.is_none() {
                // Dummy-mode drain: no audio output device, but we still
                // consume from the ring buffer so the decode loop doesn't
                let mut dummy_buf = [0.0f32; 1024];
                let count = self.output_buffer.pop_block_interleaved(&mut dummy_buf);
                let _ = count;
            }
            let dsp_elapsed = dsp_start.elapsed();
            self.telemetry.dsp_time += dsp_elapsed;
            if dsp_elapsed > self.telemetry.worst_dsp_time {
                self.telemetry.worst_dsp_time = dsp_elapsed;
            }

            // Periodic stream health check.
            self.check_stream_health();

            self.recovery.successful_playback_ticks += 1;
            if self.recovery.successful_playback_ticks >= 1000 {
                if self.recovery.stream_recovery_attempts > 0 {
                    info!("Playback stable for 5 seconds; resetting stream recovery attempts");
                    self.recovery.stream_recovery_attempts = 0;
                }
                self.recovery.successful_playback_ticks = 0;
            }
        }

        if now.duration_since(self.telemetry.last_cpu_reset) >= Duration::from_secs(2) {
            let cpu_pct = if self.telemetry.total_time.as_nanos() > 0 {
                (self.telemetry.dsp_time.as_nanos() as f32 / self.telemetry.total_time.as_nanos() as f32) * 100.0
            } else {
                0.0
            };

            let resampler_disabled = self.is_resampler_disabled();
            let convolution_ir_needs_reload = self.pipeline.convolution_ir_needs_reload();

            // Drain the output backend's clip and NaN counters. The counters
            // accumulate in the audio callback (audio thread) and are reset
            // to zero on read, so the value we get here represents the
            // number of incidents that occurred during the last 2-second
            // window. We accumulate them into the PlaybackInfo fields so
            // the UI can display running totals and reset them via the
            // normal PlaybackInfo refresh flow.
            let new_clips = if let Some(ref output) = self.audio_output {
                output.take_clips()
            } else {
                0
            };
            let new_nans = if let Some(ref output) = self.audio_output {
                output.take_nans()
            } else {
                0
            };

            let (src_depth, src_codec, src_channels, src_lossless) =
                if let Some(ref s) = self.stream {
                    match s {
                        PlaybackStream::Single { decoder, .. } => {
                            let info = decoder.format_info();
                            (
                                info.bit_depth.unwrap_or(0),
                                info.codec.clone(),
                                info.channels as u32,
                                info.is_lossless,
                            )
                        }
                        PlaybackStream::Transitioning {
                            outgoing_decoder, ..
                        } => {
                            let info = outgoing_decoder.format_info();
                            (
                                info.bit_depth.unwrap_or(0),
                                info.codec.clone(),
                                info.channels as u32,
                                info.is_lossless,
                            )
                        }
                    }
                } else {
                    (0, String::new(), 0, false)
                };

            #[cfg(feature = "audio-output")]
            let out_info = self.audio_output.as_ref().map(|o| o.output_info());

            let (out_depth, out_format) = {
                #[cfg(feature = "audio-output")]
                {
                    // Prefer the backend's own `OutputInfo.sample_format` —
                    // the precise negotiated container (e.g. I24Le for the
                    // native WASAPI 24-bit-in-32 path), which the cpal
                    // vocabulary derived from `sample_format()` cannot
                    // express. Fall back to that mapping when unavailable.
                    let format = match out_info.as_ref().map(|o| o.sample_format) {
                        Some(f) if f != OutputSampleFormat::Unknown => f,
                        _ => match self.audio_output.as_ref().map(|o| o.sample_format()) {
                            Some(cpal::SampleFormat::F32) => OutputSampleFormat::F32,
                            Some(cpal::SampleFormat::F64) => OutputSampleFormat::F64,
                            Some(cpal::SampleFormat::I16) => OutputSampleFormat::I16,
                            Some(cpal::SampleFormat::U16) => OutputSampleFormat::U16,
                            Some(cpal::SampleFormat::I32) => OutputSampleFormat::I32,
                            Some(_) | None => OutputSampleFormat::Unknown,
                        },
                    };
                    (format.bit_depth().unwrap_or(0), format)
                }
                #[cfg(not(feature = "audio-output"))]
                {
                    (32, OutputSampleFormat::F32)
                }
            };

            let resampler_active = self.clock.source_sample_rate > 0
                && (self.clock.source_sample_rate != self.output_sample_rate
                    || (self.speed - 1.0).abs() > 0.001);

            let out_exclusive = {
                #[cfg(feature = "audio-output")]
                {
                    out_info
                        .as_ref()
                        .map(|o| o.is_exclusive && !o.is_fallback)
                        .unwrap_or(false)
                }
                #[cfg(not(feature = "audio-output"))]
                {
                    false
                }
            };

            // Authoritative output-domain latency terms: resampler group
            // delay (rubato `output_delay`), ring-buffer fill, and the
            // negotiated device buffer size — none hardcoded.
            let (resampler_latency_ms, ring_buffer_latency_ms, output_device_latency_ms) =
                self.output_latency_terms();

            let mut stats = self.pipeline.engine_stats_with_output_format(
                self.clock.source_sample_rate,
                self.output_sample_rate,
                src_depth,
                out_depth,
                out_format,
                resampler_active,
                out_exclusive,
                resampler_latency_ms,
                ring_buffer_latency_ms,
                output_device_latency_ms,
            );
            stats.source_sample_rate = self.clock.source_sample_rate;
            stats.output_sample_rate = self.output_sample_rate;
            stats.source_bit_depth = src_depth;
            stats.output_bit_depth = out_depth;
            #[cfg(feature = "audio-output")]
            if let Some(ref o) = out_info {
                stats.output_backend = format!("{:?}", o.actual_backend);
                stats.output_is_exclusive = o.is_exclusive;
                stats.output_is_fallback = o.is_fallback;
                // Publish the authoritative bit-perfect report: the transport
                // verdict comes from the backend's VERIFIED access state
                // (never device-name heuristics) plus the fallback flag, and
                // the §13 access fields (requested/actual/verified/fallback)
                // are filled from the same source.
                let access_report = self.pipeline.bit_perfect_report_with_access(
                    self.clock.source_sample_rate,
                    self.output_sample_rate,
                    src_depth,
                    out_depth,
                    out_format,
                    resampler_active,
                    o.access_state,
                    o.is_fallback,
                );
                // Engine-owned report fields (§13): channel counts, decoder
                // losslessness, crossfade state, the dither actually applied
                // at the quantization boundary, and the volume path. The
                // pipeline cannot know these — the engine owns them.
                let mut bp = access_report;
                bp.source_channels = src_channels;
                bp.output_channels = o.channels as u32;
                bp.decoder_lossless = src_lossless;
                bp.crossfade_active = self.pipeline.mixer().is_crossfading();
                bp.dither_active = o.dither_enabled && !out_format.is_float();
                bp.volume_path = if self.volume_uses_hardware() {
                    VolumePath::Hardware
                } else {
                    VolumePath::Software // SoftwareOnly, or HardwarePreferred fallback
                };
                // Dither and crossfade perturb the sample sequence even when
                // every pipeline condition holds: re-derive the verdict.
                bp.finalize_with_engine_state();
                stats.bit_perfect_report = bp.clone();
                stats.bit_perfect = bp.is_bit_perfect;
                stats.bit_perfect_reason = bp.reason.clone();
            }
            stats.true_peak_dbtp = self.pipeline.limiter_max_true_peak_dbtp();

            // ── Decoder description ─────────────────────────────────────
            stats.decoder_format = if src_depth > 0 {
                format!(
                    "{src_codec} {src_depth}-bit {} Hz",
                    self.clock.source_sample_rate
                )
            } else if !src_codec.is_empty() {
                format!("{src_codec} {} Hz", self.clock.source_sample_rate)
            } else {
                String::new()
            };

            // ── Resampler quality (requested vs effective vs fallback) ──
            #[cfg(feature = "resample")]
            {
                let (requested, effective, fell_back) = self.active_resampler_quality();
                stats.resampler_requested_quality = requested.description().name.to_string();
                stats.resampler_effective_quality = effective.description().name.to_string();
                stats.resampler_quality = effective.description().name.to_string();
                stats.resampler_quality_fell_back = fell_back;
            }

            // ── Metering (per-window clips + device underruns) ──────────
            stats.clip_count = new_clips;
            stats.underruns = self.telemetry.underruns_window;
            self.telemetry.underruns_total = self.telemetry.underruns_total.saturating_add(self.telemetry.underruns_window);
            stats.starvation_count = self.telemetry.underruns_total;
            stats.deadline_miss_count = self.telemetry.deadline_miss_window;

            // ── Timing & ring-buffer diagnostics ────────────────────────
            stats.dsp_time_us = self.telemetry.dsp_time.as_micros() as u64;
            stats.worst_dsp_time_us = self.telemetry.worst_dsp_time.as_micros() as u64;
            stats.total_tick_time_us = self.telemetry.total_time.as_micros() as u64;
            stats.worst_tick_time_us = self.telemetry.worst_tick_time.as_micros() as u64;
            stats.buffer_capacity_frames = self.output_buffer.capacity();
            stats.buffer_available_frames = self.output_buffer.available();
            stats.buffer_fill_ratio = if stats.buffer_capacity_frames > 0 {
                stats.buffer_available_frames as f32 / stats.buffer_capacity_frames as f32
            } else {
                0.0
            };

            let is_bp = stats.bit_perfect;

            self.playback_info.rcu(|old| {
                let mut next: PlaybackInfo = old.as_ref().clone();
                next.cpu_usage_pct = cpu_pct;
                next.resampler_disabled = resampler_disabled;
                next.convolution_ir_needs_reload = convolution_ir_needs_reload;
                next.clip_count = next.clip_count.saturating_add(new_clips);
                next.nan_count = next.nan_count.saturating_add(new_nans);
                next.engine_stats = Some(stats.clone());
                #[cfg(feature = "audio-output")]
                {
                    next.output_info = out_info.clone();
                }
                next.bit_perfect = is_bp;
                Arc::new(next)
            });
            self.telemetry.dsp_time = Duration::ZERO;
            self.telemetry.total_time = Duration::ZERO;
            self.telemetry.worst_dsp_time = Duration::ZERO;
            self.telemetry.worst_tick_time = Duration::ZERO;
            self.telemetry.deadline_miss_window = 0;
            self.telemetry.underruns_window = 0;
            self.telemetry.last_cpu_reset = now;
        }
    }

    pub fn playback_info(&self) -> PlaybackInfo {
        self.playback_info.load().as_ref().clone()
    }

    pub fn playback_info_arc(&self) -> Arc<ArcSwap<PlaybackInfo>> {
        Arc::clone(&self.playback_info)
    }

    pub fn pipeline_mut(&mut self) -> &mut DspPipeline {
        &mut self.pipeline
    }
    pub fn pipeline(&self) -> &DspPipeline {
        &self.pipeline
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: EngineConfig) {
        self.pipeline.apply_config(&config);

        // Graphic EQ layer (§9.1): when enabled in config it is the
        // authoritative source for the pipeline's EQ bands and preamp (see
        // the precedence rule on `GraphicEqConfig`).
        if config.graphic_eq.enabled {
            self.graphic_eq = crate::dsp::GraphicEq::from_config(&config.graphic_eq);
            self.sync_graphic_eq();
        }

        if config.speed_mode == config::SpeedMode::TimeStretch {
            self.pipeline.timestretcher_mut().set_speed(self.speed);
        } else if config.speed_mode == config::SpeedMode::PitchShift {
            self.pipeline
                .timestretcher_mut()
                .set_pitch_ratio(self.speed);
        } else {
            self.pipeline.timestretcher_mut().set_speed(1.0);
        }
        if config.volume_mode == config::VolumeMode::HardwarePreferred
            || config.volume_mode == config::VolumeMode::HardwareOnly
        {
            self.pipeline.set_volume(1.0);
        }

        let backend_changed = config.output_backend != self.config.output_backend
            || config.output_device != self.config.output_device;

        // Sync the user's dither preference to the active output backend
        // (if any). This matters when set_config is called without going
        // through the SetDitherEnabled command path — e.g. when loading
        // a saved config at startup, or when the user toggles dither via
        // a config-file edit. The CpalOutput's dither flag is consulted
        // by the i16/u16 audio callbacks at the integer-quantization
        // boundary.
        let dither_changed = config.dither_enabled != self.config.dither_enabled;
        if dither_changed {
            if let Some(ref output) = self.audio_output {
                // Dither must stay off while DoP is active — it would corrupt
                // the 24-bit DoP words at the i32 conversion.
                output.set_dither_enabled(config.dither_enabled && !self.dsd.dop_active);
            }
        }

        self.config = config;

        if backend_changed {
            info!("Output backend or target device changed, triggering stream recovery to apply settings.");
            if let Err(e) = self.recover_output_stream() {
                error!(
                    "Failed to recover stream after backend/device change: {}",
                    e
                );
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Set or update the current audio source metadata in the engine telemetry.
    pub fn set_source(&mut self, source: AudioSource) {
        self.current_source = Some(source.clone());
        self.write_playback_info(|pb| pb.current_source = Some(source.clone()));
    }

    /// Check if the engine has a pending chunk, which can be used as a proxy for buffer fullness
    pub fn has_pending_chunk(&self) -> bool {
        self.scratch.pending_chunk.is_some()
    }

    #[cfg(feature = "resample")]
    pub fn is_resampler_disabled(&self) -> bool {
        match &self.stream {
            Some(PlaybackStream::Single { resampler, .. }) => {
                resampler.as_ref().is_none_or(|r| r.is_disabled())
            }
            Some(PlaybackStream::Transitioning {
                incoming_resampler, ..
            }) => incoming_resampler.as_ref().is_none_or(|r| r.is_disabled()),
            None => false,
        }
    }

    /// List currently available audio output endpoint names.
    pub fn available_devices(&self) -> Vec<String> {
        self.device_monitor.current_devices().to_vec()
    }

    /// Periodically poll the audio subsystem for endpoint hotplug events.
    pub(crate) fn poll_device_monitor(&mut self) {
        if let Some(delta) = self.device_monitor.poll(false) {
            for dev in &delta.connected {
                info!("Audio output device connected: {}", dev);
                self.emit_event(EngineEvent::DeviceConnected {
                    device: dev.clone(),
                });
            }
            for dev in &delta.disconnected {
                warn!("Audio output device disconnected: {}", dev);
                self.emit_event(EngineEvent::DeviceDisconnected {
                    device: dev.clone(),
                });
            }
            if delta.changed {
                self.emit_event(EngineEvent::DeviceListChanged {
                    devices: delta.current_devices.clone(),
                });

                // Auto-recovery if the currently selected output device was disconnected
                if let Some(ref current_dev) = self.config.output_device {
                    if delta.disconnected.iter().any(|d| d == current_dev) {
                        warn!(
                            "Configured output device '{}' was disconnected; attempting stream auto-recovery with fallback",
                            current_dev
                        );
                        if let Err(e) = self.recover_output_stream() {
                            error!(
                                "Stream auto-recovery after device disconnection failed: {}",
                                e
                            );
                        }
                    } else if delta.connected.iter().any(|d| d == current_dev) {
                        info!(
                            "Configured output device '{}' reconnected; restoring stream to preferred device",
                            current_dev
                        );
                        if let Err(e) = self.recover_output_stream() {
                            error!("Stream recovery after device reconnection failed: {}", e);
                        }
                    }
                }
            }
        }
    }

    #[cfg(not(feature = "resample"))]
    pub fn is_resampler_disabled(&self) -> bool {
        false
    }

    /// Whether the active volume path is the endpoint's hardware control.
    /// `HardwarePreferred` uses hardware only when the active backend
    /// supports it; `HardwareOnly` always routes to hardware (and reports an
    /// error rather than applying software gain when unavailable); the
    /// software modes never do. With no output stream open this is `false`
    /// for `HardwarePreferred` (nothing to control), but `true` for
    /// `HardwareOnly` so the strict no-software-fallback path is preserved
    /// until the stream opens.
    fn volume_uses_hardware(&self) -> bool {
        match self.config.volume_mode {
            config::VolumeMode::HardwarePreferred => self
                .audio_output
                .as_ref()
                .is_some_and(|o| o.supports_hardware_volume()),
            config::VolumeMode::HardwareOnly => true,
            _ => false,
        }
    }

    /// Group delay of the active resampler in milliseconds (0 when the
    /// resampler is absent, disabled, or in passthrough mode).
    #[cfg(feature = "resample")]
    fn active_resampler_latency_ms(&self) -> f32 {
        match &self.stream {
            Some(PlaybackStream::Single { resampler, .. }) => {
                resampler.as_ref().map(|r| r.latency_ms()).unwrap_or(0.0)
            }
            Some(PlaybackStream::Transitioning {
                incoming_resampler, ..
            }) => incoming_resampler
                .as_ref()
                .map(|r| r.latency_ms())
                .unwrap_or(0.0),
            None => 0.0,
        }
    }

    #[cfg(not(feature = "resample"))]
    fn active_resampler_latency_ms(&self) -> f32 {
        0.0
    }

    /// (requested, effective, fell_back) quality profile of the active
    /// resampler. Falls back to the configured quality when no resampler is
    /// active, so the diagnostic panel always reports a sensible value.
    #[cfg(feature = "resample")]
    fn active_resampler_quality(
        &self,
    ) -> (config::ResamplerQuality, config::ResamplerQuality, bool) {
        let active = match &self.stream {
            Some(PlaybackStream::Single { resampler, .. }) => resampler.as_ref(),
            Some(PlaybackStream::Transitioning {
                incoming_resampler, ..
            }) => incoming_resampler.as_ref(),
            None => None,
        };
        match active {
            Some(r) => (
                r.requested_quality(),
                r.effective_quality(),
                r.quality_fell_back(),
            ),
            None => (
                self.config.resampler_quality,
                self.config.resampler_quality,
                false,
            ),
        }
    }

    /// The three output-domain latency terms the pipeline cannot observe:
    /// (resampler group delay, ring-buffer fill, negotiated device buffer),
    /// each in milliseconds at the output sample rate.
    fn output_latency_terms(&self) -> (f32, f32, f32) {
        let resampler_latency_ms = self.active_resampler_latency_ms();

        let ring_buffer_latency_ms = {
            let avail = self.output_buffer.available();
            (avail as f32 / self.output_sample_rate.max(1) as f32) * 1000.0
        };

        let output_device_latency_ms = {
            #[cfg(feature = "audio-output")]
            {
                self.audio_output
                    .as_ref()
                    .map(|o| {
                        o.buffer_size_frames() as f32 / self.output_sample_rate.max(1) as f32
                            * 1000.0
                    })
                    .unwrap_or(0.0)
            }
            #[cfg(not(feature = "audio-output"))]
            {
                0.0
            }
        };

        (
            resampler_latency_ms,
            ring_buffer_latency_ms,
            output_device_latency_ms,
        )
    }

    /// The authoritative graph-level end-to-end latency model.
    ///
    /// This is the single latency number the engine reports. It sums — all in
    /// the output domain — the safety limiter's lookahead window **plus** its
    /// detector group delay, the convolution's partition delay, the
    /// resampler's filter group delay (rubato `output_delay`), and the output
    /// buffering (ring-buffer fill + the device buffer size).
    ///
    /// Every term comes from the component that owns it. The one caveat is the
    /// device-buffer term: when the backend cannot query the driver's buffer
    /// size (cpal reports `SupportedBufferSize::Unknown`), it is a backend
    /// target estimate — `OutputInfo::buffer_size_estimated` flags that case.
    /// See [`LatencyReport`] for the breakdown.
    pub fn graph_latency(&self) -> LatencyReport {
        let (resampler_ms, ring_ms, device_ms) = self.output_latency_terms();
        self.pipeline
            .latency_report(resampler_ms, ring_ms, device_ms)
    }

    /// Latency compensation for the playhead: `(total_latency_ms,
    /// compensated_position_secs)` for a raw decoded position `pos_secs`.
    ///
    /// The decoded position leads what is audible by the end-to-end graph
    /// latency (DSP lookaheads + resampler + ring fill + device buffer), so
    /// the compensated position is clamped at 0 — before playback has
    /// progressed past the pipeline latency, nothing is audible yet.
    fn latency_compensation(&self, pos_secs: f32) -> (f32, f32) {
        let latency_ms = self.graph_latency().total_latency_ms;
        let compensated = (pos_secs - latency_ms / 1000.0).max(0.0);
        (latency_ms, compensated)
    }

    /// Access the sample-accurate integer audio clock.
    pub fn clock(&self) -> &AudioClock {
        &self.clock
    }

    /// Transactionally reconfigure the engine pipeline.
    ///
    /// Applies all DSP parameters atomically without glitches or buffer resets,
    /// triggering stream recovery only if the output backend or device changed.
    pub fn reconfigure(&mut self, config: EngineConfig) -> Result<(), EngineError> {
        self.set_config(config);
        Ok(())
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}
