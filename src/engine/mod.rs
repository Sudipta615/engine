//! Core audio engine — wires decode → DSP → output pipeline
//!

mod buffers;
mod clock;
mod commands;
mod construction;
mod crossfade;
mod decode_loop;
mod dsd_state;
pub mod handle;
pub mod helpers;
mod loudness_state;
mod output_setup;
mod recovery;
mod stream;
mod telemetry;
#[cfg(test)]
mod tests;
mod tick;
mod track_loading;
mod volume;

pub(crate) use buffers::EngineScratch;
#[allow(unused_imports)]
pub use buffers::{
    CROSSFADE_SCRATCH_FRAMES, MAX_PENDING_MULTICHANNEL_SAMPLES, MAX_PENDING_OUTPUT_FRAMES,
    MIX_BLOCK_FRAMES,
};
pub use clock::AudioClock;
pub(crate) use dsd_state::{dop_exclusive_reason, DsdTransportState};
pub use handle::EngineHandle;
pub(crate) use loudness_state::LoudnessScanState;
pub(crate) use recovery::RecoveryState;
pub(crate) use telemetry::EngineTelemetry;

use std::sync::{atomic::AtomicBool, Arc};

use arc_swap::ArcSwap;
use crossbeam::channel::{Receiver, Sender};

// Re-export public types from submodules so the public API is unchanged.
use config::EngineConfig;
pub use stream::{EngineError, PlaybackStream};

#[cfg(feature = "audio-output")]
use crate::output::DeviceMonitor;
use crate::{
    buffer::{EngineCommand, FixedFrameBuffer, PlaybackInfo},
    dsp::analyzer::AudioAnalyzer,
    dsp::pipeline::DspPipeline,
    events::{EngineEvent, OutputEvent},
    output::Output,
    playlist::Playlist,
    sink::SampleSink,
    source::AudioSource,
};

pub struct AudioEngine {
    /// The output ring buffer — retained so output backends can drain it directly.
    output_buffer: Arc<FixedFrameBuffer>,
    /// The pluggable sample sink. Processed samples are delivered here after
    /// the resampler and final safety limiter. Defaults to a [`DacSink`](crate::sink::DacSink)
    /// that pushes into `output_buffer`.
    sample_sink: Box<dyn SampleSink>,
    cmd_tx: Sender<EngineCommand>,
    cmd_rx: Receiver<EngineCommand>,
    /// Playback info stored in an ArcSwap for wait-free concurrent reads.
    /// Writers use rcu() for atomic snapshot replacement; readers use load().
    /// This makes the decode hot path lock-free — no OS scheduler involvement.
    playback_info: Arc<ArcSwap<PlaybackInfo>>,
    running: Arc<AtomicBool>,
    /// The active output transport (cpal, or the native WASAPI exclusive
    /// backend on Windows with `wasapi-native`).
    audio_output: Option<Box<dyn Output>>,
    pipeline: DspPipeline,
    /// Graphic EQ model (§9.1) — the slider state compiled into
    /// `pipeline.eq`. Always present; only authoritative while enabled.
    graphic_eq: crate::dsp::GraphicEq,
    /// Explicitly selected output profile (§10). When `None`, the engine
    /// auto-selects from the built-in/user profile library by device name.
    output_profile: Option<crate::output::OutputProfile>,
    /// The dual-decoder state machine — replaces the single `decoder` field.
    stream: Option<PlaybackStream>,
    /// Ordered playback queue with shuffle/repeat/history.  The engine
    /// auto-advances it at EndOfStream (honoring `RepeatMode::One` by
    /// restarting the current track) and `Next`/`Previous` commands.
    playlist: Playlist,
    /// Real-time level/spectrum analyzer fed from the decode loop. Hosts
    /// share the same `Arc` via [`Self::analyzer`] or the handle.
    analyzer: Arc<AudioAnalyzer>,
    config: EngineConfig,
    duration_secs: f32,
    output_sample_rate: u32,
    speed: f32,
    /// Sample-accurate integer playback clock — the single source of truth
    /// for the playhead (position and current source sample rate).
    clock: AudioClock,
    current_source: Option<AudioSource>,
    stream_ended: bool,
    event_tx: Sender<EngineEvent>,
    event_rx: Receiver<EngineEvent>,
    /// Output device events (device connect/disconnect, list changes).
    /// Separate channel so hosts that don't drive audio output never see these.
    #[allow(dead_code)]
    #[cfg(feature = "audio-output")]
    output_event_tx: Sender<OutputEvent>,
    #[allow(dead_code)]
    #[cfg(feature = "audio-output")]
    output_event_rx: Receiver<OutputEvent>,
    #[cfg(feature = "audio-output")]
    device_monitor: DeviceMonitor,

    /// Active system-audio capture (WASAPI loopback), if any. The loopback
    /// thread fills `ActiveCapture.capture`'s ring; the tick loop drains it
    /// into the WAV writer.
    #[cfg(all(target_os = "windows", feature = "wasapi-native"))]
    capture: Option<ActiveCapture>,

    // ── Domain sub-structures ──
    pub(crate) telemetry: EngineTelemetry,
    pub(crate) dsd: DsdTransportState,
    pub(crate) loudness_scan: LoudnessScanState,
    pub(crate) recovery: RecoveryState,
    pub(crate) scratch: EngineScratch,
}

/// An active system-audio capture: the loopback endpoint plus the WAV file
/// the tick loop streams into. Windows-only (see `wasapi-native`).
#[cfg(all(target_os = "windows", feature = "wasapi-native"))]
pub(crate) struct ActiveCapture {
    pub(crate) capture: crate::output::WasapiLoopbackCapture,
    pub(crate) writer: crate::output::wav_writer::WavFileWriter,
    pub(crate) path: std::path::PathBuf,
}

impl AudioEngine {}
