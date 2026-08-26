//! Loudness measurement and normalisation (EBU R128 / ReplayGain)
//!
//! Implements loudness normalisation that applies gain adjustments based on
//! pre-computed loudness metadata. The normaliser runs in the playback pipeline
//! and applies smooth gain transitions.

pub mod meter;
#[cfg(test)]
mod tests;
pub mod types;

// Re-export public API
pub use meter::{bs1770_weights_for_layout, LoudnessMeter, LoudnessNormalizer};
pub use types::{LoudnessMeasurement, LoudnessMetadata, LoudnessMode};

// Re-exports used by tests
#[cfg(test)]
pub(crate) use crate::decode::ChannelLayout;
#[cfg(test)]
pub(crate) use meter::KWeightStage1;
