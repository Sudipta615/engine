//! The engine-facing audio output interface.
//!
//! Every output backend — `CpalOutput` (cpal-based, all platforms) and
//! `WasapiOutput` (native `IAudioClient` exclusive mode on Windows) —
//! implements this trait, so the engine can drive either without knowing
//! which transport is underneath. This is what makes a real
//! exclusive-mode backend *verifiable*: the trait's `output_info()` /
//! `capabilities()` carry the `OutputAccessState`, and a native backend can
//! report `verified: true` because it has OS-level confirmation of exclusive
//! access (e.g. `IAudioClient::Initialize` with
//! `AUDCLNT_SHAREMODE_EXCLUSIVE` succeeded), while a cpal stream can only
//! ever report shared mode.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use cpal::SampleFormat;
use crossbeam::queue::ArrayQueue;

use crate::buffer::{DsdByteBuffer, FixedFrameBuffer};
use crate::decode::dsd::DsdWireFormat;
use crate::output::capabilities::OutputCapabilities;
use crate::output::cpal_output::{OutputError, OutputVolume};
use crate::output::output_info::OutputInfo;

/// Maximum number of transport failures retained between engine ticks. The
/// queue is intentionally bounded: reporting an error must never block an
/// audio/backend callback.
pub const STREAM_ERROR_QUEUE_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorKind {
    DeviceUnavailable,
    BackendSpecific,
    Unknown,
}

/// A diagnostic event emitted by an output transport. Unlike the old boolean
/// flag this keeps the error category, display text, and debug representation
/// for each failure, so unplug/replug storms remain actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamErrorEvent {
    pub kind: StreamErrorKind,
    pub error_type: &'static str,
    pub message: String,
    pub details: String,
}

impl StreamErrorEvent {
    pub fn from_cpal(error: &cpal::Error) -> Self {
        let details = format!("{error:?}");
        // cpal 0.18 unified all per-operation error types into `cpal::Error`;
        // classify by `ErrorKind`. Anything that isn't a recognized
        // device-availability signal is treated as a backend-specific error.
        let kind = match error.kind() {
            cpal::ErrorKind::DeviceNotAvailable
            | cpal::ErrorKind::DeviceChanged
            | cpal::ErrorKind::DeviceBusy => StreamErrorKind::DeviceUnavailable,
            _ => StreamErrorKind::BackendSpecific,
        };
        Self {
            kind,
            error_type: "cpal::Error",
            message: error.to_string(),
            details,
        }
    }

    pub fn backend(error_type: &'static str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            kind: StreamErrorKind::BackendSpecific,
            error_type,
            details: message.clone(),
            message,
        }
    }
}

/// A lock-free, bounded transport-error mailbox. `ArrayQueue` preserves
/// individual events; the overflow counter records how many more failures
/// occurred when a backend produced a burst larger than the queue.
#[derive(Clone)]
pub struct StreamErrorState {
    queue: Arc<ArrayQueue<StreamErrorEvent>>,
    dropped: Arc<AtomicU32>,
}

impl Default for StreamErrorState {
    fn default() -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(STREAM_ERROR_QUEUE_CAPACITY)),
            dropped: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl StreamErrorState {
    #[inline]
    pub fn report(&self, event: StreamErrorEvent) {
        if self.queue.push(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn take(&self) -> StreamErrorBatch {
        let mut events = Vec::new();
        while let Some(event) = self.queue.pop() {
            events.push(event);
        }
        StreamErrorBatch {
            events,
            dropped: self.dropped.swap(0, Ordering::AcqRel),
        }
    }
}

#[derive(Debug, Default)]
pub struct StreamErrorBatch {
    pub events: Vec<StreamErrorEvent>,
    pub dropped: u32,
}

impl StreamErrorBatch {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.dropped == 0
    }
}

/// Parameters for switching an output into native-DSD transport mode.
///
/// `wire_format` is the preferred format; a backend that cannot open that
/// exact format may negotiate another DSD format and report it via the return
/// value of [`Output::set_native_dsd`] — the engine must use the *actual*
/// negotiated format (and its byte layout) when feeding the buffer.
pub struct NativeDsdParams {
    /// Preferred DSD wire format (e.g. DSD_U8).
    pub wire_format: DsdWireFormat,
    /// DSD bit rate of the source (e.g. 2_822_400 for DSD64).
    pub bit_rate: u32,
    /// Channel count of the DSD source.
    pub channels: u16,
    /// Byte ring the engine pushes interleaved DSD bytes into; the backend's
    /// render thread drains it to the DAC.
    pub buffer: std::sync::Arc<DsdByteBuffer>,
}

/// A typed native-DSD capability candidate exposed by an output backend.
///
/// Empty `bit_rates` or `channels` means that the backend can only provide a
/// format-level candidate and will verify the exact rate/channel combination
/// during [`Output::set_native_dsd`]. This lets backends such as ALSA avoid
/// opening a second exclusive PCM handle merely to probe a device that is
/// already open, while the actual stream negotiation remains authoritative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDsdCapability {
    pub wire_format: DsdWireFormat,
    pub bit_rates: Vec<u32>,
    pub channels: Vec<u16>,
}

impl NativeDsdCapability {
    #[inline]
    pub fn supports(&self, bit_rate: u32, channels: u16) -> bool {
        (self.bit_rates.is_empty() || self.bit_rates.contains(&bit_rate))
            && (self.channels.is_empty() || self.channels.contains(&channels))
    }
}

/// Abstract audio output device.
///
/// Object-safe so the engine can hold `Option<Box<dyn Output>>`. Requires
/// [`OutputVolume`] so hardware-volume routing works uniformly across
/// backends (cpal's `OutputVolume` is implemented by every backend).
pub trait Output: OutputVolume + Send {
    /// The sample rate the device was opened at.
    fn sample_rate(&self) -> u32;

    /// The negotiated sample container (f32/i16/...). cpal's `SampleFormat`
    /// is used as the neutral vocabulary across backends.
    fn sample_format(&self) -> SampleFormat;

    /// The negotiated callback/device buffer size in frames — the
    /// output-device buffering term of the graph latency model.
    fn buffer_size_frames(&self) -> u32;

    /// Snapshot of how the stream was actually opened (backend, rate,
    /// access mode, exclusivity verification, fallback state).
    fn output_info(&self) -> OutputInfo;

    /// Capability snapshot of the current device. Native backends fill in a
    /// *verified* access state from real negotiation results.
    fn capabilities(&self) -> OutputCapabilities;

    /// The device name for diagnostics.
    fn device_name(&self) -> String;

    /// A stable OS/backend device identifier (spec §10 — "Profile matching
    /// must not depend solely on a human-readable device name").
    ///
    /// - WASAPI: the endpoint ID (`IMMDevice::GetId`, e.g.
    ///   `{0.0.0.00000000}.{guid}`) — stable across OS language changes and
    ///   renames.
    /// - ALSA: the `hw:`/`plughw:` card+device string used to open the
    ///   stream — already a stable ID.
    /// - cpal: `None` (cpal exposes no stable device ID; name matching
    ///   applies).
    fn device_id(&self) -> Option<String> {
        None
    }

    /// Re-open the stream at `target_sample_rate` Hz (used for DoP rates and
    /// sample-rate-policy reconfiguration). Returns the rate actually
    /// negotiated. The stream may be restarted by this call.
    ///
    /// The negotiated sample container and channel count are preserved; only
    /// the sample rate changes.
    fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError>;

    /// Re-open the stream at `target_sample_rate` Hz using the requested
    /// sample container (e.g. `I32` for DSD-over-PCM), preserving the current
    /// channel count. Returns the rate actually negotiated.
    ///
    /// Backends that negotiate a fixed container (or cannot honor a specific
    /// container request) may fall back to rate-only reconfiguration; callers
    /// must verify the negotiated container via `sample_format()`.
    fn reconfigure_sample_format(
        &mut self,
        target_sample_rate: u32,
        sample_format: SampleFormat,
    ) -> Result<u32, OutputError> {
        let _ = sample_format;
        self.reconfigure_sample_rate(target_sample_rate)
    }

    /// Drop any samples buffered for the device (on track load / seek).
    fn reset_buffer(&self);

    /// Number of buffer underruns since the last call (resets to zero on read).
    fn take_underruns(&self) -> u32;

    /// Number of hard-clipped samples since the last call (resets to zero on read).
    fn take_clips(&self) -> u32;

    /// Number of non-finite samples seen since the last call (resets to zero on read).
    fn take_nans(&self) -> u32;

    /// Take all transport errors reported since the previous engine tick.
    /// The queue is bounded and lock-free; `dropped` reports overflow rather
    /// than collapsing a burst into one boolean.
    fn take_stream_errors(&self) -> StreamErrorBatch;

    /// Enable/disable TPDF dither at the integer-quantization boundary.
    /// No-op for backends that output f32 natively (no quantization happens).
    fn set_dither_enabled(&self, enabled: bool);

    /// Stop pulling frames from the buffer (silence is emitted instead).
    fn pause(&self);

    /// Resume pulling frames from the buffer after a pause.
    fn resume(&self);

    /// Open and start the stream. After this returns `Ok`, the backend must
    /// be pulling frames from its `FixedFrameBuffer` and playing them.
    fn start(&mut self) -> Result<(), OutputError>;

    /// Stop and release the stream (device-level locks, e.g. WASAPI
    /// exclusive mode, must be released here).
    fn stop(&mut self);

    // ── Native DSD transport (§7) ─────────────────────────────────────────
    // Default implementations are "unsupported", so every existing backend
    // keeps working unchanged; only DSD-capable backends override them.

    /// DSD wire formats this output can transport natively (empty = none).
    ///
    /// Kept as a compatibility surface for callers that only need format
    /// names. New negotiation code should use
    /// [`Self::native_dsd_capability_matrix`].
    fn native_dsd_capabilities(&self) -> Vec<DsdWireFormat> {
        Vec::new()
    }

    /// Typed native-DSD capability candidates. Exact rate/channel support is
    /// verified by `set_native_dsd`; a backend may return empty axes when it
    /// cannot safely probe an already-open exclusive endpoint.
    fn native_dsd_capability_matrix(&self) -> Vec<NativeDsdCapability> {
        self.native_dsd_capabilities()
            .into_iter()
            .map(|wire_format| NativeDsdCapability {
                wire_format,
                bit_rates: Vec::new(),
                channels: Vec::new(),
            })
            .collect()
    }

    /// Switch between PCM mode (`None`) and native-DSD transport mode.
    ///
    /// `Some(params)` re-opens the stream in a DSD format (preferring
    /// `params.wire_format`) and starts draining `params.buffer` to the DAC;
    /// `None` returns to PCM mode. Returns the format actually negotiated
    /// (`Ok(None)` in PCM mode). Errors are explicit — the engine must never
    /// silently fall back.
    fn set_native_dsd(
        &mut self,
        _params: Option<NativeDsdParams>,
    ) -> Result<Option<DsdWireFormat>, OutputError> {
        Err(OutputError::StreamError(
            "native DSD transport is not supported by this backend".to_string(),
        ))
    }
}

/// Construct a concrete output for the requested backend.
///
/// Dispatch rules:
/// - Windows + `wasapi-native` + `AudioBackend::ExclusiveWasapi`: try the
///   native `WasapiOutput` first. A failure to negotiate a real exclusive
///   stream falls back to `CpalOutput`, which honestly reports shared mode
///   (cpal cannot deliver WASAPI exclusivity).
/// - Every other backend: `CpalOutput` (unchanged behavior).
#[cfg(all(target_os = "windows", feature = "wasapi-native"))]
pub fn create_output(
    buffer: std::sync::Arc<FixedFrameBuffer>,
    backend: config::AudioBackend,
    target_device: Option<&str>,
    fallback_policy: config::FallbackPolicy,
) -> Result<Box<dyn Output>, OutputError> {
    if backend == config::AudioBackend::ExclusiveWasapi {
        match super::wasapi_output::WasapiOutput::new(buffer.clone(), backend, target_device) {
            Ok(out) => {
                log::info!("Audio output: using native WASAPI exclusive backend");
                return Ok(Box::new(out));
            }
            Err(e) => {
                log::warn!(
                    "Native WASAPI exclusive backend unavailable ({e}); \
                     falling back to cpal WASAPI (shared mode)"
                );
            }
        }
    }
    if backend == config::AudioBackend::ExclusiveAsio {
        match super::asio_output::AsioOutput::new(target_device, 44100, buffer.clone()) {
            Ok(out) => {
                log::info!("Audio output: using native ASIO backend");
                return Ok(Box::new(out));
            }
            Err(e) => {
                log::warn!(
                    "Native ASIO backend unavailable ({e}); \
                     falling back to cpal"
                );
            }
        }
    }
    Ok(Box::new(super::cpal_output::CpalOutput::new_with_policy(
        buffer,
        backend,
        target_device,
        fallback_policy,
    )?))
}

/// Construct a concrete output for the requested backend (non-Windows).
///
/// Dispatch rules:
/// - Linux + `AudioBackend::ExclusiveAlsa`: try the native `AlsaOutput`
///   backend (`hw:`/`plughw:` direct nodes) first. A failure to open a
///   direct node falls back to `CpalOutput`, which honestly reports shared
///   mode via its access state — never a guessed exclusivity claim.
/// - macOS + `AudioBackend::ExclusiveCoreAudioHog`: try the native
///   `CoreAudioOutput` backend (hog mode + direct IO proc) first. A failure
///   to claim the device falls back to `CpalOutput`, which honestly reports
///   shared mode — never a silent exclusivity claim.
/// - Every other backend: `CpalOutput` (unchanged behavior).
#[cfg(not(all(target_os = "windows", feature = "wasapi-native")))]
pub fn create_output(
    buffer: std::sync::Arc<FixedFrameBuffer>,
    backend: config::AudioBackend,
    target_device: Option<&str>,
    fallback_policy: config::FallbackPolicy,
    ) -> Result<Box<dyn Output>, OutputError> {
    #[cfg(target_os = "linux")]
    if backend == config::AudioBackend::ExclusiveAlsa {
        match super::alsa_output::AlsaOutput::new(
            buffer.clone(),
            backend,
            target_device,
            fallback_policy,
        ) {
            Ok(out) => {
                log::info!("Audio output: using native ALSA exclusive backend");
                return Ok(Box::new(out));
            }
            Err(e) => {
                log::warn!(
                    "Native ALSA exclusive backend unavailable ({e}); \
                     falling back to cpal ALSA (shared mode)"
                );
            }
        }
    }
    #[cfg(target_os = "macos")]
    if backend == config::AudioBackend::ExclusiveCoreAudioHog {
        match super::coreaudio_output::CoreAudioOutput::new(buffer.clone(), backend, target_device)
        {
            Ok(out) => {
                log::info!("Audio output: using native CoreAudio hog-mode backend");
                return Ok(Box::new(out));
            }
            Err(e) => {
                log::warn!(
                    "Native CoreAudio hog-mode backend unavailable ({e}); \
                     falling back to cpal CoreAudio (shared mode)"
                );
            }
        }
    }
    if backend == config::AudioBackend::ExclusiveAsio {
        match super::asio_output::AsioOutput::new(target_device, 44100, buffer.clone()) {
            Ok(out) => {
                log::info!("Audio output: using native ASIO backend");
                return Ok(Box::new(out));
            }
            Err(e) => {
                log::warn!(
                    "Native ASIO backend unavailable ({e}); \
                     falling back to cpal"
                );
            }
        }
    }
    Ok(Box::new(super::cpal_output::CpalOutput::new_with_policy(
        buffer,
        backend,
        target_device,
        fallback_policy,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_error_state_preserves_events_and_reports_overflow() {
        let state = StreamErrorState::default();
        for i in 0..(STREAM_ERROR_QUEUE_CAPACITY + 3) {
            state.report(StreamErrorEvent::backend(
                "test::Backend",
                format!("failure-{i}"),
            ));
        }
        let batch = state.take();
        assert_eq!(batch.events.len(), STREAM_ERROR_QUEUE_CAPACITY);
        assert_eq!(batch.dropped, 3);
        assert_eq!(batch.events[0].message, "failure-0");
        assert_eq!(
            batch.events[STREAM_ERROR_QUEUE_CAPACITY - 1].message,
            "failure-15"
        );
        assert!(state.take().is_empty());
    }

    #[test]
    fn native_dsd_capability_matches_typed_rate_and_channel_contract() {
        let capability = NativeDsdCapability {
            wire_format: DsdWireFormat::U32Le,
            bit_rates: vec![2_822_400, 5_644_800],
            channels: vec![2],
        };
        assert!(capability.supports(2_822_400, 2));
        assert!(capability.supports(5_644_800, 2));
        assert!(!capability.supports(11_289_600, 2));
        assert!(!capability.supports(2_822_400, 6));

        let format_only = NativeDsdCapability {
            wire_format: DsdWireFormat::U8,
            bit_rates: Vec::new(),
            channels: Vec::new(),
        };
        assert!(format_only.supports(45_158_400, 8));
    }
}

