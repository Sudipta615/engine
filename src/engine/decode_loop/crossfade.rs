//! Crossfade-transition decode and process path.

use std::sync::Arc;

use log::warn;

#[cfg(feature = "resample")]
use super::common::{drain_resampler, feed_resampled_frame};
use super::common::{extract_stereo_frame, DECODE_ERROR_THRESHOLD, MIX_BLOCK, SOURCE_FEED_BATCH};
use super::AudioEngine;
use super::MAX_PENDING_OUTPUT_FRAMES;
#[cfg(feature = "resample")]
use crate::dsp::resampler::GenericResampler;
use crate::{
    buffer::{PlaybackInfo, PlaybackState, MAX_AUDIO_BLOCK_FRAMES},
    decode::{DecodeError, Decoder},
};

impl AudioEngine {
    /// Handle a non-EOS decode failure on one side of a crossfade/fade
    /// transition.
    ///
    /// Mirrors the single-stream error path — log the error and count toward
    /// the consecutive-error circuit breaker — instead of silently converting
    /// corruption/I-O/decode failures into end-of-stream silence. The caller
    /// still treats the failing side as ended *for the current tick* so a
    /// single transient error does not tear down the transition, but repeated
    /// failures halt playback exactly like the single-stream path.
    ///
    /// Returns `true` when the breaker has tripped and the caller must abort
    /// the current tick.
    fn handle_transition_decode_error(&mut self, e: DecodeError) -> bool {
        self.recovery.consecutive_decode_errors += 1;
        warn!(
            "Crossfade decode error ({}/{}): {}",
            self.recovery.consecutive_decode_errors, DECODE_ERROR_THRESHOLD, e
        );
        if self.recovery.consecutive_decode_errors >= DECODE_ERROR_THRESHOLD {
            warn!("Too many consecutive decode errors; stopping playback");
            self.update_playback_state(PlaybackState::Stopped);
            self.stream_ended = true;
            true
        } else {
            false
        }
    }

    /// Decode and process during a crossfade transition.
    ///
    /// The two decoders and resamplers live in **source-frame space**, while
    /// the crossfade counter and mixer live in **output-frame space**. This
    /// function makes that boundary explicit: each stream advances its own
    /// source position, its resampler output is drained into an output-domain
    /// FIFO, and those FIFOs are mixed one pair at a time with the crossfade
    /// counter decremented once per output frame. Neither resampler's output
    /// is ever discarded.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn decode_transitioning_stream(
        &mut self,
        outgoing_decoder: &mut Decoder,
        #[cfg(feature = "resample")] outgoing_resampler: &mut Option<GenericResampler>,
        #[cfg(not(feature = "resample"))] outgoing_resampler: &mut Option<()>,
        incoming_decoder: &mut Decoder,
        #[cfg(feature = "resample")] incoming_resampler: &mut Option<GenericResampler>,
        #[cfg(not(feature = "resample"))] incoming_resampler: &mut Option<()>,
        crossfade_frames_remaining: &mut usize,
        crossfade_total_frames: usize,
    ) {
        // ── Drain pending output frames before processing new frames ──
        loop {
            let len = self.scratch.pending_output_frames.len();
            if len == 0 {
                break;
            }
            const DRAIN_BATCH: usize = 256;
            let n = len.min(DRAIN_BATCH);
            let mut stereo_buf = [0.0f32; DRAIN_BATCH * 2];
            for i in 0..n {
                let (l, r) = self.scratch.pending_output_frames[i];
                stereo_buf[i * 2] = l;
                stereo_buf[i * 2 + 1] = r;
            }
            let frames_written = self.push_to_sink(&stereo_buf[..n * 2], 2);
            if frames_written > 0 {
                self.scratch.pending_output_frames.drain(..frames_written);
            }
            if frames_written < n {
                return;
            }
        }
        if self.scratch.pending_output_frames.len() >= MAX_PENDING_OUTPUT_FRAMES {
            return;
        }

        // ── Decode source chunks from both decoders ──
        //
        // In Fade-transition mode the incoming track must not start until the
        // fade-in phase (the final third of the transition window), so its
        // decoder is left untouched — and the incoming side is treated as
        // not-yet-started — until then. This keeps the next track's head
        // aligned with its own sample 0 at the fade-in, matching the
        // documented Fade semantics (fade-out → silence gap → fade-in).
        let is_fade = self.config.transition_mode == config::TransitionMode::Fade;
        let fade_total = crossfade_total_frames.max(1);
        // Fade envelope: [0, total/3) fade-out, [total/3, 2·total/3) silence
        // gap, [2·total/3, total) fade-in (the mixer's Fading state applies
        // the same thirds).
        let fade_in_start_frame = fade_total.saturating_mul(2) / 3;
        let elapsed_frames = fade_total.saturating_sub(*crossfade_frames_remaining);
        // Whether the incoming decoder is live at the start of this tick:
        // always in a crossfade; in a fade only once the fade-in phase has
        // begun (so the incoming track starts from its own sample 0).
        let incoming_live_at_tick_start = !is_fade || elapsed_frames >= fade_in_start_frame;

        let (out_chunk, out_start_idx): (Option<crate::decode::DecodedChunk>, usize) =
            match self.scratch.pending_chunk.take() {
                Some((c, start)) => (Some(c), start),
                None => match outgoing_decoder.decode_next(MAX_AUDIO_BLOCK_FRAMES) {
                    Ok(c) => (Some(c), 0),
                    Err(DecodeError::EndOfStream) => {
                        // Outgoing track ended — the mixer will fade its tail
                        // (and then silence) against the incoming track.
                        (None, 0)
                    }
                    Err(e) => {
                        // Corruption / I/O failure on the outgoing side: route
                        // it through the engine's normal error path (log +
                        // consecutive-error circuit breaker) instead of
                        // silently treating it as end-of-stream. For this tick
                        // the side is treated as ended so the transition can
                        // still complete, but repeated failures halt playback.
                        if self.handle_transition_decode_error(e) {
                            return;
                        }
                        (None, 0)
                    }
                },
            };

        let (in_chunk, in_start_idx): (Option<crate::decode::DecodedChunk>, usize) =
            if incoming_live_at_tick_start {
                match self.scratch.pending_incoming_chunk.take() {
                    Some((c, start)) => (Some(c), start),
                    None => match incoming_decoder.decode_next(MAX_AUDIO_BLOCK_FRAMES) {
                        Ok(c) => (Some(c), 0),
                        Err(DecodeError::EndOfStream) => {
                            // Incoming track ended during crossfade — the fade
                            // completes into silence instead.
                            (None, 0)
                        }
                        Err(e) => {
                            if self.handle_transition_decode_error(e) {
                                return;
                            }
                            (None, 0)
                        }
                    },
                }
            } else {
                // Fade transition, pre-fade-in: the incoming track has not
                // started yet, so no chunk is produced or consumed.
                (None, 0)
            };

        // A stream that returned no chunk this tick has reached EOS (or has
        // not started, for a gated fade-in).
        let outgoing_eos = out_chunk.is_none();
        let incoming_eos = in_chunk.is_none();

        let out_samples = out_chunk
            .as_ref()
            .map(|c| c.samples.as_slice())
            .unwrap_or(&[]);
        let out_channels = out_chunk.as_ref().map(|c| c.channels).unwrap_or(2).max(1);
        let out_frame_count_total = out_chunk.as_ref().map(|c| c.frame_count).unwrap_or(0);
        let out_layout = out_chunk.as_ref().map(|c| &c.channel_layout);

        let in_samples = in_chunk
            .as_ref()
            .map(|c| c.samples.as_slice())
            .unwrap_or(&[]);
        let in_channels = in_chunk.as_ref().map(|c| c.channels).unwrap_or(2).max(1);
        let in_frame_count_total = in_chunk.as_ref().map(|c| c.frame_count).unwrap_or(0);
        let in_layout = in_chunk.as_ref().map(|c| &c.channel_layout);

        // Source-domain positions (interleaved sample indices). These advance
        // only while feeding the resamplers — they never share a clock with
        // the output-domain crossfade counter below.
        let mut out_idx = out_start_idx;
        let mut in_idx = in_start_idx;
        let mut stalled_at: Option<(usize, usize)> = None;

        // ── Output-domain mixing loop ──
        // `rs_out_buf` / `rs_in_buf` are persistent FIFOs: a stream that is
        // momentarily ahead of the other keeps its excess frames here until
        // the other stream catches up (they are never dropped).
        loop {
            if *crossfade_frames_remaining == 0 {
                break;
            }

            // The incoming side is live throughout a normal crossfade, but in
            // Fade mode only once the fade-in phase begins; before that the
            // outgoing side is mixed alone (the mixer's Fading envelope ramps
            // it out, then holds the silence gap).
            let elapsed = fade_total.saturating_sub(*crossfade_frames_remaining);
            let incoming_live = !is_fade || elapsed >= fade_in_start_frame;

            // Fill whichever side is empty. At EOS this synthesizes silence
            // so the mixer can finish the fade (tail fades into silence).
            self.fill_outgoing_side(
                outgoing_resampler,
                out_samples,
                out_channels,
                out_layout,
                &mut out_idx,
                out_frame_count_total,
                outgoing_eos,
            );
            if incoming_live {
                self.fill_incoming_side(
                    incoming_resampler,
                    in_samples,
                    in_channels,
                    in_layout,
                    &mut in_idx,
                    in_frame_count_total,
                    incoming_eos,
                );
            }

            let mix_n = if !incoming_live {
                // Fade pre-fade-in: the incoming side is not started, so the
                // outgoing FIFO alone drives the mix (the mixer envelope
                // mutes the absent incoming contribution).
                self.scratch
                    .rs_out_buf
                    .len()
                    .min(*crossfade_frames_remaining)
            } else {
                self.scratch
                    .rs_out_buf
                    .len()
                    .min(self.scratch.rs_in_buf.len())
                    .min(*crossfade_frames_remaining)
            };
            if mix_n == 0 {
                // One side is a non-EOS stream that has exhausted its current
                // chunk and resampler output — decode more next tick.
                break;
            }

            for _ in 0..mix_n {
                // Recompute per frame so a mid-batch phase boundary flip is
                // honored exactly (the mixer's own envelope is frame-based).
                let elapsed = fade_total.saturating_sub(*crossfade_frames_remaining);
                let incoming_live = !is_fade || elapsed >= fade_in_start_frame;

                let (out_l, out_r) = match self.scratch.rs_out_buf.pop_front() {
                    Some(v) => v,
                    None => break,
                };
                let (in_l, in_r) = if !incoming_live {
                    // Pre-fade-in: no incoming frames exist yet; the mixer's
                    // Fading envelope gives them zero weight anyway.
                    (0.0, 0.0)
                } else {
                    match self.scratch.rs_in_buf.pop_front() {
                        Some(v) => v,
                        None => break,
                    }
                };

                // Phase 3 S3: the graph's mix bus owns the per-stream pre-mix
                // chains, the transition envelope, and the post-mix chain —
                // all applied inside `process_block_inputs` when the block is
                // flushed. Here we only accumulate the raw output-domain
                // frames (outgoing + incoming) in lockstep.
                self.push_mix_frame(out_l, out_r, in_l, in_r);
                *crossfade_frames_remaining = crossfade_frames_remaining.saturating_sub(1);

                if self.scratch.mix_l.len() == MIX_BLOCK
                    && !self.flush_mixed_block(&mut stalled_at, out_idx, in_idx)
                {
                    break;
                }
            }

            // Flush any remaining partial mixed block.
            if !self.scratch.mix_l.is_empty() {
                self.flush_mixed_block(&mut stalled_at, out_idx, in_idx);
            }
            if stalled_at.is_some() {
                break;
            }
        }

        // When the crossfade boundary is reached, any incoming frames still
        // sitting in the FIFO are the start of the incoming track's
        // continuation. They have already passed the incoming pre-mix chain,
        // so they only need the post-mix chain before being emitted — never
        // discard them. (The outgoing leftover can be dropped: its track is
        // over.)
        if *crossfade_frames_remaining == 0 && !self.scratch.rs_in_buf.is_empty() {
            let mut leftover_stall: Option<(usize, usize)> = None;
            // The bus is in PlayingNext now: feed the leftover incoming
            // frames as the SECONDARY stream (the primary contributes
            // nothing), so they pass through the full chain — pre-mix +
            // post-mix — exactly like the promoted single stream.
            while let Some((l, r)) = self.scratch.rs_in_buf.pop_front() {
                self.push_mix_frame(0.0, 0.0, l, r);
                if self.scratch.mix_l.len() == MIX_BLOCK {
                    self.flush_mixed_block(&mut leftover_stall, out_idx, in_idx);
                }
            }
            if !self.scratch.mix_l.is_empty() {
                self.flush_mixed_block(&mut leftover_stall, out_idx, in_idx);
            }
        }
        if *crossfade_frames_remaining == 0 {
            self.scratch.rs_out_buf.clear();
            self.scratch.rs_in_buf.clear();
        }

        // Defensive: the mixed block must start empty on every tick.
        self.scratch.mix_l.clear();
        self.scratch.mix_r.clear();
        self.scratch.mix_in_l.clear();
        self.scratch.mix_in_r.clear();

        if let Some((stall_out_idx, stall_in_idx)) = stalled_at {
            if stall_out_idx < out_samples.len() {
                if let Some(chunk) = out_chunk {
                    self.scratch.pending_chunk = Some((chunk, stall_out_idx));
                }
            }
            // Cache the incoming partial chunk if it still has unprocessed frames.
            if stall_in_idx < in_samples.len() {
                if let Some(chunk) = in_chunk {
                    self.scratch.pending_incoming_chunk = Some((chunk, stall_in_idx));
                }
            }
        }

        // Advance the sample-accurate clock by the OUTGOING source frames
        // consumed this tick. The outgoing track's playhead is the natural
        // continuation of the pre-fade position, so the progress bar keeps
        // advancing smoothly through the transition (at `speed`× wall-clock,
        // exactly like single-track playback); when the transition completes,
        // `decode_and_process` resets the clock to the incoming track.
        //
        // If the outgoing track ends during the fade (e.g. at high speed the
        // fade can outlast the remaining tail), we fall back to the incoming
        // track's playhead — what the user now hears — so the position keeps
        // advancing instead of freezing.
        let outgoing_consumed = (out_idx.saturating_sub(out_start_idx)) / out_channels.max(1);
        if outgoing_consumed > 0 {
            self.clock.advance_source(outgoing_consumed as u64);
        } else if outgoing_eos {
            let incoming_rate = incoming_decoder.info().sample_rate;
            if self.clock.source_sample_rate != incoming_rate {
                self.clock.source_sample_rate = incoming_rate;
            }
            let incoming_consumed = (in_idx.saturating_sub(in_start_idx)) / in_channels.max(1);
            self.clock.advance_source(incoming_consumed as u64);
        }

        let pos = self.clock.position_secs();
        let dur = self.duration_secs;
        let (latency_ms, compensated) = self.latency_compensation(pos);
        self.playback_info.rcu(|old| {
            Arc::new(PlaybackInfo {
                position_secs: pos,
                position_secs_compensated: compensated,
                latency_ms,
                duration_secs: dur,
                ..old.as_ref().clone()
            })
        });
    }

    /// Fill the outgoing output-domain FIFO with resampled frames.
    ///
    /// Feeds source frames (applying the outgoing pre-mix chain) into the
    /// outgoing resampler and drains its output. When the stream has reached
    /// EOS it flushes the resampler tail and then synthesizes silence so the
    /// mixer can complete the fade. Returns with `self.scratch.rs_out_buf` either
    /// non-empty or genuinely unable to make progress this tick.
    #[cfg(feature = "resample")]
    #[allow(clippy::too_many_arguments)]
    fn fill_outgoing_side(
        &mut self,
        resampler: &mut Option<GenericResampler>,
        samples: &[f32],
        channels: usize,
        layout: Option<&crate::decode::ChannelLayout>,
        source_idx: &mut usize,
        source_frame_total: usize,
        eos: bool,
    ) {
        if !self.scratch.rs_out_buf.is_empty() {
            return;
        }

        // Drain any output the resampler produced on a previous tick.
        drain_resampler(resampler, &mut self.scratch.rs_out_buf);
        if !self.scratch.rs_out_buf.is_empty() {
            return;
        }

        loop {
            let consumed_frames = *source_idx / channels.max(1);
            let available = source_frame_total.saturating_sub(consumed_frames);
            if available > 0 {
                let batch = available.min(SOURCE_FEED_BATCH);
                for _ in 0..batch {
                    let Some((l, r)) = extract_stereo_frame(samples, channels, layout, *source_idx)
                    else {
                        break;
                    };
                    *source_idx += channels.max(1);
                    // Phase 3 S3: the source frames feed the resampler RAW —
                    // the per-stream pre-mix (preamp + loudness) moved into
                    // the graph's mix bus and is applied on the resampled
                    // output-domain planes inside `process_block_inputs`.
                    feed_resampled_frame(
                        resampler,
                        &mut self.scratch.rs_out_buf,
                        self.config.precision_mode,
                        l,
                        r,
                    );
                }
                drain_resampler(resampler, &mut self.scratch.rs_out_buf);
                if !self.scratch.rs_out_buf.is_empty() {
                    return;
                }
                continue;
            }

            // No source frames remain this tick.
            if eos {
                // Flush the resampler's final partial block so the outgoing
                // tail is emitted instead of being stranded in the filter.
                if let Some(r) = resampler {
                    r.flush();
                }
                drain_resampler(resampler, &mut self.scratch.rs_out_buf);
                if !self.scratch.rs_out_buf.is_empty() {
                    return;
                }
                // Truly done: synthesize silence so the mixer can finish the
                // fade (the outgoing tail fades into silence).
                self.push_crossfade_out((0.0, 0.0));
            }
            return;
        }
    }

    /// [`Self::fill_outgoing_side`] for the incoming stream.
    #[cfg(feature = "resample")]
    #[allow(clippy::too_many_arguments)]
    fn fill_incoming_side(
        &mut self,
        resampler: &mut Option<GenericResampler>,
        samples: &[f32],
        channels: usize,
        layout: Option<&crate::decode::ChannelLayout>,
        source_idx: &mut usize,
        source_frame_total: usize,
        eos: bool,
    ) {
        if !self.scratch.rs_in_buf.is_empty() {
            return;
        }

        drain_resampler(resampler, &mut self.scratch.rs_in_buf);
        if !self.scratch.rs_in_buf.is_empty() {
            return;
        }

        loop {
            let consumed_frames = *source_idx / channels.max(1);
            let available = source_frame_total.saturating_sub(consumed_frames);
            if available > 0 {
                let batch = available.min(SOURCE_FEED_BATCH);
                for _ in 0..batch {
                    let Some((l, r)) = extract_stereo_frame(samples, channels, layout, *source_idx)
                    else {
                        break;
                    };
                    *source_idx += channels.max(1);
                    // Phase 3 S3: raw source frames — the incoming pre-mix
                    // chain lives in the graph's mix bus (slot 1).
                    feed_resampled_frame(
                        resampler,
                        &mut self.scratch.rs_in_buf,
                        self.config.precision_mode,
                        l,
                        r,
                    );
                }
                drain_resampler(resampler, &mut self.scratch.rs_in_buf);
                if !self.scratch.rs_in_buf.is_empty() {
                    return;
                }
                continue;
            }

            if eos {
                if let Some(r) = resampler {
                    r.flush();
                }
                drain_resampler(resampler, &mut self.scratch.rs_in_buf);
                if !self.scratch.rs_in_buf.is_empty() {
                    return;
                }
                self.push_crossfade_in((0.0, 0.0));
            }
            return;
        }
    }

    /// Non-resample fallback: pass source frames straight through at source
    /// rate (the pre-existing behaviour when the `resample` feature is off).
    #[cfg(not(feature = "resample"))]
    #[allow(clippy::too_many_arguments)]
    fn fill_outgoing_side(
        &mut self,
        _resampler: &mut Option<()>,
        samples: &[f32],
        channels: usize,
        layout: Option<&crate::decode::ChannelLayout>,
        source_idx: &mut usize,
        source_frame_total: usize,
        eos: bool,
    ) {
        if !self.scratch.rs_out_buf.is_empty() {
            return;
        }
        let consumed_frames = *source_idx / channels.max(1);
        let available = source_frame_total.saturating_sub(consumed_frames);
        if available > 0 {
            let batch = available.min(SOURCE_FEED_BATCH);
            for _ in 0..batch {
                let Some((l, r)) = extract_stereo_frame(samples, channels, layout, *source_idx)
                else {
                    break;
                };
                *source_idx += channels.max(1);
                // Phase 3 S3: raw source frames (pre-mix moved into the bus).
                self.push_crossfade_out((l, r));
            }
            return;
        }
        if eos {
            self.push_crossfade_out((0.0, 0.0));
        }
    }

    /// Non-resample fallback for the incoming stream.
    #[cfg(not(feature = "resample"))]
    #[allow(clippy::too_many_arguments)]
    fn fill_incoming_side(
        &mut self,
        _resampler: &mut Option<()>,
        samples: &[f32],
        channels: usize,
        layout: Option<&crate::decode::ChannelLayout>,
        source_idx: &mut usize,
        source_frame_total: usize,
        eos: bool,
    ) {
        if !self.scratch.rs_in_buf.is_empty() {
            return;
        }
        let consumed_frames = *source_idx / channels.max(1);
        let available = source_frame_total.saturating_sub(consumed_frames);
        if available > 0 {
            let batch = available.min(SOURCE_FEED_BATCH);
            for _ in 0..batch {
                let Some((l, r)) = extract_stereo_frame(samples, channels, layout, *source_idx)
                else {
                    break;
                };
                *source_idx += channels.max(1);
                // Phase 3 S3: raw source frames (pre-mix moved into the bus).
                self.push_crossfade_in((l, r));
            }
            return;
        }
        if eos {
            self.push_crossfade_in((0.0, 0.0));
        }
    }

    /// Apply the post-mix chain to the accumulated mixed block and flush it
    /// to the ring buffer. On a partial flush the unwritten frames are
    /// preserved in chronological order at the FRONT of
    /// `pending_output_frames`, and `stalled_at` is set to the given source
    /// positions. Returns true when the whole block was flushed.
    fn flush_mixed_block(
        &mut self,
        stalled_at: &mut Option<(usize, usize)>,
        out_idx: usize,
        in_idx: usize,
    ) -> bool {
        let n = self
            .scratch
            .mix_l
            .len()
            .min(self.scratch.mix_r.len())
            .min(self.scratch.mix_in_l.len())
            .min(self.scratch.mix_in_r.len());
        if n == 0 {
            return true;
        }
        // Phase 3 S3: the graph runs the full chain in one call — per-stream
        // pre-mix chains (bus slots 0/1), the transition envelope, and the
        // post-mix chain — then the output-domain final limiter runs here as
        // before (the block is already in the output domain: both sides were
        // resampled before mixing).
        if self.lanes.is_empty() {
            self.graph.process_block_inputs(
                (&mut self.scratch.mix_l[..n], &mut self.scratch.mix_r[..n]),
                (
                    &mut self.scratch.mix_in_l[..n],
                    &mut self.scratch.mix_in_r[..n],
                ),
            );
        } else {
            // Active lanes (Phase 4 S6) ride after the incoming stream:
            // secondaries[0] is the incoming (slot 1), lanes occupy slots ≥ 2.
            let active = self.fill_lane_scratch(n);
            let mut iter_l = self.scratch.lane_l.iter_mut();
            let mut iter_r = self.scratch.lane_r.iter_mut();
            let mut secondaries: [(&mut [f32], &mut [f32]); crate::engine::lanes::MAX_LANES + 1] =
                std::array::from_fn(|_| {
                    let l: &mut Vec<f32> = iter_l.next().expect("lane planes preallocated");
                    let r: &mut Vec<f32> = iter_r.next().expect("lane planes preallocated");
                    (&mut l[..n], &mut r[..n])
                });
            secondaries[0] = (
                &mut self.scratch.mix_in_l[..n],
                &mut self.scratch.mix_in_r[..n],
            );
            self.graph.process_block_streams(
                (&mut self.scratch.mix_l[..n], &mut self.scratch.mix_r[..n]),
                &mut secondaries[..1 + active],
            );
        }
        self.graph.process_final_limiter_block(
            &mut self.scratch.mix_l[..n],
            &mut self.scratch.mix_r[..n],
        );
        let mut batch = [0.0f32; MIX_BLOCK * 2];
        for i in 0..n {
            batch[i * 2] = self.scratch.mix_l[i];
            batch[i * 2 + 1] = self.scratch.mix_r[i];
        }
        self.scratch.mix_l.clear();
        self.scratch.mix_r.clear();
        self.scratch.mix_in_l.clear();
        self.scratch.mix_in_r.clear();
        let written = self.output_buffer.push_block_interleaved(&batch[..n * 2]);
        let frames_written = written / 2;
        if frames_written < n {
            for i in (frames_written..n).rev() {
                self.push_pending_front((batch[i * 2], batch[i * 2 + 1]));
            }
            if stalled_at.is_none() {
                *stalled_at = Some((out_idx, in_idx));
            }
            false
        } else {
            true
        }
    }
}
