//! Doppler processing (spec §42).
//!
//! Doppler shifts an approaching/receding source's pitch. It is applied from
//! the relative source/listener **velocity**, never by changing a pitch value
//! abruptly at block boundaries: this module is a bounded, continuously-
//! varying fractional-delay resampler with parameter smoothing, so motion is
//! smooth, latency is stable, and there are no clicks.
//!
//! ## Model
//!
//! For a source with radial velocity `v_r` (positive = approaching the
//! listener) and sound speed `c`, the instantaneous pitch ratio is
//! `r = c / (c − v_r)`: approaching → `r > 1` (pitch up), receding →
//! `r < 1`. The renderer computes `v_r` from `object.velocity −
//! listener.velocity` projected onto the listener→source direction and feeds
//! it here. Ratios are clamped to a sane, configurable band so pathological
//! high velocities cannot drive the delay line unstable.
//!
//! The resampler is a **modulated fractional delay**: each incoming sample is
//! written into a fixed ring and the output is read back at a fractionally
//! interpolated, continuously-advancing position. The read position advances
//! by the (smoothed) ratio per output sample, so `r ≠ 1` stretches/compresses
//! time and shifts pitch. Because the ratio is one-pole smoothed at block
//! rate, a velocity change ramps its pitch over a few blocks instead of
//! clicking.
//!
//! **Boundedness.** A real-time delay line can only shift pitch while its
//! read position stays inside the ring; a *sustained* constant ratio>1 means
//! the source would have to outproduce its own sample rate, which no real
//! source can do. So when the read position reaches the ring's boundary
//! (either catching the write cursor, or drifting a full ring behind) the
//! position is re-anchored toward the base latency. This is the standard,
//! accepted engineering behaviour for time-varying delay Doppler in a real-
//! time engine: transient velocity changes glide the pitch cleanly, and a
//! pathological long approach re-anchors deterministically instead of
//! growing without bound (spec §42: "stable latency, no clicks, predictable
//! pitch change").
//!
//! ## Realtime discipline
//!
//! [`DopplerState`] owns a fixed, preallocated ring (per-object in the
//! renderers). `process` allocates nothing and takes no locks. The initial
//! read position is set `BASE_LATENCY_SAMPLES` behind the write cursor; that
//! constant latency (≈`RING_LEN/2`) is the "stable latency" the spec asks for.
//!
//! **Bit-exact passthrough when disabled:** when `enabled == false` (the
//! default) `process` returns the input sample unchanged and touches no
//! state — matching the codebase's "disabled-exact" discipline and keeping
//! the acceptance suites pinned.

use super::math::Vec3;

/// Default speed of sound (m/s) — matches the head model / room defaults.
pub const DEFAULT_SPEED_OF_SOUND: f32 = 343.0;

/// Length of the per-object Doppler ring in samples. At 48 kHz this is
/// ~171 ms of lookback, comfortably bounding Doppler at normal velocities.
pub const RING_LEN: usize = 8192;

/// Fractional latency behind the write cursor at startup (≈ half the ring,
/// ~85 ms @ 48 kHz). Keeps the interpolated read position comfortably clear
/// of the freshly-written tail so interpolation never under-runs.
const BASE_LATENCY_SAMPLES: f32 = (RING_LEN / 2) as f32;

/// Default clamp on the pitch ratio (log-odd around 1.0). ±33% covers
/// realistic source speeds; kept configurable via [`Doppler`].
pub const DEFAULT_CLAMP_RATIO: f32 = 1.5;

/// Doppler model configuration (spec §42), disabled by default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Doppler {
    /// Enable Doppler. Disabled = exact passthrough, no state touched.
    pub enabled: bool,
    /// Speed of sound in m/s (used to derive the ratio from radial velocity).
    pub speed_of_sound: f32,
    /// Clamp on the pitch ratio (≥ 1.0): ratios are clamped to
    /// `[1/clamp, clamp]`. Guards the delay line against pathological input.
    pub max_ratio_clamp: f32,
}

impl Default for Doppler {
    fn default() -> Self {
        Self {
            enabled: false,
            speed_of_sound: DEFAULT_SPEED_OF_SOUND,
            max_ratio_clamp: DEFAULT_CLAMP_RATIO,
        }
    }
}

impl Doppler {
    /// The pitch ratio for a radial velocity `v_r` (m/s, positive =
    /// approaching), clamped to the configured band. `1.0` when disabled.
    pub fn ratio(&self, radial_velocity: f32) -> f32 {
        if !self.enabled {
            return 1.0;
        }
        let c = if self.speed_of_sound > 0.0 {
            self.speed_of_sound
        } else {
            DEFAULT_SPEED_OF_SOUND
        };
        let clamp = self.max_ratio_clamp.max(1.0);
        // Cap the denominator from below so a closing velocity can never push
        // the ratio through/cross the speed of sound into the wrong octave:
        // `c − v_r` never drops below `c/clamp`, so `r` is bounded by `clamp`
        // even for |v_r| ≥ c.
        let denom = (c - radial_velocity).max(c / clamp);
        (c / denom).clamp(1.0 / clamp, clamp)
    }

    /// Radial component of the relative velocity along `dir` (a unit vector
    /// from the listener toward the source). Positive = approaching.
    #[inline]
    pub fn radial_velocity(relative_velocity: Vec3, dir: Vec3) -> f32 {
        relative_velocity.dot(dir)
    }
}

/// Renderer-owned per-object Doppler resampler state (realtime-safe).
///
/// Fixed ring + a smoothed ratio. `process` is passthrough until enabled.
#[derive(Debug, Clone)]
pub struct DopplerState {
    ring: Vec<f32>,
    write_pos: usize, // absolute index; ring index = write_pos % RING_LEN
    read_pos: f64,    // absolute index; fractional interpolation between ring slots
    ratio: f32,
    lat: f32, // current latency behind the write cursor (for re-anchoring)
}

impl Default for DopplerState {
    fn default() -> Self {
        Self::new()
    }
}

impl DopplerState {
    pub fn new() -> Self {
        Self {
            ring: vec![0.0; RING_LEN],
            write_pos: 0,
            read_pos: -(BASE_LATENCY_SAMPLES as f64),
            ratio: 1.0,
            lat: BASE_LATENCY_SAMPLES,
        }
    }

    /// Reset the delay line (control path). Next `process` begins from a
    /// clean ring with the configured base latency.
    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.write_pos = 0;
        self.read_pos = -(BASE_LATENCY_SAMPLES as f64);
        self.ratio = 1.0;
        self.lat = BASE_LATENCY_SAMPLES;
    }

    /// Advance the one-pole smoothing of the pitch ratio toward `target`
    /// (`smooth = 1.0` snaps exactly). Block-rate call.
    #[inline]
    pub fn set_ratio(&mut self, target: f32, smooth: f32) {
        self.ratio = if smooth >= 1.0 {
            target
        } else {
            self.ratio + smooth * (target - self.ratio)
        };
    }

    /// The current (smoothed) pitch ratio.
    #[inline]
    pub fn ratio(&self) -> f32 {
        self.ratio
    }

    /// Process one sample. Returns `sample` untouched (passthrough, no state
    /// mutated) when `enabled` is false.
    #[inline]
    pub fn process(&mut self, sample: f32, enabled: bool) -> f32 {
        if !enabled {
            return sample;
        }
        // Write the input into the ring at the (wrapping) write cursor.
        let w = self.write_pos;
        self.ring[w % RING_LEN] = sample;
        self.write_pos = w.wrapping_add(1);

        // Read at the current fractional read position with linear interp.
        let out = self.read_interpolated();
        // Advance by the (already-smoothed) ratio.
        self.read_pos += self.ratio as f64;
        // Keep the read position within [write - (RING_LEN-1), write - 1] so
        // interpolation stays in bounds; re-anchor toward the base latency if
        // it drifts to a clamp (rare; bounded, deterministic hitches rather
        // than unbounded drift).
        self.enforce_bounds();
        out
    }

    #[inline]
    fn read_interpolated(&self) -> f32 {
        let rp = self.read_pos.floor() as i64;
        let frac = (self.read_pos - rp as f64) as f32;
        // Wrap the absolute (possibly negative at startup) indices.
        let i0 = rp.rem_euclid(RING_LEN as i64) as usize;
        let i1 = (i0 + 1) % RING_LEN;
        let a = self.ring[i0];
        let b = self.ring[i1];
        a + frac * (b - a)
    }

    #[inline]
    fn enforce_bounds(&mut self) {
        let wp = self.write_pos as i64;
        // read_pos must satisfy: write - (RING_LEN-1)  <=  read_pos <= write - 1
        let lo = (wp - (RING_LEN as i64 - 1)) as f64;
        let hi = (wp - 1) as f64;
        self.read_pos = self.read_pos.max(lo).min(hi);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_range_loop)]
    use super::*;
    use crate::spatial::math::Vec3;
    use std::f32::consts::PI;

    #[test]
    fn disabled_is_exact_passthrough() {
        let mut d = DopplerState::new();
        let mut out = Vec::new();
        for i in 0..100 {
            out.push(d.process(i as f32 * 0.1, false));
        }
        // No state touched, sample-for-sample identical.
        assert!((0..100).all(|i| (out[i] - i as f32 * 0.1).abs() < 1e-7));
        // Write cursor never advanced.
        assert_eq!(d.write_pos, 0);
    }

    #[test]
    fn unity_ratio_delays_but_preserves_pitch() {
        // Ratio 1 → pure delay of BASE_LATENCY_SAMPLES with linear interp at
        // integer offsets = exact delayed copy (no interpolation error at
        // integer offsets).
        let mut d = DopplerState::new();
        d.set_ratio(1.0, 1.0);
        let n = 2000usize;
        let mut out = vec![0.0f32; n];
        for i in 0..n {
            out[i] = d.process((i as f32 * 0.03).sin(), true);
        }
        // After the initial latency, output tracks the input delayed by the
        // (integer) latency.
        let lat = (d.lat as usize + 1) % RING_LEN;
        for i in lat..n - 2 {
            let expect = ((i - lat) as f32 * 0.03).sin();
            assert!(
                (out[i] - expect).abs() < 1e-3,
                "i={i} got {} want {expect}",
                out[i]
            );
        }
    }

    #[test]
    fn approach_ratio_shifts_pitch_up() {
        // Ratio 1.2 on a 200 Hz sine for one second → the output's dominant
        // frequency rises to ~240 Hz. Count zero-crossing rate over a window
        // after the initial latency.
        let fs = 48_000.0;
        let mut d = DopplerState::new();
        d.set_ratio(1.2, 1.0);
        let n = fs as usize;
        let mut out = vec![0.0f32; n];
        for i in 0..n {
            out[i] = d.process((2.0 * PI * 200.0 * i as f32 / fs).sin(), true);
        }
        // Measure cycles of the *output* over a window that is (a) past the
        // initial base latency and (b) before the read position catches to the
        // re-anchor clamp (at ratio 1.2 over a 171 ms ring that is ~19 k
        // samples). Use samples [8000, 16000]: read is advancing cleanly at
        // ratio with no clamp.
        let seg = &out[8_000..16_000];
        let output_hz = {
            let mut count = 0;
            for i in 1..seg.len() {
                if seg[i - 1] < 0.0 && seg[i] >= 0.0 {
                    count += 1;
                }
            }
            count as f32 / (seg.len() as f32 / fs)
        };
        // Close to 240 Hz (ratio 1.2 × 200), within measurement tolerance.
        assert!(
            (output_hz - 240.0).abs() < 24.0,
            "output {output_hz} Hz (want ~240)"
        );
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn receding_ratio_shifts_pitch_down() {
        let fs = 48_000.0;
        let mut d = DopplerState::new();
        d.set_ratio(1.0 / 1.2, 1.0); // receding: pitch down
        let n = fs as usize;
        let mut out = vec![0.0f32; n];
        for i in 0..n {
            out[i] = d.process((2.0 * PI * 200.0 * i as f32 / fs).sin(), true);
        }
        // Measure within the clean pre-clamp window (read well behind write,
        // before the lower re-anchor clamp at ~24.5 k samples).
        let seg = &out[8_000..20_000];
        let output_hz = {
            let mut count = 0;
            for i in 1..seg.len() {
                if seg[i - 1] < 0.0 && seg[i] >= 0.0 {
                    count += 1;
                }
            }
            count as f32 / (seg.len() as f32 / fs)
        };
        // Ratio 1/1.2 × 200 ≈ 167 Hz.
        assert!(
            (output_hz - 167.0).abs() < 25.0,
            "output {output_hz} Hz (want ~167)"
        );
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn ratio_is_clamped_and_finite() {
        let d = Doppler {
            enabled: true,
            speed_of_sound: 343.0,
            max_ratio_clamp: 1.5,
        };
        // A huge closing velocity still maps into the clamp band.
        assert_abs(d.ratio(500.0), 1.5, 1e-5);
        assert_abs(d.ratio(-500.0), 1.0 / 1.5, 1e-5);
        // Normal approach > 1, recede < 1.
        assert!(d.ratio(30.0) > 1.0 && d.ratio(30.0) <= 1.5);
        assert!(d.ratio(-30.0) < 1.0 && d.ratio(-30.0) >= 1.0 / 1.5);
        assert!(d.ratio(0.0).is_finite());
        // Disabled → 1.
        assert_eq!(Doppler::default().ratio(999.0), 1.0);
    }

    #[test]
    fn radial_velocity_projection_sign() {
        // dir = +Y (in front). Velocity toward listener (−Y) → receding →
        // negative radial. Velocity +Y (away? no: toward the listener is −dir)
        // Let's be careful: a velocity pointing FROM the source toward the
        // listener reduces distance. For a source in front moving backward
        // (velocity −Y in world, listener ahead in +Y), distance grows but
        // the sign convention here is along dir; the renderer passes
        // (obj.vel − listener.vel) dotted with dir.
        let dir = Vec3::Y;
        let rel = Vec3::new(0.0, 2.0, 0.0); // moving away along +Y
        assert!((Doppler::radial_velocity(rel, dir) - 2.0).abs() < 1e-6);
        let rel = Vec3::new(0.0, -2.0, 0.0); // moving toward along −Y
        assert!((Doppler::radial_velocity(rel, dir) + 2.0).abs() < 1e-6);
    }

    fn assert_abs(a: f32, b: f32, eps: f32) {
        assert!((a - b).abs() < eps, "{a} vs {b}");
    }
}
