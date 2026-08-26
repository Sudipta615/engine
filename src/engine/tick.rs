//! Engine tick cycle, state publication, latency modeling, and Drop.
//!
//! This module owns the hot tick loop — command processing, decode-and-process,
//! stream health checks, telemetry publication, and every accessor that hosts
//! call from the handle path.

use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use log::{error, info, warn};

use config;

use crate::{
    buffer::{PlaybackInfo, PlaybackState},
    dsp::pipeline::{DspPipeline, LatencyReport, OutputSampleFormat, VolumePath},
    events::OutputEvent,
    source::AudioSource,
};

use super::{AudioClock, AudioEngine, EngineError, PlaybackStream};

impl AudioEngine {
    /// Stream the loopback capture ring into the active WAV file. Runs every
    /// tick while a capture is active; the capture thread never touches the
    /// file, so disk stalls cannot back-pressure the realtime path. On an
    /// I/O error the capture is torn down with a `CaptureError` event.
    pub(crate) fn drain_capture(&mut self) {
        #[cfg(all(target_os = "windows", feature = "wasapi-native"))]
        {
            let Some(active) = self.capture.as_mut() else {
                return;
            };
            let mut buf = [0.0f32; 8192];
            let ch = active.capture.channels() as usize;
            loop {
                let n = active.capture.buffer().pop_frames_interleaved(&mut buf, ch);
                if n == 0 {
                    break;
                }
                if let Err(e) = active.writer.write_frames(&buf[..n * ch]) {
                    log::error!("capture write failed: {e}");
                    // Drop the capture; the WAV writer's Drop finalizes the
                    // header so the partial file stays playable.
                    let mut active = self.capture.take().unwrap();
                    active.capture.stop();
                    let path = active.path.clone();
                    self.emit_event(crate::events::EngineEvent::CaptureError(format!(
                        "capture write to '{}' failed: {e}",
                        path.display()
                    )));
                    return;
                }
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "wasapi-native")))]
        {}
    }

    /// Drive one engine tick, blocking up to `max_wait` for an incoming
    /// command so the caller does not busy-poll.
    ///
    /// This is the preferred entry point for hosts that drive the engine
    /// from their own thread (the reference CLI, the C FFI tick thread): it
    /// wakes immediately when a command arrives (no 5 ms polling latency)
    /// and sleeps efficiently when idle. If the channel is disconnected
    /// (engine torn down), the tick still runs so final state is published.
    pub fn tick_blocking(&mut self, max_wait: std::time::Duration) {
        // Consume one command so the caller wakes the moment work arrives;
        // `tick` → `process_commands` drains the remainder of the queue.
        let _ = self.cmd_rx.recv_timeout(max_wait);
        self.tick();
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
        #[cfg(feature = "audio-output")]
        self.poll_device_monitor();
        self.drain_capture();

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
                (self.telemetry.dsp_time.as_nanos() as f32
                    / self.telemetry.total_time.as_nanos() as f32)
                    * 100.0
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
            self.telemetry.underruns_total = self
                .telemetry
                .underruns_total
                .saturating_add(self.telemetry.underruns_window);
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
                next.clip_count = next.clip_count.saturating_add(new_clips as u64);
                next.nan_count = next.nan_count.saturating_add(new_nans as u64);
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

    pub fn config(&self) -> &config::EngineConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: config::EngineConfig) {
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
    #[cfg(feature = "audio-output")]
    pub fn available_devices(&self) -> Vec<String> {
        self.device_monitor.current_devices().to_vec()
    }

    /// Periodically poll the audio subsystem for endpoint hotplug events.
    #[cfg(feature = "audio-output")]
    pub(crate) fn poll_device_monitor(&mut self) {
        if let Some(delta) = self.device_monitor.poll(false) {
            for dev in &delta.connected {
                info!("Audio output device connected: {}", dev);
                self.emit_output_event(OutputEvent::DeviceConnected {
                    device: dev.clone(),
                });
            }
            for dev in &delta.disconnected {
                warn!("Audio output device disconnected: {}", dev);
                self.emit_output_event(OutputEvent::DeviceDisconnected {
                    device: dev.clone(),
                });
            }
            if delta.changed {
                self.emit_output_event(OutputEvent::DeviceListChanged {
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
    pub(super) fn latency_compensation(&self, pos_secs: f32) -> (f32, f32) {
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
    pub fn reconfigure(&mut self, config: config::EngineConfig) -> Result<(), EngineError> {
        self.set_config(config);
        Ok(())
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}
