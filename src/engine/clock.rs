//! Sample-accurate integer playback clock — the engine's single source of
//! truth for the playhead position.

/// Precise sample-domain playback clock — the engine's single source of
/// truth for the playhead position.
///
/// Position is tracked strictly as an integer source-frame counter and
/// converted to seconds with a single division per read, so there is no
/// floating-point accumulation drift over long playback sessions.
///
/// # Speed semantics
///
/// The clock does not store a speed value because speed is already embedded
/// in the frame counter: source frames are consumed at `source_rate * speed`
/// frames per wall-clock second, so [`AudioClock::position_secs`]
/// (`source_frames / source_sample_rate`) reports the position *within the
/// current track* regardless of speed — the same semantics in single-track
/// playback and during crossfade transitions.
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioClock {
    /// Total source frames consumed from the decoder for the current track.
    /// This is the playhead; it only ever moves forward (or is reset by
    /// [`AudioClock::reset_track`] / [`AudioClock::set_source_frames`]).
    pub source_frames: u64,
    /// Source sample rate (Hz) of the current track.
    pub source_sample_rate: u32,
}

impl AudioClock {
    pub fn new(source_rate: u32) -> Self {
        Self {
            source_frames: 0,
            source_sample_rate: source_rate.max(1),
        }
    }

    /// Advance the playhead by `frames` consumed source frames.
    pub fn advance_source(&mut self, frames: u64) {
        self.source_frames += frames;
    }

    /// Set the playhead directly (seek / stop).
    pub fn set_source_frames(&mut self, frames: u64) {
        self.source_frames = frames;
    }

    /// Reset the playhead to the start of a new track.
    pub fn reset_track(&mut self, source_rate: u32) {
        self.source_frames = 0;
        self.source_sample_rate = source_rate.max(1);
    }

    /// Exact position in seconds computed directly from the integer source
    /// frame count. Eliminates floating-point accumulation error.
    pub fn position_secs(&self) -> f32 {
        if self.source_sample_rate == 0 {
            0.0
        } else {
            (self.source_frames as f64 / self.source_sample_rate as f64) as f32
        }
    }
}
