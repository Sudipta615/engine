//! Binaural head model — Woodworth ITD + Duda-Martens head shadow (spec
//! Part VII §47–48, §62).
//!
//! Binaural rendering replaces the speaker array with a *head*: two ear
//! signals whose differences localize a source. This module ships the two
//! classic, openly documented cues used by the [`BinauralRenderer`]:
//!
//! - **Interaural time difference (ITD)** — the Woodworth formula: a source
//!   at angular distance `θ` from the straight-ahead axis (measured toward
//!   the side of the ear) arrives at the contralateral ear
//!   `(a/c)·(sinθ + θ)` seconds later than at the ipsilateral ear (for
//!   `θ ≤ π/2`; `(a/c)·(π − θ + sinθ)` behind — zero again straight back,
//!   the documented front/back cone ambiguity). `a` is the head radius,
//!   `c` the speed of sound.
//! - **Head shadow / interaural level difference (ILD)** — the
//!   Duda-Martens spherical-head shelf: a first-order filter
//!   `H(s) = (1 + α·s/ω₀)/(1 + s/ω₀)` with `ω₀ = c/a`, whose high-frequency
//!   asymptote `α(φ) = 1.05 + 0.95·sin(φ)` (`φ` = azimuth measured toward
//!   the ear) is ≈ 2.0 at the ear (the diffraction boost) and ≈ 0.1 at the
//!   far side (the shadow). At DC the shelf is exactly unity — the head is
//!   acoustically transparent at low frequencies, as in reality.
//!
//! Everything here is a *function of the horizontal azimuth only*: the
//! model carries no elevation cues (a documented, bounded simplification —
//! see the renderer's module docs). All angles use the spatial layer's
//! single convention (`0` = front, `+π/2` = right; see [`math`]).
//!
//! ## Realtime discipline
//!
//! [`HeadShadow`] is a one-pole/one-zero filter whose only per-block state
//! change is the smoothed `α`; [`read_delayed`] is a fractional (linearly
//! interpolated) ring read. Both are allocation-free and lock-free, and the
//! renderer's per-path state is preallocated flat at `prepare`.

/// Default head radius (m) — the classic 8.75 cm half-head-width.
pub const DEFAULT_HEAD_RADIUS: f32 = 0.0875;

/// Default speed of sound (m/s).
pub const DEFAULT_SPEED_OF_SOUND: f32 = 343.0;

/// Which ear a head-model path serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ear {
    Left,
    Right,
}

impl Ear {
    pub const COUNT: usize = 2;

    #[inline]
    pub fn index(self) -> usize {
        match self {
            Ear::Left => 0,
            Ear::Right => 1,
        }
    }

    #[inline]
    pub fn from_index(i: usize) -> Ear {
        if i & 1 == 0 {
            Ear::Left
        } else {
            Ear::Right
        }
    }

    /// +1 toward the right, −1 toward the left (multiplies the azimuth).
    #[inline]
    fn side(self) -> f32 {
        match self {
            Ear::Left => -1.0,
            Ear::Right => 1.0,
        }
    }
}

/// Woodworth ITD magnitude (seconds) for a source `θ` off the straight-ahead
/// axis: `0` front, maximum at the ear (`θ = π/2`), `0` straight behind
/// (front/back cone ambiguity, documented). `θ` is folded into `[0, π]`.
pub fn woodworth_itd_sec(azimuth: f32, head_radius: f32, speed: f32) -> f32 {
    let theta = azimuth
        .abs()
        .rem_euclid(std::f32::consts::TAU)
        .min(std::f32::consts::PI);
    let a = head_radius.max(0.05);
    let c = speed.max(1.0);
    let t = if theta <= std::f32::consts::FRAC_PI_2 {
        theta.sin() + theta
    } else {
        std::f32::consts::PI - theta + theta.sin()
    };
    a / c * t
}

/// The ITD delay (seconds) applied to `ear`'s signal for a source at
/// `azimuth`: the **contralateral** ear is delayed by the full Woodworth
/// ITD, the ipsilateral ear by zero — the source reaches the near ear
/// first.
pub fn ear_delay_sec(azimuth: f32, ear: Ear, head_radius: f32, speed: f32) -> f32 {
    let on_ear_side = azimuth.signum() * ear.side() >= 0.0;
    if on_ear_side {
        0.0
    } else {
        woodworth_itd_sec(azimuth, head_radius, speed)
    }
}

/// Duda-Martens head-shadow coefficient `α` for `ear`: `1.05 + 0.95·sin(φ)`
/// with `φ` the azimuth measured toward that ear. `α = 1.05` straight ahead
/// (near-flat), `≈ 2.0` at the ear (diffraction boost), `≈ 0.1` at the far
/// side (shadow).
#[inline]
pub fn head_shadow_alpha(azimuth: f32, ear: Ear) -> f32 {
    1.05 + 0.95 * (azimuth * ear.side()).sin()
}

/// The maximum Woodworth ITD over all azimuths (seconds): `(a/c)(π/2 + 1)`
/// at the ear axis. Used to size the renderer's delay lines.
pub fn max_itd_sec(head_radius: f32, speed: f32) -> f32 {
    let a = head_radius.max(0.05);
    let c = speed.max(1.0);
    a / c * (std::f32::consts::FRAC_PI_2 + 1.0)
}

/// A Duda-Martens head-shadow shelf (first-order, `H(s) = (1 + α·s/ω₀)/
/// (1 + s/ω₀)`), bilinear-transformed to a one-pole/one-zero digital
/// filter. `α` is one-pole smoothed at block rate so automated azimuth
/// changes ramp instead of zippering; per-sample processing is 3
/// multiplies + 2 adds.
///
/// Properties (pinned by tests): DC gain exactly 1, high-frequency
/// asymptote exactly `α`, `α = 1` is an exact passthrough.
#[derive(Debug, Clone, Copy)]
pub struct HeadShadow {
    /// Smoothed `α` (the filter's HF asymptote).
    alpha: f32,
    /// `fs/(π·f₀)` — sample-rate constant, set once at `prepare`.
    k: f32,
    b0: f32,
    b1: f32,
    a1: f32,
    x1: f32,
    y1: f32,
}

impl Default for HeadShadow {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadShadow {
    pub fn new() -> Self {
        Self {
            alpha: 1.05,
            k: 0.0,
            b0: 1.0,
            b1: 0.0,
            a1: 0.0,
            x1: 0.0,
            y1: 0.0,
        }
    }

    /// Control path: derive the sample-rate constant from `f₀ = c/(2πa)`.
    /// The filter is usable (as a front-facing shelf) immediately.
    pub fn prepare(&mut self, sample_rate: f32, head_radius: f32, speed: f32) {
        let f0 = speed.max(1.0) / (std::f32::consts::TAU * head_radius.max(0.05));
        self.k = sample_rate.max(1.0) / (std::f32::consts::PI * f0);
        self.set_alpha(self.alpha);
    }

    fn set_alpha(&mut self, a: f32) {
        self.alpha = a;
        let k = self.k;
        let d = 1.0 + k;
        // Bilinear transform of H(s) = (1 + αs/ω₀)/(1 + s/ω₀), k = 2fs/ω₀.
        self.b0 = (1.0 + a * k) / d;
        self.b1 = (1.0 - a * k) / d;
        self.a1 = (1.0 - k) / d;
    }

    /// Current (smoothed) HF asymptote.
    pub fn alpha(&self) -> f32 {
        self.alpha
    }

    /// Advance the one-pole smoothing toward `target` (`smooth = 1.0` snaps
    /// exactly) and recompute the coefficients. Block-rate call.
    pub fn set_target(&mut self, target: f32, smooth: f32) {
        let t = target.clamp(0.05, 3.0);
        let a = if smooth >= 1.0 {
            t
        } else {
            self.alpha + smooth * (t - self.alpha)
        };
        self.set_alpha(a);
    }

    /// Filter one sample.
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 - self.a1 * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

/// Fractional (linearly interpolated) read from a ring buffer at
/// `delay_samples` behind `cursor`. `delay` is clamped to `[0, len−1]` so a
/// misconfigured delay can never index out of bounds. This is the
/// interpolation that makes ITD changes smooth as a source moves — the
/// delay moves continuously through the ring instead of stepping sample by
/// sample.
#[inline]
pub fn read_delayed(ring: &[f32], cursor: usize, delay_samples: f32, len: usize) -> f32 {
    let l = len.max(2) as f32;
    let d = delay_samples.clamp(0.0, l - 1.0);
    let i = d.floor() as usize;
    let f = d - i as f32;
    let a = ring[(cursor + len - i) % len];
    let b = ring[(cursor + len - i - 1) % len];
    a + f * (b - a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};

    const EPS: f32 = 1e-4;

    #[test]
    fn woodworth_values_pin_closed_form() {
        // front = 0, ear axis = max (a/c)(π/2 + 1), rear = 0.
        assert!(woodworth_itd_sec(0.0, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND).abs() < EPS);
        let max = max_itd_sec(DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        let at_ear = woodworth_itd_sec(FRAC_PI_2, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        assert!(
            (at_ear - max).abs() < EPS,
            "ear axis = max {at_ear} vs {max}"
        );
        assert!((max - 0.0875 / 343.0 * (FRAC_PI_2 + 1.0)).abs() < 1e-6);
        assert!(woodworth_itd_sec(PI, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND).abs() < EPS);
        assert!(
            woodworth_itd_sec(FRAC_PI_4, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND)
                - 0.0875 / 343.0 * (FRAC_PI_4.sin() + FRAC_PI_4).abs()
                < 1e-7
        );
        // Monotonic rising to the ear axis; symmetric in sign and folding.
        let mut prev = 0.0f32;
        for deg in (0..=90).step_by(10) {
            let itd = woodworth_itd_sec(
                deg as f32 * PI / 180.0,
                DEFAULT_HEAD_RADIUS,
                DEFAULT_SPEED_OF_SOUND,
            );
            assert!(itd >= prev - 1e-7, "monotonic at {deg}°");
            prev = itd;
        }
        let a = woodworth_itd_sec(0.7, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        let b = woodworth_itd_sec(-0.7, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        assert!((a - b).abs() < EPS, "sign-symmetric");
    }

    #[test]
    fn ear_delay_contralateral_only() {
        // +90° (right): the left ear is delayed by the full ITD, the right
        // by zero. −90°: mirrored.
        let itd = woodworth_itd_sec(FRAC_PI_2, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        assert!(
            (ear_delay_sec(
                FRAC_PI_2,
                Ear::Left,
                DEFAULT_HEAD_RADIUS,
                DEFAULT_SPEED_OF_SOUND
            ) - itd)
                .abs()
                < EPS
        );
        assert!(
            ear_delay_sec(
                FRAC_PI_2,
                Ear::Right,
                DEFAULT_HEAD_RADIUS,
                DEFAULT_SPEED_OF_SOUND
            )
            .abs()
                < EPS
        );
        assert!(
            ear_delay_sec(
                -FRAC_PI_2,
                Ear::Right,
                DEFAULT_HEAD_RADIUS,
                DEFAULT_SPEED_OF_SOUND
            ) > 0.0
        );
        assert!(
            ear_delay_sec(
                -FRAC_PI_2,
                Ear::Left,
                DEFAULT_HEAD_RADIUS,
                DEFAULT_SPEED_OF_SOUND
            )
            .abs()
                < EPS
        );
        // Front and rear: no delay on either ear.
        assert!(
            ear_delay_sec(0.0, Ear::Left, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND).abs() < EPS
        );
        assert!(
            ear_delay_sec(PI, Ear::Right, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND).abs() < EPS
        );
    }

    #[test]
    fn head_shadow_alpha_values_and_complement() {
        // Front: 1.05 both ears. Ear axis: 2.0 ipsi, 0.1 contra.
        assert!((head_shadow_alpha(0.0, Ear::Left) - 1.05).abs() < EPS);
        assert!((head_shadow_alpha(0.0, Ear::Right) - 1.05).abs() < EPS);
        assert!((head_shadow_alpha(FRAC_PI_2, Ear::Right) - 2.0).abs() < EPS);
        assert!((head_shadow_alpha(FRAC_PI_2, Ear::Left) - 0.1).abs() < EPS);
        // Mirror complement: α(az, L) = α(−az, R).
        for az in [-1.3f32, -0.5, 0.0, 0.5, 1.3] {
            assert!(
                (head_shadow_alpha(az, Ear::Left) - head_shadow_alpha(-az, Ear::Right)).abs()
                    < 1e-6
            );
        }
        // Bounded.
        for az in [-PI, -1.0, 0.0, 1.0, PI] {
            let a = head_shadow_alpha(az, Ear::Left);
            assert!((0.05..=3.0).contains(&a), "α bounded at {az}: {a}");
        }
    }

    #[test]
    fn shelf_dc_gain_is_unity() {
        // A constant input passes through at exactly 1.0 (the DC gain of the
        // shelf is 1 for every α).
        for &alpha in &[0.1f32, 1.05, 2.0] {
            let mut sh = HeadShadow::new();
            sh.prepare(48_000.0, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
            sh.set_target(alpha, 1.0);
            let mut y = 0.0f32;
            for _ in 0..200 {
                y = sh.process(1.0);
            }
            assert!((y - 1.0).abs() < 1e-4, "DC unity for α={alpha}: {y}");
        }
    }

    #[test]
    fn shelf_high_frequency_asymptote_is_alpha() {
        // An alternating ±1 input (Nyquist) converges to ±α.
        for &alpha in &[0.1f32, 1.05, 2.0] {
            let mut sh = HeadShadow::new();
            sh.prepare(48_000.0, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
            sh.set_target(alpha, 1.0);
            let mut y = 0.0f32;
            for n in 0..400 {
                let x = if n & 1 == 0 { 1.0 } else { -1.0 };
                y = sh.process(x);
            }
            let expect = if 399 & 1 == 0 { alpha } else { -alpha };
            assert!((y - expect).abs() < 1e-3, "Nyquist ≈ α for α={alpha}: {y}");
        }
    }

    #[test]
    fn shelf_alpha_one_is_exact_passthrough() {
        let mut sh = HeadShadow::new();
        sh.prepare(48_000.0, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        sh.set_target(1.0, 1.0);
        for x in [0.5f32, -0.25, 0.125, -0.0625, 1.0] {
            let y = sh.process(x);
            assert!((y - x).abs() < 1e-6, "α=1 → H(z)=1 (got {y} for {x})");
        }
    }

    #[test]
    fn shelf_smooths_alpha_changes() {
        let mut sh = HeadShadow::new();
        sh.prepare(48_000.0, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        sh.set_target(2.0, 0.1);
        let first = sh.alpha();
        assert!(first > 1.05 && first < 2.0, "smoothed toward 2.0: {first}");
        sh.set_target(2.0, 0.1);
        let second = sh.alpha();
        assert!(second > first, "keeps ramping: {first} → {second}");
        sh.set_target(2.0, 1.0);
        assert!((sh.alpha() - 2.0).abs() < 1e-6, "snaps with smooth=1");
    }
    #[test]
    fn read_delayed_interpolates_linearly_and_clamps() {
        let ring = [0.0f32, 10.0, 20.0, 30.0];
        // cursor 3, delay 1.5 → between ring[2] (20) and ring[1] (10) → 15.
        assert!((read_delayed(&ring, 3, 1.5, 4) - 15.0).abs() < 1e-6);
        // delay 0 → ring[cursor] = ring[3] = 30.
        assert!((read_delayed(&ring, 3, 0.0, 4) - 30.0).abs() < 1e-6);
        // Integer delay reads exactly.
        assert!((read_delayed(&ring, 3, 2.0, 4) - ring[(3 + 4 - 2) % 4]).abs() < 1e-6);
        // Oversized delay clamps to len−1 (never out of bounds).
        assert!(read_delayed(&ring, 3, 1000.0, 4).is_finite());
    }
}
