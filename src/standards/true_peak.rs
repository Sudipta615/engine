//! Formal standards models for true-peak level measurement.
//!
//! True-peak metering per ITU-R BS.1770-5 Annex 2 specifies 4× oversampling
//! (for fs ≤ 48 kHz) or 2× oversampling (for 96 kHz) using a linear-phase
//! FIR interpolation filter with ≥ 100 dB stopband attenuation and < 0.01 dB
//! passband ripple.

use serde::{Deserialize, Serialize};

/// True-peak measurement standard specification.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruePeakStandard {
    /// ITU-R BS.1770-5 Annex 2 (4× polyphase FIR oversampled peak meter).
    #[default]
    ItuBs1770_5_Annex2,
    /// EBU Tech 3341 §2 (True-peak metering with max −1.0 dBTP ceiling).
    EbuTech3341,
    /// SMPTE ST 2036-3 (Immersive audio true-peak reference).
    Smpte2036,
}

impl TruePeakStandard {
    /// Specification title.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::ItuBs1770_5_Annex2 => "ITU-R BS.1770-5 Annex 2",
            Self::EbuTech3341 => "EBU Tech 3341 §2",
            Self::Smpte2036 => "SMPTE ST 2036-3",
        }
    }

    /// Minimum oversampling factor for baseband audio (≤ 48 kHz).
    pub const fn min_oversampling_factor(&self) -> usize {
        4
    }

    /// Minimum stopband attenuation in dB.
    pub const fn min_stopband_attenuation_db(&self) -> f64 {
        100.0
    }

    /// Maximum passband ripple in dB.
    pub const fn max_passband_ripple_db(&self) -> f64 {
        0.01
    }
}

impl std::fmt::Display for TruePeakStandard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
