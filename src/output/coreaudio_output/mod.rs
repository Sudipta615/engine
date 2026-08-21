//! Native CoreAudio output backend — macOS hog-mode exclusive output.
//!
//! # What this backend provides (spec §11)
//!
//! - **Hog mode**: claims `kAudioDevicePropertyHogMode` so no other process
//!   can open the device; the claim is *verified* (re-read after set), and a
//!   device already owned by another process is rejected with an explicit
//!   error — never a silent shared fallback.
//! - **Direct IO proc rendering**: a raw `AudioDeviceIOProcID` render
//!   callback pulls frames from the engine's lock-free `FixedFrameBuffer`,
//!   bypassing the system mixer entirely.
//! - **Verified access state**: `OutputAccessState { requested: Exclusive,
//!   actual: Exclusive, verified: true }` only after hog + start succeed.
//! - **Stable device identity**: `device_id()` returns the device UID
//!   (`kAudioDevicePropertyDeviceUID`), the stable identity used by output
//!   profiles (§10).
//! - **Hardware volume**: device-scoped virtual main/master volume.
//! - **DoP**: `reconfigure_sample_format(rate, I32)` sets each output
//!   stream's virtual format to 32-bit-integer at `bit_rate/16` for
//!   DSD-over-PCM (24-bit DoP words in the I32 container).
//! - **Native DSD**: not a CoreAudio concept; `set_native_dsd` returns an
//!   explicit error so the engine's Native → DoP → PCM fallback chain stays
//!   observable (§7, §28).
//!
//! # Format handling
//!
//! The backend is **adaptive**: it reads the device's current stream format
//! (`kAudioStreamPropertyVirtualFormat`) and converts the engine's f32
//! interleaved stream to it in the callback. Supported containers: f32, f64,
//! i16, u16, 24-bit packed (i24), i32 — all little-endian LinearPCM. A
//! device presenting any other format is rejected at `start()` with an
//! explicit error.
//!
//! # Realtime safety
//!
//! The render callback (see [`render`]) is allocation-free and lock-free;
//! all negotiation happens on the control thread before `AudioDeviceStart`.
//!
//! # Validation status
//!
//! This module is macOS-only and requires a Mac to *link and run*. The code
//! is written against `objc2-core-audio`'s pre-generated bindings (no
//! build-time bindgen), so it can be reviewed without the macOS SDK, but it
//! MUST be compiled and hardware-tested on macOS before release. Runtime
//! validation steps are listed in the module docs of `output::create_output`.

mod hal;
mod render;

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use config::AudioBackend;
use cpal::SampleFormat;
use objc2_core_audio::{
    kAudioDevicePropertyAvailableNominalSampleRates, kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceUID, kAudioDevicePropertyHogMode,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreamConfiguration,
    kAudioDevicePropertyStreams, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioStreamPropertyVirtualFormat, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioObjectID,
};
// Data types are NOT re-exported by objc2-core-audio; import them from the
// companion types crate (same version, feature-unified).
use objc2_core_audio_types::{
    kAudioFormatLinearPCM, kLinearPCMFormatFlagIsBigEndian, kLinearPCMFormatFlagIsFloat,
    kLinearPCMFormatFlagIsNonInterleaved, kLinearPCMFormatFlagIsSignedInteger, AudioBufferList,
    AudioStreamBasicDescription, AudioValueRange,
};

use crate::buffer::FixedFrameBuffer;
use crate::decode::dsd::DsdWireFormat;
use crate::output::capabilities::STANDARD_RATES;
use crate::output::capabilities::{OutputAccessMode, OutputAccessState, OutputCapabilities};
use crate::output::cpal_output::{OutputError, OutputVolume};
use crate::output::format_converter::TargetFormat;
use crate::output::output::{NativeDsdParams, StreamErrorState};
use crate::output::output::{Output, StreamErrorBatch};
use crate::output::output_info::OutputInfo;

use hal::{addr, get, get_cfstring, get_opt, is_settable, property_size, set, NO_ERROR};

/// Read the current hog-mode owner of a device (PID, or -1 when unowned).
fn hog_owner(device: AudioObjectID) -> Option<i32> {
    let mut a = addr(
        kAudioDevicePropertyHogMode,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    unsafe { get_opt(device, &mut a) }
}

/// Release the hog claim on a device.
fn release_hog(device: AudioObjectID) {
    let mut a = addr(
        kAudioDevicePropertyHogMode,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    let release: i32 = -1;
    let _ = unsafe { set(device, &mut a, &release) };
}

/// The default output device (`kAudioObjectSystemObject` +
/// `kAudioHardwarePropertyDefaultOutputDevice`).
fn default_output_device() -> Option<AudioObjectID> {
    let mut a = addr(
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    unsafe { get_opt(kAudioObjectSystemObject as AudioObjectID, &mut a) }
}

/// Enumerate all audio devices (array of `AudioObjectID`).
fn all_devices() -> Vec<AudioObjectID> {
    let mut a = addr(
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    let mut out = Vec::new();
    // SAFETY: property read with a size query; the HAL fills the array.
    let size = unsafe { property_size(kAudioObjectSystemObject as AudioObjectID, &mut a) };
    let Ok(size) = size else { return out };
    if size == 0 || size % std::mem::size_of::<AudioObjectID>() != 0 {
        return out;
    }
    out.resize(size / std::mem::size_of::<AudioObjectID>(), 0);
    let mut actual = size as u32;
    // SAFETY: `out` is `size` bytes of valid AudioObjectIDs.
    let status = unsafe {
        objc2_core_audio::AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&mut a),
            0,
            std::ptr::null::<c_void>(),
            NonNull::from(&mut actual),
            NonNull::new(out.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if status != NO_ERROR {
        return Vec::new();
    }
    out
}

/// Resolve a device by stable UID first, then by case-insensitive name
/// substring; falls back to the default output device.
fn resolve_device(target: Option<&str>) -> Option<(AudioObjectID, String, Option<String>)> {
    let default = default_output_device();
    let Some(target) = target.filter(|t| !t.is_empty()) else {
        let id = default?;
        return Some((id, device_display_name(id), device_uid(id)));
    };

    let target_lower = target.to_lowercase();
    for id in all_devices() {
        if let Some(uid) = device_uid(id) {
            if uid.eq_ignore_ascii_case(target) {
                return Some((id, device_display_name(id), Some(uid)));
            }
        }
    }
    for id in all_devices() {
        let name = device_display_name(id);
        if name.to_lowercase().contains(&target_lower) {
            return Some((id, name, device_uid(id)));
        }
    }
    // No match: fall back to the default device (the caller's fallback
    // policy decides whether that is acceptable).
    let id = default?;
    Some((id, device_display_name(id), device_uid(id)))
}

/// Read a device's stable UID (`kAudioDevicePropertyDeviceUID`, a CFString).
fn device_uid(device: AudioObjectID) -> Option<String> {
    let mut a = addr(
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    unsafe { get_cfstring(device, &mut a) }
}

/// Read a human-readable device name (CFString property; fallback to the
/// deprecated char-array property, then a numeric placeholder).
fn device_display_name(device: AudioObjectID) -> String {
    let mut a = addr(
        objc2_core_audio::kAudioDevicePropertyDeviceNameCFString,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    if let Some(name) = unsafe { get_cfstring(device, &mut a) } {
        return name;
    }
    let mut a = addr(
        objc2_core_audio::kAudioDevicePropertyDeviceName,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    let mut name = [0u8; 64];
    let mut size = 64u32;
    // SAFETY: `name` is 64 bytes, matching the deprecated char[64] property.
    let status = unsafe {
        objc2_core_audio::AudioObjectGetPropertyData(
            device,
            NonNull::from(&mut a),
            0,
            std::ptr::null::<c_void>(),
            NonNull::from(&mut size),
            NonNull::new(name.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if status == NO_ERROR {
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let s = String::from_utf8_lossy(&name[..end]).into_owned();
        if !s.is_empty() {
            return s;
        }
    }
    format!("CoreAudio Device {device}")
}

/// All output stream IDs of a device (`kAudioDevicePropertyStreams`, scope
/// output).
fn output_streams(device: AudioObjectID) -> Vec<AudioObjectID> {
    let mut a = addr(
        kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeOutput,
        kAudioObjectPropertyElementMain,
    );
    let mut out = Vec::new();
    // SAFETY: size query then read; array of AudioObjectIDs.
    let size = unsafe { property_size(device, &mut a) };
    let Ok(size) = size else { return out };
    if size == 0 || size % std::mem::size_of::<AudioObjectID>() != 0 {
        return out;
    }
    out.resize(size / std::mem::size_of::<AudioObjectID>(), 0);
    let mut actual = size as u32;
    // SAFETY: `out` is `size` bytes of valid AudioObjectIDs.
    let status = unsafe {
        objc2_core_audio::AudioObjectGetPropertyData(
            device,
            NonNull::from(&mut a),
            0,
            std::ptr::null::<c_void>(),
            NonNull::from(&mut actual),
            NonNull::new(out.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if status != NO_ERROR {
        return Vec::new();
    }
    out
}

/// Total output channels of a device (`kAudioDevicePropertyStreamConfiguration`,
/// an `AudioBufferList` whose buffers describe each output stream).
fn output_channel_count(device: AudioObjectID) -> u16 {
    let mut a = addr(
        kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeOutput,
        kAudioObjectPropertyElementMain,
    );
    // SAFETY: size query; the property is an AudioBufferList.
    let size = unsafe { property_size(device, &mut a) };
    let Ok(size) = size else { return 0 };
    if size < std::mem::size_of::<u32>() {
        return 0;
    }
    let mut bytes = vec![0u8; size];
    let mut actual = size as u32;
    // SAFETY: `bytes` is `size` bytes for the AudioBufferList.
    let status = unsafe {
        objc2_core_audio::AudioObjectGetPropertyData(
            device,
            NonNull::from(&mut a),
            0,
            std::ptr::null::<c_void>(),
            NonNull::from(&mut actual),
            NonNull::new(bytes.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if status != NO_ERROR {
        return 0;
    }
    let list = bytes.as_ptr() as *const AudioBufferList;
    // SAFETY: the HAL filled a valid AudioBufferList of `mNumberBuffers`.
    let list_ref = unsafe { &*list };
    let mut total: u64 = 0;
    for i in 0..list_ref.mNumberBuffers as usize {
        // SAFETY: flexible-array access; `mNumberBuffers` buffers exist.
        let buf = unsafe { &*list_ref.mBuffers.as_ptr().add(i) };
        total += u64::from(buf.mNumberChannels);
    }
    total.min(u16::MAX as u64) as u16
}

/// The first output stream's virtual format (the format the IO proc sees).
fn stream_virtual_format(device: AudioObjectID) -> Option<AudioStreamBasicDescription> {
    let streams = output_streams(device);
    for stream in streams {
        let mut a = addr(
            kAudioStreamPropertyVirtualFormat,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        if let Ok(fmt) = unsafe { get(stream, &mut a) } {
            return Some(fmt);
        }
    }
    None
}

/// Available nominal sample rates as `(min, max)` ranges.
fn available_rates(device: AudioObjectID) -> Vec<(f64, f64)> {
    let mut a = addr(
        kAudioDevicePropertyAvailableNominalSampleRates,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    let mut out: Vec<AudioValueRange> = Vec::new();
    // SAFETY: size query then read; array of AudioValueRange.
    let size = unsafe { property_size(device, &mut a) };
    let Ok(size) = size else { return Vec::new() };
    if size == 0 || size % std::mem::size_of::<AudioValueRange>() != 0 {
        return Vec::new();
    }
    out.resize(
        size / std::mem::size_of::<AudioValueRange>(),
        AudioValueRange {
            mMinimum: 0.0,
            mMaximum: 0.0,
        },
    );
    let mut actual = size as u32;
    // SAFETY: `out` is `size` bytes of valid AudioValueRanges.
    let status = unsafe {
        objc2_core_audio::AudioObjectGetPropertyData(
            device,
            NonNull::from(&mut a),
            0,
            std::ptr::null::<c_void>(),
            NonNull::from(&mut actual),
            NonNull::new(out.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if status != NO_ERROR {
        return Vec::new();
    }
    out.into_iter().map(|r| (r.mMinimum, r.mMaximum)).collect()
}

/// Whether a nominal sample rate is supported by the device.
fn rate_supported(device: AudioObjectID, rate: u32) -> bool {
    if rate == 0 {
        return false;
    }
    let r = rate as f64;
    available_rates(device)
        .iter()
        .any(|&(min, max)| r >= min - 0.5 && r <= max + 0.5)
}

/// The device's current nominal sample rate.
fn current_nominal_rate(device: AudioObjectID) -> Option<u32> {
    let mut a = addr(
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    unsafe { get_opt(device, &mut a) }.map(|r: f64| r.round() as u32)
}

/// Set the device's nominal sample rate.
fn set_nominal_rate(device: AudioObjectID, rate: u32) -> Result<(), i32> {
    let mut a = addr(
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    let value = rate as f64;
    unsafe { set(device, &mut a, &value) }
}

/// The device's negotiated buffer size in frames.
fn buffer_size_frames(device: AudioObjectID) -> u32 {
    let mut a = addr(
        kAudioDevicePropertyBufferFrameSize,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    unsafe { get_opt(device, &mut a) }.unwrap_or(512)
}

/// Map an `AudioStreamBasicDescription` to the engine's conversion target.
/// Returns `(TargetFormat, bytes_per_sample, interleaved, OutputSampleFormat)`.
fn map_stream_format(
    fmt: &AudioStreamBasicDescription,
) -> Option<(
    TargetFormat,
    usize,
    bool,
    crate::dsp::pipeline::OutputSampleFormat,
)> {
    use crate::dsp::pipeline::OutputSampleFormat;
    if fmt.mFormatID != kAudioFormatLinearPCM {
        return None;
    }
    let flags = fmt.mFormatFlags;
    if flags & kLinearPCMFormatFlagIsBigEndian != 0 {
        return None;
    }
    let interleaved = flags & kLinearPCMFormatFlagIsNonInterleaved == 0;
    let is_float = flags & kLinearPCMFormatFlagIsFloat != 0;
    let is_signed_int = flags & kLinearPCMFormatFlagIsSignedInteger != 0;
    let bits = fmt.mBitsPerChannel;
    match (is_float, is_signed_int, bits) {
        (true, _, 32) => Some((TargetFormat::F32, 4, interleaved, OutputSampleFormat::F32)),
        (true, _, 64) => Some((TargetFormat::F64, 8, interleaved, OutputSampleFormat::F64)),
        (false, true, 16) => Some((TargetFormat::I16, 2, interleaved, OutputSampleFormat::I16)),
        (false, false, 16) => Some((TargetFormat::U16, 2, interleaved, OutputSampleFormat::U16)),
        (false, true, 24) => Some((
            TargetFormat::I24Le,
            3,
            interleaved,
            OutputSampleFormat::I24Le,
        )),
        (false, true, 32) => Some((TargetFormat::I32, 4, interleaved, OutputSampleFormat::I32)),
        _ => None,
    }
}

/// Build an `AudioStreamBasicDescription` for a requested container.
fn build_stream_description(
    rate: u32,
    channels: u32,
    target: TargetFormat,
) -> AudioStreamBasicDescription {
    let (bits, is_float) = match target {
        TargetFormat::F32 => (32, true),
        TargetFormat::F64 => (64, true),
        TargetFormat::I16 => (16, false),
        TargetFormat::U16 => (16, false),
        TargetFormat::I24Le => (24, false),
        TargetFormat::I32 => (32, false),
    };
    let bytes_per_sample = bits / 8;
    let mut flags = if is_float {
        kLinearPCMFormatFlagIsFloat
    } else {
        kLinearPCMFormatFlagIsSignedInteger
    };
    if target == TargetFormat::U16 {
        flags &= !kLinearPCMFormatFlagIsSignedInteger;
    }
    // Packed, native endian (0 = little-endian on macOS hardware).
    AudioStreamBasicDescription {
        mSampleRate: rate as f64,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: flags,
        mBytesPerPacket: bytes_per_sample * channels,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_sample * channels,
        mChannelsPerFrame: channels,
        mBitsPerChannel: bits,
        mReserved: 0,
    }
}

/// The native CoreAudio output backend.
pub struct CoreAudioOutput {
    buffer: Arc<FixedFrameBuffer>,
    device: AudioObjectID,
    device_name: String,
    device_uid: Option<String>,
    backend: AudioBackend,
    dither_enabled: bool,
    // Negotiated state (updated on start / reconfigure).
    sample_rate: u32,
    channels: u16,
    buffer_size: u32,
    target: TargetFormat,
    bytes_per_sample: usize,
    sample_format: crate::dsp::pipeline::OutputSampleFormat,
    interleaved: bool,
    // Runtime state.
    hogged: bool,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    in_callback: Arc<AtomicBool>,
    underruns: Arc<AtomicU32>,
    clips: Arc<AtomicU32>,
    nans: Arc<AtomicU32>,
    error_state: StreamErrorState,
    io_proc: Option<AudioDeviceIOProcID>,
    render_ctx: Option<Box<render::RenderContext>>,
    access_state: OutputAccessState,
    is_fallback: bool,
    fallback_reason: Option<String>,
}

impl CoreAudioOutput {
    /// Open a device handle (no stream). `target_device` may be a stable UID,
    /// a case-insensitive name substring, or `None` for the default output.
    pub fn new(
        buffer: Arc<FixedFrameBuffer>,
        backend: AudioBackend,
        target_device: Option<&str>,
    ) -> Result<Self, OutputError> {
        let (device, name, uid) = resolve_device(target_device).ok_or(OutputError::NoDevice)?;
        // A quick sanity check that the device is alive before we touch it.
        {
            let mut a = addr(
                kAudioDevicePropertyDeviceIsAlive,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            );
            if unsafe { get_opt::<u32>(device, &mut a) } != Some(1) {
                return Err(OutputError::StreamError(format!(
                    "CoreAudio device '{name}' is not alive"
                )));
            }
        }

        let sample_rate = current_nominal_rate(device).unwrap_or(44_100);
        let channels = output_channel_count(device).max(2);
        let buffer_size = buffer_size_frames(device);
        let (target, sample_format, interleaved) =
            match stream_virtual_format(device).and_then(|f| map_stream_format(&f)) {
                Some((t, _, i, of)) => (t, of, i),
                None => {
                    // Adaptive fallback: default to f32 conversion state; the
                    // real format is re-queried at start() and validated then.
                    (
                        TargetFormat::F32,
                        crate::dsp::pipeline::OutputSampleFormat::F32,
                        true,
                    )
                }
            };

        Ok(Self {
            buffer,
            device,
            device_name: name,
            device_uid: uid,
            backend,
            dither_enabled: true,
            sample_rate,
            channels,
            buffer_size,
            target,
            bytes_per_sample: match target {
                TargetFormat::F32 | TargetFormat::I32 => 4,
                TargetFormat::F64 => 8,
                TargetFormat::I16 | TargetFormat::U16 => 2,
                TargetFormat::I24Le => 3,
            },
            sample_format,
            interleaved,
            hogged: false,
            running: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            in_callback: Arc::new(AtomicBool::new(false)),
            underruns: Arc::new(AtomicU32::new(0)),
            clips: Arc::new(AtomicU32::new(0)),
            nans: Arc::new(AtomicU32::new(0)),
            error_state: StreamErrorState::default(),
            io_proc: None,
            render_ctx: None,
            access_state: OutputAccessState {
                requested: OutputAccessMode::Exclusive,
                actual: OutputAccessMode::Shared,
                verified: false,
            },
            is_fallback: false,
            fallback_reason: None,
        })
    }

    /// Claim hog mode. Returns an explicit error when another process owns
    /// the device.
    fn claim_hog(&mut self) -> Result<(), OutputError> {
        if self.hogged {
            return Ok(());
        }
        let my_pid = std::process::id() as i32;
        match hog_owner(self.device) {
            Some(owner) if owner != -1 && owner != my_pid => {
                return Err(OutputError::StreamError(format!(
                    "CoreAudio device '{}' is in use by another process (hog owner PID {})",
                    self.device_name, owner
                )));
            }
            _ => {}
        }
        let mut a = addr(
            kAudioDevicePropertyHogMode,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        unsafe { set(self.device, &mut a, &my_pid) }.map_err(|status| {
            OutputError::StreamError(format!(
                "CoreAudio refused hog mode for '{}' (OSStatus {status})",
                self.device_name
            ))
        })?;
        // Verify the claim actually landed (spec: never infer exclusivity).
        match hog_owner(self.device) {
            Some(owner) if owner == my_pid => {
                self.hogged = true;
                Ok(())
            }
            other => {
                release_hog(self.device);
                Err(OutputError::StreamError(format!(
                    "CoreAudio hog claim not verified for '{}' (owner: {other:?})",
                    self.device_name
                )))
            }
        }
    }

    /// Re-query the device format/channels/buffer and validate it is a
    /// container we can render. Updates the negotiated state.
    fn refresh_format(&mut self) -> Result<(), OutputError> {
        self.sample_rate = current_nominal_rate(self.device).unwrap_or(self.sample_rate);
        self.channels = output_channel_count(self.device).max(2);
        self.buffer_size = buffer_size_frames(self.device);
        match stream_virtual_format(self.device).and_then(|f| map_stream_format(&f)) {
            Some((target, bps, interleaved, of)) => {
                self.target = target;
                self.sample_format = of;
                self.interleaved = interleaved;
                self.bytes_per_sample = bps;
                Ok(())
            }
            None => Err(OutputError::StreamOpen(format!(
                "CoreAudio device '{}' presents an unsupported stream format \
                 (non-LinearPCM, big-endian, or >32-bit)",
                self.device_name
            ))),
        }
    }

    /// Start the IO proc (must hold hog mode first).
    fn start_stream(&mut self) -> Result<(), OutputError> {
        self.refresh_format()?;
        let channels = self.channels as usize;
        let ctx = render::RenderContext::new(
            Arc::clone(&self.buffer),
            Arc::clone(&self.paused),
            Arc::clone(&self.in_callback),
            Arc::clone(&self.underruns),
            Arc::clone(&self.clips),
            Arc::clone(&self.nans),
            Arc::new(AtomicBool::new(self.dither_enabled)),
            channels,
            self.target,
            self.bytes_per_sample,
            self.interleaved,
            self.buffer_size as usize,
        );
        let ctx_ptr = Box::into_raw(Box::new(ctx));
        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: `ctx_ptr` is a valid, leaked RenderContext; the proc keeps
        // the pointer for its lifetime and we reclaim it in stop_stream.
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                self.device,
                Some(render::render_io_proc),
                ctx_ptr as *mut c_void,
                NonNull::from(&mut proc_id),
            )
        };
        if status != NO_ERROR {
            // SAFETY: reclaim the box we leaked above.
            unsafe { render::reclaim_context(ctx_ptr as *mut c_void) };
            return Err(OutputError::StreamOpen(format!(
                "CoreAudio IO proc creation failed for '{}' (OSStatus {status})",
                self.device_name
            )));
        }
        if proc_id.is_none() {
            // SAFETY: reclaim the box we leaked above.
            unsafe { render::reclaim_context(ctx_ptr as *mut c_void) };
            return Err(OutputError::StreamOpen(format!(
                "CoreAudio returned no IO proc for '{}'",
                self.device_name
            )));
        }
        let status = unsafe { AudioDeviceStart(self.device, proc_id) };
        if status != NO_ERROR {
            let _ = unsafe { AudioDeviceDestroyIOProcID(self.device, proc_id) };
            // SAFETY: reclaim the box we leaked above.
            unsafe { render::reclaim_context(ctx_ptr as *mut c_void) };
            return Err(OutputError::StreamOpen(format!(
                "CoreAudio start failed for '{}' (OSStatus {status})",
                self.device_name
            )));
        }
        self.io_proc = Some(proc_id);
        self.render_ctx = Some(unsafe { Box::from_raw(ctx_ptr) });
        self.running.store(true, Ordering::Release);
        self.access_state = OutputAccessState {
            requested: OutputAccessMode::Exclusive,
            actual: OutputAccessMode::Exclusive,
            verified: true,
        };
        Ok(())
    }

    /// Stop the IO proc and reclaim the render context (hog stays claimed so
    /// reconfigure can restart quickly).
    fn stop_stream(&mut self) {
        if let Some(proc) = self.io_proc.take() {
            let _ = unsafe { AudioDeviceStop(self.device, proc) };
            let _ = unsafe { AudioDeviceDestroyIOProcID(self.device, proc) };
        }
        if let Some(ctx) = self.render_ctx.take() {
            // SAFETY: the proc was destroyed above, so no callback references
            // the context any more.
            unsafe {
                render::reclaim_context(Box::into_raw(ctx) as *mut c_void);
            }
        }
        self.running.store(false, Ordering::Release);
        self.access_state.verified = false;
        self.access_state.actual = OutputAccessMode::Shared;
    }

    fn build_output_info(&self) -> OutputInfo {
        OutputInfo {
            requested_backend: Some(self.backend),
            actual_backend: Some(self.backend),
            requested_rate: self.sample_rate,
            actual_rate: self.sample_rate,
            channels: self.channels,
            buffer_size_frames: self.buffer_size,
            buffer_size_estimated: false,
            sample_format: self.sample_format,
            dither_enabled: self.dither_enabled,
            access_mode: self.access_state.actual,
            access_state: self.access_state,
            is_fallback: self.is_fallback,
            fallback_reason: self.fallback_reason.clone(),
            is_exclusive: self.access_state.is_bit_perfect(),
            device_name: self.device_name.clone(),
        }
    }
}

impl Output for CoreAudioOutput {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn sample_format(&self) -> SampleFormat {
        match self.sample_format {
            crate::dsp::pipeline::OutputSampleFormat::F32 => SampleFormat::F32,
            crate::dsp::pipeline::OutputSampleFormat::F64 => SampleFormat::F64,
            crate::dsp::pipeline::OutputSampleFormat::I16 => SampleFormat::I16,
            crate::dsp::pipeline::OutputSampleFormat::U16 => SampleFormat::U16,
            // 24-bit-in-32 reports the I32 container width, mirroring the
            // WASAPI backend's vocabulary.
            crate::dsp::pipeline::OutputSampleFormat::I24Le => SampleFormat::I32,
            crate::dsp::pipeline::OutputSampleFormat::I32 => SampleFormat::I32,
            crate::dsp::pipeline::OutputSampleFormat::Unknown => SampleFormat::F32,
        }
    }

    fn buffer_size_frames(&self) -> u32 {
        self.buffer_size
    }

    fn output_info(&self) -> OutputInfo {
        self.build_output_info()
    }

    fn capabilities(&self) -> OutputCapabilities {
        let mut sample_rates: Vec<u32> = Vec::new();
        let mut hardware_ranges: Vec<(u32, u32)> = Vec::new();
        for (min, max) in available_rates(self.device) {
            let min = min.round() as u32;
            let max = max.round() as u32;
            if !hardware_ranges.contains(&(min, max)) {
                hardware_ranges.push((min, max));
            }
            for &r in STANDARD_RATES.iter() {
                let r = r as u32;
                if r >= min && r <= max && !sample_rates.contains(&r) {
                    sample_rates.push(r);
                }
            }
        }
        sample_rates.sort_unstable();
        hardware_ranges.sort_unstable();
        let formats = vec![match self.sample_format {
            crate::dsp::pipeline::OutputSampleFormat::F32 => SampleFormat::F32,
            crate::dsp::pipeline::OutputSampleFormat::F64 => SampleFormat::F64,
            crate::dsp::pipeline::OutputSampleFormat::I16 => SampleFormat::I16,
            crate::dsp::pipeline::OutputSampleFormat::U16 => SampleFormat::U16,
            _ => SampleFormat::I32,
        }];
        OutputCapabilities {
            sample_rates,
            hardware_ranges,
            formats,
            channels: vec![self.channels],
            device_name: self.device_name.clone(),
            access_mode: self.access_state.actual,
            access_state: self.access_state,
            likely_direct_access: self.access_state.verified,
            supports_exclusive: self.access_state.verified,
        }
    }

    fn device_name(&self) -> String {
        self.device_name.clone()
    }

    fn device_id(&self) -> Option<String> {
        self.device_uid.clone()
    }

    fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError> {
        if self.sample_rate == target_sample_rate {
            return Ok(self.sample_rate);
        }
        if !rate_supported(self.device, target_sample_rate) {
            return Err(OutputError::StreamError(format!(
                "CoreAudio device '{}' does not support {target_sample_rate} Hz",
                self.device_name
            )));
        }
        let was_running = self.running.load(Ordering::Acquire);
        if was_running {
            self.stop_stream();
        }
        set_nominal_rate(self.device, target_sample_rate).map_err(|status| {
            OutputError::StreamError(format!(
                "CoreAudio refused {target_sample_rate} Hz for '{}' (OSStatus {status})",
                self.device_name
            ))
        })?;
        self.sample_rate = current_nominal_rate(self.device).unwrap_or(target_sample_rate);
        if was_running {
            self.start_stream()?;
        }
        Ok(self.sample_rate)
    }

    fn reconfigure_sample_format(
        &mut self,
        target_sample_rate: u32,
        sample_format: SampleFormat,
    ) -> Result<u32, OutputError> {
        let target = match sample_format {
            SampleFormat::F32 => TargetFormat::F32,
            SampleFormat::F64 => TargetFormat::F64,
            SampleFormat::I16 => TargetFormat::I16,
            SampleFormat::U16 => TargetFormat::U16,
            SampleFormat::I32 => TargetFormat::I32,
            _ => {
                return Err(OutputError::StreamError(format!(
                    "CoreAudio backend cannot negotiate container {sample_format:?}"
                )));
            }
        };
        if !rate_supported(self.device, target_sample_rate) {
            return Err(OutputError::StreamError(format!(
                "CoreAudio device '{}' does not support {target_sample_rate} Hz",
                self.device_name
            )));
        }
        let was_running = self.running.load(Ordering::Acquire);
        if was_running {
            self.stop_stream();
        }
        // Attempt to set the nominal rate first.
        if set_nominal_rate(self.device, target_sample_rate).is_ok() {
            self.sample_rate = current_nominal_rate(self.device).unwrap_or(target_sample_rate);
        } else {
            self.sample_rate = current_nominal_rate(self.device).unwrap_or(self.sample_rate);
        }
        // Attempt to set each output stream's virtual format. If any stream
        // refuses, fall back to the device's current format (reported via
        // `sample_format()` / `output_info()`, never assumed).
        let channels = self.channels.max(2) as u32;
        let desired = build_stream_description(self.sample_rate, channels, target);
        let streams = output_streams(self.device);
        let mut set_ok = true;
        for stream in streams {
            let mut a = addr(
                kAudioStreamPropertyVirtualFormat,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            );
            if !unsafe { is_settable(stream, &mut a) } {
                set_ok = false;
                break;
            }
            if unsafe { set(stream, &mut a, &desired) }.is_err() {
                set_ok = false;
                break;
            }
        }
        if set_ok {
            if let Some(fmt) =
                stream_virtual_format(self.device).and_then(|f| map_stream_format(&f))
            {
                self.target = fmt.0;
                self.bytes_per_sample = fmt.1;
                self.interleaved = fmt.2;
                self.sample_format = fmt.3;
            }
        }
        if was_running {
            self.start_stream()?;
        }
        Ok(self.sample_rate)
    }

    fn reset_buffer(&self) {
        self.buffer.reset();
    }

    fn take_underruns(&self) -> u32 {
        self.underruns.swap(0, Ordering::AcqRel)
    }

    fn take_clips(&self) -> u32 {
        self.clips.swap(0, Ordering::AcqRel)
    }

    fn take_nans(&self) -> u32 {
        self.nans.swap(0, Ordering::AcqRel)
    }

    fn take_stream_errors(&self) -> StreamErrorBatch {
        self.error_state.take()
    }

    fn set_dither_enabled(&self, enabled: bool) {
        if let Some(ctx) = self.render_ctx.as_ref() {
            ctx.dither_enabled.store(enabled, Ordering::Release);
        }
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    fn start(&mut self) -> Result<(), OutputError> {
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }
        self.claim_hog()?;
        if let Err(e) = self.start_stream() {
            // Release the hog so the device is left in a clean state.
            if self.hogged {
                release_hog(self.device);
                self.hogged = false;
            }
            return Err(e);
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.stop_stream();
        if self.hogged {
            release_hog(self.device);
            self.hogged = false;
        }
        self.paused.store(false, Ordering::Release);
    }

    fn native_dsd_capabilities(&self) -> Vec<DsdWireFormat> {
        Vec::new()
    }

    fn set_native_dsd(
        &mut self,
        _params: Option<NativeDsdParams>,
    ) -> Result<Option<DsdWireFormat>, OutputError> {
        Err(OutputError::StreamError(
            "native DSD transport is not supported by the CoreAudio backend \
             (no CoreAudio DSD stream format exists); use DoP or DSD→PCM"
                .to_string(),
        ))
    }
}

impl OutputVolume for CoreAudioOutput {
    fn supports_hardware_volume(&self) -> bool {
        supports_device_virtual_volume(self.device)
    }

    fn set_hardware_volume_db(&self, db: f32) -> Result<(), OutputError> {
        set_device_virtual_volume(self.device, db).map_err(OutputError::StreamError)
    }
}

/// Device-scoped virtual main/master volume support check. Mirrors the
/// default-device helpers in `cpal_output::volume::coreaudio` but scoped to
/// the selected device.
fn supports_device_virtual_volume(device: AudioObjectID) -> bool {
    let mut volume: f32 = 1.0;
    for selector in [VIRTUAL_MAIN_VOLUME, VIRTUAL_MASTER_VOLUME] {
        let mut a = addr(
            selector,
            kAudioObjectPropertyScopeOutput,
            kAudioObjectPropertyElementMain,
        );
        let mut size: u32 = std::mem::size_of::<f32>() as u32;
        // SAFETY: valid buffers, no qualifier; non-zero status only means the
        // device lacks this property.
        let status = unsafe {
            objc2_core_audio::AudioObjectGetPropertyData(
                device,
                NonNull::from(&mut a),
                0,
                std::ptr::null::<c_void>(),
                NonNull::from(&mut size),
                NonNull::from(&mut volume).cast::<c_void>(),
            )
        };
        if status == NO_ERROR {
            return true;
        }
    }
    false
}

/// Device-scoped virtual main/master volume set (linear scalar from dB).
fn set_device_virtual_volume(device: AudioObjectID, db: f32) -> Result<(), String> {
    let linear = if db <= -96.0 {
        0.0
    } else {
        10.0_f32.powf(db / 20.0).clamp(0.0, 1.0)
    };
    let mut last_status = -1i32;
    for selector in [VIRTUAL_MAIN_VOLUME, VIRTUAL_MASTER_VOLUME] {
        let mut a = addr(
            selector,
            kAudioObjectPropertyScopeOutput,
            kAudioObjectPropertyElementMain,
        );
        // SAFETY: valid buffers, no qualifier.
        let status = unsafe {
            objc2_core_audio::AudioObjectSetPropertyData(
                device,
                NonNull::from(&mut a),
                0,
                std::ptr::null::<c_void>(),
                std::mem::size_of::<f32>() as u32,
                NonNull::from(&linear).cast::<c_void>(),
            )
        };
        if status == NO_ERROR {
            return Ok(());
        }
        last_status = status;
    }
    Err(format!(
        "CoreAudio refused virtual volume set (OSStatus {last_status})"
    ))
}

/// `kAudioHardwareServiceDeviceProperty_VirtualMainVolume` = 'vmvc'.
const VIRTUAL_MAIN_VOLUME: u32 = 0x766d_7663;
/// `kAudioHardwareServiceDeviceProperty_VirtualMasterVolume` = 'vmvl'.
const VIRTUAL_MASTER_VOLUME: u32 = 0x766d_766c;
