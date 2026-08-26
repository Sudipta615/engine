//! Audio-callback output kernels (cpal backends).
//!
//! # Non-finite sample policy (spec §31, §32)
//!
//! This is the **explicit** boundary where the realtime output enforces the
//! engine's non-finite-sample contract:
//!
//! - **NaN / ±Inf → sanitize to 0.0** (never propagated to the DAC) and count
//!   the incident in the `nan` counter, which the engine surfaces as
//!   `PlaybackInfo::nan_count`.
//! - **|sample| > 1.0 → clamp to ±1.0** (hard clip) and count the incident as
//!   a clip. A non-zero clip count means the upstream DSP overshot and the
//!   safety limiter failed to catch it.
//!
//! The policy is deliberately *sanitize-and-report*, not *propagate*: a
//! single malformed sample must never destabilise the realtime engine or
//! reach a DAC as NaN (which some USB devices render as a loud DC spike).
//! The sanitisation lives in the output kernel because that is the last
//! point before hardware and therefore the only place every path (f32/f64,
//! multichannel, DoP-excluded PCM) is guaranteed to pass through.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::buffer::FixedFrameBuffer;
use crate::output::format_converter::AudioFormatConverter;

pub struct CallbackGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> CallbackGuard<'a> {
    #[inline]
    pub fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self { flag }
    }
}

impl<'a> Drop for CallbackGuard<'a> {
    #[inline]
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

/// Helper: clamp and count. Returns the clamped sample and increments the
/// clip counter if the input was out of range or non-finite.
#[inline(always)]
fn clamp_and_count(sample: f32, clip_counter: &AtomicU32, nan_counter: &AtomicU32) -> f32 {
    if !sample.is_finite() {
        nan_counter.fetch_add(1, Ordering::Relaxed);
        0.0
    } else if !(-1.0..=1.0).contains(&sample) {
        clip_counter.fetch_add(1, Ordering::Relaxed);
        sample.clamp(-1.0, 1.0)
    } else {
        sample
    }
}

/// Convert a contiguous stereo block while accumulating diagnostics locally.
/// The old callback performed an atomic counter update for each clipped or
/// non-finite sample. Keeping the counters local for the duration of one
/// callback materially reduces synchronization traffic while preserving the
/// same observable totals.
#[inline]
// Generic integer-conversion kernel shared by the f32/i16/i32 callback
// paths; the closure pair carries the quantization policy.
#[allow(clippy::too_many_arguments)]
fn convert_stereo_integer_block<T, Pair, Mono>(
    data: &mut [T],
    scratch: &[f32],
    converter: &mut AudioFormatConverter,
    clip_counter: &AtomicU32,
    nan_counter: &AtomicU32,
    zero: T,
    mut pair_converter: Pair,
    mut mono_converter: Mono,
) where
    T: Copy,
    Pair: FnMut(&mut AudioFormatConverter, f32, f32) -> (T, T),
    Mono: FnMut(&mut AudioFormatConverter, f32) -> T,
{
    let pair_samples = data.len().min(scratch.len()) & !1;
    let mut clips = 0u32;
    let mut nans = 0u32;

    for (dst, src) in data[..pair_samples]
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(scratch[..pair_samples].as_chunks::<2>().0)
    {
        let left = src[0];
        let right = src[1];
        let left_finite = left.is_finite();
        let right_finite = right.is_finite();
        if left_finite && right_finite {
            clips = clips
                .saturating_add(u32::from(!(-1.0..=1.0).contains(&left)))
                .saturating_add(u32::from(!(-1.0..=1.0).contains(&right)));
            // The common finite case stays paired, so stereo-aware dither
            // advances once for the pair instead of once per lane.
            let (converted_left, converted_right) = pair_converter(converter, left, right);
            dst[0] = converted_left;
            dst[1] = converted_right;
        } else {
            if !left_finite {
                nans = nans.saturating_add(1);
                dst[0] = zero;
            } else {
                clips = clips.saturating_add(u32::from(!(-1.0..=1.0).contains(&left)));
                dst[0] = mono_converter(converter, left);
            }
            if !right_finite {
                nans = nans.saturating_add(1);
                dst[1] = zero;
            } else {
                clips = clips.saturating_add(u32::from(!(-1.0..=1.0).contains(&right)));
                dst[1] = mono_converter(converter, right);
            }
        }
    }

    if pair_samples < data.len() && pair_samples < scratch.len() {
        let sample = scratch[pair_samples];
        if !sample.is_finite() {
            nans = nans.saturating_add(1);
            data[pair_samples] = zero;
        } else {
            clips = clips.saturating_add(u32::from(!(-1.0..=1.0).contains(&sample)));
            data[pair_samples] = mono_converter(converter, sample);
        }
    }

    if pair_samples < data.len() {
        data[pair_samples.saturating_add(1)..].fill(zero);
    }
    if clips > 0 {
        clip_counter.fetch_add(clips, Ordering::Relaxed);
    }
    if nans > 0 {
        nan_counter.fetch_add(nans, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn audio_callback_f32(
    data: &mut [f32],
    buffer: &FixedFrameBuffer,
    paused: &AtomicBool,
    in_callback: &AtomicBool,
    underruns: &AtomicU32,
    channels: usize,
    clip_counter: &AtomicU32,
    nan_counter: &AtomicU32,
    _converter: &mut AudioFormatConverter,
) {
    let _guard = CallbackGuard::new(in_callback);
    if paused.load(Ordering::Acquire) {
        data.fill(0.0);
        return;
    }
    if channels == 0 {
        data.fill(0.0);
        return;
    }

    if channels == 2 {
        let got = buffer.pop_block_interleaved(data);
        if got < data.len() {
            data[got..].fill(0.0);
            underruns.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        let got_frames = buffer.pop_frames_interleaved(data, channels);
        let got_samples = got_frames.saturating_mul(channels);
        if got_samples < data.len() {
            data[got_samples..].fill(0.0);
            underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    for sample in data.iter_mut() {
        *sample = clamp_and_count(*sample, clip_counter, nan_counter);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn audio_callback_i16(
    data: &mut [i16],
    buffer: &FixedFrameBuffer,
    paused: &AtomicBool,
    in_callback: &AtomicBool,
    underruns: &AtomicU32,
    channels: usize,
    scratch_buffer: &mut [f32],
    converter: &mut AudioFormatConverter,
    dither_enabled: &AtomicBool,
    clip_counter: &AtomicU32,
    nan_counter: &AtomicU32,
) {
    let _guard = CallbackGuard::new(in_callback);
    if paused.load(Ordering::Acquire) {
        data.fill(0);
        return;
    }
    if channels == 0 {
        data.fill(0);
        return;
    }

    let dither_active = dither_enabled.load(Ordering::Relaxed);
    converter.set_dither_enabled(dither_active);

    let mut underrun_flag = false;
    if channels == 2 {
        let total_samples = data.len();
        debug_assert!(
            scratch_buffer.len() >= total_samples,
            "CPAL scratch buffer must cover the negotiated callback size"
        );
        if scratch_buffer.len() < total_samples {
            log::error!("Scratch buffer too small, audio glitch expected");
            data.fill(0);
            return;
        }
        let scratch = &mut scratch_buffer[..total_samples];
        let got = buffer.pop_block_interleaved(scratch);
        if got < total_samples {
            scratch[got..].fill(0.0);
            underrun_flag = true;
        }

        convert_stereo_integer_block(
            data,
            scratch,
            converter,
            clip_counter,
            nan_counter,
            0i16,
            |c, left, right| c.convert_stereo_to_i16(left, right),
            |c, sample| c.convert_mono_to_i16(sample),
        );
    } else {
        debug_assert!(
            scratch_buffer.len() >= data.len(),
            "CPAL scratch buffer must cover the multichannel callback"
        );
        if scratch_buffer.len() < data.len() {
            data.fill(0);
            underruns.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let scratch = &mut scratch_buffer[..data.len()];
        let got_frames = buffer.pop_frames_interleaved(scratch, channels);
        let got_samples = got_frames.saturating_mul(channels);
        if got_samples < scratch.len() {
            scratch[got_samples..].fill(0.0);
            underrun_flag = true;
        }
        for (sample, &val) in data.iter_mut().zip(scratch.iter()) {
            if !val.is_finite() {
                nan_counter.fetch_add(1, Ordering::Relaxed);
                *sample = 0;
            } else {
                if !(-1.0..=1.0).contains(&val) {
                    clip_counter.fetch_add(1, Ordering::Relaxed);
                }
                *sample = converter.convert_mono_to_i16(val);
            }
        }
    }
    if underrun_flag {
        underruns.fetch_add(1, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn audio_callback_i32(
    data: &mut [i32],
    buffer: &FixedFrameBuffer,
    paused: &AtomicBool,
    in_callback: &AtomicBool,
    underruns: &AtomicU32,
    channels: usize,
    scratch_buffer: &mut [f32],
    converter: &mut AudioFormatConverter,
    dither_enabled: &AtomicBool,
    clip_counter: &AtomicU32,
    nan_counter: &AtomicU32,
) {
    let _guard = CallbackGuard::new(in_callback);
    if paused.load(Ordering::Acquire) {
        data.fill(0);
        return;
    }
    if channels == 0 {
        data.fill(0);
        return;
    }

    let dither_active = dither_enabled.load(Ordering::Relaxed);
    converter.set_dither_enabled(dither_active);

    let mut underrun_flag = false;
    if channels == 2 {
        let total_samples = data.len();
        debug_assert!(
            scratch_buffer.len() >= total_samples,
            "CPAL scratch buffer must cover the negotiated callback size"
        );
        if scratch_buffer.len() < total_samples {
            log::error!("Scratch buffer too small, audio glitch expected");
            data.fill(0);
            return;
        }
        let scratch = &mut scratch_buffer[..total_samples];
        let got = buffer.pop_block_interleaved(scratch);
        if got < total_samples {
            scratch[got..].fill(0.0);
            underrun_flag = true;
        }

        convert_stereo_integer_block(
            data,
            scratch,
            converter,
            clip_counter,
            nan_counter,
            0i32,
            |c, left, right| c.convert_stereo_to_i32(left, right),
            |c, sample| c.convert_mono_to_i32(sample),
        );
    } else {
        debug_assert!(
            scratch_buffer.len() >= data.len(),
            "CPAL scratch buffer must cover the multichannel callback"
        );
        if scratch_buffer.len() < data.len() {
            data.fill(0);
            underruns.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let scratch = &mut scratch_buffer[..data.len()];
        let got_frames = buffer.pop_frames_interleaved(scratch, channels);
        let got_samples = got_frames.saturating_mul(channels);
        if got_samples < scratch.len() {
            scratch[got_samples..].fill(0.0);
            underrun_flag = true;
        }
        for (sample, &val) in data.iter_mut().zip(scratch.iter()) {
            if !val.is_finite() {
                nan_counter.fetch_add(1, Ordering::Relaxed);
                *sample = 0;
            } else {
                if !(-1.0..=1.0).contains(&val) {
                    clip_counter.fetch_add(1, Ordering::Relaxed);
                }
                *sample = converter.convert_mono_to_i32(val);
            }
        }
    }
    if underrun_flag {
        underruns.fetch_add(1, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn audio_callback_f64(
    data: &mut [f64],
    buffer: &FixedFrameBuffer,
    paused: &AtomicBool,
    in_callback: &AtomicBool,
    underruns: &AtomicU32,
    channels: usize,
    scratch_buffer: &mut [f32],
    converter: &mut AudioFormatConverter,
    clip_counter: &AtomicU32,
    nan_counter: &AtomicU32,
) {
    let _guard = CallbackGuard::new(in_callback);
    if paused.load(Ordering::Acquire) {
        data.fill(0.0);
        return;
    }
    if channels == 0 {
        data.fill(0.0);
        return;
    }

    let mut underrun_flag = false;
    if channels == 2 {
        let total_samples = data.len();
        debug_assert!(
            scratch_buffer.len() >= total_samples,
            "CPAL f64 scratch buffer must cover the negotiated callback size"
        );
        if scratch_buffer.len() < total_samples {
            data.fill(0.0);
            underruns.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let scratch = &mut scratch_buffer[..total_samples];
        let got = buffer.pop_block_interleaved(scratch);
        if got < total_samples {
            scratch[got..].fill(0.0);
            underrun_flag = true;
        }

        let mut i = 0;
        while i + 1 < data.len() && i + 1 < scratch.len() {
            let l_in = scratch[i];
            let r_in = scratch[i + 1];
            let left_finite = l_in.is_finite();
            let right_finite = r_in.is_finite();
            if left_finite && right_finite {
                if !(-1.0..=1.0).contains(&l_in) {
                    clip_counter.fetch_add(1, Ordering::Relaxed);
                }
                if !(-1.0..=1.0).contains(&r_in) {
                    clip_counter.fetch_add(1, Ordering::Relaxed);
                }
                let (l64, r64) = converter.convert_stereo_to_f64(l_in, r_in);
                data[i] = l64;
                data[i + 1] = r64;
            } else {
                if !left_finite {
                    nan_counter.fetch_add(1, Ordering::Relaxed);
                    data[i] = 0.0;
                } else {
                    if !(-1.0..=1.0).contains(&l_in) {
                        clip_counter.fetch_add(1, Ordering::Relaxed);
                    }
                    data[i] = converter.convert_mono_to_f64(l_in);
                }
                if !right_finite {
                    nan_counter.fetch_add(1, Ordering::Relaxed);
                    data[i + 1] = 0.0;
                } else {
                    if !(-1.0..=1.0).contains(&r_in) {
                        clip_counter.fetch_add(1, Ordering::Relaxed);
                    }
                    data[i + 1] = converter.convert_mono_to_f64(r_in);
                }
            }
            i += 2;
        }
        if i < data.len() && i < scratch.len() {
            let src = scratch[i];
            if !src.is_finite() {
                nan_counter.fetch_add(1, Ordering::Relaxed);
                data[i] = 0.0;
            } else {
                if !(-1.0..=1.0).contains(&src) {
                    clip_counter.fetch_add(1, Ordering::Relaxed);
                }
                data[i] = converter.convert_mono_to_f64(src);
            }
        }
    } else {
        debug_assert!(
            scratch_buffer.len() >= data.len(),
            "CPAL scratch buffer must cover the multichannel callback"
        );
        if scratch_buffer.len() < data.len() {
            data.fill(0.0);
            underruns.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let scratch = &mut scratch_buffer[..data.len()];
        let got_frames = buffer.pop_frames_interleaved(scratch, channels);
        let got_samples = got_frames.saturating_mul(channels);
        if got_samples < scratch.len() {
            scratch[got_samples..].fill(0.0);
            underrun_flag = true;
        }
        for (sample, &val) in data.iter_mut().zip(scratch.iter()) {
            if !val.is_finite() {
                nan_counter.fetch_add(1, Ordering::Relaxed);
                *sample = 0.0;
            } else {
                if !(-1.0..=1.0).contains(&val) {
                    clip_counter.fetch_add(1, Ordering::Relaxed);
                }
                *sample = converter.convert_mono_to_f64(val);
            }
        }
    }
    if underrun_flag {
        underruns.fetch_add(1, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn audio_callback_u16(
    data: &mut [u16],
    buffer: &FixedFrameBuffer,
    paused: &AtomicBool,
    in_callback: &AtomicBool,
    underruns: &AtomicU32,
    channels: usize,
    scratch_buffer: &mut [f32],
    converter: &mut AudioFormatConverter,
    dither_enabled: &AtomicBool,
    clip_counter: &AtomicU32,
    nan_counter: &AtomicU32,
) {
    let _guard = CallbackGuard::new(in_callback);
    if paused.load(Ordering::Acquire) {
        data.fill(32768);
        return;
    }
    if channels == 0 {
        data.fill(32768);
        return;
    }

    let dither_active = dither_enabled.load(Ordering::Relaxed);
    converter.set_dither_enabled(dither_active);

    let mut underrun_flag = false;
    if channels == 2 {
        let total_samples = data.len();
        debug_assert!(
            scratch_buffer.len() >= total_samples,
            "CPAL scratch buffer must cover the negotiated callback size"
        );
        if scratch_buffer.len() < total_samples {
            log::error!("Scratch buffer too small, audio glitch expected");
            data.fill(32768);
            return;
        }
        let scratch = &mut scratch_buffer[..total_samples];
        let got = buffer.pop_block_interleaved(scratch);
        if got < total_samples {
            scratch[got..].fill(0.0);
            underrun_flag = true;
        }

        convert_stereo_integer_block(
            data,
            scratch,
            converter,
            clip_counter,
            nan_counter,
            32768u16,
            |c, left, right| c.convert_stereo_to_u16(left, right),
            |c, sample| c.convert_mono_to_u16(sample),
        );
    } else {
        debug_assert!(
            scratch_buffer.len() >= data.len(),
            "CPAL scratch buffer must cover the multichannel callback"
        );
        if scratch_buffer.len() < data.len() {
            data.fill(32768);
            underruns.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let scratch = &mut scratch_buffer[..data.len()];
        let got_frames = buffer.pop_frames_interleaved(scratch, channels);
        let got_samples = got_frames.saturating_mul(channels);
        if got_samples < scratch.len() {
            scratch[got_samples..].fill(0.0);
            underrun_flag = true;
        }
        for (sample, &val) in data.iter_mut().zip(scratch.iter()) {
            if !val.is_finite() {
                nan_counter.fetch_add(1, Ordering::Relaxed);
                *sample = 32768;
            } else {
                if !(-1.0..=1.0).contains(&val) {
                    clip_counter.fetch_add(1, Ordering::Relaxed);
                }
                *sample = converter.convert_mono_to_u16(val);
            }
        }
    }
    if underrun_flag {
        underruns.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::dither::DitherType;
    use crate::output::format_converter::TargetFormat;

    fn callback_state() -> (AtomicBool, AtomicU32, AtomicU32, AtomicU32) {
        (
            AtomicBool::new(false),
            AtomicU32::new(0),
            AtomicU32::new(0),
            AtomicU32::new(0),
        )
    }

    #[test]
    fn stereo_integer_kernel_batches_diagnostics() {
        let buffer = FixedFrameBuffer::new(8).unwrap();
        buffer.push_block_interleaved(&[2.0, -2.0, f32::NAN, 0.25, f32::NAN, f32::INFINITY]);
        let (in_callback, underruns, clips, nans) = callback_state();
        let mut output = [0i16; 6];
        let mut scratch = [0.0f32; 6];
        let mut converter = AudioFormatConverter::new(TargetFormat::I16, DitherType::None);
        let dither = AtomicBool::new(false);

        audio_callback_i16(
            &mut output,
            &buffer,
            &AtomicBool::new(false),
            &in_callback,
            &underruns,
            2,
            &mut scratch,
            &mut converter,
            &dither,
            &clips,
            &nans,
        );

        assert!(output[0] > 0 && output[1] < 0);
        assert_eq!(output[2], 0);
        assert!(output[3] > 0, "a valid lane must survive a neighboring NaN");
        assert_eq!(output[4], 0);
        assert_eq!(output[5], 0);
        assert_eq!(clips.load(Ordering::Relaxed), 2);
        assert_eq!(nans.load(Ordering::Relaxed), 3);
        assert_eq!(underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn multichannel_callback_reads_complete_frames_in_bulk() {
        let buffer = FixedFrameBuffer::new(8).unwrap();
        let input: [f32; 8] = [0.1, 0.2, 0.3, 0.4, -0.1, -0.2, -0.3, -0.4];
        assert_eq!(buffer.push_frames_interleaved(&input, 4), 2);
        let (in_callback, underruns, clips, nans) = callback_state();
        let mut output = [0i16; 8];
        let mut scratch = [0.0f32; 8];
        let mut converter = AudioFormatConverter::new(TargetFormat::I16, DitherType::None);
        let dither = AtomicBool::new(false);

        audio_callback_i16(
            &mut output,
            &buffer,
            &AtomicBool::new(false),
            &in_callback,
            &underruns,
            4,
            &mut scratch,
            &mut converter,
            &dither,
            &clips,
            &nans,
        );

        assert!(output[0] > 0 && output[2] > output[0]);
        assert!(output[4] < 0 && output[6] < output[4]);
        assert_eq!(underruns.load(Ordering::Relaxed), 0);
        assert_eq!(clips.load(Ordering::Relaxed), 0);
        assert_eq!(nans.load(Ordering::Relaxed), 0);
    }
}
