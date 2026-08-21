//! Per-output / per-device profiles (§10).
//!
//! An [`OutputProfile`] bundles **device matching** (case-insensitive name
//! patterns), **transport preferences** (backend, sample rate, DSD policy,
//! volume mode), and the **DSP bundle** ([`DeviceProfile`]: EQ bands,
//! crossfeed, stereo width, limiter). It is the per-output/per-device
//! counterpart of the DSP-only `DeviceProfile`: the same headphone EQ can be
//! applied automatically whenever a matching DAC is the active output.
//!
//! Selection is deterministic: [`OutputProfileLibrary::select_for_device`]
//! scores every profile against the device name, prefers the highest
//! confidence match (exact > case-insensitive > substring), and breaks ties
//! by insertion order — the same device always yields the same profile.

use crate::dsp::device_profile::{DeviceCategory, DeviceProfile};
use crate::output::device_match::{classify_device_name_match, DeviceNameMatch};
use config::{
    AudioBackend, ChannelRoutingConfig, DitherPolicy, DsdOutput, ResamplerQuality, VolumeMode,
};

/// A complete per-output profile.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OutputProfile {
    /// Unique identifier (slug).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Device category hint for auto-selection fallback.
    pub category: DeviceCategory,
    /// Case-insensitive substring patterns matched against the output
    /// device's reported name. An empty list matches every device (a
    /// category/default profile).
    #[serde(default)]
    pub device_match: Vec<String>,
    /// Stable OS/backend device ID this profile targets (e.g. the WASAPI
    /// endpoint ID `{0.0.0.00000000}.{guid}` or an ALSA `hw:` string). An
    /// exact device-ID match is the strongest device identity and beats
    /// every name pattern (friendly names collide across vendors and change
    /// with the OS language).
    #[serde(default)]
    pub device_id: Option<String>,
    /// Preferred backend. Applied when the output stream is (re)created.
    #[serde(default)]
    pub backend_preference: Option<AudioBackend>,
    /// Preferred sample rate in Hz (maps to `SampleRatePolicy::Fixed`).
    #[serde(default)]
    pub sample_rate_preference: Option<u32>,
    /// Preferred resampler quality for this device.
    #[serde(default)]
    pub resampler_policy: Option<ResamplerQuality>,
    /// Dither policy for this device (follows the engine-wide setting by
    /// default; `ForceOn` / `ForceOff` override it).
    #[serde(default)]
    pub dither_policy: Option<DitherPolicy>,
    /// Preferred output channel count (e.g. 2 for a stereo DAC, 6 for a
    /// 5.1 receiver). Consumed by the channel-routing layer when the stream
    /// is created; `None` = device default.
    #[serde(default)]
    pub channel_preference: Option<u16>,
    /// Safety limiter ceiling in dBTP, overriding the DSP bundle's ceiling
    /// when set (spec §10: "safety ceiling").
    #[serde(default)]
    pub safety_ceiling_dbtp: Option<f32>,
    /// Source→destination channel-routing matrix for this device (spec §10
    /// "channel routing", §34). Applied to the multichannel routing stage
    /// when the profile is active; `None` keeps the engine-wide routing.
    #[serde(default)]
    pub channel_routing: Option<ChannelRoutingConfig>,
    /// Preferred DSD transport policy for DSD tracks.
    #[serde(default)]
    pub dsd_policy: Option<DsdOutput>,
    /// Preferred volume mode.
    #[serde(default)]
    pub volume_mode: Option<VolumeMode>,
    /// The DSP bundle applied when this profile is active.
    #[serde(default)]
    pub dsp: DeviceProfile,
}

impl Default for OutputProfile {
    fn default() -> Self {
        Self {
            id: "flat".to_string(),
            name: "Flat (No Processing)".to_string(),
            category: DeviceCategory::Generic,
            device_match: Vec::new(),
            device_id: None,
            backend_preference: None,
            sample_rate_preference: None,
            resampler_policy: None,
            dither_policy: None,
            channel_preference: None,
            safety_ceiling_dbtp: None,
            channel_routing: None,
            dsd_policy: None,
            volume_mode: None,
            dsp: DeviceProfile::flat(),
        }
    }
}

impl OutputProfile {
    /// Best confidence of this profile's patterns against `device_name`.
    pub fn matches_device(&self, device_name: &str) -> Option<DeviceNameMatch> {
        let mut best: Option<DeviceNameMatch> = None;
        for pattern in &self.device_match {
            if let Some(m) = classify_device_name_match(pattern, device_name) {
                best = Some(match best {
                    // `Ord` orders Exact < ExactCaseInsensitive < Substring,
                    // so the minimum is the strongest match.
                    Some(b) => b.min(m),
                    None => m,
                });
            }
        }
        best
    }

    /// Built-in "Flat" output profile — no processing, matches any device.
    pub fn flat() -> Self {
        Self::default()
    }

    /// Built-in headphone profile — gentle crossfeed, no EQ.
    pub fn headphones() -> Self {
        Self {
            id: "headphones".to_string(),
            name: "Headphones".to_string(),
            category: DeviceCategory::Headphones,
            device_match: vec![
                "headphone".to_string(),
                "earphone".to_string(),
                "headset".to_string(),
            ],
            ..Self::default()
        }
        .with_dsp(DeviceProfile::headphones())
    }

    /// Built-in Bluetooth profile — tighter limiter ceiling for lossy
    /// codec safety.
    pub fn bluetooth() -> Self {
        Self {
            id: "bluetooth".to_string(),
            name: "Bluetooth / TWS".to_string(),
            category: DeviceCategory::BluetoothSpeaker,
            device_match: vec!["bluetooth".to_string(), "bt".to_string()],
            ..Self::default()
        }
        .with_dsp(DeviceProfile::bluetooth())
    }

    /// Replace the DSP bundle (helper for built-ins).
    fn with_dsp(mut self, dsp: DeviceProfile) -> Self {
        self.dsp = dsp;
        self
    }

    /// Built-in output profiles.
    pub fn built_in() -> Vec<Self> {
        vec![Self::flat(), Self::headphones(), Self::bluetooth()]
    }

    /// Load a profile from a JSON file.
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read profile file: {e}"))?;
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse profile JSON: {e}"))
    }

    /// Serialize this profile to a JSON file.
    pub fn save_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize profile: {e}"))?;
        std::fs::write(path, content).map_err(|e| format!("Failed to write profile file: {e}"))
    }

    /// Directory for user-defined output profiles; created on demand.
    pub fn profile_dir() -> Option<std::path::PathBuf> {
        let base = crate::paths::data_local_dir()?;
        let dir = base.join("engine").join("output_profiles");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    /// Load all user-defined profiles from the profile directory.
    pub fn load_all_user_profiles() -> Vec<Self> {
        let Some(dir) = Self::profile_dir() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .filter_map(|e| Self::from_file(&e.path()).ok())
            .collect()
    }
}

/// Profile library — built-in + user profiles with deterministic
/// per-device selection.
pub struct OutputProfileLibrary {
    profiles: Vec<OutputProfile>,
}

impl OutputProfileLibrary {
    /// Library of built-in profiles plus user-defined JSON profiles.
    pub fn new() -> Self {
        let mut profiles = OutputProfile::built_in();
        profiles.extend(OutputProfile::load_all_user_profiles());
        Self { profiles }
    }

    /// Library from an explicit list (used by tests and by the engine when a
    /// caller supplies its own set).
    pub fn with_profiles(profiles: Vec<OutputProfile>) -> Self {
        Self { profiles }
    }

    pub fn get(&self, id: &str) -> Option<&OutputProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn all(&self) -> &[OutputProfile] {
        &self.profiles
    }

    /// Deterministically select the best profile for `device_name`.
    ///
    /// Scores every profile with `device_match` patterns against the name
    /// (exact > case-insensitive > substring) and returns the strongest
    /// match; ties break by insertion order, so selection is stable. A
    /// profile with no patterns matches any device (lowest priority — it is
    /// only chosen when nothing more specific matches). Name-only selection;
    /// see [`Self::select_for_device_with_id`] for stable-ID matching.
    pub fn select_for_device(&self, device_name: &str) -> Option<&OutputProfile> {
        self.select_for_device_with_id(device_name, None)
    }

    /// Deterministically select the best profile for a device, using the
    /// stable OS/backend device ID when available.
    ///
    /// An exact `device_id` match is the strongest possible identity and
    /// beats *every* name-pattern match (§10: "Profile matching must not
    /// depend solely on a human-readable device name"). When no profile
    /// matches the ID, name matching proceeds as in [`Self::select_for_device`].
    pub fn select_for_device_with_id(
        &self,
        device_name: &str,
        device_id: Option<&str>,
    ) -> Option<&OutputProfile> {
        // Pass 1: exact stable-ID match (if an ID is known at all).
        if let Some(id) = device_id {
            let id_match = self
                .profiles
                .iter()
                .find(|p| p.device_id.as_deref() == Some(id));
            if let Some(profile) = id_match {
                return Some(profile);
            }
        }
        // Pass 2: name-pattern scoring, as before.
        let mut best: Option<(&OutputProfile, DeviceNameMatch, usize)> = None;
        for (index, profile) in self.profiles.iter().enumerate() {
            if let Some(m) = profile.matches_device(device_name) {
                match best {
                    None => best = Some((profile, m, index)),
                    Some((_, best_match, best_index)) => {
                        let is_stronger = m < best_match;
                        let is_equal_earlier = m == best_match && index < best_index;
                        if is_stronger || is_equal_earlier {
                            best = Some((profile, m, index));
                        }
                    }
                }
            }
        }
        best.map(|(profile, _, _)| profile)
    }
}

impl Default for OutputProfileLibrary {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, patterns: Vec<&str>) -> OutputProfile {
        OutputProfile {
            id: id.to_string(),
            name: id.to_string(),
            device_match: patterns.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn matches_device_ranks_exact_over_substring() {
        let p = profile("dac", vec!["USB DAC"]);
        assert_eq!(p.matches_device("USB DAC"), Some(DeviceNameMatch::Exact));
        assert_eq!(
            p.matches_device("USB DAC Pro"),
            Some(DeviceNameMatch::Substring)
        );
        assert_eq!(p.matches_device("Speakers"), None);
        // Multi-pattern: the strongest pattern wins.
        let multi = profile("m", vec!["USB DAC Pro", "USB DAC"]);
        assert_eq!(
            multi.matches_device("USB DAC"),
            Some(DeviceNameMatch::Exact)
        );
        // Empty patterns match nothing (a default/category profile).
        let none = profile("none", vec![]);
        assert_eq!(none.matches_device("anything"), None);
    }

    #[test]
    fn library_selection_prefers_stable_device_id_over_name() {
        // A profile pinned to a stable endpoint ID must win even when a
        // name-pattern profile would otherwise match exactly.
        let mut id_pinned = profile("id-pinned", vec![]);
        id_pinned.device_id = Some("{0.0.0.00000000}.{guid}".to_string());
        let lib = OutputProfileLibrary::with_profiles(vec![
            profile("generic-usb", vec!["usb"]),
            profile("dac-exact", vec!["USB DAC"]),
            id_pinned,
        ]);

        // With the stable ID: the ID match beats the exact name match.
        assert_eq!(
            lib.select_for_device_with_id("USB DAC", Some("{0.0.0.00000000}.{guid}"))
                .unwrap()
                .id,
            "id-pinned"
        );
        // Without the ID (e.g. cpal backend): the name match applies.
        assert_eq!(
            lib.select_for_device_with_id("USB DAC", None).unwrap().id,
            "dac-exact"
        );
        // An unknown ID falls back to name matching.
        assert_eq!(
            lib.select_for_device_with_id("USB DAC", Some("{other}"))
                .unwrap()
                .id,
            "dac-exact"
        );
        // Back-compat: the name-only selector delegates with no ID.
        assert_eq!(lib.select_for_device("USB DAC").unwrap().id, "dac-exact");
    }

    #[test]
    fn library_selection_is_deterministic_and_prefers_exact() {
        let lib = OutputProfileLibrary::with_profiles(vec![
            profile("generic-usb", vec!["usb"]),
            profile("dac-exact", vec!["USB DAC"]),
            profile("dac-pro", vec!["DAC Pro"]),
        ]);

        // Same device, two selections → identical result.
        let a = lib.select_for_device("USB DAC");
        let b = lib.select_for_device("USB DAC");
        assert_eq!(a.map(|p| p.id.as_str()), b.map(|p| p.id.as_str()));
        // Exact beats substring: "USB DAC" matches dac-exact exactly and
        // generic-usb by substring → dac-exact wins.
        assert_eq!(lib.select_for_device("USB DAC").unwrap().id, "dac-exact");
        // "USB DAC Pro" matches all three by substring; the tie breaks by
        // insertion order deterministically (first profile wins).
        assert_eq!(
            lib.select_for_device("USB DAC Pro").unwrap().id,
            "generic-usb"
        );
        let again = lib.select_for_device("USB DAC Pro").unwrap().id.clone();
        assert_eq!(lib.select_for_device("USB DAC Pro").unwrap().id, again);
        // No match at all → None (no accidental default).
        assert!(lib.select_for_device("Speakers").is_none());
    }

    #[test]
    fn builtin_profiles_have_expected_dsp_bundles() {
        let flat = OutputProfile::flat();
        assert!(!flat.dsp.eq_enabled);
        assert_eq!(flat.dsp.limiter_ceiling_db, -0.3);

        let hp = OutputProfile::headphones();
        assert!(hp.dsp.crossfeed_enabled);

        let bt = OutputProfile::bluetooth();
        assert!((bt.dsp.limiter_ceiling_db - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn json_round_trip_preserves_profile() {
        let mut p = profile("dac", vec!["USB DAC"]);
        p.device_id = Some("{0.0.0.00000000}.{guid}".to_string());
        p.backend_preference = Some(AudioBackend::ExclusiveAlsa);
        p.sample_rate_preference = Some(192_000);
        p.resampler_policy = Some(ResamplerQuality::Ultra);
        p.dither_policy = Some(DitherPolicy::ForceOff);
        p.channel_preference = Some(6);
        p.safety_ceiling_dbtp = Some(-1.0);
        p.channel_routing = Some(ChannelRoutingConfig {
            enabled: true,
            matrix: vec![
                vec![1.0, 0.0, 0.0],
                vec![0.0, 1.0, 0.0],
                vec![0.0, 0.0, 1.0],
            ],
        });
        p.dsd_policy = Some(DsdOutput::DoP);
        p.volume_mode = Some(VolumeMode::HardwarePreferred);

        let json = serde_json::to_string(&p).unwrap();
        let back: OutputProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, p.id);
        assert_eq!(back.device_match, p.device_match);
        assert_eq!(back.device_id, p.device_id);
        assert_eq!(back.backend_preference, p.backend_preference);
        assert_eq!(back.sample_rate_preference, p.sample_rate_preference);
        assert_eq!(back.resampler_policy, p.resampler_policy);
        assert_eq!(back.dither_policy, p.dither_policy);
        assert_eq!(back.channel_preference, p.channel_preference);
        assert_eq!(back.safety_ceiling_dbtp, p.safety_ceiling_dbtp);
        assert_eq!(back.channel_routing, p.channel_routing);
        assert_eq!(back.dsd_policy, p.dsd_policy);
        assert_eq!(back.volume_mode, p.volume_mode);
    }
}
