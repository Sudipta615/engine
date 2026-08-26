//! Command processing and dispatch for the audio engine.
//!
//! The central `handle_command` method dispatches to handler methods defined
//! in the sibling files. Handlers are organized by concern:
//!
//! - [`playback`] — play/pause/stop/seek/speed/pitch/shutdown
//! - [`eq`] — EQ, graphic EQ, bass/treble shelves, preamp, mid-side
//! - [`dsp`] — stereo width, balance, dither, crossfeed, compressor, limiter,
//!   bit-perfect, precision, crossfade, resampler quality
//! - [`output`] — backend, device, sample rate policy, volume mode, fallback,
//!   output profiles, hardware/software volume, ASIO control panel
//! - [`multichannel`] — channel mix, policy, trim, routing, channel EQ,
//!   LFE, bass management
//! - [`lifecycle`] — Open, PrepareNext, RecoverStream, AutoRecoverStream,
//!   LoudnessScanComplete; also profile/device helpers

mod capture;
mod dsp;
mod eq;
mod lifecycle;
mod multichannel;
mod output;
mod playback;
mod playlist;

use crossbeam::channel::TryRecvError;
use log::warn;

use super::AudioEngine;
use crate::buffer::EngineCommand;

/// Merge a background loudness scan into tag-derived metadata. Tag values
/// always win; the scan only fills fields the tags left empty.
fn merge_scan_result(
    meta: &mut crate::dsp::LoudnessMetadata,
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

impl AudioEngine {
    pub(super) fn process_commands(&mut self) {
        const MAX_COMMANDS_PER_TICK: usize = 64;
        let mut processed = 0usize;
        loop {
            if processed >= MAX_COMMANDS_PER_TICK {
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

    // ── Shared profile and device helpers ──────────────────────────────────
    // These are needed by mod.rs (start) and recovery.rs, so they must be
    // pub(super) from *this* module (where super = engine).

    /// The active output profile's backend preference, if any.
    pub(super) fn profile_backend(&self) -> Option<config::AudioBackend> {
        self.output_profile
            .as_ref()
            .and_then(|p| p.backend_preference)
    }

    /// (Re)select the output profile for the current device and apply it.
    pub(super) fn refresh_output_profile(&mut self) {
        let Some(device_name) = self.audio_output.as_ref().map(|o| o.device_name()) else {
            return;
        };
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
    pub(super) fn apply_output_profile(&mut self, profile: &crate::output::OutputProfile) {
        use log::{info, warn};

        self.write_playback_info(|pb| pb.active_output_profile = Some(profile.id.clone()));
        let dsp = &profile.dsp;

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
            self.graph.eq_mut().eq = crate::dsp::equalizer::ParametricEq::from_preset(
                self.output_sample_rate as f32,
                &preset,
            );
        } else {
            self.graph.set_eq_enabled(false);
        }
        self.graph.set_crossfeed_enabled(dsp.crossfeed_enabled);
        self.graph.set_stereo_width(dsp.stereo_width);
        self.graph
            .set_limiter_params(5.0, 0.5, 100.0, dsp.limiter_ceiling_db, false);
        self.graph.set_limiter_true_peak(dsp.true_peak_limiter);
        self.graph.set_limiter_enabled(true);

        if let Some(mode) = profile.volume_mode {
            if mode != self.config.volume_mode {
                self.config.volume_mode = mode;
                let has_hw = self
                    .audio_output
                    .as_ref()
                    .is_some_and(|o| o.supports_hardware_volume());
                match mode {
                    config::VolumeMode::HardwarePreferred if has_hw => {
                        self.graph.set_volume(1.0);
                    }
                    config::VolumeMode::HardwarePreferred => {
                        let message = "HardwarePreferred: endpoint volume unavailable for the active output; using software gain"
                            .to_string();
                        warn!("{message}");
                        self.write_playback_info(|pb| pb.volume_error = Some(message.clone()));
                    }
                    config::VolumeMode::HardwareOnly if has_hw => {
                        self.graph.set_volume(1.0);
                    }
                    config::VolumeMode::HardwareOnly => {
                        let message = "HardwareOnly: endpoint volume unavailable for the active output; software volume NOT applied"
                            .to_string();
                        warn!("{message}");
                        self.graph.set_volume(1.0);
                        self.write_playback_info(|pb| {
                            pb.volume_error = Some(message.clone());
                            pb.volume_path = None;
                        });
                    }
                    config::VolumeMode::SoftwareOnly | config::VolumeMode::SoftwareAllowed => {
                        let vol = self.playback_info.load().volume;
                        self.graph.set_volume(vol);
                    }
                }
            }
        }

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
                        self.graph.update_sample_rate(actual as f32);
                    }
                }
            }
        }

        if let Some(policy) = profile.dsd_policy {
            self.config.dsd_output = policy;
        }

        if let Some(quality) = profile.resampler_policy {
            if self.config.resampler_quality != quality {
                self.config.resampler_quality = quality;
                #[cfg(feature = "resample")]
                self.resampler_set_quality_all(quality);
            }
        }

        if let Some(policy) = profile.dither_policy {
            let desired = match policy {
                config::DitherPolicy::FollowGlobal => self.config.dither_enabled,
                config::DitherPolicy::ForceOn => true,
                config::DitherPolicy::ForceOff => false,
            };
            if self.config.dither_enabled != desired {
                self.config.dither_enabled = desired;
                if let Some(ref output) = self.audio_output {
                    output.set_dither_enabled(desired && !self.dsd.dop_active);
                }
            }
        }

        if let Some(ceiling) = profile.safety_ceiling_dbtp {
            self.graph
                .set_limiter_params(5.0, 0.5, 100.0, ceiling, false);
        }

        if let Some(ref routing) = profile.channel_routing {
            self.graph.routing_mut().trimmer.set_routing(routing);
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
    pub(super) fn sync_graphic_eq(&mut self) {
        self.config.graphic_eq.layout = self.graphic_eq.layout().clone();
        self.config.graphic_eq.gains_db = self.graphic_eq.gains().to_vec();
        self.config.graphic_eq.preamp_db = self.graphic_eq.preamp_db();
        self.config.graphic_eq.enabled = self.graphic_eq.enabled();
        let n = self.graphic_eq.num_bands();
        if self.graph.eq_num_bands() < n {
            self.graph.eq_mut().eq =
                crate::dsp::equalizer::ParametricEq::new(n, self.output_sample_rate as f32);
        }
        self.graphic_eq.sync_into(&mut self.graph.eq_mut().eq);
    }

    #[cfg(feature = "resample")]
    fn resampler_set_quality_all(&mut self, quality: config::ResamplerQuality) {
        use crate::engine::PlaybackStream;
        match &mut self.stream {
            Some(PlaybackStream::Single {
                resampler: Some(ref mut r),
                ..
            }) => r.set_quality(quality),
            Some(PlaybackStream::Transitioning {
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

    /// Dispatch a single engine command to the appropriate handler.
    fn handle_command(&mut self, cmd: EngineCommand) {
        match cmd {
            // ── Playback ──
            EngineCommand::Play => self.handle_play(),
            EngineCommand::Pause => self.handle_pause(),
            EngineCommand::Stop => self.handle_stop(),
            EngineCommand::Seek(pos) => self.handle_seek(pos),
            EngineCommand::SetSpeed(speed) => self.handle_set_speed(speed),
            EngineCommand::SetPitch(semitones) => self.handle_set_pitch(semitones),
            EngineCommand::Shutdown => self.handle_shutdown(),

            // ── Volume ──
            EngineCommand::SetVolume(vol) => self.handle_set_volume(vol),
            EngineCommand::SetVolumeDb(db) => self.handle_set_volume_db(db),

            // ── EQ ──
            EngineCommand::SetEqEnabled(enabled) => self.handle_set_eq_enabled(enabled),
            EngineCommand::SetEqAutoHeadroom(enabled) => self.handle_set_eq_auto_headroom(enabled),
            EngineCommand::SetEqBand {
                index,
                frequency,
                gain_db,
                q,
                enabled,
            } => self.handle_set_eq_band(index, frequency, gain_db, q, enabled),
            EngineCommand::SetEqBandParams {
                index,
                frequency,
                gain_db,
                q,
                filter_type,
                enabled,
            } => self.handle_set_eq_band_params(index, frequency, gain_db, q, filter_type, enabled),
            EngineCommand::SetEqPreset(preset) => self.handle_set_eq_preset(preset),
            EngineCommand::SetGraphicEqLayout(layout) => self.handle_set_graphic_eq_layout(layout),
            EngineCommand::SetGraphicEqSlider { band, gain_db } => {
                self.handle_set_graphic_eq_slider(band, gain_db)
            }
            EngineCommand::SetGraphicEqPreamp(db) => self.handle_set_graphic_eq_preamp(db),
            EngineCommand::SetGraphicEqEnabled(enabled) => {
                self.handle_set_graphic_eq_enabled(enabled)
            }
            EngineCommand::SetBassShelf(db) => self.handle_set_bass_shelf(db),
            EngineCommand::SetTrebleShelf(db) => self.handle_set_treble_shelf(db),
            EngineCommand::SetPreamp(db) => self.handle_set_preamp(db),
            EngineCommand::SetMidsideEq(enabled) => self.handle_set_midside_eq(enabled),

            // ── DSP ──
            EngineCommand::SetStereoWidth(width) => self.handle_set_stereo_width(width),
            EngineCommand::SetBalance(balance) => self.handle_set_balance(balance),
            EngineCommand::SetDitherEnabled(enabled) => self.handle_set_dither_enabled(enabled),
            EngineCommand::SetCrossfeedEnabled(enabled) => {
                self.handle_set_crossfeed_enabled(enabled)
            }
            EngineCommand::SetCrossfeedProfile(profile) => {
                self.handle_set_crossfeed_profile(profile)
            }
            EngineCommand::SetCrossfeedCustomParams {
                frequency_hz,
                q,
                delay_ms,
            } => self.handle_set_crossfeed_custom_params(frequency_hz, q, delay_ms),
            EngineCommand::SetCompressorEnabled(enabled) => {
                self.handle_set_compressor_enabled(enabled)
            }
            EngineCommand::SetCompressorBandParams {
                band,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_gain_db,
            } => self.handle_set_compressor_band_params(
                band,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_gain_db,
            ),
            EngineCommand::SetPrecisionMode(mode) => self.handle_set_precision_mode(mode),
            EngineCommand::SetBitPerfect(enabled) => self.handle_set_bit_perfect(enabled),
            EngineCommand::SetLimiterMode(mode) => self.handle_set_limiter_mode(mode),
            EngineCommand::SetLimiterTruePeak(enabled) => {
                self.handle_set_limiter_true_peak(enabled)
            }
            EngineCommand::SetResamplerQuality(quality) => {
                self.handle_set_resampler_quality(quality)
            }
            EngineCommand::SetCrossfadeConfig(cfg) => self.handle_set_crossfade_config(cfg),
            EngineCommand::SetCrossfadeCurve(curve) => self.handle_set_crossfade_curve(curve),
            EngineCommand::SetTransitionMode(mode) => self.handle_set_transition_mode(mode),
            EngineCommand::SetSpeedMode(mode) => self.handle_set_speed_mode(mode),

            // ── Output ──
            EngineCommand::SetOutputBackend(backend) => self.handle_set_output_backend(backend),
            EngineCommand::SetOutputDevice(device) => self.handle_set_output_device(device),
            EngineCommand::SetSampleRatePolicy(policy) => {
                self.handle_set_sample_rate_policy(policy)
            }
            EngineCommand::SetVolumeMode(mode) => self.handle_set_volume_mode(mode),
            EngineCommand::SetFallbackPolicy(policy) => self.handle_set_fallback_policy(policy),
            EngineCommand::SetOutputProfile(profile) => self.handle_set_output_profile(profile),
            EngineCommand::ClearOutputProfile => self.handle_clear_output_profile(),
            EngineCommand::OpenAsioControlPanel => self.handle_open_asio_control_panel(),

            // ── Multichannel ──
            EngineCommand::SetChannelMix(cfg) => self.handle_set_channel_mix(cfg),
            EngineCommand::SetChannelPolicy(policy) => self.handle_set_channel_policy(policy),
            EngineCommand::SetChannelTrim(cfg) => self.handle_set_channel_trim(cfg),
            EngineCommand::SetChannelRouting(cfg) => self.handle_set_channel_routing(cfg),
            EngineCommand::SetChannelEq(cfg) => self.handle_set_channel_eq(cfg),
            EngineCommand::SetLfeConfig(cfg) => self.handle_set_lfe_config(cfg),
            EngineCommand::SetBassManagement(cfg) => self.handle_set_bass_management(cfg),

            // ── Lifecycle ──
            EngineCommand::Open(source) => self.handle_open(source),
            EngineCommand::PrepareNext(source) => self.handle_prepare_next(source),
            EngineCommand::RecoverStream => self.handle_recover_stream(),
            EngineCommand::AutoRecoverStream => self.handle_auto_recover_stream(),
            EngineCommand::LoudnessScanComplete { path, result } => {
                self.handle_loudness_scan_complete(path, result)
            }
            EngineCommand::WriteLoudnessTags(path) => self.handle_write_loudness_tags(path),

            // ── Playlist ──
            EngineCommand::Enqueue(source) => self.handle_enqueue(source),
            EngineCommand::RemoveFromPlaylist(index) => self.handle_remove_from_playlist(index),
            EngineCommand::ClearPlaylist => self.handle_clear_playlist(),
            EngineCommand::PlayIndex(index) => self.handle_play_index(index),
            EngineCommand::Next => self.handle_next(),
            EngineCommand::Previous => self.handle_previous(),
            EngineCommand::SetRepeatMode(mode) => self.handle_set_repeat_mode(mode),
            EngineCommand::SetShuffle(enabled) => self.handle_set_shuffle(enabled),

            // ── Capture ──
            EngineCommand::CaptureStart { path, device } => self.handle_capture_start(path, device),
            EngineCommand::CaptureStop => self.handle_capture_stop(),
        }
    }
}
