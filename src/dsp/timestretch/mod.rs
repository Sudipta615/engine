//! Real-time hybrid time-stretching and pitch-shifting engine.
//!
//! Combines:
//! - WSOLA (Waveform Similarity Overlap-Add) for monophonic signals, speech, and transient clarity
//! - Phase-Locked Phase Vocoder (Laroche-Dolson) for complex polyphonic music and wide transposition
//! - Transient-aware adaptive hybrid routing preventing attack smearing
//! - Polyphase windowed-sinc reconstruction for transparent resampling
//! - Formant preservation option

mod config_types;
mod phase_vocoder;
mod stretcher;
mod transient;

#[cfg(test)]
mod tests;

pub use config_types::{
    TimeStretchConfig, TimeStretchMode, DEFAULT_WSOLA_HOP_SIZE, DEFAULT_WSOLA_SEARCH_RANGE,
    DEFAULT_WSOLA_WINDOW_SIZE,
};
pub use phase_vocoder::PhaseVocoder;
pub use stretcher::TimeStretcher;
pub use transient::TransientDetector;
