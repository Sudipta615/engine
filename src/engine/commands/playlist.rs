//! Playlist command handlers — enqueue, remove, clear, skip, shuffle, repeat.
//!
//! Every mutation also publishes `PlaylistChanged` so the host UI stays in
//! sync without polling.

use log::{info, warn};

use crate::{events::EngineEvent, playlist::RepeatMode, source::AudioSource};

use super::super::AudioEngine;

impl AudioEngine {
    /// Push a source to the end of the queue.  Idempotent — queueing the
    /// current track or a duplicate is harmless.
    pub(super) fn handle_enqueue(&mut self, source: AudioSource) {
        self.playlist.enqueue(source.clone());
        self.emit_playlist_changed();
        info!(
            "Enqueued '{}' — queue length {}",
            source,
            self.playlist.len()
        );
    }

    /// Remove the entry at `index` from the queue.  If this was the current
    /// track, playback stops.
    pub(super) fn handle_remove_from_playlist(&mut self, index: usize) {
        let was_current = self.playlist.current_index() == Some(index);
        if let Some(removed) = self.playlist.remove(index) {
            if was_current {
                // Current track removed — stop decoding and reset.
                self.handle_stop();
                info!(
                    "Removed current track '{}' at index {}; playback stopped",
                    removed, index
                );
            } else {
                info!(
                    "Removed '{}' at index {} — queue length {}",
                    removed,
                    index,
                    self.playlist.len()
                );
            }
        } else {
            warn!("RemoveFromPlaylist({}): index out of bounds", index);
        }
        self.emit_playlist_changed();
    }

    /// Clear the entire queue.  The currently-playing track (if any) keeps
    /// playing until it ends or the user stops it.
    pub(super) fn handle_clear_playlist(&mut self) {
        self.playlist.clear();
        self.emit_playlist_changed();
        info!("Playlist cleared");
    }

    /// Jump directly to playlist index `index` and start playing it.  If the
    /// index is out of bounds the command is silently ignored.
    pub(super) fn handle_play_index(&mut self, index: usize) {
        let Some(src) = self.playlist.play_index(index) else {
            warn!("PlayIndex({}): index out of bounds", index);
            return;
        };
        self.emit_playlist_changed();

        // Manually load the selected source — replacing whatever was playing.
        match self.load_source(&src) {
            Ok(_) => {
                log::debug!("PlayIndex({}): loaded {}", index, src);
            }
            Err(e) => {
                warn!("PlayIndex({}): failed to load {}: {}", index, src, e);
                self.emit_event(EngineEvent::Error(format!(
                    "Failed to open playlist entry {}: {}",
                    index, e
                )));
                // Advance past the broken entry so the host can try the next one.
                self.handle_next();
                return;
            }
        }

        self.handle_play();
    }

    /// Skip to the next playlist entry.  Uses the gapless/crossfade transition
    /// machinery when a current track exists; falls back to a fresh load
    /// otherwise.
    pub(super) fn handle_next(&mut self) {
        let Some(src) = self.playlist.advance() else {
            // Queue exhausted — stop playback.
            self.emit_playlist_changed();
            self.handle_stop();
            info!("Queue exhausted; stopping");
            return;
        };
        self.emit_playlist_changed();
        self.play_source_after_track_end(&src);
    }

    /// Skip back to the previous playlist entry.
    pub(super) fn handle_previous(&mut self) {
        let Some(src) = self.playlist.previous() else {
            return;
        };
        self.emit_playlist_changed();
        self.play_source_after_track_end(&src);
    }

    /// Set the repeat mode and publish the change.
    pub(super) fn handle_set_repeat_mode(&mut self, mode: RepeatMode) {
        self.playlist.set_repeat(mode);
        info!("Repeat mode set to {:?}", mode);
        // No event needed — the mode is visible through the handle inspection.
    }

    /// Enable or disable shuffle.
    pub(super) fn handle_set_shuffle(&mut self, enabled: bool) {
        self.playlist.set_shuffle(enabled);
        info!("Shuffle {}", if enabled { "on" } else { "off" });
    }

    // ── helpers ────────────────────────────────────────────────────────────

    /// Called at EOS (or manual Next/Previous) to load a source into the
    /// engine.  Prefers the gapless/crossfade machinery when the current
    /// stream exists (a file handoff reuses the pre-opened decoder, the
    /// resampler tail, and the limiter lookahead so no gap is heard);
    /// falls back to a fresh `load_source` for memory/URI sources or when
    /// the handoff fails.
    fn play_source_after_track_end(&mut self, src: &AudioSource) {
        if matches!(src, AudioSource::File(_)) && self.stream.is_some() {
            if let AudioSource::File(ref path) = src {
                // `prepare_next_track` pre-opens the decoder and prepares
                // loudness metadata for the incoming track.
                match self.prepare_next_track(path) {
                    Ok(_) => {
                        let crossfade_transition = self.config.crossfade.enabled
                            || matches!(
                                self.config.transition_mode,
                                config::TransitionMode::Crossfade | config::TransitionMode::Fade
                            );
                        if crossfade_transition {
                            // Crossfade/Fade: begin the overlapping transition
                            // immediately.
                            self.scratch.crossfade_triggered = false;
                            self.begin_crossfade_transition();
                        } else {
                            // Gapless/Stop: swap the prepared decoder in now.
                            self.swap_to_next_track_now();
                        }
                        return;
                    }
                    Err(e) => {
                        warn!("prepare_next_track failed for next entry: {}", e);
                        // Fall through to a fresh load.
                    }
                }
            }
        }
        // Non-file source or no active stream (or prepare failed): fresh load.
        match self.load_source(src) {
            Ok(_) => self.handle_play(),
            Err(e) => {
                warn!("Failed to load next track: {}", e);
                self.emit_event(EngineEvent::Error(format!(
                    "Failed to load next track: {}",
                    e
                )));
                self.handle_stop();
            }
        }
    }

    /// For gapless manual Next: consume the already-prepared decoder and
    /// swap it into the active stream immediately, preserving the current
    /// DSP pipeline (limiter lookahead, resampler tail) so no gap is heard.
    fn swap_to_next_track_now(&mut self) {
        use crate::engine::PlaybackStream;
        let next_path = match self.loudness_scan.next_track_path.take() {
            Some(p) => p,
            None => {
                warn!("swap_to_next_track_now called but no next_track_path prepared");
                return;
            }
        };
        // Extract the resampler from the current stream so swap_to_next_track
        // can pass it through or replace it when rates differ.
        let mut old_resampler: Option<_> = {
            #[cfg(feature = "resample")]
            {
                match self.stream.as_mut() {
                    Some(PlaybackStream::Single { resampler, .. }) => resampler.take(),
                    _ => None,
                }
            }
            #[cfg(not(feature = "resample"))]
            None
        };
        #[cfg(not(feature = "resample"))]
        let _ = old_resampler;

        #[cfg(feature = "resample")]
        match self.swap_to_next_track(&next_path, &mut old_resampler) {
            Ok(_) => {}
            Err(e) => {
                warn!("Gapless handoff failed: {}; falling back to fresh load", e);
                self.handle_stop();
            }
        }
        #[cfg(not(feature = "resample"))]
        {
            let _ = self.swap_to_next_track(&next_path, &mut None);
        }
    }

    /// Emit a `PlaylistChanged` event from the engine's current state and
    /// publish the queue position/length into the atomic `PlaybackInfo` so
    /// hosts can poll it without subscribing to events.
    pub(crate) fn emit_playlist_changed(&self) {
        self.write_playback_info(|pb| {
            pb.playlist_index = self.playlist.current_index();
            pb.playlist_length = self.playlist.len();
        });
        self.emit_event(EngineEvent::PlaylistChanged {
            current_index: self.playlist.current_index(),
            length: self.playlist.len(),
        });
    }
}
