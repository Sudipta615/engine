//! Sample-accurate automation curves and interpolation modes (Item 30).
//!
//! Provides sample-accurate interpolation curves supporting:
//! - `Step`: Instantaneous parameter change on keyframe sample.
//! - `Linear`: Constant-slope linear ramp between keyframes.
//! - `Exponential`: Perceptually smooth logarithmic/exponential ramp (ideal for gain/frequency).
//! - `SCurve`: Smoothstep / cubic Hermite ease-in-ease-out curve ($3t^2 - 2t^3$).
//!
//! Hot-path execution (`render_block`) renders directly into caller-provided
//! slices with zero heap allocations and sub-block continuity.

use serde::{Deserialize, Serialize};

/// Interpolation mode between automation keyframes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum InterpolationMode {
    /// Holds the initial value until the next keyframe sample.
    Step,
    /// Linear ramp between previous and next keyframe values.
    #[default]
    Linear,
    /// Exponential curve with natural transition (curved acceleration/deceleration).
    Exponential,
    /// Cubic Hermite ease-in/ease-out S-curve ($3t^2 - 2t^3$).
    SCurve,
}

/// A single sample-addressed automation keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AutomationKeyframe {
    /// Absolute sample timestamp.
    pub sample: u64,
    /// Parameter value at this sample.
    pub value: f32,
    /// Interpolation mode leading towards the next keyframe.
    pub mode: InterpolationMode,
}

/// Sample-accurate automation track.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AutomationTrack {
    keyframes: Vec<AutomationKeyframe>,
}

impl AutomationTrack {
    /// Create a new empty automation track.
    pub fn new() -> Self {
        Self {
            keyframes: Vec::new(),
        }
    }

    /// Add or update a keyframe, maintaining ascending sample order.
    pub fn insert(&mut self, sample: u64, value: f32, mode: InterpolationMode) {
        let kf = AutomationKeyframe {
            sample,
            value,
            mode,
        };
        match self.keyframes.binary_search_by_key(&sample, |k| k.sample) {
            Ok(idx) => self.keyframes[idx] = kf,
            Err(idx) => self.keyframes.insert(idx, kf),
        }
    }

    /// Number of keyframes in the track.
    pub fn len(&self) -> usize {
        self.keyframes.len()
    }

    /// Whether the track is empty.
    pub fn is_empty(&self) -> bool {
        self.keyframes.is_empty()
    }

    /// Clear all keyframes.
    pub fn clear(&mut self) {
        self.keyframes.clear();
    }

    /// Evaluate the parameter value at an absolute sample timestamp.
    pub fn evaluate_at(&self, sample: u64) -> f32 {
        if self.keyframes.is_empty() {
            return 0.0;
        }
        if sample <= self.keyframes[0].sample {
            return self.keyframes[0].value;
        }
        let last = self.keyframes.last().unwrap();
        if sample >= last.sample {
            return last.value;
        }

        let idx = match self.keyframes.binary_search_by_key(&sample, |k| k.sample) {
            Ok(exact) => return self.keyframes[exact].value,
            Err(next_idx) => next_idx - 1,
        };

        let k0 = &self.keyframes[idx];
        let k1 = &self.keyframes[idx + 1];

        let span = (k1.sample - k0.sample) as f64;
        if span <= 0.0 {
            return k1.value;
        }

        let t = ((sample - k0.sample) as f64 / span).clamp(0.0, 1.0) as f32;
        interpolate(k0.value, k1.value, t, k0.mode)
    }

    /// Render sample-accurate parameter values across an audio block directly into `out`.
    /// Guaranteed zero allocations.
    pub fn render_block(&self, start_sample: u64, out: &mut [f32]) {
        if out.is_empty() {
            return;
        }
        if self.keyframes.is_empty() {
            out.fill(0.0);
            return;
        }

        let first_sample = self.keyframes[0].sample;
        let last_sample = self.keyframes.last().unwrap().sample;

        // Before first keyframe
        if start_sample + out.len() as u64 <= first_sample {
            out.fill(self.keyframes[0].value);
            return;
        }
        // After last keyframe
        if start_sample >= last_sample {
            out.fill(self.keyframes.last().unwrap().value);
            return;
        }

        // Per-sample evaluation with continuity across block boundary
        for (i, target) in out.iter_mut().enumerate() {
            let s = start_sample + i as u64;
            *target = self.evaluate_at(s);
        }
    }
}

/// Compute interpolated value at fractional position `t` in [0.0, 1.0].
#[inline]
pub fn interpolate(v0: f32, v1: f32, t: f32, mode: InterpolationMode) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match mode {
        InterpolationMode::Step => {
            if t >= 1.0 {
                v1
            } else {
                v0
            }
        }
        InterpolationMode::Linear => v0 + t * (v1 - v0),
        InterpolationMode::SCurve => {
            // Smoothstep cubic Hermite: 3t^2 - 2t^3
            let s = t * t * (3.0 - 2.0 * t);
            v0 + s * (v1 - v0)
        }
        InterpolationMode::Exponential => {
            // Logarithmic/exponential curve: concave for boost, convex for cut
            if (v1 - v0).abs() < 1e-6 {
                v0
            } else if v0 > 0.0 && v1 > 0.0 {
                // True exponential in positive domain
                v0 * (v1 / v0).powf(t)
            } else {
                // Squared exponential mapping
                let factor = if v1 > v0 {
                    t * t
                } else {
                    1.0 - (1.0 - t) * (1.0 - t)
                };
                v0 + factor * (v1 - v0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_step_interpolation() {
        let mut track = AutomationTrack::new();
        track.insert(100, 0.0, InterpolationMode::Step);
        track.insert(200, 1.0, InterpolationMode::Step);

        assert_eq!(track.evaluate_at(50), 0.0);
        assert_eq!(track.evaluate_at(100), 0.0);
        assert_eq!(track.evaluate_at(150), 0.0);
        assert_eq!(track.evaluate_at(200), 1.0);
        assert_eq!(track.evaluate_at(250), 1.0);
    }

    #[test]
    fn test_linear_interpolation() {
        let mut track = AutomationTrack::new();
        track.insert(100, 0.0, InterpolationMode::Linear);
        track.insert(200, 10.0, InterpolationMode::Linear);

        assert_eq!(track.evaluate_at(100), 0.0);
        assert_eq!(track.evaluate_at(150), 5.0);
        assert_eq!(track.evaluate_at(200), 10.0);
    }

    #[test]
    fn test_scurve_interpolation() {
        let mut track = AutomationTrack::new();
        track.insert(0, 0.0, InterpolationMode::SCurve);
        track.insert(100, 1.0, InterpolationMode::SCurve);

        let mid = track.evaluate_at(50);
        assert!((mid - 0.5).abs() < 1e-4);

        // S-curve derivative is 0 at ends: check quarter and three-quarter
        let q1 = track.evaluate_at(25);
        let q3 = track.evaluate_at(75);
        // At t=0.25, 3*(0.25)^2 - 2*(0.25)^3 = 0.1875 - 0.03125 = 0.15625 < 0.25 (ease in)
        assert!(q1 < 0.25);
        assert!(q3 > 0.75);
    }

    #[test]
    fn test_exponential_interpolation() {
        let mut track = AutomationTrack::new();
        track.insert(0, 1.0, InterpolationMode::Exponential);
        track.insert(100, 16.0, InterpolationMode::Exponential);

        // At t=0.5, sqrt(16) = 4.0
        let mid = track.evaluate_at(50);
        assert!((mid - 4.0).abs() < 1e-4);
    }

    #[test]
    fn test_render_block_continuity() {
        let mut track = AutomationTrack::new();
        track.insert(0, 0.0, InterpolationMode::Linear);
        track.insert(1000, 1000.0, InterpolationMode::Linear);

        let mut buf = [0.0f32; 256];
        track.render_block(100, &mut buf);
        assert_eq!(buf[0], 100.0);
        assert_eq!(buf[255], 355.0);
    }
}
