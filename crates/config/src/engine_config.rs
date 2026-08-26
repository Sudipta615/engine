//! Top-level engine configuration and presets.

use serde::{Deserialize, Serialize};

use super::dsp_config::{
    BassManagementConfig, ChannelEqConfig, ChannelMixConfig, ChannelRoutingConfig,
    ChannelTrimConfig, ConvolutionConfig, CrossfadeConfig, CrossfeedConfig, EqConfig,
    GraphicEqConfig, LfeConfig, LimiterConfig, LoudnessConfig, MultibandCompressorConfig,
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
    /// Graphic EQ layer (§9.1). When enabled, it is the authoritative source
    /// for the pipeline's EQ bands and preamp.
    #[serde(default)]
    pub graphic_eq: GraphicEqConfig,
    pub loudness: LoudnessConfig,
    pub crossfeed: CrossfeedConfig,
    pub stereo_enhancer: StereoEnhancerConfig,
    pub limiter: LimiterConfig,
    pub multiband_compressor: MultibandCompressorConfig,
    pub convolution: ConvolutionConfig,
    /// Playback speed mode (varispeed vs time-stretch stub).
    #[serde(default)]
    pub speed_mode: SpeedMode,
    /// Time-stretch/pitch-shift quality tier (spec §22): maps to the WSOLA
    /// window/hop/search parameters. Higher tiers improve transient and
    /// sustained-tonal fidelity at higher CPU and algorithmic latency.
    #[serde(default)]
    pub timestretch_quality: TimeStretchQuality,
    /// DSD output handling policy.
    #[serde(default)]
    pub dsd_output: DsdOutput,
    /// Multichannel routing policy.
    #[serde(default)]
    pub channel_policy: ChannelPolicy,
    /// Per-channel gain/delay/polarity trim for the multichannel path.
    #[serde(default)]
    pub channel_trim: ChannelTrimConfig,
    /// Independent per-channel parametric EQ for multichannel output.
    #[serde(default)]
    pub channel_eq: ChannelEqConfig,
    /// Source→destination routing matrix for the multichannel path.
    #[serde(default)]
    pub channel_routing: ChannelRoutingConfig,
    /// LFE gain management.
    #[serde(default)]
    pub lfe: LfeConfig,
    /// Main-speaker high-pass / bass-management crossover.
    #[serde(default)]
    pub bass_management: BassManagementConfig,
    /// Explicit source-to-output upmix/downmix template.
    #[serde(default)]
    pub channel_mix: ChannelMixConfig,
    /// Track-to-track transition mode.
    #[serde(default)]
    pub transition_mode: TransitionMode,
    /// Number of mix-bus slots in the DSP graph (Phase 4). Slots 0/1 are the
    /// transition pair; slots ≥ 2 are independent simultaneous streams. Must
    /// be ≥ 2; larger values are clamped by the graph to its slot bound.
    #[serde(default = "default_mix_slots")]
    pub mix_slots: usize,
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
        }
    }
}

/// Engine configuration presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePreset {
    /// Consumer defaults: safe settings with DSP enabled where useful.
    Consumer,
    /// Audiophile / transparent mode: no DSP, exclusive output, f64 pipeline.
    Fidelity,
}

/// Result of validating an [`EngineConfig`]. Each entry is a human-readable
/// description of a contradiction or unsupported combination.
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
    /// Validate the configuration for contradictions or unsupported combinations.
    /// Call this before passing the config to `AudioEngine::new()` to surface
    /// configuration problems early (e.g. BitPerfect + DitherEnabled).
    pub fn validate(&self) -> ConfigValidation {
        let mut v = ConfigValidation::default();

        // When all DSP stages are disabled and Exclusive backend is requested,
        // the user is likely aiming for bit-perfect — dither on contradicts that.
        let all_dsp_off = !self.eq.enabled
            && !self.limiter.enabled
            && !self.crossfeed.enabled
            && !self.stereo_enhancer.enabled
            && !self.multiband_compressor.enabled
            && !self.convolution.enabled;
        if all_dsp_off && self.dither_enabled {
            v.warnings.push(
                "All DSP stages are disabled (potential bit-perfect setup) but dither \
                 is enabled. Dither operates on the integer PCM sample path and will \
                 alter sample values. Disable dither if bit-perfect output is desired."
                    .to_string(),
            );
        }

        // ExclusiveAsio only meaningful on Windows.
        if self.output_backend == AudioBackend::ExclusiveAsio && !cfg!(target_os = "windows") {
            v.warnings.push(
                "ExclusiveAsio backend is only available on Windows. The engine will fall back \
                 to the default backend on this platform."
                    .to_string(),
            );
        }

        // ASIO without the asio or asio-native feature is caught at runtime by
        // the engine; the config crate cannot see which features the engine was
        // built with, so we emit a warning on all platforms here.
        if self.output_backend == AudioBackend::ExclusiveAsio {
            v.warnings.push(
                "ExclusiveAsio backend selected. Ensure the 'asio' or 'asio-native' \
                 feature is enabled when compiling the engine crate."
                    .to_string(),
            );
        }

        // Hardware-only volume with no hardware backend available.
        if self.volume_mode == VolumeMode::HardwareOnly {
            v.warnings.push(
                "VolumeMode::HardwareOnly selected. If the output device does not support \
                 hardware volume control, the engine will report an error instead of applying \
                 software gain. Consider HardwarePreferred if a software fallback is acceptable."
                    .to_string(),
            );
        }

        // Extreme limiter ceiling.
        if self.limiter.enabled && self.limiter.ceiling_db > 0.0 {
            v.warnings.push(format!(
                "Limiter ceiling is {:.1} dB — values above 0 dB may cause clipping at the \
                 DAC. Conventional practice sets the ceiling between -1.0 and 0.0 dB.",
                self.limiter.ceiling_db
            ));
        }

        // Quality mode without resample — emits a warning; the engine handles
        // this at runtime by attempting to build the resampler.
        if self.precision_mode == PrecisionMode::Quality {
            v.warnings.push(
                "PrecisionMode::Quality selected. For best results, ensure the 'resample' \
                 feature is enabled when compiling the engine crate."
                    .to_string(),
            );
        }

        v
    }

    /// Build a configuration from a named preset.
    pub fn from_preset(preset: EnginePreset) -> Self {
        match preset {
            EnginePreset::Consumer => Self::default(),
            EnginePreset::Fidelity => Self {
                // Output: request exclusive/direct hardware access
                output_backend: AudioBackend::ExclusiveAlsa, // overridden per-platform by the engine
                fallback_policy: FallbackPolicy::Strict,
                sample_rate_policy: SampleRatePolicy::FollowTrack,

                // DSP — all disabled
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

                // Volume: hardware endpoint preferred (no software volume in signal path)
                volume_mode: VolumeMode::HardwarePreferred,

                // Format: high-precision, no dither when output is float
                precision_mode: PrecisionMode::Quality,
                dither_enabled: false,
                resampler_quality: ResamplerQuality::HighQuality,

                // Gapless transition by default in fidelity mode
                transition_mode: TransitionMode::Gapless,

                ..Default::default()
            },
        }
    }
}
