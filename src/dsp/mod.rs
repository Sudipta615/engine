//! Digital Signal Processing module — EQ, limiter, loudness, resampler, and the full pipeline.

pub mod analyzer;
pub mod autoeq;
pub mod biquad;
pub mod channel_trim;
pub mod convolution;
pub mod correction;
pub mod crossfade;
pub mod crossfeed;
pub mod device_profile;
pub mod dither;
pub mod equalizer;
pub mod float;
pub mod gain;
pub mod graph;
pub mod graphic_eq;
pub mod limiter;
pub mod loudness;
pub mod multiband_compressor;
pub mod pipeline;
#[cfg(feature = "resample")]
pub mod resampler;
pub mod resampler_handle;
pub mod stereo;
pub mod timestretch;
pub mod true_peak;

pub use analyzer::{AnalyzerSnapshot, AudioAnalyzer, ANALYZER_FFT_SIZE, ANALYZER_UPDATE_HZ};
pub use autoeq::{AutoEq, AutoEqParams, AutoEqResult, FrequencyResponse, TargetCurve};
pub use biquad::{
    BiquadCoeffs, BiquadCoeffsF32, BiquadCoeffsF64, BiquadState, BiquadStateF32, BiquadStateF64,
    FilterType, SmoothedBiquad, SmoothedBiquadF32, SmoothedBiquadF64,
};
pub use channel_trim::{ChannelTrimmer, MAX_CHANNEL_DELAY_MS, MAX_CHANNEL_EQ_BANDS};
pub use convolution::ConvolutionEngine;
pub use crossfade::{CrossfadeConfig, CrossfadeCurve, MixerState, TrackMixer};
pub use dither::{Dither, DitherType};
pub use equalizer::{EqBandParams, EqFilterType, ParametricEq, MAX_EQ_BANDS};
pub use float::AudioFloat;
pub use gain::{FadeProcessor, FadeState, GainProcessor, GainProcessorF32, GainProcessorF64};
pub use graph::{DspGraph, DspNode};
pub use graphic_eq::GraphicEq;
pub use limiter::{LimiterMode, LookaheadLimiter, TruePeakMode};
pub use loudness::{
    LoudnessMeasurement, LoudnessMetadata, LoudnessMeter, LoudnessMode, LoudnessNormalizer,
};
pub use pipeline::{
    DspPipeline, DspStageCapability, EngineStats, OutputSampleFormat, PrecisionMode,
    StageChannelSupport, DSP_STAGE_CAPABILITIES,
};
#[cfg(feature = "resample")]
pub use resampler::AudioResampler;
#[cfg(feature = "resample")]
pub use resampler::ResamplerError;
pub use stereo::StereoEnhancer;
pub use timestretch::{TimeStretchConfig, TimeStretcher};
pub use true_peak::TruePeakMeter;

pub use crossfeed::Crossfeed;
pub use multiband_compressor::MultibandCompressor;
