//! Source decoder dispatch.
//!
//! The engine uses one unified decoder interface. Routing rules:
//!
//! | Extension / format | Backend | Status |
//! |--------------------|---------|--------|
//! | `.dsf` / `.dff`    | Native DSD reader + decimator | ✅ Available |
//! | `.mp3`, `.flac`, `.ogg`, `.wav`, `.aac`, `.m4a` | Symphonia | ✅ Available |
//! | `.ape` / `.mac`    | `ape-decoder` (range coder + predictor) | ✅ Available (`codec-ape`) |
//! | `.alac` / `.m4a` (ALAC inner) | Symphonia 0.6.0 ALAC codec | ✅ Available |
//! | `.opus`            | `ogg` + pure-Rust `opus-decoder` (RFC 8251) | ✅ Available (`codec-opus`) |
//! | `.oga`             | content sniff: Opus → Opus backend, else Symphonia (Vorbis) | ✅ Available |
//! | `.tta`             | native pure-Rust TTA1 decoder | ✅ Available (`codec-tta`) |
//! | `.wv`              | `wavicle` pure-Rust v5 codec (16/24/32-bit int + f32, mono/stereo) | ✅ Available (`codec-wavpack`) — multichannel/DSD/hybrid `.wv` rejected explicitly |
//! | `.tak`             | none | ⛔ DeclaredUnavailable — rejected with an explicit error |
//!
//! The [`Decoder`] enum wraps all backends so the engine never needs to know
//! which backend a track uses.
//!
//! ## APE backend
//!
//! Monkey's Audio audio decoding is supplied by the pure-Rust `ape-decoder`
//! crate (enabled by the `codec-ape` feature). The engine-facing adapter in
//! `decode/ape.rs` wraps it behind the same interface as the DSD and Symphonia
//! backends; sample-accurate seeking uses the file's seek table and each APE
//! frame is independently decodable.
//!
//! ## ALAC container integration
//!
//! Symphonia provides both the ISO/MP4 demuxer and the ALAC codec
//! (`symphonia-codec-alac`) when the crate's `codec-alac` feature is enabled,
//! so this dispatch path handles real `.m4a` packets and metadata through a
//! single, upstream-maintained ALAC backend. A separate standalone ALAC frame
//! decoder was removed to avoid maintaining two implementations of the same
//! codec; Symphonia's is the only ALAC path.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::audio_io::AudioByteSource;

/// Buffer a seekable byte source to a temporary file, then invoke `op` with
/// the resulting file path. Returns the closure's result and cleans up the
/// temp file (even on error). Used as a fallback bridge for codec backends
/// (APE, Opus, WavPack, TTA) whose third-party crates require a concrete
/// reader type that cannot be satisfied by a trait object.
fn buf_and_open_file<T>(
    mut source: Box<dyn AudioByteSource>,
    op: impl FnOnce(&Path) -> Result<T, DecodeError>,
) -> Result<T, DecodeError> {
    let _ = source.seek(SeekFrom::Start(0));
    let mut buf = Vec::new();
    source
        .read_to_end(&mut buf)
        .map_err(|e| DecodeError::FileOpen(format!("reading byte source: {}", e)))?;

    let ext = source.extension().to_string();
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "playtune_decoder_bridge_{}_{}.{}",
        std::process::id(),
        buf.len(),
        ext
    ));

    {
        let mut file = std::fs::File::create(&path)
            .map_err(|e| DecodeError::FileOpen(format!("temp file: {}", e)))?;
        file.write_all(&buf)
            .map_err(|e| DecodeError::FileOpen(format!("writing temp file: {}", e)))?;
    }

    let result = op(&path);
    let _ = std::fs::remove_file(&path);
    result
}

use crate::decode::dsd::{DsdDecoder, DsdError};
#[cfg(feature = "codec-ape")]
use crate::decode::ApeDecoder;
#[cfg(feature = "codec-opus")]
use crate::decode::OpusSource;
#[cfg(feature = "codec-tta")]
use crate::decode::TtaDecoder;
#[cfg(feature = "codec-wavpack")]
use crate::decode::WavpackDecoder;
use crate::decode::{AudioFormatInfo, DecodeError, DecodeInfo, DecodedChunk, SymphoniaDecoder};

/// A decoded source: native DSD (DSF/DFF), Monkey's Audio (APE), Ogg Opus
/// (RFC 7845), TTA (True Audio), or a Symphonia-supported codec.
pub enum Decoder {
    Symphonia(SymphoniaDecoder),
    Dsd(DsdDecoder),
    #[cfg(feature = "codec-ape")]
    Ape(ApeDecoder),
    #[cfg(feature = "codec-opus")]
    Opus(OpusSource),
    #[cfg(feature = "codec-tta")]
    Tta(TtaDecoder),
    #[cfg(feature = "codec-wavpack")]
    Wavpack(WavpackDecoder),
}

impl Decoder {
    /// Open a file for decoding.
    ///
    /// - `.dsf` / `.dff` → native DSD reader
    /// - `.ape` / `.mac` → `ape-decoder` backend (when `codec-ape` is enabled)
    /// - everything else → Symphonia
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let source = crate::audio_io::FileByteSource::open(path)
            .map_err(|e| DecodeError::FileOpen(e.to_string()))?;
        Self::open_from_source(Box::new(source))
    }

    /// Open in-memory byte buffer for decoding.
    pub fn open_memory(data: Vec<u8>, extension_hint: Option<&str>) -> Result<Self, DecodeError> {
        let source = crate::audio_io::MemoryByteSource::new(data, extension_hint.unwrap_or(""));
        Self::open_from_source(Box::new(source))
    }

    /// Open an arbitrary byte source for decoding.
    ///
    /// The extension returned by [`AudioByteSource::extension`] determines
    /// which decoder backend is selected (same routing as [`Self::open`]).
    /// For codecs whose third-party crate requires a concrete reader type
    /// (APE, Opus, WavPack, TTA), the source is fully buffered into memory
    /// before the existing path-based or memory-based constructor is invoked.
    pub fn open_from_source(source: Box<dyn AudioByteSource>) -> Result<Self, DecodeError> {
        let ext = source.extension().to_string();

        if ext.eq_ignore_ascii_case("dsf") || ext.eq_ignore_ascii_case("dff") {
            return DsdDecoder::open_from_source(source).map(Self::Dsd);
        }
        if ext.eq_ignore_ascii_case("ape") || ext.eq_ignore_ascii_case("mac") {
            #[cfg(feature = "codec-ape")]
            {
                return buf_and_open_file(source, |path| ApeDecoder::open(path).map(Self::Ape));
            }
            #[cfg(not(feature = "codec-ape"))]
            {
                return Err(DecodeError::UnsupportedFormat(
                    "Monkey's Audio (APE) audio decoding is not enabled in this build. \
                     Rebuild with the `codec-ape` feature to decode `.ape` / `.mac` files."
                        .into(),
                ));
            }
        }
        if ext.eq_ignore_ascii_case("opus") {
            #[cfg(feature = "codec-opus")]
            {
                return buf_and_open_file(source, |path| OpusSource::open(path).map(Self::Opus));
            }
            #[cfg(not(feature = "codec-opus"))]
            {
                return Err(DecodeError::UnsupportedFormat(
                    "Opus audio decoding is not enabled in this build. \
                     Rebuild with the `codec-opus` feature to decode `.opus` files."
                        .into(),
                ));
            }
        }
        if ext.eq_ignore_ascii_case("oga") {
            // `.oga` may hold Opus or Vorbis: for byte sources, defer to
            // Symphonia which probes the container correctly. The Opus probe
            // requires a filesystem path (it opens the file a second time).
            return SymphoniaDecoder::open_from_source(source).map(Self::Symphonia);
        }
        if ext.eq_ignore_ascii_case("wv") {
            #[cfg(feature = "codec-wavpack")]
            {
                return buf_and_open_file(source, |path| {
                    WavpackDecoder::open(path).map(Self::Wavpack)
                });
            }
            #[cfg(not(feature = "codec-wavpack"))]
            {
                return Err(DecodeError::UnsupportedFormat(
                    "WavPack decoding is not enabled in this build. \
                     Rebuild with the `codec-wavpack` feature to decode `.wv` files."
                        .into(),
                ));
            }
        }
        if ext.eq_ignore_ascii_case("mpc")
            || ext.eq_ignore_ascii_case("mp+")
            || ext.eq_ignore_ascii_case("mpp")
        {
            return Err(DecodeError::UnsupportedFormat(
                "Musepack audio decoding is not available in this build: no Musepack \
                 decoder is wired in. `.mpc` files cannot be decoded."
                    .into(),
            ));
        }
        if ext.eq_ignore_ascii_case("tak") {
            return Err(DecodeError::UnsupportedFormat(
                "TAK (Tom's lossless Audio Kompressor) decoding is not available in \
                 this build: no pure-Rust TAK decoder is wired in. `.tak` files \
                 cannot be decoded."
                    .into(),
            ));
        }
        if ext.eq_ignore_ascii_case("tta") {
            #[cfg(feature = "codec-tta")]
            {
                return buf_and_open_file(source, |path| TtaDecoder::open(path).map(Self::Tta));
            }
            #[cfg(not(feature = "codec-tta"))]
            {
                return Err(DecodeError::UnsupportedFormat(
                    "TTA (True Audio) decoding is not enabled in this build. \
                     Rebuild with the `codec-tta` feature to decode `.tta` files."
                        .into(),
                ));
            }
        }

        // Everything else: Symphonia as universal fallback.
        SymphoniaDecoder::open_from_source(source).map(Self::Symphonia)
    }

    /// Open an [`AudioSource`] for decoding.
    pub fn open_source(source: &crate::source::AudioSource) -> Result<Self, DecodeError> {
        match source {
            crate::source::AudioSource::File(path) => Self::open(path),
            crate::source::AudioSource::Uri(uri) => {
                if let Some(stripped) = uri.strip_prefix("file://") {
                    let decoded = crate::decode::percent_decode(stripped)
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| std::path::PathBuf::from(stripped));
                    Self::open(&decoded)
                } else {
                    Self::open(std::path::Path::new(uri))
                }
            }
            crate::source::AudioSource::Memory {
                data,
                extension_hint,
            } => Self::open_memory(data.clone(), extension_hint.as_deref()),
        }
    }

    /// Decode the next chunk of up to `max_frames` PCM frames.
    pub fn decode_next(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        match self {
            Self::Symphonia(d) => d.decode_next(max_frames),
            Self::Dsd(d) => d.decode_next(max_frames),
            #[cfg(feature = "codec-ape")]
            Self::Ape(d) => d.decode_next(max_frames),
            #[cfg(feature = "codec-opus")]
            Self::Opus(d) => d.decode_next(max_frames),
            #[cfg(feature = "codec-tta")]
            Self::Tta(d) => d.decode_next(max_frames),
            #[cfg(feature = "codec-wavpack")]
            Self::Wavpack(d) => d.decode_next(max_frames),
        }
    }

    /// Seek to a position in seconds.
    pub fn seek(&mut self, position_secs: f32) -> Result<(), DecodeError> {
        match self {
            Self::Symphonia(d) => d.seek(position_secs),
            Self::Dsd(d) => d.seek(position_secs),
            #[cfg(feature = "codec-ape")]
            Self::Ape(d) => d.seek(position_secs),
            #[cfg(feature = "codec-opus")]
            Self::Opus(d) => d.seek(position_secs),
            #[cfg(feature = "codec-tta")]
            Self::Tta(d) => d.seek(position_secs),
            #[cfg(feature = "codec-wavpack")]
            Self::Wavpack(d) => d.seek(position_secs),
        }
    }

    pub fn info(&self) -> &DecodeInfo {
        match self {
            Self::Symphonia(d) => d.info(),
            Self::Dsd(d) => d.info(),
            #[cfg(feature = "codec-ape")]
            Self::Ape(d) => d.info(),
            #[cfg(feature = "codec-opus")]
            Self::Opus(d) => d.info(),
            #[cfg(feature = "codec-tta")]
            Self::Tta(d) => d.info(),
            #[cfg(feature = "codec-wavpack")]
            Self::Wavpack(d) => d.info(),
        }
    }

    pub fn duration_secs(&self) -> f32 {
        match self {
            Self::Symphonia(d) => d.duration_secs(),
            Self::Dsd(d) => d.duration_secs(),
            #[cfg(feature = "codec-ape")]
            Self::Ape(d) => d.duration_secs(),
            #[cfg(feature = "codec-opus")]
            Self::Opus(d) => d.duration_secs(),
            #[cfg(feature = "codec-tta")]
            Self::Tta(d) => d.duration_secs(),
            #[cfg(feature = "codec-wavpack")]
            Self::Wavpack(d) => d.duration_secs(),
        }
    }

    /// Comprehensive format descriptor (UI display etc.).
    pub fn format_info(&self) -> &AudioFormatInfo {
        match self {
            Self::Symphonia(d) => d.format_info(),
            Self::Dsd(d) => d.format_info(),
            #[cfg(feature = "codec-ape")]
            Self::Ape(d) => d.format_info(),
            #[cfg(feature = "codec-opus")]
            Self::Opus(d) => d.format_info(),
            #[cfg(feature = "codec-tta")]
            Self::Tta(d) => d.format_info(),
            #[cfg(feature = "codec-wavpack")]
            Self::Wavpack(d) => d.format_info(),
        }
    }

    /// True when the source is a DSD file (DSF/DFF).
    pub fn is_dsd(&self) -> bool {
        matches!(self, Self::Dsd(_))
    }

    /// Toggle DoP mode on a DSD source (no-op for other sources).
    pub fn set_dop_mode(&mut self, dop: bool) {
        if let Self::Dsd(d) = self {
            d.set_dop_mode(dop);
        }
    }

    /// Toggle native-DSD transport mode on a DSD source (no-op otherwise).
    pub fn set_native_dsd_mode(&mut self, native: bool) {
        if let Self::Dsd(d) = self {
            d.set_native_dsd_mode(native);
        }
    }

    /// The DSD bit rate for a DSD source (e.g. 2_822_400 for DSD64); `None`
    /// for non-DSD sources.
    pub fn dsd_bit_rate(&self) -> Option<u32> {
        match self {
            Self::Dsd(d) => Some(d.dsd_bit_rate()),
            _ => None,
        }
    }

    /// True when the source decoder is in native-DSD transport mode.
    pub fn is_native_dsd(&self) -> bool {
        match self {
            Self::Dsd(d) => d.is_native_dsd(),
            _ => false,
        }
    }

    /// The DoP output rate (bit_rate / 16) when the decoder is actively in DoP
    /// mode; `None` otherwise.
    pub fn dop_rate(&self) -> Option<u32> {
        match self {
            Self::Dsd(d) => d.dop_rate(),
            Self::Symphonia(_) => None,
            #[cfg(feature = "codec-ape")]
            Self::Ape(_) => None,
            #[cfg(feature = "codec-opus")]
            Self::Opus(_) => None,
            #[cfg(feature = "codec-tta")]
            Self::Tta(_) => None,
            #[cfg(feature = "codec-wavpack")]
            Self::Wavpack(_) => None,
        }
    }
}

/// Map native DSD errors onto the engine's unified [`DecodeError`].
impl From<DsdError> for DecodeError {
    fn from(e: DsdError) -> Self {
        match e {
            DsdError::InvalidHeader(m) => DecodeError::UnsupportedFormat(m),
            DsdError::UnsupportedChannels(n) => {
                DecodeError::UnsupportedFormat(format!("Unsupported DSD channel count: {}", n))
            }
            DsdError::UnsupportedRate(r) => {
                DecodeError::UnsupportedFormat(format!("Unsupported DSD sample rate: {}", r))
            }
            DsdError::UnsupportedCompression(m) => DecodeError::UnsupportedFormat(m),
            DsdError::Io(e) => DecodeError::Io(e),
        }
    }
}

/// DSD source adapter: presents the DSD reader's output through the engine's
/// decoder interface.
///
/// - **PCM mode** (default): DSD64 decimates to 88.2 kHz f32 PCM (32×) with
///   **every source channel** exposed (center / LFE / surrounds for 5.1 DSF),
///   which flows through the resampler / pipeline like any other source.
/// - **DoP mode**: the raw 1-bit DSD bitstream is packed into 24-bit
///   DSD-over-PCM frames at `bit_rate / 16` (176.4 kHz for DSD64) and shipped
///   bit-exactly to a DoP-capable DAC; no decimation, no resampling, no DSP.
///
/// DoP is stereo-only: `set_dop_mode(true)` is refused for sources with more
/// than two channels (the decoder stays in decimation mode and `dop_rate()`
/// returns `None`).
#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path(ext: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "playtune_decoder_test_{}_{}.{}",
            std::process::id(),
            n,
            ext
        ))
    }

    /// Minimal DSF container: stereo, LSB-first, block_size per channel,
    /// `frames` DSD frames per channel (padded to whole blocks), payload
    /// bytes cycling 0..255.
    fn build_dsf(block_size: u32, frames: u64) -> Vec<u8> {
        let ch0: Vec<u8> = (0..frames).map(|i| (i % 256) as u8).collect();
        let ch1: Vec<u8> = (0..frames).map(|i| ((i * 7 + 3) % 256) as u8).collect();
        let padded = frames.div_ceil(block_size as u64) * block_size as u64;

        let mut out = Vec::new();
        out.extend_from_slice(b"DSD ");
        out.extend_from_slice(&28u64.to_le_bytes());
        let total_size_pos = out.len();
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());

        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&52u64.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&2_822_400u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(padded * 8).to_le_bytes());
        out.extend_from_slice(&block_size.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());

        // Interleaved per-block channel layout: [ch0 block][ch1 block]…
        let mut audio = vec![0u8; (padded * 2) as usize];
        for (ch, data) in [&ch0[..], &ch1[..]].iter().enumerate() {
            for (b, chunk) in data.chunks(block_size as usize).enumerate() {
                let base = (b * block_size as usize) * 2 + ch * block_size as usize;
                audio[base..base + chunk.len()].copy_from_slice(chunk);
            }
        }
        out.extend_from_slice(b"data");
        out.extend_from_slice(&((audio.len() as u64) + 12).to_le_bytes());
        out.extend_from_slice(&audio);

        let total = out.len() as u64;
        out[total_size_pos..total_size_pos + 8].copy_from_slice(&total.to_le_bytes());
        out
    }

    #[test]
    fn test_decoder_dispatch_dsf() {
        let path = temp_path("dsf");
        std::fs::write(&path, build_dsf(4096, 4096)).unwrap();
        assert!(matches!(Decoder::open(&path).unwrap(), Decoder::Dsd(_)));
        let _ = std::fs::remove_file(&path);
    }

    /// With `codec-ape` enabled, `.ape` / `.mac` must route to the APE
    /// backend. A file with no "MAC " magic is rejected by that backend with
    /// its own "Invalid APE format" message (Symphonia would report a
    /// "Probe failed" instead), which proves the dispatch hit the right arm.
    #[cfg(feature = "codec-ape")]
    #[test]
    fn test_decoder_dispatch_ape_uses_ape_backend() {
        for ext in ["ape", "mac"] {
            let path = temp_path(ext);
            std::fs::write(&path, b"not a real monkey audio file").unwrap();
            match Decoder::open(&path) {
                Err(DecodeError::UnsupportedFormat(msg)) => {
                    assert!(
                        msg.contains("APE") || msg.contains("ape"),
                        "error message should mention APE: {msg}"
                    );
                }
                other => panic!(
                    "expected UnsupportedFormat from the APE backend for .{ext}, got: {:?}",
                    other.err()
                ),
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Without `codec-ape`, `.ape` / `.mac` must fail with the explicit
    /// feature-gap error rather than falling through to Symphonia.
    #[cfg(not(feature = "codec-ape"))]
    #[test]
    fn test_decoder_dispatch_ape_returns_unsupported() {
        for ext in ["ape", "mac"] {
            let path = temp_path(ext);
            std::fs::write(&path, b"not real").unwrap();
            match Decoder::open(&path) {
                Err(DecodeError::UnsupportedFormat(msg)) => {
                    assert!(
                        msg.contains("APE") || msg.contains("Monkey"),
                        "error message should mention APE: {msg}"
                    );
                }
                other => panic!(
                    "expected UnsupportedFormat for .{ext}, got: {:?}",
                    other.err()
                ),
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    #[test]

    /// Declared-but-unavailable codecs must fail with an explicit,
    /// codec-named error instead of Symphonia's generic "Probe failed"
    /// (which would silently mislead the user into thinking the file is
    /// merely corrupt). `.opus` depends on `codec-opus`, `.wv` on
    /// `codec-wavpack`.
    #[cfg(all(not(feature = "codec-opus"), not(feature = "codec-wavpack")))]
    #[test]
    fn test_decoder_dispatch_opus_and_wavpack_return_unsupported() {
        for ext in ["opus", "wv"] {
            let path = temp_path(ext);
            std::fs::write(&path, b"not a real audio file").unwrap();
            match Decoder::open(&path) {
                Err(DecodeError::UnsupportedFormat(msg)) => {
                    let expected = if ext == "opus" { "Opus" } else { "WavPack" };
                    assert!(
                        msg.contains(expected),
                        "error message should mention {expected}: {msg}"
                    );
                }
                other => panic!(
                    "expected UnsupportedFormat for .{ext}, got: {:?}",
                    other.err()
                ),
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    /// With `codec-wavpack`, a real `.wv` file decodes end-to-end through the
    /// unified `Decoder` interface: stereo, correct rate, lossless frame
    /// count, and the WavPack dispatch arm.
    #[cfg(feature = "codec-wavpack")]
    #[test]
    fn test_decoder_dispatch_wavpack_full_decode() {
        use wavicle::EncodeParams;
        let mut state = 0xDEAD_BEEFu32;
        let frames = 5000usize;
        let mut src = Vec::with_capacity(frames * 2);
        for _ in 0..frames * 2 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            src.push(((state % 65536) as i32) - 32768);
        }
        let bytes = wavicle::encode_int(
            EncodeParams {
                channels: 2,
                sample_rate: 44_100,
                bits_per_sample: 16,
            },
            &src,
        )
        .expect("encode");
        let path = temp_path("wv");
        std::fs::write(&path, &bytes).unwrap();
        let mut dec = Decoder::open(&path).expect("open wavpack");
        assert!(
            matches!(dec, Decoder::Wavpack(_)),
            "dispatch must hit the Wavpack arm"
        );
        let mut total = 0usize;
        loop {
            match dec.decode_next(4096) {
                Ok(c) => {
                    assert_eq!(c.channels, 2);
                    assert_eq!(c.sample_rate, 44_100);
                    assert_eq!(c.samples.len(), c.frame_count * 2);
                    total += c.frame_count;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(total, frames, "exact frame count");
        assert!(!dec.is_dsd());
        let _ = std::fs::remove_file(&path);
    }

    /// With `codec-opus`, a real Ogg Opus file decodes end-to-end through
    /// the unified `Decoder` interface: correct channel count, 48 kHz decode
    /// rate, and the exact logical frame count after OpusHead pre-skip trim.
    #[cfg(feature = "codec-opus")]
    #[test]
    fn test_decoder_dispatch_opus_full_decode() {
        use crate::decode::opus::test_support::write_test_opus;
        let path = write_test_opus(2, 48_000, 312, &[]);
        let mut dec = Decoder::open(&path).expect("open opus");
        assert!(
            matches!(dec, Decoder::Opus(_)),
            "dispatch must hit the Opus arm"
        );
        let mut total = 0usize;
        loop {
            match dec.decode_next(4096) {
                Ok(c) => {
                    assert_eq!(c.channels, 2);
                    assert_eq!(c.sample_rate, 48_000);
                    assert_eq!(c.samples.len(), c.frame_count * 2);
                    total += c.frame_count;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(total, 48_000, "exact logical frames after gapless trim");
        // Metadata routing: the shared extractor must find OpusTags.
        let (title, artist, album, dur, _) = crate::decode::extract_track_metadata(&path);
        assert_eq!(title, path.file_stem().unwrap().to_str().unwrap());
        assert_eq!(artist, "Unknown Artist");
        assert_eq!(album, "Unknown Album");
        assert!((dur - 1.0).abs() < 0.05, "duration {dur}");
        let _ = std::fs::remove_file(&path);
    }

    /// With `codec-opus`, `.opus` must route to the Opus backend. A non-Ogg
    /// `.opus` file must produce an Ogg/Opus-named error, never Symphonia's
    /// generic "Probe failed".
    #[cfg(feature = "codec-opus")]
    #[test]
    fn test_decoder_dispatch_opus_uses_opus_backend() {
        let path = temp_path("opus");
        std::fs::write(&path, b"not a real audio file").unwrap();
        match Decoder::open(&path) {
            Err(e) => {
                let msg = format!("{e:?}");
                assert!(
                    !msg.contains("Probe failed"),
                    "non-Ogg .opus must not reach Symphonia's probe: {msg}"
                );
                assert!(
                    msg.contains("Opus") || msg.contains("Ogg"),
                    "error should mention Ogg/Opus: {msg}"
                );
            }
            Ok(_) => panic!("non-Ogg file must not open"),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Without `codec-wavpack`, `.wv` must still fail with the declared-
    /// unavailable WavPack error (never Symphonia's generic probe failure).
    #[cfg(all(feature = "codec-opus", not(feature = "codec-wavpack")))]
    #[test]
    fn test_decoder_dispatch_wavpack_unsupported_without_feature() {
        let path = temp_path("wv");
        std::fs::write(&path, b"not a real audio file").unwrap();
        match Decoder::open(&path) {
            Err(DecodeError::UnsupportedFormat(msg)) => {
                assert!(
                    msg.contains("WavPack"),
                    "error message should mention WavPack: {msg}"
                );
            }
            other => panic!("expected UnsupportedFormat for .wv, got: {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }
}
