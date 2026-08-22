//! Asynchronous events emitted by the audio engine to notify host applications.
//!
//! Unlike high-frequency state polling (via `PlaybackInfo` atomic telemetry),
//! [`EngineEvent`] represents discrete state transitions and lifecycle occurrences
//! such as track completion, format changes, errors, and output device switches.

use crate::source::AudioSource;

/// Discrete event emitted by the engine to the host application.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// Playback started or resumed.
    PlaybackStarted,
    /// Playback was paused.
    PlaybackPaused,
    /// Playback stopped.
    PlaybackStopped,
    /// An audio source was successfully opened and prepared for decoding.
    SourceOpened {
        /// The opened source.
        source: AudioSource,
        /// Decoded sample rate in Hz.
        sample_rate: u32,
        /// Number of audio channels.
        channels: usize,
        /// Total duration in seconds (if known).
        duration_secs: f32,
    },
    /// Playback of an audio source completed naturally (reached End of Stream).
    SourceFinished {
        /// The finished source.
        source: AudioSource,
    },
    /// A seek request completed and the playhead was relocated.
    SeekCompleted {
        /// Target position in seconds.
        position_secs: f32,
    },
    /// The active audio output device or backend changed.
    OutputDeviceChanged {
        /// Selected output device name (`None` for system default).
        device: Option<String>,
    },
    /// Stream decode or output sample format changed.
    FormatChanged {
        /// New sample rate in Hz.
        sample_rate: u32,
        /// Channel count.
        channels: usize,
    },
    /// An engine-level error occurred during playback, recovery, or decoding.
    Error(String),
}
