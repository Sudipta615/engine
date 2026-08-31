//! Top-level engine configuration and presets.

use serde::{Deserialize, Serialize};

use super::dsp_config::{
    BassManagementConfig, ChannelEqConfig, ChannelMixConfig, ChannelRoutingConfig,
    ChannelTrimConfig, ConvolutionConfig, CorrectionConfig, CrossfadeConfig, CrossfeedConfig,
    EqConfig, GraphicEqConfig, LfeConfig, LimiterConfig, LoudnessConfig, MultibandCompressorConfig,
    StereoEnhancerConfig,
};
use super::enums::{
    AudioBackend, ChannelPolicy, DsdOutput, FallbackPolicy, LoudnessMode, PerformanceMode,
    PrecisionMode, ResamplerQuality, SpeedMode, TimeStretchQuality, TransitionMode, VolumeMode,
};
use super::rate_policy::SampleRatePolicy;
use super::spatial_render::{SpatialMeterConfig, SpatialQuality, SpatialVoiceConfig};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineConfig {
    pub output_backend: AudioBackend,
    pub output_device: Option<String>,
    pub dither_enabled: bool,
    pub resampler_quality: ResamplerQuality,
    pub crossfade: CrossfadeConfig,
    pub performance_mode: PerformanceMode,
    pub precision_mode: PrecisionMode,
    pub fallback_policy: FallbackPolicy,
    pub sample_rate_policy: SampleRatePolicy,
    pub volume_mode: VolumeMode,
    pub volume_fade_ms: u64,
    pub seek_fade_ms: u64,
    pub eq: EqConfig,
    #[serde(default)]
    pub graphic_eq: GraphicEqConfig,
    pub loudness: LoudnessConfig,
    pub crossfeed: CrossfeedConfig,
    pub stereo_enhancer: StereoEnhancerConfig,
    pub limiter: LimiterConfig,
    pub multiband_compressor: MultibandCompressorConfig,
    pub convolution: ConvolutionConfig,
    #[serde(default)]
    pub speed_mode: SpeedMode,
    #[serde(default)]
    pub timestretch_quality: TimeStretchQuality,
    #[serde(default)]
    pub dsd_output: DsdOutput,
    #[serde(default)]
    pub channel_policy: ChannelPolicy,
    #[serde(default)]
    pub channel_trim: ChannelTrimConfig,
    #[serde(default)]
    pub channel_eq: ChannelEqConfig,
    #[serde(default)]
    pub channel_routing: ChannelRoutingConfig,
    #[serde(default)]
    pub lfe: LfeConfig,
    #[serde(default)]
    pub bass_management: BassManagementConfig,
    #[serde(default)]
    pub channel_mix: ChannelMixConfig,
    #[serde(default)]
    pub transition_mode: TransitionMode,
    #[serde(default = "default_mix_slots")]
    pub mix_slots: usize,
    #[serde(default)]
    pub mix_trims: Vec<SlotTrimEntry>,
    #[serde(default)]
    pub mix_sends: Vec<SlotSendConfig>,
    #[serde(default)]
    pub aux: AuxBusConfig,
    #[serde(default)]
    pub correction: CorrectionConfig,
    #[serde(default)]
    pub spatial: SpatialConfig,
    /// Optional explicit path for the active spatial scene's auto-save file
    /// (Phase 21). `None` = the engine's user-data directory default
    /// (`<data_local_dir>/engine/spatial_scene.json`). Hosts that want a
    /// custom location (or to disable persistence entirely, via `Some` of a
    /// path that is never writable) set this.
    #[serde(default)]
    pub spatial_autosave_path: Option<std::path::PathBuf>,
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotTrimEntry {
    pub slot: usize,
    pub channel: usize,
    pub gain_db: f32,
    #[serde(default)]
    pub invert: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotSendConfig {
    pub slot: usize,
    #[serde(default = "default_master_gain")]
    pub master_gain: f32,
    #[serde(default)]
    pub aux_gain: f32,
}

fn default_master_gain() -> f32 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub id: String,
    pub backend: AudioBackend,
    pub device: Option<String>,
    #[serde(default = "default_endpoint_gain")]
    pub gain: f32,
    #[serde(default = "default_endpoint_enabled")]
    pub enabled: bool,
    /// Per-endpoint clock drift correction: a slip resampler trims the
    /// nominal-rate stream to the device's real clock (ring-fill feedback on
    /// the slip ratio) so independent crystals can't drift the ring full or
    /// empty. Default true; irrelevant when the rates match.
    #[serde(default = "default_drift_correction")]
    pub drift_correction: bool,
}

fn default_endpoint_gain() -> f32 {
    1.0
}
fn default_endpoint_enabled() -> bool {
    true
}
fn default_drift_correction() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuxBusConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_master_gain")]
    pub return_gain: f32,
    /// Whether the insert convolution is active (Phase 6). Disabled = no
    /// insert processing = bit-exact.
    #[serde(default)]
    pub insert_enabled: bool,
    /// Impulse-response file for the insert convolution.
    #[serde(default)]
    pub insert_ir_path: Option<String>,
    /// Insert wet/dry mix in [0, 1] (1.0 = fully wet).
    #[serde(default = "default_master_gain")]
    pub insert_wet_mix: f32,
}

impl Default for AuxBusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            return_gain: 1.0,
            insert_enabled: false,
            insert_ir_path: None,
            insert_wet_mix: 1.0,
        }
    }
}

fn default_mix_slots() -> usize {
    2
}

/// The spatial master output stage (Phase 17): renders the mixed front
/// pair through the engine's spatial layer (binaural head model + room).
/// Disabled by default, so existing configurations render bit-identically.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpatialConfig {
    /// Master enable. Disabled = the graph's spatial step is skipped
    /// (bit-exact passthrough).
    #[serde(default)]
    pub enabled: bool,
    /// Virtual-screen center azimuth (degrees; + = right of the listener).
    #[serde(default)]
    pub center_azimuth_deg: f32,
    /// Half-width of the L/R screen spread (degrees, clamped to 90).
    #[serde(default = "default_spatial_half_width")]
    pub half_width_deg: f32,
    /// Screen elevation (degrees, clamped to ±90).
    #[serde(default)]
    pub elevation_deg: f32,
    /// Linear screen gain (clamped to 4).
    #[serde(default = "default_master_gain")]
    pub gain: f32,
    /// The room applied to the program (early reflections + late field).
    #[serde(default)]
    pub room: SpatialRoomConfig,
    /// Listener orientation (degrees; yaw positive = right turn).
    #[serde(default)]
    pub listener_yaw_deg: f32,
    #[serde(default)]
    pub listener_pitch_deg: f32,
    #[serde(default)]
    pub listener_roll_deg: f32,
    /// Renderer quality tier (spec §86).
    #[serde(default)]
    pub quality: SpatialQuality,
    /// Voice management budget (spec §76).
    #[serde(default)]
    pub voice: SpatialVoiceConfig,
    /// Output metering enable (spec §70).
    #[serde(default)]
    pub metering: SpatialMeterConfig,
}

fn default_spatial_half_width() -> f32 {
    30.0
}

impl Default for SpatialConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            center_azimuth_deg: 0.0,
            half_width_deg: default_spatial_half_width(),
            elevation_deg: 0.0,
            gain: 1.0,
            room: SpatialRoomConfig::default(),
            listener_yaw_deg: 0.0,
            listener_pitch_deg: 0.0,
            listener_roll_deg: 0.0,
            quality: SpatialQuality::default(),
            voice: SpatialVoiceConfig::default(),
            metering: SpatialMeterConfig::default(),
        }
    }
}

/// Room model for the spatial master: geometry, wall absorption,
/// reflection order, and the late field (spec §49).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpatialRoomConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Room width / depth / height in metres (origin at a corner).
    #[serde(default = "default_room_width")]
    pub width: f32,
    #[serde(default = "default_room_depth")]
    pub depth: f32,
    #[serde(default = "default_room_height")]
    pub height: f32,
    /// Wall absorption `0..1` (reflection coefficient = `1 − absorption`).
    #[serde(default = "default_room_absorption")]
    pub absorption: f32,
    /// Early-reflection order: 1 (six images) or 2 (24 images).
    #[serde(default = "default_room_order")]
    pub reflection_order: u8,
    /// Late-field RT60 in ms.
    #[serde(default = "default_room_rt60")]
    pub rt60_ms: f32,
    /// Late-field wet mix `0..1`.
    #[serde(default = "default_room_late_mix")]
    pub late_mix: f32,
    /// Program reflection send `0..1` (the object-side room send).
    #[serde(default = "default_room_wet")]
    pub wet: f32,
    /// Speed of sound (m/s), used for the reflection delays.
    #[serde(default = "default_speed_of_sound")]
    pub speed_of_sound: f32,
}

fn default_speed_of_sound() -> f32 {
    343.0
}
fn default_room_width() -> f32 {
    12.0
}
fn default_room_depth() -> f32 {
    10.0
}
fn default_room_height() -> f32 {
    3.0
}
fn default_room_absorption() -> f32 {
    0.2
}
fn default_room_order() -> u8 {
    1
}
fn default_room_rt60() -> f32 {
    800.0
}
fn default_room_late_mix() -> f32 {
    0.3
}
fn default_room_wet() -> f32 {
    0.5
}

impl Default for SpatialRoomConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            width: default_room_width(),
            depth: default_room_depth(),
            height: default_room_height(),
            absorption: default_room_absorption(),
            reflection_order: default_room_order(),
            rt60_ms: default_room_rt60(),
            late_mix: default_room_late_mix(),
            wet: default_room_wet(),
            speed_of_sound: default_speed_of_sound(),
        }
    }
}

impl SpatialRoomConfig {
    /// Validate the room fields in isolation (shared by the engine-config
    /// and scene-file validators). `Err` carries the first problem.
    pub fn validate_scene(&self) -> Result<(), String> {
        if !self.width.is_finite() || self.width <= 0.0 {
            return Err(format!("invalid room width {}", self.width));
        }
        if !self.depth.is_finite() || self.depth <= 0.0 {
            return Err(format!("invalid room depth {}", self.depth));
        }
        if !self.height.is_finite() || self.height <= 0.0 {
            return Err(format!("invalid room height {}", self.height));
        }
        if !self.absorption.is_finite() || !(0.0..=0.99).contains(&self.absorption) {
            return Err(format!("invalid room absorption {}", self.absorption));
        }
        if self.reflection_order != 1 && self.reflection_order != 2 {
            return Err(format!(
                "invalid room reflection_order {}",
                self.reflection_order
            ));
        }
        if !self.rt60_ms.is_finite() || self.rt60_ms <= 0.0 {
            return Err(format!("invalid room rt60_ms {}", self.rt60_ms));
        }
        if !self.late_mix.is_finite() || !(0.0..=1.0).contains(&self.late_mix) {
            return Err(format!("invalid room late_mix {}", self.late_mix));
        }
        if !self.wet.is_finite() || !(0.0..=1.0).contains(&self.wet) {
            return Err(format!("invalid room wet {}", self.wet));
        }
        if !self.speed_of_sound.is_finite() || self.speed_of_sound <= 0.0 {
            return Err(format!(
                "invalid room speed_of_sound {}",
                self.speed_of_sound
            ));
        }
        Ok(())
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            output_backend: AudioBackend::Auto,
            output_device: None,
            dither_enabled: true,
            resampler_quality: ResamplerQuality::Balanced,
            crossfade: CrossfadeConfig::default(),
            performance_mode: PerformanceMode::Normal,
            precision_mode: PrecisionMode::Performance,
            fallback_policy: FallbackPolicy::Allow,
            sample_rate_policy: SampleRatePolicy::FollowTrack,
            volume_mode: VolumeMode::SoftwareOnly,
            volume_fade_ms: 10,
            seek_fade_ms: 20,
            eq: EqConfig::default(),
            graphic_eq: GraphicEqConfig::default(),
            loudness: LoudnessConfig::default(),
            crossfeed: CrossfeedConfig::default(),
            stereo_enhancer: StereoEnhancerConfig::default(),
            limiter: LimiterConfig::default(),
            multiband_compressor: MultibandCompressorConfig::default(),
            convolution: ConvolutionConfig::default(),
            speed_mode: SpeedMode::default(),
            timestretch_quality: TimeStretchQuality::default(),
            dsd_output: DsdOutput::default(),
            channel_policy: ChannelPolicy::default(),
            channel_trim: ChannelTrimConfig::default(),
            channel_eq: ChannelEqConfig::default(),
            channel_routing: ChannelRoutingConfig::default(),
            lfe: LfeConfig::default(),
            bass_management: BassManagementConfig::default(),
            channel_mix: ChannelMixConfig::default(),
            transition_mode: TransitionMode::default(),
            mix_slots: default_mix_slots(),
            mix_trims: Vec::new(),
            mix_sends: Vec::new(),
            aux: AuxBusConfig::default(),
            correction: CorrectionConfig::default(),
            spatial: SpatialConfig::default(),
            spatial_autosave_path: None,
            endpoints: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePreset {
    Consumer,
    Fidelity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSeverity {
    #[default]
    Error,
    Warning,
}

/// Stable, serializable category for a configuration issue. `code()` returns
/// a machine-readable name a host can branch on without parsing the message.
/// Do not rename the codes casually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ConfigIssueKind {
    /// General / uncategorised invalid value.
    #[default]
    General,
    /// Dither policy inconsistency.
    DitherPolicy,
    /// Mix-bus slot count / per-slot trim or send config.
    MixBus,
    /// Aux bus return / insert config.
    AuxBus,
    /// Room/headphone correction parameters.
    Correction,
    /// Spatial master room / screen / gain parameters.
    Spatial,
    /// Output endpoint (id / gain / duplicates).
    Endpoint,
    /// Output backend availability.
    Backend,
}

impl ConfigIssueKind {
    pub fn code(self) -> &'static str {
        match self {
            ConfigIssueKind::General => "general",
            ConfigIssueKind::DitherPolicy => "dither_policy",
            ConfigIssueKind::MixBus => "mix_bus",
            ConfigIssueKind::AuxBus => "aux_bus",
            ConfigIssueKind::Correction => "correction",
            ConfigIssueKind::Spatial => "spatial",
            ConfigIssueKind::Endpoint => "endpoint",
            ConfigIssueKind::Backend => "backend",
        }
    }
}

/// A single typed configuration validation issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIssue {
    pub kind: ConfigIssueKind,
    pub severity: ConfigSeverity,
    pub message: String,
}

impl std::fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConfigValidation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// Typed, serializable companion to `errors` / `warnings`, giving a host a
    /// stable category per issue. Derived from the same checks, so it always
    /// agrees with the string vectors.
    pub issues: Vec<ConfigIssue>,
}
impl ConfigValidation {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

impl EngineConfig {
    pub fn validate(&self) -> ConfigValidation {
        let mut v = ConfigValidation::default();
        let all_dsp_off = !self.eq.enabled
            && !self.limiter.enabled
            && !self.crossfeed.enabled
            && !self.stereo_enhancer.enabled
            && !self.multiband_compressor.enabled
            && !self.convolution.enabled;
        if all_dsp_off && self.dither_enabled {
            let msg = "All DSP stages are disabled but dither is enabled.".to_string();
            v.warnings.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::DitherPolicy,
                severity: ConfigSeverity::Warning,
                message: v.warnings.last().unwrap().clone(),
            });
        }
        if self.mix_slots < 2 {
            let msg = "mix_slots must be at least 2".to_string();
            v.errors.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::MixBus,
                severity: ConfigSeverity::Error,
                message: v.errors.last().unwrap().clone(),
            });
        }
        if self.mix_slots > 8 {
            let msg = "mix_slots above 8 will be clamped".to_string();
            v.warnings.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::MixBus,
                severity: ConfigSeverity::Warning,
                message: v.warnings.last().unwrap().clone(),
            });
        }
        for trim in &self.mix_trims {
            if trim.slot >= 8 {
                v.errors
                    .push(format!("mix_trims slot {} is outside range", trim.slot));
                v.issues.push(ConfigIssue {
                    kind: ConfigIssueKind::MixBus,
                    severity: ConfigSeverity::Error,
                    message: v.errors.last().unwrap().clone(),
                });
            }
            if trim.channel >= 16 {
                v.errors.push(format!(
                    "mix_trims channel {} is outside range",
                    trim.channel
                ));
                v.issues.push(ConfigIssue {
                    kind: ConfigIssueKind::MixBus,
                    severity: ConfigSeverity::Error,
                    message: v.errors.last().unwrap().clone(),
                });
            }
            if !trim.gain_db.is_finite() || !(-60.0..=24.0).contains(&trim.gain_db) {
                let msg = format!("invalid trim gain {}", trim.gain_db);
                v.errors.push(msg);
                v.issues.push(ConfigIssue {
                    kind: ConfigIssueKind::MixBus,
                    severity: ConfigSeverity::Error,
                    message: v.errors.last().unwrap().clone(),
                });
            }
        }
        for send in &self.mix_sends {
            if send.slot >= 8 {
                let msg = format!("mix_sends slot {} is outside range", send.slot);
                v.errors.push(msg);
                v.issues.push(ConfigIssue {
                    kind: ConfigIssueKind::MixBus,
                    severity: ConfigSeverity::Error,
                    message: v.errors.last().unwrap().clone(),
                });
            }
            if !send.master_gain.is_finite() || !(0.0..=1.0).contains(&send.master_gain) {
                let msg = format!("invalid master send {}", send.master_gain);
                v.errors.push(msg);
                v.issues.push(ConfigIssue {
                    kind: ConfigIssueKind::MixBus,
                    severity: ConfigSeverity::Error,
                    message: v.errors.last().unwrap().clone(),
                });
            }
            if !send.aux_gain.is_finite() || !(0.0..=1.0).contains(&send.aux_gain) {
                let msg = format!("invalid aux send {}", send.aux_gain);
                v.errors.push(msg);
                v.issues.push(ConfigIssue {
                    kind: ConfigIssueKind::MixBus,
                    severity: ConfigSeverity::Error,
                    message: v.errors.last().unwrap().clone(),
                });
            }
        }
        if !self.aux.return_gain.is_finite() || !(0.0..=1.0).contains(&self.aux.return_gain) {
            let msg = format!("invalid aux return {}", self.aux.return_gain);
            v.errors.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::AuxBus,
                severity: ConfigSeverity::Error,
                message: v.errors.last().unwrap().clone(),
            });
        }
        if !self.correction.depth.is_finite() || !(0.0..=1.0).contains(&self.correction.depth) {
            let msg = format!("invalid correction depth {}", self.correction.depth);
            v.errors.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::Correction,
                severity: ConfigSeverity::Error,
                message: v.errors.last().unwrap().clone(),
            });
        }
        if self.correction.max_boost_db.is_finite()
            && !(0.0..=24.0).contains(&self.correction.max_boost_db)
        {
            let msg = format!(
                "invalid correction max_boost_db {}",
                self.correction.max_boost_db
            );
            v.errors.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::Correction,
                severity: ConfigSeverity::Error,
                message: v.errors.last().unwrap().clone(),
            });
        }
        if self.correction.smoothing_octaves.is_finite()
            && !(0.02..=1.0).contains(&self.correction.smoothing_octaves)
        {
            let msg = format!(
                "invalid correction smoothing_octaves {}",
                self.correction.smoothing_octaves
            );
            v.errors.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::Correction,
                severity: ConfigSeverity::Error,
                message: v.errors.last().unwrap().clone(),
            });
        }
        let s = &self.spatial;
        if !s.gain.is_finite() || !(0.0..=4.0).contains(&s.gain) {
            let msg = format!("invalid spatial gain {}", s.gain);
            v.errors.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::Spatial,
                severity: ConfigSeverity::Error,
                message: v.errors.last().unwrap().clone(),
            });
        }
        if !s.half_width_deg.is_finite() || !(0.0..=90.0).contains(&s.half_width_deg) {
            let msg = format!("invalid spatial half_width_deg {}", s.half_width_deg);
            v.errors.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::Spatial,
                severity: ConfigSeverity::Error,
                message: v.errors.last().unwrap().clone(),
            });
        }
        if let Err(e) = s.room.validate_scene() {
            v.errors.push(e);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::Spatial,
                severity: ConfigSeverity::Error,
                message: v.errors.last().unwrap().clone(),
            });
        }
        let mut endpoint_ids = std::collections::HashSet::new();
        for endpoint in &self.endpoints {
            if endpoint.id.trim().is_empty() {
                let msg = "endpoint id must not be empty".to_string();
                v.errors.push(msg);
                v.issues.push(ConfigIssue {
                    kind: ConfigIssueKind::Endpoint,
                    severity: ConfigSeverity::Error,
                    message: v.errors.last().unwrap().clone(),
                });
            }
            if !endpoint_ids.insert(endpoint.id.trim().to_string()) {
                let msg = format!("duplicate endpoint id '{}'", endpoint.id);
                v.errors.push(msg);
                v.issues.push(ConfigIssue {
                    kind: ConfigIssueKind::Endpoint,
                    severity: ConfigSeverity::Error,
                    message: v.errors.last().unwrap().clone(),
                });
            }
            if !endpoint.gain.is_finite() || !(0.0..=4.0).contains(&endpoint.gain) {
                let msg = format!("invalid endpoint gain {}", endpoint.gain);
                v.errors.push(msg);
                v.issues.push(ConfigIssue {
                    kind: ConfigIssueKind::Endpoint,
                    severity: ConfigSeverity::Error,
                    message: v.errors.last().unwrap().clone(),
                });
            }
        }
        if self.output_backend == AudioBackend::ExclusiveAsio && !cfg!(target_os = "windows") {
            let msg = "ExclusiveAsio is only available on Windows.".to_string();
            v.warnings.push(msg);
            v.issues.push(ConfigIssue {
                kind: ConfigIssueKind::Backend,
                severity: ConfigSeverity::Warning,
                message: v.warnings.last().unwrap().clone(),
            });
        }
        v
    }

    pub fn from_preset(preset: EnginePreset) -> Self {
        match preset {
            EnginePreset::Consumer => Self::default(),
            EnginePreset::Fidelity => Self {
                output_backend: AudioBackend::ExclusiveAlsa,
                fallback_policy: FallbackPolicy::Strict,
                eq: EqConfig {
                    enabled: false,
                    ..Default::default()
                },
                loudness: LoudnessConfig {
                    mode: LoudnessMode::Off,
                    ..Default::default()
                },
                limiter: LimiterConfig {
                    enabled: false,
                    ..Default::default()
                },
                crossfeed: CrossfeedConfig {
                    enabled: false,
                    ..Default::default()
                },
                stereo_enhancer: StereoEnhancerConfig {
                    enabled: false,
                    ..Default::default()
                },
                multiband_compressor: MultibandCompressorConfig {
                    enabled: false,
                    ..Default::default()
                },
                convolution: ConvolutionConfig {
                    enabled: false,
                    ..Default::default()
                },
                volume_mode: VolumeMode::HardwarePreferred,
                precision_mode: PrecisionMode::Quality,
                dither_enabled: false,
                resampler_quality: ResamplerQuality::HighQuality,
                transition_mode: TransitionMode::Gapless,
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad_mix_slots() -> EngineConfig {
        let mut c = EngineConfig::default();
        c.mix_slots = 1; // below the minimum of 2
        c
    }

    fn bad_endpoint_gain() -> EngineConfig {
        let mut c = EngineConfig::default();
        c.endpoints.push(EndpointConfig {
            id: "test".to_string(),
            backend: AudioBackend::default(),
            device: None,
            gain: 99.0, // far above the 0..=4.0 range
            enabled: true,
            drift_correction: true,
        });
        c
    }

    #[test]
    fn validates_and_produces_typed_issues_agreeing_with_strings() {
        let v = bad_mix_slots().validate();
        assert!(!v.is_valid());
        assert!(!v.errors.is_empty());
        assert_eq!(v.errors.len(), v.issues.len());
        assert!(v.issues.iter().any(|i| i.kind == ConfigIssueKind::MixBus
            && i.severity == ConfigSeverity::Error
            && !i.message.is_empty()));
        // codes are stable and machine-readable
        assert_eq!(ConfigIssueKind::MixBus.code(), "mix_bus");
    }

    #[test]
    fn endpoint_issues_carry_endpoint_kind() {
        let v = bad_endpoint_gain().validate();
        assert!(v.issues.iter().any(|i| {
            i.kind == ConfigIssueKind::Endpoint && i.message.contains("endpoint gain")
        }));
    }

    #[test]
    fn severity_serializes_to_snake_case() {
        let w = ConfigSeverity::Warning;
        let j = serde_json::to_string(&w).unwrap();
        assert_eq!(j, "\"warning\"");
        let back: ConfigSeverity = serde_json::from_str(&j).unwrap();
        assert_eq!(back, ConfigSeverity::Warning);
    }
}
