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

use std::path::Path;

#[cfg(feature = "codec-ape")]
use crate::decode::ApeDecoder;
#[cfg(feature = "codec-opus")]
use crate::decode::OpusSource;
#[cfg(feature = "codec-tta")]
use crate::decode::TtaDecoder;
#[cfg(feature = "codec-wavpack")]
use crate::decode::WavpackDecoder;
use crate::decode::{
    AudioFormatInfo, ChannelLayout, DecodeError, DecodeInfo, DecodedChunk, DopPacker, DsdError,
    DsdReader, RawDsdChunk, SymphoniaDecoder,
};

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
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("dsf") || ext.eq_ignore_ascii_case("dff") {
                return DsdDecoder::open(path).map(Self::Dsd);
            }
            if ext.eq_ignore_ascii_case("ape") || ext.eq_ignore_ascii_case("mac") {
                #[cfg(feature = "codec-ape")]
                {
                    return ApeDecoder::open(path).map(Self::Ape);
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
                    return OpusSource::open(path).map(Self::Opus);
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
                // `.oga` may hold Opus or Vorbis: content-sniff, then fall
                // through to Symphonia (Vorbis) when the first packet is not
                // an OpusHead.
                #[cfg(feature = "codec-opus")]
                {
                    if OpusSource::probe(path) {
                        return OpusSource::open(path).map(Self::Opus);
                    }
                }
                // Without `codec-opus`, or for Vorbis content, Symphonia
                // probes the container and decodes Vorbis normally.
            }
            if ext.eq_ignore_ascii_case("wv") {
                #[cfg(feature = "codec-wavpack")]
                {
                    return WavpackDecoder::open(path).map(Self::Wavpack);
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
                    return crate::decode::TtaDecoder::open(path).map(Self::Tta);
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
        }
        SymphoniaDecoder::open(path).map(Self::Symphonia)
    }

    /// Open in-memory byte buffer for decoding.
    pub fn open_memory(data: Vec<u8>, extension_hint: Option<&str>) -> Result<Self, DecodeError> {
        SymphoniaDecoder::open_memory(data, extension_hint).map(Self::Symphonia)
    }

    /// Open an [`AudioSource`] for decoding.
    pub fn open_source(source: &crate::source::AudioSource) -> Result<Self, DecodeError> {
        match source {
            crate::source::AudioSource::File(path) => Self::open(path),
            crate::source::AudioSource::Uri(uri) => {
                if let Some(stripped) = uri.strip_prefix("file://") {
                    let decoded = crate::engine::helpers::percent_decode(stripped)
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| std::path::PathBuf::from(stripped));
                    Self::open(&decoded)
                } else {
                    Self::open(std::path::Path::new(uri))
                }
            }
            crate::source::AudioSource::Memory { data, extension_hint } => {
                Self::open_memory(data.clone(), extension_hint.as_deref())
            }
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
pub struct DsdDecoder {
    reader: DsdReader,
    info: DecodeInfo,
    format_info: AudioFormatInfo,
    /// PCM output rate (DSD bit rate / 32).
    pcm_rate: u32,
    /// DoP output rate (DSD bit rate / 16).
    dop_pcm_rate: u32,
    /// True when decoding raw DSD as DoP frames instead of decimated PCM.
    dop: bool,
    /// True when decoding raw DSD bitstream for native-DSD transport.
    /// Mutually exclusive with `dop`: native mode ships raw 1-bit payloads
    /// (via the `raw_dsd` chunk sidecar); DoP packs them into 24-bit frames.
    native: bool,
    /// DoP frame packer; its marker toggle carries across `decode_next` calls
    /// so the 0x05/0xFA pattern stays continuous through the whole track.
    dop_packer: DopPacker,
    /// Reusable interleaved output buffer (reallocated only on growth).
    interleaved: Vec<f32>,
}

impl DsdDecoder {
    /// Open a DSF or DFF file for playback (PCM/decimation mode by default).
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let reader = DsdReader::open(path).map_err(DecodeError::from)?;
        let bit_rate = reader.rate().sample_rate_hz();
        let pcm_rate = bit_rate / 32;
        let dop_pcm_rate = bit_rate / 16;
        let format_info = reader.format_info();
        let duration_secs = format_info.duration_secs.unwrap_or(0.0) as f32;
        let channels = reader.channels();
        let info = DecodeInfo {
            sample_rate: pcm_rate,
            channels,
            duration_secs,
            codec: format!("{:?}", reader.rate()),
            bitrate_kbps: None,
        };
        Ok(Self {
            reader,
            info,
            format_info,
            pcm_rate,
            dop_pcm_rate,
            dop: false,
            native: false,
            dop_packer: DopPacker::new(),
            interleaved: Vec::with_capacity(4096 * channels),
        })
    }

    /// Toggle DoP (DSD-over-PCM) mode.
    ///
    /// In DoP mode the decoder delivers raw DSD packed into 24-bit PCM frames
    /// at `bit_rate / 16` instead of decimated PCM. DoP is stereo-only; for
    /// sources with more than two channels the mode is refused (the decoder
    /// stays in decimation mode and `dop_rate()` returns `None`).
    pub fn set_dop_mode(&mut self, dop: bool) {
        self.dop = dop && self.reader.channels() <= 2;
        if self.dop {
            // DoP and native-DSD transport are mutually exclusive.
            self.native = false;
        }
        self.info.sample_rate = if self.dop {
            self.dop_pcm_rate
        } else {
            self.pcm_rate
        };
        self.dop_packer.reset();
    }

    /// Toggle native-DSD transport mode.
    ///
    /// In native mode the decoder delivers raw (still 1-bit) DSD payload via
    /// the `raw_dsd` sidecar on [`DecodedChunk`] instead of decimated PCM or
    /// DoP frames; `info.sample_rate` reports the DSD bit rate (DSD frames
    /// per second) so the playback clock stays sample-accurate. Native mode
    /// is stereo/multichannel-capable (DoP remains stereo-only).
    pub fn set_native_dsd_mode(&mut self, native: bool) {
        self.native = native && self.reader.channels() <= 8;
        if self.native {
            // DoP and native-DSD transport are mutually exclusive.
            self.dop = false;
        }
        self.info.sample_rate = if self.native {
            self.reader.rate().sample_rate_hz()
        } else if self.dop {
            self.dop_pcm_rate
        } else {
            self.pcm_rate
        };
        self.dop_packer.reset();
    }

    /// The DSD bit rate (e.g. 2_822_400 for DSD64), regardless of output mode.
    pub fn dsd_bit_rate(&self) -> u32 {
        self.reader.rate().sample_rate_hz()
    }

    /// True when the decoder is in native-DSD transport mode.
    pub fn is_native_dsd(&self) -> bool {
        self.native
    }

    /// The output rate in DoP mode (bit_rate / 16), or `None` when the decoder
    /// is not currently in DoP mode.
    pub fn dop_rate(&self) -> Option<u32> {
        if self.dop {
            Some(self.dop_pcm_rate)
        } else {
            None
        }
    }

    /// Decode the next chunk.
    ///
    /// Requests enough DSD frames to produce up to `max_frames` output frames.
    /// Each call returns at most one DSD block (the reader clamps to block
    /// boundaries), so chunks are typically 512–2048 frames for common files —
    /// the engine processes chunks of any size.
    pub fn decode_next(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        if self.native {
            self.decode_next_native(max_frames)
        } else if self.dop {
            self.decode_next_dop(max_frames)
        } else {
            self.decode_next_pcm(max_frames)
        }
    }

    /// Native-DSD path: read raw (still 1-bit) payload, normalize the bit
    /// order to LSB-first, and return it via the `raw_dsd` sidecar. The
    /// engine routes these bytes to the negotiated DSD transport; they never
    /// enter the f32 PCM pipeline.
    fn decode_next_native(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        let dsd_budget = (max_frames as u64).min(u32::MAX as u64) as u32;
        let Some(block) = self
            .reader
            .read_dsd_block(dsd_budget)
            .map_err(DecodeError::from)?
        else {
            return Err(DecodeError::EndOfStream);
        };
        let lsbf = self.reader.is_lsb_first();
        let mut channel_bytes: Vec<Vec<u8>> = Vec::with_capacity(block.channels.len());
        for ch in &block.channels {
            if lsbf {
                channel_bytes.push(ch.clone());
            } else {
                // MSB-first sources (DSF `bits_per_sample == 8`) are reversed
                // per byte so the payload is always LSB-first downstream —
                // the same normalization the DoP path applies.
                channel_bytes.push(ch.iter().map(|b| b.reverse_bits()).collect());
            }
        }
        let frames = block.frames;
        Ok(DecodedChunk {
            samples: Vec::new(),
            channels: channel_bytes.len(),
            channel_layout: ChannelLayout::from_count(channel_bytes.len()),
            sample_rate: self.reader.rate().sample_rate_hz(),
            // frame_count carries the DSD frame budget so the engine's chunk
            // resume machinery (pending_chunk + start frame) works with raw
            // payload even though `samples` is empty by design.
            frame_count: frames as usize,
            raw_dsd: Some(RawDsdChunk {
                frames,
                channels: channel_bytes.len(),
                channel_bytes,
            }),
        })
    }

    /// Decimated-PCM path: 32 DSD frames (4 bytes) per output frame.
    fn decode_next_pcm(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        let dsd_budget = (max_frames as u64).saturating_mul(32).min(u32::MAX as u64) as u32;
        loop {
            let Some(block) = self
                .reader
                .decode_block(dsd_budget)
                .map_err(DecodeError::from)?
            else {
                return Err(DecodeError::EndOfStream);
            };
            let nch = block.channels.len();
            // Every channel decimates to the same PCM length; a trailing block
            // shorter than the FIR latency yields zero samples across the board.
            let n = block.channels.iter().map(Vec::len).min().unwrap_or(0);
            if n == 0 {
                // Trailing block shorter than the FIR latency produces no
                // PCM yet — keep reading so the caller never sees an empty
                // (wasteful) chunk.
                continue;
            }
            self.interleaved.clear();
            self.interleaved.reserve(n * nch);
            for i in 0..n {
                for ch in 0..nch {
                    self.interleaved.push(block.channels[ch][i]);
                }
            }
            let cap = self.interleaved.capacity();
            let samples = std::mem::replace(&mut self.interleaved, Vec::with_capacity(cap));
            return Ok(DecodedChunk {
                samples,
                channels: nch,
                channel_layout: ChannelLayout::from_count(nch),
                sample_rate: self.pcm_rate,
                frame_count: n,
                raw_dsd: None,
            });
        }
    }

    /// DoP path: pack raw DSD bytes into 24-bit DoP frames (2 bytes = 16 DSD
    /// samples per channel per frame).
    fn decode_next_dop(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        // One DoP frame carries 16 DSD samples = 2 payload bytes per channel.
        let dsd_budget = (max_frames as u64).saturating_mul(2).min(u32::MAX as u64) as u32;
        loop {
            let Some(block) = self
                .reader
                .read_dsd_block(dsd_budget)
                .map_err(DecodeError::from)?
            else {
                return Err(DecodeError::EndOfStream);
            };
            let left = block.left();
            let right = block.right().unwrap_or(left);
            let words = left.len().min(right.len()) / 2;
            if words == 0 {
                continue;
            }
            let lsbf = self.reader.is_lsb_first();
            self.interleaved.clear();
            self.interleaved.reserve(words * 2);
            for i in 0..words {
                let (b0l, b1l) = (left[i * 2], left[i * 2 + 1]);
                let (b0r, b1r) = (right[i * 2], right[i * 2 + 1]);
                // Consecutive DSD samples: byte 0 = samples 0-7 (bits 0-7), byte 1
                // = samples 8-15 (bits 8-15). For the rare MSB-first (DSF
                // bits_per_sample == 8) payloads, reverse each byte first so the
                // DoP stream is always in the standard LSB-first sample order.
                let (dl, dr) = if lsbf {
                    (
                        b0l as u16 | ((b1l as u16) << 8),
                        b0r as u16 | ((b1r as u16) << 8),
                    )
                } else {
                    let r0l = b0l.reverse_bits();
                    let r1l = b1l.reverse_bits();
                    let r0r = b0r.reverse_bits();
                    let r1r = b1r.reverse_bits();
                    (
                        r0l as u16 | ((r1l as u16) << 8),
                        r0r as u16 | ((r1r as u16) << 8),
                    )
                };
                let (l, r) = self.dop_packer.pack_stereo_frame_f32(dl, dr);
                self.interleaved.push(l);
                self.interleaved.push(r);
            }
            let cap = self.interleaved.capacity();
            let samples = std::mem::replace(&mut self.interleaved, Vec::with_capacity(cap));
            return Ok(DecodedChunk {
                samples,
                channels: 2,
                channel_layout: ChannelLayout::Stereo,
                sample_rate: self.dop_pcm_rate,
                frame_count: words,
                raw_dsd: None,
            });
        }
    }

    /// Seek to a position in seconds (in the DSD bitstream domain).
    pub fn seek(&mut self, position_secs: f32) -> Result<(), DecodeError> {
        if !position_secs.is_finite() || position_secs < 0.0 {
            return Err(DecodeError::Seek(format!(
                "Invalid seek position: {}",
                position_secs
            )));
        }
        let bit_rate = self.reader.rate().sample_rate_hz();
        let frame = (position_secs as f64 * bit_rate as f64).round() as u64;
        self.reader.seek_to_dsd_frame(frame);
        // Restart the marker alternation at the new position (the DAC re-locks
        // on the 0x05/0xFA pair).
        self.dop_packer.reset();
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
                let base = (b as usize * block_size as usize) * 2 + ch * block_size as usize;
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
    fn test_dsd_decoder_info() {
        let path = temp_path("dsf");
        std::fs::write(&path, build_dsf(4096, 4096 * 4)).unwrap();
        let dec = DsdDecoder::open(&path).expect("open DSF");
        assert_eq!(dec.info().sample_rate, 88_200);
        assert_eq!(dec.info().channels, 2);
        assert_eq!(dec.info().codec, "Dsd64");
        let expected = (4096.0 * 4.0 * 8.0) / 2_822_400.0;
        assert!((dec.duration_secs() - expected as f32).abs() < 1e-6);
        assert!(dec.format_info().is_dsd);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_dsd_decoder_decode_all_frames() {
        let path = temp_path("dsf");
        let frames = 4096u64 * 4; // 4 blocks
        std::fs::write(&path, build_dsf(4096, frames)).unwrap();
        let mut dec = DsdDecoder::open(&path).expect("open DSF");

        let mut total_pcm = 0usize;
        loop {
            match dec.decode_next(4096) {
                Ok(chunk) => {
                    assert_eq!(chunk.channels, 2);
                    assert_eq!(chunk.sample_rate, 88_200);
                    assert!(chunk.frame_count > 0);
                    assert_eq!(chunk.samples.len(), chunk.frame_count * 2);
                    for &s in &chunk.samples {
                        assert!((-1.0..=1.0).contains(&s), "PCM out of range: {s}");
                    }
                    total_pcm += chunk.frame_count;
                    if total_pcm > 10_000_000 {
                        panic!("decoder never reached end of stream");
                    }
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        // 4 blocks × 4096 DSD frames × 8 bits / 32 = 4096 PCM frames per
        // block; the trailing 63-sample FIR latency is absorbed at the end.
        assert_eq!(total_pcm, 4096);
        let _ = std::fs::remove_file(&path);
    }

    /// Build a `channels`-channel DSF (LSB-first) with distinct per-channel
    /// payloads, padded to whole blocks.
    fn build_dsf_nch(channels: usize, block_size: u32, frames: u64) -> Vec<u8> {
        let data: Vec<Vec<u8>> = (0..channels)
            .map(|ch| {
                (0..frames as u8)
                    .map(|i| i.wrapping_mul(ch as u8 + 1))
                    .collect()
            })
            .collect();
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
        out.extend_from_slice(&(channels as u32).to_le_bytes());
        out.extend_from_slice(&(channels as u32).to_le_bytes());
        out.extend_from_slice(&2_822_400u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(padded * 8).to_le_bytes());
        out.extend_from_slice(&block_size.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());

        let block = block_size as usize;
        let mut audio = vec![0u8; (padded as usize) * channels];
        for (ch, chan_data) in data.iter().enumerate() {
            for (b, chunk) in chan_data.chunks(block).enumerate() {
                let base = b * block * channels + ch * block;
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
    fn test_dsd_decoder_multichannel_pcm() {
        let path = temp_path("dsf");
        let channels = 6usize; // 5.1: FL FR C LFE SL SR
        std::fs::write(&path, build_dsf_nch(channels, 4096, 4096 * 4)).unwrap();
        let mut dec = DsdDecoder::open(&path).expect("open 5.1 DSF");
        assert_eq!(dec.info().channels, channels);
        assert_eq!(
            dec.format_info().channel_layout,
            ChannelLayout::FivePointOne
        );

        // DoP is stereo-only and must be refused for >2 channels.
        dec.set_dop_mode(true);
        assert_eq!(dec.dop_rate(), None);
        assert_eq!(dec.info().sample_rate, 88_200);

        let mut total = 0usize;
        loop {
            match dec.decode_next(4096) {
                Ok(chunk) => {
                    assert_eq!(chunk.channels, channels);
                    assert_eq!(chunk.channel_layout, ChannelLayout::FivePointOne);
                    assert_eq!(chunk.samples.len(), chunk.frame_count * channels);
                    total += chunk.frame_count;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(total, 4096);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_dsd_decoder_seek() {
        let path = temp_path("dsf");
        let frames = 4096u64 * 8;
        std::fs::write(&path, build_dsf(4096, frames)).unwrap();
        let mut dec = DsdDecoder::open(&path).expect("open DSF");

        let target_dsd_frames = (0.005f64 * 2_822_400.0).round() as u64;
        dec.seek(target_dsd_frames as f32 / 2_822_400.0)
            .expect("seek");
        let pos = dec.reader.position_dsd_frames();
        assert!(
            (pos as i64 - target_dsd_frames as i64).abs() <= 1,
            "reader at {pos}, expected ~{target_dsd_frames}"
        );

        let mut total_pcm = 0usize;
        loop {
            match dec.decode_next(4096) {
                Ok(chunk) => total_pcm += chunk.frame_count,
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error after seek: {e}"),
            }
        }
        assert!(total_pcm > 0);
        let _ = std::fs::remove_file(&path);
    }

    /// Like [`build_dsf`] but every payload byte on a channel is constant.
    fn build_dsf_fill(block_size: u32, frames: u64, ch0_byte: u8, ch1_byte: u8) -> Vec<u8> {
        let ch0 = vec![ch0_byte; frames as usize];
        let ch1 = vec![ch1_byte; frames as usize];
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

        let mut audio = vec![0u8; (padded * 2) as usize];
        for (ch, data) in [&ch0[..], &ch1[..]].iter().enumerate() {
            for (b, chunk) in data.chunks(block_size as usize).enumerate() {
                let base = (b as usize * block_size as usize) * 2 + ch * block_size as usize;
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

    fn word24(s: f32) -> u32 {
        ((s as f64 * 2_147_483_648.0) as i32 as u32) >> 8
    }

    #[test]
    fn test_dop_mode_info_and_rate() {
        let path = temp_path("dsf");
        std::fs::write(&path, build_dsf(4096, 4096 * 4)).unwrap();
        let mut dec = DsdDecoder::open(&path).expect("open DSF");
        assert_eq!(dec.dop_rate(), None);
        assert_eq!(dec.info().sample_rate, 88_200);

        dec.set_dop_mode(true);
        assert_eq!(dec.dop_rate(), Some(176_400));
        assert_eq!(dec.info().sample_rate, 176_400);
        assert_eq!(dec.duration_secs(), dec.info().duration_secs);

        dec.set_dop_mode(false);
        assert_eq!(dec.dop_rate(), None);
        assert_eq!(dec.info().sample_rate, 88_200);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_dop_decode_markers_and_payload() {
        let path = temp_path("dsf");
        let frames = 4096u64 * 4;
        std::fs::write(&path, build_dsf_fill(4096, frames, 0x00, 0x00)).unwrap();
        let mut dec = DsdDecoder::open(&path).expect("open DSF");
        dec.set_dop_mode(true);

        let mut total = 0usize;
        let mut expect_marker = 0x05u32;
        loop {
            match dec.decode_next(4096) {
                Ok(chunk) => {
                    assert_eq!(chunk.channels, 2);
                    assert_eq!(chunk.sample_rate, 176_400);
                    assert_eq!(chunk.samples.len(), chunk.frame_count * 2);
                    for pair in chunk.samples.chunks_exact(2) {
                        let wl = word24(pair[0]);
                        let wr = word24(pair[1]);
                        assert_eq!(wl & 0xFFFF, 0);
                        assert_eq!(wr & 0xFFFF, 0);
                        assert_eq!((wl >> 16) & 0xFF, expect_marker);
                        assert_eq!((wr >> 16) & 0xFF, expect_marker);
                        expect_marker = if expect_marker == 0x05 { 0xFA } else { 0x05 };
                    }
                    total += chunk.frame_count;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(total, 8192);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_dop_decode_substitution() {
        let path = temp_path("dsf");
        let frames = 64u64;
        std::fs::write(&path, build_dsf_fill(64, frames, 0x05, 0xFA)).unwrap();
        let mut dec = DsdDecoder::open(&path).expect("open DSF");
        dec.set_dop_mode(true);

        let chunk = dec.decode_next(1024).expect("decode");
        let wl = word24(chunk.samples[0]);
        let wr = word24(chunk.samples[1]);
        assert_eq!(wl & 0xFFFF, 0x0505);
        assert_eq!(wr & 0xFFFF, 0xFAFA);
        assert_eq!((wl >> 16) & 0xFF, 0x06);
        assert_eq!((wr >> 16) & 0xFF, 0xFB);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_dop_seek_resets_marker() {
        let path = temp_path("dsf");
        std::fs::write(&path, build_dsf_fill(4096, 4096 * 8, 0x00, 0x00)).unwrap();
        let mut dec = DsdDecoder::open(&path).expect("open DSF");
        dec.set_dop_mode(true);

        let c = dec.decode_next(2).expect("decode");
        assert_eq!(c.frame_count, 2);

        dec.seek(0.0).expect("seek");
        let c = dec.decode_next(1).expect("decode");
        assert_eq!((word24(c.samples[0]) >> 16) & 0xFF, 0x05);
        let _ = std::fs::remove_file(&path);
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
    fn test_native_dsd_mode_delivers_raw_payload() {
        let path = temp_path("dsf");
        // 2 blocks of 4096 frames, payload bytes cycling 0..255 per channel.
        std::fs::write(&path, build_dsf(4096, 4096 * 2)).unwrap();
        let mut dec = DsdDecoder::open(&path).expect("open DSF");

        dec.set_native_dsd_mode(true);
        assert!(dec.is_native_dsd());
        // Native mode reports the DSD bit rate so the playback clock is
        // sample-accurate in DSD frames.
        assert_eq!(dec.info().sample_rate, 2_822_400);
        assert_eq!(dec.dsd_bit_rate(), 2_822_400);
        // DoP and native modes are mutually exclusive.
        dec.set_dop_mode(true);
        assert!(!dec.is_native_dsd(), "DoP must clear native mode");
        assert_eq!(dec.info().sample_rate, 176_400);
        dec.set_native_dsd_mode(true);
        assert!(!dec.dop_rate().is_some(), "native mode must clear DoP");

        // First chunk: a full 4096-frame block per channel, bytes as-is
        // (this DSF is LSB-first).
        let chunk = dec.decode_next(4096).expect("native decode");
        let raw = chunk.raw_dsd.expect("raw_dsd sidecar");
        assert_eq!(raw.frames, 4096);
        assert_eq!(raw.channels, 2);
        assert_eq!(raw.channel_bytes[0].len(), 4096);
        assert_eq!(raw.channel_bytes[1].len(), 4096);
        assert_eq!(raw.channel_bytes[0][0], 0);
        assert_eq!(raw.channel_bytes[0][1], 1);
        assert_eq!(raw.channel_bytes[1][0], 3); // ch1 = (i*7+3) % 256
                                                // The PCM sample buffer must be empty in native mode: raw DSD never
                                                // enters the f32 domain.
        assert!(chunk.samples.is_empty());
        assert_eq!(chunk.sample_rate, 2_822_400);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_native_dsd_mode_full_track_and_eos() {
        let path = temp_path("dsf");
        std::fs::write(&path, build_dsf(4096, 4096 * 4)).unwrap();
        let mut dec = DsdDecoder::open(&path).expect("open DSF");
        dec.set_native_dsd_mode(true);

        let mut frames = 0u64;
        loop {
            match dec.decode_next(8192) {
                Ok(chunk) => {
                    let raw = chunk.raw_dsd.expect("sidecar");
                    frames += raw.frames as u64;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        assert_eq!(frames, 4096 * 4);
        let _ = std::fs::remove_file(&path);
    }

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
