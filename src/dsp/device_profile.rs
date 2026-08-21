//! `DeviceProfile` — predefined signal-chain presets for common output devices.
//!
//! A device profile bundles together:
//! * An EQ preset (e.g., frequency compensation for headphones)
//! * A crossfeed preset (for headphone listening)
//! * Limiter settings tuned for the device's headroom
//! * Optional stereo width
//!
//! Profiles may be user-defined (saved as JSON) or built-in.
//! The profile system is modelled after Poweramp's "Headset / BT / Speaker"
//! audio focus categories.
//!
//! # Usage
//!
//! ```rust,ignore
//! let profile = DeviceProfile::load_or_default("wh1000xm4");
//! engine.send_command(EngineCommand::SetDeviceProfile(profile));
//! ```

use std::path::PathBuf;

/// A category of output device, used to auto-select profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DeviceCategory {
    /// Over-ear or on-ear headphones
    Headphones,
    /// In-ear monitors or earbuds
    Iems,
    /// Bluetooth speaker or TWS earbuds
    BluetoothSpeaker,
    /// Desktop or bookshelf speakers (nearfield)
    Speakers,
    /// Home theater / living room speakers
    HomeTheater,
    /// HDMI/Optical output (pass-through or AVR)
    Hdmi,
    /// Generic / unknown
    Generic,
}

impl Default for DeviceCategory {
    fn default() -> Self {
        Self::Generic
    }
}

/// Per-band EQ setting within a device profile.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProfileEqBand {
    pub frequency: f32,
    pub gain_db: f32,
    pub q: f32,
    pub enabled: bool,
}

/// A complete device profile snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeviceProfile {
    /// Unique identifier (slug) for this profile.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Device category hint for auto-selection.
    pub category: DeviceCategory,
    /// Preamp gain before EQ bands (dB). Typically negative to prevent
    /// clipping when bands are boosted.
    pub preamp_db: f32,
    /// EQ band presets (up to `MAX_EQ_BANDS` bands).
    pub eq_bands: Vec<ProfileEqBand>,
    /// Enable EQ for this profile.
    pub eq_enabled: bool,
    /// Enable crossfeed (headphone-only — reduces fatigue from hard L/R panning).
    pub crossfeed_enabled: bool,
    /// Stereo width adjustment (1.0 = normal, > 1.0 wider, < 1.0 narrower).
    pub stereo_width: f32,
    /// Whether to use the FIR true-peak limiter.
    pub true_peak_limiter: bool,
    /// Limiter ceiling in dBFS. Typically -0.3 dB.
    pub limiter_ceiling_db: f32,
}

impl Default for DeviceProfile {
    fn default() -> Self {
        Self {
            id: "flat".to_string(),
            name: "Flat (No Processing)".to_string(),
            category: DeviceCategory::Generic,
            preamp_db: 0.0,
            eq_bands: Vec::new(),
            eq_enabled: false,
            crossfeed_enabled: false,
            stereo_width: 1.0,
            true_peak_limiter: true,
            limiter_ceiling_db: -0.3,
        }
    }
}

impl DeviceProfile {
    /// Built-in "Flat" profile — no DSP, suitable for transparent monitoring.
    pub fn flat() -> Self {
        Self::default()
    }

    /// Built-in profile tuned for headphone listening.
    /// Enables gentle crossfeed for listening comfort without altering the frequency response.
    pub fn headphones() -> Self {
        Self {
            id: "headphones".to_string(),
            name: "Headphones".to_string(),
            category: DeviceCategory::Headphones,
            preamp_db: 0.0,
            eq_enabled: false,
            crossfeed_enabled: true,
            stereo_width: 1.0,
            true_peak_limiter: true,
            limiter_ceiling_db: -0.3,
            eq_bands: Vec::new(),
        }
    }

    /// Built-in profile for Bluetooth / TWS earbuds.
    /// Uses a -1.0 dBFS true-peak ceiling for lossy codec safety without coloring EQ.
    pub fn bluetooth() -> Self {
        Self {
            id: "bluetooth".to_string(),
            name: "Bluetooth / TWS".to_string(),
            category: DeviceCategory::BluetoothSpeaker,
            preamp_db: 0.0,
            eq_enabled: false,
            crossfeed_enabled: false,
            stereo_width: 1.0,
            true_peak_limiter: true,
            limiter_ceiling_db: -1.0, // Tighter ceiling for compressed BT codecs
            eq_bands: Vec::new(),
        }
    }

    /// Load a profile from a JSON file.
    pub fn from_file(path: &PathBuf) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read profile file: {}", e))?;
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse profile JSON: {}", e))
    }

    /// Serialize this profile to a JSON file.
    pub fn save_to_file(&self, path: &PathBuf) -> Result<(), String> {
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize profile: {}", e))?;
        std::fs::write(path, content).map_err(|e| format!("Failed to write profile file: {}", e))
    }

    /// Return the profile directory in the user's application data directory.
    /// Creates it if it doesn't exist.
    pub fn profile_dir() -> Option<PathBuf> {
        let base = crate::paths::data_local_dir()?;
        let dir = base.join("engine").join("profiles");
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

    /// Built-in profiles list.
    pub fn built_in() -> Vec<Self> {
        vec![Self::flat(), Self::headphones(), Self::bluetooth()]
    }
}

/// Profile library — combines built-in and user profiles.
pub struct ProfileLibrary {
    profiles: Vec<DeviceProfile>,
}

impl ProfileLibrary {
    pub fn new() -> Self {
        let mut profiles = DeviceProfile::built_in();
        profiles.extend(DeviceProfile::load_all_user_profiles());
        Self { profiles }
    }

    pub fn get(&self, id: &str) -> Option<&DeviceProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn all(&self) -> &[DeviceProfile] {
        &self.profiles
    }

    pub fn by_category(&self, cat: DeviceCategory) -> Vec<&DeviceProfile> {
        self.profiles.iter().filter(|p| p.category == cat).collect()
    }
}

impl Default for ProfileLibrary {
    fn default() -> Self {
        Self::new()
    }
}
