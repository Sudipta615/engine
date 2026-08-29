//! Spatial parameter automation (spec §47).
//!
//! An object's spatial parameters (position, gain, spread, orientation) can
//! be driven over time rather than set to a static value. This module ships a
//! generic piecewise-linear curve core plus the automation container
//! [`SpatialAutomation`] that the renderers consume.
//!
//! ## Evaluation model
//!
//! Automation is authored in *positional seconds* against the scene wall
//! clock. [`SpatialAutomation`] holds optional [`CurveVec3`]/[`CurveQuat`]/
//! [`CurveScalar`] overrides; the renderer applies them at block rate
//! ([`AutomationMode::BlockRate`]) or sample-accurately
//! ([`AutomationMode::SampleAccurate`], linearly interpolated across the
//! block). The real-time evaluator caches the previous block's per-value
//! state so interpolation ramps continuously across blocks (spec §45–46:
//! smooth, no clicks).

use super::math::{Quat, Vec3};

/// Evaluation cadence for automation (spec §47).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutomationMode {
    /// Evaluate once per block; an object's parameter holds for the block.
    #[default]
    BlockRate,
    /// Evaluate at the block boundary and linear-interpolate across the
    /// block (sample-accurate).
    SampleAccurate,
}

/// The generic curve core: time-ordered keyframes with piecewise-linear
/// evaluation (held outside the range at the nearest keyframe).
#[derive(Debug, Clone, PartialEq)]
struct CurveCore<T> {
    pts: Vec<(f32, T)>,
    end_secs: f32,
}

impl<T: Clone> CurveCore<T> {
    fn new() -> Self {
        Self {
            pts: Vec::new(),
            end_secs: 0.0,
        }
    }

    fn set(&mut self, time: f32, value: T) {
        let idx = self
            .pts
            .iter()
            .position(|(t, _)| *t >= time)
            .unwrap_or(self.pts.len());
        if idx < self.pts.len() && (self.pts[idx].0 - time).abs() < 1e-6 {
            self.pts[idx] = (time, value);
        } else {
            self.pts.insert(idx, (time, value));
        }
        self.end_secs = self.pts.last().map(|p| p.0).unwrap_or(time);
    }
}

/// A scalar (f32) curve over time.
#[derive(Debug, Clone, PartialEq)]
pub struct CurveScalar {
    core: CurveCore<f32>,
}

impl Default for CurveScalar {
    fn default() -> Self {
        Self::new()
    }
}

impl CurveScalar {
    pub fn new() -> Self {
        Self {
            core: CurveCore::new(),
        }
    }
    pub fn from_points(points: &[(f32, f32)]) -> Option<Self> {
        let mut c = Self::new();
        for &(t, v) in points {
            c.set(t, v);
        }
        if c.core.pts.is_empty() {
            return None;
        }
        Some(c)
    }
    #[inline]
    pub fn set(&mut self, time: f32, value: f32) {
        self.core.set(time, value);
    }
    /// The time-ordered keyframes `(seconds, value)`. Exposed so automation can
    /// be round-tripped through a serde scene model (`to_config`).
    pub fn keyframes(&self) -> &[(f32, f32)] {
        &self.core.pts
    }
    pub fn evaluate(&self, t: f32) -> f32 {
        let pts = &self.core.pts;
        if pts.is_empty() {
            return 0.0;
        }
        if t <= pts[0].0 {
            return pts[0].1;
        }
        if t >= self.core.end_secs {
            return pts.last().unwrap().1;
        }
        for i in 0..pts.len() - 1 {
            let (ta, va) = pts[i];
            let (tb, vb) = pts[i + 1];
            if t >= ta && t <= tb {
                let f = if tb > ta { (t - ta) / (tb - ta) } else { 0.0 };
                return va + f * (vb - va);
            }
        }
        pts.last().unwrap().1
    }
}

/// A positional `Vec3` curve.
#[derive(Debug, Clone, PartialEq)]
pub struct CurveVec3 {
    core: CurveCore<Vec3>,
}

impl Default for CurveVec3 {
    fn default() -> Self {
        Self::new()
    }
}

impl CurveVec3 {
    pub fn new() -> Self {
        Self {
            core: CurveCore::new(),
        }
    }
    pub fn from_points(points: &[(f32, Vec3)]) -> Option<Self> {
        let mut c = Self::new();
        for &(t, v) in points {
            c.set(t, v);
        }
        if c.core.pts.is_empty() {
            return None;
        }
        Some(c)
    }
    #[inline]
    pub fn set(&mut self, time: f32, value: Vec3) {
        self.core.set(time, value);
    }
    /// The time-ordered keyframes `(seconds, position)` (round-trip through a
    /// serde scene model, see [`CurveScalar::keyframes`]).
    pub fn keyframes(&self) -> &[(f32, Vec3)] {
        &self.core.pts
    }
    pub fn evaluate(&self, t: f32) -> Vec3 {
        let pts = &self.core.pts;
        if pts.is_empty() {
            return Vec3::ZERO;
        }
        if t <= pts[0].0 {
            return pts[0].1;
        }
        if t >= self.core.end_secs {
            return pts.last().unwrap().1;
        }
        for i in 0..pts.len() - 1 {
            let (ta, va) = pts[i];
            let (tb, vb) = pts[i + 1];
            if t >= ta && t <= tb {
                let f = if tb > ta { (t - ta) / (tb - ta) } else { 0.0 };
                return va + (vb - va) * f;
            }
        }
        pts.last().unwrap().1
    }
}

/// An orientation `Quat` curve (shortest-arc nlerp between keyframes).
#[derive(Debug, Clone, PartialEq)]
pub struct CurveQuat {
    core: CurveCore<Quat>,
}

impl Default for CurveQuat {
    fn default() -> Self {
        Self::new()
    }
}

impl CurveQuat {
    pub fn new() -> Self {
        Self {
            core: CurveCore::new(),
        }
    }
    pub fn from_points(points: &[(f32, Quat)]) -> Option<Self> {
        let mut c = Self::new();
        for &(t, v) in points {
            c.set(t, v);
        }
        if c.core.pts.is_empty() {
            return None;
        }
        Some(c)
    }
    #[inline]
    pub fn set(&mut self, time: f32, value: Quat) {
        self.core.set(time, value);
    }
    /// The time-ordered keyframes `(seconds, orientation)` (round-trip through
    /// a serde scene model, see [`CurveScalar::keyframes`]).
    pub fn keyframes(&self) -> &[(f32, Quat)] {
        &self.core.pts
    }
    pub fn evaluate(&self, t: f32) -> Quat {
        let pts = &self.core.pts;
        if pts.is_empty() {
            return Quat::IDENTITY;
        }
        if t <= pts[0].0 {
            return pts[0].1;
        }
        if t >= self.core.end_secs {
            return pts.last().unwrap().1;
        }
        for i in 0..pts.len() - 1 {
            let (ta, qa) = pts[i];
            let (tb, qb) = pts[i + 1];
            if t >= ta && t <= tb {
                let f = if tb > ta { (t - ta) / (tb - ta) } else { 0.0 };
                return qa.nlerp(qb, f);
            }
        }
        pts.last().unwrap().1
    }
}

/// Optional automation override for one object. A curve in `Some` drives the
/// object's parameter when automation is active.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SpatialAutomation {
    pub position: Option<CurveVec3>,
    pub orientation: Option<CurveQuat>,
    pub gain: Option<CurveScalar>,
    pub spread: Option<CurveScalar>,
    pub sample_rate: f32,
}

impl SpatialAutomation {
    pub fn has_any(&self) -> bool {
        self.position.is_some()
            || self.orientation.is_some()
            || self.gain.is_some()
            || self.spread.is_some()
    }

    /// Apply the automation at time `t` (seconds) by over-writing `out`.
    /// Only curves present are applied; others are left untouched.
    pub fn apply(&self, t: f32, out: &mut SpatialAudioAutomationFrame) {
        if let Some(c) = &self.position {
            out.position = Some(c.evaluate(t));
        }
        if let Some(c) = &self.orientation {
            out.orientation = Some(c.evaluate(t));
        }
        if let Some(c) = &self.gain {
            out.gain = Some(c.evaluate(t));
        }
        if let Some(c) = &self.spread {
            out.spread = Some(c.evaluate(t));
        }
    }
}

/// One evaluated automation frame for an object at a given time.
#[derive(Debug, Clone, Default)]
pub struct SpatialAudioAutomationFrame {
    pub position: Option<Vec3>,
    pub orientation: Option<Quat>,
    pub gain: Option<f32>,
    pub spread: Option<f32>,
}

impl SpatialAudioAutomationFrame {
    pub fn none() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::math::Quat as Q;

    #[test]
    fn scalar_interpolates_and_holds() {
        let c = CurveScalar::from_points(&[(0.0, 1.0), (1.0, 3.0)]).unwrap();
        assert!((c.evaluate(0.0) - 1.0).abs() < 1e-6);
        assert!((c.evaluate(1.0) - 3.0).abs() < 1e-6);
        assert!((c.evaluate(0.5) - 2.0).abs() < 1e-6);
        // Before first and after last → clamped.
        assert!((c.evaluate(-5.0) - 1.0).abs() < 1e-6);
        assert!((c.evaluate(2.0) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn scalar_set_keeps_sorted_order() {
        let mut c = CurveScalar::new();
        c.set(1.0, 100.0);
        c.set(0.0, 50.0);
        c.set(0.5, 75.0);
        assert!((c.evaluate(0.0) - 50.0).abs() < 1e-6);
        assert!((c.evaluate(0.5) - 75.0).abs() < 1e-6);
        assert!((c.evaluate(1.0) - 100.0).abs() < 1e-6);
    }

    #[test]
    fn vec3_interpolates_componentwise() {
        let c =
            CurveVec3::from_points(&[(0.0, Vec3::ZERO), (2.0, Vec3::new(2.0, 4.0, 6.0))]).unwrap();
        let mid = c.evaluate(1.0);
        assert!((mid - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-5);
    }

    #[test]
    fn quat_uses_shortest_arc() {
        let q1 = Q::from_euler_rad(1.0, 0.0, 0.0); // ≈ 57.3° yaw
        let c = CurveQuat::from_points(&[(0.0, Q::IDENTITY), (1.0, q1)]).unwrap();
        let mid = c.evaluate(0.5);
        let end_deg = q1.angle_to(Q::IDENTITY).to_degrees();
        let mid_deg = mid.angle_to(Q::IDENTITY).to_degrees();
        // The midpoint sits halfway on the shortest arc.
        assert!(
            (mid_deg - end_deg / 2.0).abs() < 1.0,
            "{mid_deg} vs {}",
            end_deg / 2.0
        );
        assert!((mid.length() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn curve_holds_last_keyframe_past_the_end() {
        let c = CurveScalar::from_points(&[(0.0, 2.0), (0.5, 5.0)]).unwrap();
        assert!((c.evaluate(3.0) - 5.0).abs() < 1e-6);
    }

    #[test]
    fn automation_apply_sets_only_present_curves() {
        let a = SpatialAutomation {
            gain: CurveScalar::from_points(&[(0.0, 0.7)]),
            position: CurveVec3::from_points(&[(0.0, Vec3::Y)]),
            ..Default::default()
        };
        let mut f = SpatialAudioAutomationFrame::default();
        a.apply(0.0, &mut f);
        assert!(f.gain.is_some() && (f.gain.unwrap() - 0.7).abs() < 1e-6);
        assert!(f.position.is_some());
        assert!(f.orientation.is_none() && f.spread.is_none());
    }
}
