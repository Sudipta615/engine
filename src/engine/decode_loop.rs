//! Core decode-and-process loop for single and crossfade playback modes.

use log::{info, warn};

use std::sync::Arc;

use super::{AudioEngine, PlaybackStream};
#[cfg(feature = "resample")]
use crate::dsp::resampler::GenericResampler;
use crate::{
    buffer::{PlaybackInfo, PlaybackState, MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS},
    decode::{
        mix_interleaved_to_stereo_with_template, mix_interleaved_with_template, ChannelLayout,
        DecodeError, Decoder,
    },
};

/// Block size for the post-mix chain during crossfade transitions.
/// The mixer stays per-frame (stateful), but mixed frames are collected
/// into blocks of this size before the post-mix chain runs over them.
const MIX_BLOCK: usize = super::MIX_BLOCK_FRAMES;

/// Number of source frames fed per fill iteration during a crossfade. Small
/// enough that each resampler's internal output buffer is drained frequently
/// (so it can never overflow), large enough to amortize the per-frame
/// pre-mix dispatch.
const SOURCE_FEED_BATCH: usize = 256;

/// Consecutive non-EOS decode errors after which playback halts. Shared by
/// the single-stream and crossfade paths so a corrupt track trips the same
/// circuit breaker whether or not a transition is in progress.
const DECODE_ERROR_THRESHOLD: u32 = 50;

/// Extract one stereo frame from an interleaved source chunk, applying the
/// same mono/multichannel semantics as the single-stream path.
#[inline]
fn extract_stereo_frame(
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
            Some((fl + 0.7071 * c + 0.7071 * sl, fr + 0.7071 * c + 0.7071 * sr))
        }
    }
}

#[inline]
fn push_bounded_fifo(
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
fn drain_resampler(
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
fn feed_resampled_frame(
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

pub(super) fn push_pending_back_bounded(
    fifo: &mut std::collections::VecDeque<(f32, f32)>,
    frame: (f32, f32),
) {
    debug_assert!(
        fifo.len() <= fifo.capacity(),
        "pending output FIFO exceeded its preallocated capacity"
    );
    if fifo.len() < super::MAX_PENDING_OUTPUT_FRAMES && fifo.len() < fifo.capacity() {
        fifo.push_back(frame);
    } else {
        log::warn!("pending output buffer is full; preserving the bound");
    }
}

impl AudioEngine {
    #[inline]
    fn debug_assert_realtime_buffers(&self) {
        debug_assert!(self.scratch.rs_out_buf.len() <= self.scratch.rs_out_buf.capacity());
        debug_assert!(self.scratch.rs_in_buf.len() <= self.scratch.rs_in_buf.capacity());
        debug_assert!(
            self.scratch.pending_output_frames.len() <= self.scratch.pending_output_frames.capacity()
                && self.scratch.pending_output_frames.capacity() >= super::MAX_PENDING_OUTPUT_FRAMES
        );
        debug_assert!(self.scratch.pending_multichannel.len() <= self.scratch.pending_multichannel.capacity());
        debug_assert!(
            self.scratch.mix_l.len() <= self.scratch.mix_l.capacity()
                && self.scratch.mix_l.capacity() >= super::MIX_BLOCK_FRAMES
        );
        debug_assert!(
            self.scratch.mix_r.len() <= self.scratch.mix_r.capacity()
                && self.scratch.mix_r.capacity() >= super::MIX_BLOCK_FRAMES
        );
        debug_assert_eq!(self.scratch.mix_l.len(), self.scratch.mix_r.len());
    }

    #[inline]
    pub(super) fn push_pending_front(&mut self, frame: (f32, f32)) {
        debug_assert!(
            self.scratch.pending_output_frames.len() <= self.scratch.pending_output_frames.capacity(),
            "pending output FIFO exceeded its preallocated capacity"
        );
        if self.scratch.pending_output_frames.len() < super::MAX_PENDING_OUTPUT_FRAMES
            && self.scratch.pending_output_frames.len() < self.scratch.pending_output_frames.capacity()
        {
            self.scratch.pending_output_frames.push_front(frame);
        } else {
            log::warn!("pending output buffer is full; preserving the bound");
        }
    }

    #[allow(dead_code)]
    #[inline]
    pub(super) fn push_pending_back(&mut self, frame: (f32, f32)) {
        push_pending_back_bounded(&mut self.scratch.pending_output_frames, frame);
    }

    #[inline]
    pub(super) fn push_crossfade_out(&mut self, frame: (f32, f32)) {
        let _ = push_bounded_fifo(&mut self.scratch.rs_out_buf, frame, "outgoing crossfade FIFO");
    }

    #[inline]
    pub(super) fn push_crossfade_in(&mut self, frame: (f32, f32)) {
        let _ = push_bounded_fifo(&mut self.scratch.rs_in_buf, frame, "incoming crossfade FIFO");
    }

    #[inline]
    pub(super) fn push_mix_frame(&mut self, left: f32, right: f32) {
        debug_assert_eq!(
            self.scratch.mix_l.len(),
            self.scratch.mix_r.len(),
            "mixed realtime FIFOs must stay in lockstep"
        );
        debug_assert!(
            self.scratch.mix_l.len() < self.scratch.mix_l.capacity() && self.scratch.mix_r.len() < self.scratch.mix_r.capacity(),
            "mixed realtime FIFO exceeded its preallocated capacity"
        );
        if self.scratch.mix_l.len() < self.scratch.mix_l.capacity() && self.scratch.mix_r.len() < self.scratch.mix_r.capacity() {
            self.scratch.mix_l.push(left);
            self.scratch.mix_r.push(right);
        } else {
            log::error!("mixed realtime FIFO reached its preallocated capacity");
        }
    }

    /// Flush the final safety limiter's lookahead tail into the output ring
    /// buffer at end-of-stream, so the final `lookahead` output-domain samples
    /// are not stranded in the limiter's delay line.
    fn flush_final_limiter_tail(&mut self) {
        let tail = self.pipeline.flush_final_limiter();
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
            let _ = self
                .output_buffer
                .push_block_interleaved(&batch[..chunk.len() * 2]);
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
    pub(super) fn flush_resampler_tail(&mut self, resampler: &mut Option<GenericResampler>) {
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
            let (l, rr) = self.pipeline.process_final_limiter(out_l, out_r);
            batch[collected * 2] = l;
            batch[collected * 2 + 1] = rr;
            collected += 1;
            if collected == CHUNK {
                let _ = self.output_buffer.push_block_interleaved(&batch);
                collected = 0;
            }
        }
        if collected > 0 {
            let _ = self
                .output_buffer
                .push_block_interleaved(&batch[..collected * 2]);
        }
    }

    /// Core decode-and-process loop. Handles both Single and Transitioning
    /// (crossfade) playback modes, feeding distinct sample streams into
    /// the DSP pipeline and TrackMixer.
    pub(super) fn decode_and_process(&mut self) {
        self.debug_assert_realtime_buffers();
        // Check if we need to finalize a completed crossfade transition.
        // We do this by taking the stream, checking the state, and
        // either completing the transition or putting it back.
        let needs_completion = match &self.stream {
            Some(PlaybackStream::Transitioning {
                crossfade_frames_remaining,
                ..
            }) => *crossfade_frames_remaining == 0,
            _ => false,
        };

        if needs_completion {
            if let Some(PlaybackStream::Transitioning {
                incoming_decoder,
                incoming_resampler,
                ..
            }) = self.stream.take()
            {
                info!("Crossfade transition complete; incoming track is now active");
                self.clock.reset_track(incoming_decoder.info().sample_rate);
                self.duration_secs = incoming_decoder.duration_secs();
                self.scratch.crossfade_triggered = false;
                self.recovery.consecutive_decode_errors = 0;
                // The incoming track becomes the current track: promote its
                // path and loudness metadata, and feed the merged metadata to
                // the outgoing chain that single-track playback now uses.
                self.loudness_scan.current_track_path = self.loudness_scan.incoming_track_path.take();
                let incoming_meta = self.loudness_scan.pending_incoming_loudness_metadata.take();
                self.loudness_scan.pending_loudness_metadata = incoming_meta;
                if let Some(meta) = incoming_meta {
                    self.pipeline.apply_loudness_metadata_outgoing(Some(meta));
                }
                self.stream = Some(PlaybackStream::Single {
                    decoder: incoming_decoder,
                    resampler: incoming_resampler,
                });
                self.pipeline.mixer_mut().start_playing();
                self.scratch.pending_chunk = None;
                self.scratch.pending_incoming_chunk = None;
                self.scratch.rs_out_buf.clear();
                self.scratch.rs_in_buf.clear();
                // Re-arm a scan in case the incoming track still lacks EBU
                // R128 metadata (e.g. its scan never ran).
                self.start_loudness_scan();
            }
        }

        // Take the stream out of self to avoid double-&mut-self borrow
        // conflict: the decode methods need &mut self, but the stream
        // references (decoder, resampler) also come from self.stream.
        // By moving the stream to a local, self and stream are disjoint.
        let mut stream = match self.stream.take() {
            Some(s) => s,
            None => return,
        };

        match &mut stream {
            PlaybackStream::Single { decoder, resampler } => {
                self.decode_single_stream(
                    decoder,
                    #[cfg(feature = "resample")]
                    resampler,
                    #[cfg(not(feature = "resample"))]
                    resampler,
                );
            }
            PlaybackStream::Transitioning {
                outgoing_decoder,
                outgoing_resampler,
                incoming_decoder,
                incoming_resampler,
                crossfade_frames_remaining,
                crossfade_total_frames,
            } => {
                self.decode_transitioning_stream(
                    outgoing_decoder,
                    #[cfg(feature = "resample")]
                    outgoing_resampler,
                    #[cfg(not(feature = "resample"))]
                    outgoing_resampler,
                    incoming_decoder,
                    #[cfg(feature = "resample")]
                    incoming_resampler,
                    #[cfg(not(feature = "resample"))]
                    incoming_resampler,
                    crossfade_frames_remaining,
                    *crossfade_total_frames,
                );
            }
        }
        if self.stream.is_none() {
            self.stream = Some(stream);
        } else {
            // A new stream was loaded during decode_single_stream
            // (gapless transition). Discard the old stream; its decoder
            // has hit EndOfStream and is no longer needed.
            log::debug!("Gapless transition: replacing EOS stream with freshly loaded track");
        }
    }

    /// Decode and process a single (non-crossfading) track.
    fn decode_single_stream(
        &mut self,
        decoder: &mut Decoder,
        #[cfg(feature = "resample")] resampler: &mut Option<GenericResampler>,
        #[cfg(not(feature = "resample"))] _resampler: &mut Option<()>,
    ) {
        // Drain pending >2-channel output frames before processing new frames
        // (mirrors the stereo `pending_output_frames` drain below).
        if self.scratch.pending_multichannel_channels > 0 && !self.scratch.pending_multichannel.is_empty() {
            let ch = self.scratch.pending_multichannel_channels;
            let frames_written = self
                .output_buffer
                .push_frames_interleaved(&self.scratch.pending_multichannel, ch);
            if frames_written > 0 {
                self.scratch.pending_multichannel.drain(..frames_written * ch);
            }
            if !self.scratch.pending_multichannel.is_empty() {
                // Ring still full — retry on the next tick.
                return;
            }
            self.scratch.pending_multichannel_channels = 0;
        }

        // Always drain pending output frames before attempting to process new frames.
        loop {
            let len = self.scratch.pending_output_frames.len();
            if len == 0 {
                break;
            }
            // Drain up to 256 pending frames per iteration.
            const DRAIN_BATCH: usize = 256;
            let n = len.min(DRAIN_BATCH);
            let mut stereo_buf = [0.0f32; DRAIN_BATCH * 2];
            for i in 0..n {
                let (l, r) = self.scratch.pending_output_frames[i];
                stereo_buf[i * 2] = l;
                stereo_buf[i * 2 + 1] = r;
            }
            let written = self
                .output_buffer
                .push_block_interleaved(&stereo_buf[..n * 2]);
            let frames_written = written / 2;
            if frames_written > 0 {
                self.scratch.pending_output_frames.drain(..frames_written);
            }
            if frames_written < n {
                // Buffer full — leave remaining pending frames for next tick.
                return;
            }
        }

        if self.scratch.pending_output_frames.len() >= super::MAX_PENDING_OUTPUT_FRAMES {
            return;
        }

        let chunk_and_start: Option<(crate::decode::DecodedChunk, usize)> =
            self.scratch.pending_chunk.take().or_else(|| {
                match decoder.decode_next(MAX_AUDIO_BLOCK_FRAMES) {
                    Ok(chunk) => {
                        self.recovery.consecutive_decode_errors = 0;
                        Some((chunk, 0))
                    }
                    Err(DecodeError::EndOfStream) => {
                        info!("Track ended");
                        if let Some(ref source) = self.current_source {
                            self.emit_event(crate::events::EngineEvent::SourceFinished {
                                source: source.clone(),
                            });
                        }
                        self.scratch.crossfade_triggered = false;
                        let mut loaded_next = false;
                        if self.config.transition_mode != config::TransitionMode::Stop {
                            if let Some(path) = self.loudness_scan.next_track_path.take() {
                                match self.swap_to_next_track(
                                    &path,
                                    #[cfg(feature = "resample")]
                                    resampler,
                                    #[cfg(not(feature = "resample"))]
                                    _resampler,
                                ) {
                                    Ok(_info) => {
                                        info!(
                                            "Gapless transition: handed off to next track {}",
                                            path.display()
                                        );
                                        loaded_next = true;
                                        // The resampler and DSP state carry
                                        // across the boundary, so the engine
                                        // continues decoding on the next
                                        // tick. stream_ended stays false so
                                        // Play/Pause work.
                                    }
                                    Err(e) => {
                                        warn!("Gapless handoff failed: {}", e);
                                        self.update_playback_state(PlaybackState::Stopped);
                                        self.stream_ended = true;
                                    }
                                }
                            } else {
                                self.update_playback_state(PlaybackState::Stopped);
                                self.stream_ended = true;
                            }
                        } else {
                            info!("TransitionMode::Stop active: stopping at EOS without advancing");
                            self.update_playback_state(PlaybackState::Stopped);
                            self.stream_ended = true;
                        }
                        if !loaded_next {
                            self.stream_ended = true;
                            // The resampler's final partial input block is
                            // only emitted by flush(); release it through the
                            // final safety limiter so the track's tail is not
                            // dropped. Then release the limiter's own
                            // lookahead delay. (Skipped on a gapless
                            // handoff: swap_to_next_track preserves both the
                            // resampler and the limiter, so their tails flow
                            // into the next track naturally — flushing here
                            // would break the continuity.)
                            #[cfg(feature = "resample")]
                            self.flush_resampler_tail(resampler);
                            self.flush_final_limiter_tail();
                        }
                        None
                    }
                    Err(e) => {
                        self.recovery.consecutive_decode_errors += 1;
                        warn!(
                            "Decode error ({}/{}): {}",
                            self.recovery.consecutive_decode_errors, DECODE_ERROR_THRESHOLD, e
                        );
                        if self.recovery.consecutive_decode_errors >= DECODE_ERROR_THRESHOLD {
                            warn!("Too many consecutive decode errors; advancing to end of track");
                            self.update_playback_state(PlaybackState::Stopped);
                            self.stream_ended = true;
                        }
                        None
                    }
                }
            });

        let (chunk, start_frame) = match chunk_and_start {
            Some(v) => v,
            None => return,
        };

        // ── Native DSD path (§7): raw 1-bit payload never enters the f32
        // DSP pipeline. Pack the normalized channel bytes into the
        // negotiated wire format and push them into the DSD byte ring the
        // DSD-capable output backend drains to the DAC. EQ, loudness,
        // dynamics, resampler, limiter, and dither are bypassed by
        // construction — there is no f32 signal to process.
        if self.dsd.native_dsd_active {
            self.decode_native_dsd_chunk(chunk, start_frame);
            return;
        }

        let frames = chunk.frame_count;
        let channels = chunk.channels;
        let mut processed_frames: u64 = 0;

        let expected_samples = (frames as u64) * (channels as u64);
        if (chunk.samples.len() as u64) < expected_samples {
            warn!(
                "Decoder returned inconsistent data: expected {} samples, got {}",
                expected_samples,
                chunk.samples.len()
            );
            return;
        }

        // NOTE on `processed_frames` semantics: this counter tracks
        // SOURCE-consumed frames (input frames from the decoder), NOT
        // output frames written to the buffer. With a resampler, one
        // input frame can produce multiple output frames (or zero); we
        // still count 1 per source frame here.
        //
        // Block processing: decoded frames are collected into stereo
        // plane buffers, run through the whole DSP chain in one
        // `process_block` call (hoisting all stage/enabled checks out of
        // the per-frame loop), then written to the ring buffer. When the
        // ring fills we stop mid-chunk and resume from the cached frame
        // index next tick; already-produced output frames that could not
        // be flushed are preserved in `pending_output_frames` (in order).
        const BATCH_FRAMES: usize = 128;
        let mut plane_l = [0.0f32; BATCH_FRAMES];
        let mut plane_r = [0.0f32; BATCH_FRAMES];
        let mut batch = [0.0f32; BATCH_FRAMES * 2];

        let mut consumed = start_frame;
        let mut buffer_full = false;

        // Whether the resampler is in passthrough mode (no rate change).
        #[cfg(feature = "resample")]
        let bypass = resampler.as_ref().map_or(true, |r| r.is_passthrough());
        #[cfg(not(feature = "resample"))]
        let bypass = true;

        // The output device's channel count. The ring buffer carries exactly
        // this many samples per frame, so the >2-channel path only engages
        // when the device itself is multichannel and the widths line up.
        let out_ch = self
            .audio_output
            .as_ref()
            .map(|o| o.output_info().channels as usize)
            .unwrap_or(2)
            .max(1);
        let output_layout = ChannelLayout::from_count(out_ch);
        // Layout metadata is configuration/state, not per-frame data. Set it
        // once for this decode pass so the multichannel DSP loop remains
        // allocation-free after its scratch buffers are warmed.
        if out_ch > 2 {
            self.pipeline.set_multichannel_layout(&output_layout);
        }
        'block_loop: while consumed < frames {
            let n = (frames - consumed).min(BATCH_FRAMES);

            // 1. Fill planes with decoded samples, applying the configured ChannelPolicy.
            //
            // ChannelPolicy semantics:
            //
            //   ForceDownmixStereo (default):
            //     Always downmix to stereo. Mono is duplicated to both channels;
            //     multi-channel is ITU-R BS.775 sum-to-stereo.
            //
            //   PassThrough:
            //     Pass the source channel count through unchanged when it is ≤ 2.
            //     For > 2 channels, genuine multichannel passthrough is used only
            //     when the output device is the same width and no resampling is
            //     needed; otherwise the source is downmixed to stereo (the stereo
            //     filter stages — crossfeed, stereo enhancer, convolution — remain
            //     front-pair-only).
            //
            //   MaxChannels(N):
            //     Pass through if channels ≤ N; downmix if channels > N. For N > 2
            //     the multichannel passthrough rule above also applies.
            //
            // The stereo `plane_l` / `plane_r` output below is used for the
            // downmix / ≤2-channel cases; the multichannel path is handled by
            // the dedicated branch above.
            // A codec can decode more channels than this output path can
            // preserve. Multichannel passthrough is therefore a negotiated
            // intersection of policy, source layout, output layout, the
            // fixed DSP channel bound, and resampler state.
            let policy_allows_multichannel = match self.config.channel_policy {
                config::ChannelPolicy::ForceDownmixStereo => false,
                config::ChannelPolicy::PassThrough => channels > 2,
                config::ChannelPolicy::MaxChannels(max_ch) => {
                    channels > 2 && channels <= max_ch as usize
                }
            };
            // An explicit channel-mix template may widen stereo into the
            // negotiated multichannel output, or map a mismatched source
            // layout into that output. Without the explicit opt-in, the
            // legacy policy remains conservative and only preserves equal
            // source/output widths.
            let template_multichannel = self.config.channel_mix.enabled
                && channels <= MAX_CHANNELS
                && out_ch > 2
                && out_ch <= MAX_CHANNELS
                && bypass
                && (channels <= 2 || channels != out_ch);
            let multichannel_passthrough = out_ch > 2
                && out_ch <= MAX_CHANNELS
                && channels <= MAX_CHANNELS
                && bypass
                && ((policy_allows_multichannel && channels == out_ch) || template_multichannel);
            let needs_downmix = channels > 2 && !multichannel_passthrough;

            if channels > 2 && policy_allows_multichannel && !multichannel_passthrough {
                log::debug!(
                    "ChannelPolicy::{:?}: preserving {} channels requires a matching {}-channel \
                     output, <= {} channels, and no resampler; downmixing to stereo",
                    self.config.channel_policy,
                    channels,
                    out_ch,
                    MAX_CHANNELS
                );
            }

            if multichannel_passthrough {
                let start = consumed * channels;
                let end = start + n * channels;
                if end > chunk.samples.len() {
                    break 'block_loop;
                }
                let mut mc_batch = [0.0f32; BATCH_FRAMES * MAX_CHANNELS];
                let output_frames = if self.config.channel_mix.enabled {
                    mix_interleaved_with_template(
                        &chunk.samples[start..end],
                        &chunk.channel_layout,
                        channels,
                        &output_layout,
                        &self.config.channel_mix.template,
                        &mut mc_batch[..n * out_ch],
                        n,
                    )
                } else {
                    mc_batch[..n * out_ch].copy_from_slice(&chunk.samples[start..end]);
                    n
                };
                if output_frames != n {
                    warn!(
                        "ChannelMix template produced {} frames for a {}-frame block; preserving source",
                        output_frames, n
                    );
                    break 'block_loop;
                }

                // The active layout is the negotiated output layout, not the
                // source layout after an upmix/template transform. It was set
                // once above, keeping LFE role detection and main-speaker
                // bass management aligned without work in this block loop.
                self.pipeline
                    .process_block_multichannel(&mut mc_batch[..n * out_ch], out_ch);

                // Final safety stage runs multichannel lookahead limiting on all channels
                // (coherent gain reduction across all surround/height/LFE channels).
                self.pipeline
                    .process_final_limiter_multichannel(&mut mc_batch[..n * out_ch], out_ch);

                let frames_written = self
                    .output_buffer
                    .push_frames_interleaved(&mc_batch[..n * out_ch], out_ch);
                if frames_written < n {
                    // The ring filled mid-block: preserve the already-processed
                    // leftover frames for the next tick.
                    self.scratch.pending_multichannel.clear();
                    let remaining_samples = (n - frames_written) * channels;
                    debug_assert!(
                        remaining_samples <= self.scratch.pending_multichannel.capacity(),
                        "pending multichannel FIFO exceeds its preallocated capacity"
                    );
                    if remaining_samples <= self.scratch.pending_multichannel.capacity() {
                        self.scratch.pending_multichannel
                            .extend_from_slice(&mc_batch[frames_written * channels..n * channels]);
                        self.scratch.pending_multichannel_channels = channels;
                    } else {
                        log::error!(
                            "pending multichannel FIFO under-sized for {} samples; halting to preserve audio",
                            remaining_samples
                        );
                        self.stream_ended = true;
                    }
                    processed_frames += n as u64;
                    consumed += n;
                    buffer_full = true;
                    break 'block_loop;
                }
                processed_frames += n as u64;
                consumed += n;
                continue;
            }

            if channels == 1 {
                // Mono: duplicate to both channels regardless of policy.
                for j in 0..n {
                    let idx = consumed + j;
                    if idx >= chunk.samples.len() {
                        break 'block_loop;
                    }
                    let s = chunk.samples[idx];
                    plane_l[j] = s;
                    plane_r[j] = s;
                }
            } else if channels == 2 {
                // Stereo: pass through regardless of policy.
                for j in 0..n {
                    let idx = (consumed + j) * 2;
                    if idx + 2 > chunk.samples.len() {
                        break 'block_loop;
                    }
                    plane_l[j] = chunk.samples[idx];
                    plane_r[j] = chunk.samples[idx + 1];
                }
            } else if needs_downmix {
                // Multi-channel → stereo downmix. A configured template is
                // explicit; the unconfigured path retains the ITU helper.
                let start = consumed * channels;
                let end = (consumed + n) * channels;
                if end <= chunk.samples.len() {
                    if self.config.channel_mix.enabled {
                        mix_interleaved_to_stereo_with_template(
                            &chunk.samples[start..end],
                            &chunk.channel_layout,
                            channels,
                            &self.config.channel_mix.template,
                            &mut plane_l[..n],
                            &mut plane_r[..n],
                            n,
                        );
                    } else {
                        crate::decode::downmix_interleaved_to_stereo(
                            &chunk.samples[start..end],
                            &chunk.channel_layout,
                            channels,
                            &mut plane_l[..n],
                            &mut plane_r[..n],
                            n,
                        );
                    }
                } else {
                    break 'block_loop;
                }
            } else {
                // PassThrough / MaxChannels with channels ≤ max: use first two
                // channels as L and R (the pipeline is stereo — we cannot pass
                // more than 2 channels through it without a full pipeline redesign).
                for j in 0..n {
                    let idx = (consumed + j) * channels;
                    if idx + 1 >= chunk.samples.len() {
                        break 'block_loop;
                    }
                    plane_l[j] = chunk.samples[idx];
                    plane_r[j] = chunk.samples[idx + 1];
                }
            }

            // 2. Run the DSP chain (pre-mix + post-mix) over the block. In
            //    Single mode the mixer is in PlayingCurrent state, so it
            //    passes through unchanged — same as calling process() per
            //    frame. The safety limiter is intentionally NOT part of this
            //    chain: it runs in the output domain, after resampling.
            self.pipeline
                .process_block(&mut plane_l[..n], &mut plane_r[..n]);

            if bypass {
                // 3a. No resampling: this block is already in the output
                //     domain, so the final safety limiter runs on it directly
                //     before it is interleaved and flushed.
                self.pipeline
                    .process_final_limiter_block(&mut plane_l[..n], &mut plane_r[..n]);
                for j in 0..n {
                    batch[j * 2] = plane_l[j];
                    batch[j * 2 + 1] = plane_r[j];
                }
                let written = self.output_buffer.push_block_interleaved(&batch[..n * 2]);
                let frames_written = written / 2;
                if frames_written < n {
                    // Output full: keep the unwritten frames in chronological
                    // order at the FRONT of pending_output_frames.
                    for j in (frames_written..n).rev() {
                        self.push_pending_front((plane_l[j], plane_r[j]));
                    }
                    buffer_full = true;
                }
            } else {
                // 3b. Feed the processed block through the resampler, then
                //     drain all newly produced output frames.
                #[cfg(feature = "resample")]
                if let Some(ref mut r) = resampler {
                    for j in 0..n {
                        if self.config.precision_mode == config::PrecisionMode::Quality {
                            r.feed_f64(plane_l[j] as f64, plane_r[j] as f64);
                        } else {
                            r.feed_f32(plane_l[j], plane_r[j]);
                        }
                    }
                    while let Some((out_l, out_r)) = r.read_f32() {
                        // Final safety stage: enforce the ceiling on the
                        // output-domain (resampled) samples.
                        let (l, r) = self.pipeline.process_final_limiter(out_l, out_r);
                        self.push_pending_back((l, r));
                    }
                }

                // Drain pending output frames to the ring in batches; if the
                // ring fills, leave the rest for the next tick.
                loop {
                    let len = self.scratch.pending_output_frames.len();
                    if len == 0 {
                        break;
                    }
                    const DRAIN_BATCH: usize = 256;
                    let m = len.min(DRAIN_BATCH);
                    let mut stereo_buf = [0.0f32; DRAIN_BATCH * 2];
                    for k in 0..m {
                        let (l, r) = self.scratch.pending_output_frames[k];
                        stereo_buf[k * 2] = l;
                        stereo_buf[k * 2 + 1] = r;
                    }
                    let written = self
                        .output_buffer
                        .push_block_interleaved(&stereo_buf[..m * 2]);
                    let frames_written = written / 2;
                    if frames_written > 0 {
                        self.scratch.pending_output_frames.drain(..frames_written);
                    }
                    if frames_written < m {
                        buffer_full = true;
                        break;
                    }
                }
            }

            // Count the whole block of source frames as consumed.
            processed_frames += n as u64;
            consumed += n;
            if buffer_full {
                break 'block_loop;
            }
        }

        if buffer_full && consumed < frames {
            // Resume from `consumed` on the next tick; every frame before it
            // was processed and its output is in the ring or pending.
            self.scratch.pending_chunk = Some((chunk, consumed));
        } else {
            // Whole chunk processed — drain any resampler tail.
            #[cfg(feature = "resample")]
            if let Some(ref mut r) = resampler {
                while let Some((out_l, out_r)) = r.read_f32() {
                    let (l, r) = self.pipeline.process_final_limiter(out_l, out_r);
                    self.push_pending_back((l, r));
                }
                // Bulk drain remaining pending frames.
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
                    let written = self
                        .output_buffer
                        .push_block_interleaved(&stereo_buf[..n * 2]);
                    let frames_written = written / 2;
                    if frames_written > 0 {
                        self.scratch.pending_output_frames.drain(..frames_written);
                    }
                    if frames_written < n {
                        break;
                    }
                }
            }
        }

        // Advance the sample-accurate clock by the source frames consumed
        // this tick. Position is then computed exactly as `frames / rate`;
        // at speed != 1.0 the frame counter simply accumulates faster, so
        // the reported track position stays consistent with the audio.
        self.clock.advance_source(processed_frames);

        let pos = self.clock.position_secs();
        let (latency_ms, compensated) = self.latency_compensation(pos);
        self.playback_info.rcu(|old| {
            Arc::new(PlaybackInfo {
                position_secs: pos,
                position_secs_compensated: compensated,
                latency_ms,
                ..old.as_ref().clone()
            })
        });
    }

    /// Native-DSD path: pack one raw DSD chunk into the negotiated wire
    /// format and push it into the DSD byte ring the output backend drains
    /// to the DAC (§7). The raw 1-bit payload never touches the f32 DSP
    /// pipeline, resampler, limiter, or dither — bypass by construction.
    ///
    /// `start_frame` is the resume offset into the chunk's DSD frame budget
    /// (`frame_count`), matching the standard `pending_chunk` resume
    /// machinery: if the byte ring fills mid-chunk, the remainder is
    /// re-queued exactly like a filled PCM ring.
    pub(super) fn decode_native_dsd_chunk(
        &mut self,
        chunk: crate::decode::DecodedChunk,
        start_frame: usize,
    ) {
        let Some(raw) = chunk.raw_dsd.as_ref() else {
            log::error!("native DSD chunk missing raw payload");
            return;
        };
        let Some(wire) = self.dsd.dsd_wire_format else {
            log::error!("native DSD active but no negotiated wire format");
            return;
        };
        let Some(dsd_buffer) = self.dsd.dsd_byte_buffer.clone() else {
            log::error!("native DSD active but no byte buffer");
            return;
        };
        let bpw = wire.bytes_per_word();
        // All native consumption is word-aligned (samples_per_word DSD frames
        // per word), so slicing channel bytes at `start_frame / 8` bytes per
        // channel is always integral.
        let byte_offset =
            (start_frame / 8).min(raw.channel_bytes.iter().map(Vec::len).min().unwrap_or(0));
        let channel_refs: Vec<&[u8]> = raw
            .channel_bytes
            .iter()
            .map(|c| &c[byte_offset.min(c.len())..])
            .collect();
        let mut packed = std::mem::take(&mut self.dsd.dsd_pack_scratch);
        let words = crate::decode::dsd::NativeDsdPacker::pack(wire, &channel_refs, &mut packed);
        let samples_per_word = wire.samples_per_word();
        let frame_width = bpw * raw.channels;
        let bytes_to_push = words * raw.channels * bpw;
        let pushed_frames = dsd_buffer.push_frames(&packed[..bytes_to_push], frame_width);
        self.dsd.dsd_pack_scratch = packed;
        // Convert pushed wire frames back to DSD frames (per channel) for the
        // source clock and the resume offset.
        let pushed_dsd = pushed_frames * samples_per_word;
        let total_dsd = words * samples_per_word;
        if pushed_dsd < total_dsd && start_frame + pushed_dsd < chunk.frame_count as usize {
            self.scratch.pending_chunk = Some((chunk, start_frame + pushed_dsd));
        } else {
            self.scratch.pending_chunk = None;
        }
        self.clock.advance_source(pushed_dsd as u64);
        let pos = self.clock.position_secs();
        let (latency_ms, compensated) = self.latency_compensation(pos);
        self.playback_info.rcu(|old| {
            Arc::new(PlaybackInfo {
                position_secs: pos,
                position_secs_compensated: compensated,
                latency_ms,
                ..old.as_ref().clone()
            })
        });
    }

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
    fn decode_transitioning_stream(
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
            let written = self
                .output_buffer
                .push_block_interleaved(&stereo_buf[..n * 2]);
            let frames_written = written / 2;
            if frames_written > 0 {
                self.scratch.pending_output_frames.drain(..frames_written);
            }
            if frames_written < n {
                return;
            }
        }
        if self.scratch.pending_output_frames.len() >= super::MAX_PENDING_OUTPUT_FRAMES {
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
                self.scratch.rs_out_buf.len().min(*crossfade_frames_remaining)
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

                let (mixed_l, mixed_r) =
                    if self.config.precision_mode == config::PrecisionMode::Quality {
                        let (ml, mr) = self.pipeline.mixer_mut().process_f64(
                            out_l as f64,
                            out_r as f64,
                            in_l as f64,
                            in_r as f64,
                        );
                        (ml as f32, mr as f32)
                    } else {
                        self.pipeline.mixer_mut().process(out_l, out_r, in_l, in_r)
                    };
                self.push_mix_frame(mixed_l, mixed_r);
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
            while let Some((l, r)) = self.scratch.rs_in_buf.pop_front() {
                self.push_mix_frame(l, r);
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
                    let (dl, dr) = if self.config.precision_mode == config::PrecisionMode::Quality {
                        let (l64, r64) = self.pipeline.process_outgoing_f64(l as f64, r as f64);
                        (l64 as f32, r64 as f32)
                    } else {
                        self.pipeline.process_outgoing(l, r)
                    };
                    feed_resampled_frame(
                        resampler,
                        &mut self.scratch.rs_out_buf,
                        self.config.precision_mode,
                        dl,
                        dr,
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
                    let (dl, dr) = if self.config.precision_mode == config::PrecisionMode::Quality {
                        let (l64, r64) = self.pipeline.process_incoming_f64(l as f64, r as f64);
                        (l64 as f32, r64 as f32)
                    } else {
                        self.pipeline.process_incoming(l, r)
                    };
                    feed_resampled_frame(
                        resampler,
                        &mut self.scratch.rs_in_buf,
                        self.config.precision_mode,
                        dl,
                        dr,
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
                let (dl, dr) = if self.config.precision_mode == config::PrecisionMode::Quality {
                    let (l64, r64) = self.pipeline.process_outgoing_f64(l as f64, r as f64);
                    (l64 as f32, r64 as f32)
                } else {
                    self.pipeline.process_outgoing(l, r)
                };
                self.push_crossfade_out((dl, dr));
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
                let (dl, dr) = if self.config.precision_mode == config::PrecisionMode::Quality {
                    let (l64, r64) = self.pipeline.process_incoming_f64(l as f64, r as f64);
                    (l64 as f32, r64 as f32)
                } else {
                    self.pipeline.process_incoming(l, r)
                };
                self.push_crossfade_in((dl, dr));
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
        let n = self.scratch.mix_l.len().min(self.scratch.mix_r.len());
        if n == 0 {
            return true;
        }
        self.pipeline
            .process_post_mix_block(&mut self.scratch.mix_l[..n], &mut self.scratch.mix_r[..n]);
        // The mixed block is already in the output domain (both sides were
        // resampled before mixing), so the final safety limiter runs here.
        self.pipeline
            .process_final_limiter_block(&mut self.scratch.mix_l[..n], &mut self.scratch.mix_r[..n]);
        let mut batch = [0.0f32; MIX_BLOCK * 2];
        for i in 0..n {
            batch[i * 2] = self.scratch.mix_l[i];
            batch[i * 2 + 1] = self.scratch.mix_r[i];
        }
        self.scratch.mix_l.clear();
        self.scratch.mix_r.clear();
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
