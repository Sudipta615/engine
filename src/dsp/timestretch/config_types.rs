//! Configuration types for time-stretching and pitch-shifting.

use config::TimeStretchQuality;

/// Operating mode for the time-stretcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeStretchMode {
    /// Pure WSOLA (Waveform Similarity Overlap-Add) time-domain synthesis.
    /// Optimal for monophonic voice, low latency, and transient clarity.
    #[default]
    Wsola,
    /// Phase-locked Phase Vocoder (Laroche-Dolson) frequency-domain synthesis.
    /// Optimal for complex polyphonic music, sustained chords, and wide pitch shifts.
    PhaseVocoder,
    /// Adaptive hybrid mode: automatically engages WSOLA during percussive transients
    /// and Phase Vocoder during sustained polyphonic passages.
    Hybrid,
}

/// Default synthesis frame window size in samples (approx 20-30 ms at 44.1/48 kHz).
pub const DEFAULT_WSOLA_WINDOW_SIZE: usize = 1024;
/// Default synthesis hop size (75% overlap).
pub const DEFAULT_WSOLA_HOP_SIZE: usize = 256;
/// Search range for waveform similarity alignment (in samples).
pub const DEFAULT_WSOLA_SEARCH_RANGE: usize = 128;

/// Operating configuration for time-stretching and pitch-shifting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeStretchConfig {
    /// Playback speed multiplier (0.25 to 4.0, default 1.0).
    pub speed: f32,
    /// Pitch shift in semitones (-24.0 to +24.0, default 0.0).
    pub pitch_semitones: f32,
    /// Quality tier (spec §22). Maps to the three parameters below via
    /// [`TimeStretchQuality::params`].
    pub quality: TimeStretchQuality,
    /// Window size in samples.
    pub window_size: usize,
    /// Synthesis hop size in samples.
    pub hop_size: usize,
    /// Search delta range in samples.
    pub search_range: usize,
    /// Processing mode (WSOLA, PhaseVocoder, or Hybrid).
    pub mode: TimeStretchMode,
    /// Formant preservation enabled flag.
    pub formant_preservation: bool,
}

impl Default for TimeStretchConfig {
    fn default() -> Self {
        Self::for_quality(TimeStretchQuality::Balanced)
    }
}

impl TimeStretchConfig {
    /// Build a config from a quality tier, resolving its concrete WSOLA
    /// parameters. Speed/pitch default to unity (stretcher inactive).
    pub fn for_quality(quality: TimeStretchQuality) -> Self {
        let (window_size, hop_size, search_range) = quality.params();
        Self {
            speed: 1.0,
            pitch_semitones: 0.0,
            quality,
            window_size,
            hop_size,
            search_range,
            mode: TimeStretchMode::Wsola,
            formant_preservation: false,
        }
    }

    /// Apply a quality tier to an existing config (updates the three
    /// parameters in place).
    pub fn set_quality(&mut self, quality: TimeStretchQuality) {
        self.quality = quality;
        let (window_size, hop_size, search_range) = quality.params();
        self.window_size = window_size;
        self.hop_size = hop_size;
        self.search_range = search_range;
    }
}
