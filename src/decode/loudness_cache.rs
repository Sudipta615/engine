//! On-disk cache for background loudness scan results.
//!
//! Scan results are persisted as a single JSON document in the app's local
//! data directory (same location as the cover-art cache) and validated
//! against the source file's size and modification time, so an unchanged
//! track is never re-scanned and a modified file is automatically refreshed.
//!
//! The cache is best-effort: unreadable or corrupt files fall back to an
//! empty cache, and write failures are ignored.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::decode::LoudnessScanResult;

/// One cached scan result for a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    /// File size in bytes at scan time.
    size: u64,
    /// File modification time in whole seconds since the Unix epoch.
    mtime_secs: u64,
    /// EBU R128 integrated loudness in LUFS.
    ebu_r128_loudness: Option<f32>,
    /// True peak in dBTP (shared 4× FIR estimate).
    ebu_r128_peak_dbtp: Option<f32>,
    /// ReplayGain track gain in dB.
    #[serde(default)]
    replaygain_track_db: Option<f32>,
    /// ReplayGain track peak linear.
    #[serde(default)]
    replaygain_track_peak: Option<f32>,
    /// Loudness range in LU.
    #[serde(default)]
    lra_lu: Option<f32>,
    /// Frames of audio decoded during the scan.
    frames_scanned: u64,
}

/// The whole cache document, keyed by canonical file path.
#[derive(Debug, Default, Serialize, Deserialize)]
struct LoudnessCache {
    entries: HashMap<String, CacheEntry>,
}

/// In-memory copy of the cache, keyed by the cache file it was loaded from.
/// Guarded by a mutex because scan threads write while the engine thread
/// reads. Tests exercise the same code through `lookup_in`/`store_in` with
/// their own cache paths, so the two are fully isolated.
static CACHE: Mutex<Option<(PathBuf, LoudnessCache)>> = Mutex::new(None);

/// Resolve the default cache file location (app data dir, like covers).
fn default_cache_file_path() -> Option<PathBuf> {
    let mut dir = crate::paths::data_local_dir()?;
    dir.push("playtune");
    let _ = std::fs::create_dir_all(&dir);
    dir.push("loudness_cache.json");
    Some(dir)
}

fn load_cache_from(cache_path: &Path) -> LoudnessCache {
    match std::fs::read_to_string(cache_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => LoudnessCache::default(),
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

/// Reload the in-memory cache if it belongs to a different cache file.
fn ensure_loaded<'a>(
    slot: &'a mut Option<(PathBuf, LoudnessCache)>,
    cache_path: &Path,
) -> Option<&'a mut LoudnessCache> {
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

/// Look up a cached scan result for `path` in `cache_path`, validating that
/// the file on disk still matches the size and mtime the entry was created
/// from. Returns `None` when the file is missing/changed or no entry exists.
fn lookup_in(cache_path: &Path, path: &Path) -> Option<LoudnessScanResult> {
    let (size, mtime_secs) = file_size_and_mtime(path)?;
    let key = key_for(path);
    let mut guard = CACHE.lock().ok()?;
    let cache = ensure_loaded(&mut guard, cache_path)?;
    let entry = cache.entries.get(&key)?;
    if entry.size != size || entry.mtime_secs != mtime_secs {
        return None;
    }
    let rg_gain = entry
        .replaygain_track_db
        .or_else(|| entry.ebu_r128_loudness.map(|lufs| -18.0 - lufs));
    let rg_peak = entry.replaygain_track_peak.or_else(|| {
        entry
            .ebu_r128_peak_dbtp
            .map(|dbtp| 10.0_f32.powf(dbtp / 20.0))
    });
    Some(LoudnessScanResult {
        ebu_r128_loudness: entry.ebu_r128_loudness,
        ebu_r128_peak_dbtp: entry.ebu_r128_peak_dbtp,
        replaygain_track_db: rg_gain,
        replaygain_track_peak: rg_peak,
        lra_lu: entry.lra_lu,
        frames_scanned: entry.frames_scanned,
    })
}

/// Persist a scan result for `path` in `cache_path`, keyed by the file's
/// current size and mtime. No-op if the file cannot be stat'ed (e.g.
/// deleted mid-scan).
fn store_in(cache_path: &Path, path: &Path, result: &LoudnessScanResult) {
    let (size, mtime_secs) = match file_size_and_mtime(path) {
        Some(v) => v,
        None => return,
    };
    let key = key_for(path);
    let Ok(mut guard) = CACHE.lock() else { return };
    let cache = match ensure_loaded(&mut guard, cache_path) {
        Some(c) => c,
        None => return,
    };
    cache.entries.insert(
        key,
        CacheEntry {
            size,
            mtime_secs,
            ebu_r128_loudness: result.ebu_r128_loudness,
            ebu_r128_peak_dbtp: result.ebu_r128_peak_dbtp,
            replaygain_track_db: result.replaygain_track_db,
            replaygain_track_peak: result.replaygain_track_peak,
            lra_lu: result.lra_lu,
            frames_scanned: result.frames_scanned,
        },
    );
    let json = serde_json::to_string(cache).ok();
    drop(guard);
    if let Some(json) = json {
        save_json_to(cache_path, &json);
    }
}

/// Look up a cached scan result for `path` in the default cache.
pub fn lookup(path: &Path) -> Option<LoudnessScanResult> {
    lookup_in(&default_cache_file_path()?, path)
}

/// Persist a scan result for `path` in the default cache.
pub fn store(path: &Path, result: &LoudnessScanResult) {
    let Some(cache_path) = default_cache_file_path() else {
        return;
    };
    store_in(&cache_path, path, result);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests in this module: `test_persists_to_disk` and the
    /// corrupt-cache test reset the shared in-memory `CACHE`, which must not
    /// race with other tests' store/lookup calls. Recovered from poisoning
    /// so a failed assertion in one test doesn't cascade into the others.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cache_lock() -> std::sync::MutexGuard<'static, Option<(PathBuf, LoudnessCache)>> {
        CACHE.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn temp_cache_file() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "engine_loudness_cache_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("cache.json")
    }

    fn make_audio_file() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "engine_loudness_cache_file_{}_{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, [0u8; 128]).unwrap();
        path
    }

    fn sample_result() -> LoudnessScanResult {
        LoudnessScanResult {
            ebu_r128_loudness: Some(-23.5),
            ebu_r128_peak_dbtp: Some(-0.4),
            replaygain_track_db: Some(5.5),
            replaygain_track_peak: Some(0.95),
            lra_lu: Some(4.2),
            frames_scanned: 96000,
        }
    }

    fn cleanup(cache: &Path, path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(cache.parent().unwrap());
    }

    #[test]
    fn test_roundtrip() {
        let _g = test_lock();
        let cache = temp_cache_file();
        let path = make_audio_file();
        assert!(lookup_in(&cache, &path).is_none(), "no entry before store");
        store_in(&cache, &path, &sample_result());
        let got = lookup_in(&cache, &path).expect("entry after store");
        assert_eq!(got.ebu_r128_loudness, Some(-23.5));
        assert_eq!(got.ebu_r128_peak_dbtp, Some(-0.4));
        cleanup(&cache, &path);
    }

    #[test]
    fn test_persists_to_disk() {
        let _g = test_lock();
        let cache = temp_cache_file();
        let path = make_audio_file();
        store_in(&cache, &path, &sample_result());
        // Drop the in-memory cache: lookup must reload from disk.
        *cache_lock() = None;
        let got = lookup_in(&cache, &path).expect("reloaded from disk");
        assert_eq!(got.ebu_r128_loudness, Some(-23.5));
        cleanup(&cache, &path);
    }

    #[test]
    fn test_invalidated_by_mtime_change() {
        let _g = test_lock();
        let cache = temp_cache_file();
        let path = make_audio_file();
        store_in(&cache, &path, &sample_result());
        assert!(lookup_in(&cache, &path).is_some(), "valid entry");
        // Same size, newer mtime → entry must be rejected. The mtime key has
        // 1-second granularity, so advance by 2 seconds (setting it to "now"
        // can land in the same second as the store and fail to invalidate).
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        f.set_times(std::fs::FileTimes::new().set_modified(future))
            .unwrap();
        assert!(
            lookup_in(&cache, &path).is_none(),
            "mtime change invalidates the entry"
        );
        cleanup(&cache, &path);
    }

    #[test]
    fn test_invalidated_by_size_change() {
        let _g = test_lock();
        let cache = temp_cache_file();
        let path = make_audio_file();
        store_in(&cache, &path, &sample_result());
        std::fs::write(&path, [0u8; 256]).unwrap(); // different size
        assert!(
            lookup_in(&cache, &path).is_none(),
            "size change invalidates the entry"
        );
        cleanup(&cache, &path);
    }

    #[test]
    fn test_missing_file_returns_none() {
        let _g = test_lock();
        let cache = temp_cache_file();
        let path = make_audio_file();
        store_in(&cache, &path, &sample_result());
        std::fs::remove_file(&path).unwrap();
        assert!(
            lookup_in(&cache, &path).is_none(),
            "deleted file cannot be looked up"
        );
        cleanup(&cache, &path);
    }

    #[test]
    fn test_corrupt_cache_file_is_ignored() {
        let _g = test_lock();
        let cache = temp_cache_file();
        let path = make_audio_file();
        std::fs::write(&cache, "not valid json {").unwrap();
        *cache_lock() = None;
        // Must not panic; treated as an empty cache.
        assert!(lookup_in(&cache, &path).is_none());
        store_in(&cache, &path, &sample_result());
        assert!(
            lookup_in(&cache, &path).is_some(),
            "store repairs the cache"
        );
        cleanup(&cache, &path);
    }
}
