//! On-disk cache for computed [`AudioProfile`]s.
//!
//! Mirrors the loudness-scan cache's contract and storage layout: a single
//! JSON document in the app's local data directory, validated against the
//! source file's **size and modification time**, so an unchanged track is
//! never re-analyzed and a modified file is automatically refreshed. Entries
//! additionally carry the profile schema version (mismatch = miss) and an
//! optional **content fingerprint** (e.g. the AcoustID hex from the
//! `fingerprint` feature), so identical content at different paths — copies,
//! duplicates, re-tags — shares one cached profile.
//!
//! The cache is best-effort: unreadable or corrupt files fall back to an
//! empty cache, and write failures are ignored.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::profile::{AudioProfile, AUDIO_PROFILE_VERSION};

/// One cached profile for a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    /// File size in bytes at analysis time.
    size: u64,
    /// File modification time in whole seconds since the Unix epoch.
    mtime_secs: u64,
    /// Profile schema version at store time (`AUDIO_PROFILE_VERSION`).
    profile_version: u32,
    /// Optional content fingerprint (dedup key across paths).
    #[serde(default)]
    content_id: Option<String>,
    /// The stored profile.
    profile: AudioProfile,
}

/// The whole cache document, keyed by canonical file path.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileCacheFile {
    entries: HashMap<String, CacheEntry>,
}

/// In-memory copy of the cache, keyed by the cache file it was loaded from.
/// Guarded by a mutex because background analysis threads write while the
/// engine thread reads. Tests exercise the same code through
/// `lookup_in`/`store_in` with their own cache paths, so they are isolated.
static CACHE: Mutex<Option<(PathBuf, ProfileCacheFile)>> = Mutex::new(None);

/// Resolve the default cache file location (app data dir, like the loudness
/// cache).
fn default_cache_file_path() -> Option<PathBuf> {
    let mut dir = crate::paths::data_local_dir()?;
    dir.push("audio-engine");
    let _ = std::fs::create_dir_all(&dir);
    dir.push("profile_cache.json");
    Some(dir)
}

fn load_cache_from(cache_path: &Path) -> ProfileCacheFile {
    match std::fs::read_to_string(cache_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => ProfileCacheFile::default(),
    }
}

fn save_json_to(cache_path: &Path, json: &str) {
    let tmp = cache_path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_err() {
        return;
    }
    // Windows: rename cannot overwrite an existing destination.
    let _ = std::fs::remove_file(cache_path);
    let _ = std::fs::rename(&tmp, cache_path);
}

fn ensure_loaded<'a>(
    slot: &'a mut Option<(PathBuf, ProfileCacheFile)>,
    cache_path: &Path,
) -> Option<&'a mut ProfileCacheFile> {
    if slot.as_ref().is_none_or(|(p, _)| p.as_path() != cache_path) {
        *slot = Some((cache_path.to_path_buf(), load_cache_from(cache_path)));
    }
    Some(&mut slot.as_mut()?.1)
}

/// Cache key for a file: its canonical path (best effort).
fn key_for(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical.to_string_lossy().into_owned()
}

fn file_size_and_mtime(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len();
    let mtime_secs = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((size, mtime_secs))
}

fn entry_valid(entry: &CacheEntry, size: u64, mtime_secs: u64) -> bool {
    entry.size == size
        && entry.mtime_secs == mtime_secs
        && entry.profile_version == AUDIO_PROFILE_VERSION
}

/// Look up a cached profile for `path` in `cache_path`, validating the file
/// on disk still matches the size/mtime the entry was created from.
fn lookup_in(cache_path: &Path, path: &Path) -> Option<AudioProfile> {
    let (size, mtime_secs) = file_size_and_mtime(path)?;
    let key = key_for(path);
    let mut guard = CACHE.lock().ok()?;
    let cache = ensure_loaded(&mut guard, cache_path)?;
    let entry = cache.entries.get(&key)?;
    if !entry_valid(entry, size, mtime_secs) {
        return None;
    }
    Some(entry.profile.clone())
}

/// Look up by content fingerprint (dedup across paths).
fn lookup_for_id_in(cache_path: &Path, content_id: &str) -> Option<AudioProfile> {
    let mut guard = CACHE.lock().ok()?;
    let cache = ensure_loaded(&mut guard, cache_path)?;
    for entry in cache.entries.values() {
        if entry.content_id.as_deref() == Some(content_id)
            && entry.profile_version == AUDIO_PROFILE_VERSION
        {
            return Some(entry.profile.clone());
        }
    }
    None
}

/// Persist a profile for `path` in `cache_path`, keyed by the file's current
/// size and mtime. No-op if the file cannot be stat'ed.
fn store_in(cache_path: &Path, path: &Path, profile: &AudioProfile, content_id: Option<&str>) {
    let (size, mtime_secs) = match file_size_and_mtime(path) {
        Some(v) => v,
        None => return,
    };
    let key = key_for(path);
    let Ok(mut guard) = CACHE.lock() else {
        return;
    };
    let cache = match ensure_loaded(&mut guard, cache_path) {
        Some(c) => c,
        None => return,
    };
    cache.entries.insert(
        key,
        CacheEntry {
            size,
            mtime_secs,
            profile_version: profile.version,
            content_id: content_id.map(str::to_owned),
            profile: profile.clone(),
        },
    );
    let json = serde_json::to_string(&*cache).ok();
    drop(guard);
    if let Some(json) = json {
        save_json_to(cache_path, &json);
    }
}

/// Look up a cached profile for `path` in the default cache.
pub fn lookup(path: &Path) -> Option<AudioProfile> {
    let cache_path = default_cache_file_path()?;
    lookup_in(&cache_path, path)
}

/// Persist a profile for `path` in the default cache.
pub fn store(path: &Path, profile: &AudioProfile) {
    if let Some(cache_path) = default_cache_file_path() {
        store_in(&cache_path, path, profile, None);
    }
}

/// Look up a cached profile by content fingerprint in the default cache.
pub fn lookup_for_id(content_id: &str) -> Option<AudioProfile> {
    let cache_path = default_cache_file_path()?;
    lookup_for_id_in(&cache_path, content_id)
}

/// Persist a profile for `path` with an optional content fingerprint.
pub fn store_with_id(path: &Path, profile: &AudioProfile, content_id: Option<&str>) {
    if let Some(cache_path) = default_cache_file_path() {
        store_in(&cache_path, path, profile, content_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{AnalysisMask, AudioProfile};

    fn test_cache_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shadow_profile_cache_{name}_{}",
            std::process::id()
        ))
    }

    fn sample_profile() -> AudioProfile {
        AudioProfile {
            version: AUDIO_PROFILE_VERSION,
            sample_rate: 44_100,
            channels: 2,
            duration_secs: 8.0,
            mask: AnalysisMask::all(),
            ..Default::default()
        }
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        let _ = std::fs::remove_file(path);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn store_and_lookup_round_trip() {
        let cache = test_cache_path("roundtrip");
        let file = std::env::temp_dir().join(format!("profile_cache_f_{}", std::process::id()));
        write_file(&file, b"audio-bytes");
        let p = sample_profile();
        store_in(&cache, &file, &p, None);
        let got = lookup_in(&cache, &file).expect("hit");
        assert_eq!(got, p);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&cache);
    }

    #[test]
    fn changed_file_is_a_miss() {
        let cache = test_cache_path("changed");
        let file = std::env::temp_dir().join(format!("profile_cache_g_{}", std::process::id()));
        write_file(&file, b"old-bytes");
        let p = sample_profile();
        store_in(&cache, &file, &p, None);
        assert!(lookup_in(&cache, &file).is_some());
        // Rewrite with different content → size/mtime differ → miss.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_file(&file, b"new-longer-bytes-here");
        assert!(lookup_in(&cache, &file).is_none());
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&cache);
    }

    #[test]
    fn version_mismatch_is_a_miss() {
        let cache = test_cache_path("version");
        let file = std::env::temp_dir().join(format!("profile_cache_h_{}", std::process::id()));
        write_file(&file, b"v");
        let mut p = sample_profile();
        p.version = AUDIO_PROFILE_VERSION + 99;
        store_in(&cache, &file, &p, None);
        assert!(lookup_in(&cache, &file).is_none());
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&cache);
    }

    #[test]
    fn content_id_dedups_across_paths() {
        let cache = test_cache_path("contentid");
        let f1 = std::env::temp_dir().join(format!("profile_cache_i_{}", std::process::id()));
        let f2 = std::env::temp_dir().join(format!("profile_cache_j_{}", std::process::id()));
        write_file(&f1, b"same-content");
        write_file(&f2, b"same-content");
        let p = sample_profile();
        store_in(&cache, &f1, &p, Some("fp123"));
        // Different path, different size/mtime — but same content id.
        let got = lookup_for_id_in(&cache, "fp123").expect("content-id hit");
        assert_eq!(got, p);
        let _ = std::fs::remove_file(&f1);
        let _ = std::fs::remove_file(&f2);
        let _ = std::fs::remove_file(&cache);
    }

    #[test]
    fn missing_or_corrupt_cache_is_empty() {
        let cache = test_cache_path("corrupt");
        let file = std::env::temp_dir().join(format!("profile_cache_k_{}", std::process::id()));
        write_file(&file, b"x");
        std::fs::write(&cache, b"{ not json").ok();
        assert!(lookup_in(&cache, &file).is_none());
        // Store after a corrupt cache still works (replaces the document).
        let p = sample_profile();
        store_in(&cache, &file, &p, None);
        assert!(lookup_in(&cache, &file).is_some());
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&cache);
    }
}
