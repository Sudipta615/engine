//! Native ALSA PCM output backend (`cfg(target_os = "linux")`).
//!
//! Drives a direct `alsa::pcm::PCM` — the thing the cpal path cannot fully
//! express — for two purposes:
//!
//! 1. **Exclusive ALSA output** (`AudioBackend::ExclusiveAlsa`): opens `hw:` /
//!    `plughw:` device nodes. Only an exact `hw:` node bypasses the software
//!    conversion/mixer path by construction (it is the kernel's raw hardware
//!    contract, not a name-based heuristic), so only it can report
//!    `OutputAccessState { actual: DirectHw, verified: true }`. `plughw:` is
//!    retained for compatibility but is explicitly non-bit-perfect. Shared
//!    `default`/`sysdefault`/PulseAudio/PipeWire nodes are rejected for
//!    exclusive requests.
//! 2. **Native DSD transport** (§7): re-opens the same device with
//!    `SND_PCM_FORMAT_DSD_U8` (preferred) or DSD_U16/U32 and drains a
//!    [`DsdByteBuffer`] fed by the engine with raw 1-bit payload — never
//!    through the f32 pipeline.
//!
//! ## Realtime discipline
//!
//! The render threads (PCM and DSD) are plain loops over the lock-free
//! `FixedFrameBuffer` / `DsdByteBuffer`; `snd_pcm_writei` in blocking mode
//! provides natural backpressure, and `pcm.recover` handles underrun
//! (`EPIPE`) and stream-suspend (`ESTRPIPE`). The PCM handle is wrapped in
//! `Arc` so the owner can `drop()` it to unblock a pending write during
//! shutdown (alsa `PCM` is `Send` but not `Sync`; each thread owns its own
//! handle — same pattern as the WASAPI `SendCom` wrapper).
//!
//! ## Status
//!
//! Stereo first (2 channels), formats f32 → i32 → i16, DSD_U8 primary.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::thread::JoinHandle;

use alsa::pcm::{Access, Format as AlsaFormat, HwParams, PCM};
use alsa::{Direction, ValueOr};
use config::AudioBackend;

use crate::buffer::{DsdByteBuffer, FixedFrameBuffer};
use crate::decode::dsd::DsdWireFormat;
use crate::dsp::dither::DitherType;
use crate::dsp::pipeline::OutputSampleFormat;
use crate::output::capabilities::{OutputAccessMode, OutputAccessState, OutputCapabilities};
use crate::output::cpal_output::{OutputError, OutputVolume};
use crate::output::format_converter::{AudioFormatConverter, TargetFormat};
use crate::output::output::{NativeDsdParams, StreamErrorBatch, StreamErrorState};
use crate::output::output_info::OutputInfo;

/// `Arc<PCM>` wrapper with explicit Send+Sync, mirroring the WASAPI backend's
/// `SendCom` pattern.
///
/// alsa's `PCM` is `Send` but not `Sync` (it wraps a raw `snd_pcm_t*`), so
/// `Arc<PCM>` cannot cross threads even though sharing a handle between the
/// owner and one render thread is exactly the documented ALSA usage: the
/// render thread exclusively calls `io_*().writei()`, while the owner calls
/// `prepare()`/`drop()`/`recover()` (the C API allows `snd_pcm_drop` from
/// another thread to interrupt a blocked write — that is how `stop_render`
/// unblocks the render thread during shutdown).
#[derive(Clone)]
struct PcmArc(Arc<PCM>);

unsafe impl Send for PcmArc {}
unsafe impl Sync for PcmArc {}

impl std::ops::Deref for PcmArc {
    type Target = PCM;
    fn deref(&self) -> &PCM {
        &self.0
    }
}

/// Byte value written to the DAC during a DSD underrun to keep it clocked.
/// 0x69 = `0110 1001`, the conventional "silent" DSD pattern (LSB-first).
const DSD_SILENCE_BYTE: u8 = 0x69;

/// Preferred PCM sample containers, in order.
const PCM_FORMATS: &[AlsaFormat] = &[AlsaFormat::FloatLE, AlsaFormat::S32LE, AlsaFormat::S16LE];

/// Preferred native-DSD wire formats, in order.
const DSD_FORMATS: &[DsdWireFormat] = &[
    DsdWireFormat::U8,
    DsdWireFormat::U16Le,
    DsdWireFormat::U16Be,
    DsdWireFormat::U32Le,
    DsdWireFormat::U32Be,
];

/// ALSA sample container negotiated for PCM mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    F32,
    I32,
    I16,
}

impl Container {
    fn cpal(self) -> cpal::SampleFormat {
        match self {
            Self::F32 => cpal::SampleFormat::F32,
            Self::I32 => cpal::SampleFormat::I32,
            Self::I16 => cpal::SampleFormat::I16,
        }
    }

    fn output_sample_format(self) -> OutputSampleFormat {
        match self {
            Self::F32 => OutputSampleFormat::F32,
            Self::I32 => OutputSampleFormat::I32,
            Self::I16 => OutputSampleFormat::I16,
        }
    }
}

/// Native ALSA output backend.
pub struct AlsaOutput {
    device_name: String,
    backend: AudioBackend,
    requested_backend: AudioBackend,
    buffer: Arc<FixedFrameBuffer>,
    paused: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    underruns: Arc<AtomicU32>,
    clip_counter: Arc<AtomicU32>,
    nan_counter: Arc<AtomicU32>,
    stream_errors: StreamErrorState,
    dither_enabled: Arc<AtomicBool>,
    /// Live PCM handle (shared so the owner can `drop()` it to unblock the
    /// render thread's pending write during shutdown); `None` while stopped.
    pcm: Option<PcmArc>,
    /// Negotiated rate (PCM mode) or DSD frame rate (native-DSD mode).
    rate: u32,
    /// Last negotiated PCM rate, retained so native-DSD → PCM transitions
    /// never accidentally reopen a PCM stream at the DSD wire frame rate.
    pcm_rate: u32,
    channels: u16,
    container: Container,
    buffer_size_frames: u32,
    render_thread: Option<JoinHandle<()>>,
    is_fallback: bool,
    fallback_reason: Option<String>,
    // ── Native DSD state ─────────────────────────────────────────────────
    dsd_active: bool,
    dsd_wire_format: Option<DsdWireFormat>,
    dsd_buffer: Option<Arc<DsdByteBuffer>>,
}

impl AlsaOutput {
    /// Resolve the ALSA device name for an exclusive request: the target
    /// device if given (`hw:` is bit-perfect; `plughw:` is compatibility-only),
    /// else `hw:0`.
    fn resolve_device(target_device: Option<&str>) -> Result<String, OutputError> {
        match target_device {
            Some(name) if !name.is_empty() => {
                let lower = name.to_ascii_lowercase();
                if lower.starts_with("hw:") || lower.starts_with("plughw:") {
                    Ok(name.to_string())
                } else {
                    Err(OutputError::StreamOpen(format!(
                        "exclusive ALSA requires a hw:/plughw: device, got '{name}'"
                    )))
                }
            }
            _ => Ok("hw:0".to_string()),
        }
    }

    /// Open a PCM with exact (format, rate, channels) or the closest rate.
    fn open_pcm(
        device: &str,
        format: AlsaFormat,
        rate: u32,
        channels: u16,
    ) -> Result<PcmArc, OutputError> {
        let pcm = PCM::new(device, Direction::Playback, false)
            .map_err(|e| OutputError::StreamOpen(format!("PCM::new({device}): {e}")))?;
        {
            let hwp = HwParams::any(&pcm)
                .map_err(|e| OutputError::StreamOpen(format!("HwParams::any: {e}")))?;
            hwp.set_channels(u32::from(channels))
                .map_err(|e| OutputError::StreamOpen(format!("set_channels: {e}")))?;
            hwp.set_rate(rate, ValueOr::Nearest)
                .map_err(|e| OutputError::StreamOpen(format!("set_rate({rate}): {e}")))?;
            hwp.set_format(format)
                .map_err(|e| OutputError::StreamOpen(format!("set_format({format:?}): {e}")))?;
            hwp.set_access(Access::RWInterleaved)
                .map_err(|e| OutputError::StreamOpen(format!("set_access: {e}")))?;
            // Period = 1024 frames, buffer = 8 periods (≈ 21 ms at 48 kHz).
            let _ = hwp.set_period_size_near(1024, ValueOr::Nearest);
            let _ = hwp.set_buffer_size_near(8192);
            pcm.hw_params(&hwp)
                .map_err(|e| OutputError::StreamOpen(format!("hw_params: {e}")))?;
        }
        let actual_rate = {
            let hw = pcm
                .hw_params_current()
                .map_err(|e| OutputError::StreamOpen(format!("hw_params_current: {e}")))?;
            hw.get_rate()
                .map_err(|e| OutputError::StreamOpen(format!("get_rate: {e}")))?
        };
        if actual_rate != rate {
            log::warn!(
                "ALSA: device settled on {} Hz instead of requested {} Hz",
                actual_rate,
                rate
            );
        }
        pcm.prepare()
            .map_err(|e| OutputError::StreamOpen(format!("prepare: {e}")))?;
        Ok(PcmArc(Arc::new(pcm)))
    }

    /// Open the best supported PCM (format) at `rate` for `channels`.
    fn open_pcm_best(
        device: &str,
        rate: u32,
        channels: u16,
    ) -> Result<(PcmArc, Container), OutputError> {
        let mut last_err = None;
        for &format in PCM_FORMATS {
            match Self::open_pcm(device, format, rate, channels) {
                Ok(pcm) => {
                    let container = if format == AlsaFormat::FloatLE {
                        Container::F32
                    } else if format == AlsaFormat::S32LE {
                        Container::I32
                    } else {
                        Container::I16
                    };
                    return Ok((pcm, container));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| OutputError::StreamOpen("no supported PCM format".to_string())))
    }

    /// Open a native-DSD stream for `format` at the exact DSD frame rate.
    ///
    /// PCM devices may accept a nearby rate when `ValueOr::Nearest` is used,
    /// but that is not valid for raw DSD: one frame-rate error changes the
    /// bit clock and corrupts the transport. Query the settled hardware rate
    /// and fail closed when it is not exactly the negotiated DSD rate.
    fn open_pcm_dsd(
        device: &str,
        format: DsdWireFormat,
        bit_rate: u32,
        channels: u16,
    ) -> Result<(PcmArc, u32), OutputError> {
        let alsa_format = match format {
            DsdWireFormat::U8 => AlsaFormat::DSDU8,
            DsdWireFormat::U16Le => AlsaFormat::DSDU16LE,
            DsdWireFormat::U16Be => AlsaFormat::DSDU16BE,
            DsdWireFormat::U32Le => AlsaFormat::DSDU32LE,
            DsdWireFormat::U32Be => AlsaFormat::DSDU32BE,
        };
        let frame_rate = format.frame_rate_hz(bit_rate);
        let pcm = Self::open_pcm(device, alsa_format, frame_rate, channels)?;
        let (actual_rate, actual_channels) = {
            let hw = pcm
                .hw_params_current()
                .map_err(|e| OutputError::StreamOpen(format!("hw_params_current: {e}")))?;
            let actual_rate = hw
                .get_rate()
                .map_err(|e| OutputError::StreamOpen(format!("get_rate: {e}")))?;
            let actual_channels = hw
                .get_channels()
                .map_err(|e| OutputError::StreamOpen(format!("get_channels: {e}")))?;
            (actual_rate, actual_channels)
        };
        if actual_rate != frame_rate || actual_channels != u32::from(channels) {
            let _ = pcm.drop();
            return Err(OutputError::StreamOpen(format!(
                "native DSD {} requested {} Hz/{} ch but ALSA settled on {} Hz/{} ch",
                format.label(),
                frame_rate,
                channels,
                actual_rate,
                actual_channels
            )));
        }
        Ok((pcm, actual_rate))
    }

    pub fn new(
        buffer: Arc<FixedFrameBuffer>,
        backend: AudioBackend,
        target_device: Option<&str>,
        fallback_policy: config::FallbackPolicy,
    ) -> Result<Self, OutputError> {
        let device = Self::resolve_device(target_device)?;
        let (pcm, container) = Self::open_pcm_best(&device, 44_100, 2)?;
        let (actual_rate, buffer_size_frames) = {
            let hw = pcm
                .hw_params_current()
                .map_err(|e| OutputError::StreamOpen(format!("hw_params_current: {e}")))?;
            let rate = hw
                .get_rate()
                .map_err(|e| OutputError::StreamOpen(format!("get_rate: {e}")))?;
            let buffer = hw.get_buffer_size().map(|b| b as u32).unwrap_or(8192);
            (rate, buffer)
        };
        log::info!(
            "ALSA: opened {device} at {actual_rate} Hz, {:?}, 2 ch (buffer {buffer_size_frames} frames)",
            container
        );
        let _ = fallback_policy;
        Ok(Self {
            device_name: device,
            backend,
            requested_backend: backend,
            buffer,
            paused: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            underruns: Arc::new(AtomicU32::new(0)),
            clip_counter: Arc::new(AtomicU32::new(0)),
            nan_counter: Arc::new(AtomicU32::new(0)),
            stream_errors: StreamErrorState::default(),
            dither_enabled: Arc::new(AtomicBool::new(true)),
            pcm: Some(pcm),
            rate: actual_rate,
            pcm_rate: actual_rate,
            channels: 2,
            container,
            buffer_size_frames,
            render_thread: None,
            is_fallback: false,
            fallback_reason: None,
            dsd_active: false,
            dsd_wire_format: None,
            dsd_buffer: None,
        })
    }

    /// Start the PCM render thread.
    fn start_render(&mut self) -> Result<(), OutputError> {
        if self.render_thread.is_some() {
            return Ok(());
        }
        self.running.store(true, Ordering::Release);
        let buffer = Arc::clone(&self.buffer);
        let paused = Arc::clone(&self.paused);
        let running = Arc::clone(&self.running);
        let underruns = Arc::clone(&self.underruns);
        let clip_counter = Arc::clone(&self.clip_counter);
        let nan_counter = Arc::clone(&self.nan_counter);
        let channels = self.channels as usize;
        let container = self.container;
        let dither_enabled = Arc::clone(&self.dither_enabled);
        // The thread owns its own handle; the struct keeps one to `drop()`
        // the stream and unblock a pending write during shutdown.
        let pcm = self.pcm.clone().expect("ALSA PCM present");
        let _ = pcm.prepare();

        let handle = std::thread::Builder::new()
            .name("alsa-render".to_string())
            .spawn(move || {
                let mut converter = AudioFormatConverter::new(
                    match container {
                        Container::F32 => TargetFormat::F32,
                        Container::I32 => TargetFormat::I32,
                        Container::I16 => TargetFormat::I16,
                    },
                    DitherType::Triangular,
                );
                let period_frames = 1024usize;
                let mut scratch = vec![0.0f32; period_frames * channels];
                // Integer conversion buffers are allocated once per render
                // thread, not once per period. The render loop is a realtime
                // boundary and must remain allocation-free after startup.
                let mut int_scratch = vec![0i32; period_frames * channels];
                let mut short_scratch = vec![0i16; period_frames * channels];
                while running.load(Ordering::Acquire) {
                    if paused.load(Ordering::Acquire) {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    let n_samples = buffer.pop_block_interleaved(&mut scratch);
                    let frames = n_samples / channels;
                    let write_frames = if frames > 0 {
                        frames
                    } else {
                        // Underrun: deliver silence to keep the DAC clocked.
                        underruns.fetch_add(1, Ordering::Relaxed);
                        scratch[..period_frames * channels].fill(0.0);
                        period_frames
                    };
                    let data = &scratch[..write_frames * channels];
                    let result = match container {
                        Container::F32 => pcm.io_f32().expect("io_f32").writei(data),
                        Container::I32 => {
                            converter.set_dither_enabled(dither_enabled.load(Ordering::Relaxed));
                            for (i, &s) in data.iter().enumerate() {
                                let v = converter.convert_mono_to_i32(s);
                                int_scratch[i] = v;
                                if !s.is_finite() {
                                    nan_counter.fetch_add(1, Ordering::Relaxed);
                                } else if s.abs() > 1.0 {
                                    clip_counter.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            pcm.io_i32()
                                .expect("io_i32")
                                .writei(&int_scratch[..data.len()])
                        }
                        Container::I16 => {
                            converter.set_dither_enabled(dither_enabled.load(Ordering::Relaxed));
                            for (i, &s) in data.iter().enumerate() {
                                let v = converter.convert_mono_to_i16(s);
                                short_scratch[i] = v;
                                if !s.is_finite() {
                                    nan_counter.fetch_add(1, Ordering::Relaxed);
                                } else if s.abs() > 1.0 {
                                    clip_counter.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            pcm.io_i16()
                                .expect("io_i16")
                                .writei(&short_scratch[..data.len()])
                        }
                    };
                    if let Err(e) = result {
                        // EPIPE (underrun) or ESTRPIPE (suspend): recover.
                        underruns.fetch_add(1, Ordering::Relaxed);
                        let _ = pcm.recover(e.errno(), true);
                    }
                }
            })
            .map_err(|e| OutputError::StreamOpen(format!("spawn alsa-render: {e}")))?;
        self.render_thread = Some(handle);
        Ok(())
    }

    /// Start the native-DSD render thread.
    fn start_dsd_render(&mut self) -> Result<(), OutputError> {
        if self.render_thread.is_some() {
            return Ok(());
        }
        let Some(format) = self.dsd_wire_format else {
            return Err(OutputError::StreamError(
                "native DSD mode has no negotiated wire format".to_string(),
            ));
        };
        let Some(dsd_buffer) = self.dsd_buffer.clone() else {
            return Err(OutputError::StreamError(
                "native DSD mode has no byte buffer".to_string(),
            ));
        };
        self.running.store(true, Ordering::Release);
        let running = Arc::clone(&self.running);
        let paused = Arc::clone(&self.paused);
        let underruns = Arc::clone(&self.underruns);
        let frame_width = format.bytes_per_word() * self.channels as usize;
        let pcm = self.pcm.clone().expect("ALSA PCM present");
        let _ = pcm.prepare();

        let handle = std::thread::Builder::new()
            .name("alsa-dsd-render".to_string())
            .spawn(move || {
                let io = pcm.io_bytes();
                let period_frames = 1024usize;
                let mut scratch = vec![0u8; period_frames * frame_width];
                while running.load(Ordering::Acquire) {
                    if paused.load(Ordering::Acquire) {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    let available = dsd_buffer.available_bytes();
                    if available >= frame_width {
                        let want =
                            available.min(scratch.len()).div_euclid(frame_width) * frame_width;
                        let frames = dsd_buffer.pop_frames(&mut scratch[..want], frame_width);
                        let bytes = frames * frame_width;
                        if let Err(e) = io.writei(&scratch[..bytes]) {
                            underruns.fetch_add(1, Ordering::Relaxed);
                            let _ = pcm.recover(e.errno(), true);
                        }
                    } else {
                        // DSD underrun: feed the silent pattern so the DAC
                        // stays clocked (no clicks from a stopped bitstream).
                        underruns.fetch_add(1, Ordering::Relaxed);
                        scratch.fill(DSD_SILENCE_BYTE);
                        if let Err(e) = io.writei(&scratch) {
                            let _ = pcm.recover(e.errno(), true);
                        }
                    }
                }
            })
            .map_err(|e| OutputError::StreamOpen(format!("spawn alsa-dsd-render: {e}")))?;
        self.render_thread = Some(handle);
        Ok(())
    }

    fn stop_render(&mut self) {
        self.running.store(false, Ordering::Release);
        // Drop the stream to unblock any pending writei in the render thread.
        if let Some(ref pcm) = self.pcm {
            let _ = pcm.drop();
        }
        if let Some(handle) = self.render_thread.take() {
            let _ = handle.join();
        }
    }

    /// Re-open the device at `rate` with the best PCM container.
    fn reopen_pcm(&mut self, rate: u32, channels: u16) -> Result<u32, OutputError> {
        // A direct ALSA device cannot have two handles open at once. Stop the
        // render loop and release the old handle before opening the replacement
        // (especially important for native-DSD ↔ PCM transitions and track
        // changes).
        self.stop_render();
        self.pcm = None;
        let device = self.device_name.clone();
        let (pcm, container) = Self::open_pcm_best(&device, rate, channels)?;
        let actual_rate = {
            let hw = pcm
                .hw_params_current()
                .map_err(|e| OutputError::StreamOpen(format!("hw_params_current: {e}")))?;
            hw.get_rate()
                .map_err(|e| OutputError::StreamOpen(format!("get_rate: {e}")))?
        };
        self.pcm = Some(pcm);
        self.container = container;
        self.rate = actual_rate;
        self.pcm_rate = actual_rate;
        self.channels = channels;
        self.dsd_active = false;
        self.dsd_wire_format = None;
        self.dsd_buffer = None;
        Ok(actual_rate)
    }
}

impl Drop for AlsaOutput {
    fn drop(&mut self) {
        self.stop_render();
        self.pcm = None;
    }
}

impl OutputVolume for AlsaOutput {
    fn supports_hardware_volume(&self) -> bool {
        // ALSA hw: devices expose volume through the mixer; this backend
        // leaves hardware-volume routing to the existing cpal mixer path.
        false
    }

    fn set_hardware_volume_db(&self, _db: f32) -> Result<(), OutputError> {
        Err(OutputError::StreamError(
            "hardware volume not exposed by the native ALSA backend".to_string(),
        ))
    }

    fn take_external_volume_change(&self) -> Option<f32> {
        None
    }
}

impl super::output::Output for AlsaOutput {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn sample_format(&self) -> cpal::SampleFormat {
        self.container.cpal()
    }

    fn buffer_size_frames(&self) -> u32 {
        self.buffer_size_frames
    }

    fn output_info(&self) -> OutputInfo {
        let direct = self.backend == AudioBackend::ExclusiveAlsa
            && self.device_name.to_ascii_lowercase().starts_with("hw:");
        // Only an exact `hw:` node is verified direct hardware access.
        // `plughw:` inserts ALSA's conversion plugin and therefore cannot
        // support a bit-perfect verdict or trustworthy native DSD transport.
        let verified = direct && !self.is_fallback;
        let actual_access = if self.dsd_active && verified {
            OutputAccessMode::BitstreamPassthrough
        } else if direct {
            OutputAccessMode::DirectHw
        } else {
            OutputAccessMode::Shared
        };
        let access_state = OutputAccessState {
            requested: if self.backend == AudioBackend::ExclusiveAlsa {
                OutputAccessMode::Exclusive
            } else {
                OutputAccessMode::Shared
            },
            actual: actual_access,
            verified,
        };
        OutputInfo {
            requested_backend: Some(self.requested_backend),
            actual_backend: Some(self.backend),
            requested_rate: self.rate,
            actual_rate: self.rate,
            channels: self.channels,
            buffer_size_frames: self.buffer_size_frames,
            buffer_size_estimated: false,
            sample_format: if self.dsd_active {
                OutputSampleFormat::Unknown
            } else {
                self.container.output_sample_format()
            },
            dither_enabled: self.dither_enabled.load(Ordering::Relaxed),
            access_mode: actual_access,
            access_state,
            is_fallback: self.is_fallback,
            fallback_reason: self.fallback_reason.clone(),
            is_exclusive: verified,
            device_name: self.device_name.clone(),
        }
    }

    fn capabilities(&self) -> OutputCapabilities {
        let direct = self.backend == AudioBackend::ExclusiveAlsa
            && self.device_name.to_ascii_lowercase().starts_with("hw:");
        let verified = direct && !self.is_fallback;
        OutputCapabilities {
            sample_rates: vec![self.rate],
            hardware_ranges: Vec::new(),
            formats: vec![self.container.cpal()],
            channels: vec![self.channels],
            device_name: self.device_name.clone(),
            access_mode: if self.dsd_active && verified {
                OutputAccessMode::BitstreamPassthrough
            } else if verified {
                OutputAccessMode::DirectHw
            } else {
                OutputAccessMode::Shared
            },
            access_state: OutputAccessState {
                requested: OutputAccessMode::Exclusive,
                actual: if self.dsd_active && verified {
                    OutputAccessMode::BitstreamPassthrough
                } else if verified {
                    OutputAccessMode::DirectHw
                } else {
                    OutputAccessMode::Shared
                },
                verified,
            },
            likely_direct_access: direct,
            supports_exclusive: direct,
        }
    }

    fn device_name(&self) -> String {
        self.device_name.clone()
    }

    /// The ALSA device name (`hw:0,0` / `plughw:...`) is the stable
    /// card+device identifier used to open the stream — it is already a
    /// stable ID, so the device_id equals the name.
    fn device_id(&self) -> Option<String> {
        Some(self.device_name.clone())
    }

    fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError> {
        if self.dsd_active {
            return Err(OutputError::StreamError(
                "cannot reconfigure a native-DSD stream to a PCM rate".to_string(),
            ));
        }
        if target_sample_rate == self.rate {
            return Ok(target_sample_rate);
        }
        self.stop_render();
        self.reopen_pcm(target_sample_rate, self.channels)?;
        self.start_render()?;
        Ok(self.rate)
    }

    fn reconfigure_sample_format(
        &mut self,
        target_sample_rate: u32,
        sample_format: cpal::SampleFormat,
    ) -> Result<u32, OutputError> {
        if self.dsd_active {
            return Err(OutputError::StreamError(
                "cannot reconfigure a native-DSD stream to a PCM format".to_string(),
            ));
        }
        let (alsa_format, container) = match sample_format {
            cpal::SampleFormat::F32 => (AlsaFormat::FloatLE, Container::F32),
            cpal::SampleFormat::I32 => (AlsaFormat::S32LE, Container::I32),
            cpal::SampleFormat::I16 => (AlsaFormat::S16LE, Container::I16),
            _ => return Err(OutputError::UnsupportedFormat),
        };
        if target_sample_rate == self.rate && container == self.container {
            if self.render_thread.is_none() {
                self.start_render()?;
            }
            return Ok(self.rate);
        }

        self.stop_render();
        // Release the previous direct handle before opening a new format;
        // `hw:` devices reject a second simultaneous PCM handle.
        self.pcm = None;
        let device = self.device_name.clone();
        let pcm = Self::open_pcm(&device, alsa_format, target_sample_rate, self.channels)?;
        let actual_rate = pcm
            .hw_params_current()
            .map_err(|e| OutputError::StreamOpen(format!("hw_params_current: {e}")))?
            .get_rate()
            .map_err(|e| OutputError::StreamOpen(format!("get_rate: {e}")))?;
        self.pcm = Some(pcm);
        self.container = container;
        self.rate = actual_rate;
        self.pcm_rate = actual_rate;
        self.dsd_active = false;
        self.dsd_wire_format = None;
        self.dsd_buffer = None;
        self.start_render()?;
        Ok(actual_rate)
    }

    fn reset_buffer(&self) {
        self.buffer.reset();
    }

    fn take_underruns(&self) -> u32 {
        self.underruns.swap(0, Ordering::AcqRel)
    }

    fn take_clips(&self) -> u32 {
        self.clip_counter.swap(0, Ordering::AcqRel)
    }

    fn take_nans(&self) -> u32 {
        self.nan_counter.swap(0, Ordering::AcqRel)
    }

    fn take_stream_errors(&self) -> StreamErrorBatch {
        self.stream_errors.take()
    }

    fn set_dither_enabled(&self, enabled: bool) {
        self.dither_enabled.store(enabled, Ordering::Release);
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    fn start(&mut self) -> Result<(), OutputError> {
        self.start_render()
    }

    fn stop(&mut self) {
        self.stop_render();
    }

    fn native_dsd_capabilities(&self) -> Vec<DsdWireFormat> {
        if self.backend == AudioBackend::ExclusiveAlsa {
            // The current PCM handle already owns an exclusive hw: node, so
            // probing by opening a second handle would report false negatives.
            // `set_native_dsd` performs the authoritative exact open after
            // stopping this stream.
            DSD_FORMATS.to_vec()
        } else {
            Vec::new()
        }
    }

    fn native_dsd_capability_matrix(&self) -> Vec<crate::output::NativeDsdCapability> {
        if self.backend != AudioBackend::ExclusiveAlsa {
            return Vec::new();
        }
        // The active PCM handle cannot be probed for every DSD rate without
        // releasing the user's stream. Return format-level candidates and let
        // `set_native_dsd` perform the authoritative exact rate/channel open.
        // Empty axes mean "unknown until negotiated", not "all rates work".
        DSD_FORMATS
            .iter()
            .copied()
            .map(|wire_format| crate::output::NativeDsdCapability {
                wire_format,
                bit_rates: Vec::new(),
                channels: Vec::new(),
            })
            .collect()
    }

    fn set_native_dsd(
        &mut self,
        params: Option<NativeDsdParams>,
    ) -> Result<Option<DsdWireFormat>, OutputError> {
        match params {
            None => {
                if self.dsd_active {
                    self.stop_render();
                    // `self.rate` is the native DSD wire frame rate while
                    // DSD is active; restore the retained PCM rate instead.
                    let rate = self.pcm_rate;
                    self.reopen_pcm(rate, self.channels)?;
                    self.start_render()?;
                }
                Ok(None)
            }
            Some(params) => {
                if self.backend != AudioBackend::ExclusiveAlsa {
                    return Err(OutputError::StreamError(format!(
                        "native DSD requires AudioBackend::ExclusiveAlsa with a direct hw: device \
                         (current backend: {:?})",
                        self.backend
                    )));
                }
                if !self.device_name.to_ascii_lowercase().starts_with("hw:") {
                    return Err(OutputError::StreamError(
                        "native DSD requires an exact ALSA hw: node; plughw: conversion is not bit-perfect"
                            .to_string(),
                    ));
                }
                self.stop_render();
                // Release the previous direct PCM handle before probing DSD
                // formats. Keeping it alive makes a real `hw:` endpoint
                // report busy and breaks DSD→DSD track transitions.
                self.pcm = None;
                // Try the requested format first, then the full preference
                // list; the first that opens wins.
                let mut order: Vec<DsdWireFormat> = Vec::with_capacity(DSD_FORMATS.len() + 1);
                order.push(params.wire_format);
                order.extend(
                    DSD_FORMATS
                        .iter()
                        .copied()
                        .filter(|&f| f != params.wire_format),
                );
                let mut last_err = None;
                let device = self.device_name.clone();
                for format in order {
                    match Self::open_pcm_dsd(&device, format, params.bit_rate, params.channels) {
                        Ok((pcm, frame_rate)) => {
                            log::info!(
                                "ALSA: native DSD negotiated {} at {} Hz (bit rate {})",
                                format.label(),
                                frame_rate,
                                params.bit_rate
                            );
                            self.pcm = Some(pcm);
                            self.rate = frame_rate;
                            self.channels = params.channels;
                            self.dsd_active = true;
                            self.dsd_wire_format = Some(format);
                            self.dsd_buffer = Some(params.buffer);
                            self.start_dsd_render()?;
                            return Ok(Some(format));
                        }
                        Err(e) => last_err = Some(e),
                    }
                }
                Err(last_err.unwrap_or_else(|| {
                    OutputError::StreamError(
                        "no native DSD format could be opened on this device".to_string(),
                    )
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_device_rejects_shared_names_for_exclusive() {
        assert!(AlsaOutput::resolve_device(Some("default")).is_err());
        assert!(AlsaOutput::resolve_device(Some("sysdefault")).is_err());
        assert!(AlsaOutput::resolve_device(Some("hw:1,0")).is_ok());
        assert!(AlsaOutput::resolve_device(Some("plughw:2")).is_ok());
        assert!(AlsaOutput::resolve_device(Some("")).is_ok()); // falls back to hw:0
    }

    #[test]
    fn test_dsd_frame_rate_math() {
        // DSD64 through DSD_U8: 2.8224 MHz / 8 = 352.8 kHz frame rate.
        assert_eq!(DsdWireFormat::U8.frame_rate_hz(2_822_400), 352_800);
        assert_eq!(DsdWireFormat::U16Be.frame_rate_hz(2_822_400), 176_400);
        assert_eq!(DsdWireFormat::U32Le.frame_rate_hz(2_822_400), 88_200);
    }
}
