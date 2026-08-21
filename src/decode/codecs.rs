//! Codec capability registry — one source of truth for what this engine can
//! actually decode, seek, and measure.
//!
//! The UI and engine diagnostics query the same table, so a codec is never
//! advertised by one layer while being silently unsupported by another.
//! Statuses are deliberately conservative and **honest**:
//!
//! ## Current build status
//!
//! | Codec | Status | Notes |
//! |-------|--------|-------|
//! | MP3   | Available | Full decode via Symphonia |
//! | FLAC  | Available | Full decode via Symphonia |
//! | Ogg/Vorbis | Available | Full decode via Symphonia |
//! | WAV   | Available | Full decode via Symphonia |
//! | AAC   | Available | Full decode via Symphonia |
//! | ALAC  | Available | Symphonia 0.6.0 + `symphonia-codec-alac` provides full ALAC decode through ISO/MP4 |
//! | APE   | Available | Full decode via the pure-Rust `ape-decoder` crate (range coder + adaptive predictor) when the `codec-ape` feature is enabled |
//! | DSD   | Available | DSF/DFF reader, DSD→PCM decimation, DoP, native DSD wire transport on ALSA `hw:` (DSD_U8/U16/U32), DoP over WASAPI exclusive |
//! | PCM   | Available | Raw PCM via Symphonia |
//! | AIFF  | Available | AIFF PCM via Symphonia format reader |
//! | Opus  | Available (`codec-opus`) | Ogg demux + pure-Rust RFC 8251 decoder (`opus-decoder`, no FFI): 48 kHz decode, OpusHead pre-skip gapless, granule seeking, multichannel mapping families 0/1, OpusTags metadata + ReplayGain/R128 |
//! | WavPack | Available (`codec-wavpack`) | Pure-Rust `wavicle` v5 codec (MIT/Apache-2.0, no FFI): lossless 16/24/32-bit int + f32, mono/stereo, bit-exact vs the reference. Multichannel / DSD / hybrid `.wv` are rejected explicitly; tag metadata is not yet wired (`metadata = false`) |
//! | Musepack | DeclaredUnavailable | No Musepack decoder is wired in this build (the planned path is FFI to libmpcdec); `.mpc` files are rejected with an explicit error |
//! | TAK    | DeclaredUnavailable | **Blocked:** TAK has no open-source decoder (FFmpeg's support is experimental); `.tak` files are rejected with an explicit error |
//! | TTA    | Available (`codec-tta`) | Native pure-Rust TTA1 decoder (`src/decode/tta.rs`): 8/16/24-bit, mono–16 channels, frame-indexed seeking, CRC-verified frames |
//!
//! - **Available** — fully decodable in this build.
//! - **MetadataOnly** — tags are parsed but the audio stream cannot be decoded.
//! - **DeclaredUnavailable** — the feature is declared for future parity but no
//!   working decoder is wired up; opening such a file returns an explicit error.

/// Codecs known to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    Mp3,
    Flac,
    OggVorbis,
    Opus,
    Wav,
    Aac,
    Alac,
    Ape,
    Dsd,
    Pcm,
    Aiff,
    /// Matroska audio container (MKA/MKV). The inner codec is decoded by the
    /// codec features above; the registry row describes the container.
    Mka,
    WavPack,
    Musepack,
    /// TAK (Tom's lossless Audio Kompressor) — declared, no decoder wired in.
    Tak,
    /// TTA (True Audio) lossless — declared, no decoder wired in.
    Tta,
    /// Anything the registry does not recognise (or the null audio codec).
    Unknown,
}

/// Build-time availability of a codec in this engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecStatus {
    /// Fully decodable in this build.
    Available,
    /// Metadata/tags are parsed; the audio stream is not decodable.
    MetadataOnly,
    /// The feature is declared for future parity but no working decoder is wired up.
    /// Opening a file of this type returns an explicit `UnsupportedFormat` error.
    DeclaredUnavailable,
}

/// Product-facing summary of codec support. Detailed behavior remains
/// available through the individual capability axes on [`CodecCapability`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecSupportLevel {
    FullySupported,
    DecodeOnly,
    MetadataOnly,
    Experimental,
    Unavailable,
}

/// One row of the capability registry.
#[derive(Debug, Clone, Copy)]
pub struct CodecCapability {
    pub codec: Codec,
    pub status: CodecStatus,
    /// Coarse product-facing support classification derived from the axes below.
    pub support_level: CodecSupportLevel,
    /// Audio frames can be decoded in this build.
    pub decode: bool,
    /// Sample-accurate seeking is supported.
    pub seek: bool,
    /// Encoder delay / end padding is extracted and applied (gapless).
    pub gapless: bool,
    /// Backward-compatible alias for `multichannel_decode`.
    pub multichannel: bool,
    /// The decoder can expose more than two source channels. This is a source
    /// capability only; downstream output layout is negotiated independently
    /// by `OutputCapabilities`.
    pub multichannel_decode: bool,
    /// Metadata (tags / cover art) is extracted.
    pub metadata: bool,
    /// ReplayGain tags are read.
    pub replaygain: bool,
    /// Cover art is extracted and cached (ID3 APIC, FLAC `PICTURE`, Ogg
    /// `METADATA_BLOCK_PICTURE`, MP4 `covr`, Matroska attachments).
    pub cover_art: bool,
    /// Container-native chapter / embedded cue-sheet metadata is read.
    /// (Standalone `.cue` sheets are handled by the separate `decode::cue`
    /// module for any format; this axis is about container chapters.)
    pub chapters: bool,
    /// EBU R128 tags (`r128_track_gain` / `r128_album_gain`) are read.
    pub ebu_r128: bool,
    /// Progressive / streaming decode is possible from a packet stream
    /// (vs. requiring a seekable file for a random-access structure).
    pub streaming: bool,
    /// Native source precision in bits (e.g. 16/24/32/64); `None` for
    /// 1-bit DSD or unknown.
    pub native_precision_bits: Option<u32>,
    pub is_lossless: bool,
}

/// Look up the full capability record for a codec.
pub fn capability(codec: Codec) -> CodecCapability {
    let (
        status,
        decode,
        seek,
        gapless,
        multichannel,
        metadata,
        replaygain,
        cover_art,
        chapters,
        ebu_r128,
        streaming,
        bits,
        lossless,
    ) = match codec {
        Codec::Mp3 => (
            CodecStatus::Available,
            true,
            true,
            true,
            true,
            true,
            true,
            true,  // cover_art (ID3 APIC)
            false, // chapters
            true,  // ebu_r128 (R128 tags common in ID3v2)
            true,  // streaming (frame-based decode)
            Some(16),
            false,
        ),
        Codec::Flac => (
            CodecStatus::Available,
            true,
            true,
            true,
            true,
            true,
            true,
            true,  // cover_art (PICTURE block)
            false, // chapters
            true,  // ebu_r128 (Vorbis comments)
            true,  // streaming
            Some(24),
            true,
        ),
        Codec::OggVorbis => (
            CodecStatus::Available,
            true,
            true,
            true,
            true,
            true,
            true,
            true,  // cover_art (METADATA_BLOCK_PICTURE)
            false, // chapters
            true,  // ebu_r128 (Vorbis comments)
            true,  // streaming
            Some(16),
            false,
        ),
        Codec::Opus => (
            // With `codec-opus`, Ogg Opus decodes through the pure-Rust
            // `opus-decoder` crate (RFC 8251): 48 kHz, granule seeking,
            // OpusHead pre-skip gapless, multichannel families 0/1, and
            // OpusTags metadata/ReplayGain. Without the feature the row
            // is declared-but-unavailable and `.opus` files are rejected
            // with an explicit error instead of a generic "probe failed".
            #[cfg(feature = "codec-opus")]
            CodecStatus::Available,
            #[cfg(not(feature = "codec-opus"))]
            CodecStatus::DeclaredUnavailable,
            // decode
            #[cfg(feature = "codec-opus")]
            true,
            #[cfg(not(feature = "codec-opus"))]
            false,
            // seek (granule-based)
            #[cfg(feature = "codec-opus")]
            true,
            #[cfg(not(feature = "codec-opus"))]
            false,
            // gapless (OpusHead pre-skip + final-granule end trim)
            #[cfg(feature = "codec-opus")]
            true,
            #[cfg(not(feature = "codec-opus"))]
            false,
            // multichannel (mapping families 0 and 1)
            #[cfg(feature = "codec-opus")]
            true,
            #[cfg(not(feature = "codec-opus"))]
            false,
            // metadata (OpusTags / Vorbis comments)
            #[cfg(feature = "codec-opus")]
            true,
            #[cfg(not(feature = "codec-opus"))]
            false,
            // replaygain (OpusTags ReplayGain + R128)
            #[cfg(feature = "codec-opus")]
            true,
            #[cfg(not(feature = "codec-opus"))]
            false,
            // cover_art (OpusTags METADATA_BLOCK_PICTURE)
            #[cfg(feature = "codec-opus")]
            true,
            #[cfg(not(feature = "codec-opus"))]
            false,
            // chapters (container chapters not exposed)
            false,
            // ebu_r128 (OpusTags R128 tags)
            #[cfg(feature = "codec-opus")]
            true,
            #[cfg(not(feature = "codec-opus"))]
            false,
            // streaming (packet stream decode)
            #[cfg(feature = "codec-opus")]
            true,
            #[cfg(not(feature = "codec-opus"))]
            false,
            // Opus always decodes to f32 at 48 kHz — no native integer depth.
            None,
            false, // lossless (lossy codec)
        ),
        Codec::Wav => (
            CodecStatus::Available,
            true,
            true,
            false,
            true,
            true,
            true,
            false, // cover_art (RIFF LIST INFO has no standard art)
            false, // chapters
            false, // ebu_r128
            true,  // streaming
            Some(32),
            true,
        ),
        Codec::Aac => (
            CodecStatus::Available,
            true,
            true,
            true,
            true,
            true,
            true,
            true,  // cover_art (MP4 covr)
            false, // chapters
            true,  // ebu_r128 (iTunR128 in MP4)
            true,  // streaming
            Some(16),
            false,
        ),
        Codec::Alac => (
            // Symphonia 0.6.0's `alac` feature supplies the codec decoder;
            // ISO/MP4 supplies the container packet and cookie integration.
            CodecStatus::Available,
            true,
            true,
            true,
            true,
            true,
            true,
            true,  // cover_art (MP4 covr)
            false, // chapters
            true,  // ebu_r128 (iTunR128 in MP4)
            true,  // streaming
            Some(24),
            true,
        ),
        Codec::Ape => (
            // Audio decoding is supplied by the pure-Rust `ape-decoder` crate
            // (range coder + adaptive predictor reconstruction) when the
            // `codec-ape` feature is enabled; without it, only APEv2 tag
            // parsing is available and `.ape`/`.mac` files are rejected.
            #[cfg(feature = "codec-ape")]
            CodecStatus::Available,
            #[cfg(not(feature = "codec-ape"))]
            CodecStatus::MetadataOnly,
            #[cfg(feature = "codec-ape")]
            true,
            #[cfg(not(feature = "codec-ape"))]
            false,
            // Seek: the seek table + independently decodable frames give
            // sample-accurate seeking.
            true,
            // Gapless: APE has no encoder priming / end padding, so the
            // header's total sample count is already the logical length.
            true,
            // Multichannel: the backend reconstructs up to 32 channels
            // before the engine's stereo downmix boundary.
            #[cfg(feature = "codec-ape")]
            true,
            #[cfg(not(feature = "codec-ape"))]
            false,
            true,  // metadata (APEv2 tags)
            true,  // replaygain (APEv2 tags carry ReplayGain)
            false, // cover_art (no APE cover extraction in this build)
            false, // chapters
            false, // ebu_r128 (APE tags carry ReplayGain, not R128)
            false, // streaming (seek table requires random access)
            Some(24),
            true, // lossless
        ),
        Codec::Dsd => (
            // Native DSF/DFF reader: PCM decimation can expose source
            // multichannel layouts; DoP remains stereo-only.
            CodecStatus::Available,
            true,
            true,
            false,
            true,
            false,
            false,
            false, // cover_art (DSF ID3 cover not extracted)
            false, // chapters
            false, // ebu_r128
            false, // streaming (block-based random access)
            None,
            true,
        ),
        Codec::Pcm => (
            CodecStatus::Available,
            true,
            true,
            false,
            true,
            true,
            true,
            false, // cover_art
            false, // chapters
            false, // ebu_r128
            true,  // streaming
            Some(32),
            true,
        ),
        Codec::Aiff => (
            CodecStatus::Available,
            true,
            true,
            false,
            true,
            true,
            true,
            false, // cover_art (AIFF ID3 art not extracted)
            false, // chapters
            false, // ebu_r128
            true,  // streaming
            Some(32),
            true,
        ),
        Codec::Mka => (
            // Matroska audio container via Symphonia's `symphonia-format-mkv`
            // (enabled by `codec-mkv`). The inner codec — FLAC/AAC/Vorbis/
            // PCM/ALAC — is decoded by the corresponding codec feature; the
            // container itself always provides metadata (tags), seeking
            // (EBML cues), and multichannel tracks.
            #[cfg(feature = "codec-mkv")]
            CodecStatus::Available,
            #[cfg(not(feature = "codec-mkv"))]
            CodecStatus::DeclaredUnavailable,
            // decode: container-level; inner codec decides actual decodability.
            #[cfg(feature = "codec-mkv")]
            true,
            #[cfg(not(feature = "codec-mkv"))]
            false,
            true,  // seek (EBML cues)
            true,  // gapless (Matroska tracks carry CodecDelay/DiscardPadding)
            true,  // multichannel
            true,  // metadata (Matroska tags + attachments)
            true,  // replaygain (tags)
            true,  // cover_art (Matroska attachments)
            false, // chapters (not exposed by this build)
            true,  // ebu_r128 (tags)
            true,  // streaming
            None,  // precision depends on the inner codec
            false, // lossless depends on the inner codec
        ),
        Codec::WavPack => (
            // Pure-Rust `wavicle` codec (MIT/Apache-2.0, `#![forbid(unsafe_code)]`,
            // no FFI): lossless v5 decode of 16/24/32-bit integer and 32-bit
            // float, mono and stereo, bit-exact and verified against the
            // reference `wvunpack`. Multichannel / DSD / hybrid WavPack is
            // rejected explicitly at open (`src/decode/wavpack.rs`), and APEv2
            // tag metadata is not yet parsed (`metadata`/`replaygain` stay
            // false — honest, never claimed).
            #[cfg(feature = "codec-wavpack")]
            CodecStatus::Available,
            #[cfg(not(feature = "codec-wavpack"))]
            CodecStatus::DeclaredUnavailable,
            #[cfg(feature = "codec-wavpack")]
            true, // decode
            #[cfg(not(feature = "codec-wavpack"))]
            false,
            // seek: block-index seeks are sample-accurate
            true,
            // gapless: WavPack's total-sample count is the exact logical
            // length (no encoder priming / end padding), like APE.
            true,
            // multichannel: stereo/mono only (honest partial support)
            false,
            // metadata: APEv2/ID3 tag parsing not wired yet
            false,
            // replaygain: not parsed yet
            false,
            // cover_art
            false,
            // chapters
            false,
            // ebu_r128
            false,
            // streaming: block index requires random access at open
            false,
            Some(32),
            true, // lossless
        ),
        Codec::Musepack => (
            // Musepack has no pure-Rust decoder in this build.
            // Declared for future parity and rejected explicitly by the decode dispatch.
            CodecStatus::DeclaredUnavailable,
            false, // decode
            false, // seek
            false, // gapless
            false, // multichannel
            false, // metadata
            false, // replaygain
            false, // cover_art
            false, // chapters
            false, // ebu_r128
            false, // streaming
            Some(16),
            false, // lossless
        ),
        Codec::Tak => (
            // TAK (Tom's lossless Audio Kompressor) has no pure-Rust
            // decoder in this build. Declared for future parity and
            // rejected explicitly by the decode dispatch.
            CodecStatus::DeclaredUnavailable,
            false, // decode
            false, // seek
            false, // gapless
            false, // multichannel
            false, // metadata
            false, // replaygain
            false, // cover_art
            false, // chapters
            false, // ebu_r128
            false, // streaming
            Some(24),
            true, // lossless
        ),
        Codec::Tta => (
            // TTA (True Audio) lossless: native pure-Rust TTA1 decoder
            // (`src/decode/tta.rs`, enabled by the `codec-tta` feature).
            // Frame-indexed seeking, 8/16/24-bit, mono–16 channels.
            CodecStatus::Available,
            true,  // decode
            true,  // seek
            false, // gapless (no encoder-delay concept in TTA1)
            true,  // multichannel
            false, // metadata (APEv2 tag parsing not wired yet)
            false, // replaygain
            false, // cover_art
            false, // chapters
            false, // ebu_r128
            true,  // streaming (sequential frames)
            Some(24),
            true, // lossless
        ),
        Codec::Unknown => (
            CodecStatus::Available, // generic symphonia fallback
            true,
            true,
            false,
            true,
            true,
            true,
            false, // cover_art
            false, // chapters
            false, // ebu_r128
            true,  // streaming
            None,
            false,
        ),
    };
    let support_level = match (status, decode, seek, metadata) {
        (CodecStatus::MetadataOnly, _, _, true) => CodecSupportLevel::MetadataOnly,
        (CodecStatus::DeclaredUnavailable, _, _, _) => CodecSupportLevel::Unavailable,
        (CodecStatus::Available, true, true, _) => CodecSupportLevel::FullySupported,
        (CodecStatus::Available, true, false, _) => CodecSupportLevel::DecodeOnly,
        _ => CodecSupportLevel::Experimental,
    };
    CodecCapability {
        codec,
        status,
        support_level,
        decode,
        seek,
        gapless,
        multichannel,
        multichannel_decode: multichannel,
        metadata,
        replaygain,
        cover_art,
        chapters,
        ebu_r128,
        streaming,
        native_precision_bits: bits,
        is_lossless: lossless,
    }
}

/// Map a file extension (without the dot) to a [`Codec`], if recognised.
pub fn for_extension(ext: &str) -> Option<Codec> {
    let e = ext.to_ascii_lowercase();
    let codec = match e.as_str() {
        "mp3" => Codec::Mp3,
        "flac" => Codec::Flac,
        "ogg" | "oga" => Codec::OggVorbis,
        "opus" => Codec::Opus,
        "wav" | "wave" => Codec::Wav,
        // m4a/mp4 may contain AAC or ALAC; the decoder reports the actual inner codec.
        // The extension maps to AAC for broad container discovery; diagnostics use
        // `for_codec_string` after Symphonia identifies the inner stream.
        "aac" | "m4a" | "mp4" | "m4b" => Codec::Aac,
        "alac" => Codec::Alac,
        "ape" | "apl" | "mac" => Codec::Ape,
        "dsf" | "dff" => Codec::Dsd,
        "pcm" | "raw" => Codec::Pcm,
        "aiff" | "aif" | "aifc" => Codec::Aiff,
        // Matroska audio containers. Inner codec identity is reported after
        // probing; the extension only selects the container reader.
        "mka" | "mkv" | "webm" => Codec::Mka,
        "wv" => Codec::WavPack,
        "mpc" | "mp+" | "mpp" => Codec::Musepack,
        "tak" => Codec::Tak,
        "tta" => Codec::Tta,
        _ => return None,
    };
    Some(codec)
}

/// Map a Symphonia codec string (e.g. `decoder.info().codec`) to a [`Codec`].
pub fn for_codec_string(s: &str) -> Option<Codec> {
    let lower = s.to_ascii_lowercase();
    let codec = if lower.contains("mp3") || lower.contains("mp1") || lower.contains("mp2") {
        Codec::Mp3
    } else if lower.contains("flac") {
        Codec::Flac
    } else if lower.contains("vorbis") {
        Codec::OggVorbis
    } else if lower.contains("opus") {
        Codec::Opus
    } else if lower.contains("aiff") {
        Codec::Aiff
    } else if lower.contains("pcm")
        || lower.contains("wave")
        // Symphonia reports integer/float PCM with the sample-type codec
        // names (S8/S16/S24/S32, U8/U16, F32/F64) instead of "pcm".
        || lower.contains("s8")
        || lower.contains("s16")
        || lower.contains("s24")
        || lower.contains("s32")
        || lower.contains("u8")
        || lower.contains("u16")
        || lower.contains("f32")
        || lower.contains("f64")
    {
        Codec::Pcm
    } else if lower.contains("aac") {
        Codec::Aac
    } else if lower.contains("alac") {
        Codec::Alac
    } else if lower.contains("ape") || lower.contains("monkey") {
        Codec::Ape
    } else if lower.contains("dsd") {
        Codec::Dsd
    } else if lower.contains("wavpack") {
        Codec::WavPack
    } else if lower.contains("musepack") || lower.contains("mpc") {
        Codec::Musepack
    } else if lower.contains("tak") {
        Codec::Tak
    } else if lower.contains("tta") || lower.contains("true audio") {
        Codec::Tta
    } else {
        return None;
    };
    Some(codec)
}

/// All codecs the engine knows about, in a stable order.
pub fn all_codecs() -> &'static [Codec] {
    &[
        Codec::Mp3,
        Codec::Flac,
        Codec::OggVorbis,
        Codec::Opus,
        Codec::Wav,
        Codec::Aac,
        Codec::Alac,
        Codec::Ape,
        Codec::Dsd,
        Codec::Pcm,
        Codec::Aiff,
        Codec::Mka,
        Codec::WavPack,
        Codec::Musepack,
        Codec::Tak,
        Codec::Tta,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_support_levels_and_channel_semantics() {
        let flac = capability(Codec::Flac);
        assert_eq!(flac.support_level, CodecSupportLevel::FullySupported);
        assert!(flac.multichannel_decode);

        #[cfg(feature = "codec-ape")]
        {
            let ape = capability(Codec::Ape);
            assert_eq!(ape.support_level, CodecSupportLevel::FullySupported);
            assert!(ape.multichannel_decode, "APE backend decodes multichannel");
        }
    }

    #[test]
    fn test_available_codecs_are_decodable() {
        // Only codecs with Available status should have decode=true.
        let codecs: &[Codec] = &[
            Codec::Mp3,
            Codec::Flac,
            Codec::OggVorbis,
            Codec::Wav,
            Codec::Aac,
            Codec::Dsd,
            Codec::Pcm,
            Codec::Aiff,
            #[cfg(feature = "codec-ape")]
            Codec::Ape,
            #[cfg(feature = "codec-opus")]
            Codec::Opus,
            #[cfg(feature = "codec-wavpack")]
            Codec::WavPack,
        ];
        for &c in codecs {
            let cap = capability(c);
            assert!(cap.decode, "{c:?} must be decodable");
            assert_eq!(
                cap.status,
                CodecStatus::Available,
                "{c:?} must be Available"
            );
        }
    }

    #[cfg(feature = "codec-opus")]
    #[test]
    fn test_opus_is_available_with_feature() {
        let cap = capability(Codec::Opus);
        assert_eq!(cap.status, CodecStatus::Available);
        assert_eq!(cap.support_level, CodecSupportLevel::FullySupported);
        assert!(cap.decode);
        assert!(cap.seek, "Opus must seek via granule positions");
        assert!(cap.gapless, "OpusHead pre-skip must give gapless");
        assert!(cap.multichannel_decode, "Opus mapping families 0/1");
        assert!(cap.metadata, "OpusTags metadata");
        assert!(cap.replaygain, "OpusTags carry ReplayGain/R128");
        assert_eq!(cap.native_precision_bits, None, "Opus decodes to f32");
        assert!(!cap.is_lossless, "Opus is a lossy codec");
        // New capability axes (spec §8): Opus reads tags, cover art, R128,
        // and decodes progressively.
        assert!(cap.cover_art, "OpusTags METADATA_BLOCK_PICTURE");
        assert!(!cap.chapters, "no container chapter metadata exposed");
        assert!(cap.ebu_r128, "OpusTags R128 tags");
        assert!(cap.streaming, "packet stream decode");
    }

    #[test]
    fn test_new_capability_axes_are_consistent() {
        // Chapters are not exposed for any codec in this build (cue sheets
        // are handled by the separate `decode::cue` module).
        for c in all_codecs() {
            assert!(!capability(*c).chapters, "{c:?} must not claim chapters");
        }
        // Unavailable codecs claim nothing beyond their format definition.
        for c in all_codecs() {
            let cap = capability(*c);
            if cap.status == CodecStatus::DeclaredUnavailable {
                assert!(!cap.cover_art, "{c:?}");
                assert!(!cap.ebu_r128, "{c:?}");
                assert!(!cap.streaming, "{c:?}");
            }
        }
        // Cover art is extracted for the formats whose containers carry it
        // (ID3 APIC / FLAC PICTURE / METADATA_BLOCK_PICTURE / MP4 covr /
        // Matroska attachments).
        for c in [
            Codec::Mp3,
            Codec::Flac,
            Codec::OggVorbis,
            #[cfg(feature = "codec-opus")]
            Codec::Opus,
            Codec::Aac,
            Codec::Alac,
        ] {
            assert!(capability(c).cover_art, "{c:?} cover_art");
        }
        // EBU R128 tags are read wherever the container stores them.
        for c in [
            Codec::Mp3,
            Codec::Flac,
            Codec::OggVorbis,
            #[cfg(feature = "codec-opus")]
            Codec::Opus,
            Codec::Aac,
            Codec::Alac,
            Codec::Mka,
        ] {
            assert!(capability(c).ebu_r128, "{c:?} ebu_r128");
        }
    }

    #[test]
    fn test_alac_is_available() {
        let cap = capability(Codec::Alac);
        assert_eq!(cap.status, CodecStatus::Available);
        assert!(cap.decode, "ALAC must be decodable through Symphonia");
        assert!(cap.seek, "ISO/MP4 ALAC tracks must support seeking");
        assert!(cap.metadata, "ALAC container metadata should be parseable");
        assert!(cap.is_lossless);
    }

    #[test]
    fn test_aiff_is_available() {
        let cap = capability(Codec::Aiff);
        assert_eq!(cap.status, CodecStatus::Available);
        assert!(cap.decode, "AIFF must be decodable through Symphonia");
        assert!(cap.seek, "AIFF tracks must support seeking");
        assert!(cap.metadata, "AIFF metadata should be parseable");
        assert!(cap.is_lossless);
    }

    #[cfg(feature = "codec-ape")]
    #[test]
    fn test_ape_is_available() {
        let cap = capability(Codec::Ape);
        assert_eq!(cap.status, CodecStatus::Available);
        assert!(cap.decode, "APE must decode when codec-ape is enabled");
        assert!(cap.seek, "APE seek table gives sample-accurate seeking");
        assert!(cap.gapless, "APE is inherently gapless");
        assert!(cap.multichannel_decode, "APE backend decodes multichannel");
        assert!(cap.metadata, "APEv2 tags must be parseable");
        assert!(cap.replaygain, "APEv2 tags carry ReplayGain");
        assert!(cap.is_lossless);
    }

    #[cfg(not(feature = "codec-ape"))]
    #[test]
    fn test_ape_is_metadata_only() {
        let cap = capability(Codec::Ape);
        assert_eq!(
            cap.status,
            CodecStatus::MetadataOnly,
            "APE audio decoding requires the codec-ape feature"
        );
        assert!(
            !cap.decode,
            "APE must not claim decode=true without codec-ape"
        );
        assert!(cap.metadata, "APEv2 tags must be parseable");
        assert!(cap.replaygain, "APEv2 tags carry ReplayGain");
    }

    #[test]
    fn test_dsd_capabilities() {
        let cap = capability(Codec::Dsd);
        assert!(cap.decode && cap.seek);
        assert!(!cap.gapless, "DSD has no encoder-delay gapless framing");
        assert!(
            cap.multichannel_decode,
            "DSD PCM decode exposes multichannel DSF layouts"
        );
        assert!(cap.is_lossless);
    }

    #[test]
    fn test_extension_mapping() {
        assert_eq!(for_extension("MP3"), Some(Codec::Mp3));
        assert_eq!(for_extension("dsf"), Some(Codec::Dsd));
        assert_eq!(for_extension("dff"), Some(Codec::Dsd));
        assert_eq!(for_extension("flac"), Some(Codec::Flac));
        assert_eq!(for_extension("ape"), Some(Codec::Ape));
        assert_eq!(for_extension("alac"), Some(Codec::Alac));
        assert_eq!(for_extension("opus"), Some(Codec::Opus));
        assert_eq!(for_extension("aiff"), Some(Codec::Aiff));
        assert_eq!(for_extension("aif"), Some(Codec::Aiff));
        assert_eq!(for_extension("mka"), Some(Codec::Mka));
        assert_eq!(for_extension("mkv"), Some(Codec::Mka));
        assert_eq!(for_extension("webm"), Some(Codec::Mka));
        assert_eq!(for_extension("wv"), Some(Codec::WavPack));
        assert_eq!(for_extension("mpc"), Some(Codec::Musepack));
        assert_eq!(for_extension("tak"), Some(Codec::Tak));
        assert_eq!(for_extension("tta"), Some(Codec::Tta));
        assert_eq!(for_extension("xyz"), None);
    }

    #[test]
    fn test_declared_unavailable_codecs() {
        // Opus/WavPack/TTA are only unavailable when their features are off.
        #[cfg(all(
            not(feature = "codec-opus"),
            not(feature = "codec-wavpack"),
            not(feature = "codec-tta")
        ))]
        let unavailable: &[Codec] = &[
            Codec::Opus,
            Codec::WavPack,
            Codec::Musepack,
            Codec::Tak,
            Codec::Tta,
        ];
        #[cfg(all(
            feature = "codec-opus",
            not(feature = "codec-wavpack"),
            not(feature = "codec-tta")
        ))]
        let unavailable: &[Codec] = &[Codec::WavPack, Codec::Musepack, Codec::Tak, Codec::Tta];
        #[cfg(all(
            feature = "codec-opus",
            feature = "codec-wavpack",
            not(feature = "codec-tta")
        ))]
        let unavailable: &[Codec] = &[Codec::Musepack, Codec::Tak, Codec::Tta];
        #[cfg(all(
            feature = "codec-opus",
            feature = "codec-wavpack",
            feature = "codec-tta"
        ))]
        let unavailable: &[Codec] = &[Codec::Musepack, Codec::Tak];
        for &c in unavailable {
            let cap = capability(c);
            assert_eq!(cap.status, CodecStatus::DeclaredUnavailable, "{c:?} status");
            assert_eq!(
                cap.support_level,
                CodecSupportLevel::Unavailable,
                "{c:?} support level"
            );
            assert!(!cap.decode, "{c:?} must not claim decode=true");
            assert!(!cap.seek, "{c:?} must not claim seek");
            assert!(!cap.metadata, "{c:?} must not claim metadata extraction");
            assert!(!cap.replaygain, "{c:?} must not claim ReplayGain reading");
        }
        // Declared-but-unavailable lossless codecs are still lossless by
        // format definition — the flag describes the format, not this build.
        assert!(capability(Codec::Tak).is_lossless);
        assert!(capability(Codec::Tta).is_lossless);
        assert!(capability(Codec::WavPack).is_lossless);

        // TTA is fully available when its feature is on.
        #[cfg(feature = "codec-tta")]
        {
            let tta = capability(Codec::Tta);
            assert_eq!(tta.status, CodecStatus::Available);
            assert_eq!(tta.support_level, CodecSupportLevel::FullySupported);
            assert!(tta.decode && tta.seek && tta.multichannel_decode && tta.streaming);
            assert!(tta.is_lossless);
        }

        assert_eq!(for_codec_string("opus"), Some(Codec::Opus));
        assert_eq!(for_codec_string("WavPack"), Some(Codec::WavPack));
        assert_eq!(for_codec_string("musepack"), Some(Codec::Musepack));
        assert_eq!(for_codec_string("tak"), Some(Codec::Tak));
        assert_eq!(for_codec_string("tta"), Some(Codec::Tta));
        assert_eq!(for_codec_string("aiff"), Some(Codec::Aiff));
    }

    #[test]
    fn test_mka_container_capabilities() {
        let cap = capability(Codec::Mka);
        #[cfg(feature = "codec-mkv")]
        {
            assert_eq!(cap.status, CodecStatus::Available);
            assert!(cap.seek, "Matroska EBML cues must give seeking");
            assert!(
                cap.gapless,
                "Matroska tracks carry CodecDelay/DiscardPadding"
            );
            assert!(cap.metadata, "Matroska tags must be extractable");
        }
        #[cfg(not(feature = "codec-mkv"))]
        {
            assert_eq!(cap.status, CodecStatus::DeclaredUnavailable);
            assert!(!cap.decode, "MKA decode requires the codec-mkv feature");
        }
    }

    #[test]
    fn test_registry_rows_are_consistent() {
        // Available → decode=true. Non-available → decode=false.
        // This invariant is the single source of truth the UI depends on.
        for c in all_codecs() {
            let cap = capability(*c);
            assert_eq!(cap.codec, *c);
            match cap.status {
                CodecStatus::Available => {
                    assert!(cap.decode, "{c:?}: Available must imply decode=true");
                }
                CodecStatus::MetadataOnly | CodecStatus::DeclaredUnavailable => {
                    assert!(!cap.decode, "{c:?}: non-Available must imply decode=false");
                }
            }
        }
    }
}
