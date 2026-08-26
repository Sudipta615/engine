//! Asynchronous events emitted by the audio engine to notify host applications.
//!
//! Unlike high-frequency state polling (via `PlaybackInfo` atomic telemetry),
//! [`EngineEvent`] represents discrete state transitions and lifecycle occurrences
//! such as track completion, format changes, errors, and seek completion.
//!
//! # Event channels
//!
//! - **[`EngineEvent`]** — always present. Playback lifecycle events: source
//!   opened/finished, play/pause/stop transitions, seek completion, format
//!   changes, and engine errors.
//! - **[`OutputEvent`]** — only present when the `audio-output` feature is
//!   enabled. Hardware device events: device connect/disconnect, device list
//!   changes, and output device switches. Hosts that only decode and analyze
//!   audio (no DAC output) never receive these.

use crate::decode::LoudnessScanResult;
use crate::source::AudioSource;

/// Discrete event emitted by the engine to the host application.
///
/// These are lifecycle events — the host subscribes via
/// [`EngineHandle::clone_event_receiver`](crate::engine::EngineHandle::clone_event_receiver).
/// For output-device hotplug and backend-change events, use
/// [`EngineHandle::clone_output_event_receiver`](crate::engine::EngineHandle::clone_output_event_receiver)
/// (gated behind the `audio-output` feature).
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The playback queue changed (entry added/removed/cleared, or the
    /// current track index changed).
    PlaylistChanged {
        /// Index of the current track, or `None` when the queue is empty.
        current_index: Option<usize>,
        /// Total number of queue entries.
        length: usize,
    },
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
    /// Stream decode or output sample format changed.
    FormatChanged {
        /// New sample rate in Hz.
        sample_rate: u32,
        /// Channel count.
        channels: usize,
    },
    /// An engine-level error occurred during playback, recovery, or decoding.
    Error(String),
    /// A background EBU R128 / ReplayGain loudness scan completed for a loaded
    /// track. The engine has already applied the measured metadata to the
    /// active pipeline; this event notifies the host so it can e.g. update
    /// its cache or UI.
    LoudnessScanComplete {
        /// Path to the scanned audio file.
        path: std::path::PathBuf,
        /// Measured loudness (LUFS, dBTP, LRA, RG gain/peak). `None` if
        /// the file could not be decoded or yielded no measurable audio.
        result: Option<LoudnessScanResult>,
    },
    /// System-audio capture started (WASAPI loopback).
    CaptureStarted {
        /// WAV file being written.
        path: std::path::PathBuf,
    },
    /// System-audio capture stopped and the WAV file finalized.
    CaptureStopped {
        /// WAV file that was written.
        path: std::path::PathBuf,
        /// Frames captured.
        frames: u64,
        /// Approximate duration in seconds.
        duration_secs: f32,
    },
    /// A capture could not be started or stopped.
    CaptureError(String),
}

/// Hardware output device events.
///
/// Only emitted when the `audio-output` feature is enabled. Hosts that
/// only decode and analyze audio (no DAC) never receive these events.
/// Subscribe via
/// [`EngineHandle::clone_output_event_receiver`](crate::engine::EngineHandle::clone_output_event_receiver).
#[derive(Debug, Clone, PartialEq)]
pub enum OutputEvent {
    /// The active audio output device or backend changed.
    OutputDeviceChanged {
        /// Selected output device name (`None` for system default).
        device: Option<String>,
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
}
