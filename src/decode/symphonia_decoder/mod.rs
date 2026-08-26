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
use symphonia::core::codecs::audio::AudioDecoder;
use symphonia::core::formats::FormatReader;
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

mod decode;
mod downmix;
mod metadata;
mod source;

pub use downmix::downmix_interleaved_to_stereo;
pub use metadata::{extract_loudness_metadata_symphonia, extract_track_metadata};

impl SymphoniaDecoder {
    pub fn gapless_info(&self) -> &GaplessInfo {
        &self.gapless
    }

    /// Returns the comprehensive format descriptor built at open time.
    pub fn format_info(&self) -> &AudioFormatInfo {
        &self.format_info
    }
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
        let l_expected =
            0.8 + std::f32::consts::FRAC_1_SQRT_2 * 0.4 + std::f32::consts::FRAC_1_SQRT_2 * 0.3;
        let r_expected =
            -0.5 + std::f32::consts::FRAC_1_SQRT_2 * 0.4 + std::f32::consts::FRAC_1_SQRT_2 * -0.2;
        assert!((out_std[0] - l_expected).abs() < 1e-5);
        assert!((out_std[1] - r_expected).abs() < 1e-5);
        // LFE must be dropped entirely (no 0.707*LFE term in the output).
        let lfe_contribution = out_std[0]
            - (0.8 + std::f32::consts::FRAC_1_SQRT_2 * 0.4 + std::f32::consts::FRAC_1_SQRT_2 * 0.3);
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
        let l_expected = 0.8
            + std::f32::consts::FRAC_1_SQRT_2 * 0.4
            + std::f32::consts::FRAC_1_SQRT_2 * 0.3
            + 0.5 * 0.6;
        let r_expected = -0.5
            + std::f32::consts::FRAC_1_SQRT_2 * 0.4
            + std::f32::consts::FRAC_1_SQRT_2 * -0.2
            + 0.5 * 0.6;
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
        let l_expected =
            0.8 + std::f32::consts::FRAC_1_SQRT_2 * 0.4 + std::f32::consts::FRAC_1_SQRT_2 * 0.3;
        let r_expected =
            -0.5 + std::f32::consts::FRAC_1_SQRT_2 * 0.4 + std::f32::consts::FRAC_1_SQRT_2 * -0.2;
        assert!((out[0] - l_expected).abs() < 1e-5);
        assert!((out[1] - r_expected).abs() < 1e-5);
    }
}
