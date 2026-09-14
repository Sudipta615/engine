//! Plugin source resolution — config string → loaded
//! [`PluginHost`].
//!
//! A slot's `source` is either:
//! - a **library path** (`/usr/local/lib/shadow/libecho.so`, `…dll`,
//!   `…dylib`) → loaded through the dynamic loader, or
//! - a **static-registry reference** `static:<32 hex chars>` (the UID in
//!   hex) → looked up in the process-wide registry (statically linked
//!   plugins, in-process tests, FFI hosts).
//!
//! Resolved hosts are memoized per source so a generation rebuild does
//! not re-`dlopen` an unchanged library; the cache is bounded and lives
//! on the control path only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "plugin-dylib")]
use std::path::Path;

use plugin_abi::{PluginHost, PluginUid};

/// The maximum number of cached host libraries (LRU-less bounded cache;
/// beyond this the newest entry replaces the oldest by key order).
const MAX_CACHED_HOSTS: usize = 16;

fn cache() -> &'static Mutex<HashMap<String, Arc<PluginHost>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<PluginHost>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a config `source` into a loaded host, or `None` when the
/// source is invalid / the library cannot be loaded (the caller skips
/// the slot — a broken plugin never interrupts playback).
pub fn resolve_host(source: &str) -> Option<Arc<PluginHost>> {
    let source = source.trim();
    if source.is_empty() {
        return None;
    }
    {
        let cache = cache().lock().ok()?;
        if let Some(host) = cache.get(source) {
            return Some(host.clone());
        }
    }

    let host = if let Some(hex) = source.strip_prefix("static:") {
        let uid = uid_from_hex(hex)?;
        plugin_abi::host::lookup_static(&uid)?
    } else {
        #[cfg(feature = "plugin-dylib")]
        {
            let path = Path::new(source);
            if !path.is_file() {
                log::warn!("plugin source {source} is not a file");
                return None;
            }
            match PluginHost::load(path) {
                Ok(host) => host,
                Err(e) => {
                    log::warn!("plugin {source} failed to load: {e}");
                    return None;
                }
            }
        }
        #[cfg(not(feature = "plugin-dylib"))]
        {
            log::warn!(
                "plugin source {source} requires the plugin-dylib feature \
                 (dynamic loading is disabled in this build)"
            );
            return None;
        }
    };

    let mut cache = cache().lock().ok()?;
    if cache.len() >= MAX_CACHED_HOSTS {
        cache.clear();
    }
    cache.insert(source.to_string(), host.clone());
    Some(host)
}

/// Parse a 32-hex-char UID string into a [`PluginUid`].
fn uid_from_hex(hex: &str) -> Option<PluginUid> {
    if hex.len() != 32 {
        return None;
    }
    let mut uid = [0u8; 16];
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        uid[i] = (hi << 4) | lo;
    }
    Some(uid)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Register an in-process host for a hex-UID source string (tests,
/// FFI hosts, statically linked builds). Returns the `static:<uid>`
/// source string the config should reference.
pub fn register_static_host(host: Arc<PluginHost>) -> Option<String> {
    let uid = host.uid();
    if !plugin_abi::host::register_static(host) {
        return None;
    }
    Some(format!("static:{}", uid_to_hex(&uid)))
}

fn uid_to_hex(uid: &PluginUid) -> String {
    let mut out = String::with_capacity(32);
    for byte in uid {
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("hex digit"));
        out.push(char::from_digit((byte & 0xf) as u32, 16).expect("hex digit"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_uid_roundtrip() {
        let uid: PluginUid = [
            0x01, 0x23, 0xab, 0xcd, 0xef, 0x00, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10,
        ];
        let hex = uid_to_hex(&uid);
        assert_eq!(hex.len(), 32);
        assert_eq!(uid_from_hex(&hex), Some(uid));
    }

    #[test]
    fn rejects_short_hex() {
        assert!(uid_from_hex("0102").is_none());
    }
}
