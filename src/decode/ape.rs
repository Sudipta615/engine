//! Monkey's Audio (APE) decoding.
//!
//! The entropy decoder, adaptive predictor and PCM reconstruction are provided
//! by the pure-Rust [`ape-decoder`](https://crates.io/crates/ape-decoder)
//! crate (range coder + per-compression-level predictor + CRC-verified frame
//! reconstruction). This module is an **engine-facing adapter**: it wraps that
//! decoder and exposes the same interface as [`SymphoniaDecoder`] and
//! [`DsdDecoder`] so `.ape` / `.mac` files flow through the unified
//! [`crate::decode::Decoder`] dispatch.
//!
//! ## Scope
//!
//! - Decodes APE v3.95+ files (version < 3950 is rejected by the backend).
//! - Sample formats: 8 / 16 / 24 / 32-bit integer. Floating-point APE
//!   (`APE_FORMAT_FLAG_FLOATING_POINT`) is extremely rare and treated as 32-bit
//!   integer PCM; the backend's own float transform is a documented no-op.
//! - Mono, stereo and multichannel (up to 32 channels) sources are converted
//!   to interleaved `f32` and handed to the engine, which applies its usual
//!   stereo downmix boundary exactly as it does for Symphonia sources.
//! - Sample-accurate seeking via the backend's seek table; each APE frame is
//!   independently decodable (predictors are flushed per frame), so seeking
//!   does not require decoding from the start of the file.

#![cfg(feature = "codec-ape")]

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::decode::{AudioFormatInfo, ChannelLayout, DecodeError, DecodeInfo, DecodedChunk};

/// Map the backend's errors onto the engine's unified [`DecodeError`].
impl From<ape_decoder::ApeError> for DecodeError {
    fn from(e: ape_decoder::ApeError) -> Self {
        match e {
            ape_decoder::ApeError::Io(io) => DecodeError::Io(io),
            ape_decoder::ApeError::UnsupportedVersion(v) => {
                DecodeError::UnsupportedFormat(format!("Unsupported Monkey's Audio version: {}", v))
            }
            ape_decoder::ApeError::InvalidFormat(m) => {
                DecodeError::UnsupportedFormat(format!("Invalid APE format: {}", m))
            }
            ape_decoder::ApeError::InvalidChecksum => {
                DecodeError::Decode("APE frame checksum mismatch".to_string())
            }
            ape_decoder::ApeError::DecodingError(m) => {
                DecodeError::Decode(format!("APE decode error: {}", m))
            }
            // `ApeError` is `#[non_exhaustive]`; future variants degrade to a
            // generic decode error rather than a compile break.
            other => DecodeError::Decode(format!("APE decode error: {}", other)),
        }
    }
}

/// Engine-facing Monkey's Audio decoder.
pub struct ApeDecoder {
    inner: ape_decoder::ApeDecoder<BufReader<File>>,
    info: DecodeInfo,
    format_info: AudioFormatInfo,
    /// Source PCM bit depth (8 / 16 / 24 / 32).
    bits_per_sample: u16,
    /// Total compressed frames in the file.
    total_frames: u32,
    /// Interleaved f32 samples already decoded but not yet handed out.
    pending: Vec<f32>,
    /// Read offset (in samples) into `pending`.
    pending_pos: usize,
    /// Index of the next compressed frame to decode.
    next_frame: u32,
    /// Samples (per channel) to drop from the start of the next decoded
    /// frame, set by [`Self::seek`].
    skip_samples: u64,
    /// True once every frame has been decoded and `pending` is drained.
    eof: bool,
}

impl ApeDecoder {
    /// Open an APE file for playback.
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path)
            .map_err(|e| DecodeError::FileOpen(format!("Cannot open {}: {}", path.display(), e)))?;
        let inner =
            ape_decoder::ApeDecoder::new(BufReader::new(file)).map_err(DecodeError::from)?;

        let sample_rate = inner.info().sample_rate;
        let channels = inner.info().channels as usize;
        let bits_per_sample = inner.info().bits_per_sample;
        let total_frames = inner.info().total_frames;
        let duration_secs = inner.info().duration_ms as f32 / 1000.0;
        let bitrate_kbps =
            (inner.info().average_bitrate_kbps > 0).then_some(inner.info().average_bitrate_kbps);
        let channel_layout = ChannelLayout::from_count(channels);

        let info = DecodeInfo {
            sample_rate,
            channels,
            duration_secs,
            codec: "APE".to_string(),
            bitrate_kbps,
        };

        let sample_format = if inner.info().is_floating_point {
            "f32".to_string()
        } else {
            format!("i{}", bits_per_sample)
        };

        let format_info = AudioFormatInfo {
            codec: "APE".to_string(),
            container: "Monkey's Audio (APE)".to_string(),
            sample_rate,
            input_sample_rate: None,
            channels,
            channel_layout,
            bit_depth: Some(bits_per_sample as u32),
            sample_format,
            duration_secs: Some(duration_secs as f64),
            bitrate_kbps,
            gapless: None,
            replaygain_track_db: None,
            replaygain_album_db: None,
            ebu_r128_loudness: None,
            true_peak_dbtp: None,
            is_lossless: true,
            is_dsd: false,
        };

        Ok(Self {
            inner,
            info,
            format_info,
            bits_per_sample,
            total_frames,
            pending: Vec::with_capacity(4096 * channels),
            pending_pos: 0,
            next_frame: 0,
            skip_samples: 0,
            eof: false,
        })
    }

    /// Decode the next chunk of up to `max_frames` interleaved frames.
    pub fn decode_next(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        let channels = self.info.channels;
        let max_frames = max_frames.max(1);
        let mut out: Vec<f32> = Vec::with_capacity(max_frames * channels);

        while out.len() / channels < max_frames {
            // Refill `pending` once it is fully consumed.
            if self.pending_pos >= self.pending.len() {
                if self.eof || self.next_frame >= self.total_frames {
                    self.eof = true;
                    break;
                }

                let frame_idx = self.next_frame;
                let bytes = self
                    .inner
                    .decode_frame(frame_idx)
                    .map_err(DecodeError::from)?;
                self.next_frame += 1;

                self.pending.clear();
                convert_pcm_to_f32(&bytes, self.bits_per_sample, &mut self.pending);

                // Apply any in-frame seek skip before handing the frame out.
                self.pending_pos = (self.skip_samples as usize * channels).min(self.pending.len());
                self.skip_samples = 0;
            }

            let available = self.pending.len() - self.pending_pos;
            let needed = max_frames * channels - out.len();
            let take = available.min(needed);
            out.extend_from_slice(&self.pending[self.pending_pos..self.pending_pos + take]);
            self.pending_pos += take;
        }

        if out.is_empty() {
            return Err(DecodeError::EndOfStream);
        }

        let frame_count = out.len() / channels;
        Ok(DecodedChunk {
            samples: out,
            channels,
            channel_layout: self.format_info.channel_layout.clone(),
            sample_rate: self.info.sample_rate,
            frame_count,
            raw_dsd: None,
        })
    }

    /// Seek to a position in seconds (source-sample domain).
    pub fn seek(&mut self, position_secs: f32) -> Result<(), DecodeError> {
        if !position_secs.is_finite() || position_secs < 0.0 {
            return Err(DecodeError::Seek(format!(
                "Invalid seek position: {}",
                position_secs
            )));
        }
        let sample_rate = self.info.sample_rate as f64;
        let target = ((position_secs as f64) * sample_rate).round().max(0.0) as u64;
        let result = self.inner.seek(target).map_err(DecodeError::from)?;

        self.next_frame = result.frame_index;
        self.skip_samples = result.skip_samples as u64;
        self.pending.clear();
        self.pending_pos = 0;
        self.eof = false;
        Ok(())
    }

    pub fn info(&self) -> &DecodeInfo {
        &self.info
    }

    pub fn duration_secs(&self) -> f32 {
        self.info.duration_secs
    }

    pub fn format_info(&self) -> &AudioFormatInfo {
        &self.format_info
    }
}

/// Convert little-endian integer PCM bytes to normalized interleaved `f32`.
///
/// The backend emits 8-bit as unsigned (0..255, biased +128), 16/32-bit as
/// signed little-endian, and 24-bit as signed little-endian two's complement
/// (the backend's "special negative" encoding reduces to exactly that).
fn convert_pcm_to_f32(bytes: &[u8], bits: u16, out: &mut Vec<f32>) {
    match bits {
        8 => {
            out.reserve(bytes.len());
            for &b in bytes {
                out.push((b as i32 - 128) as f32 / 128.0);
            }
        }
        16 => {
            out.reserve(bytes.len() / 2);
            for c in bytes.as_chunks::<2>().0 {
                out.push(i16::from_le_bytes(*c) as f32 / 32768.0);
            }
        }
        24 => {
            out.reserve(bytes.len() / 3);
            for c in bytes.as_chunks::<3>().0 {
                let u = c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16;
                let s = ((u << 8) as i32) >> 8; // sign-extend the 24-bit value
                out.push(s as f32 / 8_388_608.0);
            }
        }
        32 => {
            out.reserve(bytes.len() / 4);
            for c in bytes.as_chunks::<4>().0 {
                out.push(i32::from_le_bytes(*c) as f32 / 2_147_483_648.0);
            }
        }
        _ => {
            // The backend only emits 8/16/24/32-bit integer PCM; any other
            // width is silently ignored rather than producing garbage.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_8bit_unsigned_bias() {
        let mut out = Vec::new();
        // 0 -> -1.0, 128 -> 0.0, 255 -> ~1.0
        convert_pcm_to_f32(&[0, 128, 255], 8, &mut out);
        assert_eq!(out.len(), 3);
        assert!((out[0] - (-1.0)).abs() < 1e-6);
        assert_eq!(out[1], 0.0);
        assert!((out[2] - (127.0 / 128.0)).abs() < 1e-6);
    }

    #[test]
    fn convert_16bit_stereo() {
        let mut out = Vec::new();
        // Frame 1: L=-32768 (min), R=32767 (max). Frame 2: 0, 16384.
        let bytes = [
            0x00, 0x80, // -32768
            0xFF, 0x7F, // 32767
            0x00, 0x00, // 0
            0x00, 0x40, // 16384
        ];
        convert_pcm_to_f32(&bytes, 16, &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], -1.0);
        assert!((out[1] - (32767.0 / 32768.0)).abs() < 1e-6);
        assert_eq!(out[2], 0.0);
        assert_eq!(out[3], 0.5);
    }

    #[test]
    fn convert_24bit_sign_extension() {
        let mut out = Vec::new();
        // 0x000001 -> 1, 0xFFFFFF -> -1, 0x800000 -> -8388608 (min), 0x7FFFFF -> max.
        let bytes = [
            0x01, 0x00, 0x00, // 1
            0xFF, 0xFF, 0xFF, // -1
            0x00, 0x00, 0x80, // -8388608
            0xFF, 0xFF, 0x7F, // 8388607
        ];
        convert_pcm_to_f32(&bytes, 24, &mut out);
        assert_eq!(out.len(), 4);
        assert!((out[0] - (1.0 / 8_388_608.0)).abs() < 1e-12);
        assert!((out[1] - (-1.0 / 8_388_608.0)).abs() < 1e-12);
        assert_eq!(out[2], -1.0);
        assert!((out[3] - (8_388_607.0 / 8_388_608.0)).abs() < 1e-12);
    }

    #[test]
    fn convert_32bit_extremes() {
        let mut out = Vec::new();
        let bytes = [
            0x00, 0x00, 0x00, 0x80, // i32::MIN
            0xFF, 0xFF, 0xFF, 0x7F, // i32::MAX
        ];
        convert_pcm_to_f32(&bytes, 32, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], -1.0);
        assert!((out[1] - (2_147_483_647.0 / 2_147_483_648.0)).abs() < 1e-9);
    }

    #[test]
    fn convert_unknown_width_ignored() {
        let mut out = Vec::new();
        convert_pcm_to_f32(&[1, 2, 3, 4], 12, &mut out);
        assert!(out.is_empty());
    }
}
