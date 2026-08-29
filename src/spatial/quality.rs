//! Renderer quality tiers (spec §86, §86-89).
//!
//! A quality tier makes the spatial engine scale CPU against perceptual
//! fidelity. Each tier maps onto a set of concrete behaviour knobs derived
//! from the spec's §86 table (spread samples, room reflection order,
//! binaural implementation, HRTF FIR length) so the tiers are
//! *perceptually meaningful* rather than arbitrary feature muting.
//!
//! Tiers are **host-advisory**: no renderer ever removes essential
//! correctness (energy, symmetry, NaN-safety) at any tier — every tier
//! still obeys the panning invariants and the real-time rules. They only
//! choose how *refined* the (already-correct) result is.
//!
//! - `Low`    — minimal spread samples, first-order room, cheapest path.
//! - `Medium` — the current default behaviour (balanced).
//! - `High`   — full spread refinement, second-order room.
//! - `Ultra`  — maximum refinement, longer HRTF convolution, higher voice
//!   budget headroom.
//!
//! [`SpatialQuality`] is serializable so it can live in an
//! [`crate::config`]-like host surface and a scene file.

use serde::{Deserialize, Serialize};

/// Renderer quality tier (spec §86). `Default = Medium` (current behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub enum SpatialQuality {
    Low,
    #[default]
    Medium,
    High,
    Ultra,
}

impl SpatialQuality {
    /// Number of directions sampled for an extended source's angular spread
    /// (the renderers' `spread` ring). Higher = smoother wide images.
    pub fn spread_samples(&self) -> usize {
        match self {
            SpatialQuality::Low => 1,
            SpatialQuality::Medium => 3,
            SpatialQuality::High => 6,
            SpatialQuality::Ultra => 8,
        }
    }

    /// Maximum early-reflection order for the room engine.
    pub fn reflection_order(&self, configured: u8) -> u8 {
        let max = match self {
            SpatialQuality::Low => 1,
            SpatialQuality::Medium => 1,
            SpatialQuality::High => 2,
            SpatialQuality::Ultra => 2,
        };
        configured.clamp(1, max)
    }

    /// HRTF FIR taps (≤ [`crate::spatial::hrtf::MAX_HRTF_TAPS`]).
    pub fn hrtf_taps(&self) -> usize {
        match self {
            SpatialQuality::Low => 32,
            SpatialQuality::Medium => 64,
            SpatialQuality::High => 96,
            SpatialQuality::Ultra => 128,
        }
    }

    /// Whether reflections get spatialized per-image (High+) or coarse.
    pub fn spatialized_reflections(&self) -> bool {
        !matches!(self, SpatialQuality::Low)
    }

    /// A coarse CPU index for telemetry (higher = more expne).
    pub fn cost_index(&self) -> u32 {
        match self {
            SpatialQuality::Low => 1,
            SpatialQuality::Medium => 2,
            SpatialQuality::High => 3,
            SpatialQuality::Ultra => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_monotone_in_refinement() {
        assert!(SpatialQuality::Low < SpatialQuality::Medium);
        assert!(SpatialQuality::Medium < SpatialQuality::High);
        assert!(SpatialQuality::High < SpatialQuality::Ultra);
    }

    #[test]
    fn knobs_monotone() {
        let qs = [
            SpatialQuality::Low,
            SpatialQuality::Medium,
            SpatialQuality::High,
            SpatialQuality::Ultra,
        ];
        for w in qs.windows(2) {
            assert!(w[0].spread_samples() <= w[1].spread_samples());
            assert!(w[0].hrtf_taps() <= w[1].hrtf_taps());
            assert!(w[0].cost_index() < w[1].cost_index());
        }
        assert!(SpatialQuality::Ultra.hrtf_taps() <= 128);
    }

    #[test]
    fn reflection_order_respects_config() {
        let configured = 2u8;
        assert_eq!(SpatialQuality::Low.reflection_order(configured), 1);
        assert_eq!(SpatialQuality::Medium.reflection_order(configured), 1);
        assert_eq!(SpatialQuality::High.reflection_order(configured), 2);
        // Never *raises* beyond the configured order.
        assert_eq!(SpatialQuality::Ultra.reflection_order(1), 1);
    }

    #[test]
    fn default_is_medium() {
        assert_eq!(SpatialQuality::default(), SpatialQuality::Medium);
    }
}
