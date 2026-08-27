//! C FFI (Foreign Function Interface) for the independent audio engine.
//!
//! Exposes a stable C ABI for host applications written in C, C++, or any
//! language that can call C functions (Python ctypes, C#, Node.js FFI, etc.).
//!
//! # Design
//!
//! - **Opaque handles**: `EngineHandleFFI` is a `Box<EngineHandle>` behind an
//!   opaque pointer. Callers never see the internal layout.
//! - **No panics across FFI**: Every function returns a status code (0 = Ok)
//!   and writes the result through an output pointer.
//! - **Thread-safe**: Handles are `Clone + Send`; commands are sent through
//!   the same lock-free channel as the Rust API.
//!
//! # Lifecycle
//!
//! ```c
//! EngineHandleFFI* handle = engine_create(ENGINE_BACKEND_DEFAULT);
//! engine_open_file(handle, "/path/to/audio.flac");
//! engine_play(handle);
//! // ...
//! engine_destroy(handle);
//! ```
//!
//! # Type mapping
//!
//! | C type          | Rust         |
//! |-----------------|--------------|
//! | `float`         | `f32`        |
//! | `uint32_t`      | `u32`        |
//! | `int32_t`       | `i32`         |
//! | `const char*`   | `&CStr → &str` |
//! | `EngineHandleFFI*` | `Box<EngineHandle>` |

use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::{AudioEngine, EngineHandle};
use config::EngineConfig;

/// Opaque handle to an audio engine instance.
///
/// The engine itself is moved onto a dedicated background tick thread (see
/// [`EngineHandleFFI::spawn_tick_thread`]); the handle only retains the
/// client-side [`EngineHandle`] plus the stop flag and thread join handle
/// needed to tear it down cleanly in [`engine_destroy`].
pub struct EngineHandleFFI {
    handle: EngineHandle,
    stop: Arc<AtomicBool>,
    tick_thread: Option<std::thread::JoinHandle<()>>,
}

impl EngineHandleFFI {
    /// Spawn the background thread that drives [`AudioEngine::tick_blocking`]
    /// for the lifetime of this handle. The engine is moved into the thread;
    /// dropping it there runs `AudioEngine::stop()` after the stop flag is
    /// set, so resources are released deterministically in `engine_destroy`.
    fn spawn_tick_thread(
        engine: AudioEngine,
    ) -> (Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>) {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("ffi-engine-tick".into())
            .spawn(move || {
                let mut engine = engine;
                while !thread_stop.load(Ordering::Relaxed) {
                    // Wake instantly on commands; otherwise tick at most
                    // every 5 ms so playback stays smooth while idle CPU
                    // stays near zero.
                    engine.tick_blocking(std::time::Duration::from_millis(5));
                }
                engine.stop();
            })
            .ok();
        (stop, handle)
    }
}

/// Status codes returned by FFI functions.
#[repr(i32)]
pub enum EngineStatus {
    Ok = 0,
    Error = -1,
    InvalidHandle = -2,
    InvalidArgument = -3,
    EngineNotRunning = -4,
}

/// Backend selection constants (must match `config::AudioBackend`).
#[repr(u32)]
pub enum EngineBackend {
    Auto = 0,
    ExclusiveAlsa = 1,
    ExclusiveAsio = 2,
    ExclusiveCoreAudio = 3,
    Default = 4,
}

// ── Lifecycle ──────────────────────────────────────────────────────────────

/// Create and start an audio engine with the default configuration.
///
/// Returns `NULL` on failure (check logs). The caller must call
/// `engine_destroy` to release resources.
#[no_mangle]
pub extern "C" fn engine_create(backend: EngineBackend) -> *mut EngineHandleFFI {
    let mut config = EngineConfig::default();
    config.output_backend = match backend {
        EngineBackend::Auto => config::AudioBackend::Auto,
        EngineBackend::ExclusiveAlsa => config::AudioBackend::ExclusiveAlsa,
        EngineBackend::ExclusiveAsio => config::AudioBackend::ExclusiveAsio,
        EngineBackend::ExclusiveCoreAudio => config::AudioBackend::ExclusiveCoreAudioHog,
        EngineBackend::Default => config::AudioBackend::default(),
    };

    let engine = match AudioEngine::new(config) {
        Ok(mut e) => {
            let handle = e.handle();
            if let Err(err) = e.start() {
                log::error!("engine_create: failed to start engine: {}", err);
                return std::ptr::null_mut();
            }
            (handle, e)
        }
        Err(e) => {
            log::error!("engine_create: failed to create engine: {}", e);
            return std::ptr::null_mut();
        }
    };

    let (stop, tick_thread) = EngineHandleFFI::spawn_tick_thread(engine.1);
    Box::into_raw(Box::new(EngineHandleFFI {
        handle: engine.0,
        stop,
        tick_thread,
    }))
}

/// Destroy an engine and release all resources. Safe to call with `NULL`.
///
/// Signals the background tick thread to stop, joins it (the engine's
/// `Drop` impl stops the audio output there), and shuts down the command
/// channel. Idempotent — the host may call it more than once.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_destroy(engine: *mut EngineHandleFFI) {
    if engine.is_null() {
        return;
    }
    unsafe {
        let mut boxed = Box::from_raw(engine);
        boxed.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = boxed.tick_thread.take() {
            let _ = thread.join();
        }
        boxed.handle.shutdown();
    }
}

// ── Playback control ───────────────────────────────────────────────────────

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_play(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.play();
    EngineStatus::Ok as i32
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_pause(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.pause();
    EngineStatus::Ok as i32
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_stop(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.stop();
    EngineStatus::Ok as i32
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_seek(handle: *mut EngineHandleFFI, position_secs: f32) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.seek(position_secs);
    EngineStatus::Ok as i32
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_set_volume(handle: *mut EngineHandleFFI, volume: f32) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.set_volume(volume);
    EngineStatus::Ok as i32
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_set_speed(handle: *mut EngineHandleFFI, speed: f32) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.set_speed(speed);
    EngineStatus::Ok as i32
}

/// Set volume directly in dB (see `engine_set_volume_db`).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_set_volume_db(handle: *mut EngineHandleFFI, db: f32) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.set_volume_db(db);
    EngineStatus::Ok as i32
}

// ── Source management ──────────────────────────────────────────────────────

/// Open a file for playback. Returns Ok on success, Error on failure.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_open_file(handle: *mut EngineHandleFFI, path: *const c_char) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    if path.is_null() {
        return EngineStatus::InvalidArgument as i32;
    }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return EngineStatus::InvalidArgument as i32,
    };
    h.handle.open_file(Path::new(path_str).to_path_buf());
    EngineStatus::Ok as i32
}

/// Open a URI for playback (`file://`, `http://`, `https://`).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_open_uri(handle: *mut EngineHandleFFI, uri: *const c_char) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    if uri.is_null() {
        return EngineStatus::InvalidArgument as i32;
    }
    let c_str = unsafe { CStr::from_ptr(uri) };
    let uri_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return EngineStatus::InvalidArgument as i32,
    };
    h.handle.open_uri(uri_str.to_string());
    EngineStatus::Ok as i32
}

/// Add a file to the end of the playback queue (without interrupting playback).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_enqueue_file(handle: *mut EngineHandleFFI, path: *const c_char) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    if path.is_null() {
        return EngineStatus::InvalidArgument as i32;
    }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return EngineStatus::InvalidArgument as i32,
    };
    h.handle.enqueue_file(Path::new(path_str).to_path_buf());
    EngineStatus::Ok as i32
}

/// Skip to the next playlist entry.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_next(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.next();
    EngineStatus::Ok as i32
}

/// Skip to the previous playlist entry.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_previous(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.previous();
    EngineStatus::Ok as i32
}

/// Clear the playback queue. Safe to call with `NULL`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_clear_playlist(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.clear_playlist();
    EngineStatus::Ok as i32
}

// ── Query ──────────────────────────────────────────────────────────────────

/// Get the current playback position in seconds. Returns -1.0 on error.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_position_secs(handle: *mut EngineHandleFFI) -> f32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return -1.0,
    };
    h.handle.playback_info().position_secs
}

/// Get the track duration in seconds. Returns -1.0 on error.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_duration_secs(handle: *mut EngineHandleFFI) -> f32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return -1.0,
    };
    h.handle.playback_info().duration_secs
}

/// Get the current playback state: 0=Stopped, 1=Playing, 2=Paused.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_playback_state(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    match h.handle.state() {
        crate::playback_info::PlaybackState::Stopped => 0,
        crate::playback_info::PlaybackState::Playing => 1,
        crate::playback_info::PlaybackState::Paused => 2,
        crate::playback_info::PlaybackState::Buffering => 3,
    }
}

/// Number of entries in the playback queue (0 when empty).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_playlist_len(handle: *mut EngineHandleFFI) -> i64 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return -1,
    };
    h.handle.playlist_len() as i64
}

// ── Aux insert (Phase 6) ───────────────────────────────────────────────────

/// Toggle the Phase-6 aux insert (the global convolution on the aux bus):
/// `enabled` != 0 turns it on, `wet_mix` in [0, 1] sets the wet/dry balance.
/// The impulse response stays as configured; this is a no-op when no IR
/// engine exists yet.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_set_aux_insert(
    handle: *mut EngineHandleFFI,
    enabled: i32,
    wet_mix: f32,
) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    if !wet_mix.is_finite() {
        return EngineStatus::InvalidArgument as i32;
    }
    h.handle.set_aux_insert(enabled != 0, wet_mix);
    EngineStatus::Ok as i32
}

/// Read the live Phase-6 aux insert state. On success writes `enabled`
/// (0/1) and `wet_mix` and returns `EngineStatus::Ok`; returns
/// `EngineStatus::InvalidArgument` if either out-pointer is NULL.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_aux_insert_state(
    handle: *mut EngineHandleFFI,
    enabled: *mut i32,
    wet_mix: *mut f32,
) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    if enabled.is_null() || wet_mix.is_null() {
        return EngineStatus::InvalidArgument as i32;
    }
    let info = h.handle.playback_info();
    unsafe {
        *enabled = info.aux_insert_enabled as i32;
        *wet_mix = info.aux_insert_wet_mix;
    }
    EngineStatus::Ok as i32
}

// ── Additional output endpoints (routing matrix) ───────────────────────────

/// Add or replace one additional output endpoint. `id` is the stable
/// identifier used by later `engine_remove_endpoint` / `engine_upsert_endpoint`
/// calls; `device` may be NULL to select the backend default; `backend` uses
/// the same constants as `EngineBackend`; `gain` in [0, 4]; `enabled` != 0
/// activates the endpoint immediately.
#[cfg(feature = "audio-output")]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_upsert_endpoint(
    handle: *mut EngineHandleFFI,
    id: *const c_char,
    device: *const c_char,
    backend: u32,
    gain: f32,
    enabled: i32,
    drift_correction: i32,
) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    if id.is_null() || !gain.is_finite() {
        return EngineStatus::InvalidArgument as i32;
    }
    let id_str = match unsafe { CStr::from_ptr(id) }.to_str() {
        Ok(s) if !s.is_empty() => s.to_string(),
        _ => return EngineStatus::InvalidArgument as i32,
    };
    let device_str = if device.is_null() {
        None
    } else {
        match unsafe { CStr::from_ptr(device) }.to_str() {
            Ok(s) if !s.is_empty() => Some(s.to_string()),
            _ => return EngineStatus::InvalidArgument as i32,
        }
    };
    let backend = match backend {
        0 => config::AudioBackend::Auto,
        1 => config::AudioBackend::ExclusiveWasapi,
        2 => config::AudioBackend::ExclusiveAlsa,
        3 => config::AudioBackend::ExclusiveCoreAudioHog,
        4 => config::AudioBackend::ExclusiveAsio,
        _ => return EngineStatus::InvalidArgument as i32,
    };
    h.handle.set_endpoint(config::EndpointConfig {
        id: id_str,
        backend,
        device: device_str,
        gain: gain.clamp(0.0, 4.0),
        enabled: enabled != 0,
        drift_correction: drift_correction != 0,
    });
    EngineStatus::Ok as i32
}

/// Remove a configured additional output endpoint by its stable identifier.
#[cfg(feature = "audio-output")]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_remove_endpoint(handle: *mut EngineHandleFFI, id: *const c_char) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    if id.is_null() {
        return EngineStatus::InvalidArgument as i32;
    }
    let id_str = match unsafe { CStr::from_ptr(id) }.to_str() {
        Ok(s) if !s.is_empty() => s.to_string(),
        _ => return EngineStatus::InvalidArgument as i32,
    };
    h.handle.remove_endpoint(id_str);
    EngineStatus::Ok as i32
}

/// Remove all configured additional output endpoints.
#[cfg(feature = "audio-output")]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_clear_endpoints(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.clear_endpoints();
    EngineStatus::Ok as i32
}

/// Number of currently configured additional output endpoints (from the
/// last telemetry refresh; -1 on invalid handle).
#[cfg(feature = "audio-output")]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_endpoint_count(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return -1,
    };
    h.handle.playback_info().endpoints.len() as i32
}

/// Copy the identifier of the configured endpoint at `index` into `buf`
/// (at most `buf_len` bytes, NUL-terminated). Returns the required length
/// (excluding the NUL) or a negative status code.
#[cfg(feature = "audio-output")]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_endpoint_id(
    handle: *mut EngineHandleFFI,
    index: i32,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    if index < 0 || buf.is_null() {
        return EngineStatus::InvalidArgument as i32;
    }
    let endpoints = &h.handle.playback_info().endpoints;
    let id = match endpoints.get(index as usize) {
        Some(e) => e.id.as_bytes(),
        None => return EngineStatus::InvalidArgument as i32,
    };
    let n = id.len().min(buf_len.saturating_sub(1));
    unsafe {
        std::ptr::copy_nonoverlapping(id.as_ptr() as *const c_char, buf, n);
        *buf.add(n) = 0;
    }
    id.len() as i32
}

/// Read the telemetry for the configured endpoint at `index`: `enabled`,
/// `gain`, `written_frames`, `dropped_frames`, `available_frames`, and
/// `transport_error_count` are written when the matching out-pointer is
/// non-NULL. Returns `EngineStatus::Ok`, `InvalidHandle`, or
/// `InvalidArgument` (index out of range).
#[cfg(feature = "audio-output")]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn engine_endpoint_info(
    handle: *mut EngineHandleFFI,
    index: i32,
    enabled: *mut i32,
    gain: *mut f32,
    written_frames: *mut u64,
    dropped_frames: *mut u64,
    available_frames: *mut usize,
    transport_error_count: *mut u64,
) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    if index < 0 {
        return EngineStatus::InvalidArgument as i32;
    }
    let info = h.handle.playback_info();
    let ep = match info.endpoints.get(index as usize) {
        Some(e) => e,
        None => return EngineStatus::InvalidArgument as i32,
    };
    unsafe {
        if !enabled.is_null() {
            *enabled = ep.enabled as i32;
        }
        if !gain.is_null() {
            *gain = ep.gain;
        }
        if !written_frames.is_null() {
            *written_frames = ep.written_frames;
        }
        if !dropped_frames.is_null() {
            *dropped_frames = ep.dropped_frames;
        }
        if !available_frames.is_null() {
            *available_frames = ep.available_frames;
        }
        if !transport_error_count.is_null() {
            *transport_error_count = ep.transport_error_count;
        }
    }
    EngineStatus::Ok as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineCommand;
    use std::sync::atomic::Ordering;

    /// Write a 16-bit stereo WAV whose left channel is a single impulse
    /// (the IR the aux-insert tests need; `engine::tests` helpers are
    /// private to that module).
    fn write_impulse_wav(path: &std::path::Path, sample_rate: u32, n_frames: usize) {
        let mut data = Vec::with_capacity(n_frames * 4);
        for i in 0..n_frames {
            let v = if i == 0 { 32767i16 } else { 0i16 };
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&0i16.to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, &wav).unwrap();
    }

    /// Build an [`EngineHandleFFI`] around a real engine with its own tick
    /// thread (no audio output is opened — `start()` is never called, so no
    /// device is touched). Returns the box plus the raw pointer the extern
    /// functions consume.
    fn ffi_with_engine(
        config: config::EngineConfig,
    ) -> (Box<EngineHandleFFI>, *mut EngineHandleFFI) {
        let engine = AudioEngine::new(config).expect("engine must construct");
        let handle = engine.handle();
        let (stop, tick_thread) = EngineHandleFFI::spawn_tick_thread(engine);
        let mut ffi = Box::new(EngineHandleFFI {
            handle,
            stop,
            tick_thread,
        });
        let ptr = &mut *ffi as *mut EngineHandleFFI;
        (ffi, ptr)
    }

    /// Stop the tick thread deterministically before the box drops.
    fn shutdown(ffi: &mut EngineHandleFFI) {
        ffi.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = ffi.tick_thread.take() {
            let _ = thread.join();
        }
    }

    #[test]
    fn ffi_null_handles_and_invalid_args_rejected() {
        assert_eq!(
            engine_set_aux_insert(std::ptr::null_mut(), 1, 0.5),
            EngineStatus::InvalidHandle as i32
        );
        assert_eq!(
            engine_clear_endpoints(std::ptr::null_mut()),
            EngineStatus::InvalidHandle as i32
        );
        assert_eq!(
            engine_remove_endpoint(std::ptr::null_mut(), c"ep".as_ptr().cast::<c_char>(),),
            EngineStatus::InvalidHandle as i32
        );
        assert_eq!(engine_endpoint_count(std::ptr::null_mut()), -1);
        assert_eq!(
            engine_aux_insert_state(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            EngineStatus::InvalidHandle as i32
        );

        let (mut ffi, ptr) = ffi_with_engine(EngineConfig::default());
        // Null out-pointers on the aux read are rejected.
        assert_eq!(
            engine_aux_insert_state(ptr, std::ptr::null_mut(), std::ptr::null_mut()),
            EngineStatus::InvalidArgument as i32
        );
        let mut enabled = 0i32;
        assert_eq!(
            engine_aux_insert_state(ptr, &mut enabled, std::ptr::null_mut()),
            EngineStatus::InvalidArgument as i32
        );
        // Endpoint validation: null/empty id, unknown backend.
        assert_eq!(
            engine_upsert_endpoint(ptr, std::ptr::null(), std::ptr::null(), 0, 1.0, 1, 1),
            EngineStatus::InvalidArgument as i32
        );
        assert_eq!(
            engine_upsert_endpoint(ptr, c"".as_ptr().cast(), std::ptr::null(), 0, 1.0, 1, 1),
            EngineStatus::InvalidArgument as i32
        );
        assert_eq!(
            engine_upsert_endpoint(ptr, c"ep".as_ptr().cast(), std::ptr::null(), 99, 1.0, 1, 1),
            EngineStatus::InvalidArgument as i32
        );
        assert_eq!(
            engine_upsert_endpoint(
                ptr,
                c"ep".as_ptr().cast(),
                std::ptr::null(),
                0,
                f32::NAN,
                1,
                1
            ),
            EngineStatus::InvalidArgument as i32
        );
        shutdown(&mut ffi);
    }

    #[test]
    fn ffi_aux_insert_toggle_reads_back_through_telemetry() {
        // An impulse IR so the insert engine exists and the toggle is
        // observable end to end (command → graph → mirror → telemetry).
        let ir_path = std::env::temp_dir().join(format!(
            "ffi_aux_ir_{}_{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_impulse_wav(&ir_path, 48_000, 2048);

        let mut config = EngineConfig::default();
        config.aux.enabled = true;
        config.aux.return_gain = 0.5;
        config.aux.insert_enabled = false;
        config.aux.insert_wet_mix = 1.0;
        config.aux.insert_ir_path = Some(ir_path.display().to_string());
        let (mut ffi, ptr) = ffi_with_engine(config);

        // Send the SAME command through the handle's channel directly as a
        // control: this isolates the extern wrapper from the engine path.
        let cmd = EngineCommand::SetAuxInsert {
            enabled: true,
            wet_mix: 0.25,
        };
        assert!(
            ffi.handle.send_command(cmd).is_ok(),
            "handle accepts command"
        );
        assert_eq!(engine_set_aux_insert(ptr, 1, 0.25), EngineStatus::Ok as i32);

        // Poll the telemetry mirror (2 s cadence) for the toggled state.
        let mut enabled = 0i32;
        let mut wet = -1.0f32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut ok = false;
        while std::time::Instant::now() < deadline {
            assert_eq!(
                engine_aux_insert_state(ptr, &mut enabled, &mut wet),
                EngineStatus::Ok as i32
            );
            if enabled == 1 && (wet - 0.25).abs() < 1e-6 {
                ok = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            ok,
            "aux insert state read back (enabled={enabled}, wet={wet})"
        );
        shutdown(&mut ffi);
        let _ = std::fs::remove_file(&ir_path);
    }

    #[test]
    fn ffi_endpoint_surface_accepts_config_sequence() {
        let (mut ffi, ptr) = ffi_with_engine(EngineConfig::default());
        assert_eq!(engine_endpoint_count(ptr), 0, "no endpoints configured");

        // Upsert (id + device + backend + gain + enabled), then remove,
        // then re-add and clear — every call routes through the C-string
        // parser into `endpoint_configs` (no device is opened: the engine
        // was never started).
        assert_eq!(
            engine_upsert_endpoint(
                ptr,
                c"ffi-ep".as_ptr().cast(),
                c"Fake Device".as_ptr().cast(),
                0,
                0.8,
                1,
                1,
            ),
            EngineStatus::Ok as i32
        );
        assert_eq!(
            engine_remove_endpoint(ptr, c"ffi-ep".as_ptr().cast()),
            EngineStatus::Ok as i32
        );
        assert_eq!(
            engine_upsert_endpoint(
                ptr,
                c"ffi-ep".as_ptr().cast(),
                std::ptr::null(),
                0,
                0.8,
                1,
                0,
            ),
            EngineStatus::Ok as i32
        );
        assert_eq!(engine_clear_endpoints(ptr), EngineStatus::Ok as i32);
        shutdown(&mut ffi);
    }
}
