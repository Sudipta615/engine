//! Packet decoding, gapless trimming, and seeking for Symphonia sources.

use log;
use symphonia::core::{
    errors::Error as SymphoniaError,
    formats::{SeekMode, SeekTo},
    units::Time,
};

use crate::decode::symphonia_decoder::downmix_interleaved_to_stereo;
use crate::decode::symphonia_decoder::{DecodeError, DecodeInfo, SymphoniaDecoder};
use crate::decode::{ChannelLayout, DecodedChunk};

impl SymphoniaDecoder {
    /// Decode the next chunk of audio.
    ///
    /// Reuses the internal `sample_buffer` across calls instead of
    /// allocating a new one on every call.
    ///
    /// ## Gapless trimming
    ///
    /// If the container reported an encoder delay > 0, the corresponding
    /// number of leading frames are silently discarded until the counter
    /// reaches zero.  If the container reported a total logical frame count,
    /// the output is truncated so that exactly `total_logical_frames` frames
    /// are delivered in total across all calls — suppressing the trailing
    /// encoder padding without any external coordination.
    pub fn decode_next(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        let channels = self.info.channels;
        self.sample_buffer.clear();

        // ── Serve leftover frames from a straddled packet first ───────────
        if !self.pending_samples.is_empty() {
            let pending_frames = self.pending_samples.len() / channels;
            let take = pending_frames.min(max_frames);
            self.sample_buffer
                .extend_from_slice(&self.pending_samples[..take * channels]);
            if take < pending_frames {
                // Still more than the caller wants; keep the rest for later.
                self.pending_samples.drain(..take * channels);
                let cap = self.sample_buffer.capacity();
                let samples = std::mem::replace(&mut self.sample_buffer, Vec::with_capacity(cap));
                return Ok(DecodedChunk {
                    samples,
                    channels,
                    channel_layout: self.format_info.channel_layout.clone(),
                    sample_rate: self.info.sample_rate,
                    frame_count: take,
                    raw_dsd: None,
                });
            }
            self.pending_samples.clear();
        }

        // Respect the logical end-of-stream: if we've already delivered all
        // logical frames (and there is nothing pending to hand out), emit
        // EndOfStream immediately.
        if self.sample_buffer.is_empty() && self.logical_frames_remaining == Some(0) {
            return Err(DecodeError::EndOfStream);
        }

        let mut frames_decoded = self.sample_buffer.len() / channels;
        let mut consecutive_skips = 0u32;
        const MAX_CONSECUTIVE_SKIPS: u32 = 32;

        while frames_decoded < max_frames {
            // Nothing left in the logical timeline to decode.
            if self.logical_frames_remaining == Some(0) {
                break;
            }

            let packet = match self.format_reader.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    // End of stream
                    break;
                }
                Err(SymphoniaError::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder.reset();
                    consecutive_skips += 1;
                    if consecutive_skips > MAX_CONSECUTIVE_SKIPS {
                        log::debug!("Max consecutive ResetRequired skips reached near stream end");
                        break;
                    }
                    continue;
                }
                Err(SymphoniaError::IoError(_)) => {
                    // Generic IO error at stream end should break to trigger EndOfStream
                    break;
                }
                Err(e) => return Err(DecodeError::Decode(format!("Packet read error: {}", e))),
            };

            if packet.track_id != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let src_channels = decoded.num_planes();
                    let decoded_frames = decoded.frames();

                    self.scratch_interleaved.clear();
                    decoded.copy_to_vec_interleaved(&mut self.scratch_interleaved);

                    // ── Apply encoder-delay skip ─────────────────────────
                    let (effective_scratch, effective_frames) = if self.frames_to_skip > 0 {
                        let skip = (self.frames_to_skip as usize).min(decoded_frames);
                        let skip_samples = skip * src_channels;
                        self.frames_to_skip -= skip as u64;
                        let remaining_frames = decoded_frames - skip;
                        (&self.scratch_interleaved[skip_samples..], remaining_frames)
                    } else {
                        (&self.scratch_interleaved[..], decoded_frames)
                    };

                    if effective_frames == 0 {
                        consecutive_skips = 0;
                        continue;
                    }

                    // ── Clamp to logical frames remaining ───────────────
                    let frames_to_use = if let Some(rem) = self.logical_frames_remaining {
                        effective_frames.min(rem as usize)
                    } else {
                        effective_frames
                    };
                    if frames_to_use == 0 {
                        break;
                    }

                    let samples_to_copy = frames_to_use * src_channels;
                    self.sample_buffer
                        .extend_from_slice(&effective_scratch[..samples_to_copy]);
                    frames_decoded += frames_to_use;

                    if let Some(ref mut rem) = self.logical_frames_remaining {
                        *rem = rem.saturating_sub(frames_to_use as u64);
                    }

                    consecutive_skips = 0;
                }
                Err(SymphoniaError::DecodeError(_)) => {
                    self.decoder.reset();
                    consecutive_skips += 1;
                    if consecutive_skips > MAX_CONSECUTIVE_SKIPS {
                        if self.sample_buffer.is_empty() {
                            return Err(DecodeError::EndOfStream);
                        }
                        break;
                    }
                    continue;
                }
                Err(e) => return Err(DecodeError::Decode(format!("Decode error: {}", e))),
            }
        }

        if self.sample_buffer.is_empty() {
            if consecutive_skips > 0 {
                log::debug!(
                    "End of stream reached after {} consecutive decode skips",
                    consecutive_skips
                );
            }
            return Err(DecodeError::EndOfStream);
        }

        // ── Split off any overflow past `max_frames` for the next call ──
        let total_frames = self.sample_buffer.len() / channels;
        let deliver = total_frames.min(max_frames);
        let keep = total_frames - deliver;
        if keep > 0 {
            self.pending_samples = self.sample_buffer.split_off(deliver * channels);
        }
        let cap = self.sample_buffer.capacity();
        let samples = std::mem::replace(&mut self.sample_buffer, Vec::with_capacity(cap));

        Ok(DecodedChunk {
            samples,
            channels,
            channel_layout: self.format_info.channel_layout.clone(),
            sample_rate: self.info.sample_rate,
            frame_count: deliver,
            raw_dsd: None,
        })
    }

    /// Seek to a position in seconds (logical time coordinate).
    pub fn seek(&mut self, position_secs: f32) -> Result<(), DecodeError> {
        if !position_secs.is_finite() || position_secs < 0.0 {
            return Err(DecodeError::Seek(format!(
                "Invalid seek position: {}",
                position_secs
            )));
        }
        let clamped = position_secs.min(86400.0);
        let sample_rate = self.info.sample_rate as f64;
        let target_logical = ((clamped as f64) * sample_rate).round().max(0.0) as u64;
        let target_physical = self.gapless.encoder_delay + target_logical;
        let physical_time_secs = target_physical as f64 / sample_rate;
        let time = Time::try_from_secs_f64(physical_time_secs).unwrap_or(Time::ZERO);
        let seek_to = SeekTo::Time {
            time,
            track_id: Some(self.track_id),
        };

        self.format_reader
            .seek(SeekMode::Accurate, seek_to)
            .map_err(|e| DecodeError::Seek(format!("Seek failed: {}", e)))?;

        self.decoder.reset();
        // Drop any leftover packet tail from the pre-seek position.
        self.pending_samples.clear();

        // ── Recompute gapless state for the new position ───────────────────
        let (frames_to_skip, logical_frames_remaining) =
            self.gapless.state_after_seek(target_physical);
        self.frames_to_skip = frames_to_skip;
        self.logical_frames_remaining = logical_frames_remaining;

        Ok(())
    }

    pub fn info(&self) -> &DecodeInfo {
        &self.info
    }

    pub fn duration_secs(&self) -> f32 {
        self.info.duration_secs
    }

    /// Extract f32 samples from an interleaved f32 slice and downmix to stereo (for tests & backward compat).
    pub fn extract_from_interleaved_f32(
        samples: &[f32],
        output: &mut Vec<f32>,
        layout: &ChannelLayout,
        src_channels: usize,
        _target_channels: usize,
        frames: usize,
    ) -> usize {
        let actual_frames = (samples.len() / src_channels.max(1)).min(frames);
        let mut plane_l = vec![0.0f32; actual_frames];
        let mut plane_r = vec![0.0f32; actual_frames];
        let got = downmix_interleaved_to_stereo(
            samples,
            layout,
            src_channels,
            &mut plane_l,
            &mut plane_r,
            actual_frames,
        );
        output.reserve(got * 2);
        for i in 0..got {
            output.push(plane_l[i]);
            output.push(plane_r[i]);
        }
        got
    }
}
