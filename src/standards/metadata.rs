//! Formal standards models for audio container metadata.
//!
//! Covers:
//! - Broadcast Wave Format (BWF, EBU Tech 3285)
//! - Broadcast Wave 64-bit (BW64, ITU-R BS.2088)
//! - iXML (XML metadata specification for audio files)
//! - ID3v2.4 (Informal tag standard)
//! - VorbisComment (Ogg / FLAC metadata specification)
//! - APEv2 (Monkey's Audio / WavPack metadata specification)

use serde::{Deserialize, Serialize};

/// Recognized professional and consumer metadata container standards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataStandard {
    /// EBU Tech 3285 (Broadcast Wave Format `bext` chunk).
    #[default]
    BwfBext,
    /// ITU-R BS.2088 (BW64 with `chna` and `axml` ADM chunks).
    Bw64,
    /// iXML specification for location sound recorders.
    IXml,
    /// ID3v2.4.0 (ISO 8859-1 and UTF-8 frames).
    Id3v2_4,
    /// Xiph.org VorbisComment specification.
    VorbisComment,
    /// APEv2 tag specification.
    ApeV2,
}

impl MetadataStandard {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::BwfBext => "EBU Tech 3285 (BWF bext)",
            Self::Bw64 => "ITU-R BS.2088 (BW64)",
            Self::IXml => "iXML Specification",
            Self::Id3v2_4 => "ID3v2.4.0",
            Self::VorbisComment => "Xiph VorbisComment",
            Self::ApeV2 => "APEv2",
        }
    }
}

impl std::fmt::Display for MetadataStandard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
