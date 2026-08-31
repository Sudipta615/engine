//! Typed, serializable diagnostic categories shared by the engine's telemetry
//! and the C FFI.
//!
//! Historically `PlaybackInfo` carried error state as a bare `Option<String>`
//! (`engine_error`) and the bit-perfect verdict's failure cause as a string
//! ([`BitPerfectReport::reason`](crate::dsp::pipeline::BitPerfectReport::reason)).
//! Hosts could display those, but they could not branch on *why*. This module
//! introduces stable category types, so a host can react programmatically:
//!
//! - [`DiagnosticKind`] classifies an engine error (output / resampler /
//!   decoder / stream / configuration / …). The existing human message is
//!   preserved in [`Diagnostic::message`], so the string surface never goes
//!   away — a typed [`Diagnostic::code`] now sits beside it.
//! - [`BitPerfectCause`] classifies exactly which stage invalidates the
//!   bit-perfect verdict (volume, EQ, dynamics, sample-rate, transport, …).
//!
//! Both are `Serialize`/`Deserialize`, so structured diagnostics can be
//! persisted, logged as JSON, or handed through an FFI boundary. Their
//! `Display`/`code()` strings are stable API — do not rename them casually.

use serde::{Deserialize, Serialize};

/// Stable, coarse category for an engine diagnostic.
///
/// This is intentionally coarse — one bucket per subsystem — so a host can
/// route, filter, or meter errors without parsing prose. The fine detail stays
/// in [`Diagnostic::message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum DiagnosticKind {
    /// Uncategorised / internal invariant violation.
    #[default]
    Internal,
    /// Source decode / decoder failure.
    Decoder,
    /// Output backend / device / sample-format failure.
    Output,
    /// Resampler creation or building failure.
    Resampler,
    /// Playback stream recovery or health failure.
    Stream,
    /// A configured additional output endpoint's transport failure.
    Endpoint,
    /// Bit-perfectness could not be proven (see [`BitPerfectCause`]).
    BitPerfect,
    /// Configuration / model validation failure.
    Configuration,
    /// Spatial renderer / voice-budget failure.
    Spatial,
    /// Loudness scanning / normalisation failure.
    Loudness,
}

impl DiagnosticKind {
    /// Stable machine-readable code (used across FFI / JSON). Do not rename.
    pub fn code(self) -> &'static str {
        match self {
            DiagnosticKind::Internal => "internal",
            DiagnosticKind::Decoder => "decoder",
            DiagnosticKind::Output => "output",
            DiagnosticKind::Resampler => "resampler",
            DiagnosticKind::Stream => "stream",
            DiagnosticKind::Endpoint => "endpoint",
            DiagnosticKind::BitPerfect => "bit_perfect",
            DiagnosticKind::Configuration => "configuration",
            DiagnosticKind::Spatial => "spatial",
            DiagnosticKind::Loudness => "loudness",
        }
    }
}

impl std::fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// A typed engine diagnostic: a coarse category plus the human-readable
/// message that previously lived alone in `PlaybackInfo::engine_error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

impl Diagnostic {
    pub fn new(kind: DiagnosticKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> DiagnosticKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// Stable machine-readable category code (a convenience over `kind`).
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Exactly which stage invalidates the bit-perfect verdict (§13). Programmatic
/// counterpart to the human `reason` string on the bit-perfect report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum BitPerfectCause {
    #[default]
    None,
    /// Volume / balance / preamp is not unity (0 dB).
    VolumeNotUnity,
    /// A parametric / graphic EQ stage is active.
    EqActive,
    /// A dynamics / DSP processor (compressor, convolution, correction,
    /// crossfeed, stereo width, loudness, limiter) is active.
    DynamicsActive,
    /// The resampler is active, or playback speed ≠ 1.0.
    SpeedOrResampleActive,
    /// Source and output sample rates differ with the resampler bypassed.
    SampleRateMismatch,
    /// Source or output precision is unknown; bit-perfect cannot be proven.
    UnknownPrecision,
    /// The output container truncates source bits.
    BitDepthTruncation,
    /// Source precision is not lossless through the output container.
    FormatConversionLossy,
    /// Output is not direct / exclusive hardware.
    OutputNotDirectExclusive,
    /// Dither is applied at the integer quantization boundary.
    DitherActive,
    /// A crossfade / gapless transition is blending two tracks.
    CrossfadeActive,
}

impl BitPerfectCause {
    /// Stable machine-readable code (used across FFI / JSON). Do not rename.
    pub fn code(self) -> &'static str {
        match self {
            BitPerfectCause::None => "none",
            BitPerfectCause::VolumeNotUnity => "volume_not_unity",
            BitPerfectCause::EqActive => "eq_active",
            BitPerfectCause::DynamicsActive => "dynamics_active",
            BitPerfectCause::SpeedOrResampleActive => "speed_or_resample_active",
            BitPerfectCause::SampleRateMismatch => "sample_rate_mismatch",
            BitPerfectCause::UnknownPrecision => "unknown_precision",
            BitPerfectCause::BitDepthTruncation => "bit_depth_truncation",
            BitPerfectCause::FormatConversionLossy => "format_conversion_lossy",
            BitPerfectCause::OutputNotDirectExclusive => "output_not_direct_exclusive",
            BitPerfectCause::DitherActive => "dither_active",
            BitPerfectCause::CrossfadeActive => "crossfade_active",
        }
    }
}

impl std::fmt::Display for BitPerfectCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_displayable() {
        for kind in [
            DiagnosticKind::Internal,
            DiagnosticKind::Decoder,
            DiagnosticKind::Output,
            DiagnosticKind::Resampler,
            DiagnosticKind::Stream,
            DiagnosticKind::Endpoint,
            DiagnosticKind::BitPerfect,
            DiagnosticKind::Configuration,
            DiagnosticKind::Spatial,
            DiagnosticKind::Loudness,
        ] {
            assert!(!kind.code().is_empty());
            assert_eq!(format!("{kind}"), kind.code());
        }
    }

    #[test]
    fn diagnostic_round_trips_through_json() {
        let d = Diagnostic::new(DiagnosticKind::Resampler, "build failed");
        assert_eq!(d.kind(), DiagnosticKind::Resampler);
        assert_eq!(d.code(), "resampler");
        assert_eq!(d.message(), "build failed");
        assert_eq!(format!("{d}"), "build failed");

        let json = serde_json::to_string(&d).unwrap();
        let back: Diagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn bit_perfect_cause_is_stable() {
        for c in [
            BitPerfectCause::None,
            BitPerfectCause::VolumeNotUnity,
            BitPerfectCause::EqActive,
            BitPerfectCause::DynamicsActive,
            BitPerfectCause::SpeedOrResampleActive,
            BitPerfectCause::SampleRateMismatch,
            BitPerfectCause::UnknownPrecision,
            BitPerfectCause::BitDepthTruncation,
            BitPerfectCause::FormatConversionLossy,
            BitPerfectCause::OutputNotDirectExclusive,
            BitPerfectCause::DitherActive,
            BitPerfectCause::CrossfadeActive,
        ] {
            assert_eq!(format!("{c}"), c.code());
        }
        let json = serde_json::to_string(&BitPerfectCause::EqActive).unwrap();
        assert_eq!(
            serde_json::from_str::<BitPerfectCause>(&json).unwrap(),
            BitPerfectCause::EqActive
        );
    }
}
