//! Playback stream state machine and engine error types.

#[cfg(feature = "resample")]
use crate::dsp::resampler::GenericResampler;
use crate::{
    decode::Decoder,
    decode::{DecodeError, DecodeInfo},
    output::cpal_output::OutputError,
};

/// Dual-decoder state machine for true gapless playback and crossfading.
///
/// `Single` represents normal single-track playback. `Transitioning` holds
/// both the outgoing (fading) and incoming (rising) decoders simultaneously,
/// allowing the `TrackMixer` to receive genuinely distinct sample streams
/// and perform real overlapping gain scaling.
#[allow(clippy::large_enum_variant)]
pub enum PlaybackStream {
    /// Playing a single track with no crossfade in progress.
    Single {
        decoder: Decoder,
        #[cfg(feature = "resample")]
        resampler: Option<GenericResampler>,
        #[cfg(not(feature = "resample"))]
        resampler: Option<()>,
    },
    /// Crossfading between two tracks. The outgoing decoder provides the
    /// tail of the current track while the incoming decoder provides the
    /// head of the next.
    Transitioning {
        outgoing_decoder: Decoder,
        #[cfg(feature = "resample")]
        outgoing_resampler: Option<GenericResampler>,
        #[cfg(not(feature = "resample"))]
        outgoing_resampler: Option<()>,
        incoming_decoder: Decoder,
        #[cfg(feature = "resample")]
        incoming_resampler: Option<GenericResampler>,
        #[cfg(not(feature = "resample"))]
        incoming_resampler: Option<()>,
        /// Frames remaining in the crossfade transition.
        crossfade_frames_remaining: usize,
        /// Total crossfade duration in frames.
        crossfade_total_frames: usize,
    },
}

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
