//! Seamless graph transitions and crossfading engine (§5.3, Item 19).
//!
//! Eliminates clicks, zipper noise, and signal discontinuities when graph topology,
//! DSP filter states, HRTF profiles, room correction IRs, or plugin parameters change.
//!
//! # Mechanisms
//! 1. **Dual-Path Equal-Power Crossfader** ([`TransitionCrossfader`]):
//!    Ramps between outgoing and incoming audio streams over a configurable transition window,
//!    preserving total acoustic power without level dips ($g_{\text{old}}^2 + g_{\text{new}}^2 = 1.0$).
//! 2. **Continuous Parameter Smoothing** ([`ContinuousParameterSmoother`]):
//!    Exponential and linear sample-accurate coefficient interpolation for EQ and dynamics.
//! 3. **Discontinuity Verification**:
//!    Numerical analysis helpers detecting click transients and derivative jumps.

use serde::{Deserialize, Serialize};

/// Curve geometry for seamless graph transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionCurve {
    /// Equal-power trigonometric curve: $\cos(t \cdot \pi / 2)$ and $\sin(t \cdot \pi / 2)$.
    /// Preserves total acoustic energy and avoids center-point dip.
    #[default]
    EqualPower,
    /// Linear crossfade: $(1 - t)$ and $t$. Best for highly correlated or mono signals.
    Linear,
    /// Cubic Hermite ease-in/ease-out S-curve ($3t^2 - 2t^3$).
    SCurve,
}

/// Seamless graph transition controller (§5.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionConfig {
    /// Transition window duration in milliseconds (typically 5.0 ms .. 50.0 ms).
    pub duration_ms: f32,
    /// Crossfade curve geometry.
    pub curve: TransitionCurve,
}

impl Default for TransitionConfig {
    fn default() -> Self {
        Self {
            duration_ms: 10.0,
            curve: TransitionCurve::EqualPower,
        }
    }
}

/// Active crossfader blending audio between two graph states across a transition window.
#[derive(Debug, Clone)]
pub struct TransitionCrossfader {
    config: TransitionConfig,
    total_frames: usize,
    frames_remaining: usize,
    active: bool,
}

impl TransitionCrossfader {
    /// Create a crossfader with given configuration.
    pub fn new(config: TransitionConfig) -> Self {
        Self {
            config,
            total_frames: 0,
            frames_remaining: 0,
            active: false,
        }
    }

    /// Whether a transition crossfade is currently active.
    pub fn is_active(&self) -> bool {
        self.active && self.frames_remaining > 0
    }

    /// Start a seamless transition at the specified sample rate.
    pub fn trigger(&mut self, sample_rate: f32) {
        let rate = sample_rate.max(1.0);
        let frames = ((self.config.duration_ms / 1000.0) * rate).round() as usize;
        self.total_frames = frames.max(1);
        self.frames_remaining = self.total_frames;
        self.active = true;
    }

    /// Complete or abort the transition immediately.
    pub fn cancel(&mut self) {
        self.active = false;
        self.frames_remaining = 0;
        self.total_frames = 0;
    }

    /// Crossfade between outgoing audio `old_planes` and incoming audio `new_planes` in place.
    /// Writes blended output directly into `dst_planes`.
    pub fn crossfade_block(
        &mut self,
        old_planes: &[&[f32]],
        new_planes: &[&[f32]],
        dst_planes: &mut [&mut [f32]],
    ) -> usize {
        if !self.is_active() || old_planes.is_empty() || new_planes.is_empty() || dst_planes.is_empty() {
            // Passthrough new planes if inactive
            let channels = dst_planes.len().min(new_planes.len());
            for ch in 0..channels {
                let frames = dst_planes[ch].len().min(new_planes[ch].len());
                dst_planes[ch][..frames].copy_from_slice(&new_planes[ch][..frames]);
            }
            return 0;
        }

        let channels = dst_planes.len().min(old_planes.len()).min(new_planes.len());
        let frames = dst_planes[0]
            .len()
            .min(old_planes[0].len())
            .min(new_planes[0].len());

        let total = self.total_frames as f32;
        let start_remaining = self.frames_remaining;

        for n in 0..frames {
            if self.frames_remaining == 0 {
                // Crossfade complete: remainder of the block takes new signal directly
                for ch in 0..channels {
                    dst_planes[ch][n] = new_planes[ch][n];
                }
                continue;
            }

            let progress = 1.0 - (self.frames_remaining as f32 / total);
            let t = progress.clamp(0.0, 1.0);

            let (g_old, g_new) = match self.config.curve {
                TransitionCurve::EqualPower => {
                    let angle = t * std::f32::consts::FRAC_PI_2;
                    (angle.cos(), angle.sin())
                }
                TransitionCurve::Linear => (1.0 - t, t),
                TransitionCurve::SCurve => {
                    let s = 3.0 * t * t - 2.0 * t * t * t;
                    (1.0 - s, s)
                }
            };

            for ch in 0..channels {
                let sample_old = old_planes[ch][n];
                let sample_new = new_planes[ch][n];
                dst_planes[ch][n] = sample_old * g_old + sample_new * g_new;
            }

            self.frames_remaining -= 1;
        }

        if self.frames_remaining == 0 {
            self.active = false;
        }

        start_remaining - self.frames_remaining
    }

    /// Stereo blend variant used by [`DspGraph::process_block`] where `old`, `new`,
    /// and `dst` all reside in **separate, caller-owned slices** — avoiding any aliasing
    /// borrow conflict that [`Self::crossfade_block`] would introduce when old/new
    /// live in scratch and dst is the caller's output buffer.
    ///
    /// Reads `old_l`/`old_r` (pre-swap audio) and `new_l`/`new_r` (post-swap audio),
    /// writes the blend result into `dst_l`/`dst_r`. Advances the fader state identically
    /// to [`Self::crossfade_block`], so callers can use whichever API fits their borrow
    /// layout.
    pub fn blend_stereo_into(
        &mut self,
        old_l: &[f32],
        old_r: &[f32],
        new_l: &[f32],
        new_r: &[f32],
        dst_l: &mut [f32],
        dst_r: &mut [f32],
    ) {
        if !self.is_active() {
            let n = dst_l.len().min(new_l.len());
            dst_l[..n].copy_from_slice(&new_l[..n]);
            let n = dst_r.len().min(new_r.len());
            dst_r[..n].copy_from_slice(&new_r[..n]);
            return;
        }
        let n = dst_l
            .len()
            .min(old_l.len())
            .min(new_l.len())
            .min(dst_r.len())
            .min(old_r.len())
            .min(new_r.len());
        let total = self.total_frames as f32;
        for i in 0..n {
            if self.frames_remaining == 0 {
                dst_l[i] = new_l[i];
                dst_r[i] = new_r[i];
                continue;
            }
            let progress = 1.0 - (self.frames_remaining as f32 / total);
            let t = progress.clamp(0.0, 1.0);
            let (g_old, g_new) = match self.config.curve {
                TransitionCurve::EqualPower => {
                    let angle = t * std::f32::consts::FRAC_PI_2;
                    (angle.cos(), angle.sin())
                }
                TransitionCurve::Linear => (1.0 - t, t),
                TransitionCurve::SCurve => {
                    let s = 3.0 * t * t - 2.0 * t * t * t;
                    (1.0 - s, s)
                }
            };
            dst_l[i] = old_l[i] * g_old + new_l[i] * g_new;
            dst_r[i] = old_r[i] * g_old + new_r[i] * g_new;
            self.frames_remaining -= 1;
        }
        if self.frames_remaining == 0 {
            self.active = false;
        }
    }
}

/// Continuous one-pole parameter smoother for DSP filter and EQ updates.
#[derive(Debug, Clone)]
pub struct ContinuousParameterSmoother {
    current: f32,
    target: f32,
    alpha: f32,
}

impl ContinuousParameterSmoother {
    pub fn new(initial: f32, tau_ms: f32, sample_rate: f32) -> Self {
        let alpha = if tau_ms <= 0.0 || sample_rate <= 0.0 {
            1.0
        } else {
            let tau_s = tau_ms / 1000.0;
            1.0 - (-1.0 / (tau_s * sample_rate)).exp()
        };

        Self {
            current: initial,
            target: initial,
            alpha: alpha.clamp(0.0001, 1.0),
        }
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    #[inline(always)]
    pub fn next_value(&mut self) -> f32 {
        self.current += self.alpha * (self.target - self.current);
        if (self.target - self.current).abs() < 0.05 {
            self.current = self.target;
        }
        self.current
    }

    pub fn current_value(&self) -> f32 {
        self.current
    }

    pub fn is_settled(&self) -> bool {
        (self.target - self.current).abs() < 0.05
    }
}

/// Measure maximum first-difference $|\Delta x[n]| = |x[n] - x[n-1]|$ in a signal buffer.
/// Used to verify that transitions produce no step discontinuities (clicks).
pub fn measure_max_discontinuity(samples: &[f32]) -> f32 {
    if samples.len() < 2 {
        return 0.0;
    }
    let mut max_delta = 0.0f32;
    for i in 1..samples.len() {
        let delta = (samples[i] - samples[i - 1]).abs();
        if delta > max_delta {
            max_delta = delta;
        }
    }
    max_delta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_power_crossfade_preserves_acoustic_power() {
        let mut fader = TransitionCrossfader::new(TransitionConfig {
            duration_ms: 10.0,
            curve: TransitionCurve::EqualPower,
        });

        // 48 kHz, 10 ms = 480 frames
        fader.trigger(48000.0);
        assert!(fader.is_active());

        // Stream A: constant 1.0 (uncorrelated source)
        let old_buf = vec![1.0f32; 480];
        // Stream B: constant 1.0
        let new_buf = vec![1.0f32; 480];
        let mut out_buf = vec![0.0f32; 480];

        let old_planes = [&old_buf[..]];
        let new_planes = [&new_buf[..]];
        let mut out_planes = [&mut out_buf[..]];

        fader.crossfade_block(&old_planes, &new_planes, &mut out_planes);

        assert!(!fader.is_active());

        // Check midpoint: at t=0.5, cos(pi/4) = sin(pi/4) = 1/sqrt(2) approx 0.7071
        // sum = 0.7071 + 0.7071 = 1.414 (correlated sum)
        // Power = g_old^2 + g_new^2 = 0.5 + 0.5 = 1.0 (exact power preservation!)
        let mid = out_buf[240];
        let angle = 0.5 * std::f32::consts::FRAC_PI_2;
        let expected_power = angle.cos().powi(2) + angle.sin().powi(2);
        assert!((expected_power - 1.0).abs() < 1e-5);
        assert!((mid - (angle.cos() + angle.sin())).abs() < 1e-4);
    }

    #[test]
    fn transition_prevents_step_discontinuities() {
        // Sudden switch from -1.0 to +1.0 (hard jump of 2.0 without crossfade)
        let old_buf = vec![-1.0f32; 200];
        let new_buf = vec![1.0f32; 200];
        let mut out_buf = vec![0.0f32; 200];

        let mut fader = TransitionCrossfader::new(TransitionConfig {
            duration_ms: 2.0, // 2ms = 96 frames at 48k
            curve: TransitionCurve::Linear,
        });
        fader.trigger(48000.0);

        fader.crossfade_block(&[&old_buf[..]], &[&new_buf[..]], &mut [&mut out_buf[..]]);

        // First sample starts at -1.0, smoothly transitions to +1.0
        let max_step = measure_max_discontinuity(&out_buf);
        // Step size per frame should be around 2.0 / 96 approx 0.021, NOT 2.0!
        assert!(max_step < 0.05, "maximum delta {max_step} should be smooth without click");
    }

    #[test]
    fn continuous_parameter_smoother_settles() {
        let mut smoother = ContinuousParameterSmoother::new(100.0, 10.0, 48000.0);
        smoother.set_target(1000.0);

        for _ in 0..10000 {
            smoother.next_value();
        }

        assert!(smoother.is_settled());
        assert!((smoother.current_value() - 1000.0).abs() < 0.01);
    }
}
