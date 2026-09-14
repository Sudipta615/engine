//! Asynchronous next-track preloading and prepared-track lifecycle management.
//!
//! # Architecture & Guarantees
//!
//! - **Bounded Preload**: Maintains at most **one prepared next track** alongside
//!   the currently active track (max 2 open decoders / file handles engine-wide).
//! - **Non-blocking Realtime Safety**: File I/O, format inspection, and decoder
//!   instantiation occur on a dedicated background worker thread (`"track-preload"`).
//! - **Generational Invalidation**: Rapid queue modifications, skips, or flushes
//!   increment a request ID counter. Late-arriving background results with stale
//!   request IDs are discarded immediately without leaking decoders.
//! - **Fault Tolerance**: If the next track fails to open (missing file, corrupt
//!   media, unsupported codec), the failure is recorded as a non-fatal status.
//!   The active playing track is **never interrupted or destabilized**.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam::channel::{self, Receiver, Sender};
use log::{info, warn};

use crate::decode::{AudioFormatInfo, DecodeInfo, Decoder};
use crate::dsp::LoudnessMetadata;
use crate::source::AudioSource;
use crate::track_cache::{CachedTrackInfo, TrackCache};

/// An already-opened, prepared track ready for immediate, sample-accurate handoff.
pub struct PreparedTrack {
    /// The target source prepared.
    pub source: AudioSource,
    /// The pre-opened decoder ready to deliver audio frames.
    pub decoder: Decoder,
    /// Decoded format and stream info.
    pub info: DecodeInfo,
    /// Detailed format descriptor, if available.
    #[allow(dead_code)]
    pub format_info: Option<AudioFormatInfo>,
    /// Loudness metadata prepared for this track.
    pub loudness: Option<LoudnessMetadata>,
    /// Cached track info, if present in the track cache.
    #[allow(dead_code)]
    pub cached_info: Option<CachedTrackInfo>,
}

impl std::fmt::Debug for PreparedTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedTrack")
            .field("source", &self.source)
            .field("sample_rate", &self.info.sample_rate)
            .field("channels", &self.info.channels)
            .field("duration_secs", &self.info.duration_secs)
            .finish()
    }
}

/// Result of an asynchronous background preload job.
pub(crate) struct PreloadResult {
    pub request_id: u64,
    pub source: AudioSource,
    pub outcome: Result<PreparedTrack, String>,
}

/// Preload manager coordinating the single prepared next track.
pub(crate) struct PreloadManager {
    /// The single prepared next track, if one is currently ready.
    prepared: Option<PreparedTrack>,
    /// Active request generation ID.
    current_request_id: u64,
    /// Counter for issuing new request IDs.
    next_request_id: Arc<AtomicU64>,
    /// Source currently being preloaded in the background, if any.
    in_flight_source: Option<AudioSource>,
    /// Receiver for completed background preload jobs.
    result_rx: Receiver<PreloadResult>,
    /// Sender shared with background preload worker threads.
    result_tx: Sender<PreloadResult>,
    /// Last preload failure message for diagnostics.
    last_error: Option<(AudioSource, String)>,
}

impl PreloadManager {
    pub fn new() -> Self {
        let (result_tx, result_rx) = channel::bounded(16);
        Self {
            prepared: None,
            current_request_id: 0,
            next_request_id: Arc::new(AtomicU64::new(1)),
            in_flight_source: None,
            result_rx,
            result_tx,
            last_error: None,
        }
    }

    /// True if an asynchronous preload is currently executing in the background.
    pub fn is_in_flight(&self) -> bool {
        self.in_flight_source.is_some()
    }

    /// The source currently in flight in the background, if any.
    pub fn in_flight_source(&self) -> Option<&AudioSource> {
        self.in_flight_source.as_ref()
    }

    /// The prepared next source, if one is currently ready.
    pub fn prepared_source(&self) -> Option<AudioSource> {
        self.prepared.as_ref().map(|p| p.source.clone())
    }

    /// Whether a prepared track is currently ready.
    pub fn has_prepared(&self) -> bool {
        self.prepared.is_some()
    }

    /// Whether the manager currently holds a prepared track matching `source`.
    pub fn has_prepared_matching(&self, source: &AudioSource) -> bool {
        self.prepared.as_ref().is_some_and(|p| &p.source == source)
    }

    /// Take the prepared track for handoff (leaving `prepared` empty).
    pub fn take_prepared(&mut self) -> Option<PreparedTrack> {
        self.prepared.take()
    }

    /// Discard any prepared track (closing its decoder and file handles).
    pub fn discard_prepared(&mut self) {
        if let Some(p) = self.prepared.take() {
            info!("Discarded prepared track '{}'", p.source);
        }
    }

    /// Cancel any running preload job and invalidate existing prepared track.
    pub fn cancel(&mut self) {
        self.discard_prepared();
        self.current_request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        self.in_flight_source = None;
    }

    /// Last recorded preload failure, if any.
    #[allow(dead_code)]
    pub fn last_error(&self) -> Option<&(AudioSource, String)> {
        self.last_error.as_ref()
    }

    /// Poll for completed preload jobs and update prepared state.
    ///
    /// Called once per engine tick on the engine thread. Non-blocking.
    pub fn poll_results(&mut self, cache: &mut TrackCache) {
        while let Ok(res) = self.result_rx.try_recv() {
            if res.request_id != self.current_request_id {
                // Outdated job from an earlier generation (e.g. user skipped or changed queue)
                continue;
            }

            self.in_flight_source = None;
            match res.outcome {
                Ok(prepared) => {
                    info!("Next track prepared: '{}'", prepared.source);
                    // Update track cache with newly inspected entry
                    if let AudioSource::File(ref path) = prepared.source {
                        let entry = TrackCache::create_entry_from_decoder(path, &prepared.decoder);
                        cache.insert(entry);
                    }
                    self.prepared = Some(prepared);
                    self.last_error = None;
                }
                Err(err) => {
                    warn!("Preload failed for '{}': {}", res.source, err);
                    self.last_error = Some((res.source, err));
                    self.prepared = None;
                }
            }
        }
    }

    /// Start asynchronous preloading for `source` if not already prepared or in flight.
    pub fn request_preload(&mut self, source: AudioSource, cache: &mut TrackCache) {
        // If already prepared for this exact source, nothing to do.
        if self.has_prepared_matching(&source) {
            return;
        }

        // If a preload is already in flight for this exact source, let it finish.
        if self.in_flight_source.as_ref() == Some(&source) {
            return;
        }

        // Discard any existing prepared track for a different source.
        self.discard_prepared();

        // Increment generation request ID to invalidate any prior running jobs.
        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        self.current_request_id = request_id;
        self.in_flight_source = Some(source.clone());

        let result_tx = self.result_tx.clone();
        let target_source = source.clone();

        // Check if metadata is cached to warm the worker
        let cached_info = match &source {
            AudioSource::File(path) => cache.lookup(path),
            _ => None,
        };

        // Spawn background worker thread
        let worker_res = std::thread::Builder::new()
            .name("track-preload".into())
            .spawn(move || {
                let outcome = Self::execute_preload(&target_source, cached_info);
                let _ = result_tx.send(PreloadResult {
                    request_id,
                    source: target_source,
                    outcome,
                });
            });

        if let Err(e) = worker_res {
            warn!("Failed to spawn track-preload worker: {}", e);
            self.in_flight_source = None;
        }
    }

    /// Background worker execution: opens decoder and gathers metadata without blocking audio thread.
    fn execute_preload(
        source: &AudioSource,
        cached: Option<CachedTrackInfo>,
    ) -> Result<PreparedTrack, String> {
        let decoder = match source {
            AudioSource::File(path) => {
                Decoder::open(path).map_err(|e| format!("Decoder::open failed: {e}"))?
            }
            AudioSource::Uri(uri) => {
                let path_buf = if let Some(stripped) = uri.strip_prefix("file://") {
                    crate::decode::percent_decode(stripped)
                        .map(PathBuf::from)
                        .ok_or_else(|| format!("Invalid file URI: {uri}"))?
                } else {
                    PathBuf::from(uri)
                };
                Decoder::open(&path_buf).map_err(|e| format!("Decoder::open failed: {e}"))?
            }
            AudioSource::Memory {
                data,
                extension_hint,
            } => Decoder::open_memory(data.clone(), extension_hint.as_deref())
                .map_err(|e| format!("Decoder::open_memory failed: {e}"))?,
        };

        let info = decoder.info().clone();
        let format_info = Some(decoder.format_info().clone());

        // Extract or reuse loudness metadata
        let loudness = if let Some(ref c) = cached {
            Some(c.loudness)
        } else if let AudioSource::File(ref path) = source {
            let mut meta = crate::decode::extract_loudness_metadata(path);
            if meta.ebu_r128_loudness.is_none() {
                if let Some(cached_res) = crate::decode::loudness_cache::lookup(path) {
                    meta.ebu_r128_loudness = cached_res.ebu_r128_loudness;
                    meta.ebu_r128_peak = cached_res.ebu_r128_peak_dbtp;
                    meta.replaygain_track_db = cached_res.replaygain_track_db;
                    meta.replaygain_track_peak = cached_res.replaygain_track_peak;
                }
            }
            Some(meta)
        } else {
            None
        };

        Ok(PreparedTrack {
            source: source.clone(),
            decoder,
            info,
            format_info,
            loudness,
            cached_info: cached,
        })
    }
}

impl Default for PreloadManager {
    fn default() -> Self {
        Self::new()
    }
}
