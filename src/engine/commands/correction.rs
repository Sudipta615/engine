//! Room & headphone correction command handlers (Phase 7 S5).
//!
//! The four-engine surface: `SetCorrectionEnabled` / `SetCorrectionDepth`
//! are live toggles that ride the graph's SPSC control queue (a swap
//! replays them via the sticky bus state); `LoadCorrectionIr` runs the
//! S2→S4 chain on the control thread and lands a rendered IR set;
//! `MeasureRoom` plays the S1 sweep on the primary stream, captures it
//! (WASAPI loopback on Windows; a generic input backend is Horizon), then
//! deconvolves → conditions → derives → lands, with progress/completion
//! events. Everything here is control-thread DSP — heap-happy by design;
//! the realtime path only ever sees precomputed partitioned-FFT state.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::{info, warn};

use crate::dsp::correction::{
    deconvolve, derive_correction_ir, derive_params_from_config, estimate_snr_db, read_wav_ir,
    CorrectionError, CorrectionIrSet, EssConfig, EssSweep, IrConditioner, WavIr,
};
use crate::events::EngineEvent;

use super::AudioEngine;

/// A room measurement in flight (Phase 7 S5): the sweep is playing and its
/// loopback capture is scheduled to stop at `stop_at`. Control-thread only.
pub(crate) struct PendingMeasurement {
    /// The sweep that was generated and played (needed to deconvolve the
    /// recording).
    pub(crate) sweep: EssSweep,
    /// The sweep WAV played on the primary stream (removed on completion).
    pub(crate) sweep_wav: PathBuf,
    /// Where the loopback recording is being written.
    pub(crate) recording: PathBuf,
    /// When the sweep has finished (+ margin) and capture must stop.
    pub(crate) stop_at: std::time::Instant,
}

impl AudioEngine {
    pub(super) fn handle_set_correction_enabled(&mut self, enabled: bool) {
        self.config.correction.enabled = enabled;
        self.graph.set_correction_enabled(enabled);
        info!(
            "correction {}",
            if enabled { "enabled" } else { "disabled" }
        );
    }

    pub(super) fn handle_set_correction_depth(&mut self, depth: f32) {
        let depth = depth.clamp(0.0, 1.0);
        self.config.correction.depth = depth;
        self.graph.set_correction_depth(depth);
    }

    /// Load a measured IR file and derive the correction from it (S2 → S4,
    /// using the config's target / boost clamp / smoothing / phase mode),
    /// then enable it. A missing or unreadable file keeps the previous
    /// correction (or none) — never a failure state.
    pub(super) fn handle_load_correction_ir(&mut self, path: PathBuf) {
        let set = match self.derive_correction_from_wav(&path, 60.0) {
            Ok(set) => set,
            Err(e) => {
                warn!(
                    "correction: failed to derive from '{}': {e}",
                    path.display()
                );
                return;
            }
        };
        self.land_correction(Arc::new(set));
        info!("correction: derived + enabled from '{}'", path.display());
    }

    /// Phase-7 S5 measurement orchestration: generate the S1 sweep, play it
    /// on the primary stream, capture it, and schedule the S1→S2→S4 landing
    /// (see [`AudioEngine::check_measurement`]).
    pub(super) fn handle_measure_room(&mut self, seconds: f32, pre_emphasis: f32) {
        if self.measurement.is_some() {
            self.emit_event(EngineEvent::MeasurementFailed(
                "a room measurement is already in progress — stop it first".to_string(),
            ));
            return;
        }
        let seconds = seconds.clamp(0.25, 120.0);
        let rate = self.output_sample_rate.max(1) as f64;
        let sweep = match EssSweep::new(EssConfig {
            sample_rate: rate,
            duration_secs: seconds as f64,
            f_start: 20.0,
            f_end: (rate * 0.45).min(20_000.0),
            amplitude: 0.4,
            fade_secs: 0.05,
            pre_emphasis: pre_emphasis > 0.0,
        }) {
            Ok(s) => s,
            Err(e) => {
                self.emit_event(EngineEvent::MeasurementFailed(format!(
                    "sweep generation failed: {e}"
                )));
                return;
            }
        };
        self.emit_event(EngineEvent::MeasurementProgress {
            stage: "sweep generated".to_string(),
        });

        // Play the sweep on the primary stream (it reaches the DAC / the
        // loopback through the normal output path).
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let sweep_wav = std::env::temp_dir().join(format!("freebuff_sweep_{stamp}.wav"));
        if let Err(e) = write_mono_i16_wav(&sweep_wav, self.output_sample_rate, sweep.samples()) {
            self.emit_event(EngineEvent::MeasurementFailed(format!(
                "cannot write the sweep WAV: {e}"
            )));
            return;
        }
        self.handle_open(crate::source::AudioSource::File(sweep_wav.clone()));
        self.handle_play();

        // Capture the system mix. On platforms without a capture backend the
        // CaptureError event fires and the measurement cannot complete.
        let recording = std::env::temp_dir().join(format!("freebuff_measurement_{stamp}.wav"));
        self.handle_capture_start(Some(recording.clone()), None);
        if !self.capture_active() {
            let _ = std::fs::remove_file(&sweep_wav);
            self.emit_event(EngineEvent::MeasurementFailed(
                "capture unavailable — a generic input backend (Horizon) will bring \
                 integrated measurement to this platform"
                    .to_string(),
            ));
            return;
        }
        self.emit_event(EngineEvent::MeasurementProgress {
            stage: "capture started".to_string(),
        });
        self.measurement = Some(PendingMeasurement {
            sweep,
            sweep_wav,
            recording,
            stop_at: std::time::Instant::now() + std::time::Duration::from_secs_f32(seconds + 1.5),
        });
    }

    /// Tick hook: when the in-flight measurement's sweep has finished (+
    /// margin), stop the capture and land the S1→S2→S4 result as the live
    /// correction. Control-thread only; called from the engine tick.
    pub(crate) fn check_measurement(&mut self) {
        let Some(meas) = self.measurement.take() else {
            return;
        };
        if std::time::Instant::now() < meas.stop_at {
            self.measurement = Some(meas);
            return;
        }
        // Sweep finished: stop the capture (finalizes the WAV), then derive.
        self.handle_capture_stop();
        if !meas.recording.exists() {
            let _ = std::fs::remove_file(&meas.sweep_wav);
            self.emit_event(EngineEvent::MeasurementFailed(format!(
                "recording '{}' was not produced",
                meas.recording.display()
            )));
            return;
        }
        match self.land_measurement(&meas) {
            Ok(snr_db) => {
                info!(
                    "correction: room measurement landed from '{}' (SNR {snr_db:.1} dB)",
                    meas.recording.display()
                );
                self.emit_event(EngineEvent::MeasurementComplete {
                    path: meas.recording.clone(),
                    snr_db,
                });
            }
            Err(e) => {
                self.emit_event(EngineEvent::MeasurementFailed(format!(
                    "measurement processing failed: {e}"
                )));
            }
        }
        let _ = std::fs::remove_file(&meas.sweep_wav);
    }

    /// S2 condition + S4 derive over a measured IR WAV (control thread).
    /// `snr_db` is the measurement's reported SNR (a config-driven load has
    /// no measurement and passes a confident default).
    fn derive_correction_from_wav(
        &self,
        path: &Path,
        snr_db: f64,
    ) -> Result<CorrectionIrSet, CorrectionError> {
        let wav = read_wav_ir(path)?;
        let session_rate = self.output_sample_rate.max(1) as f64;
        let conditioner = IrConditioner::default();
        let measured = conditioner.condition(&wav, session_rate)?;
        let ir_len = measured.channels[0]
            .len()
            .next_power_of_two()
            .clamp(1024, 32_768)
            .max(1024);
        let params =
            derive_params_from_config(&self.config.correction, session_rate, ir_len, snr_db);
        derive_correction_ir(&measured, &params)
    }

    /// Land a rendered correction IR set: load it into the active node,
    /// mirror it onto the sticky bus state (a generation swap replays it),
    /// and enable it.
    fn land_correction(&mut self, set: Arc<CorrectionIrSet>) {
        self.graph.load_correction_ir(set);
        self.config.correction.enabled = true;
        self.graph.set_correction_enabled(true);
    }

    /// S1 deconvolution → S2 condition → S4 derive over the finished sweep
    /// recording, landing the result as the live correction. Returns the
    /// measurement SNR (dB).
    fn land_measurement(&mut self, meas: &PendingMeasurement) -> Result<f32, CorrectionError> {
        let recorded = read_wav_ir(&meas.recording)?;
        let mono =
            recorded
                .channels
                .first()
                .cloned()
                .ok_or_else(|| CorrectionError::InvalidConfig {
                    what: "recording",
                    message: "the recording has no channels".into(),
                })?;
        let ir = deconvolve(&mono, &meas.sweep)?;
        let snr_db = estimate_snr_db(&ir).unwrap_or(0.0).max(0.0) as f32;
        let conditioner = IrConditioner::default();
        let measured = conditioner.condition(
            &WavIr {
                channels: vec![ir.real_ir()],
                sample_rate: ir.sample_rate,
            },
            ir.sample_rate,
        )?;
        let ir_len = measured.channels[0]
            .len()
            .next_power_of_two()
            .clamp(1024, 32_768)
            .max(1024);
        let params = derive_params_from_config(
            &self.config.correction,
            ir.sample_rate,
            ir_len,
            snr_db as f64,
        );
        let set = derive_correction_ir(&measured, &params)?;
        self.land_correction(Arc::new(set));
        Ok(snr_db)
    }
}

/// Whether a loopback capture is currently active (the Windows-only field
/// is hidden behind the cfg; this method is the portable probe).
impl AudioEngine {
    pub(crate) fn capture_active(&self) -> bool {
        #[cfg(all(target_os = "windows", feature = "wasapi-native"))]
        {
            self.capture.is_some()
        }
        #[cfg(not(all(target_os = "windows", feature = "wasapi-native")))]
        {
            false
        }
    }
}

/// Write a mono 16-bit PCM WAV (control-path helper; the sweep is small).
fn write_mono_i16_wav(path: &Path, sample_rate: u32, samples: &[f64]) -> std::io::Result<()> {
    let data_len = samples.len() * 2;
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    bytes.extend_from_slice(&2u16.to_le_bytes()); // block align
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, bytes)
}
