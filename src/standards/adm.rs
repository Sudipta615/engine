//! Formal standards model for the Audio Definition Model (ADM, ITU-R BS.2076).
//!
//! Provides the complete hierarchical XML/metadata structure specified in
//! ITU-R BS.2076-1 and BS.2076-2 for immersive, object-based, and scene-based audio.

use serde::{Deserialize, Serialize};

/// ADM specification version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmStandard {
    /// ITU-R BS.2076-1 (Initial ADM specification).
    ItuBs2076_1,
    /// ITU-R BS.2076-2 (Current ADM recommendation with expanded HOA/binaural).
    #[default]
    ItuBs2076_2,
}

/// Type definition for audio format definitions in ADM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioTypeDefinition {
    /// DirectSpeakers (channel-based / bed layout).
    #[default]
    DirectSpeakers,
    /// Matrix (downmix / upmix matrices).
    Matrix,
    /// Objects (dynamic point sources with coordinates, width, divergence).
    Objects,
    /// HOA (Higher Order Ambisonics).
    Hoa,
    /// Binaural (pre-rendered head-related impulse response).
    Binaural,
}

/// Highest level of audio presentation in ADM (ITU-R BS.2076 §3.1).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioProgramme {
    pub id: String,
    pub name: String,
    pub language: Option<String>,
    pub max_loudness: Option<f32>,
    pub contents: Vec<String>, // IDs of associated AudioContent elements
}

/// Grouping of audio objects/beds representing a single logical component (e.g. Dialogue, Music, Effects).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioContent {
    pub id: String,
    pub name: String,
    pub dialogue: Option<u8>, // 0 = no dialogue, 1 = dialogue, 2 = mixed
    pub objects: Vec<String>, // IDs of associated AudioObject elements
}

/// An individual audio entity or bed in ADM.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioObject {
    pub id: String,
    pub name: String,
    pub start: Option<f64>, // Time in seconds
    pub duration: Option<f64>,
    pub head_locked: bool,
    pub screen_relative: bool,
    pub pack_formats: Vec<String>, // IDs of associated AudioPackFormat elements
    pub track_formats: Vec<String>, // IDs of associated AudioTrackFormat elements
}

/// Logical grouping of audio channels (e.g. 5.1 bed, HOA order 2, Object bundle).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioPackFormat {
    pub id: String,
    pub name: String,
    pub audio_type: AudioTypeDefinition,
    pub channel_formats: Vec<String>, // IDs of associated AudioChannelFormat elements
}

/// Physical or virtual audio channel definition.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioChannelFormat {
    pub id: String,
    pub name: String,
    pub audio_type: AudioTypeDefinition,
    pub blocks: Vec<AudioBlockFormat>,
}

/// Time-bounded parameter slice within an audio channel format.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioBlockFormat {
    pub id: String,
    pub r#type: AudioTypeDefinition,
    pub start: f64,
    pub duration: f64,
    // Spatial object coordinates (Cartesian or Polar)
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub z: Option<f32>,
    pub azimuth: Option<f32>,
    pub elevation: Option<f32>,
    pub distance: Option<f32>,
    // Object properties
    pub gain: Option<f32>,
    pub spread: Option<f32>,
    pub divergence: Option<f32>,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub depth: Option<f32>,
    pub diffuseness: Option<f32>,
    pub screen_ref: bool,
    // HOA parameters
    pub order: Option<u32>,
    pub degree: Option<i32>,
    pub normalization: Option<String>,
}

/// Stream format associating track formats with channel formats.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioStreamFormat {
    pub id: String,
    pub name: String,
    pub audio_type: AudioTypeDefinition,
    pub channel_format_id: Option<String>,
    pub pack_format_id: Option<String>,
}

/// Representation of a single audio track in a container (e.g. BW64/BWF).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioTrackFormat {
    pub id: String,
    pub name: String,
    pub format: String, // e.g. "PCM"
    pub stream_format_id: Option<String>,
}

/// Complete ADM document container.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AdmDocument {
    pub standard: AdmStandard,
    pub programmes: Vec<AudioProgramme>,
    pub contents: Vec<AudioContent>,
    pub objects: Vec<AudioObject>,
    pub pack_formats: Vec<AudioPackFormat>,
    pub channel_formats: Vec<AudioChannelFormat>,
    pub stream_formats: Vec<AudioStreamFormat>,
    pub track_formats: Vec<AudioTrackFormat>,
    #[serde(default)]
    pub zones: Vec<AdmZoneExclusion>,
}

/// 3D exclusion zone definition in ADM (ITU-R BS.2076 §3.6).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AdmZoneExclusion {
    pub min_x: f32,
    pub max_x: f32,
    pub min_y: f32,
    pub max_y: f32,
    pub min_z: f32,
    pub max_z: f32,
}

/// Screen reference geometry in ADM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenReferenceAdm {
    pub screen_id: String,
    pub aspect_ratio: f32,
    pub width_m: f32,
    pub height_m: f32,
}

impl Default for ScreenReferenceAdm {
    fn default() -> Self {
        Self {
            screen_id: "screen_0".to_string(),
            aspect_ratio: 16.0 / 9.0,
            width_m: 2.0,
            height_m: 1.125,
        }
    }
}

/// Convert ADM polar coordinates (azimuth, elevation, distance) to engine Cartesian coordinates.
/// ITU-R BS.2076: Azimuth counter-clockwise from front (+Y), Elevation positive upwards (+Z).
/// Engine coordinate frame: +X right, +Y front, +Z up.
pub fn adm_polar_to_cartesian(
    azimuth_deg: f32,
    elevation_deg: f32,
    distance_m: f32,
) -> (f32, f32, f32) {
    let az_rad = azimuth_deg.to_radians();
    let el_rad = elevation_deg.to_radians();
    let r = distance_m.max(0.0);

    // BS.2076 polar coordinates:
    // x = -r * cos(el) * sin(az)  (where az > 0 is left, so +X right is -sin(az))
    // y =  r * cos(el) * cos(az)  (front)
    // z =  r * sin(el)            (up)
    // When standard right-handed ADM defines az = 0 (front), az = -90 (right):
    let x = -r * el_rad.cos() * az_rad.sin();
    let y = r * el_rad.cos() * az_rad.cos();
    let z = r * el_rad.sin();
    (x, y, z)
}

/// Convert engine Cartesian coordinates (+X right, +Y front, +Z up) to ADM polar coordinates.
pub fn cartesian_to_adm_polar(x: f32, y: f32, z: f32) -> (f32, f32, f32) {
    let dist = (x * x + y * y + z * z).sqrt();
    if dist < 1e-6 {
        return (0.0, 0.0, 0.0);
    }
    let el_deg = (z / dist).asin().to_degrees();
    // az = -atan2(x, y) in degrees
    let az_deg = (-x).atan2(y).to_degrees();
    (az_deg, el_deg, dist)
}
