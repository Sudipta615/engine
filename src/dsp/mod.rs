//! Digital Signal Processing module — EQ, limiter, loudness, resampler, and the full pipeline.

pub mod aelog;
pub mod analysis;
pub mod analyzer;
pub mod autoeq;
pub mod biquad;
pub mod channel_trim;
pub mod convolution;
pub mod correction;
pub mod crossfade;
pub mod crossfeed;
pub mod crossover;
pub mod deterministic;
pub mod device_profile;
pub mod dither;
pub mod dynamics;
pub mod equalizer;
pub mod float;
pub mod gain;
pub mod graph2;
pub mod graphic_eq;
pub mod limiter;
pub mod loudness;
pub mod meters;
pub mod modulation;
pub mod multiband_compressor;
pub mod parameters;
pub mod pipeline;
#[cfg(feature = "resample")]
pub mod resampler;
pub mod resampler_handle;
pub mod safety;
pub mod simd;
pub mod stereo;
pub mod timeline;
pub mod timestretch;
pub mod true_peak;

pub use analysis::{AnalysisEngine, AnalysisSnapshot};
pub use analyzer::{AnalyzerSnapshot, AudioAnalyzer, ANALYZER_FFT_SIZE, ANALYZER_UPDATE_HZ};
pub use autoeq::{AutoEq, AutoEqParams, AutoEqResult, FrequencyResponse, TargetCurve};
pub use biquad::{
    BiquadCoeffs, BiquadCoeffsF32, BiquadCoeffsF64, BiquadState, BiquadStateF32, BiquadStateF64,
    FilterType, SmoothedBiquad, SmoothedBiquadF32, SmoothedBiquadF64,
};
pub use channel_trim::{ChannelTrimmer, MAX_CHANNEL_DELAY_MS, MAX_CHANNEL_EQ_BANDS};
pub use convolution::ConvolutionEngine;
pub use crossfade::{CrossfadeConfig, CrossfadeCurve, MixerState, TrackMixer};
pub use crossover::{
    Crossover2Way, Crossover3Way, CrossoverArchitecture, CrossoverCpuTier, CrossoverPhaseBehavior,
};
pub use dither::{Dither, DitherType};
pub use dynamics::{
    BallisticEnvelope, ChannelLinkMode, DetectionMode, DetectorConfig, DynamicsDetector,
    SidechainFilter,
};
pub use equalizer::{
    DynamicEq, DynamicEqBand, DynamicEqBandParams, EqBandParams, EqFilterType, ParametricEq,
    MAX_DYNAMIC_EQ_BANDS, MAX_EQ_BANDS,
};
pub use float::AudioFloat;
pub use gain::{FadeProcessor, FadeState, GainProcessor, GainProcessorF32, GainProcessorF64};
pub use graphic_eq::GraphicEq;
pub use limiter::{LimiterMode, LookaheadLimiter, TruePeakMode};
pub use loudness::{
    LoudnessMeasurement, LoudnessMetadata, LoudnessMeter, LoudnessMode, LoudnessNormalizer,
};
pub use modulation::{AdsrEnvelope, EnvelopeFollower, Lfo, ModulationMatrix};
pub use pipeline::{
    DspPipeline, DspStageCapability, EngineStats, OutputSampleFormat, PrecisionMode,
    StageChannelSupport, DSP_STAGE_CAPABILITIES,
};
#[cfg(feature = "resample")]
pub use resampler::AudioResampler;
#[cfg(feature = "resample")]
pub use resampler::LatencyProvider;
#[cfg(feature = "resample")]
pub use resampler::ResamplerError;
pub use stereo::StereoEnhancer;
pub use timestretch::{TimeStretchConfig, TimeStretcher};
pub use true_peak::TruePeakMeter;

pub use crossfeed::Crossfeed;
pub use deterministic::{compare_buffers, DeterministicMode, EquivalenceClass};
pub use meters::{ProfessionalMeterSnapshot, ProfessionalMeters};
pub use multiband_compressor::MultibandCompressor;
pub use parameters::{
    ParameterCurve, ParameterDescriptor, ParameterId, ParameterRegistry, ParameterSmoothing,
    ParameterUnit,
};
pub use safety::{
    contain_non_finite_block, contain_non_finite_planes, FloatSafetyMode, NonFiniteIncident,
    NonFinitePolicy,
};
