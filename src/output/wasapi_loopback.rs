//! WASAPI loopback capture — record the system mix ("what you hear").
//!
//! Opens an `IAudioClient` on the render endpoint in `AUDCLNT_SHAREMODE_SHARED`
//! mode with `AUDCLNT_STREAMFLAGS_LOOPBACK`, which makes the client *capture*
//! whatever is currently playing through that endpoint (the system mixer
//! output, including audio from other applications). A dedicated capture
//! thread waits on a buffer-end event and drains `IAudioCaptureClient`
//! packets into a shared [`FixedFrameBuffer`] as interleaved `f32`.
//!
//! This is independent of the engine's output stream: the engine can capture
//! system audio even while it is not playing anything, and (on most systems)
//! while its own output runs on a different endpoint.
//!
//! ## Wire format
//!
//! The mix format is read from `IAudioClient::GetMixFormat`. On modern
//! Windows this is IEEE-float32; integer mix formats (16/24/32-bit PCM,
//! plain or `WAVEFORMATEXTENSIBLE`) are converted to `f32` at capture time
//! so consumers always see normalized `[-1, 1]` samples. `SILENT` packets
//! are delivered as silence (the mixer marks them so a volume-zero app
//! doesn't burn CPU in the capture path).
//!
//! ## Lifecycle
//!
//! `new()` initializes COM and activates the client but does not start
//! streaming; `start()` spawns the capture thread and calls `Start()`;
//! `stop()` joins the thread and releases the stream. The ring buffer is
//! shared via `Arc` so the engine can drain it from its tick loop while the
//! capture thread fills it (SPSC: one producer, one consumer).
//!
//! ## Status
//!
//! Same hardware-verification caveat as the WASAPI output backend: the code
//! is written against the windows 0.59 API and compile-checked for
//! `x86_64-pc-windows-msvc`, but not yet validated on real hardware.

#![cfg(all(target_os = "windows", feature = "wasapi-native"))]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use windows::Win32::{
    Foundation::{CloseHandle, WAIT_OBJECT_0},
    Media::{
        Audio::{
            IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
            WAVEFORMATEX, WAVE_FORMAT_PCM,
        },
        KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE},
        Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
    },
    System::{
        Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED},
        Threading::CreateEventW,
    },
};

use crate::buffer::{FixedFrameBuffer, MAX_CHANNELS};
use crate::output::cpal_output::OutputError;

use super::wasapi_output::client::select_device;
use super::wasapi_output::com::{ComInitGuard, SendCom, SendHandle};

/// WASAPI loopback capture endpoint.
pub struct WasapiLoopbackCapture {
    client: SendCom<IAudioClient>,
    capture_client: SendCom<IAudioCaptureClient>,
    /// Interleaved f32 ring the capture thread fills; the engine drains it.
    buffer: Arc<FixedFrameBuffer>,
    /// Device friendly name (for logging / display).
    device_name: String,
    sample_rate: u32,
    channels: u16,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    event: Option<SendHandle>,
    /// Packets dropped because the consumer did not drain fast enough.
    overflow_count: Arc<AtomicU32>,
    /// Capture cycles completed (for diagnostics).
    packet_count: Arc<AtomicU32>,
}

/// Describes the mix format of a loopback client.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MixSampleFormat {
    Float32,
    Int(u16), // bits per sample: 16, 24, or 32
}

impl WasapiLoopbackCapture {
    /// Open a loopback client on the given render endpoint (`None` = the
    /// system default). Does not start capturing.
    pub fn new(
        target_device: Option<&str>,
        ring_capacity_frames: usize,
    ) -> Result<Self, OutputError> {
        // One COM apartment per thread; the capture thread does not use COM
        // (it only touches the already-activated interfaces).
        if !unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok() {
            return Err(OutputError::StreamOpen("CoInitializeEx failed".to_string()));
        }
        let _com = ComInitGuard;

        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| {
                OutputError::StreamOpen(format!("CoCreateInstance(MMDeviceEnumerator): {e}"))
            })?;
        let (device, device_name) = select_device(&enumerator, target_device)?;

        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| OutputError::StreamOpen(format!("IAudioClient activate: {e}")))?;

        // The mix format is mandatory for loopback: the client is initialized
        // in shared mode, which requires a format the engine mixer supports
        // (typically the mix format itself).
        let mix: *mut WAVEFORMATEX = unsafe { client.GetMixFormat() }
            .map_err(|e| OutputError::StreamOpen(format!("GetMixFormat: {e}")))?;
        let sample_rate = unsafe { (*mix).nSamplesPerSec };
        let channels = unsafe { (*mix).nChannels }.clamp(1, MAX_CHANNELS as u16);
        let sample_format = classify_mix_format(unsafe { &*mix }).ok_or_else(|| {
            OutputError::StreamOpen(format!(
                "unsupported WASAPI mix format: tag=0x{:04X} bits={}",
                unsafe { (*mix).wFormatTag },
                unsafe { (*mix).wBitsPerSample }
            ))
        })?;

        // Event-driven shared loopback stream. 500 ms buffer: plenty for a
        // non-realtime consumer to drain; the ring absorbs scheduling jitter.
        // `Initialize` must see the mix format pointer before it is freed.
        const BUFFER_HNS: i64 = 500 * 10_000;
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_HNS,
                0,
                mix,
                None,
            )
        }
        .map_err(|e| OutputError::StreamOpen(format!("IAudioClient::Initialize(loopback): {e}")))?;
        unsafe { CoTaskMemFree(Some(mix as *const _)) };

        let capture_client: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|e| {
            OutputError::StreamOpen(format!("GetService(IAudioCaptureClient): {e}"))
        })?;

        let event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| OutputError::StreamOpen(format!("CreateEventW: {e}")))?;
        unsafe { client.SetEventHandle(event) }
            .map_err(|e| OutputError::StreamOpen(format!("SetEventHandle: {e}")))?;

        // After Initialize, the client is in the shared-mode stream; nothing
        // else uses COM on this thread, so the guard is disarmed.
        std::mem::forget(_com);

        let buffer = Arc::new(
            FixedFrameBuffer::new(ring_capacity_frames)
                .map_err(|e| OutputError::StreamOpen(format!("ring allocation: {e}")))?,
        );

        log::info!(
            "WASAPI loopback: '{}' mix format {} Hz / {} ch / {:?}",
            device_name,
            sample_rate,
            channels,
            sample_format
        );

        Ok(Self {
            client: SendCom(client),
            capture_client: SendCom(capture_client),
            buffer,
            device_name,
            sample_rate,
            channels,
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
            event: Some(SendHandle(event)),
            overflow_count: Arc::new(AtomicU32::new(0)),
            packet_count: Arc::new(AtomicU32::new(0)),
        })
    }

    /// The shared capture ring (interleaved f32).
    pub fn buffer(&self) -> Arc<FixedFrameBuffer> {
        Arc::clone(&self.buffer)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Packets dropped because the consumer lagged.
    pub fn take_overflows(&self) -> u32 {
        self.overflow_count.swap(0, Ordering::Relaxed)
    }

    pub fn take_packets(&self) -> u32 {
        self.packet_count.swap(0, Ordering::Relaxed)
    }

    /// Start the capture thread and the stream.
    pub fn start(&mut self) -> Result<(), OutputError> {
        if self.running.load(Ordering::Relaxed) {
            return Ok(());
        }
        unsafe { self.client.0.Start() }
            .map_err(|e| OutputError::StreamError(format!("IAudioClient::Start(loopback): {e}")))?;
        self.running.store(true, Ordering::Release);

        let running = Arc::clone(&self.running);
        let overflow = Arc::clone(&self.overflow_count);
        let packets = Arc::clone(&self.packet_count);
        let buffer = Arc::clone(&self.buffer);
        let capture = SendCom(self.capture_client.0.clone());
        // SendHandle exists precisely because HANDLE is not Send; the
        // capture thread owns the only waiter. The struct keeps the original
        // handle (a Copy wrapper) so `Drop` can close it exactly once after
        // the thread has been joined.
        let event = SendHandle(self.event.as_ref().unwrap().0);
        let channels = self.channels as usize;

        let handle = thread::Builder::new()
            .name("wasapi-loopback".to_string())
            .spawn(move || {
                capture_loop(capture, event, buffer, channels, running, overflow, packets)
            })
            .map_err(|e| OutputError::StreamError(format!("capture thread spawn: {e}")))?;
        self.thread = Some(handle);
        Ok(())
    }

    /// Stop the stream and join the capture thread.
    pub fn stop(&mut self) {
        if !self.running.swap(false, Ordering::AcqRel) {
            return;
        }
        let _ = unsafe { self.client.0.Stop() };
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }

    /// Drop buffered audio and reset counters.
    pub fn reset(&self) {
        self.buffer.reset();
    }
}

impl Drop for WasapiLoopbackCapture {
    fn drop(&mut self) {
        self.stop();
        if let Some(SendHandle(h)) = self.event.take() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

impl std::fmt::Debug for WasapiLoopbackCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiLoopbackCapture")
            .field("device_name", &self.device_name)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("running", &self.is_running())
            .finish()
    }
}

/// Classify the mix format into something the capture path can convert.
fn classify_mix_format(fmt: &WAVEFORMATEX) -> Option<MixSampleFormat> {
    let tag = fmt.wFormatTag as u32;
    match tag {
        WAVE_FORMAT_IEEE_FLOAT if fmt.wBitsPerSample == 32 => Some(MixSampleFormat::Float32),
        WAVE_FORMAT_PCM => Some(MixSampleFormat::Int(fmt.wBitsPerSample)),
        WAVE_FORMAT_EXTENSIBLE => {
            let ext = fmt as *const WAVEFORMATEX
                as *const windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE;
            let sub = unsafe { (*ext).SubFormat };
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && fmt.wBitsPerSample == 32 {
                Some(MixSampleFormat::Float32)
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                Some(MixSampleFormat::Int(fmt.wBitsPerSample))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The capture thread: wait for the buffer-end event, then drain every
/// available packet into the ring as interleaved f32.
fn capture_loop(
    capture: SendCom<IAudioCaptureClient>,
    event: SendHandle,
    buffer: Arc<FixedFrameBuffer>,
    channels: usize,
    running: Arc<AtomicBool>,
    overflow: Arc<AtomicU32>,
    packets: Arc<AtomicU32>,
) {
    // Reused scratch for one packet's samples.
    let mut scratch: Vec<f32> = Vec::new();

    while running.load(Ordering::Acquire) {
        // 50 ms timeout keeps shutdown responsive.
        let wait = unsafe { windows::Win32::System::Threading::WaitForSingleObject(event.0, 50) };
        if wait != WAIT_OBJECT_0 {
            continue;
        }

        // Drain all pending packets.
        loop {
            let mut frames: u32;
            match unsafe { capture.0.GetNextPacketSize() } {
                Ok(n) if n == 0 => break,
                Ok(n) => frames = n,
                Err(_) => break,
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut flags: u32 = 0;
            if unsafe {
                capture
                    .0
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            }
            .is_err()
            {
                break;
            }

            if frames > 0 {
                let total = frames as usize * channels;
                scratch.resize(total, 0.0);
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                if silent || data.is_null() {
                    scratch.fill(0.0);
                } else {
                    // The mix format is float32 (checked at open), so a raw
                    // copy suffices. If the mix format were integer, we would
                    // need conversion here; classify_mix_format currently only
                    // admits float32 mixes.
                    let src = unsafe { std::slice::from_raw_parts(data as *const f32, total) };
                    scratch.copy_from_slice(src);
                }
                let written = buffer.push_frames_interleaved(&scratch, channels);
                if written < frames as usize {
                    overflow.fetch_add((frames as usize - written) as u32, Ordering::Relaxed);
                }
                packets.fetch_add(1, Ordering::Relaxed);
            }

            unsafe {
                let _ = capture.0.ReleaseBuffer(frames);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_format_classification() {
        // Plain float32.
        let f32fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            wBitsPerSample: 32,
            ..Default::default()
        };
        assert!(matches!(
            classify_mix_format(&f32fmt),
            Some(MixSampleFormat::Float32)
        ));

        // Plain 16-bit PCM.
        let pcm16 = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            wBitsPerSample: 16,
            ..Default::default()
        };
        assert!(matches!(
            classify_mix_format(&pcm16),
            Some(MixSampleFormat::Int(16))
        ));

        // Unsupported tags are rejected.
        let weird = WAVEFORMATEX {
            wFormatTag: 0x00FF,
            ..Default::default()
        };
        assert!(classify_mix_format(&weird).is_none());
    }
}
