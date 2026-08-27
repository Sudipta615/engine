//! Audio output stream recovery and health monitoring.
//!

use std::{sync::Arc, time::Duration};

use log::{error, info, warn};

use super::{AudioEngine, EngineError, PlaybackStream};
#[cfg(feature = "resample")]
use crate::buffer::PlaybackState;
#[cfg(feature = "resample")]
use crate::dsp::resampler::{AudioResampler, GenericResampler};
use crate::output::create_output;

pub(super) const MAX_RECOVERY_ATTEMPTS: u32 = 5;

#[derive(Debug, Default)]
pub(crate) struct RecoveryState {
    pub(crate) consecutive_decode_errors: u32,
    pub(crate) stream_recovery_attempts: u32,
    pub(crate) stream_recovery_burst_start: Option<std::time::Instant>,
    pub(crate) successful_playback_ticks: u32,
}

#[inline]
pub(super) fn recovery_attempt_limit_reached(attempts: u32) -> bool {
    attempts >= MAX_RECOVERY_ATTEMPTS
}

impl AudioEngine {
    /// Attempt to recover the audio output stream after a device change
    /// or error. This pauses decoding, re-detects the output device,
    /// rebuilds the stream at the new sample rate, and hot-swaps the
    /// output without requiring an application restart.
    pub fn recover_output_stream(&mut self) -> Result<(), EngineError> {
        /// Cooldown after which the attempt counter is reset, allowing the
        /// engine to retry recovery instead of being permanently stuck.
        /// 30 seconds is long enough to avoid tight retry loops but short
        /// enough that a user who reconnects a USB audio device half a
        /// minute later will get playback back automatically.
        const RECOVERY_COOLDOWN_SECS: u64 = 30;
        if recovery_attempt_limit_reached(self.recovery.stream_recovery_attempts) {
            let now = std::time::Instant::now();
            let should_reset = self
                .recovery
                .stream_recovery_burst_start
                .map(|start| now.duration_since(start).as_secs() >= RECOVERY_COOLDOWN_SECS)
                .unwrap_or(true);
            if should_reset {
                info!(
                    "Stream recovery: resetting attempt counter after {}s cooldown \
                     (had {} failed attempts)",
                    RECOVERY_COOLDOWN_SECS, self.recovery.stream_recovery_attempts
                );
                self.recovery.stream_recovery_attempts = 0;
                self.recovery.stream_recovery_burst_start = None;
            } else {
                return Err(EngineError::StreamRecovery(format!(
                    "Exceeded maximum stream recovery attempts ({}); \
                     retrying in {}s",
                    MAX_RECOVERY_ATTEMPTS, RECOVERY_COOLDOWN_SECS
                )));
            }
        }

        // Record the burst start time on the first attempt of a new burst.
        if self.recovery.stream_recovery_attempts == 0 {
            self.recovery.stream_recovery_burst_start = Some(std::time::Instant::now());
        }

        self.recovery.stream_recovery_attempts += 1;
        info!(
            "Attempting stream recovery (attempt {}/{})",
            self.recovery.stream_recovery_attempts, MAX_RECOVERY_ATTEMPTS
        );

        // Stop the current output.
        if let Some(mut output) = self.audio_output.take() {
            output.stop();
        }

        // Settle debounce: allow a minimum settle window (50ms) for the audio endpoint
        // and driver state to stabilize without spawning throwaway worker threads.
        const SETTLE_DEBOUNCE: Duration = Duration::from_millis(50);
        if let Some(burst_start) = self.recovery.stream_recovery_burst_start {
            let elapsed = burst_start.elapsed();
            if elapsed < SETTLE_DEBOUNCE {
                std::thread::sleep(SETTLE_DEBOUNCE - elapsed);
            }
        }

        // Re-detect the output device and sample rate.
        let old_rate = self.output_sample_rate;

        // Reuse the existing engine buffer. The old output has been stopped,
        // but its buffered samples are still valid engine state; allocating a
        // replacement here used to discard the device FIFO on every recovery.
        // Keeping the same Arc lets a replug/rate change resume from the
        // already-produced audio instead of rebuilding from an empty buffer.
        let new_buffer = Arc::clone(&self.output_buffer);

        // An active output profile's backend preference wins over the config
        // default when the stream is (re)created.
        let audio_backend = self.profile_backend().unwrap_or(self.config.output_backend);
        let mut new_output = create_output(
            Arc::clone(&new_buffer),
            audio_backend,
            self.config.output_device.as_deref(),
            self.config.fallback_policy,
        )?;
        // Sync the user's dither preference to the new output. The new
        // output defaults to dither enabled; we re-apply the user's
        // setting so recovery doesn't silently flip dither on/off.
        new_output.set_dither_enabled(self.config.dither_enabled);
        let mut actual_rate = new_output.sample_rate();

        // Re-negotiate native DSD before starting the replacement output.
        // Starting a PCM stream while the decoder is still emitting raw DSD
        // would either lose the bitstream or feed it into the wrong transport.
        // Exact wire/rate/channel support is verified by the backend here.
        let mut native_was_disabled = false;
        let mut dop_was_disabled = false;
        let mut dop_restored = false;
        let mut native_failure_reason: Option<String> = None;
        let mut needs_start = true;
        if self.dsd.native_dsd_active {
            let native_result = self.active_native_dsd_params();
            match native_result {
                Some((bit_rate, channels, wire_format, buffer)) => {
                    let params = crate::output::NativeDsdParams {
                        wire_format,
                        bit_rate,
                        channels,
                        buffer,
                    };
                    match new_output.set_native_dsd(Some(params)) {
                        Ok(Some(actual_format)) => {
                            actual_rate = new_output.sample_rate();
                            self.dsd.dsd_wire_format = Some(actual_format);
                            self.dsd.dsd_transport_report.actual =
                                crate::decode::DsdTransport::Native;
                            self.dsd.dsd_transport_report.wire_format = Some(actual_format);
                            needs_start = false;
                            new_output.set_dither_enabled(false);
                            info!(
                                "Native DSD transport restored during recovery: {} at {} Hz",
                                actual_format.label(),
                                actual_rate
                            );
                        }
                        Ok(None) => {
                            let error = "backend returned PCM while native DSD was requested";
                            warn!("Native DSD recovery failed: {error}; attempting DoP fallback");
                            self.disable_native_dsd_state();
                            native_failure_reason = Some(error.to_string());
                            native_was_disabled = true;
                        }
                        Err(e) => {
                            warn!(
                                "Native DSD recovery failed ({}); attempting DoP fallback",
                                e
                            );
                            native_failure_reason = Some(e.to_string());
                            self.disable_native_dsd_state();
                            native_was_disabled = true;
                        }
                    }
                }
                None => {
                    let error = "active native DSD state is incomplete";
                    warn!("Native DSD recovery failed: {error}; attempting DoP fallback");
                    native_failure_reason = Some(error.to_string());
                    self.disable_native_dsd_state();
                    native_was_disabled = true;
                }
            }
        }

        // A native-DSD recovery failure follows the same explicit fallback
        // order as initial track negotiation: try DoP before PCM conversion.
        // The decoder is switched only after native transport has failed, so
        // raw DSD can never be sent to a PCM stream accidentally.
        if native_was_disabled
            && self.config.dsd_output == config::DsdOutput::NativeDsd
            && !self.dsd.dop_active
        {
            if let Some(dop_rate) = self.enable_dop_for_recovery() {
                let direct = new_output.output_info().access_state.is_bit_perfect();
                if direct {
                    match new_output.reconfigure_sample_format(dop_rate, cpal::SampleFormat::I32) {
                        Ok(r)
                            if r == dop_rate
                                && new_output.sample_format() == cpal::SampleFormat::I32
                                && new_output.output_info().access_state.is_bit_perfect() =>
                        {
                            actual_rate = r;
                            self.dsd.dop_active = true;
                            self.dsd.dop_rate = r;
                            self.graph.set_dop_bypass(true);
                            new_output.set_dither_enabled(false);
                            needs_start = false;
                            dop_restored = true;
                            let reason = native_failure_reason
                                .as_deref()
                                .unwrap_or("native DSD transport was unavailable");
                            self.dsd
                                .dsd_transport_report
                                .step(format!("native DSD unavailable during recovery ({reason})"));
                            self.dsd.dsd_transport_report.step("fallback: DoP");
                            self.dsd.dsd_transport_report.actual = crate::decode::DsdTransport::Dop;
                            let report = self.dsd.dsd_transport_report.clone();
                            self.write_playback_info(|pb| {
                                pb.native_dsd_active = false;
                                pb.dop_active = true;
                                pb.dsd_transport = crate::decode::DsdTransport::Dop;
                                pb.dsd_transport_report = report.clone();
                            });
                            info!(
                                "DoP transport restored after native DSD recovery failure at {} Hz",
                                r
                            );
                        }
                        Ok(r) => {
                            warn!(
                                "DoP recovery settled on {} Hz/{:?}; required {} Hz/I32",
                                r,
                                new_output.sample_format(),
                                dop_rate
                            );
                            self.disable_dop_for_stream();
                        }
                        Err(e) => {
                            warn!("DoP recovery failed: {e}");
                            self.disable_dop_for_stream();
                        }
                    }
                } else {
                    warn!("DoP recovery requires verified direct output; using PCM conversion");
                    self.disable_dop_for_stream();
                }
            }
            if !dop_restored {
                let reason = native_failure_reason
                    .as_deref()
                    .unwrap_or("native DSD transport was unavailable");
                self.record_native_dsd_pcm_fallback(reason);
            }
        }

        // If a DoP stream is active, the new output must run at the DoP rate
        // (bit_rate/16) in exclusive mode; otherwise fall back to DSD→PCM.
        if self.dsd.dop_active && !dop_restored {
            match new_output.reconfigure_sample_format(self.dsd.dop_rate, cpal::SampleFormat::I32) {
                Ok(r)
                    if r == self.dsd.dop_rate
                        && new_output.output_info().access_state.is_bit_perfect() =>
                {
                    actual_rate = r;
                    needs_start = false; // reconfigure already started the stream
                    new_output.set_dither_enabled(false);
                }
                Ok(r) => {
                    warn!(
                        "DoP unsupported by the new output ({} Hz without verified I32 \
                         exclusive access); falling back to DSD→PCM",
                        r
                    );
                    actual_rate = r;
                    self.disable_dop_for_stream();
                    self.record_dop_pcm_fallback(&format!(
                        "new output settled on {} Hz without verified I32 exclusive access",
                        r
                    ));
                    dop_was_disabled = true;
                }
                Err(e) => {
                    warn!(
                        "DoP output reconfiguration failed ({}); falling back to DSD→PCM",
                        e
                    );
                    self.disable_dop_for_stream();
                    self.record_dop_pcm_fallback(&e.to_string());
                    dop_was_disabled = true;
                }
            }
        }
        if needs_start {
            new_output.start()?;
        }

        self.audio_output = Some(new_output);
        self.output_sample_rate = actual_rate;
        // Re-select and re-apply the output profile for the (possibly new)
        // device — per-device profiles follow device changes.
        self.refresh_output_profile();
        // Multi-endpoint routing matrix: reopen the additional endpoints
        // against the (possibly new) master rate. Failures are logged and
        // the endpoint dropped — recovery of the primary must never be
        // blocked by a secondary device.
        self.open_additional_endpoints();
        // Treat a DoP fallback as a rate change even if the number happens to
        // match: the source rate changed (DoP rate → PCM rate) and the
        // resampler must be rebuilt from the decoder's current info.
        let sample_rate_changed =
            actual_rate != old_rate || dop_was_disabled || native_was_disabled;

        // A normal device/rate transition keeps source-domain decoder chunks,
        // output-domain FIFOs, the ring-buffered audio, and DSP state alive.
        // Resamplers have their own bounded asynchronous rebuild path, which
        // preserves their pending input/output and blends the old filter tail
        // into the new ratio. Only a DoP→PCM encoding transition must flush
        // encoded samples, because DoP words cannot be mixed with PCM frames.
        if dop_was_disabled || native_was_disabled {
            self.sample_sink.reset();
            self.scratch.pending_output_frames.clear();
            self.scratch.pending_multichannel.clear();
            self.scratch.pending_multichannel_channels = 0;
            self.scratch.pending_chunk = None;
            self.scratch.pending_incoming_chunk = None;
            self.scratch.rs_out_buf.clear();
            self.scratch.rs_in_buf.clear();
            self.scratch.mix_l.clear();
            self.scratch.mix_r.clear();
        }

        // If the sample rate changed, update the rate-dependent state without
        // reconstructing the whole playback pipeline. This is especially
        // important when a device changes during a crossfade: its envelope
        // progress and both resampler FIFOs remain continuous.
        if sample_rate_changed {
            info!(
                "Sample rate changed during recovery: {} Hz -> {} Hz",
                old_rate, actual_rate
            );
            self.graph.update_sample_rate(actual_rate as f32);
            if old_rate != actual_rate {
                if let Some(PlaybackStream::Transitioning {
                    crossfade_frames_remaining,
                    crossfade_total_frames,
                    ..
                }) = self.stream.as_mut()
                {
                    *crossfade_frames_remaining = Self::rescale_frame_count(
                        *crossfade_frames_remaining,
                        old_rate,
                        actual_rate,
                    );
                    *crossfade_total_frames =
                        Self::rescale_frame_count(*crossfade_total_frames, old_rate, actual_rate);
                }
            }

            #[cfg(feature = "resample")]
            if let Some(ref mut stream) = self.stream {
                match stream {
                    PlaybackStream::Single { resampler, .. } => {
                        if let Some(r) = resampler {
                            r.set_output_rate(actual_rate as f32);
                        }
                    }
                    PlaybackStream::Transitioning {
                        outgoing_resampler,
                        incoming_resampler,
                        ..
                    } => {
                        if let Some(r) = outgoing_resampler {
                            r.set_output_rate(actual_rate as f32);
                        }
                        if let Some(r) = incoming_resampler {
                            r.set_output_rate(actual_rate as f32);
                        }
                    }
                }
            }
        }

        // A DSD transport fallback changed the decoder's source format, so
        // rebuild only the affected resampler(s) after encoded/raw FIFOs have
        // been flushed.
        if dop_was_disabled || native_was_disabled {
            // Rebuild resampler(s) if we have an active stream.
            #[cfg(feature = "resample")]
            if let Some(ref mut stream) = self.stream {
                match stream {
                    PlaybackStream::Single { decoder, resampler } => {
                        let source_rate = decoder.info().sample_rate;
                        *resampler = build_resampler(
                            self.config.resampler_quality,
                            source_rate as f32,
                            actual_rate as f32,
                            self.speed,
                            self.config.precision_mode,
                        );
                        if resampler.is_none()
                            && (source_rate != actual_rate || (self.speed - 1.0).abs() > 0.001)
                        {
                            error!("Critical: Resampler required ({} Hz -> {} Hz) but failed to build!", source_rate, actual_rate);
                            self.write_playback_info(|pb| {
                                pb.resampler_disabled = true;
                                pb.resampler_failed_fatal = true;
                                pb.engine_error = Some("Resampler failed to build for new sample rate; halted to prevent incorrect pitch".to_string());
                            });
                            // Actually halt: continuing would play at the wrong
                            // rate/pitch (the decode path treats a missing
                            // resampler as passthrough).
                            self.stream_ended = true;
                            self.update_playback_state(PlaybackState::Stopped);
                        }
                    }
                    PlaybackStream::Transitioning {
                        outgoing_decoder,
                        outgoing_resampler,
                        incoming_decoder,
                        incoming_resampler,
                        ..
                    } => {
                        // Rebuild outgoing resampler
                        let out_rate = outgoing_decoder.info().sample_rate;
                        *outgoing_resampler = build_resampler(
                            self.config.resampler_quality,
                            out_rate as f32,
                            actual_rate as f32,
                            self.speed,
                            self.config.precision_mode,
                        );
                        // Rebuild incoming resampler
                        let in_rate = incoming_decoder.info().sample_rate;
                        *incoming_resampler = build_resampler(
                            self.config.resampler_quality,
                            in_rate as f32,
                            actual_rate as f32,
                            self.speed,
                            self.config.precision_mode,
                        );
                        if (outgoing_resampler.is_none()
                            && (out_rate != actual_rate || (self.speed - 1.0).abs() > 0.001))
                            || (incoming_resampler.is_none()
                                && (in_rate != actual_rate || (self.speed - 1.0).abs() > 0.001))
                        {
                            error!("Critical: Resampler required for transitioning stream but failed to build!");
                            self.write_playback_info(|pb| {
                                pb.resampler_disabled = true;
                                pb.resampler_failed_fatal = true;
                                pb.engine_error = Some("Resampler build failed during transition; halted to prevent incorrect pitch".to_string());
                            });
                            // Actually halt (see the Single case above).
                            self.stream_ended = true;
                            self.update_playback_state(PlaybackState::Stopped);
                        }
                    }
                }
            }
        }

        self.recovery.successful_playback_ticks = 0; // Reset the stability timer on recovery
        self.recovery.stream_recovery_attempts = 0;
        self.recovery.stream_recovery_burst_start = None;
        info!(
            "Stream recovery successful (output rate: {} Hz)",
            actual_rate
        );
        Ok(())
    }

    /// Convert a source-frame count measured at one rate to the equivalent
    /// count at another rate (used when DoP is disabled mid-stream: DoP frames
    /// at bit_rate/16 become PCM frames at bit_rate/32, so the playhead must be
    /// rescaled to stay accurate). Rounding keeps the position monotonic and
    /// the same guards as [`Self::rescale_frame_count`] apply.
    #[inline]
    fn rescale_source_frames(frames: u64, old_rate: u32, new_rate: u32) -> u64 {
        if old_rate == 0 || new_rate == 0 || old_rate == new_rate {
            return frames;
        }
        (((frames as u128 * new_rate as u128 + (old_rate as u128 / 2)) / old_rate as u128)
            .min(u64::MAX as u128)) as u64
    }

    /// Convert a frame count measured at one output rate to the equivalent
    /// duration at another rate. Saturating arithmetic keeps malformed device
    /// reports from overflowing the transition state.
    #[inline]
    fn rescale_frame_count(frames: usize, old_rate: u32, new_rate: u32) -> usize {
        if old_rate == 0 || new_rate == 0 || old_rate == new_rate {
            return frames;
        }
        ((frames as u128 * new_rate as u128 + (old_rate as u128 / 2)) / old_rate as u128)
            .min(usize::MAX as u128) as usize
    }

    /// Return the active native-DSD negotiation context used when rebuilding
    /// an output after device removal.
    fn active_native_dsd_params(
        &self,
    ) -> Option<(
        u32,
        u16,
        crate::decode::dsd::DsdWireFormat,
        Arc<crate::buffer::DsdByteBuffer>,
    )> {
        let bit_rate = match &self.stream {
            Some(PlaybackStream::Single { decoder, .. }) => decoder.dsd_bit_rate()?,
            Some(PlaybackStream::Transitioning {
                outgoing_decoder, ..
            }) => outgoing_decoder.dsd_bit_rate()?,
            None => return None,
        };
        let channels = match &self.stream {
            Some(PlaybackStream::Single { decoder, .. }) => decoder.info().channels,
            Some(PlaybackStream::Transitioning {
                outgoing_decoder, ..
            }) => outgoing_decoder.info().channels,
            None => return None,
        } as u16;
        Some((
            bit_rate,
            channels,
            self.dsd.dsd_wire_format?,
            self.dsd.dsd_byte_buffer.clone()?,
        ))
    }

    /// Leave native DSD after a failed recovery negotiation, but do not yet
    /// choose the next transport. Recovery must try DoP first when policy
    /// permits; committing PCM here would make the fallback order depend on
    /// which backend failed rather than on the negotiated capabilities.
    fn disable_native_dsd_state(&mut self) {
        self.dsd.native_dsd_active = false;
        self.dsd.dsd_wire_format = None;
        self.dsd.dsd_byte_buffer = None;
        self.graph.set_dop_bypass(false);

        let old_rate = self.clock.source_sample_rate;
        let new_rate = if let Some(ref mut stream) = self.stream {
            match stream {
                PlaybackStream::Single { decoder, .. } => {
                    decoder.set_native_dsd_mode(false);
                }
                PlaybackStream::Transitioning {
                    outgoing_decoder,
                    incoming_decoder,
                    ..
                } => {
                    outgoing_decoder.set_native_dsd_mode(false);
                    incoming_decoder.set_native_dsd_mode(false);
                }
            }
            stream.active_sample_rate()
        } else {
            0
        };
        if new_rate > 0 && old_rate != new_rate {
            self.clock.source_frames =
                Self::rescale_source_frames(self.clock.source_frames, old_rate, new_rate);
            self.clock.source_sample_rate = new_rate;
        }
    }

    /// Record the terminal PCM-conversion result after native and DoP
    /// recovery attempts have both failed. This is kept separate from the
    /// state transition so a successful DoP fallback never reports a stale
    /// `actual = PCM conversion` result.
    fn record_native_dsd_pcm_fallback(&mut self, reason: &str) {
        self.dsd.dsd_transport_report.actual = crate::decode::DsdTransport::PcmConversion;
        self.dsd.dsd_transport_report.wire_format = None;
        self.dsd
            .dsd_transport_report
            .step(format!("native DSD unavailable during recovery ({reason})"));
        self.dsd
            .dsd_transport_report
            .step("fallback: DSD→PCM conversion");
        let report = self.dsd.dsd_transport_report.clone();
        self.write_playback_info(|pb| {
            pb.native_dsd_active = false;
            pb.dop_active = false;
            pb.dsd_transport = crate::decode::DsdTransport::PcmConversion;
            pb.dsd_transport_report = report.clone();
        });
    }

    /// Switch the active DSD decoder(s) to DoP and return the requested DoP
    /// frame rate. The output remains untouched until the caller proves an
    /// exact I32/direct stream can be opened.
    fn enable_dop_for_recovery(&mut self) -> Option<u32> {
        let (bit_rate, channels) = match self.stream.as_ref()? {
            PlaybackStream::Single { decoder, .. } => {
                (decoder.dsd_bit_rate()?, decoder.info().channels)
            }
            PlaybackStream::Transitioning {
                outgoing_decoder, ..
            } => (
                outgoing_decoder.dsd_bit_rate()?,
                outgoing_decoder.info().channels,
            ),
        };
        if channels > 2 {
            return None;
        }
        let old_rate = self.clock.source_sample_rate;
        match self.stream.as_mut()? {
            PlaybackStream::Single { decoder, .. } => decoder.set_dop_mode(true),
            PlaybackStream::Transitioning {
                outgoing_decoder,
                incoming_decoder,
                ..
            } => {
                outgoing_decoder.set_dop_mode(true);
                incoming_decoder.set_dop_mode(true);
            }
        }
        let new_rate = self.stream.as_ref()?.active_sample_rate();
        if new_rate > 0 && old_rate != new_rate {
            self.clock.source_frames =
                Self::rescale_source_frames(self.clock.source_frames, old_rate, new_rate);
            self.clock.source_sample_rate = new_rate;
        }
        Some(bit_rate / 16)
    }

    /// Disable DoP for the active stream(s), falling back to DSD→PCM
    /// decimation mid-stream, and convert the playhead clock so the position
    /// stays accurate (DoP frames at bit_rate/16 ↔ PCM frames at bit_rate/32).
    fn disable_dop_for_stream(&mut self) {
        self.dsd.dop_active = false;
        self.dsd.dop_rate = 0;
        self.graph.set_dop_bypass(false);
        if let Some(ref mut stream) = self.stream {
            let old_rate = self.clock.source_sample_rate;
            match stream {
                PlaybackStream::Single { decoder, .. } => decoder.set_dop_mode(false),
                PlaybackStream::Transitioning {
                    outgoing_decoder,
                    incoming_decoder,
                    ..
                } => {
                    outgoing_decoder.set_dop_mode(false);
                    incoming_decoder.set_dop_mode(false);
                }
            }
            let new_rate = stream.active_sample_rate();
            if new_rate > 0 && old_rate != new_rate {
                self.clock.source_frames =
                    Self::rescale_source_frames(self.clock.source_frames, old_rate, new_rate);
                self.clock.source_sample_rate = new_rate;
            }
        }
        if let Some(ref output) = self.audio_output {
            output.set_dither_enabled(self.config.dither_enabled);
        }
    }

    /// Record a failed DoP recovery as an explicit terminal PCM fallback.
    fn record_dop_pcm_fallback(&mut self, reason: &str) {
        self.dsd.dsd_transport_report.actual = crate::decode::DsdTransport::PcmConversion;
        self.dsd.dsd_transport_report.wire_format = None;
        self.dsd
            .dsd_transport_report
            .step(format!("DoP unavailable during recovery ({reason})"));
        self.dsd
            .dsd_transport_report
            .step("fallback: DSD→PCM conversion");
        let report = self.dsd.dsd_transport_report.clone();
        self.write_playback_info(|pb| {
            pb.native_dsd_active = false;
            pb.dop_active = false;
            pb.dsd_transport = crate::decode::DsdTransport::PcmConversion;
            pb.dsd_transport_report = report.clone();
        });
    }

    /// Check if the audio output has encountered an error that requires
    /// stream recovery (e.g., device disconnection). Also checks for
    /// device changes by comparing the current device against the default.
    pub(super) fn check_stream_health(&mut self) {
        if let Some(ref output) = self.audio_output {
            // Drain every diagnostic event reported by the backend. A device
            // unplug can produce several distinct failures before the engine
            // gets scheduled; retain them all for logs and recovery policy.
            let errors = output.take_stream_errors();
            if !errors.is_empty() {
                for event in &errors.events {
                    warn!(
                        "Audio stream error [{}::{:?}]: {} ({})",
                        event.error_type, event.kind, event.message, event.details
                    );
                }
                if errors.dropped > 0 {
                    warn!(
                        "Audio stream error queue overflowed; {} additional event(s) were dropped",
                        errors.dropped
                    );
                }
                warn!("Audio stream error detected — attempting recovery");
                match self.recover_output_stream() {
                    Ok(()) => {
                        info!("Stream recovered after error detection");
                        self.write_playback_info(|pb| pb.engine_error = None);
                    }
                    Err(e) => {
                        let err_msg = format!("Stream recovery failed: {}", e);
                        error!("{}", err_msg);
                        self.write_playback_info(|pb| pb.engine_error = Some(err_msg.clone()));
                    }
                }
                return;
            }

            // High underrun count can indicate stream issues. Drain the
            // per-tick counter and accumulate it into the engine's
            // reporting-window total (published in `EngineStats`) instead of
            // discarding it after the warning check.
            let underruns = output.take_underruns();
            self.telemetry.underruns_window =
                self.telemetry.underruns_window.saturating_add(underruns);
            if underruns > 10 {
                warn!(
                    "High underrun count ({}) detected; may indicate device issue",
                    underruns
                );
            }
        }
    }
}

/// Shared helper for creating a resampler with the engine's current config
/// and speed settings. Eliminates duplicated match/Ok/Err blocks across
/// `load_track`, `begin_crossfade_transition`, and `recover_output_stream`.
///
/// Returns `None` if the resampler feature is disabled or if creation fails
/// (a warning is logged on failure).
#[cfg(feature = "resample")]
pub(super) fn build_resampler(
    quality: config::ResamplerQuality,
    source_rate: f32,
    output_rate: f32,
    speed: f32,
    precision: config::PrecisionMode,
) -> Option<GenericResampler> {
    match precision {
        config::PrecisionMode::Performance => {
            match AudioResampler::<f32>::new(quality, source_rate, output_rate) {
                Ok(mut r) => {
                    if (speed - 1.0).abs() > 0.001 {
                        r.set_speed(speed);
                    }
                    Some(GenericResampler::F32(r))
                }
                Err(e) => {
                    warn!("Failed to create f32 resampler: {}", e);
                    None
                }
            }
        }
        config::PrecisionMode::Quality => {
            match AudioResampler::<f64>::new(quality, source_rate, output_rate) {
                Ok(mut r) => {
                    if (speed - 1.0).abs() > 0.001 {
                        r.set_speed(speed);
                    }
                    Some(GenericResampler::F64(r))
                }
                Err(e) => {
                    warn!("Failed to create f64 resampler: {}", e);
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{recovery_attempt_limit_reached, AudioEngine, MAX_RECOVERY_ATTEMPTS};

    #[test]
    fn recovery_rescales_transition_frames_by_duration() {
        assert_eq!(AudioEngine::rescale_frame_count(480, 48_000, 96_000), 960);
        assert_eq!(AudioEngine::rescale_frame_count(960, 96_000, 48_000), 480);
        assert_eq!(AudioEngine::rescale_frame_count(123, 48_000, 48_000), 123);
        // Round-trip through a rate change preserves the duration.
        let original = 123_456usize;
        let up = AudioEngine::rescale_frame_count(original, 48_000, 88_200);
        let down = AudioEngine::rescale_frame_count(up, 88_200, 48_000);
        assert!(
            (down as i128 - original as i128).unsigned_abs() <= 1,
            "round trip must stay within one frame: {original} -> {up} -> {down}"
        );
    }

    #[test]
    fn recovery_rescale_handles_invalid_rates_without_losing_state() {
        assert_eq!(AudioEngine::rescale_frame_count(123, 0, 96_000), 123);
        assert_eq!(AudioEngine::rescale_frame_count(123, 48_000, 0), 123);
    }

    #[test]
    fn recovery_rescale_saturates_instead_of_overflowing() {
        // Malformed device reports (huge frame count × huge rate ratio) must
        // saturate at usize::MAX, never wrap or panic.
        let saturated = AudioEngine::rescale_frame_count(usize::MAX / 2, 1, u32::MAX);
        assert_eq!(saturated, usize::MAX);
        assert_eq!(
            AudioEngine::rescale_frame_count(usize::MAX, 44_100, 192_000),
            usize::MAX
        );
    }

    #[test]
    fn recovery_attempt_limit_bounds() {
        assert!(!recovery_attempt_limit_reached(MAX_RECOVERY_ATTEMPTS - 1));
        assert!(recovery_attempt_limit_reached(MAX_RECOVERY_ATTEMPTS));
        assert!(recovery_attempt_limit_reached(u32::MAX));
    }

    #[test]
    fn recovery_rescales_source_frames_for_dop_fallback() {
        // DoP→PCM fallback: DSD64 DoP frames at 176.4 kHz become PCM frames
        // at 88.2 kHz — the playhead halves while preserving the duration.
        assert_eq!(
            AudioEngine::rescale_source_frames(176_400, 176_400, 88_200),
            88_200
        );
        assert_eq!(
            AudioEngine::rescale_source_frames(88_200, 88_200, 176_400),
            176_400
        );
        // Rounding: 1 frame at 176.4k ≈ 0.5 frames at 88.2k — rounds to 1.
        assert_eq!(AudioEngine::rescale_source_frames(1, 176_400, 88_200), 1);
        // Identical rates and zero rates pass through unchanged.
        assert_eq!(AudioEngine::rescale_source_frames(42, 44_100, 44_100), 42);
        assert_eq!(AudioEngine::rescale_source_frames(42, 0, 88_200), 42);
        assert_eq!(AudioEngine::rescale_source_frames(42, 88_200, 0), 42);
        // Saturation instead of wrap on absurd ratios.
        assert_eq!(
            AudioEngine::rescale_source_frames(u64::MAX, 1, u32::MAX),
            u64::MAX
        );
    }
}
