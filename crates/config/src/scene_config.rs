//! Spatial scene file format (spec Part XXVI).
//!
//! A scene file is a Serde-serializable description of a spatial scene —
//! the listener, the three content classes (objects, beds, fields) and the
//! room — independent of any output speaker layout and of the renderer (the
//! renderer is a host/render-time choice, matching the engine architecture:
//! "decoder selection is separate from the scene representation"). Hosts
//! save scenes with the engine's `save_scene_json` and load them with
//! `load_scene_json`; the conversion between this model and the engine's
//! live [`SpatialScene`] happens on the engine side (`SpatialScene::
//! from_config` / `to_config`).
//!
//! The file format is versioned only by the crate version; fields are
//! forward-compatible (`#[serde(default)]` on every optional field), so an
//! older host reading a newer file keeps working.

use serde::{Deserialize, Serialize};

use super::SpatialRoomConfig;

/// Default scene sample rate (48 kHz — the engine's nominal rate).
fn default_scene_rate() -> u32 {
    48_000
}

/// The listener: position (metres, `[x, y, z]`) and orientation as a unit
/// quaternion `[x, y, z, w]` (the engine's canonical orientation; identity
/// = facing `+Y`). The quaternion is lossless — no Euler decomposition is
/// involved in a scene round-trip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneListenerConfig {
    #[serde(default)]
    pub position: [f32; 3],
    #[serde(default = "default_orientation")]
    pub orientation: [f32; 4],
}

fn default_orientation() -> [f32; 4] {
    [0.0, 0.0, 0.0, 1.0]
}

impl Default for SceneListenerConfig {
    fn default() -> Self {
        Self {
            position: [0.0, 0.0, 0.0],
            orientation: default_orientation(),
        }
    }
}

/// A point/extended source (spec §13.2): position in metres, gain, spread,
/// and its sends into the room / LFE paths.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpatialObjectConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub position: [f32; 3],
    #[serde(default = "default_gain")]
    pub gain: f32,
    /// Normalized angular spread in [0, 1] (0 = point source).
    #[serde(default)]
    pub spread: f32,
    /// Room reflection send in [0, 1].
    #[serde(default)]
    pub room_send: f32,
    /// LFE effects send in [0, 1].
    #[serde(default)]
    pub lfe_send: f32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_gain() -> f32 {
    1.0
}
fn default_enabled() -> bool {
    true
}

impl Default for SpatialObjectConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            position: [0.0, 2.0, 0.0],
            gain: 1.0,
            spread: 0.0,
            room_send: 0.0,
            lfe_send: 0.0,
            enabled: true,
        }
    }
}

/// A channel-based bed (spec §13.1): its authored channel roles (semantic
/// names, e.g. `["FL", "FR", "C", "LFE", "SL", "SR"]`) plus gain. The
/// renderer routes each channel by its role.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpatialBedConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_bed_channels")]
    pub channels: Vec<String>,
    #[serde(default = "default_gain")]
    pub gain: f32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_bed_channels() -> Vec<String> {
    vec!["FL".to_string(), "FR".to_string()]
}

impl Default for SpatialBedConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            channels: default_bed_channels(),
            gain: 1.0,
            enabled: true,
        }
    }
}

/// A diffuse field (spec §13.3): rain, wind, ambience — positionless,
/// decoded to surrounding ambience.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpatialFieldConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_gain")]
    pub gain: f32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl Default for SpatialFieldConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            gain: 1.0,
            enabled: true,
        }
    }
}

/// A full spatial scene (spec §13, §16–18): listener + objects + beds +
/// fields + room. Content only — the renderer and output layout are host
/// choices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpatialSceneConfig {
    #[serde(default = "default_scene_rate")]
    pub sample_rate: u32,
    #[serde(default)]
    pub listener: SceneListenerConfig,
    #[serde(default)]
    pub objects: Vec<SpatialObjectConfig>,
    #[serde(default)]
    pub beds: Vec<SpatialBedConfig>,
    #[serde(default)]
    pub fields: Vec<SpatialFieldConfig>,
    #[serde(default)]
    pub room: SpatialRoomConfig,
}

impl Default for SpatialSceneConfig {
    fn default() -> Self {
        Self {
            sample_rate: default_scene_rate(),
            listener: SceneListenerConfig::default(),
            objects: Vec::new(),
            beds: Vec::new(),
            fields: Vec::new(),
            room: SpatialRoomConfig::default(),
        }
    }
}

impl SpatialSceneConfig {
    /// Validate the scene for conversion into an engine scene. Bounds mirror
    /// the engine's hard caps (see the `MAX_*` constants in the spatial
    /// stores); role names must be known (`ChannelId::from_name`).
    pub fn validate(&self) -> Result<(), String> {
        if !(8_000..=768_000).contains(&self.sample_rate) {
            return Err(format!("invalid sample_rate {}", self.sample_rate));
        }
        if self.objects.len() > 64 {
            return Err(format!("too many objects ({})", self.objects.len()));
        }
        if self.beds.len() > 8 {
            return Err(format!("too many beds ({})", self.beds.len()));
        }
        if self.fields.len() > 16 {
            return Err(format!("too many fields ({})", self.fields.len()));
        }
        for (i, o) in self.objects.iter().enumerate() {
            if o.position.iter().any(|v| !v.is_finite()) {
                return Err(format!("object {i}: non-finite position"));
            }
            if !o.gain.is_finite() || !(0.0..=4.0).contains(&o.gain) {
                return Err(format!("object {i}: invalid gain {}", o.gain));
            }
            if !o.spread.is_finite() || !(0.0..=1.0).contains(&o.spread) {
                return Err(format!("object {i}: invalid spread {}", o.spread));
            }
            if !o.room_send.is_finite() || !(0.0..=1.0).contains(&o.room_send) {
                return Err(format!("object {i}: invalid room_send {}", o.room_send));
            }
            if !o.lfe_send.is_finite() || !(0.0..=1.0).contains(&o.lfe_send) {
                return Err(format!("object {i}: invalid lfe_send {}", o.lfe_send));
            }
        }
        for (i, b) in self.beds.iter().enumerate() {
            if b.channels.is_empty() || b.channels.len() > 16 {
                return Err(format!("bed {i}: channels out of range"));
            }
            for (c, name) in b.channels.iter().enumerate() {
                if !is_valid_role(name) {
                    return Err(format!("bed {i}: unknown channel role '{name}' at {c}"));
                }
            }
            if !b.gain.is_finite() || !(0.0..=4.0).contains(&b.gain) {
                return Err(format!("bed {i}: invalid gain {}", b.gain));
            }
        }
        for (i, f) in self.fields.iter().enumerate() {
            if !f.gain.is_finite() || !(0.0..=4.0).contains(&f.gain) {
                return Err(format!("field {i}: invalid gain {}", f.gain));
            }
        }
        let l = &self.listener;
        if l.position.iter().any(|v| !v.is_finite()) {
            return Err("listener: non-finite position".to_string());
        }
        if l.orientation.iter().any(|v| !v.is_finite()) {
            return Err("listener: non-finite orientation".to_string());
        }
        let norm_sq: f32 = l.orientation.iter().map(|v| v * v).sum();
        if (norm_sq - 1.0).abs() > 1e-2 {
            return Err(format!("listener: orientation not unit ({norm_sq})"));
        }
        self.room.validate_scene()
    }
}

/// Known channel-role names accepted in a bed's `channels` list (the engine
/// maps them to `ChannelId`). Unknown names are rejected at validation.
pub fn is_valid_role(name: &str) -> bool {
    matches!(
        name,
        "FL" | "FR"
            | "C"
            | "LFE"
            | "SL"
            | "SR"
            | "RL"
            | "RR"
            | "BC"
            | "TFL"
            | "TFR"
            | "TRL"
            | "TRR"
    )
}
