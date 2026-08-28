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
            endpoints: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePreset {
    Consumer,
    Fidelity,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigValidation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
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
            v.warnings
                .push("All DSP stages are disabled but dither is enabled.".to_string());
        }
        if self.mix_slots < 2 {
            v.errors.push("mix_slots must be at least 2".to_string());
        }
        if self.mix_slots > 8 {
            v.warnings
                .push("mix_slots above 8 will be clamped".to_string());
        }
        for trim in &self.mix_trims {
            if trim.slot >= 8 {
                v.errors
                    .push(format!("mix_trims slot {} is outside range", trim.slot));
            }
            if trim.channel >= 16 {
                v.errors.push(format!(
                    "mix_trims channel {} is outside range",
                    trim.channel
                ));
            }
            if !trim.gain_db.is_finite() || !(-60.0..=24.0).contains(&trim.gain_db) {
                v.errors.push(format!("invalid trim gain {}", trim.gain_db));
            }
        }
        for send in &self.mix_sends {
            if send.slot >= 8 {
                v.errors
                    .push(format!("mix_sends slot {} is outside range", send.slot));
            }
            if !send.master_gain.is_finite() || !(0.0..=1.0).contains(&send.master_gain) {
                v.errors
                    .push(format!("invalid master send {}", send.master_gain));
            }
            if !send.aux_gain.is_finite() || !(0.0..=1.0).contains(&send.aux_gain) {
                v.errors.push(format!("invalid aux send {}", send.aux_gain));
            }
        }
        if !self.aux.return_gain.is_finite() || !(0.0..=1.0).contains(&self.aux.return_gain) {
            v.errors
                .push(format!("invalid aux return {}", self.aux.return_gain));
        }
        if !self.correction.depth.is_finite() || !(0.0..=1.0).contains(&self.correction.depth) {
            v.errors.push(format!(
                "invalid correction depth {}",
                self.correction.depth
            ));
        }
        if self.correction.max_boost_db.is_finite()
            && !(0.0..=24.0).contains(&self.correction.max_boost_db)
        {
            v.errors.push(format!(
                "invalid correction max_boost_db {}",
                self.correction.max_boost_db
            ));
        }
        if self.correction.smoothing_octaves.is_finite()
            && !(0.02..=1.0).contains(&self.correction.smoothing_octaves)
        {
            v.errors.push(format!(
                "invalid correction smoothing_octaves {}",
                self.correction.smoothing_octaves
            ));
        }
        let mut endpoint_ids = std::collections::HashSet::new();
        for endpoint in &self.endpoints {
            if endpoint.id.trim().is_empty() {
                v.errors.push("endpoint id must not be empty".to_string());
            }
            if !endpoint_ids.insert(endpoint.id.trim().to_string()) {
                v.errors
                    .push(format!("duplicate endpoint id '{}'", endpoint.id));
            }
            if !endpoint.gain.is_finite() || !(0.0..=4.0).contains(&endpoint.gain) {
                v.errors
                    .push(format!("invalid endpoint gain {}", endpoint.gain));
            }
        }
        if self.output_backend == AudioBackend::ExclusiveAsio && !cfg!(target_os = "windows") {
            v.warnings
                .push("ExclusiveAsio is only available on Windows.".to_string());
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
