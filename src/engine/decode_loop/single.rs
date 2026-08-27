//! Single-track (non-crossfading) decode and process path.

use std::sync::Arc;

use log::{info, warn};

use super::common::DECODE_ERROR_THRESHOLD;
use super::AudioEngine;
use super::MAX_PENDING_OUTPUT_FRAMES;
#[cfg(feature = "resample")]
use crate::dsp::resampler::GenericResampler;
use crate::{
    buffer::{PlaybackInfo, PlaybackState, MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS},
    decode::{
        mix_interleaved_to_stereo_with_template, mix_interleaved_with_template, ChannelLayout,
        DecodeError, Decoder,
    },
};

impl AudioEngine {
    pub(super) fn decode_single_stream(
        &mut self,
        decoder: &mut Decoder,
        #[cfg(feature = "resample")] resampler: &mut Option<GenericResampler>,
        #[cfg(not(feature = "resample"))] _resampler: &mut Option<()>,
    ) {
        // Drain pending >2-channel output frames before processing new frames
        // (mirrors the stereo `pending_output_frames` drain below).
        if self.scratch.pending_multichannel_channels > 0
            && !self.scratch.pending_multichannel.is_empty()
        {
            let ch = self.scratch.pending_multichannel_channels;
            let frames_written = self.push_to_sink(&self.scratch.pending_multichannel, ch);
            if frames_written > 0 {
                self.scratch
                    .pending_multichannel
                    .drain(..frames_written * ch);
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
            let frames_written = self.push_to_sink(&stereo_buf[..n * 2], 2);
            if frames_written > 0 {
                self.scratch.pending_output_frames.drain(..frames_written);
            }
            if frames_written < n {
                // Buffer full — leave remaining pending frames for next tick.
                return;
            }
        }

        if self.scratch.pending_output_frames.len() >= MAX_PENDING_OUTPUT_FRAMES {
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

                        // Repeat-One: restart the current track at its start
                        // instead of advancing the queue.
                        if self.playlist.repeat() == crate::playlist::RepeatMode::One
                            && self.playlist.current_index().is_some()
                        {
                            match decoder.seek(0.0) {
                                Ok(()) => {
                                    self.clock.set_source_frames(0);
                                    self.scratch.pending_chunk = None;
                                    info!("Repeat-One: restarting current track");
                                    return None;
                                }
                                Err(e) => {
                                    warn!("Repeat-One restart failed ({}); advancing", e);
                                }
                            }
                        }

                        // Auto-advance the playlist when no track was
                        // explicitly prepared via `prepare_next`.
                        if self.loudness_scan.next_track_path.is_none()
                            && self.config.transition_mode != config::TransitionMode::Stop
                        {
                            if let Some(src) = self.playlist.advance() {
                                self.emit_playlist_changed();
                                match src {
                                    crate::source::AudioSource::File(ref p) => {
                                        if let Err(e) = self.prepare_next_track(p) {
                                            warn!(
                                                "Failed to prepare next playlist entry {}: {}",
                                                p.display(),
                                                e
                                            );
                                        }
                                    }
                                    other => {
                                        // Memory/URI sources cannot use the
                                        // path-based gapless handoff; load
                                        // them directly.
                                        match self.load_source(&other) {
                                            Ok(_) => loaded_next = true,
                                            Err(e) => {
                                                warn!("Failed to load next playlist entry: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }

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
                            } else if !loaded_next {
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
        let bypass = resampler.as_ref().is_none_or(|r| r.is_passthrough());
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
            self.graph.set_multichannel_layout(&output_layout);
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
                self.graph
                    .process_block_multichannel(&mut mc_batch[..n * out_ch], out_ch);

                // Final safety stage runs multichannel lookahead limiting on all channels
                // (coherent gain reduction across all surround/height/LFE channels).
                self.graph
                    .process_final_limiter_multichannel(&mut mc_batch[..n * out_ch], out_ch);

                let frames_written = self.push_to_sink(&mc_batch[..n * out_ch], out_ch);
                #[cfg(feature = "audio-output")]
                self.push_to_endpoints(&mc_batch[..n * out_ch], out_ch);
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
                        self.scratch
                            .pending_multichannel
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
            //    Active lanes (Phase 4 S6) are decoded and mixed as
            //    secondaries on bus slots ≥ 2.
            if self.lanes.is_empty() {
                self.graph
                    .process_block(&mut plane_l[..n], &mut plane_r[..n]);
            } else {
                // `fill_lane_scratch` placed every lane's audio on its own
                // slot's planes (index k ↔ bus slot k + 2) and zeroed the
                // rest; feed ALL lane slots so each lane reaches the right
                // bus slot even after removals/re-adds (a removed lane's
                // slot is detached and contributes nothing).
                let _active = self.fill_lane_scratch(n);
                let mut iter_l = self.scratch.lane_l.iter_mut();
                let mut iter_r = self.scratch.lane_r.iter_mut();
                let mut secondaries: [(&mut [f32], &mut [f32]); crate::engine::lanes::MAX_LANES] =
                    std::array::from_fn(|_| {
                        let l: &mut Vec<f32> = iter_l.next().expect("lane planes preallocated");
                        let r: &mut Vec<f32> = iter_r.next().expect("lane planes preallocated");
                        (&mut l[..n], &mut r[..n])
                    });
                self.graph
                    .process_block_lanes((&mut plane_l[..n], &mut plane_r[..n]), &mut secondaries);
            }

            if bypass {
                // 3a. No resampling: this block is already in the output
                //     domain, so the final safety limiter runs on it directly
                //     before it is interleaved and flushed.
                self.graph
                    .process_final_limiter_block(&mut plane_l[..n], &mut plane_r[..n]);
                // Multi-endpoint fan-out: every additional endpoint gets the
                // master-domain block (resampler + its own limiter inside).
                self.fanout_endpoint_block(&plane_l[..n], &plane_r[..n]);
                for j in 0..n {
                    batch[j * 2] = plane_l[j];
                    batch[j * 2 + 1] = plane_r[j];
                }
                let frames_written = self.push_to_sink(&batch[..n * 2], 2);
                #[cfg(feature = "audio-output")]
                self.push_to_endpoints(&batch[..n * 2], 2);
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
                        let (l, r) = self.graph.process_final_limiter(out_l, out_r);
                        self.push_pending_back((l, r));
                        // Multi-endpoint fan-out (per-frame resampled path).
                        self.fanout_endpoint_frame(l, r);
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
                // Multi-endpoint fan-out: flush the per-frame endpoint
                // chains to their rings alongside the master drain.
                self.drain_endpoints();
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
                    let (l, r) = self.graph.process_final_limiter(out_l, out_r);
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
                self.drain_endpoints();
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
    pub(crate) fn decode_native_dsd_chunk(
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
        if pushed_dsd < total_dsd && start_frame + pushed_dsd < chunk.frame_count {
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
}
