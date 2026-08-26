//! Native WASAPI exclusive-mode output backend (bypasses cpal).
//!
//! This module drives a real `IAudioClient` stream in
//! `AUDCLNT_SHAREMODE_EXCLUSIVE` mode — the thing cpal cannot do (its
//! WASAPI backend initializes every client in shared mode). Because the
//! exclusive-mode negotiation is performed directly against the OS, the
//! stream can report *verified* exclusivity: [`WasapiOutput::output_info`]
//! sets `OutputAccessState.verified = true` only after
//! `IAudioClient::Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, ...)` **and**
//! `IAudioClient::Start()` both succeeded. That is the OS-level confirmation
//! the cpal path can never provide (see `src/output/cpal_output.rs`).
//!
//! ## Architecture
//!
//! ```text
//! engine (DSP) ──push──▶ FixedFrameBuffer ──pull──▶ render thread
//!                                                    │  WaitForSingleObject(event)
//!                                                    ▼
//!                               IAudioRenderClient::GetBuffer / ReleaseBuffer
//! ```
//!
//! - **`new()`** — initializes COM (MTA), selects the render endpoint
//!   (honoring the engine's `output_device` config: enumerates active
//!   endpoints via `IMMDeviceCollection` and matches by endpoint ID first,
//!   then `PKEY_Device_FriendlyName`, falling back to the default endpoint),
//!   probes which standard rates are actually supported in exclusive mode
//!   (`IsFormatSupported`), then `Initialize`s the client in exclusive mode
//!   with an event-driven, 10×default-period buffer.
//! - **`start()`** — creates the buffer-end event, sets it on the client,
//!   spawns the render thread, calls `Start()`, and only then marks the
//!   stream verified-exclusive.
//! - **render thread** — waits on the buffer-end event (with a short timeout
//!   so shutdown stays responsive), computes `buffer_size − padding`, pulls
//!   that many frames from the shared `FixedFrameBuffer` (silence + underrun
//!   counter on starvation, exactly like the cpal callbacks), quantizes the
//!   block into the negotiated container
//!   (f32 passthrough, or i16/i32 with TPDF dither via
//!   `AudioFormatConverter`, counting clips/NaNs like the cpal callbacks),
//!   and writes the frames via `IAudioRenderClient`.
//! - **`reconfigure_sample_rate()`** — exclusive mode requires re-activating
//!   a fresh `IAudioClient` for a new format (a client can only be
//!   initialized once), so this stops the stream, releases the old client,
//!   re-activates, re-negotiates, and restarts.
//! - **hardware volume** — `IAudioEndpointVolume` (real Windows hardware
//!   volume; closes the Windows gap noted in `OutputVolume`). The backend
//!   also registers an `IAudioEndpointVolumeCallback` change-notification
//!   callback, so external volume changes (OS slider, hardware knob, other
//!   processes) are surfaced through `take_external_volume_change()` and
//!   folded into `PlaybackInfo.volume` by the engine.
//!
//! ## Sketch status
//!
//! This is a working architectural sketch, not yet validated on hardware:
//!
//! - Targets the `windows` crate **0.59** API. The COM call signatures
//!   (especially `IMMDevice::Activate`, `CoCreateInstance` and the tuple
//!   returns of `GetDevicePeriod`) are version-sensitive — if the pin is
//!   bumped, re-verify them with `cargo check --target x86_64-pc-windows-msvc`.
//! - Stereo only, f32-first containers: `open_exclusive_client` negotiates
//!   f32 → i32 → 24-bit-in-32 (I24Le) → i16, opening the first container
//!   the endpoint accepts in exclusive mode at the requested rate. Only
//!   when **all four** are refused does the factory fall back to cpal
//!   shared mode. The negotiated container is reported through
//!   `sample_format()`/`output_info()` (I24Le surfaces as
//!   `OutputSampleFormat::I24Le` in `output_info`; the cpal vocabulary
//!   reports it as I32 since it is a 32-bit container), and
//!   `capabilities().formats` lists only containers that passed a real
//!   exclusive-mode probe. TPDF dither is applied at the 16- and 24-bit
//!   quantization boundaries (a no-op at 32 bits, below the f32 source's
//!   own noise floor), mirroring the cpal i16/i32 callbacks.
//! - Render-thread shutdown uses a 50 ms poll instead of `SetEvent`, so
//!   `stop()`/`reconfigure_sample_rate()` can take up to ~50 ms.

#![cfg(all(target_os = "windows", feature = "wasapi-native"))]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use config::AudioBackend;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Media::Audio::{IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator},
    System::{
        Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED},
        Threading::CreateEventW,
    },
};

use crate::buffer::FixedFrameBuffer;
use crate::dsp::dither::DitherType;
use crate::dsp::pipeline::OutputSampleFormat;
use crate::output::capabilities::{OutputAccessMode, OutputAccessState, OutputCapabilities};
use crate::output::cpal_output::{OutputError, OutputVolume};
use crate::output::format_converter::{AudioFormatConverter, TargetFormat};
use crate::output::output::{StreamErrorBatch, StreamErrorState};
use crate::output::output_info::OutputInfo;

pub(crate) mod client;
pub(crate) mod com;
mod format;
mod render;
mod volume;

use client::{
    open_exclusive_client, open_exclusive_client_preferring, select_device, ExclusiveClient,
    RenderContext,
};
use com::{ComInitGuard, SendCom, SendHandle};
use format::{
    default_exclusive_rate, probe_exclusive_rates, probe_supported_formats, WasapiContainer,
};
use render::render_loop;

/// Native WASAPI exclusive-mode output.
pub struct WasapiOutput {
    device_name: String,
    /// Stable OS endpoint ID (`IMMDevice::GetId`) captured at construction;
    /// `None` only when the driver could not report one.
    device_id: Option<String>,
    backend: AudioBackend,
    requested_backend: AudioBackend,
    buffer: Arc<FixedFrameBuffer>,
    paused: Arc<AtomicBool>,
    in_callback: Arc<AtomicBool>,
    underruns: Arc<AtomicU32>,
    clip_counter: Arc<AtomicU32>,
    nan_counter: Arc<AtomicU32>,
    stream_errors: StreamErrorState,
    /// Whether TPDF dither is applied at the integer-quantization boundary
    /// when the stream runs in an i16/i32 container (no-op for f32). Read by
    /// the render thread each period, mirroring the cpal callbacks.
    dither_enabled: Arc<AtomicBool>,
    hardware_volume_db: Arc<AtomicU32>,
    /// The activated device, kept so `reconfigure_sample_rate` can activate
    /// a fresh client (a client can only be initialized once). Wrapped in
    /// `SendCom` because windows-rs 0.59 interfaces are not `Send` and the
    /// `Output` trait requires `Send` (COM handles are thread-safe by
    /// contract; see `SendCom` above).
    device: Option<SendCom<IMMDevice>>,
    client: Option<SendCom<ExclusiveClient>>,
    /// Sample rates that passed a real exclusive-mode `IsFormatSupported`
    /// probe at construction time (used for the capabilities snapshot).
    supported_exclusive_rates: Vec<u32>,
    /// Sample containers (f32/i32/i24-in-32/i16) that passed a real
    /// exclusive-mode `IsFormatSupported` probe at the initial rate.
    supported_formats: Vec<WasapiContainer>,
    running: Arc<AtomicBool>,
    render_thread: Option<std::thread::JoinHandle<()>>,
    /// True once `Initialize(EXCLUSIVE)` + `Start()` succeeded on the current
    /// client. This is the OS-level exclusivity verification.
    exclusive_verified: bool,
    is_fallback: bool,
    fallback_reason: Option<String>,
}

impl WasapiOutput {
    /// Activate the default render endpoint and negotiate an exclusive-mode
    /// f32 client at the endpoint's default rate.
    pub fn new(
        buffer: Arc<FixedFrameBuffer>,
        backend: AudioBackend,
        target_device: Option<&str>,
    ) -> Result<Self, OutputError> {
        // One COM apartment per thread; the render thread initializes its own.
        // CoInitializeEx returns the HRESULT directly in windows 0.59.
        if !unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok() {
            return Err(OutputError::StreamOpen("CoInitializeEx failed".to_string()));
        }
        let com = ComInitGuard;

        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| {
                OutputError::StreamOpen(format!("CoCreateInstance(IMMDeviceEnumerator): {e}"))
            })?;
        let (device, device_name) = select_device(&enumerator, target_device)?;
        let initial_rate = default_exclusive_rate(&device).unwrap_or(44_100);

        // Probe first, while no exclusive client holds the endpoint: an
        // open exclusive stream can make `IsFormatSupported` fail with
        // AUDCLNT_E_DEVICE_IN_USE. These are the device's true
        // exclusive-mode rates and containers — every entry is a real probe
        // result.
        let supported_exclusive_rates = probe_exclusive_rates(&device);
        let supported_formats = probe_supported_formats(&device, initial_rate);
        let client = open_exclusive_client(&device, initial_rate)?;

        let device_id = client::endpoint_id_of(&device);

        let out = Self {
            device_name,
            device_id,
            backend,
            requested_backend: backend,
            buffer,
            paused: Arc::new(AtomicBool::new(false)),
            in_callback: Arc::new(AtomicBool::new(false)),
            underruns: Arc::new(AtomicU32::new(0)),
            clip_counter: Arc::new(AtomicU32::new(0)),
            nan_counter: Arc::new(AtomicU32::new(0)),
            stream_errors: StreamErrorState::default(),
            dither_enabled: Arc::new(AtomicBool::new(true)),
            hardware_volume_db: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            device: Some(SendCom(device)),
            client: Some(SendCom(client)),
            supported_exclusive_rates,
            supported_formats,
            running: Arc::new(AtomicBool::new(false)),
            render_thread: None,
            exclusive_verified: false,
            is_fallback: false,
            fallback_reason: None,
        };

        // COM stays initialized for the lifetime of the output (its Drop impl
        // un-initializes it after releasing the COM objects).
        std::mem::forget(com);
        Ok(out)
    }

    fn client(&self) -> &ExclusiveClient {
        &self.client.as_ref().expect("WASAPI client released").0
    }

    fn client_mut(&mut self) -> &mut ExclusiveClient {
        &mut self.client.as_mut().expect("WASAPI client released").0
    }

    fn start_client(&mut self) -> Result<(), OutputError> {
        if self.render_thread.is_some() {
            return Ok(());
        }

        // Create a fresh buffer-end event and hand it to the client. Must be
        // done before Start().
        let event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| OutputError::StreamOpen(format!("CreateEventW: {e}")))?;
        {
            let client = self.client_mut();
            unsafe { client.audio_client.SetEventHandle(event) }.map_err(|e| {
                OutputError::StreamOpen(format!("IAudioClient::SetEventHandle: {e}"))
            })?;
            // Keep the handle so stop() can close it after the thread joins.
            client.event = event;
        }

        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Release);

        let converter = AudioFormatConverter::new(
            match self.client().sample_format {
                WasapiContainer::I16 => TargetFormat::I16,
                WasapiContainer::I24Le => TargetFormat::I24Le,
                WasapiContainer::I32 => TargetFormat::I32,
                WasapiContainer::F32 => TargetFormat::F32,
            },
            DitherType::Triangular,
        );

        let ctx = RenderContext {
            audio_client: SendCom(self.client().audio_client.clone()),
            render_client: SendCom(self.client().render_client.clone()),
            event: SendHandle(event),
            buffer: Arc::clone(&self.buffer),
            paused: Arc::clone(&self.paused),
            in_callback: Arc::clone(&self.in_callback),
            running: Arc::clone(&self.running),
            underruns: Arc::clone(&self.underruns),
            clip_counter: Arc::clone(&self.clip_counter),
            nan_counter: Arc::clone(&self.nan_counter),
            stream_errors: self.stream_errors.clone(),
            buffer_size_frames: self.client().buffer_size_frames,
            channels: self.client().channels as usize,
            sample_format: self.client().sample_format,
            dither_enabled: Arc::clone(&self.dither_enabled),
            converter,
        };

        let handle = std::thread::Builder::new()
            .name("wasapi-render".to_string())
            .spawn(move || render_loop(ctx))
            .map_err(|e| OutputError::StreamOpen(format!("spawn wasapi-render thread: {e}")))?;
        self.render_thread = Some(handle);

        // With the render thread waiting on the event, start the engine.
        unsafe { self.client().audio_client.Start() }
            .map_err(|e| OutputError::StreamOpen(format!("IAudioClient::Start: {e}")))?;

        // Start() succeeding on an exclusive-mode client is the OS-level
        // confirmation that this stream owns the endpoint unmixed.
        self.exclusive_verified = true;
        log::info!(
            "WASAPI exclusive stream verified: {} Hz, {} ch, {} frames buffer",
            self.client().sample_rate,
            self.client().channels,
            self.client().buffer_size_frames
        );
        Ok(())
    }

    fn stop_client(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.render_thread.take() {
            let _ = handle.join();
        }
        if let Some(client) = self.client.as_ref() {
            let _ = unsafe { client.0.audio_client.Stop() };
        }
        // Release the event handle (the thread has been joined, so nobody is
        // waiting on it anymore).
        if let Some(client) = self.client.as_mut() {
            if !client.0.event.is_invalid() {
                unsafe {
                    let _ = CloseHandle(client.0.event);
                }
                client.0.event = HANDLE(std::ptr::null_mut());
            }
        }
        self.exclusive_verified = false;
    }

    /// Replace the client, requesting `preferred` as the first container
    /// choice (e.g. I32 for DSD-over-PCM). The endpoint may still negotiate
    /// another container; callers verify via `sample_format()`. The old
    /// client (and with it the exclusive lock) is released only after the
    /// new one is negotiated.
    fn replace_client_with_format(
        &mut self,
        rate: u32,
        preferred: Option<WasapiContainer>,
    ) -> Result<(), OutputError> {
        let device = self
            .device
            .as_ref()
            .expect("WASAPI device released")
            .0
            .clone();
        let new_client = open_exclusive_client_preferring(&device, rate, preferred)?;
        self.client = Some(SendCom(new_client));
        Ok(())
    }

    pub fn sample_rate(&self) -> u32 {
        self.client().sample_rate
    }

    pub fn sample_format(&self) -> cpal::SampleFormat {
        // I24Le (24 valid bits in a 32-bit container) reports as I32 — the
        // cpal vocabulary has no 24-bit-in-32; `output_info()` carries the
        // precise `OutputSampleFormat::I24Le`.
        self.client().sample_format.cpal()
    }

    pub fn buffer_size_frames(&self) -> u32 {
        self.client().buffer_size_frames
    }

    pub fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError> {
        self.reconfigure_sample_rate_with_preference(target_sample_rate, None)
    }

    /// Shared rate-reconfiguration body; `preferred` requests a specific
    /// container first (I32 for DoP) when reopening the client.
    fn reconfigure_sample_rate_with_preference(
        &mut self,
        target_sample_rate: u32,
        preferred: Option<WasapiContainer>,
    ) -> Result<u32, OutputError> {
        let prev_rate = self.client().sample_rate;
        let format_matches = preferred
            .map(|format| self.client().sample_format == format)
            .unwrap_or(true);
        if target_sample_rate == prev_rate && format_matches {
            return Ok(target_sample_rate);
        }
        log::info!(
            "WASAPI: re-opening exclusive client at {} Hz (was {} Hz){}",
            target_sample_rate,
            prev_rate,
            preferred
                .map(|f| format!(" preferring {f:?}"))
                .unwrap_or_default()
        );
        self.stop_client();
        match self.replace_client_with_format(target_sample_rate, preferred) {
            Ok(()) => {
                self.start_client()?;
                Ok(self.client().sample_rate)
            }
            Err(e) => {
                // The device refused the new rate in exclusive mode. Restore
                // the previous rate so the engine is not left with a silent
                // (stopped) stream; the caller still sees the error and can
                // fall back (e.g. DoP → DSD/PCM).
                log::warn!(
                    "WASAPI re-open at {} Hz failed ({e}); restoring {} Hz",
                    target_sample_rate,
                    prev_rate
                );
                if self
                    .replace_client_with_format(prev_rate, preferred)
                    .is_ok()
                {
                    let _ = self.start_client();
                }
                Err(e)
            }
        }
    }

    /// Re-open the stream at `target_sample_rate` Hz requesting the given
    /// sample container first (exclusive mode requires a fresh client per
    /// format). DSD-over-PCM (DoP) requests `I32` so the 24-bit DoP words
    /// reach the DAC bit-exactly; an endpoint that refuses I32 negotiates
    /// another container and the caller verifies via `sample_format()`.
    pub fn reconfigure_sample_format(
        &mut self,
        target_sample_rate: u32,
        sample_format: cpal::SampleFormat,
    ) -> Result<u32, OutputError> {
        let preferred = match sample_format {
            cpal::SampleFormat::I32 => Some(WasapiContainer::I32),
            cpal::SampleFormat::F32 => Some(WasapiContainer::F32),
            cpal::SampleFormat::I16 => Some(WasapiContainer::I16),
            _ => None,
        };
        self.reconfigure_sample_rate_with_preference(target_sample_rate, preferred)
    }

    pub fn output_info(&self) -> OutputInfo {
        let verified = self.exclusive_verified && !self.is_fallback;
        let access_state = OutputAccessState {
            requested: OutputAccessMode::Exclusive,
            actual: OutputAccessMode::Exclusive,
            verified,
        };
        let sample_format = match self.client().sample_format {
            WasapiContainer::I16 => OutputSampleFormat::I16,
            WasapiContainer::I24Le => OutputSampleFormat::I24Le,
            WasapiContainer::I32 => OutputSampleFormat::I32,
            WasapiContainer::F32 => OutputSampleFormat::F32,
        };
        OutputInfo {
            requested_backend: Some(self.requested_backend),
            actual_backend: Some(self.backend),
            requested_rate: self.client().sample_rate,
            actual_rate: self.client().sample_rate,
            channels: self.client().channels,
            buffer_size_frames: self.client().buffer_size_frames,
            buffer_size_estimated: false,
            sample_format,
            dither_enabled: self.dither_enabled.load(Ordering::Relaxed),
            access_mode: OutputAccessMode::Exclusive,
            access_state,
            is_fallback: self.is_fallback,
            fallback_reason: self.fallback_reason.clone(),
            is_exclusive: verified,
            device_name: self.device_name.clone(),
        }
    }

    pub fn capabilities(&self) -> OutputCapabilities {
        let mut sample_rates = self.supported_exclusive_rates.clone();
        sample_rates.sort_unstable();
        sample_rates.dedup();
        // Containers that passed a real exclusive-mode probe at the initial
        // rate (f32 → i32 → i24 → i16 preference order). I24Le (24 valid
        // bits in a 32-bit container) appears as I32 in the cpal vocabulary,
        // so it dedups with a plain-I32 probe result.
        let mut formats: Vec<cpal::SampleFormat> =
            self.supported_formats.iter().map(|c| c.cpal()).collect();
        formats.sort_by_key(|f| match f {
            cpal::SampleFormat::F32 => 0,
            cpal::SampleFormat::I32 => 1,
            cpal::SampleFormat::I16 => 2,
            _ => 3,
        });
        formats.dedup();
        OutputCapabilities {
            sample_rates,
            hardware_ranges: Vec::new(),
            formats,
            channels: vec![self.client().channels],
            device_name: self.device_name.clone(),
            access_mode: OutputAccessMode::Exclusive,
            access_state: OutputAccessState {
                requested: OutputAccessMode::Exclusive,
                actual: OutputAccessMode::Exclusive,
                verified: self.exclusive_verified && !self.is_fallback,
            },
            likely_direct_access: true,
            supports_exclusive: true,
        }
    }

    pub fn device_name(&self) -> String {
        self.device_name.clone()
    }

    fn device_id(&self) -> Option<String> {
        self.device_id.clone()
    }

    pub fn reset_buffer(&self) {
        self.pause();
        self.buffer.reset();
        self.resume();
    }

    pub fn take_underruns(&self) -> u32 {
        self.underruns.swap(0, Ordering::AcqRel)
    }

    pub fn take_clips(&self) -> u32 {
        self.clip_counter.swap(0, Ordering::AcqRel)
    }

    pub fn take_nans(&self) -> u32 {
        self.nan_counter.swap(0, Ordering::AcqRel)
    }

    pub fn take_stream_errors(&self) -> StreamErrorBatch {
        self.stream_errors.take()
    }

    pub fn set_dither_enabled(&self, enabled: bool) {
        // f32 output has no integer-quantization boundary, so dither is a
        // no-op here; the flag is stored for API symmetry with cpal outputs.
        self.dither_enabled.store(enabled, Ordering::Release);
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while self.in_callback.load(Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                log::warn!("WasapiOutput::pause(): render callback did not exit within 50ms");
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    pub fn supports_hardware_volume(&self) -> bool {
        self.client().endpoint_volume.is_some()
    }

    pub fn set_hardware_volume_db(&self, db: f32) -> Result<(), OutputError> {
        let clamped = db.clamp(-96.0, 0.0);
        let volume = self.client().endpoint_volume.as_ref().ok_or_else(|| {
            OutputError::StreamError(
                "IAudioEndpointVolume unavailable on this endpoint".to_string(),
            )
        })?;
        // pguideventcontext is a raw pointer (not Option) in windows 0.59.
        unsafe { volume.SetMasterVolumeLevel(clamped, std::ptr::null()) }.map_err(|e| {
            OutputError::StreamError(format!("IAudioEndpointVolume::SetMasterVolumeLevel: {e}"))
        })?;
        self.hardware_volume_db
            .store(clamped.to_bits(), Ordering::Release);
        Ok(())
    }

    /// Take the most recent hardware volume level (linear 0.0–1.0) observed
    /// via the endpoint-volume change-notification callback (OS slider,
    /// hardware knob, programmatic sets from any process), if a change
    /// arrived since the last call. `None` when nothing changed or the
    /// endpoint does not expose a volume service / registration failed.
    pub fn take_external_volume_change(&self) -> Option<f32> {
        let state = self.client().volume_callback_state.as_ref()?;
        if !state.changed.swap(false, Ordering::AcqRel) {
            return None;
        }
        Some(f32::from_bits(state.volume_linear.load(Ordering::Acquire)))
    }

    pub fn start(&mut self) -> Result<(), OutputError> {
        self.start_client()
    }

    pub fn stop(&mut self) {
        self.stop_client();
    }
}

impl Drop for WasapiOutput {
    fn drop(&mut self) {
        self.stop_client();
        // Release every COM object *before* un-initializing COM for this
        // thread (fields would otherwise drop after this method returns).
        drop(self.device.take());
        drop(self.client.take());
        unsafe {
            CoUninitialize();
        }
    }
}

impl OutputVolume for WasapiOutput {
    fn supports_hardware_volume(&self) -> bool {
        WasapiOutput::supports_hardware_volume(self)
    }

    fn set_hardware_volume_db(&self, db: f32) -> Result<(), OutputError> {
        WasapiOutput::set_hardware_volume_db(self, db)
    }

    fn take_external_volume_change(&self) -> Option<f32> {
        WasapiOutput::take_external_volume_change(self)
    }
}

impl super::output::Output for WasapiOutput {
    fn sample_rate(&self) -> u32 {
        WasapiOutput::sample_rate(self)
    }

    fn sample_format(&self) -> cpal::SampleFormat {
        WasapiOutput::sample_format(self)
    }

    fn buffer_size_frames(&self) -> u32 {
        WasapiOutput::buffer_size_frames(self)
    }

    fn output_info(&self) -> OutputInfo {
        WasapiOutput::output_info(self)
    }

    fn capabilities(&self) -> OutputCapabilities {
        WasapiOutput::capabilities(self)
    }

    fn device_name(&self) -> String {
        WasapiOutput::device_name(self)
    }

    fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError> {
        WasapiOutput::reconfigure_sample_rate(self, target_sample_rate)
    }

    fn reconfigure_sample_format(
        &mut self,
        target_sample_rate: u32,
        sample_format: cpal::SampleFormat,
    ) -> Result<u32, OutputError> {
        WasapiOutput::reconfigure_sample_format(self, target_sample_rate, sample_format)
    }

    fn reset_buffer(&self) {
        WasapiOutput::reset_buffer(self);
    }

    fn take_underruns(&self) -> u32 {
        WasapiOutput::take_underruns(self)
    }

    fn take_clips(&self) -> u32 {
        WasapiOutput::take_clips(self)
    }

    fn take_nans(&self) -> u32 {
        WasapiOutput::take_nans(self)
    }

    fn take_stream_errors(&self) -> StreamErrorBatch {
        WasapiOutput::take_stream_errors(self)
    }

    fn set_dither_enabled(&self, enabled: bool) {
        WasapiOutput::set_dither_enabled(self, enabled);
    }

    fn pause(&self) {
        WasapiOutput::pause(self);
    }

    fn resume(&self) {
        WasapiOutput::resume(self);
    }

    fn start(&mut self) -> Result<(), OutputError> {
        WasapiOutput::start(self)
    }

    fn stop(&mut self) {
        WasapiOutput::stop(self);
    }
}
