//! Real-time audio rendering and planar buffer conversion for ASIO callbacks.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use crate::buffer::{FixedFrameBuffer, MAX_CHANNELS};
use super::types::*;

/// Shared state passed to the ASIO callback handler.
pub struct AsioRenderContext {
    pub ring_buffer: Arc<FixedFrameBuffer>,
    pub num_channels: usize,
    pub sample_type: ASIOSampleType,
    pub buffer_size_frames: usize,
    pub dither_enabled: AtomicBool,
    pub active: AtomicBool,
    pub underrun_count: AtomicU32,
    pub clip_count: AtomicU32,
    pub nan_count: AtomicU32,
}

impl AsioRenderContext {
    pub fn new(
        ring_buffer: Arc<FixedFrameBuffer>,
        num_channels: usize,
        sample_type: ASIOSampleType,
        buffer_size_frames: usize,
    ) -> Self {
        Self {
            ring_buffer,
            num_channels: num_channels.max(1).min(MAX_CHANNELS),
            sample_type,
            buffer_size_frames,
            dither_enabled: AtomicBool::new(false),
            active: AtomicBool::new(true),
            underrun_count: AtomicU32::new(0),
            clip_count: AtomicU32::new(0),
            nan_count: AtomicU32::new(0),
        }
    }

    /// Fill planar output buffers from the interleaved ring buffer during `bufferSwitch`.
    ///
    /// # Safety
    /// `dest_buffers` must point to valid memory allocated by the driver for `frames` samples per channel.
    pub unsafe fn render_block(
        &self,
        dest_buffers: &[*mut std::ffi::c_void],
        frames: usize,
    ) {
        if !self.active.load(Ordering::Relaxed) || dest_buffers.is_empty() {
            // Fill with silence
            for &buf in dest_buffers {
                if !buf.is_null() {
                    let bytes = frames * sample_type_byte_size(self.sample_type);
                    std::ptr::write_bytes(buf as *mut u8, 0, bytes);
                }
            }
            return;
        }

        let ch_count = self.num_channels.min(dest_buffers.len());
        let total_samples = frames * ch_count;

        // Thread-local scratch buffer for interleaved f32 frames
        thread_local! {
            static SCRATCH: std::cell::RefCell<Vec<f32>> = std::cell::RefCell::new(Vec::new());
        }

        SCRATCH.with(|cell| {
            let mut scratch = cell.borrow_mut();
            if scratch.len() < total_samples {
                scratch.resize(total_samples, 0.0);
            }

            let slice = &mut scratch[..total_samples];
            let read_frames = self.ring_buffer.pop_frames_interleaved(slice, ch_count);

            if read_frames < frames {
                self.underrun_count.fetch_add(1, Ordering::Relaxed);
                // Zero remaining frames (underrun silence padding)
                slice[read_frames * ch_count..total_samples].fill(0.0);
            }

            // Scatter interleaved f32 samples to driver planar buffers
            for (ch, &buf_ptr) in dest_buffers.iter().enumerate().take(ch_count) {
                if buf_ptr.is_null() {
                    continue;
                }

                match self.sample_type {
                    ASIOSampleType::Float32LSB | ASIOSampleType::Float32MSB => {
                        let out = buf_ptr as *mut f32;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + ch];
                            if !s.is_finite() {
                                self.nan_count.fetch_add(1, Ordering::Relaxed);
                                *out.add(frame) = 0.0;
                            } else {
                                if s.abs() > 1.0 {
                                    self.clip_count.fetch_add(1, Ordering::Relaxed);
                                }
                                *out.add(frame) = s.clamp(-1.0, 1.0);
                            }
                        }
                    }
                    ASIOSampleType::Float64LSB | ASIOSampleType::Float64MSB => {
                        let out = buf_ptr as *mut f64;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + ch];
                            if !s.is_finite() {
                                self.nan_count.fetch_add(1, Ordering::Relaxed);
                                *out.add(frame) = 0.0;
                            } else {
                                *out.add(frame) = s.clamp(-1.0, 1.0) as f64;
                            }
                        }
                    }
                    ASIOSampleType::Int32LSB | ASIOSampleType::Int32MSB => {
                        let out = buf_ptr as *mut i32;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + ch];
                            let scaled = (s.clamp(-1.0, 1.0) * 2147483647.0) as i32;
                            *out.add(frame) = scaled;
                        }
                    }
                    ASIOSampleType::Int32LSB24 => {
                        let out = buf_ptr as *mut i32;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + ch];
                            let scaled = ((s.clamp(-1.0, 1.0) * 8388607.0) as i32) << 8;
                            *out.add(frame) = scaled;
                        }
                    }
                    ASIOSampleType::Int16LSB | ASIOSampleType::Int16MSB => {
                        let out = buf_ptr as *mut i16;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + ch];
                            let scaled = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                            *out.add(frame) = scaled;
                        }
                    }
                    ASIOSampleType::Int24LSB => {
                        let out = buf_ptr as *mut u8;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + ch];
                            let scaled = (s.clamp(-1.0, 1.0) * 8388607.0) as i32;
                            let bytes = scaled.to_le_bytes();
                            *out.add(frame * 3) = bytes[0];
                            *out.add(frame * 3 + 1) = bytes[1];
                            *out.add(frame * 3 + 2) = bytes[2];
                        }
                    }
                    _ => {
                        // Default fallback: 32-bit float
                        let out = buf_ptr as *mut f32;
                        for frame in 0..frames {
                            *out.add(frame) = slice[frame * ch_count + ch].clamp(-1.0, 1.0);
                        }
                    }
                }
            }
        });
    }
}

/// Returns sample byte size for memory clearing.
#[inline]
pub fn sample_type_byte_size(st: ASIOSampleType) -> usize {
    match st {
        ASIOSampleType::Int16LSB | ASIOSampleType::Int16MSB => 2,
        ASIOSampleType::Int24LSB | ASIOSampleType::Int24MSB => 3,
        ASIOSampleType::Int32LSB
        | ASIOSampleType::Int32MSB
        | ASIOSampleType::Int32LSB16
        | ASIOSampleType::Int32LSB18
        | ASIOSampleType::Int32LSB20
        | ASIOSampleType::Int32LSB24
        | ASIOSampleType::Float32LSB
        | ASIOSampleType::Float32MSB => 4,
        ASIOSampleType::Float64LSB | ASIOSampleType::Float64MSB => 8,
        ASIOSampleType::DSDInt8LSB1 | ASIOSampleType::DSDInt8MSB1 | ASIOSampleType::DSDInt8NER8 => 1,
        _ => 4,
    }
}
