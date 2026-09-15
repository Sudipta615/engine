//! Spatial Bass subsystem: crossover filtering, dedicated bass management,
//! and the spatial bass engine (Pure, BassManaged, BassImmersion modes).

pub mod engine;
pub mod filter;
pub mod management;
pub mod psychoacoustic;

pub use engine::SpatialBassEngine;
pub use filter::{CrossoverFilter, SubwooferDelay, SubwooferPhase, MAX_SUB_DELAY_SAMPLES};
pub use management::{BassManager, MAX_BASS_CHANNELS};
pub use psychoacoustic::{
    FundamentalDetector, HarmonicSelector, MaskingModel, PsychoacousticBassProcessor,
    SpeakerCapabilityModel,
};
