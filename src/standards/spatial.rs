//! Formal standards models for spatial coordinates, Ambisonics, and scene orientation.
//!
//! Governs:
//! - Coordinate systems (Right-Handed Cartesian: +X right, +Y forward, +Z up)
//! - Polar conventions (Azimuth: counter-clockwise from +Y forward; Elevation: positive above horizontal)
//! - Higher-Order Ambisonics (HOA) ordering and normalization (ACN, SN3D, N3D, Max-rE) per ITU-R BS.2076

use serde::{Deserialize, Serialize};

/// Coordinate frame convention standard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateSystemStandard {
    /// Right-Handed Cartesian (+X right, +Y forward, +Z up).
    /// ISO 2631 / Audio Engineering standard frame.
    #[default]
    RightHandedCartesian,
    /// OpenGL / Camera convention (+X right, +Y up, +Z backward).
    OpenGlCamera,
}

/// Higher-Order Ambisonic channel ordering convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbisonicOrderingStandard {
    /// Ambisonic Channel Number (ACN): order l, degree m index = l*(l + 1) + m.
    /// ITU-R BS.2076-2 / Google Spatial Audio standard.
    #[default]
    Acn,
    /// Furse-Malham (FuMa) order (legacy B-format order 1 / 2).
    FurseMalham,
    /// Spatial Audio Data Format (SID).
    Sid,
}

/// Higher-Order Ambisonic spherical harmonics normalization standard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbisonicNormalizationStandard {
    /// Schmidt semi-normalized spherical harmonics (SN3D).
    /// Used by ITU-R BS.2076 ADM and MPEG-H 3D Audio.
    #[default]
    Sn3d,
    /// Fully normalized spherical harmonics (N3D).
    N3d,
    /// Maximum energy vector weighting (Max-rE).
    MaxRe,
}

impl CoordinateSystemStandard {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::RightHandedCartesian => "Right-Handed Cartesian (+X right, +Y forward, +Z up)",
            Self::OpenGlCamera => "OpenGL (+X right, +Y up, -Z forward)",
        }
    }
}
