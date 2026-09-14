//! Track cache test suite: bounded LRU, invalidation on mtime/size, and metrics.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use engine::decode::channel_layout::ChannelLayout;
use engine::dsp::LoudnessMetadata;
use engine::track_cache::{CachedTrackInfo, TrackCache};

static CACHE_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_dummy_file(size_bytes: usize) -> PathBuf {
    let id = CACHE_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("track_cache_test_{}_{id}.dat", std::process::id()));
    let mut f = File::create(&path).expect("create file");
    f.write_all(&vec![0xAA; size_bytes]).expect("write bytes");
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn make_info(path: &Path, sr: u32, ch: usize) -> CachedTrackInfo {
    let (size, mtime) = TrackCache::file_size_and_mtime(path).unwrap_or((0, 0));
    CachedTrackInfo {
        canonical_path: path.to_path_buf(),
        file_size: size,
        mtime_secs: mtime,
        duration_secs: 60.0,
        codec: "pcm".into(),
        container: "wav".into(),
        sample_rate: sr,
        bit_depth: Some(16),
        channels: ch,
        channel_layout: ChannelLayout::from_count(ch),
        loudness: LoudnessMetadata::default(),
        true_peak_dbtp: None,
        profile: None,
    }
}

#[test]
fn test_track_cache_lru_bounded_capacity() {
    let mut cache = TrackCache::new(3); // Capacity = 3

    let f1 = write_dummy_file(100);
    let f2 = write_dummy_file(200);
    let f3 = write_dummy_file(300);
    let f4 = write_dummy_file(400);

    cache.insert(make_info(&f1, 44100, 2));
    cache.insert(make_info(&f2, 48000, 2));
    cache.insert(make_info(&f3, 96000, 2));

    assert_eq!(cache.len(), 3);

    // Lookup f1 to mark it recently used
    assert!(cache.lookup(&f1).is_some());

    // Insert f4 -> should evict f2 (least recently used)
    cache.insert(make_info(&f4, 192000, 2));
    assert_eq!(cache.len(), 3);

    assert!(cache.lookup(&f1).is_some());
    assert!(cache.lookup(&f2).is_none(), "f2 should have been evicted");
    assert!(cache.lookup(&f3).is_some());
    assert!(cache.lookup(&f4).is_some());

    let _ = std::fs::remove_file(&f1);
    let _ = std::fs::remove_file(&f2);
    let _ = std::fs::remove_file(&f3);
    let _ = std::fs::remove_file(&f4);
}

#[test]
fn test_track_cache_invalidation_on_mutation() {
    let mut cache = TrackCache::new(10);
    let f = write_dummy_file(500);

    cache.insert(make_info(&f, 44100, 2));
    assert!(cache.lookup(&f).is_some(), "first lookup should hit");

    // Sleep briefly so mtime changes
    std::thread::sleep(Duration::from_millis(50));

    // Append data to change file size and mtime
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&f)
            .expect("open append");
        file.write_all(b"APPENDED BYTES").expect("append");
    }

    // Cache should detect size/mtime change and invalidate
    let lookup = cache.lookup(&f);
    assert!(lookup.is_none(), "modified file must be invalidated");
    assert!(cache.invalidations() >= 1, "should record invalidation");

    let _ = std::fs::remove_file(&f);
}

#[test]
fn test_track_cache_hits_misses_metrics() {
    let mut cache = TrackCache::new(5);
    let f1 = write_dummy_file(100);
    let f_missing = PathBuf::from("/non/existent/path/file.wav");

    // Miss on non-existent
    assert!(cache.lookup(&f_missing).is_none());

    cache.insert(make_info(&f1, 48000, 2));

    // Hits
    assert!(cache.lookup(&f1).is_some());
    assert!(cache.lookup(&f1).is_some());

    assert_eq!(cache.hits(), 2);
    assert!(cache.misses() >= 1);

    let _ = std::fs::remove_file(&f1);
}
