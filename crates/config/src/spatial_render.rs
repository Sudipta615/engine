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
