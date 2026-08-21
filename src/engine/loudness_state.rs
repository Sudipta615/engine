//! State for background track loudness scanning and metadata synchronization.

use std::path::PathBuf;
use crate::dsp::loudness::LoudnessMetadata;

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
