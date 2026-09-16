//! # Shadow Desktop Plugin ABI
//!
//! The versioned Rust-native plugin specification for Shadow Desktop
//! Effect plugins compiled as pure Rust crates that expose a
//! **stable C-ABI vtable** and are hosted in the engine's production DSP
//! graph at the master insert seam.
//!
//! ## Design contract
//!
//! - **`#[repr(C)]` everywhere.** The vtable and all crossing types are
//!   C-compatible; a plugin compiled with a different `rustc` (or even a
//!   C implementation) links against the same symbols.
//! - **Plain data on the wire.** Every struct that crosses the ABI
//!   boundary is fixed-layout plain data — no `Vec`, `String`, `Box`, or
//!   trait objects. Variable-length data travels through caller-owned
//!   buffers with explicit lengths.
//! - **Realtime-safe process.** `process()` runs on the audio thread
//!   inside a plan step: it must not allocate, lock, or block. The host
//!   enforces this with the zero-allocation fidelity suite
//!   (`tests/fidelity/plugin_host.rs`) and the worst-case reference
//!   plugin.
//! - **Versioned.** [`PLUGIN_ABI_VERSION`] gates the handshake; the host
//!   refuses mismatched plugins with a structured error rather than UB.
//!
//! ## Life of a plugin instance
//!
//! ```text
//! descriptor() → instantiate() → prepare() → set_params()*
//!             → process()* (audio thread) → save_state()/load_state()
//!             → reset() → drop_instance()
//! ```
//!
//! The host (see [`host`]) owns exactly one instance per graph node; the
//! instance pointer is opaque host/plugin-shared state and is never
//! dereferenced by the host.

#![allow(clippy::missing_safety_doc)]

pub mod automation;
pub mod bus;
pub mod host;
pub mod midi;
pub mod params;
pub mod sandbox;
pub mod state;
pub mod transport;

pub use automation::{ParamAutomationBatch, ParamAutomationEvent, MAX_AUTOMATION_EVENTS_PER_BLOCK};
pub use bus::{AudioBussesMut, BusDescriptor, PluginBusLayout, PluginBusRole, MAX_PLUGIN_BUSSES};
pub use host::{PluginHost, PluginInstance};
pub use midi::{MidiEvent, MidiEventType, MidiPacket, MAX_MIDI_EVENTS_PER_BLOCK};
pub use params::{ParamDescriptor, ParamValue, PluginParams};
pub use sandbox::{PluginFaultKind, PluginSandboxConfig, PluginSandboxMode, PluginSandboxState};
pub use state::{PluginStateError, StateBuffer};
pub use transport::TransportInfo;

use std::fmt;

/// The plugin ABI version this crate speaks. Bump **only** with a host
/// release that breaks plugin compatibility (a major engine version).
pub const PLUGIN_ABI_VERSION: u32 = 1;

/// Maximum channel count a plugin instance may be prepared for (mirrors
/// the engine's `MAX_CHANNELS`).
pub const MAX_PLUGIN_CHANNELS: usize = 18;

/// Maximum parameter count a plugin may declare.
pub const MAX_PLUGIN_PARAMS: usize = 64;

/// Maximum bytes of serialized plugin state the ABI will carry (256 KiB).
pub const MAX_STATE_BYTES: usize = 256 * 1024;

/// A plugin-unique stable identifier (choose a UUID's first 16 bytes, or
/// any stable 128-bit value). The host matches saved state and node
/// configs by this + the semantic version.
pub type PluginUid = [u8; 16];

/// Fixed-capacity semantic version string (NUL-padded), e.g. `"1.2.0"`.
pub type VersionString = [u8; 32];

/// Error codes crossing the ABI. `0` is success; everything else is a
/// refusal the host can surface as a structured config issue.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbiStatus {
    Ok = 0,
    /// The host passed an ABI version the plugin does not speak.
    VersionMismatch = 1,
    /// `instantiate` failed (bad config, out of resources).
    InstantiationFailed = 2,
    /// A parameter index/value was rejected.
    BadParam = 3,
    /// The prepared channel count / sample rate is unsupported.
    UnsupportedFormat = 4,
    /// State serialization/deserialization failed.
    BadState = 5,
    /// A host contract violation (null pointer, oversized buffer).
    InvalidArgument = 6,
    /// The plugin detected it cannot honor realtime constraints.
    NotRealtimeSafe = 7,
}

impl AbiStatus {
    /// `true` for the success code only.
    pub fn is_ok(self) -> bool {
        self == AbiStatus::Ok
    }

    /// Decode a raw ABI status integer. Unknown codes map to
    /// [`AbiStatus::InvalidArgument`] (defensive: plugins must stick to
    /// the declared codes).
    pub fn from_abi_i32(code: i32) -> Self {
        match code {
            0 => AbiStatus::Ok,
            1 => AbiStatus::VersionMismatch,
            2 => AbiStatus::InstantiationFailed,
            3 => AbiStatus::BadParam,
            4 => AbiStatus::UnsupportedFormat,
            5 => AbiStatus::BadState,
            6 => AbiStatus::InvalidArgument,
            7 => AbiStatus::NotRealtimeSafe,
            _ => AbiStatus::InvalidArgument,
        }
    }

    /// Stable message for logs / config issues.
    pub fn message(self) -> &'static str {
        match self {
            AbiStatus::Ok => "ok",
            AbiStatus::VersionMismatch => "plugin ABI version mismatch",
            AbiStatus::InstantiationFailed => "plugin instantiation failed",
            AbiStatus::BadParam => "invalid plugin parameter",
            AbiStatus::UnsupportedFormat => "unsupported channel/sample-rate format",
            AbiStatus::BadState => "invalid plugin state",
            AbiStatus::InvalidArgument => "invalid host argument",
            AbiStatus::NotRealtimeSafe => "plugin is not realtime-safe",
        }
    }
}

/// A planar non-interleaved audio block: `channels` planes of `frames`
/// samples each. Borrowed for the duration of the `process` call — the
/// plugin must not retain pointers.
#[repr(C)]
pub struct AudioBlockMut {
    /// One pointer per channel plane (`channels` entries).
    pub planes: *mut *mut f32,
    pub channels: u32,
    pub frames: u32,
}

/// The plugin descriptor: plain-data identity + capability metadata,
/// returned by `descriptor()` without instantiating. All strings are
/// NUL-padded fixed arrays; the host NUL-terminates defensively when
/// copying out.
#[repr(C)]
pub struct PluginDescriptor {
    /// ABI version the plugin was built against (checked against
    /// [`PLUGIN_ABI_VERSION`] by the host).
    pub abi_version: u32,
    /// Stable plugin identity (see [`PluginUid`]).
    pub uid: PluginUid,
    /// Human-readable name, NUL-padded.
    pub name: VersionString,
    /// Semantic version, NUL-padded (e.g. "1.0.0").
    pub version: VersionString,
    /// Vendor / author, NUL-padded.
    pub vendor: VersionString,
    /// Number of declared parameters (≤ [`MAX_PLUGIN_PARAMS`]).
    pub param_count: u32,
    /// Deterministic processing latency in samples at the prepared rate
    /// (the graph's alignment pass consumes this).
    pub latency_samples: u32,
    /// Ring-down tail in samples after input stops.
    pub tail_samples: u32,
    /// `1` if the plugin processes multichannel blocks (up to
    /// [`MAX_PLUGIN_CHANNELS`]); `0` = stereo-pair only.
    pub supports_multichannel: u32,
}

impl PluginDescriptor {
    /// Decode a NUL-padded ABI string into a `String` for display.
    pub fn abi_string(bytes: &VersionString) -> String {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }
}

/// The process vtable: audio-thread entry points. All are `extern "C"`.
///
/// # Safety contract (host side)
///
/// - `instance` is the pointer returned by `instantiate` for THIS vtable
///   and is never concurrently invoked from two threads.
/// - `process` is called only between `prepare` and `drop_instance`,
///   never concurrently with any other vtable call on the same instance.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginVTable {
    /// `fn(descriptor: *mut PluginDescriptor, abi_version: u32) -> i32`
    ///
    /// Ask the plugin to fill the descriptor and check the version.
    pub descriptor: Option<unsafe extern "C" fn(*mut PluginDescriptor, u32) -> i32>,
    /// `fn(abi_version: u32, sample_rate: f32) -> *mut c_void`
    ///
    /// Create one instance. Returns null on failure.
    pub instantiate: Option<unsafe extern "C" fn(u32, f32) -> *mut std::ffi::c_void>,
    /// `fn(instance: *mut c_void, channels: u32, frames_capacity: u32) -> i32`
    ///
    /// Prepare for a channel count + max block size (control path).
    pub prepare: Option<unsafe extern "C" fn(*mut std::ffi::c_void, u32, u32) -> i32>,
    /// `fn(instance: *mut c_void, index: u32, value: f32) -> i32`
    pub set_param: Option<unsafe extern "C" fn(*mut std::ffi::c_void, u32, f32) -> i32>,
    /// `fn(instance: *mut c_void, block: *const AudioBlockMut) -> i32`
    ///
    /// **Audio thread.** Must not allocate / lock / block.
    pub process: Option<unsafe extern "C" fn(*mut std::ffi::c_void, *const AudioBlockMut) -> i32>,
    /// `fn(instance: *mut c_void, buffer: *mut u8, capacity: usize) -> i32`
    ///
    /// Serialize state into `buffer`; `-1`-style refusal is
    /// `BadState`. Returns required size if capacity is 0.
    pub save_state: Option<unsafe extern "C" fn(*mut std::ffi::c_void, *mut u8, usize) -> isize>,
    /// `fn(instance: *mut c_void, bytes: *const u8, len: usize) -> i32`
    pub load_state: Option<unsafe extern "C" fn(*mut std::ffi::c_void, *const u8, usize) -> i32>,
    /// `fn(instance: *mut c_void)` — clear filter/tail state (seek).
    pub reset: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    /// `fn(instance: *mut c_void)` — destroy the instance.
    pub drop_instance: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
}

impl fmt::Debug for PluginVTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginVTable")
            .field("descriptor", &self.descriptor.is_some())
            .field("instantiate", &self.instantiate.is_some())
            .field("prepare", &self.prepare.is_some())
            .field("set_param", &self.set_param.is_some())
            .field("process", &self.process.is_some())
            .field("save_state", &self.save_state.is_some())
            .field("load_state", &self.load_state.is_some())
            .field("reset", &self.reset.is_some())
            .field("drop_instance", &self.drop_instance.is_some())
            .finish()
    }
}

/// The full versioned handshake a plugin library exports: one function
/// `plugin_abi_v1() -> *const PluginAbiV1` returning this structure.
///
/// The host `dlopen`s the library, resolves `plugin_abi_v1`, checks
/// `abi_version`, then copies the vtable out.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginAbiV1 {
    /// Must equal [`PLUGIN_ABI_VERSION`] (= 1).
    pub abi_version: u32,
    /// The size of this struct as the plugin compiled it (guards
    /// against trailing-field mismatches).
    pub size: u32,
    pub vtable: PluginVTable,
}

impl fmt::Debug for PluginAbiV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginAbiV1")
            .field("abi_version", &self.abi_version)
            .field("size", &self.size)
            .field("vtable", &self.vtable)
            .finish()
    }
}

/// The symbol name a v1 plugin library exports.
pub const PLUGIN_ABI_SYMBOL: &str = "plugin_abi_v1";

/// The error type surfaced by the host and the safe facade.
#[derive(Debug)]
pub enum PluginAbiError {
    /// The library's `plugin_abi_v1` returned a mismatched version.
    VersionMismatch { plugin: u32, host: u32 },
    /// The exported symbol is missing or has the wrong type.
    SymbolMissing(String),
    /// An ABI call refused with this status.
    Status(AbiStatus),
    /// A plugin returned a structurally invalid descriptor.
    InvalidDescriptor(String),
    /// State serialization round-trip failed.
    State(String),
    /// Loading the dynamic library failed (path + message).
    Load(String, String),
}

impl fmt::Display for PluginAbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginAbiError::VersionMismatch { plugin, host } => write!(
                f,
                "plugin ABI version mismatch: plugin speaks v{plugin}, host speaks v{host}"
            ),
            PluginAbiError::SymbolMissing(s) => write!(f, "plugin symbol missing: {s}"),
            PluginAbiError::Status(s) => write!(f, "plugin refused: {}", s.message()),
            PluginAbiError::InvalidDescriptor(s) => write!(f, "invalid plugin descriptor: {s}"),
            PluginAbiError::State(s) => write!(f, "plugin state error: {s}"),
            PluginAbiError::Load(path, msg) => {
                write!(f, "cannot load plugin library {path}: {msg}")
            }
        }
    }
}

impl std::error::Error for PluginAbiError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_string_decodes_nul_padded() {
        let mut s = [0u8; 32];
        s[..5].copy_from_slice(b"echo\x00".as_slice());
        s[4] = 0;
        assert_eq!(PluginDescriptor::abi_string(&s), "echo");
    }

    #[test]
    fn status_roundtrip() {
        assert!(AbiStatus::Ok.is_ok());
        assert_eq!(AbiStatus::BadParam.message(), "invalid plugin parameter");
    }
}
