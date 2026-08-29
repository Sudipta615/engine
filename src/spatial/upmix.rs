//! Upmix compatibility modes (spec §87–88).
//!
//! The conventional stereo→surround matrix modes remain useful compatibility
//! features but **must never be described as recovering original spatial
//! objects** (spec §87). [`UpmixMode`] selects a deterministic, inexpensive
//! policy for widening a (typically) stereo bed onto a larger layout:
//!
//! - `SimpleMatrix` — the classic static 5.1/7.1 trims (centre from M − S,
//!   rears from a decorrelated side band). First implementation stays cheap.
//! - `Music` — keeps the image mostly front, light rear fill.
//! - `Cinema` — boosts centre and rears for dialogue/ambience.
//! - `Ambience` — decorrelated diffuse rear extraction (a one-tap allpass
//!   decorrelator so rears read as ambience, not a phantom image).
//!
//! This is a *policy selector* for the engine's conventional
//! [`crate::decode::ChannelLayout`]→layout matrix path. The existing channel
//! mixers already ship the static 5.1/7.1 templates (see
//! `decode::channel_layout` / `channel_mix`); [`UpmixMode`] parameterises
//! their centre/ambience character deterministically.

use serde::{Deserialize, Serialize};

/// Compatibility upmix policy (spec §88). `Default = SimpleMatrix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UpmixMode {
    #[default]
    SimpleMatrix,
    Music,
    Cinema,
    Ambience,
}

/// Level trims applied to the centre / rear buses for a mode (linear).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UpmixTrims {
    pub front_left: f32,
    pub front_right: f32,
    pub center: f32,
    pub side_left: f32,
    pub side_right: f32,
    pub rear_left: f32,
    pub rear_right: f32,
}

impl UpmixMode {
    /// Deterministic centre/rear character for this mode. All coefficients
    /// are fixed scalars (no state); used by the channel-mix path to
    /// parameterise the static templates.
    pub fn trims(&self) -> UpmixTrims {
        match self {
            UpmixMode::SimpleMatrix => UpmixTrims {
                front_left: 1.0,
                front_right: 1.0,
                center: 0.9,
                side_left: 0.6,
                side_right: 0.6,
                rear_left: 0.4,
                rear_right: 0.4,
            },
            UpmixMode::Music => UpmixTrims {
                front_left: 1.0,
                front_right: 1.0,
                center: 1.0,
                side_left: 0.35,
                side_right: 0.35,
                rear_left: 0.2,
                rear_right: 0.2,
            },
            UpmixMode::Cinema => UpmixTrims {
                front_left: 0.85,
                front_right: 0.85,
                center: 1.2,
                side_left: 0.7,
                side_right: 0.7,
                rear_left: 0.7,
                rear_right: 0.7,
            },
            UpmixMode::Ambience => UpmixTrims {
                front_left: 1.0,
                front_right: 1.0,
                center: 0.8,
                side_left: 0.8,
                side_right: 0.8,
                rear_left: 0.8,
                rear_right: 0.8,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_are_positive_and_deterministic() {
        for m in [
            UpmixMode::SimpleMatrix,
            UpmixMode::Music,
            UpmixMode::Cinema,
            UpmixMode::Ambience,
        ] {
            let t = m.trims();
            assert!(t.center > 0.0);
            assert!(t.front_left > 0.0 && t.front_right > 0.0);
            assert!(t.side_left > 0.0 && t.side_right > 0.0);
            assert!(t.rear_left > 0.0 && t.rear_right > 0.0);
            assert_eq!(m.trims(), m.trims()); // deterministic
        }
    }

    #[test]
    fn cinema_boosts_centre_surrounds() {
        let music = UpmixMode::Music.trims();
        let cinema = UpmixMode::Cinema.trims();
        assert!(cinema.center > music.center);
        assert!(cinema.rear_left > music.rear_left);
        assert!(cinema.rear_right > music.rear_right);
    }
}
