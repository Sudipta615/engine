//! Track loading, DSD transport negotiation, and gapless handoff.
//!
//! This module owns the decode→engine integration: opening decoders,
//! negotiating native-DSD / DoP / PCM transport paths, resampler creation,
//! loudness metadata retrieval, and the state-preserving `swap_to_next_track`.

use std::path::Path;
use std::sync::Arc;

use log::{error, info, warn};

use config;

#[cfg(feature = "resample")]
use crate::dsp::resampler::GenericResampler;
use crate::{
    buffer::{PlaybackInfo, PlaybackState},
    decode::{DecodeInfo, Decoder},
    events::EngineEvent,
    output::OutputError,
    source::AudioSource,
};

use super::{dop_exclusive_reason, recovery, AudioEngine, EngineError, PlaybackStream};

impl AudioEngine {
    pub fn load_track(&mut self, path: &Path) -> Result<DecodeInfo, EngineError> {
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
    pub fn load_memory(
        &mut self,
        data: Vec<u8>,
        extension_hint: Option<&str>,
    ) -> Result<DecodeInfo, EngineError> {
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
        loudness_path: Option<&Path>,
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
            self.graph
                .update_sample_rate(self.output_sample_rate as f32);
            self.dsd.native_dsd_active = false;
            self.dsd.dsd_wire_format = None;
            self.dsd.dsd_byte_buffer = None;
            self.graph.set_dop_bypass(false);
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
                            self.graph.update_sample_rate(dr as f32);
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
            self.graph.set_dop_bypass(true);
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
            self.graph.set_dop_bypass(false);
            if let Some(ref output) = self.audio_output {
                output.set_dither_enabled(self.config.dither_enabled);
            }
        }

        let info = decoder.info().clone();
        self.clock.reset_track(info.sample_rate);
        self.analyzer.set_sample_rate(info.sample_rate);
        self.duration_secs = info.duration_secs;
        self.recovery.consecutive_decode_errors = 0;

        if !dop_active && !native_dsd_active {
            if let Some(ref mut output) = self.audio_output {
                let caps = output.capabilities();
                let target_rate =
                    caps.best_rate_for(info.sample_rate, &self.config.sample_rate_policy);
                if let Ok(actual_rate) = output.reconfigure_sample_rate(target_rate) {
                    self.output_sample_rate = actual_rate;
                    self.graph.update_sample_rate(actual_rate as f32);
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
            error!(
                "Critical: Resampler required ({} Hz -> {} Hz) but could not be initialized!",
                self.clock.source_sample_rate, self.output_sample_rate
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
            self.sample_sink.reset();
        }
        self.scratch.pending_output_frames.clear();
        self.scratch.pending_multichannel.clear();
        self.scratch.pending_multichannel_channels = 0;
        self.scratch.pending_chunk = None;
        self.scratch.pending_incoming_chunk = None;
        self.scratch.rs_out_buf.clear();
        self.scratch.rs_in_buf.clear();
        self.graph.reset();

        let current_volume = self.playback_info.load().volume;
        self.graph.set_volume(current_volume);
        self.graph.volume_mut().processor.snap();

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
        self.graph
            .apply_loudness_metadata_outgoing(Some(loudness_meta));
        if loudness_path.is_some() {
            self.start_loudness_scan();
        }

        self.graph.begin_playing();

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
                    crate::decode::percent_decode(stripped)
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| {
                            EngineError::InvalidSource(format!("Invalid file URI: {}", uri))
                        })?
                } else {
                    std::path::PathBuf::from(uri)
                };
                self.load_track(&path_buf)
            }
            AudioSource::Memory {
                data,
                extension_hint,
            } => self.load_memory(data.clone(), extension_hint.as_deref()),
        }
    }

    /// Pre-open the next [`AudioSource`] for seamless gapless / crossfade transition.
    pub fn prepare_next_source(&mut self, source: &AudioSource) -> Result<DecodeInfo, EngineError> {
        match source {
            AudioSource::File(path) => self.prepare_next_track(path),
            AudioSource::Uri(uri) => {
                let path_buf = if let Some(stripped) = uri.strip_prefix("file://") {
                    crate::decode::percent_decode(stripped)
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| {
                            EngineError::InvalidSource(format!("Invalid file URI: {}", uri))
                        })?
                } else {
                    std::path::PathBuf::from(uri)
                };
                self.prepare_next_track(&path_buf)
            }
            AudioSource::Memory {
                data,
                extension_hint,
            } => {
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
        let out_info = {
            let Some(output) = self.audio_output.as_mut() else {
                return Err(OutputError::StreamError(
                    "no output device is active".to_string(),
                ));
            };
            let caps = output.native_dsd_capability_matrix();
            let info = output.output_info();
            (caps, info)
        };
        let (caps, out_info) = out_info;
        let output = self.audio_output.as_mut().unwrap();

        // Guard: native DSD requires an exclusive, bit-perfect transport
        // path. On shared-mode backends (cpal Auto), the DSD capability
        // matrix is always empty — but we surface a per-platform error
        // explaining which exclusive backend to select instead, matching
        // the DoP path's diagnostic contract.
        if caps.is_empty() {
            let hint = if out_info.is_fallback {
                match &out_info.fallback_reason {
                    Some(r) => {
                        format!("the exclusive backend request fell back to a shared device ({r})")
                    }
                    None => {
                        "the exclusive backend request fell back to a shared device".to_string()
                    }
                }
            } else if self.config.output_backend == config::AudioBackend::Auto {
                if cfg!(target_os = "windows") {
                    "select AudioBackend::ExclusiveAsio for native DSD transport \
                     (enable the 'asio-native' or 'asio' feature)"
                        .to_string()
                } else if cfg!(target_os = "linux") {
                    "select AudioBackend::ExclusiveAlsa with a direct hw: device \
                     for native DSD transport"
                        .to_string()
                } else {
                    "switch to an exclusive backend for native DSD transport".to_string()
                }
            } else if self.config.output_backend == config::AudioBackend::ExclusiveAsio
                && !cfg!(feature = "asio")
                && !cfg!(feature = "asio-native")
            {
                "the ASIO backend is not compiled in (enable the 'asio-native' or 'asio' feature)"
                    .to_string()
            } else {
                format!(
                    "backend {:?} does not advertise native DSD transport capabilities",
                    self.config.output_backend
                )
            };
            return Err(OutputError::StreamError(format!(
                "native DSD unavailable: {hint}"
            )));
        }

        // Capability candidates are typed by wire format, rate, and channel
        // constraints. The exact stream is still verified by set_native_dsd;
        // this prevents choosing DSD_U8 merely because it was first in a
        // legacy format list when a DSD128+ endpoint exposes another format.
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

        info!(
            "Native DSD: requesting {} at {} bps / {} ch from backend {:?} (device \"{}\")",
            wire_format.label(),
            bit_rate,
            channels,
            out_info
                .actual_backend
                .unwrap_or(self.config.output_backend),
            out_info.device_name,
        );

        let negotiated = output.set_native_dsd(Some(params))?;
        let format = negotiated.ok_or_else(|| {
            OutputError::StreamError("backend returned no native DSD format".to_string())
        })?;

        info!(
            "Native DSD: backend negotiated {} (requested {})",
            format.label(),
            wire_format.label()
        );

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
        path: &Path,
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
                error!(
                    "Critical: Resampler required ({} Hz -> {} Hz) for gapless handoff \
                     but could not be initialized!",
                    info.sample_rate, self.output_sample_rate
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
        self.graph.set_dop_bypass(false);

        self.clock.reset_track(info.sample_rate);
        self.analyzer.set_sample_rate(info.sample_rate);
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
        self.graph
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
}
