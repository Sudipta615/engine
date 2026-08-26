//! Output command handlers — backend, device, sample rate policy, volume mode,
//! fallback policy, output profiles, hardware/software volume, ASIO control panel.

use log::{error, info, warn};

use super::super::AudioEngine;
use crate::dsp::pipeline::VolumePath;

impl AudioEngine {
    pub(super) fn handle_set_output_backend(&mut self, backend: config::AudioBackend) {
        if self.config.output_backend != backend {
            self.config.output_backend = backend;
            info!("Output backend set to {:?}, recovering stream...", backend);
            if let Err(e) = self.recover_output_stream() {
                error!("Failed to recover stream after backend change: {}", e);
            }
        }
    }

    pub(super) fn handle_set_output_device(&mut self, device: Option<String>) {
        if self.config.output_device != device {
            self.config.output_device = device.clone();
            info!("Output device set to {:?}, recovering stream...", device);
            if let Err(e) = self.recover_output_stream() {
                error!("Failed to recover stream after device change: {}", e);
            }
            #[cfg(feature = "audio-output")]
            self.emit_output_event(crate::events::OutputEvent::OutputDeviceChanged { device });
        }
    }

    pub(super) fn handle_set_sample_rate_policy(&mut self, policy: config::SampleRatePolicy) {
        info!("Sample rate policy set to: {}", policy.display_name());
        self.config.sample_rate_policy = policy.clone();
        if let Some(ref mut output) = self.audio_output {
            let caps = output.capabilities();
            let target = caps.best_rate_for(self.clock.source_sample_rate, &policy);
            if let Ok(actual) = output.reconfigure_sample_rate(target) {
                self.output_sample_rate = actual;
                self.graph.update_sample_rate(actual as f32);
            }
        }
    }

    pub(super) fn handle_set_volume_mode(&mut self, mode: config::VolumeMode) {
        info!("Volume mode set to {:?}", mode);
        self.config.volume_mode = mode;
        let has_hw = self
            .audio_output
            .as_ref()
            .is_some_and(|o| o.supports_hardware_volume());
        match mode {
            config::VolumeMode::HardwarePreferred | config::VolumeMode::HardwareOnly if has_hw => {
                self.graph.set_volume(1.0);
                info!("{mode:?} active: software pipeline set to unity gain (1.0).");
                self.write_playback_info(|pb| {
                    pb.volume_path = Some(VolumePath::Hardware);
                    pb.volume_error = None;
                });
            }
            config::VolumeMode::HardwarePreferred => {
                let message = "HardwarePreferred: endpoint volume unavailable for the active output; using software gain"
                    .to_string();
                warn!("{message}");
                let vol = self.playback_info.load().volume;
                self.graph.set_volume(vol);
                self.write_playback_info(|pb| {
                    pb.volume_error = Some(message.clone());
                    pb.volume_path = Some(VolumePath::Software);
                });
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
                self.write_playback_info(|pb| {
                    pb.volume_path = Some(VolumePath::Software);
                    pb.volume_error = None;
                });
            }
        }
    }

    pub(super) fn handle_set_fallback_policy(&mut self, policy: config::FallbackPolicy) {
        info!("Fallback policy set to {:?}", policy);
        self.config.fallback_policy = policy;
    }

    pub(super) fn handle_set_volume(&mut self, vol: f32) {
        if !vol.is_finite() {
            warn!("SetVolume ignored: non-finite value {}", vol);
            return;
        }
        let clamped = vol.clamp(0.0, 1.0);
        if self.graph.is_bit_perfect() && !self.volume_uses_hardware() {
            let message = "Bit-Perfect mode: software volume is disabled; use hardware volume or disable Bit-Perfect mode".to_string();
            warn!("{message}");
            self.write_playback_info(|pb| {
                pb.volume_error = Some(message.clone());
                pb.volume_path = None;
            });
            return;
        }
        if !self.volume_uses_hardware() {
            self.graph.set_volume(clamped);
            self.write_playback_info(|pb| {
                pb.volume = clamped;
                pb.volume_error = None;
                pb.volume_path = Some(VolumePath::Software);
            });
        } else {
            self.apply_hardware_volume_linear(clamped);
        }
    }

    pub(super) fn handle_set_volume_db(&mut self, db: f32) {
        if !db.is_finite() {
            warn!("SetVolumeDb ignored: non-finite value {}", db);
            return;
        }
        let linear = if db <= -60.0 {
            0.0
        } else {
            10.0_f32.powf(db.clamp(-60.0, 0.0) / 20.0)
        };
        if self.graph.is_bit_perfect() && !self.volume_uses_hardware() {
            let message = "Bit-Perfect mode: software volume is disabled; use hardware volume or disable Bit-Perfect mode".to_string();
            warn!("{message}");
            self.write_playback_info(|pb| {
                pb.volume_error = Some(message.clone());
                pb.volume_path = None;
            });
            return;
        }
        if !self.volume_uses_hardware() {
            self.graph.set_volume_db(db);
            self.write_playback_info(|pb| {
                pb.volume = linear;
                pb.volume_error = None;
                pb.volume_path = Some(VolumePath::Software);
            });
        } else {
            self.apply_hardware_volume_db(db, linear);
        }
    }

    /// Apply a hardware endpoint volume change from a linear gain value.
    fn apply_hardware_volume_linear(&mut self, clamped: f32) {
        let db = if clamped <= 0.0001 {
            -96.0
        } else {
            20.0 * clamped.log10()
        };
        self.apply_hardware_volume_db(db, clamped);
    }

    /// Shared path for hardware volume: converts to dB (if needed), sends to
    /// output backend, and falls back to software gain when appropriate.
    fn apply_hardware_volume_db(&mut self, db: f32, linear: f32) {
        match self
            .audio_output
            .as_ref()
            .map(|o| o.set_hardware_volume_db(db))
        {
            Some(Ok(())) => {
                self.graph.set_volume(1.0);
                self.write_playback_info(|pb| {
                    pb.volume = linear;
                    pb.volume_error = None;
                    pb.volume_path = Some(VolumePath::Hardware);
                });
            }
            Some(Err(e)) => {
                error!("Hardware volume set failed: {}", e);
                if self.config.volume_mode == config::VolumeMode::HardwareOnly {
                    self.graph.set_volume(1.0);
                    self.write_playback_info(|pb| {
                        pb.volume = linear;
                        pb.volume_error = Some(format!(
                            "HardwareOnly: endpoint volume set failed ({e}); \
                             software volume NOT applied"
                        ));
                        pb.volume_path = None;
                    });
                } else {
                    self.graph.set_volume(linear);
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
                self.graph.set_volume(1.0);
                self.write_playback_info(|pb| {
                    pb.volume = linear;
                    pb.volume_error = None;
                    pb.volume_path = if self.config.volume_mode == config::VolumeMode::HardwareOnly
                    {
                        None
                    } else {
                        Some(VolumePath::Hardware)
                    };
                });
            }
        }
    }

    pub(super) fn handle_set_output_profile(&mut self, profile: crate::output::OutputProfile) {
        let backend_changed = profile
            .backend_preference
            .is_some_and(|b| Some(b) != self.profile_backend());
        self.output_profile = Some(profile.clone());
        self.config.output_backend = profile
            .backend_preference
            .unwrap_or(self.config.output_backend);
        if self.audio_output.is_some() && backend_changed {
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

    pub(super) fn handle_clear_output_profile(&mut self) {
        self.output_profile = None;
        self.write_playback_info(|pb| pb.active_output_profile = None);
        info!("Output profile cleared");
    }

    pub(super) fn handle_open_asio_control_panel(&mut self) {
        if let Some(ref output) = self.audio_output {
            match output.open_control_panel() {
                Ok(()) => info!("ASIO control panel opened"),
                Err(e) => warn!("Could not open ASIO control panel: {e}"),
            }
        } else {
            warn!("OpenAsioControlPanel: no output device is active");
        }
    }
}
