//! The plugin host: loads v1 plugins, wraps the C vtable in a safe
//! facade, and provides an **in-process registry** so statically-linked
//! Rust plugins (and the reference plugin / tests) can register without
//! dynamic loading.
//!
//! Two loading paths, one facade:
//!
//! - [`PluginHost::load`] — `dlopen`/`LoadLibrary` a plugin library,
//!   resolve `plugin_abi_v1`, check the version, copy the vtable.
//! - [`PluginHost::from_vtable`] / [`PluginHost::register_static`] —
//!   adopt an already-built vtable (statically linked, tests, FFI).
//!
//! The facade enforces the host contract: one instance per
//! [`PluginInstance`], no calls after `drop`, `process` only between
//! `prepare` and drop, and all results surfaced as [`PluginAbiError`].

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "host")]
use libloading::Library;

use crate::{
    AbiStatus, AudioBlockMut, PluginAbiError, PluginAbiV1, PluginDescriptor, PluginUid,
    MAX_PLUGIN_CHANNELS, MAX_STATE_BYTES, PLUGIN_ABI_SYMBOL, PLUGIN_ABI_VERSION,
};

/// A loaded plugin library: the vtable plus (for dynamic plugins) the
/// owned `Library` that must outlive every instance.
pub struct PluginHost {
    vtable: crate::PluginVTable,
    #[cfg(feature = "host")]
    #[allow(dead_code)]
    library: Option<Arc<Library>>,
    descriptor: PluginDescriptor,
}

// SAFETY: the vtable functions are `extern "C"` with no thread affinity;
// the host contract serializes calls per instance (one owner), and the
// library handle is `Send` once loaded (loading is complete).
unsafe impl Send for PluginHost {}
unsafe impl Sync for PluginHost {}

impl PluginHost {
    /// Resolve and adopt a vtable without a library. Verifies the
    /// version + descriptor. Used by static registration + tests.
    ///
    /// # Safety
    /// `abi` must point to a valid `PluginAbiV1` that remains valid for
    /// the lifetime of every instance created from the returned host.
    pub unsafe fn from_abi(abi: *const PluginAbiV1) -> Result<Self, PluginAbiError> {
        if abi.is_null() {
            return Err(PluginAbiError::SymbolMissing(PLUGIN_ABI_SYMBOL.into()));
        }
        // SAFETY: caller guarantees validity for the host's lifetime.
        let abi = unsafe { &*abi };
        if abi.abi_version != PLUGIN_ABI_VERSION {
            return Err(PluginAbiError::VersionMismatch {
                plugin: abi.abi_version,
                host: PLUGIN_ABI_VERSION,
            });
        }
        if abi.size as usize != std::mem::size_of::<PluginAbiV1>() {
            return Err(PluginAbiError::VersionMismatch {
                plugin: abi.abi_version,
                host: PLUGIN_ABI_VERSION,
            });
        }
        let vtable = abi.vtable;
        let host = Self {
            vtable,
            #[cfg(feature = "host")]
            library: None,
            descriptor: PluginDescriptor {
                abi_version: PLUGIN_ABI_VERSION,
                uid: [0; 16],
                name: [0; 32],
                version: [0; 32],
                vendor: [0; 32],
                param_count: 0,
                latency_samples: 0,
                tail_samples: 0,
                supports_multichannel: 0,
            },
        };
        host.with_verified_descriptor()
    }

    /// Build a host directly from a vtable (the static path).
    ///
    /// # Safety
    /// The vtable's function pointers must remain valid for the lifetime
    /// of every instance created from the returned host.
    pub unsafe fn from_vtable(vtable: crate::PluginVTable) -> Result<Self, PluginAbiError> {
        let fake = PluginAbiV1 {
            abi_version: PLUGIN_ABI_VERSION,
            size: std::mem::size_of::<PluginAbiV1>() as u32,
            vtable,
        };
        // SAFETY: the local struct outlives the call; the vtable itself
        // is caller-guaranteed.
        unsafe { Self::from_abi(&fake) }
    }

    /// `dlopen` a plugin library and adopt its vtable (the dynamic
    /// path). Requires the `host` feature.
    #[cfg(feature = "host")]
    pub fn load(path: &std::path::Path) -> Result<Arc<Self>, PluginAbiError> {
        let library = unsafe {
            Library::new(path)
                .map_err(|e| PluginAbiError::Load(path.display().to_string(), e.to_string()))?
        };
        let library = Arc::new(library);
        // SAFETY: the symbol is resolved from the loaded library and its
        // lifetime is tied to `library` (stored below). The vtable is
        // copied out by value.
        let abi = unsafe {
            library
                .get::<unsafe extern "C" fn() -> *const PluginAbiV1>(PLUGIN_ABI_SYMBOL.as_bytes())
                .map_err(|_| {
                    PluginAbiError::SymbolMissing(format!(
                        "{PLUGIN_ABI_SYMBOL} in {}",
                        path.display()
                    ))
                })?
        };
        let abi_ptr = unsafe { abi() };
        // SAFETY: the library is stored in the host and outlives it.
        let mut host = unsafe { Self::from_abi(abi_ptr) }?;
        host.library = Some(library);
        Ok(Arc::new(host))
    }

    fn with_verified_descriptor(mut self) -> Result<Self, PluginAbiError> {
        let fill = self
            .vtable
            .descriptor
            .ok_or_else(|| PluginAbiError::SymbolMissing("descriptor".into()))?;
        // SAFETY: descriptor points at our owned struct; the plugin
        // fills it. Host contract: single-threaded host-owned call.
        let status = unsafe {
            fill(
                &mut self.descriptor as *mut PluginDescriptor,
                PLUGIN_ABI_VERSION,
            )
        };
        let status = AbiStatus::from_abi_i32(status);
        if !status.is_ok() {
            return Err(PluginAbiError::Status(status));
        }
        if self.descriptor.abi_version != PLUGIN_ABI_VERSION {
            return Err(PluginAbiError::VersionMismatch {
                plugin: self.descriptor.abi_version,
                host: PLUGIN_ABI_VERSION,
            });
        }
        if self.descriptor.param_count as usize > crate::MAX_PLUGIN_PARAMS {
            return Err(PluginAbiError::InvalidDescriptor(format!(
                "param_count {} exceeds the maximum {}",
                self.descriptor.param_count,
                crate::MAX_PLUGIN_PARAMS
            )));
        }
        Ok(self)
    }

    /// The verified descriptor.
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// The stable plugin id.
    pub fn uid(&self) -> PluginUid {
        self.descriptor.uid
    }

    /// The plugin's human-readable name.
    pub fn name(&self) -> String {
        PluginDescriptor::abi_string(&self.descriptor.name)
    }

    /// The plugin's semantic version string.
    pub fn version(&self) -> String {
        PluginDescriptor::abi_string(&self.descriptor.version)
    }

    /// Whether the plugin can process more than a stereo pair.
    pub fn supports_multichannel(&self) -> bool {
        self.descriptor.supports_multichannel != 0
    }

    /// Instantiate the plugin at `sample_rate`. One instance per node —
    /// the returned [`PluginInstance`] owns the raw pointer and drops it
    /// via `drop_instance`.
    pub fn instantiate(&self, sample_rate: f32) -> Result<PluginInstance, PluginAbiError> {
        let ctor = self
            .vtable
            .instantiate
            .as_ref()
            .ok_or_else(|| PluginAbiError::SymbolMissing("instantiate".into()))?;
        // SAFETY: the plugin owns the returned pointer; the version was
        // verified at load.
        let raw = unsafe { ctor(PLUGIN_ABI_VERSION, sample_rate) };
        if raw.is_null() {
            return Err(PluginAbiError::Status(AbiStatus::InstantiationFailed));
        }
        Ok(PluginInstance {
            host: self as *const PluginHost,
            raw,
            vtable: self.vtable,
        })
    }
}

// SAFETY: `host` back-pointers are `*const` and never dereferenced
// concurrently; instances are moved between threads only before first
// use (graph construction happens on the control thread).
unsafe impl Send for PluginInstance {}
unsafe impl Sync for PluginInstance {}

/// One live plugin instance (owned by a graph node). Dropping it calls
/// `drop_instance`.
pub struct PluginInstance {
    #[allow(dead_code)]
    host: *const PluginHost,
    raw: *mut c_void,
    vtable: crate::PluginVTable,
}

impl PluginInstance {
    /// Prepare for `channels` planes and blocks up to
    /// `max_frames` (control path).
    pub fn prepare(&mut self, channels: usize, max_frames: usize) -> Result<(), PluginAbiError> {
        if channels == 0 || channels > MAX_PLUGIN_CHANNELS || max_frames == 0 {
            return Err(PluginAbiError::Status(AbiStatus::InvalidArgument));
        }
        let prep = self
            .vtable
            .prepare
            .ok_or_else(|| PluginAbiError::SymbolMissing("prepare".into()))?;
        // SAFETY: `raw` is the pointer this instance owns; single-thread
        // host contract.
        let status = unsafe { prep(self.raw, channels as u32, max_frames as u32) };
        let status = AbiStatus::from_abi_i32(status);
        if status.is_ok() {
            Ok(())
        } else {
            Err(PluginAbiError::Status(status))
        }
    }

    /// Set one parameter by index (control path).
    pub fn set_param(&mut self, index: u32, value: f32) -> Result<(), PluginAbiError> {
        let set = self
            .vtable
            .set_param
            .ok_or_else(|| PluginAbiError::SymbolMissing("set_param".into()))?;
        // SAFETY: as above.
        let status = unsafe { set(self.raw, index, value) };
        let status = AbiStatus::from_abi_i32(status);
        if status.is_ok() {
            Ok(())
        } else {
            Err(PluginAbiError::Status(status))
        }
    }

    /// **Audio thread.** Process one planar block in place. The planes
    /// are borrowed for the call only; the plugin must not retain them.
    ///
    /// # Safety (host contract, honored by the caller)
    /// `planes.len()` must equal the prepared channel count and every
    /// plane slice must be at least `frames` long. This facade assumes
    /// the caller (the graph plan runner) guarantees that.
    pub unsafe fn process(&mut self, planes: &mut [&mut [f32]]) -> Result<(), PluginAbiError> {
        let proc = self
            .vtable
            .process
            .ok_or_else(|| PluginAbiError::SymbolMissing("process".into()))?;
        let channels = planes.len();
        let frames = planes.first().map(|p| p.len()).unwrap_or(0);
        if channels == 0 || frames == 0 {
            return Ok(());
        }
        let mut ptrs: [*mut f32; MAX_PLUGIN_CHANNELS] = [std::ptr::null_mut(); MAX_PLUGIN_CHANNELS];
        for (i, p) in planes.iter_mut().enumerate().take(MAX_PLUGIN_CHANNELS) {
            ptrs[i] = p.as_mut_ptr();
        }
        let block = AudioBlockMut {
            planes: ptrs.as_mut_ptr(),
            channels: channels as u32,
            frames: frames as u32,
        };
        // SAFETY: the caller guarantees plane lengths; the plugin owns
        // no retained pointers. Fault isolation wraps the call in catch_unwind
        // so a plugin panic does not bring down the audio host.
        let raw_ptr = self.raw;
        let call_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            proc(raw_ptr, &block as *const AudioBlockMut)
        }));

        match call_res {
            Ok(code) => {
                let status = AbiStatus::from_abi_i32(code);
                if status.is_ok() {
                    Ok(())
                } else {
                    Err(PluginAbiError::Status(status))
                }
            }
            Err(_) => Err(PluginAbiError::Status(AbiStatus::InstantiationFailed)),
        }
    }

    /// Serialize the plugin's state. Control path; may allocate.
    pub fn save_state(&mut self) -> Result<Vec<u8>, PluginAbiError> {
        let save = self
            .vtable
            .save_state
            .ok_or_else(|| PluginAbiError::SymbolMissing("save_state".into()))?;
        // First call: query the required size.
        // SAFETY: capacity 0 = size query per the ABI contract.
        let need = unsafe { save(self.raw, std::ptr::null_mut(), 0) };
        if need < 0 {
            return Err(PluginAbiError::Status(AbiStatus::BadState));
        }
        let need = need as usize;
        if need == 0 {
            return Ok(Vec::new());
        }
        if need > MAX_STATE_BYTES {
            return Err(PluginAbiError::State(format!(
                "plugin asked for {need} state bytes, over the {}-byte cap",
                MAX_STATE_BYTES
            )));
        }
        let mut buf = vec![0u8; need];
        // SAFETY: `buf` is `need` bytes long, owned by us.
        let wrote = unsafe { save(self.raw, buf.as_mut_ptr(), need) };
        if wrote < 0 || wrote as usize > need {
            return Err(PluginAbiError::Status(AbiStatus::BadState));
        }
        buf.truncate(wrote.max(0) as usize);
        Ok(buf)
    }

    /// Restore a previously saved state blob. Control path.
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), PluginAbiError> {
        let load = self
            .vtable
            .load_state
            .ok_or_else(|| PluginAbiError::SymbolMissing("load_state".into()))?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(PluginAbiError::State(format!(
                "state payload is {} bytes, over the {}-byte cap",
                bytes.len(),
                MAX_STATE_BYTES
            )));
        }
        // SAFETY: the slice lives for the call; the plugin copies out.
        let status = if bytes.is_empty() {
            unsafe { load(self.raw, std::ptr::null(), 0) }
        } else {
            unsafe { load(self.raw, bytes.as_ptr(), bytes.len()) }
        };
        let status = AbiStatus::from_abi_i32(status);
        if status.is_ok() {
            Ok(())
        } else {
            Err(PluginAbiError::Status(status))
        }
    }

    /// Clear filter/tail state (seek / track boundary). Audio-path-safe
    /// by plugin contract (no allocation).
    pub fn reset(&mut self) {
        if let Some(reset) = self.vtable.reset {
            // SAFETY: `raw` is owned by this instance.
            unsafe { reset(self.raw) };
        }
    }

    /// The raw instance pointer (for hosts that need to pass it back
    /// through their own FFI).
    pub fn as_ptr(&self) -> *mut c_void {
        self.raw
    }
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        if let Some(drop_fn) = self.vtable.drop_instance {
            // SAFETY: the pointer was created by `instantiate` and is
            // dropped exactly once here.
            unsafe { drop_fn(self.raw) };
        }
    }
}

// ── Static registry ──────────────────────────────────────────────────────────

/// The process-wide registry of statically-linked plugins. Hosts with
/// plugins compiled in (or test setups) register vtables by UID; config
/// references a plugin by `static:<hex-uid>` or by library path.
static STATIC_REGISTRY: OnceLock<Mutex<BTreeMap<PluginUid, Arc<PluginHost>>>> = OnceLock::new();

fn registry() -> &'static Mutex<BTreeMap<PluginUid, Arc<PluginHost>>> {
    STATIC_REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Register a plugin host under its UID (static / test path). Returns
/// `false` (and refuses) when the UID is already registered with a
/// different plugin.
pub fn register_static(host: Arc<PluginHost>) -> bool {
    let uid = host.uid();
    let mut reg = registry().lock().expect("plugin registry poisoned");
    if let Some(existing) = reg.get(&uid) {
        return Arc::ptr_eq(existing, &host);
    }
    reg.insert(uid, host);
    true
}

/// Look up a statically-registered plugin by UID.
pub fn lookup_static(uid: &PluginUid) -> Option<Arc<PluginHost>> {
    registry()
        .lock()
        .expect("plugin registry poisoned")
        .get(uid)
        .cloned()
}

/// Remove a static registration (tests only).
pub fn unregister_static(uid: &PluginUid) -> bool {
    registry()
        .lock()
        .expect("plugin registry poisoned")
        .remove(uid)
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roundtrip() {
        // A null-vtable host cannot be built; use a stub vtable with
        // only the descriptor entry point (enough for registration).
        unsafe extern "C" fn desc(d: *mut PluginDescriptor, v: u32) -> i32 {
            if v != 1 {
                return AbiStatus::VersionMismatch as i32;
            }
            // SAFETY: test descriptor pointer.
            unsafe {
                *d = PluginDescriptor {
                    abi_version: 1,
                    uid: [0xab; 16],
                    name: zero_str(b"stub"),
                    version: zero_str(b"0.1.0"),
                    vendor: zero_str(b"test"),
                    param_count: 0,
                    latency_samples: 0,
                    tail_samples: 0,
                    supports_multichannel: 0,
                };
            }
            AbiStatus::Ok as i32
        }
        fn zero_str(s: &[u8]) -> [u8; 32] {
            let mut out = [0u8; 32];
            out[..s.len().min(32)].copy_from_slice(s);
            out
        }
        let vtable = crate::PluginVTable {
            descriptor: Some(desc),
            instantiate: None,
            prepare: None,
            set_param: None,
            process: None,
            save_state: None,
            load_state: None,
            reset: None,
            drop_instance: None,
        };
        let host = unsafe { PluginHost::from_vtable(vtable) }.expect("stub host builds");
        let uid = host.uid();
        assert!(register_static(Arc::new(host)));
        assert!(lookup_static(&uid).is_some());
        assert!(unregister_static(&uid));
        assert!(lookup_static(&uid).is_none());
    }
}
