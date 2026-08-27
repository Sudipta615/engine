//! Real-time audio rendering and planar buffer conversion for ASIO callbacks.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use super::types::*;
use crate::buffer::{DsdByteBuffer, FixedFrameBuffer, MAX_CHANNELS};

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
    /// When in native-DSD mode, the engine pushes raw interleaved DSD bytes
    /// into this ring and the render callback drains it directly to the
    /// driver's planar buffers. `None` in PCM mode.
    pub dsd_buffer: Option<Arc<DsdByteBuffer>>,
    /// Bytes per DSD word in the current wire format.
    pub dsd_frame_width: usize,
    /// Optional source→output channel remap: `map[out_ch] = source_ch`.
    /// `None` (default) is identity. Loaded once per block (lock-free), so a
    /// host can rewire multichannel ASIO outputs at runtime. Shared with the
    /// owning [`AsioOutput`](super::AsioOutput) so it survives context rebuilds.
    pub channel_map: Arc<arc_swap::ArcSwap<Option<Vec<u16>>>>,
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
            num_channels: num_channels.clamp(1, MAX_CHANNELS),
            sample_type,
            buffer_size_frames,
            dither_enabled: AtomicBool::new(false),
            active: AtomicBool::new(true),
            underrun_count: AtomicU32::new(0),
            clip_count: AtomicU32::new(0),
            nan_count: AtomicU32::new(0),
            dsd_buffer: None,
            dsd_frame_width: 0,
            channel_map: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(None::<Vec<u16>>)),
        }
    }

    /// Fill planar output buffers from the interleaved ring buffer during `bufferSwitch`.
    ///
    /// # Safety
    /// `dest_buffers` must point to valid memory allocated by the driver for `frames` samples per channel.
    pub unsafe fn render_block(&self, dest_buffers: &[*mut std::ffi::c_void], frames: usize) {
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
            static SCRATCH: std::cell::RefCell<Vec<f32>> = const { std::cell::RefCell::new(Vec::new()) };
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

            // Source→output channel remap, if the host installed one.
            let map = self.channel_map.load();

            // Scatter interleaved f32 samples to driver planar buffers
            for (ch, &buf_ptr) in dest_buffers.iter().enumerate().take(ch_count) {
                if buf_ptr.is_null() {
                    continue;
                }

                // Identity by default; otherwise route `ch` from the mapped
                // source channel (or silence when the map is out of range).
                let src_ch = match map.as_ref() {
                    Some(m) => m.get(ch).copied().unwrap_or(u16::MAX),
                    None => ch as u16,
                };
                if src_ch as usize >= ch_count {
                    // Out of range → silence this output channel.
                    let bytes = frames * sample_type_byte_size(self.sample_type);
                    std::ptr::write_bytes(buf_ptr as *mut u8, 0, bytes);
                    continue;
                }
                let src_ch = src_ch as usize;

                match self.sample_type {
                    ASIOSampleType::Float32LSB | ASIOSampleType::Float32MSB => {
                        let out = buf_ptr as *mut f32;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + src_ch];
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
                            let s = slice[frame * ch_count + src_ch];
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
                            let s = slice[frame * ch_count + src_ch];
                            let scaled = (s.clamp(-1.0, 1.0) * 2147483647.0) as i32;
                            *out.add(frame) = scaled;
                        }
                    }
                    ASIOSampleType::Int32LSB24 => {
                        let out = buf_ptr as *mut i32;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + src_ch];
                            let scaled = ((s.clamp(-1.0, 1.0) * 8388607.0) as i32) << 8;
                            *out.add(frame) = scaled;
                        }
                    }
                    ASIOSampleType::Int16LSB | ASIOSampleType::Int16MSB => {
                        let out = buf_ptr as *mut i16;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + src_ch];
                            let scaled = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                            *out.add(frame) = scaled;
                        }
                    }
                    ASIOSampleType::Int24LSB => {
                        let out = buf_ptr as *mut u8;
                        for frame in 0..frames {
                            let s = slice[frame * ch_count + src_ch];
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
                            *out.add(frame) = slice[frame * ch_count + src_ch].clamp(-1.0, 1.0);
                        }
                    }
                }
            }
        });
    }

    /// Fill planar output buffers from the native-DSD byte ring.
    ///
    /// Each DSD word (`dsd_frame_width / channels` bytes per channel) holds
    /// `samples_per_word` DSD samples; the driver expects one word per
    /// channel per ASIO buffer frame. The byte ring carries interleaved words
    /// that we scatter directly into the driver's planar buffers.
    ///
    /// # Safety
    /// `dest_buffers` must point to valid memory allocated by the driver.
    pub unsafe fn render_block_dsd(&self, dest_buffers: &[*mut std::ffi::c_void], frames: usize) {
        let Some(ref dsd_buf) = self.dsd_buffer else {
            return;
        };
        if !self.active.load(Ordering::Relaxed)
            || dest_buffers.is_empty()
            || self.dsd_frame_width == 0
        {
            for &buf in dest_buffers {
                if !buf.is_null() {
                    std::ptr::write_bytes(
                        buf as *mut u8,
                        0x69,
                        frames * self.dsd_frame_width.max(1) / self.num_channels.max(1),
                    );
                }
            }
            return;
        }

        let ch_count = self.num_channels.min(dest_buffers.len());
        let bpw = self.dsd_frame_width / ch_count.max(1);
        let want_bytes = frames * self.dsd_frame_width;

        thread_local! {
            static DSD_SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }

        DSD_SCRATCH.with(|cell| {
            let mut scratch = cell.borrow_mut();
            if scratch.len() < want_bytes {
                scratch.resize(want_bytes, 0);
            }
            let popped = dsd_buf.pop_frames(&mut scratch[..want_bytes], self.dsd_frame_width);
            let bytes = popped * self.dsd_frame_width;

            if popped < frames {
                self.underrun_count.fetch_add(1, Ordering::Relaxed);
                scratch[bytes..].fill(0x69);
            }

            for (ch, &buf_ptr) in dest_buffers.iter().enumerate().take(ch_count) {
                if buf_ptr.is_null() {
                    continue;
                }
                let dst = buf_ptr as *mut u8;
                for f in 0..frames {
                    let src_off = f * self.dsd_frame_width + ch * bpw;
                    let dst_off = f * bpw;
                    std::ptr::copy_nonoverlapping(
                        scratch.as_ptr().add(src_off),
                        dst.add(dst_off),
                        bpw,
                    );
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
        ASIOSampleType::DSDInt8LSB1 | ASIOSampleType::DSDInt8MSB1 | ASIOSampleType::DSDInt8NER8 => {
            1
        }
        _ => 4,
    }
}
