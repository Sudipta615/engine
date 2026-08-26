use std::path::Path;

pub mod ape;
pub mod codecs;
pub mod cue;
pub mod decoder;
pub mod dsd;
pub mod fingerprint;
pub mod loudness_cache;
#[cfg(feature = "codec-opus")]
pub mod opus;
pub mod scanner;
pub mod symphonia_decoder;
pub mod tags;
#[cfg(feature = "codec-tta")]
pub mod tta;
#[cfg(feature = "codec-wavpack")]
pub mod wavpack;

#[cfg(feature = "codec-ape")]
pub use ape::ApeDecoder;
pub use codecs::{
    all_codecs, capability, for_codec_string, for_extension, Codec, CodecCapability, CodecStatus,
    CodecSupportLevel,
};
pub use cue::{CueIndex, CueParseError, CueSheet, CueTrack};
pub use decoder::Decoder;
pub use dsd::{
    DopPacker, DsdBlock, DsdDecoder, DsdError, DsdPcmBlock, DsdRate, DsdReader, DsdToPcmDecimator,
    DsdWireFormat, NativeDsdPacker,
};
pub use fingerprint::{
    extract_fingerprint, fingerprint_to_hex, AudioFingerprint, FingerprintError,
};
#[cfg(feature = "codec-opus")]
pub use opus::OpusSource;
pub use scanner::{scan_track_loudness, LoudnessScanResult};
pub use symphonia_decoder::{
    downmix_interleaved_to_stereo, extract_loudness_metadata_symphonia, DecodeError, DecodeInfo,
    DecodedChunk, SymphoniaDecoder,
};
#[cfg(feature = "tag-write")]
pub use tags::write_loudness_tags;
pub use tags::TagWriteError;
#[cfg(feature = "codec-tta")]
pub use tta::TtaDecoder;
#[cfg(feature = "codec-wavpack")]
pub use wavpack::WavpackDecoder;

// ── Format-routing metadata extractors ───────────────────────────────────────
//
pub mod channel_layout;
pub mod channel_mix;
pub mod format_descriptors;

// Re-export types now living in sub-modules
pub use channel_layout::{ChannelId, ChannelLayout};
pub use channel_mix::{mix_interleaved_to_stereo_with_template, mix_interleaved_with_template};
pub use format_descriptors::{
    AudioFormatInfo, DsdTransport, DsdTransportReport, GaplessInfo, RawDsdChunk,
};

// The standalone metadata extractors below dispatch by file extension so a
// single entry point serves every codec: Ogg Opus tags can only be read by
// the Opus backend (`opus-decoder`/`ogg`), everything else by Symphonia's
// probe. Callers should use these instead of reaching into the per-backend
// modules.

/// True when the path is an Ogg Opus file and the `codec-opus` feature is
/// enabled (OpusTags are not readable through Symphonia's probe).
#[allow(dead_code)]
fn is_opus_path(path: &Path) -> bool {
    #[cfg(feature = "codec-opus")]
    {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("opus"))
    }
    #[cfg(not(feature = "codec-opus"))]
    {
        let _ = path;
        false
    }
}

/// Extract title, artist, album, duration (seconds), and a formatted
/// duration string. Routes `.opus` to the Opus backend, everything else to
/// Symphonia.
pub fn extract_track_metadata(path: &Path) -> (String, String, String, f64, String) {
    #[cfg(feature = "codec-opus")]
    if is_opus_path(path) {
        return opus::extract_track_metadata(path);
    }
    symphonia_decoder::extract_track_metadata(path)
}

/// Extract ReplayGain / EBU R128 loudness metadata from file tags. Routes
/// `.opus` to the Opus backend (OpusTags), everything else to Symphonia.
pub fn extract_loudness_metadata(path: &Path) -> crate::dsp::LoudnessMetadata {
    #[cfg(feature = "codec-opus")]
    if is_opus_path(path) {
        return opus::extract_loudness_metadata(path);
    }
    symphonia_decoder::extract_loudness_metadata_symphonia(path)
}

/// Percent-decode a URI-encoded string (e.g. `%20` → space).
/// Returns `None` if the encoding is malformed.
pub fn percent_decode(s: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chars = s.as_bytes().iter().copied();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next()?;
            let h2 = chars.next()?;
            let pair = [h1, h2];
            let hex = std::str::from_utf8(&pair).ok()?;
            let val = u8::from_str_radix(hex, 16).ok()?;
            bytes.push(val);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}
