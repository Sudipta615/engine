//! Musical / tempo-mapped control curves (v3.41).
//!
//! A [`CurveBeats`] is a parameter automation curve **authored in musical
//! time** (beats) and evaluated against a [`TempoMap`] — so the same curve
//! lands on the correct samples as the tempo changes, and a tempo
//! *change* seamlessly stretches the curve under it. This is the
//! tempo-mapped counterpart to `dsp::spatial`'s positional-seconds
//! [`CurveScalar`](crate::spatial::automation::CurveScalar): author the
//! keyframes in beats, carry a tempo map, and the curve becomes sample-
//! accurate playback on the master clock.
//!
//! ```text
//! CurveBeats { (beat, value) … }  ──  sample ──▶ evaluate(sample, &TempoMap)
//!                                         │  TempoMap::beat_at_sample
//!                                         ▼
//!                                  evaluate_beats(beat)  (piecewise-linear,
//!                                                          musical time)
//! ```
//!
//! The Graph 2.0 `OfflineExecutor` consumes these directly: a registered
//! `set_gain_automation(node, curve)` + `set_tempo_map` drives that node's
//! gain with a smooth sample-accurate linear ramp each block, and aelog
//! records both the curve and the tempo map so a musical session replays
//! deterministically. Control/offline-path by design — no allocation or
//! lock on any realtime audio thread.

use super::tempo::TempoMap;
use serde::{Deserialize, Serialize};

/// Piecewise-linear interpolation factor (assumes `[tb > ta]` or 1.0).
#[inline]
fn lerp_factor(t: f64, ta: f64, tb: f64) -> f64 {
    if tb > ta {
        ((t - ta) / (tb - ta)).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// A tempo-mapped scalar control curve: time-ordered keyframes in **beats**,
/// evaluated piecewise-linearly in musical time (held outside the range at
/// the nearest keyframe). Serde: curves are recorded verbatim in aelog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurveBeats {
    /// `(beat, value)` keyframes, time-ordered (kept sorted on insert).
    pts: Vec<(f64, f32)>,
}

impl Default for CurveBeats {
    fn default() -> Self {
        Self::new()
    }
}

impl CurveBeats {
    pub fn new() -> Self {
        Self { pts: Vec::new() }
    }

    /// Build from `(beat, value)` points; `None` if empty.
    pub fn from_points(points: &[(f64, f32)]) -> Option<Self> {
        let mut c = Self::new();
        for &(b, v) in points {
            c.set(b, v);
        }
        if c.pts.is_empty() {
            None
        } else {
            Some(c)
        }
    }

    /// Insert/replace a keyframe at `beat`, keeping points beat-ordered
    /// (an existing keyframe at the same beat is overwritten).
    pub fn set(&mut self, beat: f64, value: f32) {
        let idx = self
            .pts
            .iter()
            .position(|(b, _)| *b >= beat)
            .unwrap_or(self.pts.len());
        if idx < self.pts.len() && (self.pts[idx].0 - beat).abs() < 1e-9 {
            self.pts[idx] = (beat, value);
        } else {
            self.pts.insert(idx, (beat, value));
        }
    }

    /// The time-ordered `(beat, value)` keyframes (for round-tripping /
    /// serde scene models).
    pub fn keyframes(&self) -> &[(f64, f32)] {
        &self.pts
    }

    pub fn is_empty(&self) -> bool {
        self.pts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pts.len()
    }

    /// Evaluate the curve at a **beat position** (piecewise-linear in
    /// musical time; held constant before the first / after the last
    /// keyframe — the `CurveScalar` convention).
    pub fn evaluate_beats(&self, beat: f64) -> f32 {
        let pts = &self.pts;
        if pts.is_empty() {
            return 0.0;
        }
        if beat <= pts[0].0 {
            return pts[0].1;
        }
        let last = pts.len() - 1;
        if beat >= pts[last].0 {
            return pts[last].1;
        }
        for i in 0..last {
            let (ba, va) = pts[i];
            let (bb, vb) = pts[i + 1];
            if beat >= ba && beat <= bb {
                let f = lerp_factor(beat, ba, bb) as f32;
                return va + f * (vb - va);
            }
        }
        pts[last].1
    }

    /// Evaluate the curve at an absolute **master sample** by mapping the
    /// sample back to a beat through `map`, then interpolating musically.
    /// This is the tempo-mapped read: a tempo change just remaps where each
    /// beat lands, and the value follows the musical time grid.
    pub fn evaluate(&self, sample: u64, map: &TempoMap, sample_rate: f32) -> f32 {
        if self.is_empty() {
            return 0.0;
        }
        let beat = map.beat_at_sample(sample as f64, sample_rate);
        self.evaluate_beats(beat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    #[test]
    fn evaluates_piecewise_linear_in_beats() {
        let c = CurveBeats::from_points(&[(0.0, 0.0), (2.0, 1.0), (4.0, 0.0)]).unwrap();
        assert!((c.evaluate_beats(0.0) - 0.0).abs() < 1e-6);
        assert!((c.evaluate_beats(1.0) - 0.5).abs() < 1e-6);
        assert!((c.evaluate_beats(2.0) - 1.0).abs() < 1e-6);
        assert!((c.evaluate_beats(3.0) - 0.5).abs() < 1e-6);
        // Held outside the range.
        assert!((c.evaluate_beats(-1.0) - 0.0).abs() < 1e-6);
        assert!((c.evaluate_beats(9.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn sorts_keyframes_on_insert() {
        let mut c = CurveBeats::new();
        c.set(2.0, 1.0);
        c.set(0.0, 0.0);
        c.set(1.0, 0.5);
        assert_eq!(c.keyframes(), &[(0.0, 0.0), (1.0, 0.5), (2.0, 1.0)]);
    }

    #[test]
    fn tempo_maps_the_curve_to_samples() {
        // 120 BPM → 1 beat = 24000 samples. Curve 0..=1.0 over beat 0..4.
        let mut map = TempoMap::new();
        map.push(0.0, 120.0);
        let c = CurveBeats::from_points(&[(0.0, 0.0), (4.0, 1.0)]).unwrap();
        // sample 0 → beat 0 → 0.0; sample 48000 → beat 2 → 0.5;
        // sample 96000 → beat 4 → 1.0.
        assert!((c.evaluate(0, &map, SR) - 0.0).abs() < 1e-6);
        assert!((c.evaluate(48_000, &map, SR) - 0.5).abs() < 1e-6);
        assert!((c.evaluate(96_000, &map, SR) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_tempo_change_moves_where_beats_land() {
        // Same curve, but a doubling to 240 BPM at beat 2: beat 4 now lands
        // at 48000+24000 = 72000 samples instead of 96000. The value at a
        // *beat* is unchanged — it is the sample mapping that shifts.
        let mut map = TempoMap::new();
        map.push(0.0, 120.0);
        map.push(2.0, 240.0);
        let c = CurveBeats::from_points(&[(0.0, 0.0), (4.0, 1.0)]).unwrap();
        assert!(
            (c.evaluate(48_000, &map, SR) - 0.5).abs() < 1e-6,
            "beat 2 still 0.5"
        );
        assert!(
            (c.evaluate(72_000, &map, SR) - 1.0).abs() < 1e-6,
            "beat 4 at 72000"
        );
        // And the musical value is tempo-independent given the beat.
        assert!((map.beat_at_sample(72_000.0, SR) - 4.0).abs() < 1e-6);
    }
}
