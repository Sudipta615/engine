//! Helper methods for AudioEngine — utility functions for playback info,
//! state management, event emission, and URI decoding.

use std::sync::Arc;

use super::AudioEngine;
use crate::buffer::{PlaybackInfo, PlaybackState};
use crate::events::EngineEvent;

impl AudioEngine {
    /// Read just the current PlaybackState without cloning the entire
    /// PlaybackInfo struct.
    pub fn current_state(&self) -> PlaybackState {
        self.playback_info.load().state
    }

    /// Emit an asynchronous discrete engine event to all subscribed handles.
    pub(crate) fn emit_event(&self, event: EngineEvent) {
        let _ = self.event_tx.try_send(event);
    }

    /// Emit an output device event on the dedicated output event channel.
    /// Only compiled when the `audio-output` feature is enabled.
    #[cfg(feature = "audio-output")]
    pub(crate) fn emit_output_event(&self, event: crate::events::OutputEvent) {
        let _ = self.output_event_tx.try_send(event);
    }

    /// Update playback state and publish an event if the state changed.
    pub fn update_playback_state(&self, state: PlaybackState) {
        let prev_state = self.current_state();
        if prev_state != state {
            match state {
                PlaybackState::Playing => self.emit_event(EngineEvent::PlaybackStarted),
                PlaybackState::Paused => self.emit_event(EngineEvent::PlaybackPaused),
                PlaybackState::Stopped => self.emit_event(EngineEvent::PlaybackStopped),
                PlaybackState::Buffering => {}
            }
        }
        self.playback_info.rcu(|old| {
            Arc::new(PlaybackInfo {
                state,
                ..old.as_ref().clone()
            })
        });
    }

    /// Helper for short one-field writes to PlaybackInfo.
    pub(super) fn write_playback_info<F: FnMut(&mut PlaybackInfo)>(&self, mut f: F) {
        self.playback_info.rcu(|old| {
            let mut next = old.as_ref().clone();
            f(&mut next);
            Arc::new(next)
        });
    }
}
