//! Formal standards models for physical and multichannel speaker layouts.
//!
//! Encompasses recommendations from:
//! - ITU-R BS.775 (Multichannel stereophonic sound system with and without accompanying picture)
//! - ITU-R BS.2051 (Advanced sound system for programme production)
//! - SMPTE ST 2036-2 (UHDTV Audio formats)
//! - Microsoft WAVEFORMATEXTENSIBLE channel mask conventions

use serde::{Deserialize, Serialize};

/// Standard specification governing channel layouts and speaker placements.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayoutStandard {
    /// ITU-R BS.775-4 (Standard 5.1 / stereo layouts).
    #[default]
    ItuR_BS775,
    /// ITU-R BS.2051-3 (Immersive Sound Systems: System A through J, e.g. 7.1.4, 9.1.6).
    ItuR_BS2051,
    /// SMPTE ST 2036-2 (Multichannel programme production).
    Smpte2036,
    /// Microsoft WAVEFORMATEXTENSIBLE speaker mask (USB Audio Class 2).
    WavChannelMask,
}

impl ChannelLayoutStandard {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::ItuR_BS775 => "ITU-R BS.775-4",
            Self::ItuR_BS2051 => "ITU-R BS.2051-3",
            Self::Smpte2036 => "SMPTE ST 2036-2",
            Self::WavChannelMask => "WAVEFORMATEXTENSIBLE / UAC2",
        }
    }
}

impl std::fmt::Display for ChannelLayoutStandard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
