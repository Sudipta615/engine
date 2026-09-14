//! Unified, sample-accurate monotonic audio clock — the engine's single source of
//! truth for the playhead position and timeline synchronization.

/// Precise sample-domain playback clock tracking source, DSP, output, and
/// compensation timing.
///
/// Position is tracked strictly as an integer frame counter and converted to
/// seconds with a single division per read, preventing floating-point accumulation
/// drift over long playback sessions.
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioClock {
    /// Total source frames consumed from the decoder for the current track.
    /// This is the track playhead; it moves forward or is reset on track boundaries.
    pub source_frames: u64,
    /// Source sample rate (Hz) of the current track.
    pub source_sample_rate: u32,
    /// Total output (DAC-domain) frames rendered through the pipeline for this track.
    pub output_frames: u64,
    /// Output sample rate (Hz) delivered to the DAC/sink.
    pub output_sample_rate: u32,
    /// Pipeline latency in output frames (DSP lookahead, resamplers, device buffers).
    pub latency_frames: u64,
    /// Monotonic total frame counter since engine construction (unbroken across tracks).
    pub monotonic_frames: u64,
}

impl AudioClock {
    pub fn new(source_rate: u32) -> Self {
        Self {
            source_frames: 0,
            source_sample_rate: source_rate.max(1),
            output_frames: 0,
            output_sample_rate: source_rate.max(1),
            latency_frames: 0,
            monotonic_frames: 0,
        }
    }

    /// Advance the playhead by `frames` consumed source frames.
    pub fn advance_source(&mut self, frames: u64) {
        self.source_frames += frames;
    }

    /// Advance the output and monotonic clock by `frames` rendered to the sink.
    pub fn advance_output(&mut self, frames: u64) {
        self.output_frames += frames;
        self.monotonic_frames += frames;
    }

    /// Set the output sample rate in Hz.
    pub fn set_output_sample_rate(&mut self, rate: u32) {
        self.output_sample_rate = rate.max(1);
    }

    /// Set the pipeline latency in output frames for compensated playhead calculations.
    pub fn set_latency_frames(&mut self, frames: u64) {
        self.latency_frames = frames;
    }

    /// Set the playhead directly (seek / stop).
    pub fn set_source_frames(&mut self, frames: u64) {
        self.source_frames = frames;
    }

    /// Reset the track-level counters to the start of a new track.
    /// The monotonic timeline clock is preserved across track boundaries.
    pub fn reset_track(&mut self, source_rate: u32) {
        self.source_frames = 0;
        self.output_frames = 0;
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

    /// Latency-compensated audible position in seconds (clamped at 0).
    pub fn position_secs_compensated(&self) -> f32 {
        let raw = self.position_secs();
        let latency_secs = if self.output_sample_rate == 0 {
            0.0
        } else {
            self.latency_frames as f64 / self.output_sample_rate as f64
        };
        (raw as f64 - latency_secs).max(0.0) as f32
    }

    /// Monotonic elapsed playback time in seconds since engine start.
    pub fn monotonic_secs(&self) -> f64 {
        if self.output_sample_rate == 0 {
            0.0
        } else {
            self.monotonic_frames as f64 / self.output_sample_rate as f64
        }
    }
}
