//! System-audio capture handlers (WASAPI loopback on Windows).
//!
//! Capture is fully asynchronous: `handle_capture_start` opens the loopback
//! client, spawns its capture thread (which fills a ring), and creates the
//! WAV writer; the engine tick loop then drains the ring into the file
//! (`AudioEngine::drain_capture` in `src/engine/tick.rs`) — so file I/O and
//! the capture thread never contend. `handle_capture_stop` stops the thread,
//! drains the remainder, and finalizes the WAV header.
//!
//! On non-Windows builds (or without `wasapi-native`) the commands emit a
//! `CaptureError` event instead of failing hard.

use std::path::PathBuf;

use crate::events::EngineEvent;

use super::AudioEngine;

impl AudioEngine {
    pub(super) fn handle_capture_start(&mut self, path: Option<PathBuf>, device: Option<String>) {
        #[cfg(all(target_os = "windows", feature = "wasapi-native"))]
        {
            if self.capture.is_some() {
                self.emit_event(EngineEvent::CaptureError(
                    "a capture is already active — stop it first".to_string(),
                ));
                return;
            }
            // The capture ring absorbs ~4 s of system audio at 48 kHz × 2 ch.
            const RING_FRAMES: usize = 48_000 * 4;

            match crate::output::WasapiLoopbackCapture::new(device.as_deref(), RING_FRAMES) {
                Ok(mut cap) => {
                    let rate = cap.sample_rate();
                    let channels = cap.channels();
                    let path = path.unwrap_or_else(|| PathBuf::from("capture.wav"));
                    let writer = match crate::output::wav_writer::WavFileWriter::create(
                        &path, rate, channels,
                    ) {
                        Ok(w) => w,
                        Err(e) => {
                            self.emit_event(EngineEvent::CaptureError(format!(
                                "cannot create '{}': {e}",
                                path.display()
                            )));
                            return;
                        }
                    };
                    if let Err(e) = cap.start() {
                        self.emit_event(EngineEvent::CaptureError(format!(
                            "capture start failed: {e}"
                        )));
                        return;
                    }
                    log::info!(
                        "capture started: '{}' ({} Hz / {} ch from '{}')",
                        path.display(),
                        rate,
                        channels,
                        cap.device_name()
                    );
                    self.capture = Some(crate::engine::ActiveCapture {
                        capture: cap,
                        writer,
                        path,
                    });
                    self.emit_event(EngineEvent::CaptureStarted {
                        path: self.capture.as_ref().unwrap().path.clone(),
                    });
                }
                Err(e) => {
                    self.emit_event(EngineEvent::CaptureError(format!(
                        "capture start failed: {e}"
                    )));
                }
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "wasapi-native")))]
        {
            let _ = (path, device);
            self.emit_event(EngineEvent::CaptureError(
                "system-audio capture requires the 'wasapi-native' feature on Windows".to_string(),
            ));
        }
    }

    pub(super) fn handle_capture_stop(&mut self) {
        #[cfg(all(target_os = "windows", feature = "wasapi-native"))]
        {
            let Some(active) = self.capture.take() else {
                self.emit_event(EngineEvent::CaptureError(
                    "no active capture to stop".to_string(),
                ));
                return;
            };
            let mut active = active;
            // Stop the thread, then drain whatever the ring still holds.
            active.capture.stop();
            let mut leftovers = [0.0f32; 4096];
            loop {
                let ch = active.capture.channels() as usize;
                let n = active
                    .capture
                    .buffer()
                    .pop_frames_interleaved(&mut leftovers, ch);
                if n == 0 {
                    break;
                }
                let _ = active.writer.write_frames(&leftovers[..n * ch]);
            }
            let frames = active.writer.frames_written();
            let duration = frames as f32 / active.capture.sample_rate().max(1) as f32;
            match active.writer.finalize() {
                Ok(()) => {
                    log::info!(
                        "capture stopped: '{}' ({} frames, {:.1}s)",
                        active.path.display(),
                        frames,
                        duration
                    );
                    self.emit_event(EngineEvent::CaptureStopped {
                        path: active.path.clone(),
                        frames,
                        duration_secs: duration,
                    });
                }
                Err(e) => {
                    self.emit_event(EngineEvent::CaptureError(format!(
                        "capture finalized with an error: {e}"
                    )));
                }
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "wasapi-native")))]
        {
            self.emit_event(EngineEvent::CaptureError(
                "system-audio capture requires the 'wasapi-native' feature on Windows".to_string(),
            ));
        }
    }
}
