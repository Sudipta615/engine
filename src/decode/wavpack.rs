//! WavPack (`.wv`) decoding via the pure-Rust `wavicle` codec.
//!
//! `wavicle` (MIT/Apache-2.0, `#![forbid(unsafe_code)]`, no FFI) losslessly
//! decodes WavPack v5 streams: 16/24/32-bit integer and bit-exact 32-bit
//! float, mono and stereo. This module adapts it behind the engine's unified
//! [`Decoder`](crate::decode::decoder::Decoder) interface, mirroring the
//! `ApeDecoder` / `TtaDecoder` pattern.
//!
//! # Incremental, bounded-memory decode
//!
//! `wavicle`'s public entry point (`decode_stream`) decodes a whole byte
//! slice; its block decoder is private. We keep the memory profile bounded
//! by exploiting the format's structure: WavPack blocks are **independent**
//! (each carries its own decorrelation state), so a single block slice
//! decodes standalone. [`WavpackDecoder`] therefore:
//!
//! 1. scans block headers at open (32-byte reads, O(1) memory) to build a
//!    block index (`first_frame` → byte offset),
//! 2. reads and decodes **one block at a time** on demand
//!    (`wavicle::decode_stream` over exactly that block's bytes),
//! 3. serves `decode_next` chunks directly out of the cached decoded block
//!    and seeks by jumping to the block containing the target frame.
//!
//! Peak memory is one encoded block (bounded by the format's 1 MB limit)
//! plus one decoded block, independent of file length.
//!
//! # Honest scope rejection (never a silent downgrade)
//!
//! `wavicle` refuses — and so do we, at open time with a codec-named error —
//! the WavPack families it does not implement:
//!
//! - **multichannel** (`>2` channels: `CHANNEL_INFO` count, or mono/stereo
//!   blocks spanning a channel group — the `initial`/`final` block-flag
//!   pattern),
//! - **DSD** and **hybrid/lossy** modes (block-header flags),
//! - **correction files** and pre-4.0 legacy streams (header version range).
//!
//! A valid stereo/mono `.wv` decodes; everything else fails loudly. The
//! capability registry (`codecs.rs`) mirrors this: `decode/seek = true`,
//! `multichannel = false`.
//!
//! # Known limitations (documented, not hidden)
//!
//! - Metadata/tag extraction (APEv2 or ID3 tags commonly appended to `.wv`)
//!   is **not** wired yet: `metadata = false`, `replaygain = false` in the
//!   registry. The engine's metadata extractors fall back to Symphonia's
//!   probe, which does not recognise WavPack, and return empty/defaults.
//! - Block-level CRC mismatches and over-magnitude samples are hard decode
//!   errors (`wavicle` never silently mutes a corrupt block).
//! - Decode memory is bounded per block as described above; the *encoded*
//!   file itself is read on demand via `std::io` seeks, never loaded whole.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use wavicle::block::{Block, BlockHeader};
use wavicle::error::Error as WavicleError;
use wavicle::format::meta;

use crate::decode::{AudioFormatInfo, ChannelLayout, DecodeError, DecodeInfo, DecodedChunk};

/// Maximum bytes of one encoded block (format limit, mirrored from wavicle).
/// Kept here so the adapter's read path never allocates unbounded memory
/// from a malicious `block_len` claim.
const MAX_BLOCK_BYTES: usize = 1 << 20;

/// One indexed audio block: where it lives in the file and which source
/// frames it covers.
#[derive(Debug, Clone, Copy)]
struct BlockEntry {
    /// Byte offset of the 32-byte block header.
    start: u64,
    /// On-disk length of the whole block including the header.
    len: usize,
    /// First source frame index of this block.
    first_frame: u64,
    /// Frames in this block.
    frames: u32,
}

/// Fixed stream facts gathered from the first audio block.
#[derive(Debug, Clone, Copy)]
struct StreamFacts {
    bits_per_sample: u32,
    is_float: bool,
    channels: u32,
    sample_rate: u32,
    total_samples: Option<u64>,
}

/// Engine-facing WavPack decoder (see module docs for the design).
pub struct WavpackDecoder {
    file: BufReader<File>,
    blocks: Vec<BlockEntry>,
    facts: StreamFacts,
    info: DecodeInfo,
    format_info: AudioFormatInfo,
    /// Block currently decoded into `cached_samples` (index into `blocks`).
    current_block: usize,
    /// Source frames consumed within the current block.
    block_consumed: u64,
    /// Decoded interleaved i32 samples of the current block (native bit
    /// depth for integers, raw IEEE-754 bit patterns for floats).
    cached_samples: Vec<i32>,
    /// Reusable scratch for the current block's encoded bytes.
    block_bytes: Vec<u8>,
    reached_eof: bool,
}

impl WavpackDecoder {
    /// Open a `.wv` file: scan the block index and validate the stream's
    /// format. Rejects multichannel / DSD / hybrid WavPack explicitly.
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path)
            .map_err(|e| DecodeError::FileOpen(format!("Cannot open {}: {}", path.display(), e)))?;
        let mut reader = BufReader::new(file);

        let (blocks, facts) = scan_blocks(&mut reader)?;
        let facts = facts.ok_or_else(|| {
            DecodeError::UnsupportedFormat("WavPack stream contains no audio blocks".to_string())
        })?;

        let channels = facts.channels as usize;
        let total_samples = facts.total_samples.unwrap_or_else(|| {
            blocks
                .last()
                .map(|b| b.first_frame + b.frames as u64)
                .unwrap_or(0)
        });
        let duration_secs = if facts.sample_rate > 0 {
            total_samples as f64 / facts.sample_rate as f64
        } else {
            0.0
        };
        // Average bitrate from the encoded block bytes (excluding trailing
        // tags, which the scanner stops at).
        let encoded_bytes: u64 = blocks.iter().map(|b| b.len as u64).sum();
        let bitrate_kbps = if duration_secs > 1e-9 {
            ((encoded_bytes * 8) as f64 / duration_secs / 1000.0).round() as u32
        } else {
            0
        };

        let codec = "WavPack".to_string();
        let info = DecodeInfo {
            sample_rate: facts.sample_rate,
            channels,
            duration_secs: duration_secs as f32,
            codec: codec.clone(),
            bitrate_kbps: (bitrate_kbps > 0).then_some(bitrate_kbps),
        };

        let sample_format = if facts.is_float {
            "f32".to_string()
        } else {
            format!("i{}", facts.bits_per_sample)
        };
        let format_info = AudioFormatInfo {
            codec,
            container: "WavPack (.wv)".to_string(),
            sample_rate: facts.sample_rate,
            input_sample_rate: None,
            channels,
            channel_layout: ChannelLayout::from_count(channels),
            bit_depth: Some(facts.bits_per_sample),
            sample_format,
            duration_secs: Some(duration_secs),
            bitrate_kbps: (bitrate_kbps > 0).then_some(bitrate_kbps),
            gapless: None,
            replaygain_track_db: None,
            replaygain_album_db: None,
            ebu_r128_loudness: None,
            true_peak_dbtp: None,
            is_lossless: true,
            is_dsd: false,
        };

        let mut decoder = Self {
            file: reader,
            blocks,
            facts,
            info,
            format_info,
            current_block: 0,
            block_consumed: 0,
            cached_samples: Vec::new(),
            block_bytes: Vec::with_capacity(MAX_BLOCK_BYTES),
            reached_eof: false,
        };
        // Decode the first block eagerly so a CRC/scope failure in the very
        // first audio block surfaces at open (like other backends).
        decoder.load_block(0)?;
        Ok(decoder)
    }

    /// Read the block at `blocks[block_idx]` and decode it into
    /// `cached_samples`, resetting per-block decode state.
    fn load_block(&mut self, block_idx: usize) -> Result<(), DecodeError> {
        let entry = self.blocks[block_idx];
        self.block_bytes.clear();
        if entry.len > MAX_BLOCK_BYTES {
            return Err(DecodeError::Decode(format!(
                "WavPack block {block_idx} exceeds the {} byte format limit",
                MAX_BLOCK_BYTES
            )));
        }
        self.file
            .seek(SeekFrom::Start(entry.start))
            .map_err(DecodeError::Io)?;
        self.block_bytes.resize(entry.len, 0);
        self.file.read_exact(&mut self.block_bytes).map_err(|e| {
            DecodeError::Decode(format!("truncated WavPack block {block_idx}: {e}"))
        })?;

        let samples = wavicle::decode_stream(&self.block_bytes)
            .map_err(|e| map_wavicle_error(format!("WavPack block {block_idx}: {e}"), &e))?;
        debug_assert_eq!(
            samples.channels, self.facts.channels,
            "block channel count must match the stream facts"
        );
        // The decoded block holds exactly `block_samples` frames per channel
        // (decode_stream errors otherwise), so the slice is a whole block.
        self.cached_samples = samples.samples;
        self.current_block = block_idx;
        self.block_consumed = 0;
        Ok(())
    }

    /// Convert one decoded i32 sample to f32 (integer: native-depth scaling;
    /// float: exact bit-pattern reinterpretation).
    #[inline]
    fn sample_to_f32(&self, v: i32) -> f32 {
        if self.facts.is_float {
            f32::from_bits(v as u32)
        } else {
            let max = 1u64 << (self.facts.bits_per_sample - 1);
            (v as f64 / max as f64) as f32
        }
    }

    /// Decode the next chunk of up to `max_frames` interleaved frames,
    /// serving directly out of the currently decoded block.
    pub fn decode_next(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        let max_frames = max_frames.max(1);
        let channels = self.facts.channels as usize;
        let mut out: Vec<f32> = Vec::with_capacity(max_frames * channels);
        let mut have_frames = 0usize;

        while have_frames < max_frames {
            let Some(entry) = self.blocks.get(self.current_block).copied() else {
                self.reached_eof = true;
                break;
            };
            if self.block_consumed >= entry.frames as u64 {
                // Current block exhausted: advance (or end).
                if self.current_block + 1 < self.blocks.len() {
                    self.load_block(self.current_block + 1)?;
                } else {
                    self.reached_eof = true;
                    break;
                }
                continue;
            }
            let take = ((entry.frames as u64 - self.block_consumed) as usize)
                .min(max_frames - have_frames);
            let start = (self.block_consumed as usize) * channels;
            let end = start + take * channels;
            debug_assert!(end <= self.cached_samples.len());
            for &s in &self.cached_samples[start..end] {
                out.push(self.sample_to_f32(s));
            }
            self.block_consumed += take as u64;
            have_frames += take;
        }

        if have_frames == 0 {
            return Err(DecodeError::EndOfStream);
        }
        Ok(DecodedChunk {
            samples: out,
            channels,
            channel_layout: ChannelLayout::from_count(channels),
            sample_rate: self.facts.sample_rate,
            frame_count: have_frames,
            raw_dsd: None,
        })
    }

    /// Seek to a position in seconds (sample-accurate via the block index).
    pub fn seek(&mut self, position_secs: f32) -> Result<(), DecodeError> {
        if !position_secs.is_finite() || position_secs < 0.0 {
            return Err(DecodeError::Seek(format!(
                "invalid WavPack seek position: {position_secs}"
            )));
        }
        // Round rather than truncate: the f32 seconds value already carries
        // representation error, and truncation would land one frame early on
        // values like 40000/44100 that round-trip slightly below the target.
        let target = (position_secs as f64 * self.facts.sample_rate as f64).round() as u64;
        let idx = self
            .blocks
            .binary_search_by_key(&target, |b| b.first_frame)
            .unwrap_or_else(|insert| insert.saturating_sub(1));
        if idx >= self.blocks.len() {
            // Past the end: land at the last block's end.
            let last = self.blocks.len() - 1;
            self.load_block(last)?;
            self.block_consumed = self.blocks[last].frames as u64;
            self.reached_eof = true;
            return Ok(());
        }
        self.load_block(idx)?;
        let entry = self.blocks[idx];
        self.block_consumed = target
            .saturating_sub(entry.first_frame)
            .min(entry.frames as u64);
        self.reached_eof = false;
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

/// Scan the file's block headers into an index, gathering stream facts from
/// the first audio block. Stops at the first non-block byte (trailing
/// APEv2/ID3 tags) instead of erroring, matching how real `.wv` files are
/// laid out. Rejects — with a codec-named error — DSD / hybrid /
/// multichannel WavPack, non-contiguous blocks, and out-of-range versions.
fn scan_blocks(
    reader: &mut BufReader<File>,
) -> Result<(Vec<BlockEntry>, Option<StreamFacts>), DecodeError> {
    let mut blocks: Vec<BlockEntry> = Vec::new();
    let mut facts: Option<StreamFacts> = None;
    let mut offset: u64 = 0;
    let mut expected_frame: u64 = 0;

    let mut header = [0u8; 32];
    loop {
        reader
            .seek(SeekFrom::Start(offset))
            .map_err(DecodeError::Io)?;
        let mut filled = 0usize;
        while filled < header.len() {
            match reader.read(&mut header[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => break,
            }
        }
        if filled < header.len() {
            break; // clean EOF (or trailing short tag bytes)
        }
        if header[0..4] != *b"wvpk" {
            break; // trailing tag / non-block data: stop scanning
        }
        let parsed = BlockHeader::parse(&header)
            .map_err(|e| map_wavicle_error(format!("WavPack header at byte {offset}: {e}"), &e))?;
        let len = parsed.block_len();
        if parsed.block_samples == 0 {
            offset += len as u64;
            continue; // metadata-only block
        }
        // A mono/stereo WavPack block carries BOTH initial and final flags;
        // a multi-block channel group (multichannel) sets only one on each
        // member, so this check rejects multichannel at the header level.
        if !(parsed.flags.initial_block() && parsed.flags.final_block()) {
            return Err(DecodeError::UnsupportedFormat(
                "WavPack multichannel streams (mono/stereo blocks spanning a \
                 channel group) are not supported by the pure-Rust decoder \
                 (stereo/mono only)"
                    .to_string(),
            ));
        }
        if parsed.block_index != expected_frame {
            return Err(DecodeError::Decode(format!(
                "non-contiguous WavPack blocks: expected frame {expected_frame}, found {}",
                parsed.block_index
            )));
        }
        expected_frame += parsed.block_samples as u64;

        let entry = BlockEntry {
            start: offset,
            len,
            first_frame: parsed.block_index,
            frames: parsed.block_samples,
        };
        if facts.is_none() {
            // First audio block: full validation (scope sub-blocks) and the
            // stream facts (channel count, custom sample rate).
            facts = Some(inspect_first_block(reader, &entry)?);
        }
        blocks.push(entry);
        offset += len as u64;
    }

    // A multichannel rejection is only meaningful when audio exists.
    if let Some(facts) = facts.as_ref() {
        if facts.channels > 2 {
            return Err(DecodeError::UnsupportedFormat(
                "WavPack multichannel (more than two channels) is not supported \
                 by the pure-Rust decoder (stereo/mono only)"
                    .to_string(),
            ));
        }
    }
    Ok((blocks, facts))
}

/// Read one whole block, run wavicle's scope gate over its sub-blocks, and
/// gather the stream facts (channels, bit depth, float-ness, rate, total).
fn inspect_first_block(
    reader: &mut BufReader<File>,
    entry: &BlockEntry,
) -> Result<StreamFacts, DecodeError> {
    let mut bytes = vec![0u8; entry.len];
    reader
        .seek(SeekFrom::Start(entry.start))
        .map_err(DecodeError::Io)?;
    reader
        .read_exact(&mut bytes)
        .map_err(|e| DecodeError::Decode(format!("truncated WavPack first block: {e}")))?;

    let header = BlockHeader::parse(&bytes[..32])
        .map_err(|e| map_wavicle_error(format!("WavPack header: {e}"), &e))?;
    let block = Block {
        header,
        metadata: &bytes[32..],
    };

    let mut channels = header.flags.output_channels();
    let mut custom_rate: Option<u32> = None;
    for sub in block.sub_blocks() {
        let sub = sub.map_err(|e| map_wavicle_error(format!("WavPack metadata: {e}"), &e))?;
        wavicle::metadata::check_scope(sub)
            .map_err(|e| map_wavicle_error(format!("WavPack metadata: {e}"), &e))?;
        if sub.id == meta::SAMPLE_RATE && sub.data.len() >= 3 {
            custom_rate = Some(
                u32::from(sub.data[0]) | u32::from(sub.data[1]) << 8 | u32::from(sub.data[2]) << 16,
            );
        }
        if sub.id == meta::CHANNEL_INFO {
            if let Some(&n) = sub.data.first() {
                channels = u32::from(n);
            }
        }
    }
    let sample_rate = header.flags.sample_rate().or(custom_rate).ok_or_else(|| {
        DecodeError::UnsupportedFormat("WavPack stream declares no sample rate".to_string())
    })?;

    Ok(StreamFacts {
        bits_per_sample: header.flags.bytes_per_sample() * 8,
        is_float: header.flags.is_float(),
        channels,
        sample_rate,
        total_samples: header.total_samples,
    })
}

/// Map a `wavicle` error onto the engine's [`DecodeError`], keeping the
/// out-of-scope families as explicit, codec-named format rejections.
fn map_wavicle_error(context: impl Into<String>, e: &WavicleError) -> DecodeError {
    let context = context.into();
    match e {
        WavicleError::OutOfScope(scope) => {
            let family = match scope {
                wavicle::error::Scope::Dsd => "DSD audio",
                wavicle::error::Scope::Hybrid => "hybrid/lossy mode",
                wavicle::error::Scope::CorrectionFile => "correction-file (.wvc) content",
                wavicle::error::Scope::MoreThanTwoChannels => "more than two channels",
                wavicle::error::Scope::MultichannelSpanning => "multichannel block spanning",
            };
            DecodeError::UnsupportedFormat(format!(
                "WavPack {family} is not supported by the pure-Rust decoder: {context}"
            ))
        }
        WavicleError::NotYetImplemented(what) => DecodeError::UnsupportedFormat(format!(
            "WavPack {what} is not implemented by the pure-Rust decoder: {context}"
        )),
        WavicleError::BadMagic(_) | WavicleError::BadBlockSize(_) => {
            DecodeError::UnsupportedFormat(format!("not a valid WavPack file: {context}"))
        }
        _ => DecodeError::Decode(context),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A tiny deterministic pseudo-random generator (xorshift) so fixture
    /// signals are reproducible without an external dependency.
    fn signal(frames: usize, channels: usize, bits: u32, seed: u32) -> Vec<i32> {
        let max = (1i64 << (bits - 1)) - 1;
        let mut state = seed;
        let mut out = Vec::with_capacity(frames * channels);
        for i in 0..frames * channels {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            // Mix in a deterministic ramp so the signal is not pure noise.
            let v = ((state as i64 % (max * 2 + 1)) - max) + (i as i64 % 97) as i64;
            out.push(v.clamp(-max, max) as i32);
        }
        out
    }

    fn temp_path(ext: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "engine-wavpack-test-{}-{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            ext
        ))
    }

    fn write_fixture(bytes: &[u8], ext: &str) -> PathBuf {
        let path = temp_path(ext);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn encode_fixture(samples: &[i32], channels: u32, rate: u32, bits: u32) -> Vec<u8> {
        wavicle::encode_int(
            wavicle::EncodeParams {
                channels,
                sample_rate: rate,
                bits_per_sample: bits,
            },
            samples,
        )
        .expect("encode")
    }

    #[test]
    fn round_trip_i16_stereo_is_lossless() {
        let channels = 2u32;
        let frames = 8192usize;
        let bits = 16u32;
        let src = signal(frames, channels as usize, bits, 0x1234_5678);
        let bytes = encode_fixture(&src, channels, 44_100, bits);
        let path = write_fixture(&bytes, "wv");

        let mut dec = WavpackDecoder::open(&path).expect("open");
        assert_eq!(dec.info.sample_rate, 44_100);
        assert_eq!(dec.info.channels, 2);
        assert_eq!(dec.facts.bits_per_sample, 16);
        assert!(!dec.facts.is_float);
        assert!((dec.info.duration_secs - (frames as f32 / 44_100.0)).abs() < 1e-3);

        let mut got: Vec<f32> = Vec::new();
        loop {
            match dec.decode_next(512) {
                Ok(c) => {
                    assert_eq!(c.channels, 2);
                    assert_eq!(c.sample_rate, 44_100);
                    assert_eq!(c.samples.len(), c.frame_count * 2);
                    got.extend_from_slice(&c.samples);
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(got.len(), src.len(), "all frames decoded");
        for (i, &s) in src.iter().enumerate() {
            let expected = (s as f64 / 32768.0) as f32;
            assert!(
                (got[i] - expected).abs() <= 1e-6,
                "sample {i}: got {}, expected {}",
                got[i],
                expected
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trip_i24_stereo_is_lossless() {
        let channels = 2u32;
        let frames = 5000usize;
        let bits = 24u32;
        let src = signal(frames, channels as usize, bits, 0x99);
        let bytes = encode_fixture(&src, channels, 96_000, bits);
        let path = write_fixture(&bytes, "wv");

        let mut dec = WavpackDecoder::open(&path).expect("open");
        assert_eq!(dec.facts.bits_per_sample, 24);
        assert_eq!(dec.info.sample_rate, 96_000);
        let mut total = 0usize;
        loop {
            match dec.decode_next(1024) {
                Ok(c) => total += c.frame_count,
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(total, frames);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trip_f32_is_bit_exact() {
        let channels = 2u32;
        let frames = 4000usize;
        // Deterministic float signal with full dynamic range (kept inside
        // wavicle's magnitude bound so encode succeeds).
        let mut src_f: Vec<f32> = Vec::with_capacity(frames * 2);
        let mut state = 0xC0FFEEu32;
        for i in 0..frames * 2 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let base = ((state % 2_000_000) as f32 / 1_000_000.0) - 1.0;
            let tone = ((i as f32 * 0.01).sin()) * 0.5;
            src_f.push(base * 0.5 + tone);
        }
        let bytes = wavicle::encode_float(channels, 48_000, &src_f).expect("encode float");
        let path = write_fixture(&bytes, "wv");

        let mut dec = WavpackDecoder::open(&path).expect("open");
        assert!(dec.facts.is_float, "stream must be flagged float");
        assert_eq!(dec.facts.bits_per_sample, 32);
        let mut got: Vec<f32> = Vec::new();
        loop {
            match dec.decode_next(256) {
                Ok(c) => got.extend_from_slice(&c.samples),
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(got.len(), src_f.len());
        for (i, (&g, &s)) in got.iter().zip(src_f.iter()).enumerate() {
            assert_eq!(
                g.to_bits(),
                s.to_bits(),
                "float sample {i} must be bit-exact"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mono_file_decodes_to_one_channel() {
        let channels = 1u32;
        let frames = 3000usize;
        let bits = 16u32;
        let src = signal(frames, 1, bits, 7);
        let bytes = encode_fixture(&src, channels, 44_100, bits);
        let path = write_fixture(&bytes, "wv");

        let mut dec = WavpackDecoder::open(&path).expect("open");
        assert_eq!(dec.info.channels, 1);
        let mut got_frames = 0usize;
        loop {
            match dec.decode_next(256) {
                Ok(c) => {
                    assert_eq!(c.channels, 1);
                    assert_eq!(c.samples.len(), c.frame_count);
                    got_frames += c.frame_count;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(got_frames, frames);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn seek_lands_on_exact_frames() {
        let channels = 2u32;
        let frames = 200_000usize; // > wavicle's 32768 BLOCK_FRAMES → multi-block
        let bits = 16u32;
        let src = signal(frames, channels as usize, bits, 0xABCD);
        let bytes = encode_fixture(&src, channels, 44_100, bits);
        let path = write_fixture(&bytes, "wv");

        let mut dec = WavpackDecoder::open(&path).expect("open");
        assert!(
            dec.blocks.len() >= 2,
            "long fixture must span multiple blocks ({} blocks)",
            dec.blocks.len()
        );
        // Seek to a frame inside the second block.
        let target_frame = 40_000u64;
        let target_secs = target_frame as f32 / 44_100.0;
        dec.seek(target_secs).expect("seek");

        let mut got: Vec<f32> = Vec::new();
        loop {
            match dec.decode_next(1024) {
                Ok(c) => got.extend_from_slice(&c.samples),
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        // 200000 - 40000 = 160000 frames × 2 channels.
        assert_eq!(
            got.len(),
            (frames - target_frame as usize) * channels as usize
        );
        // The post-seek stream must match the source region exactly (within
        // f32 rounding of the 16-bit values).
        for (i, &s) in got.iter().enumerate() {
            let expected =
                (src[target_frame as usize * channels as usize + i] as f64 / 32768.0) as f32;
            assert!(
                (s - expected).abs() <= 1e-6,
                "post-seek sample {i} mismatch: got {}, expected {}",
                s,
                expected
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn garbage_bytes_are_rejected_without_panic() {
        let path = write_fixture(b"this is not a wavpack file at all", "wv");
        match WavpackDecoder::open(&path) {
            Err(DecodeError::UnsupportedFormat(msg)) => {
                assert!(msg.contains("WavPack"), "message must name WavPack: {msg}");
            }
            other => panic!("garbage .wv must not open: {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncated_file_is_rejected_cleanly() {
        let channels = 2u32;
        let frames = 4096usize;
        let src = signal(frames, channels as usize, 16, 3);
        let mut bytes = encode_fixture(&src, channels, 44_100, 16);
        // Truncate mid-block.
        bytes.truncate(bytes.len() / 2);
        let path = write_fixture(&bytes, "wv");
        match WavpackDecoder::open(&path) {
            Err(_) => {} // any clean error is fine (no panic)
            Ok(_) => {
                // A truncation that happens to land on a block boundary is
                // also acceptable; decoding must then error, not panic.
                let mut dec = WavpackDecoder::open(&path).expect("open");
                let ended = loop {
                    match dec.decode_next(512) {
                        Ok(_) => {}
                        Err(_) => break true,
                    }
                };
                assert!(ended, "truncated stream must eventually error");
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn multichannel_stream_is_rejected_explicitly() {
        // Build a valid mono stream, then rewrite its first block's
        // CHANNEL_INFO sub-block to declare 6 channels. wavicle's scope gate
        // must reject it at open with a codec-named error.
        let channels = 1u32;
        let frames = 2048usize;
        let src = signal(frames, 1, 16, 5);
        let bytes = encode_fixture(&src, channels, 44_100, 16);
        // Insert a CHANNEL_INFO sub-block (id 0x0d, 1 word, [6, 0]) right
        // after the 32-byte header of the first block.
        let mut with_info = Vec::with_capacity(bytes.len() + 4);
        with_info.extend_from_slice(&bytes[..32]);
        with_info.extend_from_slice(&[meta::CHANNEL_INFO, 1, 6, 0]);
        with_info.extend_from_slice(&bytes[32..]);
        // The header's ck_size must grow by 4 to match the new block length.
        let old_size = u32::from_le_bytes(with_info[4..8].try_into().unwrap());
        with_info[4..8].copy_from_slice(&(old_size + 4).to_le_bytes());

        let path = write_fixture(&with_info, "wv");
        match WavpackDecoder::open(&path) {
            Err(DecodeError::UnsupportedFormat(msg)) => {
                assert!(
                    msg.to_lowercase().contains("channel"),
                    "multichannel rejection must mention channels: {msg}"
                );
            }
            other => panic!("multichannel .wv must not open: {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dsd_and_hybrid_flags_are_rejected() {
        let channels = 2u32;
        let frames = 2048usize;
        let src = signal(frames, channels as usize, 16, 9);
        let bytes = encode_fixture(&src, channels, 44_100, 16);
        // DSD flag = bit 31 of the header flags word (offset 24).
        for (bit, label) in [(31u32, "DSD"), (3u32, "hybrid")] {
            let mut mutated = bytes.clone();
            let flags = u32::from_le_bytes(mutated[24..28].try_into().unwrap());
            mutated[24..28].copy_from_slice(&(flags | (1 << bit)).to_le_bytes());
            let path = write_fixture(&mutated, "wv");
            match WavpackDecoder::open(&path) {
                Err(DecodeError::UnsupportedFormat(msg)) => {
                    assert!(
                        msg.contains("WavPack"),
                        "{label} rejection must name WavPack: {msg}"
                    );
                }
                other => panic!("{label} .wv must not open: {:?}", other.err()),
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn trailing_tag_bytes_are_ignored() {
        // Real .wv files often carry an APEv2 tag appended after the last
        // block. The scanner must stop at the non-block bytes, not error.
        let channels = 2u32;
        let frames = 3000usize;
        let src = signal(frames, channels as usize, 16, 11);
        let mut bytes = encode_fixture(&src, channels, 44_100, 16);
        // Minimal APEv2-style footer: "APETAGEX" + garbage.
        bytes.extend_from_slice(b"APETAGEX");
        bytes.extend_from_slice(&[0u8; 40]);
        let path = write_fixture(&bytes, "wv");

        let mut dec = WavpackDecoder::open(&path).expect("open with trailing tag");
        let mut total = 0usize;
        loop {
            match dec.decode_next(512) {
                Ok(c) => total += c.frame_count,
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(total, frames);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn long_file_spans_blocks_without_frames_dropped() {
        let channels = 2u32;
        let frames = 500_000usize;
        let src = signal(frames, channels as usize, 24, 0xFEED);
        let bytes = encode_fixture(&src, channels, 192_000, 24);
        let path = write_fixture(&bytes, "wv");

        let mut dec = WavpackDecoder::open(&path).expect("open");
        let mut total = 0usize;
        loop {
            match dec.decode_next(4096) {
                Ok(c) => total += c.frame_count,
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(total, frames, "no frames dropped across blocks");
        assert!(dec.blocks.len() > 1, "500k frames must span blocks");
        let _ = std::fs::remove_file(&path);
    }
}
