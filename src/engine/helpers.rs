//! Helper methods for AudioEngine — utility functions for playback info,
//! state management, and URI decoding.

use std::sync::Arc;

use super::AudioEngine;
use crate::buffer::{PlaybackInfo, PlaybackState};

impl AudioEngine {
    /// Read just the current PlaybackState without cloning the entire
    /// PlaybackInfo struct.
    pub fn current_state(&self) -> PlaybackState {
        self.playback_info.load().state
    }

    pub fn update_playback_state(&self, state: PlaybackState) {
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

/// Percent-decode a URI-encoded string (e.g. `%20` → space).
/// Returns `None` if the encoding is malformed.
pub(super) fn percent_decode(s: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chars = s.as_bytes().iter().copied();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next()?;
            let h2 = chars.next()?;
            let pair = [h1, h2];
            let hex = std::str::from_utf8(&pair).ok()?;
            let val = u8::from_str_radix(hex, 16).ok()?;
            bytes.push(val);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}
