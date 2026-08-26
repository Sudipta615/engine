//! State for background track loudness scanning and metadata synchronization.

use std::path::PathBuf;

use log::{info, warn};

use crate::{buffer::EngineCommand, dsp::LoudnessMetadata};

#[derive(Default)]
pub(crate) struct LoudnessScanState {
    /// Path of the currently loaded track. Used to match background loudness
    /// scan results to the track that is actually playing.
    pub(crate) current_track_path: Option<PathBuf>,
    /// Path of the incoming (next) track during a crossfade transition.
    pub(crate) incoming_track_path: Option<PathBuf>,
    /// Path of the next track to crossfade into, if provided.
    pub(crate) next_track_path: Option<PathBuf>,
    /// Tag-derived loudness metadata for the current track.
    pub(crate) pending_loudness_metadata: Option<LoudnessMetadata>,
    /// Tag-derived loudness metadata for the incoming track.
    pub(crate) pending_incoming_loudness_metadata: Option<LoudnessMetadata>,
    /// True while a background loudness scan is running. Scans are serialized
    /// so at most one decode thread is active at a time.
    pub(crate) loudness_scan_in_flight: bool,
}

impl super::AudioEngine {
    /// Spawn a background loudness scan for the currently loaded track if it
    /// lacks EBU R128 metadata.
    ///
    /// Scans are serialized (at most one in flight). A result for a track
    /// that has since been superseded is discarded by the completion handler,
    /// which then re-arms scans for anything still missing metadata.
    pub(super) fn start_loudness_scan(&mut self) {
        if self.loudness_scan.loudness_scan_in_flight {
            return;
        }
        let Some(path) = self.loudness_scan.current_track_path.clone() else {
            return;
        };
        let needs_scan = self
            .loudness_scan
            .pending_loudness_metadata
            .as_ref()
            .is_none_or(|m| m.ebu_r128_loudness.is_none() || m.replaygain_track_db.is_none());
        if !needs_scan {
            return;
        }
        self.spawn_loudness_scan(path);
    }

    /// Spawn a background loudness scan for the incoming (next) track if it
    /// lacks EBU R128 / ReplayGain metadata. The path is either the pending next
    /// track (`next_track_path`) or the track currently fading in
    /// (`incoming_track_path`).
    pub(super) fn start_incoming_loudness_scan(&mut self) {
        if self.loudness_scan.loudness_scan_in_flight {
            return;
        }
        let Some(path) = self
            .loudness_scan
            .incoming_track_path
            .clone()
            .or_else(|| self.loudness_scan.next_track_path.clone())
        else {
            return;
        };
        let needs_scan = self
            .loudness_scan
            .pending_incoming_loudness_metadata
            .as_ref()
            .is_none_or(|m| m.ebu_r128_loudness.is_none() || m.replaygain_track_db.is_none());
        if !needs_scan {
            return;
        }
        self.spawn_loudness_scan(path);
    }

    /// Shared scan-thread spawner. Guards on `loudness_scan_in_flight` so at
    /// most one decode thread is active at a time.
    pub(super) fn spawn_loudness_scan(&mut self, path: PathBuf) {
        if self.loudness_scan.loudness_scan_in_flight {
            return;
        }
        self.loudness_scan.loudness_scan_in_flight = true;
        let cmd_tx = self.cmd_tx.clone();
        let path_display = path.display().to_string();
        match std::thread::Builder::new()
            .name("loudness-scan".into())
            .spawn(move || {
                let result = crate::decode::scan_track_loudness(&path);
                // Persist the result keyed by the file's size + mtime so an
                // unchanged track is never re-scanned on a later load.
                if let Some(ref r) = result {
                    crate::decode::loudness_cache::store(&path, r);
                }
                let _ = cmd_tx.send(EngineCommand::LoudnessScanComplete { path, result });
            }) {
            Ok(_) => {
                info!("Background loudness scan started for {}", path_display);
            }
            Err(e) => {
                self.loudness_scan.loudness_scan_in_flight = false;
                warn!("Failed to spawn loudness scan thread: {}", e);
            }
        }
    }
}
