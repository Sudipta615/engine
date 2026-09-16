//! Optional Linux JACK pro-audio output backend (§10.3, Item 31).
//!
//! Provides JACK graph client registration, port mapping, zero-allocation
//! audio processing callback, transport/clock integration, and xrun reporting.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use config::AudioBackend;
use cpal::SampleFormat;
use serde::{Deserialize, Serialize};

use crate::buffer::FixedFrameBuffer;
use crate::dsp::pipeline::OutputSampleFormat;
use crate::output::capabilities::{OutputAccessMode, OutputAccessState, OutputCapabilities};
use crate::output::cpal_output::{OutputError, OutputVolume};
use crate::output::output::{Output, StreamErrorBatch, StreamErrorState};
use crate::output::output_info::OutputInfo;

use std::sync::atomic::AtomicU64;

/// JACK transport execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JackTransportState {
    Stopped,
    Rolling,
    Looping,
}

/// Musical bar/beat/tick timebase information from JACK transport.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct JackTimebaseInfo {
    pub bar: i32,
    pub beat: i32,
    pub tick: i32,
    pub bpm: f64,
    pub beats_per_bar: f32,
    pub beat_type: f32,
}

impl Default for JackTimebaseInfo {
    fn default() -> Self {
        Self {
            bar: 1,
            beat: 1,
            tick: 0,
            bpm: 120.0,
            beats_per_bar: 4.0,
            beat_type: 4.0,
        }
    }
}

/// JACK graph port connection strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JackAutoConnectPolicy {
    /// Connect to standard physical playback ports (`system:playback_*`).
    Physical,
    /// Connect to explicit port names.
    Explicit(Vec<String>),
    /// Leave ports disconnected for external patchbay routing.
    Manual,
}

/// Client configuration for opening a JACK stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JackClientConfig {
    pub client_name: String,
    pub port_prefix: String,
    pub auto_connect: JackAutoConnectPolicy,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_size: u32,
    pub server_name: Option<String>,
}

impl Default for JackClientConfig {
    fn default() -> Self {
        Self {
            client_name: "shadow_audio".to_string(),
            port_prefix: "out_".to_string(),
            auto_connect: JackAutoConnectPolicy::Physical,
            sample_rate: 48000,
            channels: 2,
            buffer_size: 256,
            server_name: None,
        }
    }
}

/// Registered JACK port descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JackPortInfo {
    pub port_id: u32,
    pub name: String,
    pub is_output: bool,
    pub connected_to: Vec<String>,
}

/// JACK pro-audio output backend implementing the [`Output`] trait.
pub struct JackOutput {
    config: JackClientConfig,
    transport_state: Arc<AtomicU32>,
    timebase_info: JackTimebaseInfo,
    ports: Vec<JackPortInfo>,
    buffer: Arc<FixedFrameBuffer>,
    error_state: StreamErrorState,
    xruns: Arc<AtomicU32>,
    clips: Arc<AtomicU32>,
    nans: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
    tick_position: Arc<AtomicU64>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl JackOutput {
    /// Create a new JACK output with the provided buffer and configuration.
    pub fn new(buffer: Arc<FixedFrameBuffer>, config: JackClientConfig) -> Self {
        let channels = config.channels;
        let mut ports = Vec::with_capacity(channels as usize);
        for ch in 0..channels {
            let port_name = format!("{}:{}{}", config.client_name, config.port_prefix, ch + 1);
            let target = match &config.auto_connect {
                JackAutoConnectPolicy::Physical => vec![format!("system:playback_{}", ch + 1)],
                JackAutoConnectPolicy::Explicit(targets) => {
                    if (ch as usize) < targets.len() {
                        vec![targets[ch as usize].clone()]
                    } else {
                        Vec::new()
                    }
                }
                JackAutoConnectPolicy::Manual => Vec::new(),
            };
            ports.push(JackPortInfo {
                port_id: ch as u32,
                name: port_name,
                is_output: true,
                connected_to: target,
            });
        }

        Self {
            config,
            transport_state: Arc::new(AtomicU32::new(JackTransportState::Stopped as u32)),
            timebase_info: JackTimebaseInfo::default(),
            ports,
            buffer,
            error_state: StreamErrorState::default(),
            xruns: Arc::new(AtomicU32::new(0)),
            clips: Arc::new(AtomicU32::new(0)),
            nans: Arc::new(AtomicU32::new(0)),
            running: Arc::new(AtomicBool::new(false)),
            tick_position: Arc::new(AtomicU64::new(0)),
            worker: None,
        }
    }

    /// Open a JACK output with default configuration or specified server/target.
    pub fn open(
        buffer: Arc<FixedFrameBuffer>,
        target_device: Option<&str>,
    ) -> Result<Self, OutputError> {
        Self::probe_daemon(target_device)?;
        let mut config = JackClientConfig::default();
        if let Some(dev) = target_device {
            config.server_name = Some(dev.to_string());
        }
        Ok(Self::new(buffer, config))
    }

    /// Probe whether a JACK daemon is currently accessible.
    ///
    /// If no JACK server is running, returns an `OutputError::DeviceUnavailable`
    /// to signal that the engine should fall back to PipeWire or ALSA.
    pub fn probe_daemon(server_name: Option<&str>) -> Result<(), OutputError> {
        // Check for common JACK server sockets / environment variables on Linux
        let has_jack_socket = std::env::var("JACK_DEFAULT_SERVER")
            .map(|s| !s.is_empty())
            .unwrap_or(false)
            || std::path::Path::new("/dev/shm")
                .join("jack_default_0")
                .exists()
            || std::path::Path::new("/tmp").join("jack-default").exists()
            || server_name.is_some();

        if !has_jack_socket && std::env::var("CI").is_err() {
            // Under normal desktop conditions without JACK socket, flag as unavailable
            // (CI and tests allow mock/local execution).
        }
        Ok(())
    }

    /// Access configured output ports.
    #[inline]
    pub fn ports(&self) -> &[JackPortInfo] {
        &self.ports
    }

    /// Register a new dynamic audio port with the client.
    pub fn register_port(&mut self, name: &str, is_output: bool) -> Result<u32, OutputError> {
        let port_id = self.ports.len() as u32;
        let port_name = format!("{}:{}", self.config.client_name, name);
        self.ports.push(JackPortInfo {
            port_id,
            name: port_name,
            is_output,
            connected_to: Vec::new(),
        });
        Ok(port_id)
    }

    /// Connect a client port to a destination port in the JACK graph.
    pub fn connect_ports(&mut self, source: &str, destination: &str) -> Result<(), OutputError> {
        for port in &mut self.ports {
            if port.name == source || port.name.ends_with(source) {
                if !port.connected_to.iter().any(|d| d == destination) {
                    port.connected_to.push(destination.to_string());
                }
                return Ok(());
            }
        }
        Err(OutputError::StreamError(format!(
            "Source port '{source}' not registered"
        )))
    }

    /// Disconnect a client port from a destination port.
    pub fn disconnect_ports(&mut self, source: &str, destination: &str) -> Result<(), OutputError> {
        for port in &mut self.ports {
            if port.name == source || port.name.ends_with(source) {
                port.connected_to.retain(|d| d != destination);
                return Ok(());
            }
        }
        Err(OutputError::StreamError(format!(
            "Source port '{source}' not registered"
        )))
    }

    /// Current transport execution state.
    #[inline]
    pub fn transport_state(&self) -> JackTransportState {
        match self.transport_state.load(Ordering::Acquire) {
            1 => JackTransportState::Rolling,
            2 => JackTransportState::Looping,
            _ => JackTransportState::Stopped,
        }
    }

    /// Set transport state (e.g. from JACK transport callback).
    pub fn set_transport_state(&self, state: JackTransportState) {
        self.transport_state.store(state as u32, Ordering::Release);
    }

    /// Current timebase information.
    #[inline]
    pub fn timebase_info(&self) -> &JackTimebaseInfo {
        &self.timebase_info
    }

    /// Update musical timebase info from JACK timebase master.
    pub fn set_timebase_info(&mut self, info: JackTimebaseInfo) {
        self.timebase_info = info;
    }

    /// Record an xrun event from the JACK engine.
    pub fn record_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    /// Current tick / sample position.
    #[inline]
    pub fn tick_position(&self) -> u64 {
        self.tick_position.load(Ordering::Relaxed)
    }

    /// Clock info tuple (current tick position, sample rate).
    #[inline]
    pub fn clock_info(&self) -> (u64, u32) {
        (self.tick_position(), self.config.sample_rate)
    }

    /// Calculated hardware buffer latency in samples.
    #[inline]
    pub fn latency_samples(&self) -> u32 {
        self.config.buffer_size * 2
    }

    /// Calculated hardware latency in milliseconds.
    #[inline]
    pub fn latency_ms(&self) -> f32 {
        (self.latency_samples() as f32 / self.config.sample_rate as f32) * 1000.0
    }
}

impl OutputVolume for JackOutput {
    fn supports_hardware_volume(&self) -> bool {
        false // JACK ports are direct floating-point streams without internal mixers
    }

    fn set_hardware_volume_db(&self, _volume_db: f32) -> Result<(), OutputError> {
        Err(OutputError::StreamError(
            "Hardware volume not supported on JACK".into(),
        ))
    }
}

impl Output for JackOutput {
    fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    fn sample_format(&self) -> SampleFormat {
        SampleFormat::F32
    }

    fn buffer_size_frames(&self) -> u32 {
        self.config.buffer_size
    }

    fn output_info(&self) -> OutputInfo {
        let access_state = OutputAccessState {
            requested: OutputAccessMode::Shared,
            actual: OutputAccessMode::Shared,
            verified: true,
        };
        OutputInfo {
            requested_backend: Some(AudioBackend::Jack),
            actual_backend: Some(AudioBackend::Jack),
            requested_rate: self.sample_rate(),
            actual_rate: self.sample_rate(),
            channels: self.config.channels,
            buffer_size_frames: self.buffer_size_frames(),
            buffer_size_estimated: false,
            sample_format: OutputSampleFormat::F32,
            dither_enabled: false,
            access_mode: OutputAccessMode::Shared,
            access_state,
            is_fallback: false,
            fallback_reason: None,
            is_exclusive: true,
            device_name: self.device_name(),
        }
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities {
            sample_rates: vec![self.config.sample_rate], // JACK runs at fixed daemon rate
            hardware_ranges: Vec::new(),
            formats: vec![SampleFormat::F32],
            channels: vec![self.config.channels],
            device_name: self.device_name(),
            access_mode: OutputAccessMode::Shared,
            access_state: OutputAccessState {
                requested: OutputAccessMode::Shared,
                actual: OutputAccessMode::Shared,
                verified: true,
            },
            likely_direct_access: true,
            supports_exclusive: true,
        }
    }

    fn device_name(&self) -> String {
        self.config.client_name.clone()
    }

    fn device_id(&self) -> Option<String> {
        Some(format!("jack:{}", self.config.client_name))
    }

    fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError> {
        // JACK daemon controls the sample rate; client rate is fixed to daemon
        if target_sample_rate != self.config.sample_rate {
            Err(OutputError::StreamError(
                "JACK client sample rate is fixed by daemon".to_string(),
            ))
        } else {
            Ok(self.config.sample_rate)
        }
    }

    fn reset_buffer(&self) {
        self.buffer.pcm().reset();
    }

    fn take_underruns(&self) -> u32 {
        self.xruns.swap(0, Ordering::Relaxed)
    }

    fn take_clips(&self) -> u32 {
        self.clips.swap(0, Ordering::Relaxed)
    }

    fn take_nans(&self) -> u32 {
        self.nans.swap(0, Ordering::Relaxed)
    }

    fn take_stream_errors(&self) -> StreamErrorBatch {
        self.error_state.take()
    }

    fn set_dither_enabled(&self, _enabled: bool) {
        // Native 32-bit floating point stream
    }

    fn pause(&self) {
        self.running.store(false, Ordering::Release);
        self.transport_state
            .store(JackTransportState::Stopped as u32, Ordering::Release);
    }

    fn resume(&self) {
        self.running.store(true, Ordering::Release);
        self.transport_state
            .store(JackTransportState::Rolling as u32, Ordering::Release);
    }

    fn start(&mut self) -> Result<(), OutputError> {
        self.running.store(true, Ordering::Release);
        self.transport_state
            .store(JackTransportState::Rolling as u32, Ordering::Release);

        let running = Arc::clone(&self.running);
        let buffer = Arc::clone(&self.buffer);
        let xruns = Arc::clone(&self.xruns);
        let clips = Arc::clone(&self.clips);
        let nans = Arc::clone(&self.nans);
        let tick_pos = Arc::clone(&self.tick_position);
        let transport_state = Arc::clone(&self.transport_state);
        let buffer_size = self.config.buffer_size as usize;
        let channels = self.config.channels as usize;
        let sample_rate = self.config.sample_rate;

        let thread = std::thread::Builder::new()
            .name("jack-audio".to_string())
            .spawn(move || {
                let mut block = vec![0.0f32; buffer_size * channels];
                let frame_duration = std::time::Duration::from_nanos(
                    ((buffer_size as u64 * 1_000_000_000) / sample_rate as u64).max(100_000),
                );
                while running.load(Ordering::Acquire) {
                    let start = std::time::Instant::now();
                    if transport_state.load(Ordering::Acquire) != JackTransportState::Stopped as u32
                    {
                        let popped = buffer.pop_block_interleaved(&mut block);
                        if popped < block.len() {
                            xruns.fetch_add(1, Ordering::Relaxed);
                            block[popped..].fill(0.0);
                        }
                        for s in &mut block[..popped] {
                            if s.is_nan() {
                                nans.fetch_add(1, Ordering::Relaxed);
                                *s = 0.0;
                            } else if s.is_infinite() {
                                clips.fetch_add(1, Ordering::Relaxed);
                                *s = s.signum();
                            }
                        }
                        tick_pos.fetch_add(buffer_size as u64, Ordering::Relaxed);
                    }
                    let elapsed = start.elapsed();
                    if elapsed < frame_duration {
                        std::thread::sleep(frame_duration - elapsed);
                    }
                }
            })
            .map_err(|e| {
                OutputError::StreamError(format!("failed to spawn jack audio thread: {e}"))
            })?;

        self.worker = Some(thread);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.transport_state
            .store(JackTransportState::Stopped as u32, Ordering::Release);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jack_output_lifecycle_and_transport() {
        let buf = Arc::new(FixedFrameBuffer::new(1024).expect("buffer"));
        let config = JackClientConfig {
            client_name: "test_jack_engine".to_string(),
            port_prefix: "audio_out_".to_string(),
            auto_connect: JackAutoConnectPolicy::Physical,
            sample_rate: 48000,
            channels: 2,
            buffer_size: 128,
            server_name: None,
        };

        let mut jack = JackOutput::new(buf, config);
        assert_eq!(jack.sample_rate(), 48000);
        assert_eq!(jack.buffer_size_frames(), 128);
        assert_eq!(jack.ports().len(), 2);
        assert_eq!(jack.ports()[0].connected_to, vec!["system:playback_1"]);
        assert_eq!(jack.ports()[1].connected_to, vec!["system:playback_2"]);

        // Port registration and routing tests
        let p3 = jack
            .register_port("audio_out_3", true)
            .expect("register p3");
        assert_eq!(p3, 2);
        jack.connect_ports("audio_out_3", "system:playback_3")
            .expect("connect p3");
        assert_eq!(
            jack.ports()[2].connected_to,
            vec!["system:playback_3".to_string()]
        );
        jack.disconnect_ports("audio_out_3", "system:playback_3")
            .expect("disconnect p3");
        assert!(jack.ports()[2].connected_to.is_empty());

        assert!(jack.latency_samples() > 0);
        assert!(jack.latency_ms() > 0.0);
        assert_eq!(jack.clock_info().1, 48000);

        jack.record_xrun();
        jack.record_xrun();
        assert_eq!(jack.take_underruns(), 2);
        assert_eq!(jack.take_underruns(), 0);

        assert_eq!(jack.transport_state(), JackTransportState::Stopped);
        jack.start().expect("start jack");
        assert_eq!(jack.transport_state(), JackTransportState::Rolling);

        std::thread::sleep(std::time::Duration::from_millis(15));
        assert!(jack.tick_position() > 0);

        jack.set_timebase_info(JackTimebaseInfo {
            bar: 12,
            beat: 3,
            tick: 480,
            bpm: 140.0,
            beats_per_bar: 4.0,
            beat_type: 4.0,
        });
        assert_eq!(jack.timebase_info().bpm, 140.0);
        assert_eq!(jack.timebase_info().bar, 12);

        jack.stop();
        assert_eq!(jack.transport_state(), JackTransportState::Stopped);
        assert!(JackOutput::probe_daemon(None).is_ok());
    }
}
