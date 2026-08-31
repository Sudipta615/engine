//! Deterministic perceptual audio profile (the "AudioProfile" layer).
//!
//! [`AudioProfile`] fuses loudness, dynamics, spectral, transient, stereo,
//! spatial, and content measurements into **one versioned, serializable
//! model** that consumers (spatial defaults, AutoEQ aggressiveness, upmix
//! selection, quality diagnostics, hosts) can request selectively and cache
//! by content fingerprint.
//!
//! # Design rules
//!
//! - **Deterministic DSP only.** Every feature is derived from standard
//!   measurements (BS.1770-4 loudness via the shared [`LoudnessMeter`], a
//!   Hann-windowed FFT, running mid/side and L/R statistics, onset energy
//!   deltas). There is no learned model; the interface is deliberately shaped
//!   so a future tiny ML feature-supplier can fill the same fields without
//!   changing consumers.
//! - **Off the audio path.** Analysis is a push-based, bounded-memory
//!   streaming pass ([`ProfileAnalyzer`]) intended for background threads or
//!   offline scanning — never the realtime callback. The audio callback is
//!   untouched.
//! - **Selective analysis.** [`AnalysisMask`] lets a consumer compute only
//!   the sub-profiles it needs; unrequested sub-profiles report `None`
//!   core values and cost no work.
//! - **Cacheable.** [`super::profile::cache`] persists profiles on disk,
//!   validated against file size/mtime and optionally keyed by a content
//!   fingerprint for cross-path deduplication.
//! - **Honest confidence.** [`AudioProfile::confidence`] reflects how much
//!   audio was analyzed and how many requested sub-profiles actually
//!   produced data. Every numeric field is `Option`-typed and documented
//!   with its unit and range; fields are only ever filled with finite
//!   values so profiles stay JSON-round-trippable.
//!
//! [`LoudnessMeter`]: crate::dsp::LoudnessMeter

use serde::{Deserialize, Serialize};

pub mod analysis;
pub mod cache;

pub use analysis::{
    analyze_decoder, analyze_path, analyze_path_cached, analyze_path_cached_by_fingerprint,
    ProfileAnalyzer,
};
pub use cache::{lookup, lookup_for_id, store, store_with_id};

/// Current schema version of [`AudioProfile`]. Bump when the model changes;
/// the on-disk cache rejects profiles with a different version.
pub const AUDIO_PROFILE_VERSION: u32 = 1;

/// FFT size for the spectral pass (4096 → ~10.8 Hz resolution at 44.1 kHz).
pub const PROFILE_FFT_SIZE: usize = 4096;

/// Analysis duration (seconds) at/above which the duration component of
/// [`AudioProfile::confidence`] reaches 1.0.
pub const PROFILE_REQUIRED_SECS: f32 = 30.0;

/// Error returned by the profile analysis entry points.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("decode error: {0}")]
    Decode(#[from] crate::decode::DecodeError),
    #[error("no audio frames were decoded for profiling")]
    NoAudio,
}

/// Which sub-profiles a consumer wants computed.
///
/// Default is *all*; disable the ones you do not need so the analysis pass
/// skips their (cheap, but non-zero) accumulators. The mask is stored on the
/// resulting [`AudioProfile`] so readers know which parts were computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisMask {
    pub loudness: bool,
    pub dynamics: bool,
    pub spectral: bool,
    pub transient: bool,
    pub stereo: bool,
    pub spatial: bool,
    pub content: bool,
}

impl Default for AnalysisMask {
    /// Every sub-profile enabled (the useful default).
    fn default() -> Self {
        Self::all()
    }
}

impl AnalysisMask {
    /// Every sub-profile enabled.
    pub const ALL: Self = Self {
        loudness: true,
        dynamics: true,
        spectral: true,
        transient: true,
        stereo: true,
        spatial: true,
        content: true,
    };

    /// Every sub-profile enabled (the [`Default`] value).
    pub const fn all() -> Self {
        Self::ALL
    }
}

/// Perceived loudness and loudness stability (EBU R128 / BS.1770-4).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LoudnessProfile {
    /// Integrated loudness in LUFS (dual-threshold gated). Range: typically
    /// −35…0 LUFS. `None` when the signal never rose above the −70 LUFS
    /// absolute gate.
    pub integrated_lufs: Option<f32>,
    /// Short-term loudness (3 s window at end of analysis) in LUFS.
    pub short_term_lufs: Option<f32>,
    /// True peak in dBTP (the shared 4× FIR oversampled detector).
    pub true_peak_dbtp: Option<f32>,
    /// Loudness range in LU (10th–95th percentile of gated blocks). `None`
    /// when the source is shorter than ~6 s or never gated (same
    /// `lra_valid` semantics as the meter).
    pub loudness_range_lu: Option<f32>,
    /// Loudness stability, 0..1 — `1 - (std of momentary LUFS / 12 LU)`,
    /// clamped. 1 = rock-steady programme level; 0 = ±12 LU swings.
    pub stability: Option<f32>,
}

/// Dynamic character of the programme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DynamicCharacter {
    /// Not enough data (no crest factor and no LRA).
    #[default]
    Unknown,
    /// Crest ≤ 8 dB or LRA ≤ 5 LU.
    Compressed,
    /// Crest/LRA between the compressed and dynamic thresholds.
    Moderate,
    /// Crest ≥ 16 dB or LRA ≥ 14 LU.
    Dynamic,
}

/// Dynamics measurements (crest factor, LRA, compression heuristic).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DynamicProfile {
    /// Crest factor in dB (`20·log10(peak / RMS)` of the mono mix), ≥ 0.
    pub crest_factor_db: Option<f32>,
    /// Dynamic range in dB — the LRA (10th–95th percentile), equivalent in
    /// unit to dB for programme range.
    pub dynamic_range_db: Option<f32>,
    /// Coarse dynamic character class (see [`DynamicCharacter`]).
    pub character: DynamicCharacter,
    /// Compression heuristic, 0..1 — `1 - (LRA / 18 LU)` (or
    /// `1 - (crest / 16 dB)` when LRA is unavailable). 1 = heavily
    /// compressed, 0 = wide-open. A heuristic, not a calibrated loudness-war
    /// measure.
    pub compression: Option<f32>,
}

/// Spectral balance features, all derived from one averaged power spectrum
/// (Hann-windowed, 50%-overlapped FFTs).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpectralProfile {
    /// Spectral centroid in Hz (power-weighted mean frequency).
    pub centroid_hz: Option<f32>,
    /// Spectral rolloff in Hz: the frequency below which 85% of the energy
    /// sits (brightness boundary).
    pub rolloff_hz: Option<f32>,
    /// Spectral tilt in dB per octave — least-squares slope of the dB
    /// spectrum vs. log2 frequency over 100 Hz–10 kHz. Typically negative
    /// (e.g. −3…−10 dB/oct for music); near 0 = flat/white.
    pub slope_db_per_octave: Option<f32>,
    /// Spectral flatness, 0..1 — geometric/arithmetic mean of the power
    /// spectrum. 0 = pure tone, 1 = white noise.
    pub flatness: Option<f32>,
    /// Brightness in dB: `10·log10(energy 2–10 kHz / energy 20–200 Hz)`.
    /// 0 dB = equal energy in the two bands; positive = brighter.
    pub brightness_db: Option<f32>,
}

/// Transient (onset) behaviour, from per-~23 ms window RMS energy deltas.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TransientProfile {
    /// Onset density in events per second (a window whose RMS jumps ≥ 10 dB
    /// over the previous window, above a −50 dBFS floor, counts as an onset).
    pub density_per_sec: Option<f32>,
    /// Mean onset strength, 0..1 — mean onset excess in dB divided by 20
    /// (a 20 dB jump maps to 1.0).
    pub strength: Option<f32>,
}

/// Stereo image measurements from the L/R pair.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StereoProfile {
    /// Channel correlation, −1..1 (full-track running L·R covariance).
    /// 1 = mono, 0 = decorrelated, −1 = fully out of phase.
    pub correlation: Option<f32>,
    /// Perceived width, 0..1 — `1 − |correlation|` heuristic. 0 = mono,
    /// 1 = fully decorrelated.
    pub width: Option<f32>,
    /// L/R balance in dB (`10·log10(L energy / R energy)`); 0 = centered,
    /// positive = left-heavy. `None` for mono input.
    pub balance_db: Option<f32>,
    /// Phase risk, 0..1 — fraction of analysis windows whose correlation was
    /// below −0.4 (out-of-phase content risks cancellation on mono fold-down).
    pub phase_risk: Option<f32>,
}

/// Spatial/ambience indicators. Deterministic proxies from the mid/side
/// decomposition (no full spatial render is run — these describe the *input*
/// signal's apparent spatial density).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpatialProfile {
    /// Side-energy fraction, 0..1 — `side² / (mid² + side²)`. 0 = fully
    /// mono-centric; higher = wider/ambient-heavy. `None` for mono input.
    pub side_fraction: Option<f32>,
    /// Ambience in dB — `10·log10(side energy / mid energy)`. Negative =
    /// mono-centric; 0 = equal; positive = wide/decorrelated (ambience,
    /// reverb, stereo room tone).
    pub ambience_db: Option<f32>,
}

/// Masking-relevant indicators (heuristics, not a cochlear model).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MaskingProfile {
    /// Tonal-masker density, 0..1 — `1 − spectral flatness`. 1 = strong
    /// tonal maskers (harmonics, resonances).
    pub tonal_density: Option<f32>,
    /// Dynamic masking risk, 0..1 — crest factor / 20 dB. High crest
    /// (transient-heavy) content can mask quieter passages.
    pub dynamic_risk: Option<f32>,
}

/// Heuristic content-class probabilities (soft indicators, **not** a
/// calibrated speech/music/ambience model). The three probabilities are
/// normalized to sum ≈ 1. Treat them as directional evidence, gated by
/// [`AudioProfile::confidence`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ContentProfile {
    /// Speech probability, 0..1.
    pub speech: f32,
    /// Music probability, 0..1.
    pub music: f32,
    /// Ambient/noise probability, 0..1.
    pub ambient: f32,
    /// Masking indicators.
    pub masking: MaskingProfile,
    /// Whether any evidence input (spectrum, transients, stereo, silence)
    /// was available. `false` → the probabilities are the neutral ⅓/⅓/⅓
    /// prior and should be ignored.
    pub evidence: bool,
}

/// The consolidated perceptual profile.
///
/// `confidence` (0..1) combines how much audio was analyzed
/// (`min(1, duration_secs / 30)` — LRA and spectral averages stabilize
/// after ~30 s) with coverage (the fraction of *requested* sub-profiles that
/// produced at least one valid value). Confidence < 0.5 means the profile is
/// provisional (short clip and/or missing data). All fields are `Option` /
/// documented-range so a consumer can rely on `None` = not computable.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioProfile {
    /// Schema version (see [`AUDIO_PROFILE_VERSION`]).
    pub version: u32,
    /// Sample rate of the analyzed audio, Hz.
    pub sample_rate: u32,
    /// Channel count of the analyzed audio.
    pub channels: u8,
    /// Seconds of audio actually analyzed.
    pub duration_secs: f32,
    /// Which sub-profiles were requested (all others are `None`).
    pub mask: AnalysisMask,
    pub loudness: LoudnessProfile,
    pub dynamics: DynamicProfile,
    pub spectral: SpectralProfile,
    pub transient: TransientProfile,
    pub stereo: StereoProfile,
    pub spatial: SpatialProfile,
    pub content: ContentProfile,
    /// Aggregate confidence, 0..1 (see struct docs for semantics).
    pub confidence: f32,
}

impl AudioProfile {
    /// Aggregate confidence: duration component × coverage component.
    pub(crate) fn compute_confidence(duration_secs: f32, coverage: f32) -> f32 {
        let duration_conf = (duration_secs / PROFILE_REQUIRED_SECS).clamp(0.0, 1.0);
        let coverage = coverage.clamp(0.0, 1.0);
        duration_conf * (0.5 + 0.5 * coverage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile() -> AudioProfile {
        AudioProfile {
            version: AUDIO_PROFILE_VERSION,
            sample_rate: 44_100,
            channels: 2,
            duration_secs: 40.0,
            mask: AnalysisMask::all(),
            loudness: LoudnessProfile {
                integrated_lufs: Some(-16.2),
                short_term_lufs: Some(-15.0),
                true_peak_dbtp: Some(-1.1),
                loudness_range_lu: Some(8.4),
                stability: Some(0.72),
            },
            dynamics: DynamicProfile {
                crest_factor_db: Some(12.3),
                dynamic_range_db: Some(8.4),
                character: DynamicCharacter::Moderate,
                compression: Some(0.53),
            },
            spectral: SpectralProfile {
                centroid_hz: Some(1850.0),
                rolloff_hz: Some(7200.0),
                slope_db_per_octave: Some(-4.8),
                flatness: Some(0.12),
                brightness_db: Some(1.4),
            },
            transient: TransientProfile {
                density_per_sec: Some(2.1),
                strength: Some(0.34),
            },
            stereo: StereoProfile {
                correlation: Some(0.55),
                width: Some(0.45),
                balance_db: Some(0.1),
                phase_risk: Some(0.02),
            },
            spatial: SpatialProfile {
                side_fraction: Some(0.31),
                ambience_db: Some(-3.5),
            },
            content: ContentProfile {
                speech: 0.05,
                music: 0.92,
                ambient: 0.03,
                masking: MaskingProfile {
                    tonal_density: Some(0.88),
                    dynamic_risk: Some(0.62),
                },
                evidence: true,
            },
            confidence: 0.99,
        }
    }

    #[test]
    fn mask_default_is_all() {
        assert_eq!(AnalysisMask::default(), AnalysisMask::all());
        let m = AnalysisMask::default();
        assert!(
            m.loudness
                && m.dynamics
                && m.spectral
                && m.transient
                && m.stereo
                && m.spatial
                && m.content
        );
    }

    #[test]
    fn profile_json_round_trip() {
        let p = sample_profile();
        let json = serde_json::to_string(&p).unwrap();
        let back: AudioProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
        // The sample has no None fields, so no JSON nulls appear (a NaN/Inf
        // would also serialize as null and fail here).
        assert!(!json.contains("null"));
    }

    #[test]
    fn profile_json_round_trip_with_all_none() {
        let p = AudioProfile {
            version: AUDIO_PROFILE_VERSION,
            sample_rate: 44_100,
            channels: 1,
            duration_secs: 3.0,
            mask: AnalysisMask {
                stereo: false,
                spatial: false,
                ..AnalysisMask::all()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: AudioProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
        assert_eq!(back.loudness.integrated_lufs, None);
    }

    #[test]
    fn confidence_is_bounded_and_increases_with_duration() {
        let short = AudioProfile::compute_confidence(5.0, 1.0);
        let long = AudioProfile::compute_confidence(40.0, 1.0);
        assert!((0.0..=1.0).contains(&short));
        assert_eq!(long, 1.0);
        assert!(long > short);

        let no_coverage = AudioProfile::compute_confidence(40.0, 0.0);
        assert_eq!(no_coverage, 0.5);
    }
}
