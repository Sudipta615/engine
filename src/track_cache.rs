//! Small, bounded in-memory cache for track metadata, format info, and analysis.
//!
//! # Architecture & Safety
//!
//! - **Bounded RAM**: strictly caches lightweight metadata (identity, format,
//!   duration, loudness, and profile references). It **never** caches full-song
//!   PCM or large decoded audio buffers, keeping RAM usage suitable for legacy
//!   and embedded devices (e.g. ~50 KiB for 128 entries).
//! - **Filesystem Validation**: On lookup, entries are validated against the
//!   source file's current size and modification time. If the file has changed or
//!   been removed, the stale entry is automatically invalidated.
//! - **Cache Reuse**: Leverages existing on-disk [`crate::decode::loudness_cache`]
//!   and [`crate::profile::cache`] rather than duplicating storage or re-analyzing
//!   tracks unnecessarily.
//! - **Thread Safety**: Independent instance owned by the engine; lookup and
//!   population occur off the realtime audio thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::decode::channel_layout::ChannelLayout;
use crate::decode::{AudioFormatInfo, DecodeInfo, Decoder};
use crate::dsp::LoudnessMetadata;
use crate::profile::AudioProfile;

/// Default capacity for the in-memory track cache (128 entries ≈ 50 KiB).
pub const DEFAULT_TRACK_CACHE_CAPACITY: usize = 128;

/// A cached record containing metadata and technical format details for one track.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedTrackInfo {
    /// Canonical file path (or unique resource identifier).
    pub canonical_path: PathBuf,
    /// File size in bytes at inspection time.
    pub file_size: u64,
    /// File modification time in whole seconds since the Unix epoch.
    pub mtime_secs: u64,
    /// Track duration in seconds.
    pub duration_secs: f32,
    /// Codec identifier (e.g. "FLAC", "MP3", "DSD", "Opus").
    pub codec: String,
    /// Container/format name.
    pub container: String,
    /// Source sample rate in Hz.
    pub sample_rate: u32,
    /// Bit depth in bits (e.g. 16, 24, 32), if known.
    pub bit_depth: Option<u32>,
    /// Number of audio channels.
    pub channels: usize,
    /// Channel layout.
    pub channel_layout: ChannelLayout,
    /// Pre-computed loudness metadata (EBU R128 and/or ReplayGain).
    pub loudness: LoudnessMetadata,
    /// True peak estimate in dBTP, if available.
    pub true_peak_dbtp: Option<f32>,
    /// Associated AudioProfile from the profile analysis cache, if present.
    pub profile: Option<Arc<AudioProfile>>,
}

/// Bounded LRU cache for [`CachedTrackInfo`].
#[derive(Debug, Clone)]
pub struct TrackCache {
    entries: HashMap<String, CachedTrackInfo>,
    access_order: Vec<String>,
    capacity: usize,
    hits: u64,
    misses: u64,
    invalidations: u64,
}

impl TrackCache {
    /// Create a new cache with the specified capacity limit.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            access_order: Vec::new(),
            capacity: capacity.max(1),
            hits: 0,
            misses: 0,
            invalidations: 0,
        }
    }

    /// Default constructor with standard bounded capacity.
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_TRACK_CACHE_CAPACITY)
    }

    /// Number of entries currently stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is currently empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Maximum number of entries allowed before eviction.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Total cache hits since construction.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Total cache misses since construction.
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Total invalidations triggered by file modifications.
    pub fn invalidations(&self) -> u64 {
        self.invalidations
    }

    /// Canonical cache key for a path.
    pub fn key_for(path: &Path) -> String {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        canonical.to_string_lossy().into_owned()
    }

    /// Get current filesystem size and modification time for validation.
    pub fn file_size_and_mtime(path: &Path) -> Option<(u64, u64)> {
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

    /// Look up cached track information for `path`.
    ///
    /// Validates the entry against the file on disk. If the file has changed
    /// or cannot be read, the entry is invalidated and `None` is returned.
    pub fn lookup(&mut self, path: &Path) -> Option<CachedTrackInfo> {
        let (disk_size, disk_mtime) = match Self::file_size_and_mtime(path) {
            Some(sm) => sm,
            None => {
                // File does not exist or cannot be accessed; remove any stale entry.
                let key = Self::key_for(path);
                if self.entries.remove(&key).is_some() {
                    self.access_order.retain(|k| k != &key);
                    self.invalidations += 1;
                }
                self.misses += 1;
                return None;
            }
        };

        let key = Self::key_for(path);
        let valid_entry = if let Some(entry) = self.entries.get(&key) {
            if entry.file_size == disk_size && entry.mtime_secs == disk_mtime {
                Some(entry.clone())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(entry) = valid_entry {
            self.touch(&key);
            self.hits += 1;
            return Some(entry);
        } else if self.entries.contains_key(&key) {
            // Stale entry: file modified since last inspection
            self.entries.remove(&key);
            self.access_order.retain(|k| k != &key);
            self.invalidations += 1;
        }

        self.misses += 1;
        None
    }

    /// Insert or update an entry in the cache, evicting the least recently used
    /// entry if at capacity.
    pub fn insert(&mut self, info: CachedTrackInfo) {
        let key = info.canonical_path.to_string_lossy().into_owned();

        if self.entries.contains_key(&key) {
            self.touch(&key);
            self.entries.insert(key, info);
            return;
        }

        // Evict if at capacity
        while self.entries.len() >= self.capacity && !self.access_order.is_empty() {
            let oldest = self.access_order.remove(0);
            self.entries.remove(&oldest);
        }

        self.access_order.push(key.clone());
        self.entries.insert(key, info);
    }

    /// Populate a [`CachedTrackInfo`] using an opened [`Decoder`], querying
    /// existing loudness and profile caches.
    pub fn create_entry_from_decoder(path: &Path, decoder: &Decoder) -> CachedTrackInfo {
        let (file_size, mtime_secs) = Self::file_size_and_mtime(path).unwrap_or((0, 0));
        let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        let info: &DecodeInfo = decoder.info();
        let format_info: &AudioFormatInfo = decoder.format_info();

        // Query existing loudness cache if available
        let mut loudness = crate::decode::extract_loudness_metadata(path);
        let mut true_peak = format_info.true_peak_dbtp;

        if loudness.ebu_r128_loudness.is_none() {
            if let Some(cached) = crate::decode::loudness_cache::lookup(path) {
                loudness.ebu_r128_loudness = cached.ebu_r128_loudness;
                loudness.ebu_r128_peak = cached.ebu_r128_peak_dbtp;
                loudness.replaygain_track_db = cached.replaygain_track_db;
                loudness.replaygain_track_peak = cached.replaygain_track_peak;
                if true_peak.is_none() {
                    true_peak = cached.ebu_r128_peak_dbtp;
                }
            }
        }

        // Query existing profile cache if available
        let profile = crate::profile::lookup(path).map(Arc::new);

        CachedTrackInfo {
            canonical_path,
            file_size,
            mtime_secs,
            duration_secs: info.duration_secs,
            codec: info.codec.clone(),
            container: format_info.container.clone(),
            sample_rate: info.sample_rate,
            bit_depth: format_info.bit_depth,
            channels: info.channels,
            channel_layout: format_info.channel_layout.clone(),
            loudness,
            true_peak_dbtp: true_peak,
            profile,
        }
    }

    /// Invalidate an entry for `path` explicitly.
    pub fn invalidate(&mut self, path: &Path) {
        let key = Self::key_for(path);
        if self.entries.remove(&key).is_some() {
            self.access_order.retain(|k| k != &key);
            self.invalidations += 1;
        }
    }

    /// Clear all entries from the cache.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.access_order.clear();
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.access_order.iter().position(|k| k == key) {
            let k = self.access_order.remove(pos);
            self.access_order.push(k);
        }
    }
}

impl Default for TrackCache {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_track_cache_lru_and_eviction() {
        let mut cache = TrackCache::new(2);

        let info1 = CachedTrackInfo {
            canonical_path: PathBuf::from("/tmp/track1.flac"),
            file_size: 100,
            mtime_secs: 1000,
            duration_secs: 180.0,
            codec: "FLAC".into(),
            container: "flac".into(),
            sample_rate: 44100,
            bit_depth: Some(16),
            channels: 2,
            channel_layout: ChannelLayout::Stereo,
            loudness: LoudnessMetadata::default(),
            true_peak_dbtp: None,
            profile: None,
        };

        let mut info2 = info1.clone();
        info2.canonical_path = PathBuf::from("/tmp/track2.flac");

        let mut info3 = info1.clone();
        info3.canonical_path = PathBuf::from("/tmp/track3.flac");

        cache.insert(info1.clone());
        cache.insert(info2.clone());
        assert_eq!(cache.len(), 2);

        // Access track1 to make it most recently used
        let key1 = info1.canonical_path.to_string_lossy().to_string();
        cache.touch(&key1);

        // Insert track3, which should evict track2 (the LRU)
        cache.insert(info3.clone());
        assert_eq!(cache.len(), 2);

        let key2 = info2.canonical_path.to_string_lossy().to_string();
        let key3 = info3.canonical_path.to_string_lossy().to_string();
        assert!(cache.entries.contains_key(&key1));
        assert!(!cache.entries.contains_key(&key2));
        assert!(cache.entries.contains_key(&key3));
    }
}
