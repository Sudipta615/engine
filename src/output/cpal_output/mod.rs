//! Audio output using cpal
//!
//! The output callback is designed to be zero-allocation, zero-blocking.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use config::{AudioBackend, FallbackPolicy};
use cpal::{
    traits::{DeviceTrait, HostTrait},
    Device, SampleFormat, StreamConfig,
};
use thiserror::Error;

use crate::buffer::FixedFrameBuffer;
use crate::output::output::{StreamErrorBatch, StreamErrorState};
use crate::output::output_info::OutputInfo;

mod reconfigure;
mod stream_owner;
#[cfg(test)]
mod tests;
mod volume;

use crate::output::device_match::{classify_device_name_match, DeviceNameMatch};
use reconfigure::{renegotiate_reconfigure_config, select_reconfigure_config, SupportedConfig};
#[cfg(target_os = "macos")]
use volume::coreaudio;
#[cfg(target_os = "linux")]
use volume::{alsa_hardware_volume_supported, set_alsa_volume_db};

#[derive(Debug, Error)]
pub enum OutputError {
    #[error("No audio device available")]
    NoDevice,
    #[error("Failed to open stream: {0}")]
    StreamOpen(String),
    #[error("Unsupported sample format")]
    UnsupportedFormat,
    #[error("Buffer underrun")]
    Underrun,
    #[error("Stream error: {0}")]
    StreamError(String),
}

/// Trait for hardware endpoint volume control.
///
/// ## Implementation contract
///
/// - `supports_hardware_volume()` must return `true` **only** when
///   `set_hardware_volume_db()` will actually change the DAC/mixer level.
/// - **Linux ALSA `hw:`**: uses `amixer sset Master <pct>%`. Returns `true`
///   only when `amixer` is found in `PATH`.
/// - **WASAPI**: implemented by the native `wasapi-native` backend on Windows
///   via `IAudioEndpointVolume` (with an external-change notification
///   callback). The cpal WASAPI path below does **not** implement it.
/// - **CoreAudio (macOS)**: sets the default output device's virtual
///   main/master volume via `AudioObjectSetPropertyData`. Returns `true` when
///   the device exposes that control.
/// - **ASIO**: not implemented in this build. Returns `false` / `Err`; an
///   ASIO channel-gain backend is required.
/// - Callers that route volume through `HardwarePreferred` mode must check
///   `supports_hardware_volume()` before assuming the call is effective.
pub trait OutputVolume {
    /// Whether the active output stream/device supports native hardware volume
    /// control in this build. Returns `false` when hardware volume is not
    /// implemented for the current platform/backend.
    fn supports_hardware_volume(&self) -> bool;
    /// Set hardware endpoint volume in dB ([-60.0, 0.0] dB).
    ///
    /// Returns `Err` if hardware volume is not supported on this platform/backend.
    fn set_hardware_volume_db(&self, db: f32) -> Result<(), OutputError>;

    /// Take the most recent hardware-endpoint volume level (linear 0.0–1.0)
    /// observed via the endpoint's change-notification callback — the OS
    /// volume slider, a hardware knob, or programmatic sets from any
    /// process — if a change was reported since the last call.
    ///
    /// Returns `None` when the backend cannot observe external changes
    /// (the cpal backends) or nothing changed since the last read. The
    /// engine folds a `Some` value into `PlaybackInfo.volume` in
    /// `HardwarePreferred` mode so the displayed volume tracks the real
    /// hardware level. Backends that register a notification callback
    /// (the native WASAPI output) override this; the default is `None`.
    fn take_external_volume_change(&self) -> Option<f32> {
        None
    }
}

/// Commands handled by the thread that owns a CPAL stream. CPAL deliberately
/// does not promise that `Stream` is portable across threads, so the stream
/// never leaves this owner thread and no Send/Sync assertion is required.
enum CpalStreamCommand {
    Pause,
    Resume,
    Stop,
}

struct CpalStreamHandle {
    commands: crossbeam::channel::Sender<CpalStreamCommand>,
    owner: Option<std::thread::JoinHandle<()>>,
}

impl CpalStreamHandle {
    fn send(&self, command: CpalStreamCommand) {
        let _ = self.commands.send(command);
    }

    fn stop(mut self) {
        self.send(CpalStreamCommand::Stop);
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

/// The callback kernel selected during stream negotiation. CPAL requires a
/// concrete callback sample type, so this plan is resolved once when the
/// stream is opened; the callback itself never performs format dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpalFormatPlan {
    F32,
    F64,
    I16,
    U16,
    I32,
    Unsupported,
}

impl CpalFormatPlan {
    fn from_sample_format(format: SampleFormat) -> Self {
        match format {
            SampleFormat::F32 => Self::F32,
            SampleFormat::F64 => Self::F64,
            SampleFormat::I16 => Self::I16,
            SampleFormat::U16 => Self::U16,
            SampleFormat::I32 => Self::I32,
            _ => Self::Unsupported,
        }
    }
}

/// Audio output using cpal
pub struct CpalOutput {
    stream: Option<CpalStreamHandle>,
    device: Device,
    /// Resolved stream config (sample rate, channels, buffer size)
    stream_config: StreamConfig,
    /// The callback buffer size in frames: the actual `Fixed(n)` value when
    /// the device reported a range, or the per-backend target when the device
    /// reported an unknown buffer size (in which case
    /// [`Self::buffer_size_estimated`] is `true` and the value is only an
    /// estimate for the graph latency model).
    buffer_size_frames: u32,
    /// Whether `buffer_size_frames` is an estimate (cpal reported
    /// `SupportedBufferSize::Unknown`) rather than a negotiated size.
    buffer_size_estimated: bool,
    /// Sample format for the output stream
    sample_format: SampleFormat,
    /// Concrete callback/conversion kernel chosen at negotiation time.
    format_plan: CpalFormatPlan,
    /// Shared buffer between DSP thread and output callback
    buffer: Arc<FixedFrameBuffer>,
    /// Flag to pause output
    paused: Arc<AtomicBool>,
    /// Flag indicating if the audio thread is inside the callback
    in_callback: Arc<AtomicBool>,
    /// Underruns counter
    underruns: Arc<AtomicU32>,
    /// Sample rate of the output device
    actual_sample_rate: u32,
    /// Lock-free diagnostic mailbox for backend stream failures.
    stream_errors: StreamErrorState,
    /// Whether TPDF dither should be applied at the integer-quantization
    /// boundary. Consulted by the i16/u16 audio callbacks. For f32 output
    /// this flag has no effect (no quantization happens).
    dither_enabled: Arc<AtomicBool>,
    /// Counter for samples that exceeded ±1.0 and were clamped. Read with
    /// `take_clips()` (resets to 0 on read).
    clip_counter: Arc<AtomicU32>,
    /// Counter for non-finite (NaN/Inf) samples encountered in the output
    /// callback. Read with `take_nans()` (resets to 0 on read).
    nan_counter: Arc<AtomicU32>,
    /// Hardware endpoint volume setting in dB (stored as f32 bits) for
    /// future read-back; currently only written.
    #[allow(dead_code)]
    hardware_volume_db: Arc<AtomicU32>,
    backend: AudioBackend,
    target_device: Option<String>,
    requested_backend: AudioBackend,
    is_fallback: bool,
    fallback_reason: Option<String>,
    fallback_policy: FallbackPolicy,
}

impl CpalOutput {
    /// Enumerate available output device names for a given audio backend
    pub fn enumerate_devices(backend: AudioBackend) -> Vec<String> {
        super::cpal_devices::enumerate_devices(backend)
    }

    /// Create a new cpal output with automatic fallback (Allow policy by default).
    pub fn new(
        buffer: Arc<FixedFrameBuffer>,
        backend: AudioBackend,
        target_device: Option<&str>,
    ) -> Result<Self, OutputError> {
        Self::new_with_policy(buffer, backend, target_device, FallbackPolicy::Allow)
    }

    /// Create a new cpal output with explicit fallback policy.
    pub fn new_with_policy(
        buffer: Arc<FixedFrameBuffer>,
        backend: AudioBackend,
        target_device: Option<&str>,
        fallback_policy: FallbackPolicy,
    ) -> Result<Self, OutputError> {
        match Self::new_raw(buffer.clone(), backend, target_device, fallback_policy) {
            Ok(output) => Ok(output),
            Err(e) => {
                if fallback_policy == FallbackPolicy::Strict {
                    return Err(e);
                }
                let is_custom = backend != AudioBackend::Auto
                    || target_device.is_some_and(|d| !d.is_empty() && d != "Default / Automatic");
                if is_custom {
                    log::warn!(
                        "Audio output: Exclusive mode or target device {:?} failed during init ({}); falling back to default shared device (`Auto`).",
                        target_device,
                        e
                    );
                    let mut output =
                        Self::new_raw(buffer, AudioBackend::Auto, None, fallback_policy)?;
                    output.requested_backend = backend;
                    output.is_fallback = true;
                    output.fallback_reason = Some(e.to_string());
                    Ok(output)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn new_raw(
        buffer: Arc<FixedFrameBuffer>,
        backend: AudioBackend,
        target_device: Option<&str>,
        fallback_policy: FallbackPolicy,
    ) -> Result<Self, OutputError> {
        // Do not open a shared cpal stream while claiming an exclusive
        // product mode. Unsupported backend requests are surfaced to the
        // caller so `FallbackPolicy::Allow` can explicitly mark the switch
        // to Auto/shared, while Strict mode fails closed.
        if backend == AudioBackend::ExclusiveWasapi {
            return Err(OutputError::StreamOpen(
                "cpal WASAPI streams are shared-mode only; native WASAPI is required for exclusive output"
                    .to_string(),
            ));
        }
        if backend == AudioBackend::ExclusiveCoreAudioHog {
            return Err(OutputError::StreamOpen(
                "cpal does not implement CoreAudio hog mode; a native backend is required"
                    .to_string(),
            ));
        }
        #[cfg(not(target_os = "linux"))]
        if backend == AudioBackend::ExclusiveAlsa {
            return Err(OutputError::StreamOpen(
                "exclusive ALSA output is only available on Linux".to_string(),
            ));
        }
        #[cfg(not(all(target_os = "windows", feature = "asio")))]
        if backend == AudioBackend::ExclusiveAsio {
            return Err(OutputError::StreamOpen(
                "ASIO output is unavailable because the Windows ASIO feature is not enabled"
                    .to_string(),
            ));
        }

        let host = match backend {
            #[cfg(target_os = "linux")]
            AudioBackend::ExclusiveAlsa => {
                log::info!("Audio output: Requesting exclusive ALSA host");
                cpal::host_from_id(cpal::HostId::Alsa).unwrap_or_else(|_| cpal::default_host())
            }
            #[cfg(target_os = "windows")]
            AudioBackend::ExclusiveWasapi => {
                // cpal's WASAPI backend initializes every IAudioClient in
                // AUDCLNT_SHAREMODE_SHARED (verified against cpal 0.15
                // source); there is no way to request
                // AUDCLNT_SHAREMODE_EXCLUSIVE through its stream API. This
                // is a host selection, NOT exclusive mode — the OS mixer
                // remains in the signal path. A native IAudioClient backend
                // is required for real WASAPI exclusive access.
                log::warn!(
                    "Audio output: WASAPI exclusive mode is not available through cpal \
                     (cpal opens every IAudioClient in shared mode); the stream will run \
                     through the shared WASAPI mixer."
                );
                cpal::host_from_id(cpal::HostId::Wasapi).unwrap_or_else(|_| cpal::default_host())
            }
            #[cfg(all(target_os = "windows", feature = "asio"))]
            AudioBackend::ExclusiveAsio => {
                log::info!("Audio output: Requesting exclusive ASIO host");
                cpal::host_from_id(cpal::HostId::Asio).unwrap_or_else(|_| cpal::default_host())
            }
            #[cfg(all(target_os = "windows", not(feature = "asio")))]
            AudioBackend::ExclusiveAsio => {
                log::warn!(
                    "Audio output: ASIO support not compiled in; falling back to default host"
                );
                cpal::default_host()
            }
            #[cfg(target_os = "macos")]
            AudioBackend::ExclusiveCoreAudioHog => {
                // cpal's CoreAudio backend does not implement hog mode
                // (AudioHardwareSetProperty); this selection only picks the
                // default CoreAudio host, and the stream runs through the
                // shared CoreAudio mixer. A native hog-mode implementation
                // is required for real exclusivity.
                log::warn!(
                    "Audio output: CoreAudio Hog Mode is not implemented by cpal; \
                     the stream will run through the shared CoreAudio mixer."
                );
                cpal::default_host() // CoreAudio is the default on macOS
            }
            _ => cpal::default_host(),
        };

        #[allow(unused_mut)]
        let mut device = None;

        if let Some(target_name) = target_device {
            if !target_name.is_empty() && target_name != "Default / Automatic" {
                if let Ok(devices) = host.output_devices() {
                    // Prefer the most precise match so a substring never
                    // shadows an exact match when several endpoints share
                    // similar names. cpal only exposes the device *name*
                    // (no stable endpoint ID), so exact-then-substring is the
                    // most deterministic selection available here; the native
                    // WASAPI backend additionally matches the endpoint ID.
                    let mut best: Option<(DeviceNameMatch, Device, String)> = None;
                    for d in devices {
                        if let Ok(desc) = d.description() {
                            let name = desc.name().to_string();
                            if let Some(m) = classify_device_name_match(target_name, &name) {
                                let better = match &best {
                                    Some((cur, _, _)) => m < *cur,
                                    None => true,
                                };
                                if better {
                                    best = Some((m, d, name));
                                }
                            }
                        }
                    }
                    if let Some((m, d, name)) = best {
                        if m == DeviceNameMatch::Substring {
                            log::warn!(
                                "Audio output: target device '{}' matched '{}' by substring only; \
                                 the name is ambiguous across endpoints — prefer an exact device name",
                                target_name,
                                name
                            );
                        }
                        log::info!("Audio output: Selected target device: {}", name);
                        device = Some(d);
                    }
                }
                if device.is_none() {
                    log::warn!(
                        "Target audio device '{}' not found on host; falling back to automatic device selection",
                        target_name
                    );
                }
            }
        }

        // If ALSA was requested and no specific target device found/selected, try to find a hardware device rather than 'default'
        if device.is_none() && backend == AudioBackend::ExclusiveAlsa {
            #[cfg(target_os = "linux")]
            if let Ok(devices) = host.output_devices() {
                let mut valid_devices: Vec<_> = devices
                    .filter(|d| {
                        let name = d
                            .description()
                            .map(|desc| desc.name().to_lowercase())
                            .unwrap_or_default();
                        name != "default"
                            && !name.starts_with("sysdefault")
                            && !name.contains("pulse")
                            && !name.contains("pipewire")
                            && !name.contains("dmix")
                    })
                    .collect();

                valid_devices.sort_by_key(|d| {
                    if d.description()
                        .map(|desc| desc.name().to_lowercase())
                        .unwrap_or_default()
                        .contains("analog")
                    {
                        0
                    } else {
                        1
                    }
                });

                if let Some(hw_dev) = valid_devices.into_iter().next() {
                    log::info!(
                        "Audio output: Selected exclusive hardware device: {}",
                        hw_dev
                            .description()
                            .map(|desc| desc.name().to_string())
                            .unwrap_or_default()
                    );
                    device = Some(hw_dev);
                }
            }
        }

        let device = device
            .or_else(|| host.default_output_device())
            .ok_or(OutputError::NoDevice)?;

        #[cfg(target_os = "linux")]
        if backend == AudioBackend::ExclusiveAlsa {
            let name = device
                .description()
                .map(|desc| desc.name().to_lowercase())
                .unwrap_or_default();
            if !name.starts_with("hw:") && !name.starts_with("plughw:") {
                return Err(OutputError::StreamOpen(format!(
                    "exclusive ALSA requires a direct hw:/plughw: device, got '{}'",
                    name
                )));
            }
        }

        // Use the device's default config instead of max-sample-rate.
        let default_config = device
            .default_output_config()
            .map_err(|e| OutputError::StreamOpen(format!("Cannot get default config: {}", e)))?;

        let target_sample_rate = default_config.sample_rate();

        let supported = device
            .supported_output_configs()
            .map_err(|e| OutputError::StreamOpen(format!("Cannot query configs: {}", e)))?;
        let supported_configs: Vec<_> = supported.collect();

        let native_fmt = default_config.sample_format();
        let mut format_priority = vec![native_fmt];
        for fmt in [
            SampleFormat::F32,
            SampleFormat::I32,
            SampleFormat::I16,
            SampleFormat::U16,
            SampleFormat::F64,
        ] {
            if !format_priority.contains(&fmt) {
                format_priority.push(fmt);
            }
        }

        let config = format_priority
            .iter()
            .find_map(|&fmt| {
                supported_configs
                    .iter()
                    .find(|c| {
                        c.sample_format() == fmt
                            && c.min_sample_rate() <= target_sample_rate
                            && c.max_sample_rate() >= target_sample_rate
                    })
                    .map(|c| c.with_sample_rate(target_sample_rate))
            })
            .or_else(|| {
                format_priority.iter().find_map(|&fmt| {
                    supported_configs
                        .iter()
                        .find(|c| c.sample_format() == fmt)
                        .map(|c| {
                            let rate =
                                target_sample_rate.clamp(c.min_sample_rate(), c.max_sample_rate());
                            c.with_sample_rate(rate)
                        })
                })
            })
            .ok_or(OutputError::UnsupportedFormat)?;

        let actual_sample_rate = config.sample_rate();
        let channels = config.channels();
        let sample_format = config.sample_format();

        let target_buffer_frames: u32 = match backend {
            #[cfg(all(target_os = "windows", feature = "asio"))]
            AudioBackend::ExclusiveAsio => 512,
            #[cfg(target_os = "linux")]
            AudioBackend::ExclusiveAlsa => 1024,
            #[cfg(target_os = "windows")]
            AudioBackend::ExclusiveWasapi => 1024,
            #[cfg(target_os = "macos")]
            AudioBackend::ExclusiveCoreAudioHog => 1024,
            _ => 2048,
        };

        let (buffer_size, buffer_size_frames, buffer_size_estimated) = match config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                let n = target_buffer_frames.clamp(*min, *max);
                (cpal::BufferSize::Fixed(n), n, false)
            }
            cpal::SupportedBufferSize::Unknown => {
                log::warn!(
                    "Audio output: device did not report a buffer size; using {} frames \
                         as an estimate (reported latency will be approximate)",
                    target_buffer_frames
                );
                (cpal::BufferSize::Default, target_buffer_frames, true)
            }
        };

        let stream_config = StreamConfig {
            channels,
            sample_rate: actual_sample_rate,
            buffer_size,
        };

        log::info!(
            "Audio output: {} Hz, {} ch, {:?}, buffer size: {:?}",
            actual_sample_rate,
            channels,
            sample_format,
            buffer_size
        );

        Ok(Self {
            stream: None,
            device,
            stream_config,
            buffer_size_frames,
            buffer_size_estimated,
            sample_format,
            format_plan: CpalFormatPlan::from_sample_format(sample_format),
            buffer,
            paused: Arc::new(AtomicBool::new(false)),
            in_callback: Arc::new(AtomicBool::new(false)),
            underruns: Arc::new(AtomicU32::new(0)),
            actual_sample_rate,
            stream_errors: StreamErrorState::default(),
            dither_enabled: Arc::new(AtomicBool::new(true)),
            clip_counter: Arc::new(AtomicU32::new(0)),
            nan_counter: Arc::new(AtomicU32::new(0)),
            hardware_volume_db: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            backend,
            target_device: target_device.map(|s| s.to_string()),
            requested_backend: backend,
            is_fallback: false,
            fallback_reason: None,
            fallback_policy,
        })
    }

    /// Enable or disable TPDF dither at the integer-quantization boundary.
    pub fn set_dither_enabled(&self, enabled: bool) {
        self.dither_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Take (and reset) the clip counter.
    pub fn take_clips(&self) -> u32 {
        self.clip_counter.swap(0, Ordering::Relaxed)
    }

    /// Take (and reset) the NaN counter.
    pub fn take_nans(&self) -> u32 {
        self.nan_counter.swap(0, Ordering::Relaxed)
    }

    /// Start the audio output stream with explicit fallback policy enforcement.
    pub fn start(&mut self) -> Result<(), OutputError> {
        match self.start_raw() {
            Ok(()) => Ok(()),
            Err(e) => {
                if self.fallback_policy == FallbackPolicy::Strict {
                    log::error!("Audio output: Stream open failed under Strict policy ({}); aborting without fallback.", e);
                    return Err(e);
                }
                let is_custom = self.backend != AudioBackend::Auto
                    || self
                        .target_device
                        .as_ref()
                        .is_some_and(|d| !d.is_empty() && d != "Default / Automatic");
                if is_custom {
                    log::warn!(
                        "Audio output: Failed to start stream in exclusive mode / target device {:?} ({}); falling back to default shared device (`Auto`).",
                        self.target_device,
                        e
                    );
                    let mut fallback = Self::new_raw(
                        self.buffer.clone(),
                        AudioBackend::Auto,
                        None,
                        self.fallback_policy,
                    )?;
                    fallback.start_raw()?;
                    fallback.requested_backend = self.requested_backend;
                    fallback.is_fallback = true;
                    fallback.fallback_reason = Some(e.to_string());
                    *self = fallback;
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Reconfigure output sample rate dynamically.
    ///
    /// Only the sample rate changes: the currently negotiated sample format
    /// and channel count are preserved. This keeps output characteristics
    /// stable across track changes (a rate change must never silently flip
    /// the container or channel layout, which is particularly important for
    /// bit-perfect paths).
    pub fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError> {
        self.reconfigure(target_sample_rate, None)
    }

    /// Reconfigure output sample rate while requesting a specific sample
    /// container (used by DSD-over-PCM, which requires an `I32` container for
    /// its left-aligned 24-bit frames). The current channel count is preserved.
    ///
    /// If the requested container is unavailable at any supported rate, this
    /// fails with [`OutputError::UnsupportedFormat`] rather than silently
    /// negotiating a different container.
    pub fn reconfigure_sample_format(
        &mut self,
        target_sample_rate: u32,
        sample_format: SampleFormat,
    ) -> Result<u32, OutputError> {
        self.reconfigure(target_sample_rate, Some(sample_format))
    }

    /// Shared reconfiguration path: preserve the current container and
    /// channel count (or request a specific container) while moving the rate.
    fn reconfigure(
        &mut self,
        target_sample_rate: u32,
        requested_format: Option<SampleFormat>,
    ) -> Result<u32, OutputError> {
        let format = requested_format.unwrap_or(self.sample_format);
        let channels = self.stream_config.channels;

        if self.actual_sample_rate == target_sample_rate
            && format == self.sample_format
            && self.stream.is_some()
        {
            return Ok(self.actual_sample_rate);
        }
        self.stop();

        let supported = self
            .device
            .supported_output_configs()
            .map_err(|e| OutputError::StreamOpen(format!("Cannot query configs: {}", e)))?;
        let supported: Vec<SupportedConfig> =
            supported.map(|c| SupportedConfig::from_cpal(&c)).collect();

        let choice = select_reconfigure_config(&supported, format, channels, target_sample_rate);

        let choice = match choice {
            Some(c) => c,
            // A specific container request that cannot be honored must fail,
            // not silently negotiate a different one (DoP depends on I32).
            None if requested_format.is_some() => {
                log::warn!(
                    "Audio output: device cannot provide {:?}/{}ch at {} Hz",
                    format,
                    channels,
                    target_sample_rate
                );
                return Err(OutputError::UnsupportedFormat);
            }
            None => {
                // The negotiated format/channels are no longer advertised by
                // the device (e.g. it changed under us). Re-derive the format
                // from the device's reported configs, but log that the
                // characteristics are changing — this is a recovery path, not
                // the normal track-change path.
                log::warn!(
                    "Audio output: {:?}/{}ch no longer available at {} Hz; \
                     re-negotiating format after device change",
                    format,
                    channels,
                    target_sample_rate
                );
                renegotiate_reconfigure_config(&supported, format, target_sample_rate)
                    .ok_or(OutputError::UnsupportedFormat)?
            }
        };

        self.actual_sample_rate = choice.rate;
        self.sample_format = choice.format;
        self.format_plan = CpalFormatPlan::from_sample_format(choice.format);
        self.stream_config.sample_rate = choice.rate;
        self.stream_config.channels = choice.channels;

        self.start()?;
        Ok(self.actual_sample_rate)
    }

    /// Pause the output
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        if let Some(ref stream) = self.stream {
            stream.send(CpalStreamCommand::Pause);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while self.in_callback.load(Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                log::warn!(
                    "CpalOutput::pause(): callback did not exit within 50ms; \
                     proceeding to avoid deadlock"
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
    }

    /// Resume the output
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        if let Some(ref stream) = self.stream {
            stream.send(CpalStreamCommand::Resume);
        }
    }

    /// Reset the output buffer safely.
    pub fn reset_buffer(&self) {
        self.pause();
        self.buffer.reset();
        self.resume();
    }

    /// Get the number of underruns since last check
    pub fn take_underruns(&self) -> u32 {
        self.underruns.swap(0, Ordering::Relaxed)
    }

    /// Get the actual sample rate
    pub fn sample_rate(&self) -> u32 {
        self.actual_sample_rate
    }

    /// The sample format of the active output stream. DoP requires a signed
    /// 32-bit container (the 24-bit DoP words are carried left-aligned).
    pub fn sample_format(&self) -> SampleFormat {
        self.sample_format
    }

    /// Take all stream diagnostics reported since the previous engine tick.
    pub fn take_stream_errors(&self) -> StreamErrorBatch {
        self.stream_errors.take()
    }

    /// Stop the output stream
    pub fn stop(&mut self) {
        self.pause();
        if let Some(stream) = self.stream.take() {
            // The owner thread stops the backend and drops `cpal::Stream`
            // itself. No raw pointer, leak fallback, or Send/Sync assertion
            // is needed, and destruction remains serialized with callbacks.
            stream.stop();
        }
    }

    /// Get a reference to the underlying CPAL audio device.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Query the capabilities of the current output device.
    ///
    /// The static device query cannot prove exclusive/direct access, so the
    /// access state of the **actually-opened stream** (derived from the
    /// backend, negotiated device and fallback state in
    /// [`Self::output_info`]) is overlaid onto the snapshot. This is what
    /// makes [`OutputAccessState::is_bit_perfect`] and
    /// [`OutputCapabilities::validate_stream`] with `require_direct`
    /// trustworthy when called on a live output.
    pub fn capabilities(&self) -> super::capabilities::OutputCapabilities {
        let mut caps = super::capabilities::OutputCapabilities::query(&self.device);
        let info = self.output_info();
        caps.access_state = info.access_state;
        caps.access_mode = info.access_mode;
        caps
    }

    /// Get the current device name for diagnostic purposes.
    pub fn device_name(&self) -> String {
        self.device
            .description()
            .map(|desc| desc.name().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    /// The negotiated callback buffer size in frames (the output-device
    /// buffering term of the graph latency model).
    pub fn buffer_size_frames(&self) -> u32 {
        self.buffer_size_frames
    }

    /// Return an `OutputInfo` snapshot reflecting the current output state.
    pub fn output_info(&self) -> OutputInfo {
        let dev_name = self.device_name();
        let dev_name_lower = dev_name.to_lowercase();
        let is_shared_server = dev_name_lower.contains("pulse")
            || dev_name_lower.contains("pipewire")
            || dev_name_lower.contains("dmix")
            || dev_name_lower == "default"
            || dev_name_lower == "sysdefault";

        let is_exclusive = match self.backend {
            // Only an exact ALSA `hw:` node is a verified direct endpoint.
            // `plughw:` inserts ALSA's conversion plugin and cannot support
            // a bit-perfect verdict.
            AudioBackend::ExclusiveAlsa => !is_shared_server && dev_name_lower.starts_with("hw:"),
            // WASAPI: cpal cannot request AUDCLNT_SHAREMODE_EXCLUSIVE (its
            // stream API only ever initializes shared-mode IAudioClients), so
            // requesting this backend never delivers exclusive access. A
            // native IAudioClient backend is required.
            AudioBackend::ExclusiveWasapi => false,
            // ASIO is exclusive by protocol: the stream drives the hardware
            // directly with no OS mixer in the path.
            #[cfg(feature = "asio")]
            AudioBackend::ExclusiveAsio => true,
            // CoreAudio Hog Mode is not implemented by cpal (no
            // AudioHardwareSetProperty); this backend is shared-mode only.
            // A native hog-mode implementation is required.
            AudioBackend::ExclusiveCoreAudioHog => false,
            _ => false,
        };

        let access_mode = if is_exclusive && !self.is_fallback {
            if dev_name_lower.starts_with("hw:") {
                crate::output::capabilities::OutputAccessMode::DirectHw
            } else {
                crate::output::capabilities::OutputAccessMode::Exclusive
            }
        } else {
            crate::output::capabilities::OutputAccessMode::Shared
        };

        // Verified means we have OS-level confirmation of exclusive/direct access.
        // - ALSA hw:N → direct hardware access: verified (see the comment on
        //   `is_exclusive` above); plughw is deliberately not verified.
        // - WASAPI / CoreAudio Hog → cpal cannot deliver exclusive mode, so
        //   these are never verified until a native backend exists.
        // - ASIO → the ASIO protocol guarantees direct hardware access.
        let is_verified = match self.backend {
            AudioBackend::ExclusiveAlsa => access_mode.is_direct() && !is_shared_server,
            AudioBackend::ExclusiveWasapi => false, // cpal is shared-mode only
            #[cfg(feature = "asio")]
            AudioBackend::ExclusiveAsio => !self.is_fallback,
            AudioBackend::ExclusiveCoreAudioHog => false, // cpal has no hog mode
            _ => false,
        };

        let access_state = crate::output::capabilities::OutputAccessState {
            requested: if matches!(
                self.requested_backend,
                AudioBackend::ExclusiveAlsa
                    | AudioBackend::ExclusiveWasapi
                    | AudioBackend::ExclusiveAsio
                    | AudioBackend::ExclusiveCoreAudioHog
            ) {
                crate::output::capabilities::OutputAccessMode::Exclusive
            } else {
                crate::output::capabilities::OutputAccessMode::Shared
            },
            actual: access_mode,
            verified: is_verified,
        };

        OutputInfo {
            requested_backend: Some(self.requested_backend),
            actual_backend: Some(self.backend),
            requested_rate: self.actual_sample_rate,
            actual_rate: self.actual_sample_rate,
            channels: self.stream_config.channels,
            buffer_size_frames: self.buffer_size_frames,
            buffer_size_estimated: self.buffer_size_estimated,
            sample_format: match self.sample_format {
                SampleFormat::F32 => crate::dsp::pipeline::OutputSampleFormat::F32,
                SampleFormat::F64 => crate::dsp::pipeline::OutputSampleFormat::F64,
                SampleFormat::I16 => crate::dsp::pipeline::OutputSampleFormat::I16,
                SampleFormat::U16 => crate::dsp::pipeline::OutputSampleFormat::U16,
                SampleFormat::I32 => crate::dsp::pipeline::OutputSampleFormat::I32,
                _ => crate::dsp::pipeline::OutputSampleFormat::Unknown,
            },
            dither_enabled: self.dither_enabled.load(Ordering::Relaxed),
            access_mode,
            access_state,
            is_fallback: self.is_fallback,
            fallback_reason: self.fallback_reason.clone(),
            is_exclusive: is_exclusive && !self.is_fallback,
            device_name: dev_name,
        }
    }

    /// Whether the active output stream/device supports native hardware volume
    /// control in this build.
    ///
    /// Returns `true` on Linux ALSA `hw:` / `plughw:` endpoints when `amixer`
    /// is available in PATH, and on macOS when the default CoreAudio output
    /// device exposes a virtual main/master volume control. WASAPI endpoint
    /// volume is implemented by the native `wasapi-native` backend (not this
    /// cpal path); ASIO remains unimplemented in this build.
    pub fn supports_hardware_volume(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            let dev_name = self.device_name().to_lowercase();
            let is_hw_endpoint = dev_name.starts_with("hw:") || dev_name.starts_with("plughw:");
            is_hw_endpoint && alsa_hardware_volume_supported(&self.device_name())
        }
        #[cfg(target_os = "macos")]
        {
            coreaudio::default_output_device()
                .map(|device| coreaudio::supports_virtual_volume(device))
                .unwrap_or(false)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            // ASIO channel gain is not implemented in this build.
            false
        }
    }

    /// Set hardware endpoint volume in dB ([-96.0, 0.0] dB).
    ///
    /// On Linux ALSA `hw:` endpoints: invokes `amixer sset Master <pct>%` to
    /// set the ALSA Master mixer control. Also stores the value in the atomic
    /// for read-back.
    ///
    /// On macOS: converts dB to a linear scalar and sets the default output
    /// device's virtual main/master volume via
    /// `AudioObjectSetPropertyData`. Also stores the value in the atomic for
    /// read-back.
    ///
    /// On other platforms / non-hw endpoints: returns `Err(OutputError::StreamError)`
    /// because no hardware volume backend is implemented in this cpal path
    /// (WASAPI endpoint volume lives in the native `wasapi-native` backend;
    /// ASIO remains unimplemented). Callers should check
    /// `supports_hardware_volume()` before calling this.
    pub fn set_hardware_volume_db(&self, _db: f32) -> Result<(), OutputError> {
        #[cfg(target_os = "linux")]
        {
            let db = _db;
            let clamped = db.clamp(-96.0, 0.0);
            let dev_name = self.device_name().to_lowercase();
            let is_hw_endpoint = dev_name.starts_with("hw:") || dev_name.starts_with("plughw:");
            if is_hw_endpoint {
                // Only record the value once the OS call actually succeeded,
                // so the atomic read-back never reports an unapplied level.
                let result = set_alsa_volume_db(clamped, &self.device_name());
                if result.is_ok() {
                    self.hardware_volume_db
                        .store(clamped.to_bits(), Ordering::Release);
                }
                return result;
            }
        }

        #[cfg(target_os = "macos")]
        {
            let db = _db.clamp(-96.0, 0.0);
            let device = coreaudio::default_output_device().ok_or_else(|| {
                OutputError::StreamError("no default CoreAudio output device".to_string())
            })?;
            let linear = coreaudio::db_to_linear(db);
            // Only record the value once the OS call actually succeeded.
            let result = coreaudio::set_virtual_volume(device, linear);
            if result.is_ok() {
                self.hardware_volume_db
                    .store(db.to_bits(), Ordering::Release);
            }
            return result.map_err(OutputError::StreamError);
        }

        // No hardware-volume backend on this platform (e.g. ASIO).
        Err(OutputError::StreamError(format!(
            "Hardware volume is not implemented for backend {:?} on this platform. \
             Use VolumeMode::SoftwareOnly for software gain control.",
            self.backend
        )))
    }
}

impl OutputVolume for CpalOutput {
    fn supports_hardware_volume(&self) -> bool {
        CpalOutput::supports_hardware_volume(self)
    }

    fn set_hardware_volume_db(&self, db: f32) -> Result<(), OutputError> {
        CpalOutput::set_hardware_volume_db(self, db)
    }
}

impl super::output::Output for CpalOutput {
    fn sample_rate(&self) -> u32 {
        CpalOutput::sample_rate(self)
    }

    fn sample_format(&self) -> cpal::SampleFormat {
        CpalOutput::sample_format(self)
    }

    fn buffer_size_frames(&self) -> u32 {
        CpalOutput::buffer_size_frames(self)
    }

    fn output_info(&self) -> OutputInfo {
        CpalOutput::output_info(self)
    }

    fn capabilities(&self) -> super::capabilities::OutputCapabilities {
        CpalOutput::capabilities(self)
    }

    fn device_name(&self) -> String {
        CpalOutput::device_name(self)
    }

    fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError> {
        CpalOutput::reconfigure_sample_rate(self, target_sample_rate)
    }

    fn reconfigure_sample_format(
        &mut self,
        target_sample_rate: u32,
        sample_format: cpal::SampleFormat,
    ) -> Result<u32, OutputError> {
        CpalOutput::reconfigure_sample_format(self, target_sample_rate, sample_format)
    }

    fn reset_buffer(&self) {
        CpalOutput::reset_buffer(self);
    }

    fn take_underruns(&self) -> u32 {
        CpalOutput::take_underruns(self)
    }

    fn take_clips(&self) -> u32 {
        CpalOutput::take_clips(self)
    }

    fn take_nans(&self) -> u32 {
        CpalOutput::take_nans(self)
    }

    fn take_stream_errors(&self) -> StreamErrorBatch {
        CpalOutput::take_stream_errors(self)
    }

    fn set_dither_enabled(&self, enabled: bool) {
        CpalOutput::set_dither_enabled(self, enabled);
    }

    fn pause(&self) {
        CpalOutput::pause(self);
    }

    fn resume(&self) {
        CpalOutput::resume(self);
    }

    fn start(&mut self) -> Result<(), OutputError> {
        CpalOutput::start(self)
    }

    fn stop(&mut self) {
        CpalOutput::stop(self);
    }
}
