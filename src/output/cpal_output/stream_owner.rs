//! The `cpal-stream-owner` thread: opens the negotiated format's stream on a
//! dedicated thread (cpal `Stream` is not portable across threads), then
//! services pause/resume/stop commands until shutdown.

use std::sync::{atomic::AtomicBool, Arc};

use cpal::traits::{DeviceTrait, StreamTrait};

use super::{CpalFormatPlan, CpalOutput, CpalStreamCommand, CpalStreamHandle, OutputError};
use crate::dsp::dither::DitherType;
use crate::output::format_converter::{AudioFormatConverter, TargetFormat};
use crate::output::output::StreamErrorEvent;

impl CpalOutput {
    pub(crate) fn start_raw(&mut self) -> Result<(), OutputError> {
        let buffer = Arc::clone(&self.buffer);
        let paused = Arc::clone(&self.paused);
        let in_callback = Arc::clone(&self.in_callback);
        let underruns = Arc::clone(&self.underruns);
        let stream_errors = self.stream_errors.clone();
        let channels = self.stream_config.channels as usize;
        let callback_initialized = Arc::new(AtomicBool::new(false));
        let dither_enabled = Arc::clone(&self.dither_enabled);
        let clip_counter = Arc::clone(&self.clip_counter);
        let nan_counter = Arc::clone(&self.nan_counter);

        let error_callback = move |err: cpal::Error| {
            let event = StreamErrorEvent::from_cpal(&err);
            log::error!(
                "Audio output error [{}]: {} ({})",
                event.error_type,
                event.message,
                event.details
            );
            stream_errors.report(event);
        };

        let device = self.device.clone();
        let stream_config = self.stream_config.clone();
        let format_plan = self.format_plan;
        let (commands, command_rx) = crossbeam::channel::bounded(8);
        let (ready_tx, ready_rx) = crossbeam::channel::bounded(1);

        let owner = std::thread::Builder::new()
            .name("cpal-stream-owner".to_string())
            .spawn(move || {
                let result = (|| -> Result<(), String> {
                    let stream = match format_plan {
                        CpalFormatPlan::I32 => {
                            let in_callback = Arc::clone(&in_callback);
                            let callback_initialized = Arc::clone(&callback_initialized);
                            let clip_counter = Arc::clone(&clip_counter);
                            let nan_counter = Arc::clone(&nan_counter);
                            let dither_enabled = Arc::clone(&dither_enabled);

                            let scratch_cap = match stream_config.buffer_size {
                                cpal::BufferSize::Fixed(f) => (f as usize) * channels,
                                cpal::BufferSize::Default => 65536,
                            };
                            let mut scratch_buffer = vec![0.0f32; scratch_cap];
                            let mut converter =
                                AudioFormatConverter::new(TargetFormat::I32, DitherType::None);

                            device
                        .build_output_stream(
                        stream_config,
                        move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                            crate::output::cpal_devices::escalate_callback_thread_priority(
                                &callback_initialized,
                            );
                            crate::output::cpal_callbacks::audio_callback_i32(
                                data,
                                &buffer,
                                &paused,
                                &in_callback,
                                &underruns,
                                channels,
                                &mut scratch_buffer,
                                &mut converter,
                                &dither_enabled,
                                &clip_counter,
                                &nan_counter,
                            );
                        },
                        error_callback,
                        None,
                    )
                    .map_err(|e| format!("{}", e))?
                        }
                        CpalFormatPlan::F64 => {
                            let in_callback = Arc::clone(&in_callback);
                            let callback_initialized = Arc::clone(&callback_initialized);
                            let clip_counter = Arc::clone(&clip_counter);
                            let nan_counter = Arc::clone(&nan_counter);

                            let scratch_cap = match stream_config.buffer_size {
                                cpal::BufferSize::Fixed(f) => (f as usize) * channels,
                                cpal::BufferSize::Default => 65536,
                            };
                            let mut scratch_buffer = vec![0.0f32; scratch_cap];
                            let mut converter =
                                AudioFormatConverter::new(TargetFormat::F64, DitherType::None);

                            device
                        .build_output_stream(
                        stream_config,
                        move |data: &mut [f64], _: &cpal::OutputCallbackInfo| {
                            crate::output::cpal_devices::escalate_callback_thread_priority(
                                &callback_initialized,
                            );
                            crate::output::cpal_callbacks::audio_callback_f64(
                                data,
                                &buffer,
                                &paused,
                                &in_callback,
                                &underruns,
                                channels,
                                &mut scratch_buffer,
                                &mut converter,
                                &clip_counter,
                                &nan_counter,
                            );
                        },
                        error_callback,
                        None,
                    )
                    .map_err(|e| format!("{}", e))?
                        }
                        CpalFormatPlan::F32 => {
                            let in_callback = Arc::clone(&in_callback);
                            let callback_initialized = Arc::clone(&callback_initialized);
                            let clip_counter = Arc::clone(&clip_counter);
                            let nan_counter = Arc::clone(&nan_counter);
                            let mut converter =
                                AudioFormatConverter::new(TargetFormat::F32, DitherType::None);

                            device
                        .build_output_stream(
                        stream_config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            crate::output::cpal_devices::escalate_callback_thread_priority(
                                &callback_initialized,
                            );
                            crate::output::cpal_callbacks::audio_callback_f32(
                                data,
                                &buffer,
                                &paused,
                                &in_callback,
                                &underruns,
                                channels,
                                &clip_counter,
                                &nan_counter,
                                &mut converter,
                            );
                        },
                        error_callback,
                        None,
                    )
                    .map_err(|e| format!("{}", e))?
                        }
                        CpalFormatPlan::I16 => {
                            let in_callback = Arc::clone(&in_callback);
                            let callback_initialized = Arc::clone(&callback_initialized);
                            let dither_enabled = Arc::clone(&dither_enabled);
                            let clip_counter = Arc::clone(&clip_counter);
                            let nan_counter = Arc::clone(&nan_counter);

                            let scratch_cap = match stream_config.buffer_size {
                                cpal::BufferSize::Fixed(f) => (f as usize) * channels,
                                cpal::BufferSize::Default => 65536,
                            };
                            let mut scratch_buffer = vec![0.0f32; scratch_cap];
                            let mut converter = AudioFormatConverter::new(
                                TargetFormat::I16,
                                DitherType::Triangular,
                            );

                            device
                        .build_output_stream(
                        stream_config,
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                            crate::output::cpal_devices::escalate_callback_thread_priority(
                                &callback_initialized,
                            );
                            crate::output::cpal_callbacks::audio_callback_i16(
                                data,
                                &buffer,
                                &paused,
                                &in_callback,
                                &underruns,
                                channels,
                                &mut scratch_buffer,
                                &mut converter,
                                &dither_enabled,
                                &clip_counter,
                                &nan_counter,
                            );
                        },
                        error_callback,
                        None,
                    )
                    .map_err(|e| format!("{}", e))?
                        }
                        CpalFormatPlan::U16 => {
                            let in_callback = Arc::clone(&in_callback);
                            let callback_initialized = Arc::clone(&callback_initialized);
                            let dither_enabled = Arc::clone(&dither_enabled);
                            let clip_counter = Arc::clone(&clip_counter);
                            let nan_counter = Arc::clone(&nan_counter);

                            let scratch_cap = match stream_config.buffer_size {
                                cpal::BufferSize::Fixed(f) => (f as usize) * channels,
                                cpal::BufferSize::Default => 65536,
                            };
                            let mut scratch_buffer = vec![0.0f32; scratch_cap];
                            let mut converter = AudioFormatConverter::new(
                                TargetFormat::U16,
                                DitherType::Triangular,
                            );

                            device
                        .build_output_stream(
                        stream_config,
                        move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                            crate::output::cpal_devices::escalate_callback_thread_priority(
                                &callback_initialized,
                            );
                            crate::output::cpal_callbacks::audio_callback_u16(
                                data,
                                &buffer,
                                &paused,
                                &in_callback,
                                &underruns,
                                channels,
                                &mut scratch_buffer,
                                &mut converter,
                                &dither_enabled,
                                &clip_counter,
                                &nan_counter,
                            );
                        },
                        error_callback,
                        None,
                    )
                    .map_err(|e| format!("{}", e))?
                        }
                        CpalFormatPlan::Unsupported => {
                            return Err(OutputError::UnsupportedFormat.to_string());
                        }
                    };

                    stream.play().map_err(|e| format!("Play failed: {}", e))?;
                    let _ = ready_tx.send(Ok(()));
                    while let Ok(command) = command_rx.recv() {
                        match command {
                            CpalStreamCommand::Pause => {
                                let _ = stream.pause();
                            }
                            CpalStreamCommand::Resume => {
                                let _ = stream.play();
                            }
                            CpalStreamCommand::Stop => break,
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|e| {
                OutputError::StreamOpen(format!("Failed to spawn CPAL stream owner: {}", e))
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.stream = Some(CpalStreamHandle {
                    commands,
                    owner: Some(owner),
                });
                log::info!("Audio output stream started successfully");
                Ok(())
            }
            Ok(Err(error)) => {
                let _ = owner.join();
                Err(OutputError::StreamOpen(error))
            }
            Err(_) => {
                let _ = owner.join();
                Err(OutputError::StreamOpen(
                    "CPAL stream owner exited before reporting readiness".to_string(),
                ))
            }
        }
    }
}
