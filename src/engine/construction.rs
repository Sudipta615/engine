//! Engine construction, configuration detection, command dispatch, and handle creation.

use std::{
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use arc_swap::ArcSwap;
use crossbeam::channel::{self, Sender};
use log::warn;

use config::EngineConfig;

#[cfg(feature = "audio-output")]
use crate::output::DeviceMonitor;
use crate::{
    buffer::{
        EngineCommand, FixedFrameBuffer, PlaybackInfo, DEFAULT_SAMPLE_RATE, OUTPUT_BUFFER_FRAMES,
    },
    dsp::{DspGraph, GraphicEq},
    playlist::Playlist,
    sink::SampleSink,
};

use super::{
    AudioClock, AudioEngine, DsdTransportState, EngineError, EngineHandle, EngineScratch,
    EngineTelemetry, LoudnessScanState, RecoveryState,
};

impl AudioEngine {
    /// Create a new audio engine with the default (DAC) sample sink.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        let output_buffer = Arc::new(
            FixedFrameBuffer::new(OUTPUT_BUFFER_FRAMES)
                .map_err(|e| EngineError::Config(format!("Output buffer: {}", e)))?,
        );
        let sample_sink = Box::new(crate::sink::DacSink::new(Arc::clone(&output_buffer)));
        Self::new_inner(config, output_buffer, sample_sink)
    }

    /// Create an engine with a custom [`SampleSink`].
    ///
    /// Use [`NoopSink`](crate::sink::NoopSink) for headless/analyzer mode
    /// (no output device needed). Use [`VecSink`](crate::sink::VecSink) to
    /// collect samples for off-line analysis. Implement your own for
    /// network broadcast, shared-memory visualization, or disk writing.
    ///
    /// When the sink is not a [`DacSink`](crate::sink::DacSink), no output
    /// backend can be started — the engine runs decode-and-analyze without
    /// driving a DAC.
    pub fn with_sink(config: EngineConfig, sink: Box<dyn SampleSink>) -> Result<Self, EngineError> {
        let output_buffer = Arc::new(
            FixedFrameBuffer::new(OUTPUT_BUFFER_FRAMES)
                .map_err(|e| EngineError::Config(format!("Output buffer: {}", e)))?,
        );
        Self::new_inner(config, output_buffer, sink)
    }

    /// Shared constructor body.
    fn new_inner(
        config: EngineConfig,
        output_buffer: Arc<FixedFrameBuffer>,
        sample_sink: Box<dyn SampleSink>,
    ) -> Result<Self, EngineError> {
        let (cmd_tx, cmd_rx) = channel::bounded(256);
        let (event_tx, event_rx) = channel::bounded(256);
        #[cfg(feature = "audio-output")]
        let (output_event_tx, output_event_rx) = channel::bounded(64);
        let output_sample_rate = DEFAULT_SAMPLE_RATE;
        let graph = DspGraph::from_config(&config, output_sample_rate as f32);
        let graphic_eq = GraphicEq::from_config(&config.graphic_eq);
        let info = PlaybackInfo {
            sample_rate: output_sample_rate,
            ..Default::default()
        };
        let clock = AudioClock::new(DEFAULT_SAMPLE_RATE);
        #[cfg(feature = "audio-output")]
        let device_monitor = DeviceMonitor::new(config.output_backend, Duration::from_millis(1500));

        Ok(Self {
            output_buffer,
            sample_sink,
            cmd_tx,
            cmd_rx,
            playback_info: Arc::new(ArcSwap::new(Arc::new(info))),
            running: Arc::new(AtomicBool::new(false)),
            audio_output: None,
            graph,
            graphic_eq,
            output_profile: None,
            stream: None,
            playlist: Playlist::new(),
            analyzer: Arc::new(crate::dsp::AudioAnalyzer::new_default()),
            config,
            duration_secs: 0.0,
            output_sample_rate,
            speed: 1.0,
            clock,
            current_source: None,
            stream_ended: false,
            event_tx,
            event_rx,
            #[cfg(feature = "audio-output")]
            output_event_tx,
            #[cfg(feature = "audio-output")]
            output_event_rx,
            #[cfg(feature = "audio-output")]
            device_monitor,
            #[cfg(all(target_os = "windows", feature = "wasapi-native"))]
            capture: None,

            telemetry: EngineTelemetry::default(),
            dsd: DsdTransportState::default(),
            loudness_scan: LoudnessScanState::default(),
            recovery: RecoveryState::default(),
            scratch: EngineScratch::default(),
        })
    }

    /// Convenience constructor using the default configuration.
    pub fn new_default() -> Result<Self, EngineError> {
        Self::new(EngineConfig::default())
    }

    #[allow(dead_code)]
    pub(super) fn detect_output_sample_rate() -> Option<u32> {
        #[cfg(test)]
        {
            // Avoid querying OS audio drivers during unit tests to prevent WASAPI/COM access violations on Windows CI runners.
            None
        }
        #[cfg(not(test))]
        {
            use cpal::traits::{DeviceTrait, HostTrait};
            let host = cpal::default_host();
            let device = host.default_output_device()?;
            let default_config = device.default_output_config().ok()?;
            Some(default_config.sample_rate())
        }
    }

    /// Push interleaved f32 samples to the active [`SampleSink`].
    ///
    /// Returns the number of **frames** accepted. The engine retries
    /// unwritten tail frames on the next tick. This is the only write
    /// path from the decode loop to the output — every stereo interleaved
    /// and multichannel interleaved push routes through here. It also feeds
    /// the real-time analyzer.
    #[inline]
    pub(crate) fn push_to_sink(&self, samples: &[f32], channels: usize) -> usize {
        self.analyzer.update(samples, channels);
        self.sample_sink.push_interleaved(samples, channels)
    }

    /// Access the shared real-time analyzer (levels + spectrum).
    pub fn analyzer(&self) -> Arc<crate::dsp::AudioAnalyzer> {
        Arc::clone(&self.analyzer)
    }

    pub fn send_command(&self, cmd: EngineCommand) {
        let is_critical = matches!(
            cmd,
            EngineCommand::Play
                | EngineCommand::Pause
                | EngineCommand::Stop
                | EngineCommand::Shutdown
        );
        let timeout = if is_critical {
            std::time::Duration::from_secs(5)
        } else {
            std::time::Duration::from_millis(100)
        };
        match self.cmd_tx.send_timeout(cmd, timeout) {
            Ok(()) => {}
            Err(crossbeam::channel::SendTimeoutError::Timeout(cmd)) => {
                // For idempotent state-setting commands (volume, seek, eq, balance),
                // attempt a fallback non-blocking send before giving up.
                if self.cmd_tx.try_send(cmd.clone()).is_err() {
                    warn!(
                        "Engine command channel saturated; dropped non-critical command: {:?}",
                        cmd
                    );
                }
            }
            Err(crossbeam::channel::SendTimeoutError::Disconnected(_)) => {
                warn!("Engine command channel disconnected; command dropped");
            }
        }
    }

    pub fn send_command_channel(&mut self) -> Sender<EngineCommand> {
        self.cmd_tx.clone()
    }

    /// Create a safe, decoupled [`EngineHandle`] for host applications and controllers.
    pub fn handle(&self) -> EngineHandle {
        EngineHandle::new(
            self.cmd_tx.clone(),
            Arc::clone(&self.playback_info),
            self.event_rx.clone(),
            #[cfg(feature = "audio-output")]
            self.output_event_rx.clone(),
            Arc::clone(&self.analyzer),
        )
    }
}
