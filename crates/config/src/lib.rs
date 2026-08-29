//! Engine configuration types.
//!
//! The crate is organized into focused modules — [`enums`], [`dsp_config`],
//! [`rate_policy`], and [`engine_config`] — all re-exported at the crate root
//! so the public API remains `config::TypeName`.

mod dsp_config;
mod engine_config;
mod enums;
mod rate_policy;
mod scene_config;
mod spatial_render;

pub use dsp_config::{
    BandConfig, BassManagementConfig, ChannelEqConfig, ChannelEqEntry, ChannelMixConfig,
    ChannelMixTemplate, ChannelRoutingConfig, ChannelTrimConfig, ChannelTrimEntry,
    ConvolutionConfig, CorrectionConfig, CorrectionPhaseMode, CorrectionTarget, CrossfadeConfig,
    CrossfeedConfig, EqBandConfig, EqConfig, EqPreset, GraphicEqConfig, GraphicEqLayout, LfeConfig,
    LimiterConfig, LoudnessConfig, MultibandCompressorConfig, StereoEnhancerConfig,
};
pub use engine_config::{
    AuxBusConfig, ConfigValidation, EndpointConfig, EngineConfig, EnginePreset, SlotSendConfig,
    SlotTrimEntry, SpatialConfig, SpatialRoomConfig,
};
pub use enums::{
    AudioBackend, ChannelPolicy, CompressorDetector, CrossfadeCurve, CrossfeedProfile,
    DitherPolicy, DsdOutput, FallbackPolicy, FilterType, LoudnessMode, OutputAccessMode,
    OutputAccessState, PerformanceMode, PrecisionMode, RateFallbackPolicy, ResamplerQuality,
    ResamplerQualityInfo, SpeedMode, TimeStretchQuality, TransitionMode, VolumeMode,
};
pub use rate_policy::{apply_fallback, base_rate, clock_family, nearest_rate, SampleRatePolicy};
pub use scene_config::{
    is_valid_role, CurveQuatConfig, CurveScalarConfig, CurveVec3Config, SceneListenerConfig,
    SpatialAutomationConfig, SpatialBedConfig, SpatialFieldConfig, SpatialObjectConfig,
    SpatialSceneConfig,
};
pub use spatial_render::{SpatialMeterConfig, SpatialQuality, SpatialVoiceConfig, VoicePriority};

pub mod types {
    pub mod enums {
        pub use crate::{CrossfeedProfile, ResamplerQuality};
    }
}
