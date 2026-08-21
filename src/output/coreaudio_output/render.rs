//! The CoreAudio IO-proc render callback and its conversion kernels.
//!
//! # Realtime-safety contract (spec §3.7, §27)
//!
//! The callback runs on CoreAudio's dedicated IO thread. It must be
//! allocation-free and lock-free:
//!
//! - samples are pulled from the lock-free [`FixedFrameBuffer`];
//! - conversion state (the [`AudioFormatConverter`] dither + a scratch
//!   buffer) is pre-allocated in the [`RenderContext`] and reused;
//! - all diagnostics are lock-free atomics.
//!
//! # Non-finite sample policy (spec §31)
//!
//! Identical to the cpal/WASAPI output kernels: NaN/±Inf → sanitize to `0.0`
//! and count; |sample| > 1.0 → clamp and count as a clip; underruns are
//! zero-filled and counted. One malformed sample must never reach the DAC.
//!
//! # Format adaptation
//!
//! The callback adapts to whatever PCM format the device's IO stream
//! presents (`TargetFormat` + interleaving flag, captured before
//! `AudioDeviceStart`). Big-endian and non-LinearPCM formats are rejected at
//! start time — they never reach this callback.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use objc2_core_audio::AudioObjectID;
// Data types are NOT re-exported by objc2-core-audio; import them from the
// companion types crate (same version, feature-unified).
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};

use crate::buffer::FixedFrameBuffer;
use crate::dsp::dither::DitherType;
use crate::output::cpal_callbacks::CallbackGuard;
use crate::output::format_converter::{AudioFormatConverter, TargetFormat};

/// Immutable-per-stream context handed to the IO proc via `client_data`.
///
/// The `UnsafeCell`s are safe because CoreAudio invokes a given IO proc from
/// a single dedicated IO thread (the standard contract, same as cpal's
/// stream-owner thread owning its converter). The context is boxed and leaked
/// for the lifetime of the proc and reclaimed in `stop()`.
pub struct RenderContext {
    pub buffer: Arc<FixedFrameBuffer>,
    pub paused: Arc<AtomicBool>,
    pub in_callback: Arc<AtomicBool>,
    pub underruns: Arc<AtomicU32>,
    pub clips: Arc<AtomicU32>,
    pub nans: Arc<AtomicU32>,
    pub dither_enabled: Arc<AtomicBool>,
    /// Negotiated conversion state — immutable while the IO proc runs.
    pub channels: usize,
    pub target: TargetFormat,
    pub bytes_per_sample: usize,
    /// True when the device format is interleaved (one buffer holds all
    /// channels); false = non-interleaved (one buffer per channel).
    pub interleaved: bool,
    /// `AudioFormatConverter` state (dither PRNG, noise-shaping error).
    /// Only the single IO thread touches it.
    pub converter: UnsafeCell<AudioFormatConverter>,
    /// Pre-allocated f32 scratch large enough for the negotiated buffer size.
    pub scratch: UnsafeCell<Vec<f32>>,
}

impl RenderContext {
    /// Allocate the conversion state. `buffer_size` is the negotiated device
    /// buffer in frames; the scratch is sized for `buffer_size * channels`.
    pub fn new(
        buffer: Arc<FixedFrameBuffer>,
        paused: Arc<AtomicBool>,
        in_callback: Arc<AtomicBool>,
        underruns: Arc<AtomicU32>,
        clips: Arc<AtomicU32>,
        nans: Arc<AtomicU32>,
        dither_enabled: Arc<AtomicBool>,
        channels: usize,
        target: TargetFormat,
        bytes_per_sample: usize,
        interleaved: bool,
        buffer_size: usize,
    ) -> Self {
        let dither_type = DitherType::Triangular;
        Self {
            buffer,
            paused,
            in_callback,
            underruns,
            clips,
            nans,
            dither_enabled,
            channels,
            target,
            bytes_per_sample,
            interleaved,
            converter: UnsafeCell::new(AudioFormatConverter::new(target, dither_type)),
            scratch: UnsafeCell::new(vec![0.0f32; buffer_size.saturating_mul(channels).max(64)]),
        }
    }
}

/// The CoreAudio IO proc. `client_data` is a `Box<RenderContext>` (leaked).
///
/// # Safety
///
/// - `output` and the other `NonNull` params are valid for the duration of
///   the call (CoreAudio contract).
/// - `client_data` is the `*mut RenderContext` established at
///   `AudioDeviceCreateIOProcID` and is valid until `AudioDeviceDestroyIOProcID`.
pub unsafe extern "C-unwind" fn render_io_proc(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    _input_data: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    output_data: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    // SAFETY: `client_data` is the leaked RenderContext from start().
    let ctx = unsafe { &*(client_data as *const RenderContext) };
    let _guard = CallbackGuard::new(&ctx.in_callback);

    // The output AudioBufferList and the pre-allocated scratch are touched
    // only here (single IO thread).
    // SAFETY: `converter` and `scratch` are only accessed from this callback,
    // and CoreAudio serializes IO proc invocations per proc.
    let converter = unsafe { &mut *ctx.converter.get() };
    let scratch = unsafe { &mut *ctx.scratch.get() };
    let out = unsafe { output_data.as_ref() };

    if ctx.paused.load(Ordering::Acquire) || out.mNumberBuffers == 0 {
        zero_output(out, ctx.bytes_per_sample);
        return 0;
    }

    // Frames expected by the device for this cycle. Interleaved buffers hold
    // `frames * channels` samples; non-interleaved buffers hold `frames`
    // samples per channel.
    let bytes_per_frame = if ctx.interleaved {
        ctx.bytes_per_sample.saturating_mul(ctx.channels).max(1)
    } else {
        ctx.bytes_per_sample.max(1)
    };
    let frames = out.mBuffers[0].mDataByteSize as usize / bytes_per_frame;
    if frames == 0 {
        return 0;
    }
    let need = frames.saturating_mul(ctx.channels);
    if scratch.len() < need {
        // Negotiated size mismatch — must not allocate here; emit silence.
        zero_output(out, ctx.bytes_per_sample);
        ctx.underruns.fetch_add(1, Ordering::Relaxed);
        return 0;
    }

    let got_frames = ctx
        .buffer
        .pop_frames_interleaved(&mut scratch[..need], ctx.channels);
    let got_samples = got_frames.saturating_mul(ctx.channels);
    if got_samples < need {
        scratch[got_samples..need].fill(0.0);
        ctx.underruns.fetch_add(1, Ordering::Relaxed);
    }

    let dither_active = ctx.dither_enabled.load(Ordering::Relaxed);
    converter.set_dither_enabled(dither_active);

    if ctx.interleaved {
        render_interleaved(out, ctx, converter, scratch, frames);
    } else {
        render_non_interleaved(out, ctx, converter, scratch, frames);
    }
    0
}

/// Zero every output buffer (pause / underrun guard).
fn zero_output(out: &AudioBufferList, bytes_per_sample: usize) {
    let n = out.mNumberBuffers as usize;
    for i in 0..n {
        // SAFETY: flexible-array member access — the HAL allocated
        // `mNumberBuffers` AudioBuffers.
        let buf = unsafe { &*out.mBuffers.as_ptr().add(i) };
        if !buf.mData.is_null() && buf.mDataByteSize > 0 {
            // SAFETY: `mData` is writable for `mDataByteSize` bytes.
            unsafe { std::ptr::write_bytes(buf.mData as *mut u8, 0, buf.mDataByteSize as usize) };
        }
    }
    let _ = bytes_per_sample;
}

/// Interleaved layout: one buffer holds every channel per frame.
fn render_interleaved(
    out: &AudioBufferList,
    ctx: &RenderContext,
    converter: &mut AudioFormatConverter,
    scratch: &[f32],
    frames: usize,
) {
    // SAFETY: the HAL guarantees at least one buffer for an interleaved
    // stream.
    let buf = unsafe { &*out.mBuffers.as_ptr() };
    let dst = buf.mData as *mut u8;
    if dst.is_null() || buf.mDataByteSize == 0 {
        return;
    }
    let total = frames.saturating_mul(ctx.channels);
    let capacity = buf.mDataByteSize as usize;

    if ctx.target == TargetFormat::F32 {
        // Fast path: the f32 container is exactly what the engine produces.
        let dst_f32 = dst as *mut f32;
        let max = capacity / 4;
        for i in 0..total.min(max) {
            // SAFETY: `dst_f32` covers `capacity` bytes, and `i < max` keeps
            // the write inside it.
            unsafe {
                *dst_f32.add(i) = sanitize(scratch[i], ctx.clips.as_ref(), ctx.nans.as_ref())
            };
        }
        return;
    }

    for i in 0..total {
        let out_pos = i * ctx.bytes_per_sample;
        if out_pos + ctx.bytes_per_sample > capacity {
            break;
        }
        let s = sanitize(scratch[i], ctx.clips.as_ref(), ctx.nans.as_ref());
        // SAFETY: `dst` covers `capacity` bytes and `out_pos` is in bounds.
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(dst.add(out_pos), ctx.bytes_per_sample) };
        converter.convert_sample_to_bytes(s, bytes);
    }
}

/// Non-interleaved layout: one buffer per channel.
fn render_non_interleaved(
    out: &AudioBufferList,
    ctx: &RenderContext,
    converter: &mut AudioFormatConverter,
    scratch: &[f32],
    frames: usize,
) {
    let n = out.mNumberBuffers as usize;
    for ch in 0..n.min(ctx.channels) {
        // SAFETY: flexible-array member access; the HAL allocated
        // `mNumberBuffers` AudioBuffers.
        let buf = unsafe { &*out.mBuffers.as_ptr().add(ch) };
        let dst = buf.mData as *mut u8;
        if dst.is_null() || buf.mDataByteSize == 0 {
            continue;
        }
        let capacity = buf.mDataByteSize as usize;

        if ctx.target == TargetFormat::F32 {
            let dst_f32 = dst as *mut f32;
            let max = capacity / 4;
            for f in 0..frames.min(max) {
                // SAFETY: `dst_f32` covers `capacity` bytes, and `f < max`
                // keeps the write inside it.
                unsafe {
                    *dst_f32.add(f) = sanitize(
                        scratch[f * ctx.channels + ch],
                        ctx.clips.as_ref(),
                        ctx.nans.as_ref(),
                    )
                };
            }
            continue;
        }

        for f in 0..frames {
            let out_pos = f * ctx.bytes_per_sample;
            if out_pos + ctx.bytes_per_sample > capacity {
                break;
            }
            // SAFETY: `dst` covers `capacity` bytes; `out_pos` is in bounds.
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(dst.add(out_pos), ctx.bytes_per_sample) };
            let sample = scratch[f * ctx.channels + ch];
            let s = sanitize(sample, ctx.clips.as_ref(), ctx.nans.as_ref());
            converter.convert_sample_to_bytes(s, bytes);
        }
    }
}

/// Sanitize-and-count: NaN/Inf → 0.0 + nan count; |x|>1 → clamp + clip count.
#[inline]
fn sanitize(sample: f32, clips: &AtomicU32, nans: &AtomicU32) -> f32 {
    if !sample.is_finite() {
        nans.fetch_add(1, Ordering::Relaxed);
        0.0
    } else if sample > 1.0 || sample < -1.0 {
        clips.fetch_add(1, Ordering::Relaxed);
        sample.clamp(-1.0, 1.0)
    } else {
        sample
    }
}

/// Reclaim a leaked RenderContext (called from `stop()`).
///
/// # Safety
///
/// `ptr` must be the exact pointer returned by `Box::into_raw` and must no
/// longer be referenced by a running IO proc.
pub unsafe fn reclaim_context(ptr: *mut c_void) {
    if !ptr.is_null() {
        // SAFETY: caller guarantees the box is no longer in use.
        drop(unsafe { Box::from_raw(ptr as *mut RenderContext) });
    }
}
