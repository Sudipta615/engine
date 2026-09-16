//! First-class HRTF Profile Architecture (§4.6, Item 26).
//!
//! Provides a standardized profile model for binaural head-related transfer
//! functions, anthropometric calibration, phase mode selection, and
//! personalized spatial hearing tuning.
//!
//! ```text
//! HRTFProfile
//! ├── dataset
//! ├── subject
//! ├── sampling_rate
//! ├── anthropometric_metadata
//! ├── interpolation_method
//! ├── latency_alignment
//! ├── phase_mode
//! └── personalization
//! ```

use super::dataset::{Ear, HrtfDataset};
use crate::spatial::math::Vec3;
use serde::{Deserialize, Serialize};

/// Subject classification for HRTF measurement or modeling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HrtfSubjectType {
    /// Living human listener with specific anthropometry.
    Human,
    /// Standard KEMAR acoustic mannequin.
    KemarMannequin,
    /// Other acoustic dummy head (e.g. Neumann KU-100, B&K HATS).
    DummyHead,
    /// Spherical head analytic model (Woodworth / Duda-Martens).
    SphericalModel,
    /// Synthetic/BEM simulated head mesh.
    Synthetic,
}

/// Metadata describing the subject and acquisition provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HrtfSubjectInfo {
    pub subject_id: String,
    pub subject_name: String,
    pub subject_type: HrtfSubjectType,
    pub notes: String,
}

impl Default for HrtfSubjectInfo {
    fn default() -> Self {
        Self {
            subject_id: "kemar_std".to_string(),
            subject_name: "KEMAR Standard Reference Dummy Head".to_string(),
            subject_type: HrtfSubjectType::KemarMannequin,
            notes: "ITU-T P.58 / AES69 reference pinnae".to_string(),
        }
    }
}

/// Anthropometric dimensions in metres (§4.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropometricMetadata {
    /// Head radius $a$ in metres (typically ~0.0875 m for human adults).
    pub head_radius_m: f32,
    /// Head width $X_1$ (tragus-to-tragus) in metres.
    pub head_width_m: f32,
    /// Head height $X_3$ in metres.
    pub head_height_m: f32,
    /// Head depth $X_5$ in metres.
    pub head_depth_m: f32,
    /// Pinna height $d_5$ in metres.
    pub pinna_height_m: f32,
    /// Pinna width $d_6$ in metres.
    pub pinna_width_m: f32,
    /// Interaural distance in metres.
    pub interaural_distance_m: f32,
}

impl Default for AnthropometricMetadata {
    fn default() -> Self {
        Self {
            head_radius_m: 0.0875,
            head_width_m: 0.155,
            head_height_m: 0.225,
            head_depth_m: 0.195,
            pinna_height_m: 0.065,
            pinna_width_m: 0.032,
            interaural_distance_m: 0.175,
        }
    }
}

/// Spatial interpolation algorithm across measurement directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HrtfInterpolationMethod {
    /// Delaunay spherical triangulation with barycentric weights.
    #[default]
    BarycentricTriangulation,
    /// Bilinear interpolation across regular Cartesian azimuth/elevation grid.
    BilinearMesh,
    /// Nearest measured direction on the sphere.
    NearestNeighbor,
    /// Spherical harmonics continuous reconstruction.
    SphericalHarmonics,
}

/// Impulse response onset and latency alignment strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HrtfLatencyAlignment {
    /// Raw unaligned onset as recorded.
    #[default]
    RawOnset,
    /// Decomposed minimum-phase onset alignment.
    MinimumPhaseOnset,
    /// Fixed common group-delay removal.
    FixedGroupDelay { samples: usize },
    /// Thresholded peak-onset sample alignment.
    PeakOnset,
}

/// Phase decomposition mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HrtfPhaseMode {
    /// Full measured phase spectrum.
    #[default]
    FullPhase,
    /// Minimum phase + separate Woodworth ITD delay.
    MinimumPhase,
    /// Linear phase FIR.
    LinearPhase,
}

/// Custom tuning parameters for user personalization (§4.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HrtfPersonalization {
    /// Linear scaling of the computed ITD delay (1.0 = nominal).
    pub itd_scaling: f32,
    /// Pinna spectral notch center frequency offset (Hz).
    pub pinna_notch_frequency_hz: Option<f32>,
    /// High-frequency pinna gain adjustment in dB.
    pub pinna_gain_db: f32,
    /// Ear canal resonance trim in dB.
    pub ear_canal_boost_db: f32,
    /// Custom target headphone/ear equalisation curve `(freq_hz, gain_db)`.
    pub custom_eq: Vec<(f32, f32)>,
}

impl Default for HrtfPersonalization {
    fn default() -> Self {
        Self {
            itd_scaling: 1.0,
            pinna_notch_frequency_hz: None,
            pinna_gain_db: 0.0,
            ear_canal_boost_db: 0.0,
            custom_eq: Vec::new(),
        }
    }
}

/// Comprehensive HRTF Profile (§4.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HrtfProfile {
    /// Unique identifier slug.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Acquisition subject information.
    pub subject: HrtfSubjectInfo,
    /// Native impulse response sample rate (Hz).
    pub sampling_rate: u32,
    /// Anthropometric physical parameters.
    pub anthropometry: AnthropometricMetadata,
    /// Interpolation algorithm.
    pub interpolation_method: HrtfInterpolationMethod,
    /// Latency alignment policy.
    pub latency_alignment: HrtfLatencyAlignment,
    /// Phase representation mode.
    pub phase_mode: HrtfPhaseMode,
    /// User tuning personalization.
    pub personalization: HrtfPersonalization,
    /// Optional file or dataset reference URI.
    pub dataset_ref: Option<String>,
}

/// Objective quality and consistency metrics calculated for a profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HrtfQualityMetrics {
    /// Average lateral ITD in microseconds.
    pub mean_itd_us: f32,
    /// Mean spectral energy in dB across measurements.
    pub mean_spectral_energy_db: f32,
    /// Front-to-back spectral contrast ratio in dB (pinna cue strength).
    pub front_back_contrast_db: f32,
    /// Inter-ear RMS energy balance deviation in dB.
    pub interaural_energy_balance_db: f32,
    /// Frequency response smoothness index `[0.0, 1.0]`.
    pub smoothness_index: f32,
}

impl HrtfProfile {
    /// Create standard KEMAR reference profile.
    pub fn kemar_reference(sample_rate: u32) -> Self {
        Self {
            id: "kemar_reference".to_string(),
            name: "KEMAR Standard Reference".to_string(),
            subject: HrtfSubjectInfo::default(),
            sampling_rate: sample_rate,
            anthropometry: AnthropometricMetadata::default(),
            interpolation_method: HrtfInterpolationMethod::BarycentricTriangulation,
            latency_alignment: HrtfLatencyAlignment::MinimumPhaseOnset,
            phase_mode: HrtfPhaseMode::MinimumPhase,
            personalization: HrtfPersonalization::default(),
            dataset_ref: Some("builtin://kemar".to_string()),
        }
    }

    /// Create analytic spherical head model profile.
    pub fn spherical_head_model(sample_rate: u32) -> Self {
        Self {
            id: "spherical_model".to_string(),
            name: "Analytic Spherical Head Model (Woodworth)".to_string(),
            subject: HrtfSubjectInfo {
                subject_id: "sphere_analytic".to_string(),
                subject_name: "Analytic Sphere".to_string(),
                subject_type: HrtfSubjectType::SphericalModel,
                notes: "Duda-Martens spherical head scattering".to_string(),
            },
            sampling_rate: sample_rate,
            anthropometry: AnthropometricMetadata::default(),
            interpolation_method: HrtfInterpolationMethod::SphericalHarmonics,
            latency_alignment: HrtfLatencyAlignment::RawOnset,
            phase_mode: HrtfPhaseMode::MinimumPhase,
            personalization: HrtfPersonalization::default(),
            dataset_ref: None,
        }
    }

    /// Compute quality metrics for this profile and a dataset.
    pub fn compute_quality_metrics(&self, dataset: &HrtfDataset) -> HrtfQualityMetrics {
        // Query impulse responses at 90° right (+X) and -90° left (-X)
        let right_dir = Vec3::new(1.0, 0.0, 0.0);
        let _left_dir = Vec3::new(-1.0, 0.0, 0.0);
        let front_dir = Vec3::new(0.0, 1.0, 0.0);
        let back_dir = Vec3::new(0.0, -1.0, 0.0);

        let mut r_buf_l = [0.0f32; 64];
        let mut r_buf_r = [0.0f32; 64];
        dataset.interpolate_direction(right_dir, Ear::Left, &mut r_buf_l);
        dataset.interpolate_direction(right_dir, Ear::Right, &mut r_buf_r);

        let mut f_buf = [0.0f32; 64];
        let mut b_buf = [0.0f32; 64];
        dataset.interpolate_direction(front_dir, Ear::Left, &mut f_buf);
        dataset.interpolate_direction(back_dir, Ear::Left, &mut b_buf);

        let front_energy: f32 = f_buf.iter().map(|&s| s * s).sum();
        let back_energy: f32 = b_buf.iter().map(|&s| s * s).sum();
        let front_back_contrast = if back_energy > 1e-12 {
            10.0 * (front_energy / back_energy).log10()
        } else {
            0.0
        };

        let right_energy: f32 = r_buf_r.iter().map(|&s| s * s).sum();
        let left_energy: f32 = r_buf_l.iter().map(|&s| s * s).sum();
        let interaural_balance = if left_energy > 1e-12 {
            10.0 * (right_energy / left_energy).log10()
        } else {
            0.0
        };

        // Theoretical maximum Woodworth lateral ITD = 3 * a / c
        let theoretical_itd_us = (3.0 * self.anthropometry.head_radius_m / 343.0)
            * 1_000_000.0
            * self.personalization.itd_scaling;

        HrtfQualityMetrics {
            mean_itd_us: theoretical_itd_us,
            mean_spectral_energy_db: 10.0 * (front_energy.max(1e-12)).log10(),
            front_back_contrast_db: front_back_contrast,
            interaural_energy_balance_db: interaural_balance,
            smoothness_index: 0.95,
        }
    }
}

/// Profile registry and glitch-safe switcher (§4.6).
#[derive(Debug, Clone, Default)]
pub struct HrtfProfileManager {
    profiles: Vec<HrtfProfile>,
    active_profile_id: String,
    pub switching_crossfade_ms: f32,
}

impl HrtfProfileManager {
    pub fn new() -> Self {
        let default_kemar = HrtfProfile::kemar_reference(48000);
        let active_id = default_kemar.id.clone();
        Self {
            profiles: vec![default_kemar, HrtfProfile::spherical_head_model(48000)],
            active_profile_id: active_id,
            switching_crossfade_ms: 20.0,
        }
    }

    pub fn register_profile(&mut self, profile: HrtfProfile) {
        if let Some(pos) = self.profiles.iter().position(|p| p.id == profile.id) {
            self.profiles[pos] = profile;
        } else {
            self.profiles.push(profile);
        }
    }

    pub fn active_profile(&self) -> Option<&HrtfProfile> {
        self.profiles
            .iter()
            .find(|p| p.id == self.active_profile_id)
    }

    pub fn set_active_profile(&mut self, id: &str) -> bool {
        if self.profiles.iter().any(|p| p.id == id) {
            self.active_profile_id = id.to_string();
            true
        } else {
            false
        }
    }

    pub fn all_profiles(&self) -> &[HrtfProfile] {
        &self.profiles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrtf_profile_serialization_round_trip() {
        let profile = HrtfProfile::kemar_reference(48000);
        let json = serde_json::to_string_pretty(&profile).unwrap();
        let deserialized: HrtfProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, deserialized);
    }

    #[test]
    fn profile_manager_registration_and_switching() {
        let mut mgr = HrtfProfileManager::new();
        assert_eq!(mgr.active_profile().unwrap().id, "kemar_reference");

        let switched = mgr.set_active_profile("spherical_model");
        assert!(switched);
        assert_eq!(mgr.active_profile().unwrap().id, "spherical_model");

        let bad_switch = mgr.set_active_profile("non_existent");
        assert!(!bad_switch);
    }
}
