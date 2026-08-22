//! Audio decoder using Symphonia for format support.
//!
//! Decodes MP3, FLAC, OGG/Vorbis, WAV PCM, AAC, ALAC and raw PCM in this
//! build (ALAC via `symphonia-codec-alac`, enabled by the `codec-alac`
//! feature). Symphonia itself has no APE audio decoder (only APEv2 tag
//! parsing); `.ape` / `.mac` audio is instead routed to the separate
//! `ape-decoder` backend in `crate::decode::ape`. All decoding is off the
//! audio thread and thread-safe.
//!
//! ## Gapless playback
//!
//! When the container exposes encoder delay and/or end-padding (MP3 via
//! Xing/LAME/iTunSMPB, AAC via iTunSMPB/ASC priming), `SymphoniaDecoder`
//! automatically discards leading silence frames (`encoder_delay`) and
//! terminates the stream at the correct logical frame count (suppressing
//! `end_padding` samples). Both corrections are applied inside
//! [`SymphoniaDecoder::decode_next`] so the engine always receives only
//! true audio — the reported `duration_secs` reflects the logical (corrected)
//! duration, not the raw container length.
#![allow(clippy::incompatible_msrv)]

use std::{fs::File, path::Path};

use symphonia::core::{
    codecs::audio::{AudioDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO},
    errors::Error as SymphoniaError,
    formats::{probe::Hint, FormatOptions, FormatReader, SeekMode, SeekTo},
    io::MediaSourceStream,
    meta::{MetadataOptions, StandardTag},
    units::{Time, Timestamp},
};
use thiserror::Error;

use crate::decode::{AudioFormatInfo, ChannelLayout, GaplessInfo, RawDsdChunk};

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("Failed to open file: {0}")]
    FileOpen(String),
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("Decode error: {0}")]
    Decode(String),
    #[error("Seek error: {0}")]
    Seek(String),
    #[error("End of stream")]
    EndOfStream,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Decoded audio format information
#[derive(Debug, Clone)]
pub struct DecodeInfo {
    pub sample_rate: u32,
    pub channels: usize,
    pub duration_secs: f32,
    pub codec: String,
    pub bitrate_kbps: Option<u32>,
}

/// A chunk of decoded PCM audio
#[derive(Debug, Clone)]
pub struct DecodedChunk {
    /// Interleaved f32 samples.
    pub samples: Vec<f32>,
    pub channels: usize,
    pub channel_layout: ChannelLayout,
    pub sample_rate: u32,
    pub frame_count: usize,
    /// Raw native-DSD payload when the decoder runs in native-DSD transport
    /// mode (`samples` is then empty). `None` for every PCM path.
    pub raw_dsd: Option<RawDsdChunk>,
}

/// Symphonia-based audio decoder with sample-accurate gapless trimming.
pub struct SymphoniaDecoder {
    format_reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    info: DecodeInfo,
    /// Reusable sample buffer for decoded output, passed across
    /// decode_next calls instead of allocating a new Vec each time.
    sample_buffer: Vec<f32>,
    /// Reusable scratch buffer for generic sample to f32 interleaved conversion.
    scratch_interleaved: Vec<f32>,
    /// Tail of a packet that straddled the previous call's `max_frames`
    /// boundary, in interleaved samples. Carried over so no decoded frames
    /// are ever dropped (see `decode_next`).
    pending_samples: Vec<f32>,
    // ── Gapless state ──────────────────────────────────────────────────────
    /// GaplessInfo extracted from the container at open time.
    gapless: GaplessInfo,
    /// Number of leading frames still to discard (counts down from encoder_delay).
    frames_to_skip: u64,
    /// Number of logical frames remaining before we suppress the stream.
    /// `None` means no limit (no total_logical_frames info in the container).
    logical_frames_remaining: Option<u64>,
    /// Full format descriptor (built at open time).
    format_info: AudioFormatInfo,
}

impl SymphoniaDecoder {
    /// Open a file for decoding
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path)
            .map_err(|e| DecodeError::FileOpen(format!("Cannot open {}: {}", path.display(), e)))?;

        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let format_opts = FormatOptions::default();
        let metadata_opts = MetadataOptions::default();
        let decoder_opts = AudioDecoderOptions::default();

        let format_reader = symphonia::default::get_probe()
            .probe(&hint, mss, format_opts, metadata_opts)
            .map_err(|e| DecodeError::UnsupportedFormat(format!("Probe failed: {}", e)))?;

        let track = format_reader
            .tracks()
            .iter()
            .find(|t| {
                t.codec_params
                    .as_ref()
                    .and_then(|cp| cp.audio())
                    .is_some_and(|a| a.codec != CODEC_ID_NULL_AUDIO)
            })
            .ok_or_else(|| DecodeError::UnsupportedFormat("No audio track found".to_string()))?;

        let track_id = track.id;
        let audio_params = track
            .codec_params
            .as_ref()
            .and_then(|cp| cp.audio())
            .ok_or_else(|| DecodeError::UnsupportedFormat("No audio codec params".to_string()))?;

        // ── Extract gapless metadata ─────────────────────────────────────────────────
        let encoder_delay = track.delay.unwrap_or(0) as u64;
        let end_padding = track.padding.unwrap_or(0) as u64;
        let priming_frames = encoder_delay; // same for most formats
        let num_frames_physical = track.num_frames.map(|n| n as u64);
        let total_logical_frames = num_frames_physical
            .map(|n| n.saturating_sub(encoder_delay).saturating_sub(end_padding));

        let gapless = GaplessInfo {
            encoder_delay,
            end_padding,
            priming_frames,
            total_logical_frames,
        };

        if gapless.needs_correction() {
            log::info!(
                "Gapless info: encoder_delay={}, end_padding={}, logical_frames={:?}",
                encoder_delay,
                end_padding,
                total_logical_frames
            );
        }

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(audio_params, &decoder_opts)
            .map_err(|e| DecodeError::Decode(format!("Cannot create decoder: {}", e)))?;

        let sample_rate = audio_params.sample_rate.unwrap_or(44100);
        let src_channels = audio_params
            .channels
            .as_ref()
            .map(|c| c.count())
            .unwrap_or(2);
        let channel_layout = ChannelLayout::from_count(src_channels);
        let channels = src_channels;

        // Use logical duration (after gapless trimming) for UI display.
        let duration_secs = total_logical_frames
            .map(|f| f as f32 / sample_rate as f32)
            .or_else(|| num_frames_physical.map(|f| f as f32 / sample_rate as f32))
            .unwrap_or(0.0);

        // symphonia 0.6's `AudioCodecId` Debug-prints as a numeric ID
        // (e.g. `AudioCodecId(264)`); the registered `CodecInfo` provides the
        // human-readable name (e.g. "pcm_s16le", "flac", "mp3"). Using it
        // keeps `DecodeInfo.codec` / `AudioFormatInfo.codec` displayable and
        // makes the lossless classification below work.
        let codec_str = decoder.codec_info().short_name.to_string();

        // Determine lossless flag from codec. Symphonia reports WAV/AIFF PCM
        // with the integer/float codec names (S8/S16/S24/S32, U8/U16, F32/F64)
        // rather than a generic "pcm", so those are matched explicitly — a
        // WAV file must never be reported as lossy (C4: decoder_lossless).
        let is_lossless = {
            let lower = codec_str.to_lowercase();
            lower.contains("flac")
                || lower.contains("alac")
                || lower.contains("ape")
                || lower.contains("pcm")
                || lower.contains("wav")
                || lower.contains("aiff")
                || ["s8", "s16", "s24", "s32", "u8", "u16", "f32", "f64"]
                    .iter()
                    .any(|c| lower.contains(c))
        };

        let bit_depth = audio_params.bits_per_sample.map(|b| b as u32);
        let sample_format = match bit_depth {
            Some(32) => "i32".to_string(),
            Some(24) => "i24".to_string(),
            Some(16) => "i16".to_string(),
            _ => "f32".to_string(),
        };

        let format_info = AudioFormatInfo {
            codec: codec_str.clone(),
            container: path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("unknown")
                .to_uppercase(),
            sample_rate,
            input_sample_rate: None,
            channels: src_channels,
            channel_layout: channel_layout.clone(),
            bit_depth,
            sample_format,
            duration_secs: Some(duration_secs as f64),
            bitrate_kbps: None,
            gapless: if gapless.needs_correction() {
                Some(gapless.clone())
            } else {
                None
            },
            replaygain_track_db: None,
            replaygain_album_db: None,
            ebu_r128_loudness: None,
            true_peak_dbtp: None,
            is_lossless,
            is_dsd: false,
        };

        let info = DecodeInfo {
            sample_rate,
            channels,
            duration_secs,
            codec: codec_str,
            bitrate_kbps: None,
        };

        Ok(Self {
            format_reader,
            decoder,
            track_id,
            info,
            sample_buffer: Vec::with_capacity(4096 * channels),
            scratch_interleaved: Vec::with_capacity(4096 * src_channels),
            pending_samples: Vec::new(),
            frames_to_skip: encoder_delay,
            logical_frames_remaining: total_logical_frames,
            gapless,
            format_info,
        })
    }

    /// Returns a reference to the gapless framing information extracted
    /// from the container at open time.
    pub fn gapless_info(&self) -> &GaplessInfo {
        &self.gapless
    }

    /// Returns the comprehensive format descriptor built at open time.
    pub fn format_info(&self) -> &AudioFormatInfo {
        &self.format_info
    }

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

/// Downmix an interleaved PCM slice to stereo using ITU-R BS.775 semantic channel matrix.
pub fn downmix_interleaved_to_stereo(
    samples: &[f32],
    layout: &ChannelLayout,
    src_channels: usize,
    plane_l: &mut [f32],
    plane_r: &mut [f32],
    frames: usize,
) -> usize {
    let actual_frames = (samples.len() / src_channels.max(1))
        .min(frames)
        .min(plane_l.len())
        .min(plane_r.len());

    if src_channels == 2 {
        for frame in 0..actual_frames {
            let idx = frame * 2;
            plane_l[frame] = samples[idx];
            plane_r[frame] = samples[idx + 1];
        }
        return actual_frames;
    }

    if src_channels == 1 {
        for frame in 0..actual_frames {
            let s = samples[frame];
            plane_l[frame] = s;
            plane_r[frame] = s;
        }
        return actual_frames;
    }

    if src_channels > 2 {
        use crate::decode::ChannelId;
        let fl_idx = layout.position_of(ChannelId::FrontLeft).unwrap_or(0);
        let fr_idx = layout.position_of(ChannelId::FrontRight).unwrap_or(1);
        let c_idx = layout.position_of(ChannelId::Center);
        let sl_idx = layout.position_of(ChannelId::SideLeft);
        let sr_idx = layout.position_of(ChannelId::SideRight);
        let rl_idx = layout.position_of(ChannelId::RearLeft);
        let rr_idx = layout.position_of(ChannelId::RearRight);
        let bc_idx = layout.position_of(ChannelId::BackCenter);

        const SQRT_HALF: f32 = 0.707_106_78;
        for frame in 0..actual_frames {
            let base = frame * src_channels;
            let fl = (base + fl_idx < samples.len())
                .then(|| samples[base + fl_idx])
                .unwrap_or(0.0);
            let fr = (base + fr_idx < samples.len())
                .then(|| samples[base + fr_idx])
                .unwrap_or(0.0);

            let center = c_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let sl = sl_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let sr = sr_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let rl = rl_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let rr = rr_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            // 6.1-style back center folds into both channels (BS.775 rear
            // scaling). LFE is intentionally excluded from the fold.
            let bc = bc_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);

            // Fold in front, center, side surrounds, back center and rear surrounds
            let l = fl + SQRT_HALF * center + SQRT_HALF * sl + 0.5 * bc + 0.5 * rl;
            let r = fr + SQRT_HALF * center + SQRT_HALF * sr + 0.5 * bc + 0.5 * rr;

            plane_l[frame] = l;
            plane_r[frame] = r;
        }
        return actual_frames;
    }

    actual_frames
}

/// Extract title, artist, album, duration_secs, and duration_str from an audio file.
pub fn extract_track_metadata(path: &Path) -> (String, String, String, f64, String) {
    let default_title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown Track")
        .to_string();
    let mut title = default_title.clone();
    let mut artist = "Unknown Artist".to_string();
    let mut album = "Unknown Album".to_string();
    let mut duration_secs = 0.0;

    if let Ok(file) = File::open(path) {
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let metadata_opts = MetadataOptions::default();
        let format_opts = FormatOptions::default();

        if let Ok(mut format_reader) =
            symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts)
        {
            if let Some(track) = format_reader.tracks().first() {
                if let Some(tb) = track.time_base {
                    if let Some(n_frames) = track.num_frames {
                        if let Some(time) = tb.calc_time(Timestamp::new(n_frames as i64)) {
                            duration_secs = time.as_secs_f64();
                        }
                    }
                }
            }

            if let Some(current) = format_reader.metadata().current() {
                for tag in &current.media.tags {
                    if let Some(std) = &tag.std {
                        match std {
                            StandardTag::TrackTitle(val) if !val.is_empty() => {
                                title = val.to_string();
                            }
                            StandardTag::Artist(val) if !val.is_empty() => {
                                artist = val.to_string();
                            }
                            StandardTag::Album(val) if !val.is_empty() => {
                                album = val.to_string();
                            }
                            _ => {}
                        }
                    } else {
                        let key_str = tag.raw.key.to_lowercase();
                        let val_str = tag.raw.value.to_string();
                        if (key_str.contains("title") || key_str == "tracktitle")
                            && !val_str.is_empty()
                        {
                            title = val_str;
                        } else if key_str.contains("artist") && !val_str.is_empty() {
                            artist = val_str;
                        } else if key_str.contains("album") && !val_str.is_empty() {
                            album = val_str;
                        }
                    }
                }
            }
        }
    }

    let duration_str = if duration_secs > 0.0 {
        format!(
            "{}:{:02}",
            (duration_secs as i32) / 60,
            (duration_secs as i32) % 60
        )
    } else {
        "0:00".to_string()
    };

    (title, artist, album, duration_secs, duration_str)
}

/// Extract ReplayGain / EBU R128 loudness metadata from file tags, for
/// Symphonia-probeable formats. Ogg Opus is handled by
/// `decode::extract_loudness_metadata` (OpusTags cannot be read by
/// Symphonia's probe), which dispatches here for everything else.
pub fn extract_loudness_metadata_symphonia(path: &Path) -> crate::dsp::loudness::LoudnessMetadata {
    use crate::dsp::loudness::LoudnessMetadata;

    let mut meta = LoudnessMetadata::default();

    let parse_f32 = |s: &str| -> Option<f32> {
        // Tags often look like "-6.34 dB" — strip non-numeric prefix/suffix.
        let trimmed: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
            .collect();
        trimmed.parse::<f32>().ok().filter(|v| v.is_finite())
    };

    // R128 tag values are integer LUFS × 100 (per the EBU R128 tag spec).
    // Some encoders write the value as a plain float LUFS string; we detect
    // both forms by attempting the integer-÷-100 conversion first.
    let parse_r128 = |s: &str| -> Option<f32> {
        let trimmed: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
            .collect();
        if let Ok(v) = trimmed.parse::<f32>() {
            if v.is_finite() {
                // Heuristic: if |v| > 200 it's almost certainly the encoded
                // integer form (a typical track is -23 LUFS = -2300 encoded).
                // Otherwise treat it as a plain LUFS value.
                if v.abs() > 200.0 {
                    return Some(v / 100.0);
                }
                return Some(v);
            }
        }
        None
    };

    if let Ok(file) = File::open(path) {
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let metadata_opts = MetadataOptions::default();
        let format_opts = FormatOptions::default();

        if let Ok(mut format_reader) =
            symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts)
        {
            if let Some(current) = format_reader.metadata().current() {
                for tag in &current.media.tags {
                    if let Some(std) = &tag.std {
                        match std {
                            StandardTag::ReplayGainTrackGain(v) => {
                                meta.replaygain_track_db = parse_f32(v);
                            }
                            StandardTag::ReplayGainAlbumGain(v) => {
                                meta.replaygain_album_db = parse_f32(v);
                            }
                            StandardTag::ReplayGainTrackPeak(v) => {
                                meta.replaygain_track_peak = parse_f32(v);
                            }
                            StandardTag::ReplayGainAlbumPeak(v) => {
                                meta.replaygain_album_peak = parse_f32(v);
                            }
                            _ => {}
                        }
                    }
                    let key = tag.raw.key.to_lowercase();
                    let value = tag.raw.value.to_string();
                    if value.is_empty() {
                        continue;
                    }
                    if key == "replaygain_track_gain" && meta.replaygain_track_db.is_none() {
                        meta.replaygain_track_db = parse_f32(&value);
                    } else if key == "replaygain_album_gain" && meta.replaygain_album_db.is_none() {
                        meta.replaygain_album_db = parse_f32(&value);
                    } else if key == "replaygain_track_peak" && meta.replaygain_track_peak.is_none()
                    {
                        meta.replaygain_track_peak = parse_f32(&value);
                    } else if key == "replaygain_album_peak" && meta.replaygain_album_peak.is_none()
                    {
                        meta.replaygain_album_peak = parse_f32(&value);
                    } else if key == "r128_track_gain" {
                        meta.ebu_r128_loudness = parse_r128(&value);
                    } else if key == "r128_album_gain" {
                        // Reuse the same field — AlbumReplayGain mode reads
                        // replaygain_album_db, but if only R128 tags are
                        // present we treat them as the track loudness.
                        if meta.ebu_r128_loudness.is_none() {
                            meta.ebu_r128_loudness = parse_r128(&value);
                        }
                    }
                }
            }
        }
    }

    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::GaplessInfo;

    #[test]
    fn test_decode_info() {
        let info = DecodeInfo {
            sample_rate: 44100,
            channels: 2,
            duration_secs: 180.0,
            codec: "mp3".to_string(),
            bitrate_kbps: Some(320),
        };
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 2);
    }

    /// The downmix matrix resolves channel SEMANTICS from the layout, not
    /// raw indices: the same signal at the Center role downmixes identically
    /// whether the layout puts Center at index 2 (5.1) or at a non-standard
    /// position (custom layout).
    #[test]
    fn test_downmix_uses_layout_semantics() {
        // 5.1 frame: FL=0.8, FR=-0.5, C=0.4, LFE=0.9, SL=0.3, SR=-0.2
        let five_one = [0.8f32, -0.5, 0.4, 0.9, 0.3, -0.2];
        let mut out_std = Vec::new();
        let layout_5_1 = crate::decode::ChannelLayout::FivePointOne;
        SymphoniaDecoder::extract_from_interleaved_f32(
            &five_one,
            &mut out_std,
            &layout_5_1,
            6,
            2,
            1,
        );
        let l_expected = 0.8 + 0.707_106_78 * 0.4 + 0.707_106_78 * 0.3;
        let r_expected = -0.5 + 0.707_106_78 * 0.4 + 0.707_106_78 * -0.2;
        assert!((out_std[0] - l_expected).abs() < 1e-5);
        assert!((out_std[1] - r_expected).abs() < 1e-5);
        // LFE must be dropped entirely (no 0.707*LFE term in the output).
        let lfe_contribution = out_std[0] - (0.8 + 0.707_106_78 * 0.4 + 0.707_106_78 * 0.3);
        assert!(lfe_contribution.abs() < 1e-5, "LFE must not be folded in");

        // Same content, but in a Custom layout where the roles are shuffled:
        // [SL, SR, C, FL, FR, LFE]. The matrix must follow the ROLES.
        let custom = crate::decode::ChannelLayout::Custom(vec![
            crate::decode::ChannelId::SideLeft,
            crate::decode::ChannelId::SideRight,
            crate::decode::ChannelId::Center,
            crate::decode::ChannelId::FrontLeft,
            crate::decode::ChannelId::FrontRight,
            crate::decode::ChannelId::Lfe,
        ]);
        // Interleave the same values according to the shuffled roles:
        // idx0=SL=0.3, idx1=SR=-0.2, idx2=C=0.4, idx3=FL=0.8, idx4=FR=-0.5, idx5=LFE=0.9
        let shuffled = [0.3f32, -0.2, 0.4, 0.8, -0.5, 0.9];
        let mut out_custom = Vec::new();
        SymphoniaDecoder::extract_from_interleaved_f32(
            &shuffled,
            &mut out_custom,
            &custom,
            6,
            2,
            1,
        );
        assert!(
            (out_custom[0] - out_std[0]).abs() < 1e-5,
            "downmix must follow semantics: {} vs {}",
            out_custom[0],
            out_std[0]
        );
        assert!((out_custom[1] - out_std[1]).abs() < 1e-5);
    }

    /// Regression for the gapless-after-seek bug: `seek()` must recompute
    /// `frames_to_skip` and `logical_frames_remaining` from the target
    /// PHYSICAL position instead of leaving the stale pre-seek values.
    #[test]
    fn test_gapless_state_after_seek() {
        // MP3-style framing: 529 delay frames, 529 end-padding, 200_000
        // logical frames => 201_058 physical frames.
        let gapless = GaplessInfo {
            encoder_delay: 529,
            end_padding: 529,
            priming_frames: 529,
            total_logical_frames: Some(200_000),
        };

        // Seek to physical 0: must re-arm the encoder-delay skip and restore
        // the FULL logical frame budget.
        let (skip, remaining) = gapless.state_after_seek(0);
        assert_eq!(skip, 529, "seeking to start must re-skip the encoder delay");
        assert_eq!(
            remaining,
            Some(200_000),
            "seeking to start must restore the full logical budget"
        );

        // Seek to the middle of the track: no delay ahead, logical budget
        // relative to the new position (logical 99_471 => 100_529 left).
        let (skip, remaining) = gapless.state_after_seek(100_000);
        assert_eq!(skip, 0, "mid-track seek has no encoder delay ahead");
        assert_eq!(remaining, Some(100_529));

        // Seek INSIDE the encoder-delay region: the remaining delay frames
        // must still be discarded (logical position 0).
        let (skip, remaining) = gapless.state_after_seek(300);
        assert_eq!(
            skip, 229,
            "seek inside the delay region keeps the tail of the delay"
        );
        assert_eq!(remaining, Some(200_000));

        // Seek near the end: budget shrinks to the remaining tail.
        let (skip, remaining) = gapless.state_after_seek(200_000);
        assert_eq!(skip, 0);
        assert_eq!(remaining, Some(529), "logical tail = 200_000 - 199_471");

        // A file with NO gapless metadata stays untouched.
        let plain = GaplessInfo::default();
        assert_eq!(plain.state_after_seek(12_345), (0, None));
    }
}

// ── MKA (Matroska audio) container smoke test ─────────────────────────────
// Builds a minimal-but-valid Matroska file with one 16-bit PCM stereo track
// and verifies Symphonia's `symphonia-format-mkv` reader (feature
// `codec-mkv`) decodes it end-to-end through the engine's decode path.

#[cfg(all(feature = "codec-mkv", test))]
mod mka_tests {
    use super::*;
    use crate::decode::Decoder;

    /// EBML vint for element sizes, matching the decoder used by
    /// symphonia-format-mkv: width = leading-zeros(first byte) + 1, the
    /// marker bit is `1 << (8 - width)`, and the value is packed as
    /// `(first_byte & ((1 << (8 - width)) - 1)) << (8 * (width - 1))` plus
    /// `width - 1` full 8-bit bytes. (Not the 7-bits-per-byte packing some
    /// EBML implementations use — that mismatch makes sizes decode to the
    /// wrong length and the reader loses the Segment.)
    fn vint_size(v: u64) -> Vec<u8> {
        for w in 1..=8u32 {
            // Value capacity for width w: (8 - w) + 8 * (w - 1) = 7w bits.
            let max = 1u64 << (7 * w);
            if v < max {
                let mut out = vec![0u8; w as usize];
                let mut val = v;
                // w - 1 full 8-bit bytes, least significant first.
                for i in (1..w).rev() {
                    out[i as usize] = (val & 0xFF) as u8;
                    val >>= 8;
                }
                // First byte: remaining (8 - w) value bits + marker.
                out[0] = (val as u8) | (0x80 >> (w - 1));
                return out;
            }
        }
        unreachable!()
    }

    /// Write an element: raw ID bytes (already including their vint marker
    /// bits, per the EBML ID table) + size vint + payload.
    fn element(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&vint_size(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn write_u16(v: u16) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }

    fn write_u32(v: u32) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }

    fn write_f64(v: f64) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }

    fn write_ascii(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    /// Build a minimal Matroska file: one audio track, 16-bit signed
    /// little-endian PCM, stereo, 44.1 kHz, `frames` frames of a 440 Hz sine.
    fn write_test_mka(frames: usize) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "engine_mka_test_{}_{}.mka",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        // ── Audio payload: interleaved 16-bit LE PCM ──
        let mut pcm: Vec<u8> = Vec::with_capacity(frames * 4);
        for i in 0..frames {
            let s = ((i as f64 * 440.0 * 2.0 * std::f64::consts::PI / 44_100.0).sin()
                * 0.5
                * 32767.0) as i16;
            pcm.extend_from_slice(&(s as u16).to_le_bytes());
            pcm.extend_from_slice(&(s as u16).to_le_bytes());
        }

        // ── EBML header ──
        // EBML header element IDs (RFC 8794): EBMLVersion=0x4286,
        // EBMLReadVersion=0x42F7, EBMLMaxIDLength=0x42F2,
        // EBMLMaxSizeLength=0x42F3, DocType=0x4282, DocTypeVersion=0x4287,
        // DocTypeReadVersion=0x4285.
        let mut ebml = Vec::new();
        ebml.extend_from_slice(&element(&[0x42, 0x86], &write_u16(1))); // EBMLVersion
        ebml.extend_from_slice(&element(&[0x42, 0xF7], &write_u16(1))); // EBMLReadVersion
        ebml.extend_from_slice(&element(&[0x42, 0xF2], &[4])); // EBMLMaxIDLength
        ebml.extend_from_slice(&element(&[0x42, 0xF3], &[8])); // EBMLMaxSizeLength
        ebml.extend_from_slice(&element(&[0x42, 0x82], &write_ascii("matroska"))); // DocType
        ebml.extend_from_slice(&element(&[0x42, 0x87], &write_u16(4))); // DocTypeVersion
        ebml.extend_from_slice(&element(&[0x42, 0x85], &write_u16(2))); // DocTypeReadVersion
        let ebml_elem = element(&[0x1A, 0x45, 0xDF, 0xA3], &ebml);

        // ── Segment children ──
        let mut info_inner = Vec::new();
        info_inner.extend_from_slice(&element(&[0x2A, 0xD7, 0xB1], &write_u32(1_000_000))); // TimestampScale
        info_inner.extend_from_slice(&element(
            &[0x44, 0x89],
            &write_f64(frames as f64 / 44_100.0), // Duration
        ));
        // Mandatory Info children per the Matroska spec / symphonia reader:
        // MuxingApp (0x4D80) and WritingApp (0x5741).
        info_inner.extend_from_slice(&element(&[0x4D, 0x80], &write_ascii("playtune-test"))); // MuxingApp
        info_inner.extend_from_slice(&element(&[0x57, 0x41], &write_ascii("playtune-test"))); // WritingApp
        let info = element(&[0x15, 0x49, 0xA9, 0x66], &info_inner);

        let mut audio_inner = Vec::new();
        audio_inner.extend_from_slice(&element(&[0xB5], &write_f64(44_100.0))); // SamplingFrequency
        audio_inner.extend_from_slice(&element(&[0x9F], &[2])); // Channels
        audio_inner.extend_from_slice(&element(&[98, 100], &write_u16(16))); // BitDepth
        let audio = element(&[0xE1], &audio_inner);
        let mut track_inner = Vec::new();
        track_inner.extend_from_slice(&element(&[0xD7], &[1])); // TrackNumber
        track_inner.extend_from_slice(&element(&[0x73, 0xC5], &[0x01])); // TrackUID
        track_inner.extend_from_slice(&element(&[131], &[2])); // TrackType = audio
        track_inner.extend_from_slice(&element(&[134], &write_ascii("A_PCM/INT/LIT"))); // CodecID
        track_inner.extend_from_slice(&audio); // Audio settings
        let track_entry = element(&[0xAE], &track_inner);
        let tracks = element(&[0x16, 0x54, 0xAE, 0x6B], &track_entry);

        // Multiple SimpleBlocks with increasing relative timestamps (ms), so
        // the demuxer's forward scan can seek into the track without Cues.
        const BLOCK_FRAMES: usize = 128;
        let mut cluster_inner = Vec::new();
        cluster_inner.extend_from_slice(&element(&[0xE7], &write_u16(0))); // Timestamp
        for (bi, chunk) in pcm.chunks(BLOCK_FRAMES * 4).enumerate() {
            let frame_offset = bi * BLOCK_FRAMES;
            let rel_ts_ms = (frame_offset * 1000 / 44_100) as i16;
            let mut block = Vec::new();
            block.push(0x81); // track number vint (1)
            block.extend_from_slice(&rel_ts_ms.to_be_bytes());
            block.push(0x00); // flags
            block.extend_from_slice(chunk);
            cluster_inner.extend_from_slice(&element(&[0xA3], &block));
        }
        let cluster = element(&[0x1F, 0x43, 0xB6, 0x75], &cluster_inner);

        let segment_content = {
            let mut inner = Vec::new();
            inner.extend_from_slice(&info);
            inner.extend_from_slice(&tracks);
            inner.extend_from_slice(&cluster);
            inner
        };
        // Segment size = content length (fixed, not "unknown").
        let segment = element(&[24, 83, 128, 103], &segment_content);

        let mut file = Vec::new();
        file.extend_from_slice(&ebml_elem);
        file.extend_from_slice(&segment);
        std::fs::write(&path, &file).unwrap();
        path
    }

    #[test]
    fn test_mka_pcm_decode_roundtrip() {
        let path = write_test_mka(4096);
        let mut dec = Decoder::open(&path).expect("open MKA");
        let info = dec.info().clone();
        assert_eq!(info.sample_rate, 44_100, "PCM track sample rate");
        assert_eq!(info.channels, 2, "stereo track");

        let mut total_frames = 0u64;
        let mut first_l: Option<f32> = None;
        loop {
            match dec.decode_next(1024) {
                Ok(chunk) => {
                    assert_eq!(chunk.channels, 2);
                    assert_eq!(chunk.sample_rate, 44_100);
                    assert!(chunk.frame_count > 0, "each chunk must carry frames");
                    if first_l.is_none() {
                        first_l = Some(chunk.samples[0]);
                    }
                    total_frames += chunk.frame_count as u64;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(total_frames, 4096, "all PCM frames must decode");
        // First frame: sine at sample 0 is 0.0 * 0.5 ≈ 0.
        let l = first_l.expect("decoded at least one sample");
        assert!(
            (l - 0.0).abs() < 1e-3,
            "first sample of a 440 Hz sine starting at phase 0 must be ~0, got {l}"
        );

        // Seek support: the demuxer seeks by scanning forward from the
        // current position (no Cue elements in this minimal file), so seek
        // while still mid-stream — this also exercises the engine's seek
        // path against a Matroska container.
        let mut dec2 = Decoder::open(&path).expect("open MKA for seek");
        // Decode a small prefix so the reader is positioned mid-stream.
        let mut prefix_frames = 0u64;
        loop {
            match dec2.decode_next(256) {
                Ok(chunk) => {
                    prefix_frames += chunk.frame_count as u64;
                    if prefix_frames >= 512 {
                        break;
                    }
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("prefix decode error: {e}"),
            }
        }
        assert!(prefix_frames >= 512, "decoded prefix must be mid-stream");
        // Seek forward past the prefix (0.05 s ≈ 2205 frames @ 44.1 kHz).
        dec2.seek(0.05).expect("MKA seek");
        let mut frames_after_seek = 0u64;
        loop {
            match dec2.decode_next(1024) {
                Ok(chunk) => {
                    assert!(chunk.frame_count > 0);
                    frames_after_seek += chunk.frame_count as u64;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode after seek: {e}"),
            }
        }
        assert!(
            frames_after_seek > 0,
            "must decode frames after seeking (got {frames_after_seek})"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// H2: the extended layout ladder maps conventional channel counts to
    /// named layouts with the right semantic roles.
    #[test]
    fn test_extended_layout_mapping() {
        use crate::decode::{ChannelId, ChannelLayout};
        let cases: &[(usize, ChannelLayout)] = &[
            (1, ChannelLayout::Mono),
            (2, ChannelLayout::Stereo),
            (3, ChannelLayout::ThreePointZero),
            (4, ChannelLayout::FourPointZero),
            (5, ChannelLayout::FivePointZero),
            (6, ChannelLayout::FivePointOne),
            (7, ChannelLayout::SevenPointZero),
            (8, ChannelLayout::SevenPointOne),
        ];
        for (count, layout) in cases {
            assert_eq!(
                ChannelLayout::from_count(*count),
                *layout,
                "from_count({count})"
            );
            assert_eq!(layout.channel_count(), *count, "channel_count");
            assert_eq!(layout.channel_ids().len(), *count, "channel_ids length");
        }

        // Semantic positions of the new layouts.
        assert_eq!(
            ChannelLayout::FivePointZero
                .position_of(ChannelId::Center)
                .unwrap_or(99),
            2
        );
        assert_eq!(
            ChannelLayout::SixPointOne
                .position_of(ChannelId::Lfe)
                .unwrap_or(99),
            3
        );
        assert_eq!(
            ChannelLayout::SixPointOne
                .position_of(ChannelId::BackCenter)
                .unwrap_or(99),
            6
        );
        assert_eq!(
            ChannelLayout::SevenPointZero
                .position_of(ChannelId::RearRight)
                .unwrap_or(99),
            6
        );
        assert_eq!(
            ChannelLayout::FourPointOne
                .position_of(ChannelId::Lfe)
                .unwrap_or(99),
            2
        );
        assert_eq!(
            ChannelLayout::ThreePointOne
                .position_of(ChannelId::Lfe)
                .unwrap_or(99),
            3
        );
    }

    /// H2: 6.1's back center folds into both L and R at 0.5; LFE stays out.
    #[test]
    fn test_downmix_six_point_one_back_center() {
        use crate::decode::ChannelLayout;
        // FL=0.8, FR=-0.5, C=0.4, LFE=0.9, SL=0.3, SR=-0.2, BC=0.6
        let six_one = [0.8f32, -0.5, 0.4, 0.9, 0.3, -0.2, 0.6];
        let mut out = Vec::new();
        SymphoniaDecoder::extract_from_interleaved_f32(
            &six_one,
            &mut out,
            &ChannelLayout::SixPointOne,
            7,
            2,
            1,
        );
        let l_expected = 0.8 + 0.707_106_78 * 0.4 + 0.707_106_78 * 0.3 + 0.5 * 0.6;
        let r_expected = -0.5 + 0.707_106_78 * 0.4 + 0.707_106_78 * -0.2 + 0.5 * 0.6;
        assert!((out[0] - l_expected).abs() < 1e-5);
        assert!((out[1] - r_expected).abs() < 1e-5);
    }

    /// H2: 5.0 downmixes without an LFE slot and with C at index 2.
    #[test]
    fn test_downmix_five_point_zero() {
        use crate::decode::ChannelLayout;
        // FL=0.8, FR=-0.5, C=0.4, SL=0.3, SR=-0.2
        let five_zero = [0.8f32, -0.5, 0.4, 0.3, -0.2];
        let mut out = Vec::new();
        SymphoniaDecoder::extract_from_interleaved_f32(
            &five_zero,
            &mut out,
            &ChannelLayout::FivePointZero,
            5,
            2,
            1,
        );
        let l_expected = 0.8 + 0.707_106_78 * 0.4 + 0.707_106_78 * 0.3;
        let r_expected = -0.5 + 0.707_106_78 * 0.4 + 0.707_106_78 * -0.2;
        assert!((out[0] - l_expected).abs() < 1e-5);
        assert!((out[1] - r_expected).abs() < 1e-5);
    }
}
