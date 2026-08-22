//! Playback stream state machine and engine error types.
//!
//! The `PlaybackStream` enum is defined in `mod.rs` so that its private
//! fields are accessible from all engine submodules (Rust privacy: child
//! modules can access items defined in their parent). This file contains
//! the `EngineError` enum and the `impl PlaybackStream` method block.

use super::PlaybackStream;
use crate::{
    decode::{DecodeError, DecodeInfo},
    output::cpal_output::OutputError,
};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("Invalid audio source: {0}")]
    InvalidSource(String),
    #[error("Output error: {0}")]
    Output(#[from] OutputError),
    #[error("Decode error: {0}")]
    Decode(#[from] DecodeError),
    #[error("Engine already running")]
    AlreadyRunning,
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Stream recovery failed: {0}")]
    StreamRecovery(String),
    /// The resampler is required (source rate != output rate, or speed != 1.0)
    /// but could not be built. Playback is halted because continuing would
    /// play at the wrong rate/pitch.
    #[error("Resampler error: {0}")]
    Resampler(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl PlaybackStream {
    /// Returns true if we are in the Transitioning (crossfading) state.
    pub fn is_crossfading(&self) -> bool {
        matches!(self, PlaybackStream::Transitioning { .. })
    }

    /// Get the decode info from the active (primary) decoder.
    pub fn active_info(&self) -> &DecodeInfo {
        match self {
            PlaybackStream::Single { decoder, .. } => decoder.info(),
            PlaybackStream::Transitioning {
                incoming_decoder, ..
            } => incoming_decoder.info(),
        }
    }

    /// Get the sample rate of the active decoder.
    pub fn active_sample_rate(&self) -> u32 {
        self.active_info().sample_rate
    }

    /// Get the duration of the outgoing (current) track in seconds.
    pub fn outgoing_duration_secs(&self) -> f32 {
        match self {
            PlaybackStream::Single { decoder, .. } => decoder.duration_secs(),
            PlaybackStream::Transitioning {
                outgoing_decoder, ..
            } => outgoing_decoder.duration_secs(),
        }
    }
}
