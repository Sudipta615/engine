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
#[no_mangle]
pub extern "C" fn engine_destroy(engine: *mut EngineHandleFFI) {
    if engine.is_null() {
        return;
    }
    unsafe {
        let boxed = Box::from_raw(engine);
        boxed.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = boxed.tick_thread.take() {
            let _ = thread.join();
        }
        boxed.handle.shutdown();
    }
}

// ── Playback control ───────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn engine_play(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.play();
    EngineStatus::Ok as i32
}

#[no_mangle]
pub extern "C" fn engine_pause(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.pause();
    EngineStatus::Ok as i32
}

#[no_mangle]
pub extern "C" fn engine_stop(handle: *mut EngineHandleFFI) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.stop();
    EngineStatus::Ok as i32
}

#[no_mangle]
pub extern "C" fn engine_seek(handle: *mut EngineHandleFFI, position_secs: f32) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.seek(position_secs);
    EngineStatus::Ok as i32
}

#[no_mangle]
pub extern "C" fn engine_set_volume(handle: *mut EngineHandleFFI, volume: f32) -> i32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return EngineStatus::InvalidHandle as i32,
    };
    h.handle.set_volume(volume);
    EngineStatus::Ok as i32
}

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
#[no_mangle]
pub extern "C" fn engine_position_secs(handle: *mut EngineHandleFFI) -> f32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return -1.0,
    };
    h.handle.playback_info().position_secs
}

/// Get the track duration in seconds. Returns -1.0 on error.
#[no_mangle]
pub extern "C" fn engine_duration_secs(handle: *mut EngineHandleFFI) -> f32 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return -1.0,
    };
    h.handle.playback_info().duration_secs
}

/// Get the current playback state: 0=Stopped, 1=Playing, 2=Paused.
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
#[no_mangle]
pub extern "C" fn engine_playlist_len(handle: *mut EngineHandleFFI) -> i64 {
    let h = match unsafe { handle.as_ref() } {
        Some(h) => h,
        None => return -1,
    };
    h.handle.playlist_len() as i64
}
