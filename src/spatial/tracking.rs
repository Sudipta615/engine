//! Head tracking — the VR/AR orientation seam (spec §48, §136; roadmap
//! Phase 15).
//!
//! The scene's listener already owns a world-space orientation
//! ([`crate::spatial::scene::Listener::orientation`]) and every renderer
//! applies it per block, so *head tracking is purely a control-side
//! problem*: turn a stream of raw orientation samples (an IMU, a webcam, a
//! game engine's VR rig — anything that can produce a timestamped
//! [`Quat`]) into a smooth, current head orientation that the host applies
//! to the listener before each render block. The audio thread never
//! touches the tracker.
//!
//! ## The pipeline (all host-thread)
//!
//! ```text
//! IMU / VR rig ── HeadSample(time, quat) ──> HeadTracker ── sample(now)
//!        ──> Listener::set_orientation ──> scene ──> renderers (unchanged)
//! ```
//!
//! - **Interpolation** — between the two most recent samples the tracker
//!   returns the shortest-path nlerp (a pure yaw sweep between samples
//!   glides, never snaps). When no new sample has arrived since the last
//!   query, the latest sample is held.
//! - **Smoothing** — a one-pole (exponential) filter on the orientation
//!   *error* toward the interpolated target, with a configurable time
//!   constant: raw samples are jittery, and the listener must not zipper.
//!   `smoothing_ms = 0` snaps exactly (used by tests and by hosts that
//!   pre-smooth upstream).
//! - **Rate limiting** (optional) — `max_angular_rate_deg_s` clamps the
//!   angular step per sample, so a violent head jump (or a sensor glitch)
//!   cannot fling the soundfield; the tracker then catches up at the
//!   configured maximum. `0` disables the limit.
//!
//! ## Realtime discipline
//!
//! `HeadTracker` holds a handful of scalars and quaternions — `push` and
//! `sample` allocate nothing and take no locks, so the host can even run
//! them on the audio thread's caller (the renderers themselves stay
//! untouched and lock-free). Deterministic: the same sample stream produces
//! bit-identical orientations (verified by the acceptance suite).

use super::math::Quat;
use super::scene::Listener;

/// Smoothing / limiting policy for a [`HeadTracker`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackingConfig {
    /// One-pole smoothing time constant (ms) applied to the orientation
    /// error at each `sample` call. `0` = exact (no smoothing).
    pub smoothing_ms: f32,
    /// Maximum angular rate the head may appear to move, in degrees per
    /// second. A single sample step larger than `rate × dt` is clamped.
    /// `0` = unlimited.
    pub max_angular_rate_deg_s: f32,
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            smoothing_ms: 12.0,
            max_angular_rate_deg_s: 0.0,
        }
    }
}

/// One timestamped orientation from a head-tracking source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeadSample {
    /// Sample time in seconds (the host's clock domain; must be
    /// non-decreasing across `push` calls).
    pub time: f64,
    /// The head's world-space orientation at `time`.
    pub orientation: Quat,
}

impl HeadSample {
    pub fn new(time: f64, orientation: Quat) -> Self {
        Self { time, orientation }
    }
}

/// A control-side head tracker: interpolates and smooths a stream of
/// [`HeadSample`]s into the current listener orientation (spec §48).
/// Allocation-free and lock-free; the host calls [`HeadTracker::sample`]
/// (or [`HeadTracker::apply_to`]) once per render block and the audio
/// thread only ever reads the listener it writes.
#[derive(Debug, Clone)]
pub struct HeadTracker {
    config: TrackingConfig,
    /// Previous sample (the interpolation segment's start).
    prev_time: f64,
    prev_quat: Quat,
    /// Latest sample (the segment's end / held target).
    latest_time: f64,
    latest_quat: Quat,
    /// Last emitted (smoothed) orientation + the time it was computed at.
    smoothed: Quat,
    smoothed_time: f64,
    has_samples: bool,
}

impl Default for HeadTracker {
    fn default() -> Self {
        Self::new(TrackingConfig::default())
    }
}

impl HeadTracker {
    pub fn new(config: TrackingConfig) -> Self {
        Self {
            config,
            prev_time: 0.0,
            prev_quat: Quat::IDENTITY,
            latest_time: 0.0,
            latest_quat: Quat::IDENTITY,
            smoothed: Quat::IDENTITY,
            smoothed_time: 0.0,
            has_samples: false,
        }
    }

    /// Ingest a new orientation sample (host thread; typically the IMU /
    /// VR callback). Times must be non-decreasing; the segment between the
    /// previous and this sample is what `sample` interpolates across.
    pub fn push(&mut self, sample: HeadSample) -> &mut Self {
        if self.has_samples {
            self.prev_time = self.latest_time;
            self.prev_quat = self.latest_quat;
        }
        self.latest_time = sample.time;
        self.latest_quat = sample.orientation;
        self.has_samples = true;
        self
    }

    /// Replace all state with a single sample (resets interpolation and
    /// smoothing to the given orientation at the given time).
    pub fn reset(&mut self, sample: HeadSample) -> &mut Self {
        self.prev_time = sample.time;
        self.prev_quat = sample.orientation;
        self.latest_time = sample.time;
        self.latest_quat = sample.orientation;
        self.smoothed = sample.orientation;
        self.smoothed_time = sample.time;
        self.has_samples = true;
        self
    }

    /// The tracker's configuration.
    pub fn config(&self) -> TrackingConfig {
        self.config
    }

    /// The last orientation this tracker emitted (identity before any
    /// sample).
    pub fn current(&self) -> Quat {
        self.smoothed
    }

    /// Whether at least one sample has been ingested.
    pub fn has_samples(&self) -> bool {
        self.has_samples
    }

    /// The one-pole factor for this sample step: `1 − exp(−dt/τ)`, `1.0`
    /// when smoothing is disabled or `dt` is zero (first call snaps).
    fn alpha(&self, dt: f32) -> f32 {
        if self.config.smoothing_ms <= 0.0 || dt <= 0.0 {
            1.0
        } else {
            1.0 - (-dt / (self.config.smoothing_ms / 1000.0)).exp()
        }
    }

    /// The interpolated (pre-smoothing) target at `time`: nlerp across the
    /// last sample segment, or the latest sample when `time` is past it.
    fn target(&self, time: f64) -> Quat {
        if !self.has_samples {
            return Quat::IDENTITY;
        }
        if time >= self.latest_time || self.prev_time == self.latest_time {
            self.latest_quat
        } else if time <= self.prev_time {
            self.prev_quat
        } else {
            let f = ((time - self.prev_time) / (self.latest_time - self.prev_time)) as f32;
            self.prev_quat.nlerp(self.latest_quat, f)
        }
    }

    /// Sample the smoothed head orientation at `time` (host thread, once
    /// per render block). Advances the one-pole smoothing toward the
    /// interpolated target and applies the optional rate limit. The first
    /// call snaps to the target (no easing from identity).
    pub fn sample(&mut self, time: f64) -> Quat {
        let dt = (time - self.smoothed_time).max(0.0) as f32;
        self.smoothed_time = time.max(self.smoothed_time);
        let target = self.target(time);
        let alpha = self.alpha(dt);
        let mut next = self.smoothed.nlerp(target, alpha);

        if self.config.max_angular_rate_deg_s > 0.0 && dt > 0.0 {
            let max_step = self.config.max_angular_rate_deg_s.to_radians() * dt;
            let angle = self.smoothed.angle_to(next);
            if angle > max_step && angle > 1e-9 {
                next = self.smoothed.nlerp(next, (max_step / angle).min(1.0));
            }
        }

        self.smoothed = next;
        next
    }

    /// Convenience: sample and write the result straight onto a listener
    /// (the host's per-block loop).
    pub fn apply_to(&mut self, listener: &mut Listener, time: f64) -> Quat {
        let q = self.sample(time);
        listener.set_orientation(q);
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::math::Quat as Q;

    const EPS: f32 = 1e-3;

    fn yaw(deg: f32) -> Quat {
        Q::from_euler_rad(deg.to_radians(), 0.0, 0.0)
    }

    fn deg_between(a: Quat, b: Quat) -> f32 {
        a.angle_to(b).to_degrees()
    }

    #[test]
    fn sample_interpolates_across_the_segment() {
        // Two samples 0°→90° yaw over 100 ms; sampling at the midpoint must
        // give ≈ 45°, and past the latest sample the latest is held.
        let mut t = HeadTracker::new(TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 0.0,
        });
        t.push(HeadSample::new(0.0, yaw(0.0)));
        t.push(HeadSample::new(0.1, yaw(90.0)));
        let mid = t.sample(0.05);
        assert!(
            (deg_between(mid, Q::IDENTITY) - 45.0).abs() < EPS,
            "mid {mid:?}"
        );
        let late = t.sample(0.5);
        assert!(deg_between(late, yaw(90.0)) < EPS, "holds latest");
        // Deterministic: a fresh tracker reproduces the same stream.
        let mut t2 = HeadTracker::new(TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 0.0,
        });
        t2.push(HeadSample::new(0.0, yaw(0.0)));
        t2.push(HeadSample::new(0.1, yaw(90.0)));
        assert_eq!(t2.sample(0.05), mid);
    }

    #[test]
    fn interpolation_takes_the_shortest_arc() {
        // 350° → 10° interpolates through 0° (short way), never through
        // 180°.
        let mut t = HeadTracker::new(TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 0.0,
        });
        t.push(HeadSample::new(0.0, yaw(350.0)));
        t.push(HeadSample::new(0.1, yaw(10.0)));
        let mid = t.sample(0.05);
        assert!(
            deg_between(mid, Q::IDENTITY) < 1.0,
            "short way through 0°: {}",
            deg_between(mid, Q::IDENTITY)
        );
    }

    #[test]
    fn smoothing_ramps_a_jump_and_converges() {
        // A 90° jump with τ = 20 ms sampled at 100 Hz: every step is a
        // bounded fraction of the remaining error (no zipper), and the
        // tracker converges to the target exponentially.
        let mut t = HeadTracker::new(TrackingConfig {
            smoothing_ms: 20.0,
            max_angular_rate_deg_s: 0.0,
        });
        t.push(HeadSample::new(0.0, yaw(0.0)));
        t.push(HeadSample::new(0.01, yaw(90.0)));
        let mut prev = t.sample(0.0);
        let mut max_step = 0.0f32;
        for k in 1..=60 {
            let q = t.sample(0.01 * k as f64);
            let step = deg_between(prev, q);
            max_step = max_step.max(step);
            assert!(
                step < 90.0,
                "no single-block 90° jump (step {step}° at {k})"
            );
            prev = q;
        }
        assert!(max_step < 45.0, "first step bounded ({max_step}°)");
        assert!(
            deg_between(prev, yaw(90.0)) < 1.0,
            "converged after 60 samples: {}",
            deg_between(prev, yaw(90.0))
        );
    }

    #[test]
    fn exact_mode_applies_instantly() {
        let mut t = HeadTracker::new(TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 0.0,
        });
        t.push(HeadSample::new(0.0, yaw(0.0)));
        t.push(HeadSample::new(0.01, yaw(90.0)));
        let q = t.sample(0.01);
        assert!(
            deg_between(q, yaw(90.0)) < 1e-4,
            "exact mode snaps to the target"
        );
    }

    #[test]
    fn rate_limit_caps_the_angular_step() {
        // 100°/s limit at 100 Hz → at most 1° per sample; a 90° jump then
        // takes ~90 samples and never exceeds the cap.
        let mut t = HeadTracker::new(TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 100.0,
        });
        t.push(HeadSample::new(0.0, yaw(0.0)));
        t.push(HeadSample::new(0.01, yaw(90.0)));
        let mut prev = t.sample(0.0);
        let mut max_step = 0.0f32;
        for k in 1..=120 {
            let q = t.sample(0.01 * k as f64);
            max_step = max_step.max(deg_between(prev, q));
            prev = q;
        }
        assert!(max_step <= 1.001, "per-sample step ≤ 1° (max {max_step}°)");
        assert!(
            deg_between(prev, yaw(90.0)) < 1.0,
            "caught up to the target"
        );
    }

    #[test]
    fn reset_replaces_state_and_first_sample_snaps() {
        // A fresh tracker's first sample snaps to the target even with
        // smoothing on (no easing from identity).
        let mut t = HeadTracker::new(TrackingConfig::default());
        t.reset(HeadSample::new(0.0, yaw(45.0)));
        let q = t.sample(0.1);
        assert!(deg_between(q, yaw(45.0)) < 1e-4, "reset snaps");
        // Identity before any sample.
        let mut t2 = HeadTracker::new(TrackingConfig::default());
        assert_eq!(t2.current(), Q::IDENTITY);
        assert!(!t2.has_samples());
        assert_eq!(t2.sample(0.5), Q::IDENTITY);
    }

    #[test]
    fn apply_to_writes_the_listener() {
        let mut t = HeadTracker::new(TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 0.0,
        });
        t.push(HeadSample::new(0.0, yaw(0.0)));
        t.push(HeadSample::new(0.1, yaw(90.0)));
        let mut scene = crate::spatial::scene::SpatialScene::new(48_000);
        let q = t.apply_to(&mut scene.listener, 0.1);
        assert_eq!(scene.listener.orientation, q);
        assert!(scene.listener.orientation.angle_to(yaw(90.0)) < 1e-5);
    }

    #[test]
    fn yaw_sweep_tracks_closed_form_block_by_block() {
        // The host pattern: push each IMU sample, then sample the listener
        // orientation for the current render block. With exact tracking the
        // tracker output equals the closed-form yaw at every block.
        let mut t = HeadTracker::new(TrackingConfig {
            smoothing_ms: 0.0,
            max_angular_rate_deg_s: 0.0,
        });
        let n = 24usize;
        for k in 0..=n {
            let deg = 137.0 * k as f32 / n as f32;
            t.push(HeadSample::new(0.01 * k as f64, yaw(deg)));
            let q = t.sample(0.01 * k as f64);
            let got = deg_between(q, yaw(deg));
            assert!(got < 1e-3, "sweep point {k}: {got}°");
            assert!(q.length() - 1.0 < 1e-6, "unit quaternion");
        }
    }
}
