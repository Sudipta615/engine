//! Shared utilities, constants, and helpers for the decode loop.
//!
//! Used by both the single-stream path (`single.rs`) and the
//! crossfade-transition path (`crossfade.rs`).

use super::AudioEngine;
#[cfg(feature = "resample")]
use crate::dsp::resampler::GenericResampler;

use super::{MAX_PENDING_OUTPUT_FRAMES, MIX_BLOCK_FRAMES};

/// Block size for the post-mix chain during crossfade transitions.
/// The mixer stays per-frame (stateful), but mixed frames are collected
/// into blocks of this size before the post-mix chain runs over them.
pub(super) const MIX_BLOCK: usize = MIX_BLOCK_FRAMES;

/// Number of source frames fed per fill iteration during a crossfade. Small
/// enough that each resampler's internal output buffer is drained frequently
/// (so it can never overflow), large enough to amortize the per-frame
/// pre-mix dispatch.
pub(super) const SOURCE_FEED_BATCH: usize = 256;

/// Consecutive non-EOS decode errors after which playback halts. Shared by
/// the single-stream and crossfade paths so a corrupt track trips the same
/// circuit breaker whether or not a transition is in progress.
pub(super) const DECODE_ERROR_THRESHOLD: u32 = 50;

/// Extract one stereo frame from an interleaved source chunk, applying the
/// same mono/multichannel semantics as the single-stream path.
#[inline]
pub(crate) fn extract_stereo_frame(
    samples: &[f32],
    channels: usize,
    layout: Option<&crate::decode::ChannelLayout>,
    idx: usize,
) -> Option<(f32, f32)> {
    if idx + channels > samples.len() {
        return None;
    }
    match channels {
        1 => {
            let s = samples[idx];
            Some((s, s))
        }
        2 => Some((samples[idx], samples[idx + 1])),
        _ => {
            use crate::decode::ChannelId;
            let fl_i = layout
                .and_then(|l| l.position_of(ChannelId::FrontLeft))
                .unwrap_or(0);
            let fr_i = layout
                .and_then(|l| l.position_of(ChannelId::FrontRight))
                .unwrap_or(1);
            let c_i = layout.and_then(|l| l.position_of(ChannelId::Center));
            let sl_i = layout.and_then(|l| l.position_of(ChannelId::SideLeft));
            let sr_i = layout.and_then(|l| l.position_of(ChannelId::SideRight));
            let fl = samples.get(idx + fl_i).copied().unwrap_or(0.0);
            let fr = samples.get(idx + fr_i).copied().unwrap_or(0.0);
            let c = c_i
                .and_then(|i| samples.get(idx + i))
                .copied()
                .unwrap_or(0.0);
            let sl = sl_i
                .and_then(|i| samples.get(idx + i))
                .copied()
                .unwrap_or(0.0);
            let sr = sr_i
                .and_then(|i| samples.get(idx + i))
                .copied()
                .unwrap_or(0.0);
            let sq = std::f32::consts::FRAC_1_SQRT_2;
            Some((fl + sq * c + sq * sl, fr + sq * c + sq * sr))
        }
    }
}

#[inline]
pub(super) fn push_bounded_fifo(
    fifo: &mut std::collections::VecDeque<(f32, f32)>,
    frame: (f32, f32),
    name: &'static str,
) -> bool {
    debug_assert!(
        fifo.len() < fifo.capacity(),
        "{} exceeded its preallocated realtime capacity",
        name
    );
    if fifo.len() < fifo.capacity() {
        fifo.push_back(frame);
        true
    } else {
        // This is an invariant failure, not a reason to let VecDeque grow on
        // the realtime path. The normal sizing contract makes this branch
        // unreachable; retain the bound in release builds as a last resort.
        log::error!(
            "{} reached its realtime capacity; preserving the bound",
            name
        );
        false
    }
}

#[cfg(feature = "resample")]
#[inline]
pub(super) fn drain_resampler(
    resampler: &mut Option<GenericResampler>,
    scratch: &mut std::collections::VecDeque<(f32, f32)>,
) {
    if let Some(r) = resampler {
        // Do not read one frame past the FIFO bound: leaving it in the
        // resampler is lossless and avoids turning an invariant violation
        // into a dropped sample.
        while scratch.len() < scratch.capacity() {
            let Some((l, rv)) = r.read_f32() else { break };
            let _ = push_bounded_fifo(scratch, (l, rv), "crossfade resampler FIFO");
        }
        debug_assert!(
            scratch.len() <= scratch.capacity(),
            "crossfade resampler FIFO exceeded its preallocated capacity"
        );
    }
}

#[cfg(feature = "resample")]
#[inline]
pub(super) fn feed_resampled_frame(
    resampler: &mut Option<GenericResampler>,
    scratch: &mut std::collections::VecDeque<(f32, f32)>,
    precision: config::PrecisionMode,
    left: f32,
    right: f32,
) {
    match resampler {
        Some(r) => {
            if precision == config::PrecisionMode::Quality {
                r.feed_f64(left as f64, right as f64);
            } else {
                r.feed_f32(left, right);
            }
            drain_resampler(resampler, scratch);
        }
        None => {
            let _ = push_bounded_fifo(scratch, (left, right), "crossfade resampler FIFO");
        }
    }
}

pub(crate) fn push_pending_back_bounded(
    fifo: &mut std::collections::VecDeque<(f32, f32)>,
    frame: (f32, f32),
) {
    debug_assert!(
        fifo.len() <= fifo.capacity(),
        "pending output FIFO exceeded its preallocated capacity"
    );
    if fifo.len() < MAX_PENDING_OUTPUT_FRAMES && fifo.len() < fifo.capacity() {
        fifo.push_back(frame);
    } else {
        log::warn!("pending output buffer is full; preserving the bound");
    }
}

impl AudioEngine {
    #[inline]
    pub(super) fn debug_assert_realtime_buffers(&self) {
        debug_assert!(self.scratch.rs_out_buf.len() <= self.scratch.rs_out_buf.capacity());
        debug_assert!(self.scratch.rs_in_buf.len() <= self.scratch.rs_in_buf.capacity());
        debug_assert!(
            self.scratch.pending_output_frames.len()
                <= self.scratch.pending_output_frames.capacity()
                && self.scratch.pending_output_frames.capacity() >= MAX_PENDING_OUTPUT_FRAMES
        );
        debug_assert!(
            self.scratch.pending_multichannel.len() <= self.scratch.pending_multichannel.capacity()
        );
        debug_assert!(
            self.scratch.mix_l.len() <= self.scratch.mix_l.capacity()
                && self.scratch.mix_l.capacity() >= MIX_BLOCK_FRAMES
        );
        debug_assert!(
            self.scratch.mix_r.len() <= self.scratch.mix_r.capacity()
                && self.scratch.mix_r.capacity() >= MIX_BLOCK_FRAMES
        );
        debug_assert!(
            self.scratch.mix_in_l.len() <= self.scratch.mix_in_l.capacity()
                && self.scratch.mix_in_l.capacity() >= MIX_BLOCK_FRAMES
        );
        debug_assert!(
            self.scratch.mix_in_r.len() <= self.scratch.mix_in_r.capacity()
                && self.scratch.mix_in_r.capacity() >= MIX_BLOCK_FRAMES
        );
        debug_assert_eq!(self.scratch.mix_l.len(), self.scratch.mix_r.len());
        debug_assert_eq!(self.scratch.mix_in_l.len(), self.scratch.mix_in_r.len());
        debug_assert_eq!(self.scratch.mix_l.len(), self.scratch.mix_in_l.len());
    }

    #[inline]
    pub(super) fn push_pending_front(&mut self, frame: (f32, f32)) {
        debug_assert!(
            self.scratch.pending_output_frames.len()
                <= self.scratch.pending_output_frames.capacity(),
            "pending output FIFO exceeded its preallocated capacity"
        );
        if self.scratch.pending_output_frames.len() < MAX_PENDING_OUTPUT_FRAMES
            && self.scratch.pending_output_frames.len()
                < self.scratch.pending_output_frames.capacity()
        {
            self.scratch.pending_output_frames.push_front(frame);
        } else {
            log::warn!("pending output buffer is full; preserving the bound");
        }
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn push_pending_back(&mut self, frame: (f32, f32)) {
        push_pending_back_bounded(&mut self.scratch.pending_output_frames, frame);
    }

    #[inline]
    pub(crate) fn push_crossfade_out(&mut self, frame: (f32, f32)) {
        let _ = push_bounded_fifo(
            &mut self.scratch.rs_out_buf,
            frame,
            "outgoing crossfade FIFO",
        );
    }

    #[inline]
    pub(crate) fn push_crossfade_in(&mut self, frame: (f32, f32)) {
        let _ = push_bounded_fifo(
            &mut self.scratch.rs_in_buf,
            frame,
            "incoming crossfade FIFO",
        );
    }

    #[inline]
    pub(crate) fn push_mix_frame(&mut self, left: f32, right: f32, in_left: f32, in_right: f32) {
        debug_assert_eq!(
            self.scratch.mix_l.len(),
            self.scratch.mix_r.len(),
            "mixed realtime FIFOs must stay in lockstep"
        );
        debug_assert!(
            self.scratch.mix_l.len() < self.scratch.mix_l.capacity()
                && self.scratch.mix_r.len() < self.scratch.mix_r.capacity(),
            "mixed realtime FIFO exceeded its preallocated capacity"
        );
        if self.scratch.mix_l.len() < self.scratch.mix_l.capacity()
            && self.scratch.mix_r.len() < self.scratch.mix_r.capacity()
        {
            self.scratch.mix_l.push(left);
            self.scratch.mix_r.push(right);
            self.scratch.mix_in_l.push(in_left);
            self.scratch.mix_in_r.push(in_right);
        } else {
            log::error!("mixed realtime FIFO reached its preallocated capacity");
        }
    }

    /// Flush the final safety limiter's lookahead tail into the output ring
    /// buffer at end-of-stream, so the final `lookahead` output-domain samples
    /// are not stranded in the limiter's delay line.
    pub(super) fn flush_final_limiter_tail(&mut self) {
        let tail = self.graph.flush_final_limiter();
        if tail.is_empty() {
            return;
        }
        const CHUNK: usize = 256;
        let mut batch = [0.0f32; CHUNK * 2];
        for chunk in tail.chunks(CHUNK) {
            for (i, (l, r)) in chunk.iter().enumerate() {
                batch[i * 2] = *l;
                batch[i * 2 + 1] = *r;
            }
            let _ = self.push_to_sink(&batch[..chunk.len() * 2], 2);
        }
    }

    /// Flush the resampler's final partial input block and push the
    /// recovered output-domain frames through the final safety limiter into
    /// the output ring, so the tail of the track is not dropped.
    ///
    /// The resampler processes input in fixed-size blocks (1024 frames for
    /// rubato's Fft resampler at the default quality); any remainder below that
    /// size is only emitted when [`GenericResampler::flush`] is called, and
    /// single-track playback never called it, so the final partial block was
    /// silently lost. The crossfade path already flushes via
    /// `fill_outgoing_side` / `fill_incoming_side`; this closes the same gap
    /// for ordinary playback, and it is also used by the rate-changing
    /// gapless handoff to complete the outgoing track before its resampler
    /// is rebuilt for the next track's rate.
    #[cfg(feature = "resample")]
    pub(crate) fn flush_resampler_tail(&mut self, resampler: &mut Option<GenericResampler>) {
        let Some(r) = resampler else { return };
        if r.is_disabled() {
            return;
        }
        r.flush();
        // The flushed tail (final partial block + filter delay) is in the
        // output domain, so it must pass through the final safety limiter
        // before reaching the ring. The limiter's own lookahead tail is
        // flushed separately afterwards by `flush_final_limiter_tail`.
        const CHUNK: usize = 256;
        let mut batch = [0.0f32; CHUNK * 2];
        let mut collected = 0usize;
        while let Some((out_l, out_r)) = r.read_f32() {
            let (l, rr) = self.graph.process_final_limiter(out_l, out_r);
            batch[collected * 2] = l;
            batch[collected * 2 + 1] = rr;
            collected += 1;
            if collected == CHUNK {
                let _ = self.push_to_sink(&batch, 2);
                collected = 0;
            }
        }
        if collected > 0 {
            let _ = self.push_to_sink(&batch[..collected * 2], 2);
        }
    }
}
