//! Engine configuration types.
//!
//! The crate is organized into focused modules — [`enums`], [`dsp_config`],
//! [`rate_policy`], and [`engine_config`] — all re-exported at the crate root
//! so the public API remains `config::TypeName`.

mod dsp_config;
mod engine_config;
mod enums;
mod rate_policy;

pub use dsp_config::{
    BandConfig, BassManagementConfig, ChannelEqConfig, ChannelEqEntry, ChannelMixConfig,
    ChannelMixTemplate, ChannelRoutingConfig, ChannelTrimConfig, ChannelTrimEntry,
    ConvolutionConfig, CrossfadeConfig, CrossfeedConfig, EqBandConfig, EqConfig, EqPreset,
    GraphicEqConfig, GraphicEqLayout, LfeConfig, LimiterConfig, LoudnessConfig,
    MultibandCompressorConfig, StereoEnhancerConfig,
};
pub use engine_config::{EngineConfig, EnginePreset};
pub use enums::{
    AudioBackend, ChannelPolicy, CompressorDetector, CrossfadeCurve, CrossfeedProfile,
    DitherPolicy, DsdOutput, FallbackPolicy, FilterType, LoudnessMode, OutputAccessMode,
    OutputAccessState, PerformanceMode, PrecisionMode, RateFallbackPolicy, ResamplerQuality,
    ResamplerQualityInfo, SpeedMode, TimeStretchQuality, TransitionMode, VolumeMode,
};
pub use rate_policy::{apply_fallback, base_rate, clock_family, nearest_rate, SampleRatePolicy};

pub mod types {
    pub mod enums {
        pub use crate::{CrossfeedProfile, ResamplerQuality};
    }
}
