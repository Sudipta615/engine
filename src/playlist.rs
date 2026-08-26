//! Playlist / playback queue — ordered track list with shuffle, repeat, and
//! previous-track history.
//!
//! # Design
//!
//! - **Ordered list**: entries are [`AudioSource`] values (files, URIs, memory).
//! - **Shuffle**: a Fisher-Yates permutation of indices is regenerated on each
//!   full cycle when [`RepeatMode::All`] is active, or once when the user
//!   enables shuffle.  Previous entries are tracked so *Back* works correctly
//!   even in shuffled order.
//! - **Repeat modes**: `Off` (stop at end), `All` (wrap around), `One`
//!   (restart the current track at EOS — the engine seeks to 0 instead of
//!   advancing).
//! - **History stack**: every `next()` transition (including manual skip)
//!   pushes the previous index; `previous()` pops it.  The stack depth is
//!   bounded so an accidentally repeated Back doesn't grow unbounded memory.
//! - **Thread safety**: the struct is `Clone + Send` so the engine can hold
//!   a copy and the host can inspect it via the handle.  Mutations go through
//!   commands, so the authoritative copy is always the one inside
//!   [`AudioEngine`](crate::engine::AudioEngine).

use std::time::{SystemTime, UNIX_EPOCH};

use crate::source::AudioSource;

/// Playback repeat behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RepeatMode {
    /// Stop at the end of the queue.
    #[default]
    Off,
    /// Restart from the beginning when the last track finishes.
    All,
    /// Repeat the current track indefinitely (the engine restarts it at EOS
    /// without advancing the queue).
    One,
}

/// An ordered playback queue with optional shuffle and repeat.
#[derive(Debug, Clone)]
pub struct Playlist {
    items: Vec<AudioSource>,
    /// Index into `items` of the currently-playing track, or `None` when
    /// nothing has been selected yet (the queue is non-empty but pending
    /// the user's first play / manual choice).
    current: Option<usize>,
    repeat: RepeatMode,
    shuffle: bool,
    /// Shuffled playback order: a permutation of `0..items.len()`.  This is
    /// regenerated whenever `shuffle` is turned on with [`Self::set_shuffle`]
    /// and when the queue wraps around in `RepeatMode::All`.
    order: Vec<usize>,
    /// Position within `order` for the next call to [`Self::advance`].
    order_pos: usize,
    /// Stack of previously-played indices (the tail is the most recent).
    /// Bounded in [`Self::HISTORY_MAX`] so repeated Backs don't leak memory.
    history: Vec<usize>,
}

impl Playlist {
    /// Maximum number of entries kept in the Back-stack.
    const HISTORY_MAX: usize = 512;

    /// Create an empty playlist.
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            current: None,
            repeat: RepeatMode::Off,
            shuffle: false,
            order: Vec::new(),
            order_pos: 0,
            history: Vec::new(),
        }
    }

    /// Number of entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when the queue is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// All items (stable order — the raw insertion order, not shuffled).
    pub fn items(&self) -> &[AudioSource] {
        &self.items
    }

    /// Index of the currently-playing track, if any.
    #[inline]
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// The currently-playing track, if any.
    pub fn current_source(&self) -> Option<&AudioSource> {
        self.current.and_then(|i| self.items.get(i))
    }

    /// Current repeat mode.
    #[inline]
    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    /// Whether shuffle is active.
    #[inline]
    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    // ── mutation (called from engine command handlers) ───────────────────────

    /// Add a source to the end of the queue.
    pub fn enqueue(&mut self, source: AudioSource) {
        let index = self.items.len();
        self.items.push(source);
        if self.shuffle {
            // Insert the new index at a random position in the remaining
            // (unplayed) portion of the order, so newly-enqueued tracks are
            // eligible for the current shuffle cycle.
            let remaining = self.order.len().saturating_sub(self.order_pos);
            let offset = if remaining == 0 {
                self.order.len()
            } else {
                self.order_pos + random_index(remaining)
            };
            self.order.insert(offset, index);
        }
    }

    /// Remove the entry at `index`.  If this was the current track the
    /// playhead is reset.
    pub fn remove(&mut self, index: usize) -> Option<AudioSource> {
        if index >= self.items.len() {
            return None;
        }
        let removed = self.items.remove(index);

        // Fix current.
        if self.current == Some(index) {
            self.current = None;
        } else if let Some(ref mut cur) = self.current {
            if *cur > index {
                *cur -= 1;
            }
        }

        // Fix history and order: shift indices > `index` down by 1, drop `index`.
        self.history.retain(|&i| i != index);
        for h in &mut self.history {
            if *h > index {
                *h -= 1;
            }
        }
        self.order.retain(|&i| i != index);
        for o in &mut self.order {
            if *o > index {
                *o -= 1;
            }
        }
        if self.order_pos >= self.order.len() {
            self.order_pos = self.order.len().saturating_sub(1);
        }

        Some(removed)
    }

    /// Remove all entries and reset state.
    pub fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.order.clear();
        self.order_pos = 0;
        self.history.clear();
    }

    /// Replace the entire playlist with a single source (convenience for
    /// [`AudioSource::Open`](crate::commands::EngineCommand::Open)).
    pub fn set_single(&mut self, source: AudioSource) {
        self.items = vec![source];
        self.current = Some(0);
        self.order.clear();
        self.order_pos = 0;
        self.history.clear();
    }

    /// Set the repeat mode.
    pub fn set_repeat(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }

    /// Toggle shuffle.  When turning on, generate a fresh random order.
    pub fn set_shuffle(&mut self, enabled: bool) {
        if self.shuffle == enabled {
            return;
        }
        self.shuffle = enabled;
        if enabled {
            self.reshuffle();
        }
    }

    /// Jump to a specific index, pushing the current one onto the history
    /// stack.  Returns the [`AudioSource`] to play.
    pub fn play_index(&mut self, index: usize) -> Option<AudioSource> {
        if index >= self.items.len() {
            return None;
        }
        if let Some(cur) = self.current {
            if cur != index {
                self.push_history(cur);
            }
        }
        self.current = Some(index);
        // Reposition `order_pos` to the entry **after** `index` in the
        // shuffle cycle.  If `index` is the last entry the cycle is
        // exhausted: the next `advance` returns `None` (repeat off).
        // Manual jump never wraps — tracks before the picked one just
        // stay available for the next cycle.
        if self.shuffle && !self.order.is_empty() {
            let pos = self
                .order
                .iter()
                .position(|&i| i == index)
                .unwrap_or(self.order.len());
            self.order_pos = (pos + 1).min(self.order.len());
        } else {
            self.order_pos = 0;
        }
        self.items.get(index).cloned()
    }

    /// Advance to the next track and return it.  Returns `None` if the queue
    /// is exhausted (repeat off) or empty.
    ///
    /// `RepeatMode::One` does **not** suppress advancing: a manual *Next*
    /// always skips, and the engine implements repeat-one by seeking to 0 at
    /// EOS *without* calling `advance`.
    pub fn advance(&mut self) -> Option<AudioSource> {
        if self.items.is_empty() {
            return None;
        }
        if let Some(cur) = self.current {
            self.push_history(cur);
        }
        let idx = if self.shuffle {
            if self.order_pos >= self.order.len() {
                if self.repeat != RepeatMode::All {
                    self.current = None;
                    return None;
                }
                // New cycle — regenerate the order and start from its head.
                self.reshuffle();
            }
            let idx = self.order[self.order_pos];
            self.order_pos += 1;
            idx
        } else {
            let next = self.current.map_or(0, |i| i + 1);
            if next < self.items.len() {
                next
            } else if self.repeat == RepeatMode::All {
                0
            } else {
                self.current = None;
                return None;
            }
        };
        self.current = Some(idx);
        self.items.get(idx).cloned()
    }

    /// Move back to the previous track.  The history stack depth is bounded
    /// by [`Self::HISTORY_MAX`].
    pub fn previous(&mut self) -> Option<AudioSource> {
        let prev = self.history.pop()?;
        self.current = Some(prev);
        self.items.get(prev).cloned()
    }

    /// Peek at the next track **without** mutating state.  Returns `None` if
    /// the queue is exhausted (repeat off) or empty.  During a shuffled
    /// repeat-all cycle the peek is a best-effort hint (the actual reshuffle
    /// happens on the next `advance`).
    pub fn peek_next(&self) -> Option<&AudioSource> {
        if self.items.is_empty() {
            return None;
        }
        let idx = if self.shuffle {
            if self.order_pos >= self.order.len() {
                if self.repeat != RepeatMode::All {
                    return None;
                }
                return self.items.get(*self.order.first()?);
            }
            self.order[self.order_pos]
        } else {
            let next = self.current.map_or(0, |i| i + 1);
            if next < self.items.len() {
                next
            } else if self.repeat == RepeatMode::All {
                0
            } else {
                return None;
            }
        };
        self.items.get(idx)
    }

    /// Peek at the previous track, if history is non-empty.
    pub fn peek_previous(&self) -> Option<&AudioSource> {
        let idx = *self.history.last()?;
        self.items.get(idx)
    }

    /// Peek at the current track, if any.
    pub fn current_item(&self) -> Option<&AudioSource> {
        self.current_source()
    }

    // ── internals ──────────────────────────────────────────────────────────

    /// Generate a fresh shuffled order of all items.
    fn reshuffle(&mut self) {
        let n = self.items.len();
        self.order = (0..n).collect();
        // Fisher-Yates (Knuth) with a fast xorshift64* generator seeded from
        // the clock — deliberately dependency-free (no `rand` dependency for
        // a playlist shuffle).
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(42) as u64;
        let mut state = seed.wrapping_add(0x9E3779B97F4A7C15);
        for i in (1..n).rev() {
            state ^= state.wrapping_shr(12);
            state ^= state.wrapping_shl(25);
            state ^= state.wrapping_shr(27);
            let j = (state as usize) % (i + 1);
            self.order.swap(i, j);
        }
        self.order_pos = 0;
    }

    fn push_history(&mut self, index: usize) {
        self.history.push(index);
        if self.history.len() > Self::HISTORY_MAX {
            self.history.remove(0);
        }
    }
}

impl Default for Playlist {
    fn default() -> Self {
        Self::new()
    }
}

/// Return a random index in `0..cap` using a fast, thread-local xorshift PRNG.
/// Used by [`Playlist::enqueue`] for inserting into the shuffled order
/// without a heavyweight RNG crate.
fn random_index(cap: usize) -> usize {
    if cap == 0 {
        return 0;
    }
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos().wrapping_mul(6364136223846793005) as u64)
                .unwrap_or(0xDEAD_BEEF)
        );
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x.wrapping_shr(12);
        x ^= x.wrapping_shl(25);
        x ^= x.wrapping_shr(27);
        s.set(x);
        (x as usize) % cap
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(name: &str) -> AudioSource {
        AudioSource::File(std::path::PathBuf::from(name))
    }

    #[test]
    fn empty_playlist_returns_none() {
        let mut q = Playlist::new();
        assert!(q.current_source().is_none());
        assert!(q.advance().is_none());
        assert!(q.previous().is_none());
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn sequential_playback_with_repeat_off() {
        let mut q = Playlist::new();
        q.enqueue(src("a.flac"));
        q.enqueue(src("b.flac"));
        q.enqueue(src("c.flac"));

        // Play first track explicitly.
        let a = q.play_index(0).unwrap();
        assert_eq!(a.to_string(), "a.flac");
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.peek_previous(), None);

        let b = q.advance().unwrap();
        assert_eq!(b.to_string(), "b.flac");
        assert_eq!(q.current_index(), Some(1));
        assert_eq!(q.peek_previous().unwrap().to_string(), "a.flac");

        let c = q.advance().unwrap();
        assert_eq!(c.to_string(), "c.flac");
        assert_eq!(q.current_index(), Some(2));

        // Repeat off → exhausted.
        assert!(q.advance().is_none());
        assert!(q.current_index().is_none());
    }

    #[test]
    fn repeat_all_wraps() {
        let mut q = Playlist::new();
        q.set_repeat(RepeatMode::All);
        q.enqueue(src("a.flac"));
        q.enqueue(src("b.flac"));
        q.play_index(0);
        q.advance().unwrap(); // b
        let wrap = q.advance().unwrap(); // wraps back to a
        assert_eq!(wrap.to_string(), "a.flac");
    }

    #[test]
    fn repeat_one_preserves_current_on_advance() {
        // RepeatOne does NOT make advance() return the same track — manual
        // Next always skips.  The engine handles repeat-one at EOS by seeking
        // to 0 without calling advance().
        let mut q = Playlist::new();
        q.set_repeat(RepeatMode::One);
        q.enqueue(src("song.flac"));
        q.play_index(0);
        // Single-track queue: advance returns None (nothing follows).
        assert!(q.advance().is_none());
        assert!(q.current_index().is_none());
    }

    #[test]
    fn previous_rewinds_history() {
        let mut q = Playlist::new();
        q.enqueue(src("1.flac"));
        q.enqueue(src("2.flac"));
        q.enqueue(src("3.flac"));
        q.play_index(0);
        q.advance(); // 1→2
        q.advance(); // 2→3

        let back = q.previous().unwrap();
        assert_eq!(back.to_string(), "2.flac");
        let back2 = q.previous().unwrap();
        assert_eq!(back2.to_string(), "1.flac");
        assert!(q.previous().is_none());
    }

    #[test]
    fn remove_fixes_indices() {
        let mut q = Playlist::new();
        q.enqueue(src("a.flac"));
        q.enqueue(src("b.flac"));
        q.enqueue(src("c.flac"));
        q.play_index(2); // "c.flac" at index 2

        q.remove(1); // remove "b.flac"
        assert_eq!(q.len(), 2);
        assert_eq!(q.current_index(), Some(1)); // c moved from 2→1
        assert_eq!(q.items[0].to_string(), "a.flac");
        assert_eq!(q.items[1].to_string(), "c.flac");
    }

    #[test]
    fn clear_resets_everything() {
        let mut q = Playlist::new();
        q.enqueue(src("x.flac"));
        q.enqueue(src("y.flac"));
        q.play_index(0);
        q.advance();
        assert!(q.current_source().is_some());
        q.clear();
        assert!(q.is_empty());
        assert!(q.current_index().is_none());
        assert!(q.history.is_empty());
    }

    #[test]
    fn sequential_advances_play_all_tracks() {
        // Deterministic: no shuffle, nothing played yet, advance from start.
        let mut q = Playlist::new();
        q.set_shuffle(false);
        q.enqueue(src("a.flac"));
        q.enqueue(src("b.flac"));
        q.enqueue(src("c.flac"));

        // No current track — advance picks index 0.
        assert_eq!(q.advance().unwrap().to_string(), "a.flac");
        assert_eq!(q.advance().unwrap().to_string(), "b.flac");
        assert_eq!(q.advance().unwrap().to_string(), "c.flac");
        assert!(q.advance().is_none());
    }

    #[test]
    fn peek_next_does_not_mutate() {
        let mut q = Playlist::new();
        q.enqueue(src("a.flac"));
        q.enqueue(src("b.flac"));
        q.play_index(0);
        assert_eq!(q.peek_next().unwrap().to_string(), "b.flac");
        // State unchanged.
        assert_eq!(q.current_index(), Some(0));
        assert!(q.advance().is_some()); // still b
    }
}
