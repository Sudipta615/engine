//! Declarative spatial render knobs (serde) for quality, voice management,
//! and metering (spec §86, §76, §70).
//!
//! These types let a host configure the spatial engine's *render* behavior —
//! as opposed to the scene's *content* ([`super::SpatialSceneConfig`]) — from
//! serialized config (files, JSON, CLI), then hand the values to the live
//! renderers / graph node. On the engine side they map onto `spatial::quality
//! ::SpatialQuality`, `spatial::voice::VoiceBudget`, and the renderers'
//! per-speaker meters.
//!
//! Every field is `#[serde(default)]`-friendly and forward-compatible (the
//! file format rule, spec Part XXVI), so older hosts keep reading newer files.

use serde::{Deserialize, Serialize};

/// Renderer quality tier (spec §86). Mirrors `spatial::quality::SpatialQuality`
/// (`Default = Medium`, the current balanced behaviour). Tiers are
/// host-advisory: they scale how *refined* the (always-correct) render is —
/// spread samples, room reflection order, HRTF tap length — never essential
/// correctness (energy, symmetry, NaN-safety) or the real-time rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpatialQuality {
    /// Minimal spread samples, first-order room, cheapest path.
    Low,
    /// The current default behaviour (balanced).
    #[default]
    Medium,
    /// Full spread refinement, second-order room.
    High,
    /// Maximum refinement, longer HRTF convolution, higher voice budget.
    Ultra,
}

/// How voices are ranked when the voice budget is tighter than the scene
/// (spec §76). Mirrors `spatial::voice::VoicePriority`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoicePriority {
    /// Fixed scene/authored order (slot order wins).
    #[default]
    Fixed,
    /// Closer objects are prioritised (distance ascends).
    DistanceWeighted,
    /// Louder objects are prioritised.
    GainWeighted,
    /// Host-provided priority order (highest `priority()` wins).
    UserDefined,
    /// Psychoacoustic audibility weighting: (gain * importance) / distance.max(0.1).
    AdaptiveAudibility,
}

fn default_voice_capacity() -> usize {
    48
}
fn default_voice_full_quality() -> usize {
    24
}

/// The scene's voice budget (spec §76): a hard capacity plus a sub-capacity
/// for full-quality voices, ranked by [`VoicePriority`]. The engine's
/// `VoiceBudget` consumes this to build a per-block admission plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpatialVoiceConfig {
    /// Master enable. When disabled the budget is left at the engine default.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Hard per-scene capacity (concurrent voices at any quality).
    #[serde(default = "default_voice_capacity")]
    pub capacity: usize,
    /// Full-quality sub-capacity (voices beyond this are degraded).
    #[serde(default = "default_voice_full_quality")]
    pub full_quality_capacity: usize,
    /// How to rank candidates for admission.
    #[serde(default)]
    pub policy: VoicePriority,
}

impl Default for SpatialVoiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capacity: default_voice_capacity(),
            full_quality_capacity: default_voice_full_quality(),
            policy: VoicePriority::default(),
        }
    }
}

/// Output metering (spec §70): per-speaker/bus/LFE peak + RMS. The renderers
/// accumulate meters on the audio thread (allocation-free) and a host reads
/// the snapshot on the control thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpatialMeterConfig {
    /// Master enable. Disabled = the meter accumulators stay dormant (zero
    /// cost and zero reported levels).
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for SpatialMeterConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Operating mode for the spatial bass engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpatialBassMode {
    /// Pure bit-perfect pass-through of LFE and bass channels.
    #[default]
    Pure,
    /// Bass-managed: direct steering to subwoofer according to crossover.
    BassManaged,
    /// Bass immersion: dynamic low-shelf + psychoacoustic harmonic reinforcement.
    BassImmersion,
}

fn default_immersion_amount() -> f32 {
    0.5
}

fn default_bass_crossover_hz() -> f32 {
    80.0
}

/// Configuration for the spatial bass engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpatialBassConfig {
    /// Operating mode.
    #[serde(default)]
    pub mode: SpatialBassMode,
    /// Crossover frequency in Hz (60.0 .. 120.0 Hz, default 80.0 Hz).
    #[serde(default = "default_bass_crossover_hz")]
    pub crossover_hz: f32,
    /// Crossover slope (12, 24, or 48 dB/oct).
    #[serde(default)]
    pub slope: super::CrossoverSlope,
    /// Crossover filter topology (Linkwitz-Riley or Butterworth).
    #[serde(default)]
    pub filter_type: super::CrossoverFilterType,
    /// Subwoofer delay compensation in milliseconds (0.0 .. 50.0 ms).
    #[serde(default)]
    pub sub_delay_ms: f32,
    /// Subwoofer phase alignment in degrees (0.0 .. 180.0°).
    #[serde(default)]
    pub sub_phase_degrees: f32,
    /// Subwoofer polarity inversion.
    #[serde(default)]
    pub sub_polarity_invert: bool,
    /// Psychoacoustic harmonic reinforcement amount [0.0, 1.0] in BassImmersion mode.
    #[serde(default = "default_immersion_amount")]
    pub harmonic_amount: f32,
    /// Dynamic low-shelf boost [0.0, 12.0] dB in BassImmersion mode.
    #[serde(default)]
    pub dynamic_boost_db: f32,
}

impl Default for SpatialBassConfig {
    fn default() -> Self {
        Self {
            mode: SpatialBassMode::default(),
            crossover_hz: default_bass_crossover_hz(),
            slope: super::CrossoverSlope::default(),
            filter_type: super::CrossoverFilterType::default(),
            sub_delay_ms: 0.0,
            sub_phase_degrees: 0.0,
            sub_polarity_invert: false,
            harmonic_amount: default_immersion_amount(),
            dynamic_boost_db: 0.0,
        }
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_serde_round_trips_and_is_totally_ordered() {
        for q in [
            SpatialQuality::Low,
            SpatialQuality::Medium,
            SpatialQuality::High,
            SpatialQuality::Ultra,
        ] {
            let mut v = serde_json::to_value(q).unwrap();
            let back: SpatialQuality = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(q, back);
            v = serde_json::to_value(q).unwrap();
            assert!(v.is_string());
        }
        assert!(SpatialQuality::Low < SpatialQuality::Medium);
        assert!(SpatialQuality::Medium < SpatialQuality::High);
        assert!(SpatialQuality::High < SpatialQuality::Ultra);
        assert_eq!(SpatialQuality::default(), SpatialQuality::Medium);
    }

    #[test]
    fn voice_meter_serialize_deserialize() {
        let v = SpatialVoiceConfig {
            capacity: 32,
            full_quality_capacity: 10,
            policy: VoicePriority::DistanceWeighted,
            ..Default::default()
        };
        let j = serde_json::to_string(&v).unwrap();
        let back: SpatialVoiceConfig = serde_json::from_str(&j).unwrap();
        assert_eq!(v, back);

        // Unspecified fields default (forward/backward compatible).
        let partial = r#"{}"#;
        let d: SpatialMeterConfig = serde_json::from_str(partial).unwrap();
        assert!(d.enabled);
        let dv: SpatialVoiceConfig = serde_json::from_str(partial).unwrap();
        assert_eq!(dv.capacity, 48);
        assert_eq!(dv.policy, VoicePriority::Fixed);
    }
}
