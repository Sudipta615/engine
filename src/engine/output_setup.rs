//! Output transport setup, start, and stop.
//!
//! This module owns the output device creation, volume-mode negotiation,
//! background device monitor thread, and the `start`/`stop` lifecycle methods.

use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use log::{info, warn};

use crate::{
    buffer::{EngineCommand, PlaybackState},
    output::create_output,
};
use config;

use super::{endpoints::EndpointTransport, AudioEngine, EngineError};

impl AudioEngine {
    #[cfg(feature = "audio-output")]
    pub(super) fn reopen_configured_endpoints(&mut self) -> Result<(), EngineError> {
        for endpoint in &mut self.endpoints {
            endpoint.stop();
        }
        self.endpoints.clear();
        for config in self.endpoint_configs.clone() {
            if !config.enabled {
                continue;
            }
            let endpoint_config = crate::output::EndpointConfig::from_config(config);
            self.endpoints.push(
                crate::output::EndpointWorker::open(
                    endpoint_config,
                    crate::buffer::OUTPUT_BUFFER_FRAMES,
                )
                .map_err(|e| EngineError::Config(format!("Endpoint open: {e}")))?,
            );
        }
        Ok(())
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
        if let Err(error) = self.reopen_configured_endpoints() {
            if let Some(mut endpoint) = self.endpoints.pop() {
                endpoint.stop();
            }
            return Err(error);
        }
        // Apply the active output profile (or auto-select one) now that the
        // device name is known.
        self.refresh_output_profile();

        self.running.store(true, Ordering::Release);
        self.graph
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
                        .and_then(|d| d.description().ok().map(|desc| desc.name().to_string()))
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

    /// Open + start every enabled additional endpoint from
    /// `config.additional_endpoints`. Control path (start/recovery): each
    /// endpoint gets its own ring, rate-matched resampler, and final
    /// limiter; failures are logged and the endpoint skipped so the primary
    /// device is never taken down by a secondary one. Idempotent: any
    /// existing endpoints are stopped and replaced.
    pub(crate) fn open_additional_endpoints(&mut self) {
        self.extra_endpoints.clear();
        let master_rate = self.output_sample_rate;
        for cfg in self.config.additional_endpoints.iter() {
            if !cfg.enabled {
                continue;
            }
            // One ring, shared by the backend (drains it in its realtime
            // callback) and the transport (feeds it from the decode loop).
            let ring =
                match crate::buffer::FixedFrameBuffer::new(crate::buffer::OUTPUT_BUFFER_FRAMES) {
                    Ok(r) => Arc::new(r),
                    Err(e) => {
                        warn!("Endpoint '{}' ring allocation failed: {}", cfg.device, e);
                        continue;
                    }
                };
            let output = match create_output(
                Arc::clone(&ring),
                cfg.backend,
                Some(cfg.device.as_str()),
                self.config.fallback_policy,
            ) {
                Ok(o) => o,
                Err(e) => {
                    warn!("Endpoint '{}' failed to open: {}", cfg.device, e);
                    continue;
                }
            };
            let mut ep = match EndpointTransport::open(
                cfg.clone(),
                output,
                ring,
                master_rate,
                self.config.resampler_quality,
                self.config.precision_mode,
                &self.config.limiter,
            ) {
                Ok(ep) => ep,
                Err(e) => {
                    warn!("Endpoint '{}' failed to initialize: {}", cfg.device, e);
                    continue;
                }
            };
            ep.gain = cfg.gain.clamp(0.0, 1.0);
            ep.output.set_dither_enabled(self.config.dither_enabled);
            if let Err(e) = ep.output.start() {
                warn!("Endpoint '{}' failed to start: {}", cfg.device, e);
                continue;
            }
            info!(
                "Additional endpoint '{}' started ({} Hz, master {} Hz, resampler {})",
                cfg.device,
                ep.rate,
                master_rate,
                if ep.resampler.is_some() { "on" } else { "off" }
            );
            self.extra_endpoints.push(ep);
        }
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        for endpoint in &mut self.endpoints {
            endpoint.stop();
        }
        self.endpoints.clear();
        #[cfg(feature = "audio-output")]
        self.reset_endpoints();
        if let Some(mut output) = self.audio_output.take() {
            output.stop();
        }
        for mut ep in self.extra_endpoints.drain(..) {
            ep.output.stop();
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
        self.graph.set_dop_bypass(false);
        if let Some(ref output) = self.audio_output {
            output.set_dither_enabled(self.config.dither_enabled);
        }
        // Keep the published playhead consistent with the reset internal
        // clock (see the Stop command handler for the same fix).
        self.write_playback_info(|pb| pb.position_secs = 0.0);
        self.update_playback_state(PlaybackState::Stopped);
        info!("Audio engine stopped");
    }
}
