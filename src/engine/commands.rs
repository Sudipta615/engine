//! Command processing and dispatch for the audio engine.

use crossbeam::channel::TryRecvError;
use log::{error, info, warn};

use super::{helpers::percent_decode, AudioEngine, PlaybackStream};
use crate::buffer::{EngineCommand, PlaybackState};
use crate::dsp::pipeline::VolumePath;

impl AudioEngine {
    pub(super) fn process_commands(&mut self) {
        const MAX_COMMANDS_PER_TICK: usize = 64;
        let mut processed = 0usize;
        loop {
            if processed >= MAX_COMMANDS_PER_TICK {
                // Log at debug level to avoid spamming if a runaway sender
                // is flooding the queue. The next tick will continue
                // draining.
                log::debug!(
                    "process_commands: hit per-tick cap of {} commands; \
                     remaining commands will be processed next tick",
                    MAX_COMMANDS_PER_TICK
                );
                break;
            }
            match self.cmd_rx.try_recv() {
                Ok(cmd) => {
                    self.handle_command(cmd);
                    processed += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    warn!("Command channel disconnected");
                    break;
                }
            }
        }
    }

    /// The active output profile's backend preference, if any.
    pub(super) fn profile_backend(&self) -> Option<config::AudioBackend> {
        self.output_profile
            .as_ref()
            .and_then(|p| p.backend_preference)
    }

    /// (Re)select the output profile for the current device and apply it.
    ///
    /// The explicit `output_profile` wins; otherwise the built-in/user
    /// profile library is consulted by device name. Called after stream
    /// (re)creation so per-device profiles follow device changes. Never
    /// triggers a stream recovery itself (no recursion).
    pub(super) fn refresh_output_profile(&mut self) {
        let Some(device_name) = self.audio_output.as_ref().map(|o| o.device_name()) else {
            return;
        };
        // Match on the stable OS/backend device ID first (§10); name patterns
        // are the fallback for backends without a stable ID (cpal).
        let device_id = self.audio_output.as_ref().and_then(|o| o.device_id());
        let profile = self.output_profile.clone().or_else(|| {
            crate::output::OutputProfileLibrary::new()
                .select_for_device_with_id(&device_name, device_id.as_deref())
                .cloned()
        });
        match profile {
            Some(profile) => self.apply_output_profile(&profile),
            None => self.write_playback_info(|pb| pb.active_output_profile = None),
        }
    }

    /// Apply an output profile to the active stream and configuration.
    ///
    /// Applies the DSP bundle (EQ, crossfeed, stereo width, limiter) through
    /// the pipeline, then the transport preferences (volume mode, sample
    /// rate policy, DSD policy). The backend preference is *not* applied
    /// here — it is read at stream creation ([`Self::profile_backend`]) to
    /// avoid recovery recursion; a caller that needs it takes effect must
    /// trigger a recovery after setting the profile.
    pub(super) fn apply_output_profile(&mut self, profile: &crate::output::OutputProfile) {
        self.write_playback_info(|pb| pb.active_output_profile = Some(profile.id.clone()));
        let dsp = &profile.dsp;

        // ── DSP bundle ────────────────────────────────────────────────────
        // EQ: compile the profile's bands + preamp through the same preset
        // path as SetEqPreset (no new DSP stage).
        let preset = config::EqPreset {
            name: format!("profile:{}", profile.id),
            output_device_pattern: None,
            preamp_db: dsp.preamp_db,
            bands: dsp
                .eq_bands
                .iter()
                .map(|b| config::EqBandConfig {
                    enabled: b.enabled,
                    filter_type: config::FilterType::Peaking,
                    frequency: b.frequency,
                    gain_db: b.gain_db,
                    q: b.q,
                })
                .collect(),
        };
        if dsp.eq_enabled {
            self.pipeline.eq = crate::dsp::equalizer::ParametricEq::from_preset(
                self.output_sample_rate as f32,
                &preset,
            );
        } else {
            self.pipeline.set_eq_enabled(false);
        }
        self.pipeline.set_crossfeed_enabled(dsp.crossfeed_enabled);
        self.pipeline.set_stereo_width(dsp.stereo_width);
        self.pipeline
            .set_limiter_params(5.0, 0.5, 100.0, dsp.limiter_ceiling_db, false);
        self.pipeline.set_limiter_true_peak(dsp.true_peak_limiter);
        self.pipeline.set_limiter_enabled(true);

        // ── Volume mode ───────────────────────────────────────────────────
        if let Some(mode) = profile.volume_mode {
            if mode != self.config.volume_mode {
                self.config.volume_mode = mode;
                let has_hw = self
                    .audio_output
                    .as_ref()
                    .is_some_and(|o| o.supports_hardware_volume());
                match mode {
                    config::VolumeMode::HardwarePreferred if has_hw => {
                        self.pipeline.set_volume(1.0);
                    }
                    config::VolumeMode::HardwarePreferred => {
                        let message = "HardwarePreferred: endpoint volume unavailable for the active output; using software gain"
                            .to_string();
                        warn!("{}", message);
                        self.write_playback_info(|pb| pb.volume_error = Some(message.clone()));
                    }
                    config::VolumeMode::HardwareOnly if has_hw => {
                        self.pipeline.set_volume(1.0);
                    }
                    config::VolumeMode::HardwareOnly => {
                        let message = "HardwareOnly: endpoint volume unavailable for the active output; software volume NOT applied"
                            .to_string();
                        warn!("{}", message);
                        self.pipeline.set_volume(1.0);
                        self.write_playback_info(|pb| {
                            pb.volume_error = Some(message.clone());
                            pb.volume_path = None;
                        });
                    }
                    config::VolumeMode::SoftwareOnly | config::VolumeMode::SoftwareAllowed => {
                        let vol = self.playback_info.load().volume;
                        self.pipeline.set_volume(vol);
                    }
                }
            }
        }

        // ── Sample rate preference ────────────────────────────────────────
        if let Some(rate) = profile.sample_rate_preference {
            if self.config.sample_rate_policy != config::SampleRatePolicy::Fixed(rate) {
                self.config.sample_rate_policy = config::SampleRatePolicy::Fixed(rate);
                if let Some(ref mut output) = self.audio_output {
                    let caps = output.capabilities();
                    let target = caps.best_rate_for(
                        self.clock.source_sample_rate,
                        &config::SampleRatePolicy::Fixed(rate),
                    );
                    if let Ok(actual) = output.reconfigure_sample_rate(target) {
                        self.output_sample_rate = actual;
                        self.pipeline.update_sample_rate(actual as f32);
                    }
                }
            }
        }

        // ── DSD policy (takes effect on the next DSD track) ───────────────
        if let Some(policy) = profile.dsd_policy {
            self.config.dsd_output = policy;
        }

        // ── Resampler policy ──────────────────────────────────────────────
        if let Some(quality) = profile.resampler_policy {
            if self.config.resampler_quality != quality {
                self.config.resampler_quality = quality;
                #[cfg(feature = "resample")]
                match &mut self.stream {
                    Some(crate::engine::PlaybackStream::Single {
                        resampler: Some(ref mut r),
                        ..
                    }) => r.set_quality(quality),
                    Some(crate::engine::PlaybackStream::Transitioning {
                        outgoing_resampler,
                        incoming_resampler,
                        ..
                    }) => {
                        if let Some(r) = outgoing_resampler {
                            r.set_quality(quality);
                        }
                        if let Some(r) = incoming_resampler {
                            r.set_quality(quality);
                        }
                    }
                    _ => {}
                }
            }
        }

        // ── Dither policy ─────────────────────────────────────────────────
        if let Some(policy) = profile.dither_policy {
            let desired = match policy {
                config::DitherPolicy::FollowGlobal => self.config.dither_enabled,
                config::DitherPolicy::ForceOn => true,
                config::DitherPolicy::ForceOff => false,
            };
            if self.config.dither_enabled != desired {
                self.config.dither_enabled = desired;
                if let Some(ref output) = self.audio_output {
                    // Dither must stay off while DoP is active — it would
                    // corrupt the bitstream (same rule as SetDitherEnabled).
                    output.set_dither_enabled(desired && !self.dsd.dop_active);
                }
            }
        }

        // ── Safety ceiling (overrides the DSP bundle's ceiling) ──────────
        if let Some(ceiling) = profile.safety_ceiling_dbtp {
            self.pipeline
                .set_limiter_params(5.0, 0.5, 100.0, ceiling, false);
        }

        // ── Channel routing (§10 "channel routing", §34) ─────────────────
        // The profile's routing matrix becomes the active multichannel
        // routing matrix; `None` (the default profile) leaves the engine-wide
        // config untouched.
        if let Some(ref routing) = profile.channel_routing {
            self.pipeline.channel_trim.set_routing(routing);
        }

        let device = self
            .audio_output
            .as_ref()
            .map(|o| o.device_name())
            .unwrap_or_default();
        info!(
            "Output profile '{}' applied (device '{}', EQ {}, crossfeed {}, limiter ceiling {:.1} dB)",
            profile.id,
            device,
            if dsp.eq_enabled { "on" } else { "off" },
            if dsp.crossfeed_enabled { "on" } else { "off" },
            dsp.limiter_ceiling_db
        );
    }

    /// Compile the graphic EQ model into the pipeline's parametric stage.
    ///
    /// The model is the single source of truth for the active graphic
    /// layout: its layout / gains / preamp / enabled flag are mirrored into
    /// `config.graphic_eq` (so `set_config` round-trips), the pipeline EQ's
    /// band count is grown when the layout needs more bands than the
    /// pipeline was created with, and every band is written through
    /// `set_band` so the change is smoothed and click-free.
    pub(super) fn sync_graphic_eq(&mut self) {
        self.config.graphic_eq.layout = self.graphic_eq.layout().clone();
        self.config.graphic_eq.gains_db = self.graphic_eq.gains().to_vec();
        self.config.graphic_eq.preamp_db = self.graphic_eq.preamp_db();
        self.config.graphic_eq.enabled = self.graphic_eq.enabled();
        let n = self.graphic_eq.num_bands();
        if self.pipeline.eq_num_bands() < n {
            self.pipeline.eq =
                crate::dsp::equalizer::ParametricEq::new(n, self.output_sample_rate as f32);
        }
        self.graphic_eq.sync_into(&mut self.pipeline.eq);
    }

    fn handle_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::Play => {
                if self.stream.is_some() && !self.stream_ended {
                    if let Some(ref output) = self.audio_output {
                        output.resume();
                    }
                    self.update_playback_state(PlaybackState::Playing);
                    info!("Playback started");
                } else if self.stream_ended {
                    log::warn!(
                        "Play command ignored: stream has ended. Reload the track to play again."
                    );
                } else {
                    // No track has ever been loaded (self.stream is None and
                    // stream_ended is false, e.g. the very first Play press
                    // of a session before any OpenUri has been sent). This
                    // used to be a silent no-op: nothing played, but the UI
                    // layer had already flipped to a "playing" state,
                    // leaving the app and engine permanently out of sync.
                    // Fail loudly instead and make sure our own state
                    // reflects reality so the UI can be corrected.
                    log::warn!("Play command ignored: no track loaded");
                    self.update_playback_state(PlaybackState::Stopped);
                }
            }
            EngineCommand::Pause => {
                if self.stream.is_some() {
                    if let Some(ref output) = self.audio_output {
                        output.pause();
                    }
                    self.update_playback_state(PlaybackState::Paused);
                    info!("Playback paused");
                }
            }
            EngineCommand::Stop => {
                if let Some(ref output) = self.audio_output {
                    output.reset_buffer();
                } else {
                    self.output_buffer.reset();
                }
                self.scratch.pending_output_frames.clear();
                self.clock.set_source_frames(0);
                self.pipeline.reset();
                self.stream = None;
                self.stream_ended = false;
                self.scratch.crossfade_triggered = false;
                self.loudness_scan.next_track_path = None;
                self.scratch.cached_incoming_decoder = None;
                self.loudness_scan.current_track_path = None;
                self.loudness_scan.pending_loudness_metadata = None;
                self.loudness_scan.incoming_track_path = None;
                self.loudness_scan.pending_incoming_loudness_metadata = None;
                self.scratch.pending_chunk = None;
                self.scratch.pending_incoming_chunk = None;
                self.recovery.consecutive_decode_errors = 0;
                // The internal clock was reset above; publish the playhead too,
                // otherwise a UI reading playback_info() keeps showing the
                // pre-stop position after the state already flipped to Stopped.
                self.write_playback_info(|pb| pb.position_secs = 0.0);
                self.update_playback_state(PlaybackState::Stopped);
                info!("Playback stopped");
            }
            EngineCommand::Seek(pos_secs) => {
                if !pos_secs.is_finite() || pos_secs < 0.0 {
                    warn!("Seek ignored: invalid position {}", pos_secs);
                    return;
                }
                // Seek only works cleanly in Single mode. If crossfading,
                // cancel the crossfade and seek in the incoming track.
                let seek_in_incoming = self.stream.as_ref().is_some_and(|s| s.is_crossfading());
                if seek_in_incoming {
                    // Promote incoming to single, discard outgoing.
                    if let Some(PlaybackStream::Transitioning {
                        incoming_decoder,
                        incoming_resampler,
                        ..
                    }) = self.stream.take()
                    {
                        self.clock.source_sample_rate = incoming_decoder.info().sample_rate;
                        self.duration_secs = incoming_decoder.duration_secs();
                        self.scratch.crossfade_triggered = false;
                        self.recovery.consecutive_decode_errors = 0;
                        // The incoming track becomes current: promote its path
                        // and loudness metadata too.
                        self.loudness_scan.current_track_path = self.loudness_scan.incoming_track_path.take();
                        let incoming_meta = self.loudness_scan.pending_incoming_loudness_metadata.take();
                        self.loudness_scan.pending_loudness_metadata = incoming_meta;
                        if let Some(meta) = incoming_meta {
                            self.pipeline.apply_loudness_metadata_outgoing(Some(meta));
                        }
                        self.stream = Some(PlaybackStream::Single {
                            decoder: incoming_decoder,
                            resampler: incoming_resampler,
                        });
                        self.pipeline.mixer_mut().start_playing();
                    }
                }

                if let Some(PlaybackStream::Single {
                    ref mut decoder,
                    ref mut resampler,
                }) = self.stream
                {
                    let clamped_pos = if self.duration_secs > 0.0 {
                        pos_secs.min(self.duration_secs - 0.05).max(0.0)
                    } else {
                        // No duration known — still clamp to a sane upper
                        // bound (24h) to avoid passing absurd values to the
                        // decoder which might overflow internal time math.
                        pos_secs.min(86400.0)
                    };

                    self.scratch.pending_output_frames.clear();
                    if let Some(ref output) = self.audio_output {
                        output.reset_buffer();
                    } else {
                        self.output_buffer.reset();
                    }

                    self.pipeline.begin_seek_fadeout();

                    // Push a short fadeout ramp of silence through the
                    // pipeline so the limiter/filters settle to the new
                    // (silent) state before the seek position is decoded.
                    // With `pending_output_frames` cleared above, these
                    // 128 frames are the FIRST samples the user hears
                    // after the seek — they're silence with a fade-out
                    // envelope applied, which prevents a click.
                    //
                    // Skipped for DoP: plain 0.0 frames would break the
                    // 0x05/0xFA marker framing; the DAC re-locks at the
                    // new position instead.
                    if !self.dsd.dop_active {
                        for _ in 0..128 {
                            if self.scratch.pending_output_frames.len() >= super::MAX_PENDING_OUTPUT_FRAMES
                            {
                                break;
                            }
                            let (l, r) = self.pipeline.process(0.0, 0.0);
                            super::decode_loop::push_pending_back_bounded(
                                &mut self.scratch.pending_output_frames,
                                (l, r),
                            );
                        }
                    }
                    match decoder.seek(clamped_pos) {
                        Ok(()) => {
                            self.clock.set_source_frames(
                                (clamped_pos * self.clock.source_sample_rate as f32).round() as u64,
                            );
                            #[cfg(feature = "resample")]
                            if let Some(ref mut r) = resampler {
                                r.reset();
                            }
                            #[cfg(not(feature = "resample"))]
                            let _ = resampler;
                            self.pipeline.reset_filters_only();
                            self.pipeline.begin_seek_fadein();
                            // Reset crossfade trigger since position changed.
                            self.scratch.crossfade_triggered = false;
                            self.scratch.pending_chunk = None;
                            self.scratch.pending_incoming_chunk = None;
                            self.write_playback_info(|pb| pb.position_secs = clamped_pos);
                            info!("Seeked to {:.1}s", clamped_pos);
                        }
                        Err(e) => {
                            self.pipeline.begin_seek_fadein();
                            // The clock may have been switched to the incoming
                            // track's time base during the crossfade promotion
                            // above; reset it so the frame counter and rate stay
                            // coherent (playback continues from the current
                            // decoder position, reported as the track start).
                            self.clock.reset_track(self.clock.source_sample_rate);
                            self.write_playback_info(|pb| pb.position_secs = 0.0);
                            warn!("Seek failed: {}", e);
                        }
                    }
                }
            }
            EngineCommand::SetVolume(vol) => {
                if !vol.is_finite() {
                    warn!("SetVolume ignored: non-finite value {}", vol);
                    return;
                }
                let clamped = vol.clamp(0.0, 1.0);
                if self.pipeline.is_bit_perfect() && !self.volume_uses_hardware() {
                    let message = "Bit-Perfect mode: software volume is disabled; use hardware volume or disable Bit-Perfect mode".to_string();
                    warn!("{}", message);
                    self.write_playback_info(|pb| {
                        pb.volume_error = Some(message.clone());
                        pb.volume_path = None;
                    });
                    return;
                }
                if !self.volume_uses_hardware() {
                    // SoftwareOnly, or HardwarePreferred with an endpoint that
                    // lacks native volume control (fallback).
                    self.pipeline.set_volume(clamped);
                    self.write_playback_info(|pb| {
                        pb.volume = clamped;
                        pb.volume_error = None;
                        pb.volume_path = Some(VolumePath::Software);
                    });
                } else {
                    let db = if clamped <= 0.0001 {
                        -96.0
                    } else {
                        20.0 * clamped.log10()
                    };
                    match self
                        .audio_output
                        .as_ref()
                        .map(|o| o.set_hardware_volume_db(db))
                    {
                        // Applied: the hardware level changed, so publish the
                        // new value and keep the DSP path at unity gain.
                        Some(Ok(())) => {
                            self.pipeline.set_volume(1.0);
                            self.write_playback_info(|pb| {
                                pb.volume = clamped;
                                pb.volume_error = None;
                                pb.volume_path = Some(VolumePath::Hardware);
                            });
                        }
                        // Failed (e.g. no native hardware-volume backend on
                        // this platform): HardwarePreferred falls back to
                        // software gain; HardwareOnly keeps the signal
                        // untouched and reports the failure (never a silent
                        // software path — spec §5.1, §12).
                        Some(Err(e)) => {
                            error!("Hardware volume set failed: {}", e);
                            if self.config.volume_mode == config::VolumeMode::HardwareOnly {
                                self.pipeline.set_volume(1.0);
                                self.write_playback_info(|pb| {
                                    pb.volume = clamped;
                                    pb.volume_error = Some(format!(
                                        "HardwareOnly: endpoint volume set failed ({e}); \
                                         software volume NOT applied"
                                    ));
                                    pb.volume_path = None;
                                });
                            } else {
                                self.pipeline.set_volume(clamped);
                                self.write_playback_info(|pb| {
                                    pb.volume = clamped;
                                    pb.volume_error = Some(format!(
                                        "Hardware volume set failed ({}); using software gain",
                                        e
                                    ));
                                    pb.volume_path = Some(VolumePath::Software);
                                });
                            }
                        }
                        // No output stream open yet: nothing to set; keep the
                        // software pipeline at unity. HardwareOnly reports no
                        // hardware path (it cannot be verified until the
                        // stream opens); HardwarePreferred reports Hardware.
                        None => {
                            self.pipeline.set_volume(1.0);
                            self.write_playback_info(|pb| {
                                pb.volume = clamped;
                                pb.volume_error = None;
                                pb.volume_path = if self.config.volume_mode
                                    == config::VolumeMode::HardwareOnly
                                {
                                    None
                                } else {
                                    Some(VolumePath::Hardware)
                                };
                            });
                        }
                    }
                }
            }
            EngineCommand::SetVolumeDb(db) => {
                if !db.is_finite() {
                    warn!("SetVolumeDb ignored: non-finite value {}", db);
                    return;
                }
                let linear = if db <= -60.0 {
                    0.0
                } else {
                    10.0_f32.powf(db.clamp(-60.0, 0.0) / 20.0)
                };
                if self.pipeline.is_bit_perfect() && !self.volume_uses_hardware() {
                    let message = "Bit-Perfect mode: software volume is disabled; use hardware volume or disable Bit-Perfect mode".to_string();
                    warn!("{}", message);
                    self.write_playback_info(|pb| {
                        pb.volume_error = Some(message.clone());
                        pb.volume_path = None;
                    });
                    return;
                }
                if !self.volume_uses_hardware() {
                    self.pipeline.set_volume_db(db);
                    self.write_playback_info(|pb| {
                        pb.volume = linear;
                        pb.volume_error = None;
                        pb.volume_path = Some(VolumePath::Software);
                    });
                } else {
                    match self
                        .audio_output
                        .as_ref()
                        .map(|o| o.set_hardware_volume_db(db))
                    {
                        Some(Ok(())) => {
                            self.pipeline.set_volume(1.0);
                            self.write_playback_info(|pb| {
                                pb.volume = linear;
                                pb.volume_error = None;
                                pb.volume_path = Some(VolumePath::Hardware);
                            });
                        }
                        Some(Err(e)) => {
                            error!("Hardware volume set failed: {}", e);
                            if self.config.volume_mode == config::VolumeMode::HardwareOnly {
                                self.pipeline.set_volume(1.0);
                                self.write_playback_info(|pb| {
                                    pb.volume = linear;
                                    pb.volume_error = Some(format!(
                                        "HardwareOnly: endpoint volume set failed ({e}); \
                                         software volume NOT applied"
                                    ));
                                    pb.volume_path = None;
                                });
                            } else {
                                self.pipeline.set_volume_db(db);
                                self.write_playback_info(|pb| {
                                    pb.volume = linear;
                                    pb.volume_error = Some(format!(
                                        "Hardware volume set failed ({}); using software gain",
                                        e
                                    ));
                                    pb.volume_path = Some(VolumePath::Software);
                                });
                            }
                        }
                        None => {
                            self.pipeline.set_volume(1.0);
                            self.write_playback_info(|pb| {
                                pb.volume = linear;
                                pb.volume_error = None;
                                pb.volume_path = if self.config.volume_mode
                                    == config::VolumeMode::HardwareOnly
                                {
                                    None
                                } else {
                                    Some(VolumePath::Hardware)
                                };
                            });
                        }
                    }
                }
            }
            EngineCommand::SetSpeed(speed) => {
                if !speed.is_finite() {
                    warn!("SetSpeed ignored: non-finite value {}", speed);
                    return;
                }
                let clamped = speed.clamp(0.25, 4.0);
                if self.dsd.dop_active {
                    // DoP bitstreams are always played at 1.0× — changing the
                    // rate would desync the marker framing / resample the data.
                    warn!("SetSpeed ignored while DSD DoP is active (DoP is fixed at 1.0×)");
                    return;
                }
                self.speed = clamped;

                match self.config.speed_mode {
                    config::SpeedMode::TimeStretch => {
                        self.pipeline.timestretcher_mut().set_speed(clamped);
                        // In time-stretch mode, resampler speed stays at 1.0 (pitch-invariant)
                        #[cfg(feature = "resample")]
                        match &mut self.stream {
                            Some(PlaybackStream::Single {
                                resampler: Some(ref mut r),
                                ..
                            }) => {
                                r.set_speed(1.0);
                            }
                            Some(PlaybackStream::Transitioning {
                                outgoing_resampler,
                                incoming_resampler,
                                ..
                            }) => {
                                if let Some(ref mut r) = outgoing_resampler {
                                    r.set_speed(1.0);
                                }
                                if let Some(ref mut r) = incoming_resampler {
                                    r.set_speed(1.0);
                                }
                            }
                            _ => {}
                        }
                    }
                    config::SpeedMode::PitchShift => {
                        self.pipeline.timestretcher_mut().set_pitch_ratio(clamped);
                        // Pitch shift uses timestretcher without changing resampler playback speed
                        #[cfg(feature = "resample")]
                        match &mut self.stream {
                            Some(PlaybackStream::Single {
                                resampler: Some(ref mut r),
                                ..
                            }) => {
                                r.set_speed(1.0);
                            }
                            Some(PlaybackStream::Transitioning {
                                outgoing_resampler,
                                incoming_resampler,
                                ..
                            }) => {
                                if let Some(ref mut r) = outgoing_resampler {
                                    r.set_speed(1.0);
                                }
                                if let Some(ref mut r) = incoming_resampler {
                                    r.set_speed(1.0);
                                }
                            }
                            _ => {}
                        }
                    }
                    config::SpeedMode::Varispeed => {
                        self.pipeline.timestretcher_mut().set_speed(1.0);
                        // Update resampler(s) in the active stream.
                        #[cfg(feature = "resample")]
                        match &mut self.stream {
                            Some(PlaybackStream::Single {
                                resampler: Some(ref mut r),
                                ..
                            }) => {
                                r.set_speed(clamped);
                            }
                            Some(PlaybackStream::Single { .. }) => {}
                            Some(PlaybackStream::Transitioning {
                                outgoing_resampler,
                                incoming_resampler,
                                ..
                            }) => {
                                if let Some(ref mut r) = outgoing_resampler {
                                    r.set_speed(clamped);
                                }
                                if let Some(ref mut r) = incoming_resampler {
                                    r.set_speed(clamped);
                                }
                            }
                            None => {}
                        }
                    }
                }

                self.write_playback_info(|pb| pb.speed = clamped);
                info!(
                    "Playback speed set to {:.2}x ({:?})",
                    clamped, self.config.speed_mode
                );
            }
            EngineCommand::NextTrack => {
                log::debug!("NextTrack: handled by PlaybackService, not engine");
            }
            EngineCommand::PrevTrack => {
                log::debug!("PrevTrack: handled by PlaybackService, not engine");
            }
            EngineCommand::LoadTrack(_id) => {
                log::debug!("LoadTrack by ID: use load_track() directly on AudioEngine");
            }
            EngineCommand::Shutdown => {
                self.stop();
            }
            EngineCommand::SetOutputBackend(backend) => {
                if self.config.output_backend != backend {
                    self.config.output_backend = backend;
                    info!("Output backend set to {:?}, recovering stream...", backend);
                    if let Err(e) = self.recover_output_stream() {
                        error!("Failed to recover stream after backend change: {}", e);
                    }
                }
            }
            EngineCommand::SetOutputDevice(device) => {
                if self.config.output_device != device {
                    self.config.output_device = device.clone();
                    info!("Output device set to {:?}, recovering stream...", device);
                    if let Err(e) = self.recover_output_stream() {
                        error!("Failed to recover stream after device change: {}", e);
                    }
                }
            }

            EngineCommand::SetEqEnabled(enabled) => {
                self.pipeline.set_eq_enabled(enabled);
            }
            EngineCommand::SetEqAutoHeadroom(enabled) => {
                self.config.eq.auto_headroom = enabled;
                self.pipeline.set_eq_auto_headroom(enabled);
                info!(
                    "EQ auto headroom: {}",
                    if enabled { "enabled" } else { "disabled" }
                );
            }
            EngineCommand::SetEqBand {
                index,
                frequency,
                gain_db,
                q,
                enabled,
            } => {
                use crate::dsp::equalizer::{EqBandParams, EqFilterType};
                // Graphic EQ bands defaults (first and last are shelves)
                let num_bands = self.pipeline.eq_num_bands();
                let filter_type = if index == 0 {
                    EqFilterType::LowShelf
                } else if num_bands > 1 && index == num_bands - 1 {
                    EqFilterType::HighShelf
                } else {
                    EqFilterType::Peaking
                };
                self.pipeline.set_eq_band(
                    index,
                    EqBandParams {
                        frequency,
                        gain_db,
                        q,
                        filter_type,
                        enabled,
                    },
                );
            }
            EngineCommand::SetEqBandParams {
                index,
                frequency,
                gain_db,
                q,
                filter_type,
                enabled,
            } => {
                use crate::dsp::equalizer::EqBandParams;
                self.pipeline.set_eq_band(
                    index,
                    EqBandParams {
                        frequency,
                        gain_db,
                        q,
                        filter_type,
                        enabled,
                    },
                );
            }
            EngineCommand::SetEqPreset(preset) => {
                // Apply a complete EQ preset (e.g. an AutoEQ result): replace
                // the pipeline's bands + preamp and enable the EQ. Rebuilding
                // from the preset resets filter state — acceptable for a
                // deliberate curve replacement.
                use crate::dsp::equalizer::ParametricEq;
                self.pipeline.eq =
                    ParametricEq::from_preset(self.output_sample_rate as f32, &preset);
                info!(
                    "EQ preset '{}' applied ({} bands, preamp {:.1} dB)",
                    preset.name,
                    preset.bands.len(),
                    preset.preamp_db
                );
            }
            EngineCommand::SetGraphicEqLayout(layout) => {
                self.graphic_eq.set_layout(layout);
                self.graphic_eq.set_enabled(true);
                self.sync_graphic_eq();
                info!(
                    "Graphic EQ: layout {:?} activated ({} bands)",
                    self.graphic_eq.layout(),
                    self.graphic_eq.num_bands()
                );
            }
            EngineCommand::SetGraphicEqSlider { band, gain_db } => {
                self.graphic_eq.set_slider(band, gain_db);
                self.graphic_eq.set_enabled(true);
                self.sync_graphic_eq();
            }
            EngineCommand::SetGraphicEqPreamp(db) => {
                self.graphic_eq.set_preamp_db(db);
                self.sync_graphic_eq();
            }
            EngineCommand::SetGraphicEqEnabled(enabled) => {
                self.graphic_eq.set_enabled(enabled);
                self.sync_graphic_eq();
                info!(
                    "Graphic EQ {}",
                    if enabled { "enabled" } else { "disabled" }
                );
            }
            EngineCommand::SetOutputProfile(profile) => {
                let backend_changed = profile
                    .backend_preference
                    .is_some_and(|b| Some(b) != self.profile_backend());
                self.output_profile = Some(profile.clone());
                self.config.output_backend = profile
                    .backend_preference
                    .unwrap_or(self.config.output_backend);
                if self.audio_output.is_some() && backend_changed {
                    // Recreate the stream so the preferred backend is used.
                    info!(
                        "Output profile '{}': backend preference changed, recovering stream",
                        profile.id
                    );
                    if let Err(e) = self.recover_output_stream() {
                        error!("Stream recovery after profile change failed: {}", e);
                    }
                } else {
                    self.apply_output_profile(&profile);
                }
                info!("Output profile '{}' selected", profile.id);
            }
            EngineCommand::ClearOutputProfile => {
                self.output_profile = None;
                self.write_playback_info(|pb| pb.active_output_profile = None);
                info!("Output profile cleared");
            }
            EngineCommand::SetResamplerQuality(quality) => {
                self.config.resampler_quality = quality;
                #[cfg(feature = "resample")]
                match &mut self.stream {
                    Some(crate::engine::PlaybackStream::Single {
                        resampler: Some(ref mut r),
                        ..
                    }) => {
                        r.set_quality(quality);
                    }
                    Some(crate::engine::PlaybackStream::Single { .. }) => {}
                    Some(crate::engine::PlaybackStream::Transitioning {
                        outgoing_resampler,
                        incoming_resampler,
                        ..
                    }) => {
                        if let Some(ref mut r) = outgoing_resampler {
                            r.set_quality(quality);
                        }
                        if let Some(ref mut r) = incoming_resampler {
                            r.set_quality(quality);
                        }
                    }
                    None => {}
                }
                info!("Resampler quality set to {:?}", quality);
            }
            EngineCommand::SetBassShelf(gain_db) => {
                self.pipeline.set_bass_shelf(gain_db);
            }
            EngineCommand::SetTrebleShelf(gain_db) => {
                self.pipeline.set_treble_shelf(gain_db);
            }
            EngineCommand::SetPreamp(db) => {
                self.pipeline.set_preamp_db(db);
            }
            EngineCommand::SetStereoWidth(width) => {
                self.pipeline.set_stereo_width(width);
            }
            EngineCommand::SetBalance(balance) => {
                self.pipeline.set_balance(balance);
            }
            EngineCommand::SetDitherEnabled(enabled) => {
                // Forward to the output backend so the i16/u16 audio callbacks
                // apply (or skip) TPDF dither at the integer-quantization
                // boundary. The pipeline's dither stage is now a configuration
                // hint only; the actual quantization-time dither happens in
                // the cpal callback (see audio_callback_i16 / _u16).
                if let Some(ref output) = self.audio_output {
                    // Dither must stay OFF while DoP is active — it would
                    // corrupt the 24-bit DoP words at the i32 conversion.
                    output.set_dither_enabled(enabled && !self.dsd.dop_active);
                }
                // Persist the setting so that future stream recoveries pick
                // it up. The CpalOutput's dither flag is per-stream; a new
                // stream created during recovery defaults to enabled=true,
                // so we re-apply the user's preference here.
                self.config.dither_enabled = enabled;
            }
            EngineCommand::SetMidsideEq(enabled) => {
                // Toggle M/S processing. Resetting the EQ filters on toggle
                // avoids discontinuities: when M/S mode changes, the biquad
                // state (which holds "previous sample" context) is
                // reinterpreted from L/R domain to M/S domain, which would
                // otherwise produce a transient pop.
                let was_enabled = self.pipeline.is_midside_eq();
                if was_enabled != enabled {
                    self.pipeline.set_midside_eq(enabled);
                    self.pipeline.eq.reset();
                }
            }
            EngineCommand::SetCrossfeedEnabled(enabled) => {
                self.pipeline.set_crossfeed_enabled(enabled);
            }
            EngineCommand::SetCrossfeedProfile(profile) => {
                self.pipeline.set_crossfeed_profile(profile);
            }
            EngineCommand::SetCrossfeedCustomParams {
                frequency_hz,
                q,
                delay_ms,
            } => {
                self.pipeline
                    .set_crossfeed_custom_params(frequency_hz, q, delay_ms);
            }
            EngineCommand::SetCompressorEnabled(enabled) => {
                self.pipeline.set_compressor_enabled(enabled);
            }
            EngineCommand::SetCompressorBandParams {
                band,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_gain_db,
            } => {
                self.pipeline.set_compressor_band_params(
                    band,
                    threshold_db,
                    ratio,
                    attack_ms,
                    release_ms,
                    makeup_gain_db,
                );
            }

            EngineCommand::SetShuffle(_enabled) => {
                info!("Shuffle state change requested via MPRIS (handled by playback layer)");
            }
            EngineCommand::SetLoopStatus(status) => {
                info!(
                    "Loop status set to '{}' via MPRIS (handled by playback layer)",
                    status
                );
            }
            EngineCommand::OpenUri(uri) => {
                // Accept both file:// URIs (MPRIS) and plain filesystem paths
                // (sent by the UI layer). Previously only file:// URIs were
                // accepted, so every track selected in the UI was silently
                // rejected and no audio was ever produced.
                let path_opt = if let Some(stripped) = uri.strip_prefix("file://") {
                    percent_decode(stripped).map(std::path::PathBuf::from)
                } else {
                    Some(std::path::PathBuf::from(uri.clone()))
                };

                let path = match path_opt {
                    Some(p) => p,
                    None => {
                        warn!("OpenUri: failed to percent-decode URI: {}", uri);
                        return;
                    }
                };

                match std::fs::metadata(&path) {
                    Ok(metadata) if metadata.is_file() => {}
                    Ok(_) => {
                        warn!("OpenUri: path is not a regular file: {}", path.display());
                        self.update_playback_state(PlaybackState::Stopped);
                        return;
                    }
                    Err(_) => {
                        warn!("OpenUri: cannot access path: {}", path.display());
                        self.update_playback_state(PlaybackState::Stopped);
                        return;
                    }
                }
                let load_path = match path.canonicalize() {
                    Ok(canonical) => canonical,
                    Err(e) => {
                        log::debug!(
                            "OpenUri: canonicalize failed for {} ({}); using original path",
                            path.display(),
                            e
                        );
                        path.clone()
                    }
                };
                match self.load_track(&load_path) {
                    Ok(info) => {
                        info!(
                            "Loaded URI: {} Hz, {} ch, {:.1}s",
                            info.sample_rate, info.channels, info.duration_secs
                        );
                        self.update_playback_state(PlaybackState::Playing);
                        self.write_playback_info(|pb| {
                            pb.track_id = self.current_track_id;
                        });
                    }
                    Err(e) => {
                        warn!("Failed to load URI '{}': {}", uri, e);
                        self.update_playback_state(PlaybackState::Stopped);
                    }
                }
            }
            EngineCommand::PrepareNextTrack(path) => match self.prepare_next_track(&path) {
                Ok(info) => {
                    info!(
                        "Prepared next track for crossfade: {} Hz, {:.1}s",
                        info.sample_rate, info.duration_secs
                    );
                }
                Err(e) => {
                    warn!("Failed to prepare next track: {}", e);
                }
            },
            EngineCommand::RecoverStream => match self.recover_output_stream() {
                Ok(()) => info!("Stream recovered via command"),
                Err(e) => error!("Stream recovery failed: {}", e),
            },
            EngineCommand::AutoRecoverStream => {
                if self.config.output_backend == config::AudioBackend::Auto {
                    // Do not interrupt an active, healthy playback stream for background
                    // monitor polling triggers unless an actual CPAL stream error occurred!
                    if self.current_state() == PlaybackState::Playing {
                        if let Some(ref output) = self.audio_output {
                            let errors = output.take_stream_errors();
                            if errors.is_empty() {
                                log::debug!(
                                    "AutoRecoverStream ignored: active audio stream is healthy"
                                );
                                return;
                            }
                            for event in &errors.events {
                                warn!(
                                    "Auto recovery saw stream error [{}::{:?}]: {} ({})",
                                    event.error_type, event.kind, event.message, event.details
                                );
                            }
                            if errors.dropped > 0 {
                                warn!(
                                    "Auto recovery lost {} additional stream error event(s) to queue overflow",
                                    errors.dropped
                                );
                            }
                        }
                    }
                    match self.recover_output_stream() {
                        Ok(()) => info!("Stream recovered via auto-detection"),
                        Err(e) => error!("Auto stream recovery failed: {}", e),
                    }
                }
            }
            EngineCommand::LoudnessScanComplete { path, result } => {
                self.loudness_scan.loudness_scan_in_flight = false;

                // The incoming track's path is either still queued in
                // `next_track_path` or already fading in (`incoming_track_path`).
                let incoming_path = self
                    .loudness_scan
                    .incoming_track_path
                    .clone()
                    .or_else(|| self.loudness_scan.next_track_path.clone());

                if self.loudness_scan.current_track_path.as_deref() == Some(path.as_path()) {
                    let merged = self.loudness_scan.pending_loudness_metadata.as_mut().map(|meta| {
                        merge_scan_result(meta, result);
                        *meta
                    });
                    if let Some(meta) = merged {
                        self.pipeline.apply_loudness_metadata_outgoing(Some(meta));
                        info!(
                            "Loudness scan complete for {}: {:?} LUFS, {:?} dBTP",
                            path.display(),
                            meta.ebu_r128_loudness,
                            meta.ebu_r128_peak
                        );
                    }
                } else if incoming_path.as_deref() == Some(path.as_path()) {
                    let merged = self
                        .loudness_scan
                        .pending_incoming_loudness_metadata
                        .as_mut()
                        .map(|meta| {
                            merge_scan_result(meta, result);
                            *meta
                        });
                    if let Some(meta) = merged {
                        self.pipeline.apply_loudness_metadata_incoming(Some(meta));
                        info!(
                            "Loudness scan complete for incoming {}: {:?} LUFS, {:?} dBTP",
                            path.display(),
                            meta.ebu_r128_loudness,
                            meta.ebu_r128_peak
                        );
                    }
                } else {
                    log::debug!(
                        "Loudness scan result discarded for superseded track {}",
                        path.display()
                    );
                }

                // Re-arm scans for anything that still lacks EBU R128 metadata
                // (current track first, then the incoming track).
                self.start_loudness_scan();
                self.start_incoming_loudness_scan();
            }

            // ── New Poweramp-class commands ──────────────────────────────────
            EngineCommand::SetPrecisionMode(mode) => {
                info!("DSP precision mode set to {:?}", mode);
                self.pipeline.set_precision_mode(mode);
            }

            EngineCommand::SetBitPerfect(enabled) => {
                info!("Bit-perfect mode: {}", if enabled { "on" } else { "off" });
                self.pipeline.set_bit_perfect(enabled);
                if enabled {
                    // A bit-perfect request is fail-closed: software volume
                    // and seek fades are forced to unity/bypass. Hardware
                    // endpoint volume remains available when the backend
                    // actually supports it.
                    self.pipeline.set_volume(1.0);
                    self.pipeline.seek_fade.reset();
                    let uses_hardware = self.volume_uses_hardware();
                    self.write_playback_info(|pb| {
                        pb.volume_path = if uses_hardware {
                            Some(VolumePath::Hardware)
                        } else {
                            None
                        };
                        pb.volume_error = if uses_hardware {
                            None
                        } else {
                            Some("Bit-Perfect mode: software volume disabled; hardware volume unavailable".to_string())
                        };
                        pb.bit_perfect = true;
                    });
                } else {
                    self.write_playback_info(|pb| {
                        pb.bit_perfect = false;
                        pb.volume_path = None;
                        pb.volume_error = None;
                    });
                }
            }

            EngineCommand::SetLimiterMode(mode) => {
                info!("Limiter mode set to {:?}", mode);
                self.pipeline.set_limiter_mode(mode);
            }

            EngineCommand::SetLimiterTruePeak(enabled) => {
                info!(
                    "Limiter true-peak FIR: {}",
                    if enabled { "enabled" } else { "disabled" }
                );
                self.pipeline.set_limiter_true_peak(enabled);
            }

            EngineCommand::SetSampleRatePolicy(policy) => {
                info!("Sample rate policy set to: {}", policy.display_name());
                self.config.sample_rate_policy = policy.clone();
                if let Some(ref mut output) = self.audio_output {
                    let caps = output.capabilities();
                    let target = caps.best_rate_for(self.clock.source_sample_rate, &policy);
                    if let Ok(actual) = output.reconfigure_sample_rate(target) {
                        self.output_sample_rate = actual;
                        self.pipeline.update_sample_rate(actual as f32);
                    }
                }
            }

            EngineCommand::SetVolumeMode(mode) => {
                info!("Volume mode set to {:?}", mode);
                self.config.volume_mode = mode;
                let has_hw = self
                    .audio_output
                    .as_ref()
                    .is_some_and(|o| o.supports_hardware_volume());
                match mode {
                    config::VolumeMode::HardwarePreferred | config::VolumeMode::HardwareOnly
                        if has_hw =>
                    {
                        // Set software DSP volume to unity (0 dB) so the
                        // endpoint controls output level without
                        // double-attenuation or precision loss.
                        self.pipeline.set_volume(1.0);
                        info!("{mode:?} active: software pipeline set to unity gain (1.0).");
                        self.write_playback_info(|pb| {
                            pb.volume_path = Some(VolumePath::Hardware);
                            pb.volume_error = None;
                        });
                    }
                    config::VolumeMode::HardwarePreferred => {
                        // Preferred mode with no endpoint volume support:
                        // accept the mode and fall back to software gain
                        // (never reject).
                        let message = "HardwarePreferred: endpoint volume unavailable for the active output; using software gain"
                            .to_string();
                        warn!("{}", message);
                        let vol = self.playback_info.load().volume;
                        self.pipeline.set_volume(vol);
                        self.write_playback_info(|pb| {
                            pb.volume_error = Some(message.clone());
                            pb.volume_path = Some(VolumePath::Software);
                        });
                    }
                    config::VolumeMode::HardwareOnly => {
                        // Strict mode with no endpoint volume support: never
                        // introduce software volume into the signal path
                        // (spec §5.1, §12). Surface the failure instead.
                        let message = "HardwareOnly: endpoint volume unavailable for the active output; software volume NOT applied"
                            .to_string();
                        warn!("{}", message);
                        self.pipeline.set_volume(1.0);
                        self.write_playback_info(|pb| {
                            pb.volume_error = Some(message.clone());
                            pb.volume_path = None;
                        });
                    }
                    config::VolumeMode::SoftwareOnly | config::VolumeMode::SoftwareAllowed => {
                        let vol = self.playback_info.load().volume;
                        self.pipeline.set_volume(vol);
                        self.write_playback_info(|pb| {
                            pb.volume_path = Some(VolumePath::Software);
                            pb.volume_error = None;
                        });
                    }
                }
            }

            EngineCommand::SetFallbackPolicy(policy) => {
                info!("Fallback policy set to {:?}", policy);
                self.config.fallback_policy = policy;
            }

            EngineCommand::SetCrossfadeConfig(cfg) => {
                info!(
                    "Crossfade config updated: enabled={}, duration={}ms, curve={:?}",
                    cfg.enabled, cfg.duration_ms, cfg.curve
                );
                self.config.crossfade = cfg.clone();
                self.pipeline.mixer.set_curve(cfg.curve.into());
                self.pipeline
                    .mixer
                    .set_duration_ms(cfg.duration_ms, self.output_sample_rate as f32);
                self.pipeline.mixer.set_enabled(cfg.enabled);
            }

            EngineCommand::SetCrossfadeCurve(curve) => {
                info!("Crossfade curve set to {:?}", curve);
                self.config.crossfade.curve = curve;
                self.pipeline.mixer.set_curve(curve.into());
            }

            EngineCommand::SetTransitionMode(mode) => {
                info!("Transition mode set to {:?}", mode);
                self.config.transition_mode = mode;
            }

            EngineCommand::SetSpeedMode(mode) => {
                info!("Speed mode set to {:?}", mode);
                self.config.speed_mode = mode;
                // Re-apply current speed with the new mode
                let current_speed = self.speed;
                let _ = self.handle_command(EngineCommand::SetSpeed(current_speed));
            }

            EngineCommand::SetPitch(semitones) => {
                if !semitones.is_finite() {
                    warn!("SetPitch ignored: non-finite value {}", semitones);
                    return;
                }
                let clamped = semitones.clamp(-24.0, 24.0);
                info!("Pitch shift set to {:.2} semitones", clamped);
                self.pipeline
                    .timestretcher_mut()
                    .set_pitch_semitones(clamped);
            }

            EngineCommand::SetChannelMix(cfg) => {
                info!(
                    "Channel mix config updated: enabled={}, template={:?}",
                    cfg.enabled, cfg.template
                );
                self.config.channel_mix = cfg;
            }

            EngineCommand::SetChannelPolicy(policy) => {
                info!("Channel policy set to {:?}", policy);
                self.config.channel_policy = policy;
            }

            EngineCommand::SetChannelTrim(cfg) => {
                info!(
                    "Channel trim config updated: enabled={}, entries={}",
                    cfg.enabled,
                    cfg.entries.len()
                );
                self.config.channel_trim = cfg.clone();
                let sr = self.pipeline.sample_rate();
                self.pipeline.channel_trim.set_config(&cfg, sr);
            }

            EngineCommand::SetChannelRouting(cfg) => {
                info!("Channel routing config updated: enabled={}", cfg.enabled);
                self.config.channel_routing = cfg.clone();
                self.pipeline.channel_trim.set_routing(&cfg);
            }

            EngineCommand::SetChannelEq(cfg) => {
                info!(
                    "Channel EQ config updated: enabled={}, entries={}",
                    cfg.enabled,
                    cfg.entries.len()
                );
                self.config.channel_eq = cfg.clone();
                let sr = self.pipeline.sample_rate();
                self.pipeline.channel_trim.set_channel_eq(&cfg, sr);
            }

            EngineCommand::SetLfeConfig(cfg) => {
                info!(
                    "LFE config updated: enabled={}, gain_db={:.1}, crossover={:?}",
                    cfg.enabled, cfg.gain_db, cfg.crossover_hz
                );
                self.config.lfe = cfg.clone();
                let mut lfe = cfg;
                if self.config.bass_management.enabled
                    && lfe.crossover_hz.is_none()
                    && lfe.enabled
                {
                    lfe.crossover_hz = Some(self.config.bass_management.crossover_hz);
                }
                self.pipeline.channel_trim.set_lfe(&lfe);
            }

            EngineCommand::SetBassManagement(cfg) => {
                info!(
                    "Bass management updated: enabled={}, crossover={}Hz",
                    cfg.enabled, cfg.crossover_hz
                );
                self.config.bass_management = cfg.clone();
                let sr = self.pipeline.sample_rate();
                self.pipeline.channel_trim.set_bass_management(&cfg, sr);
                let mut lfe = self.config.lfe.clone();
                if cfg.enabled && lfe.crossover_hz.is_none() && lfe.enabled {
                    lfe.crossover_hz = Some(cfg.crossover_hz);
                }
                self.pipeline.channel_trim.set_lfe(&lfe);
            }
        }
    }
}

/// Merge a background loudness scan into tag-derived metadata. Tag values
/// always win; the scan only fills fields the tags left empty.
fn merge_scan_result(
    meta: &mut crate::dsp::loudness::LoudnessMetadata,
    result: Option<crate::decode::LoudnessScanResult>,
) {
    if let Some(r) = result {
        if meta.ebu_r128_loudness.is_none() {
            meta.ebu_r128_loudness = r.ebu_r128_loudness;
        }
        if meta.ebu_r128_peak.is_none() {
            meta.ebu_r128_peak = r.ebu_r128_peak_dbtp;
        }
        if meta.replaygain_track_db.is_none() {
            meta.replaygain_track_db = r
                .replaygain_track_db
                .or_else(|| r.ebu_r128_loudness.map(|lufs| -18.0 - lufs));
        }
        if meta.replaygain_track_peak.is_none() {
            meta.replaygain_track_peak = r
                .replaygain_track_peak
                .or_else(|| r.ebu_r128_peak_dbtp.map(|dbtp| 10.0_f32.powf(dbtp / 20.0)));
        }
    }
}
