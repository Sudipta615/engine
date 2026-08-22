//! Asynchronous events emitted by the audio engine to notify host applications.
//!
//! Unlike high-frequency state polling (via `PlaybackInfo` atomic telemetry),
//! [`EngineEvent`] represents discrete state transitions and lifecycle occurrences
//! such as track completion, format changes, errors, output device switches, and hotplug changes.

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
    /// The list of available audio output endpoints changed (e.g. USB DAC plugged in/out).
    DeviceListChanged {
        /// Currently available output device names.
        devices: Vec<String>,
    },
    /// An audio output endpoint was connected / arrived.
    DeviceConnected {
        /// Name of the connected audio device.
        device: String,
    },
    /// An audio output endpoint was disconnected / removed.
    DeviceDisconnected {
        /// Name of the disconnected audio device.
        device: String,
    },
    /// An engine-level error occurred during playback, recovery, or decoding.
    Error(String),
}
