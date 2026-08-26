//! Source opening and Symphonia probe setup.

use std::fs::File;
use std::io::Cursor;
use std::path::Path;

use symphonia::core::{
    codecs::audio::{AudioDecoderOptions, CODEC_ID_NULL_AUDIO},
    formats::{probe::Hint, FormatOptions},
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
};

use crate::decode::symphonia_decoder::{DecodeError, DecodeInfo, SymphoniaDecoder};
use crate::decode::{AudioFormatInfo, ChannelLayout, GaplessInfo};

impl SymphoniaDecoder {
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path)
            .map_err(|e| DecodeError::FileOpen(format!("Cannot open {}: {}", path.display(), e)))?;

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        Self::open_media_source(
            Box::new(file),
            hint,
            path.extension().and_then(|e| e.to_str()),
        )
    }

    /// Open in-memory byte buffer for decoding with an optional file extension hint.
    pub fn open_memory(data: Vec<u8>, extension_hint: Option<&str>) -> Result<Self, DecodeError> {
        let cursor = Cursor::new(data);
        let mut hint = Hint::new();
        if let Some(ext) = extension_hint {
            hint.with_extension(ext);
        }

        Self::open_media_source(Box::new(cursor), hint, extension_hint)
    }

    /// Open any `MediaSource` stream for decoding.
    pub fn open_media_source(
        media_source: Box<dyn MediaSource>,
        hint: Hint,
        container_hint: Option<&str>,
    ) -> Result<Self, DecodeError> {
        let mss = MediaSourceStream::new(media_source, Default::default());

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
        let num_frames_physical = track.num_frames;
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

        let bit_depth = audio_params.bits_per_sample;
        let sample_format = match bit_depth {
            Some(32) => "i32".to_string(),
            Some(24) => "i24".to_string(),
            Some(16) => "i16".to_string(),
            _ => "f32".to_string(),
        };

        let format_info = AudioFormatInfo {
            codec: codec_str.clone(),
            container: container_hint.unwrap_or("unknown").to_uppercase(),
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

    /// Open from a generic byte source by buffering into memory.
    pub fn open_from_source(
        mut source: Box<dyn crate::audio_io::AudioByteSource>,
    ) -> Result<Self, DecodeError> {
        let ext = source.extension().to_string();
        let mut data = Vec::new();
        use std::io::Read;
        source
            .read_to_end(&mut data)
            .map_err(|e| DecodeError::FileOpen(format!("Cannot read source: {}", e)))?;
        Self::open_memory(data, Some(&ext))
    }
}
