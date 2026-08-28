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
//! Everything here is a *function of the horizontal azimuth* (the classic
//! model carries no elevation cues), extended in Phase 18 with:
//!
//! - **Elevation cues** — [`ElevationNotch`], a documented pinna-notch
//!   biquad whose center frequency rises with elevation
//!   (`f = 6 kHz + 4 kHz·sin(el)`, depth `−8 dB·|sin(el)|`, so 0° elevation
//!   is an exact passthrough). This is the analytic fallback when no
//!   measured dataset is loaded.
//! - **Measured spectral HRTFs** — [`HrtfDataset`]: a grid of per-ear
//!   impulse responses (azimuth × elevation) with bilinear interpolation,
//!   so a renderer can replace the analytic shelf/ITD path with real
//!   head-related impulse responses that carry both the ITD and the
//!   elevation-dependent spectral cues.
//!
//! All angles use the spatial layer's single convention (`0` = front,
//! `+π/2` = right; see [`math`]).
//!
//! ## Realtime discipline
//!
//! [`HeadShadow`] is a one-pole/one-zero filter whose only per-block state
//! change is the smoothed `α`; [`ElevationNotch`] is a biquad with the same
//! block-smoothed coefficient pattern; [`read_delayed`] is a fractional
//! (linearly interpolated) ring read; and [`HrtfDataset::bilinear_interpolate`]
//! writes into a caller-provided scratch. All are allocation-free and
//! lock-free, and the renderer's per-path state is preallocated flat at
//! `prepare`.

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
/// (front/back cone ambiguity, documented). `θ` is folded into `[0, π]` by
/// *reflection*: the ITD is even in azimuth (left/right symmetric), so an
/// angle of 300° (physically −60°) folds to 60°, not to 180°.
pub fn woodworth_itd_sec(azimuth: f32, head_radius: f32, speed: f32) -> f32 {
    let mut theta = azimuth.abs().rem_euclid(std::f32::consts::TAU);
    if theta > std::f32::consts::PI {
        theta = std::f32::consts::TAU - theta;
    }
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
/// first. The left/right side test uses `sin(azimuth)` (the signed lateral
/// component in the `0 = front, +π/2 = right` convention) rather than
/// `azimuth.signum()`, so it stays correct for azimuths wrapped past ±π
/// (e.g. 300° = −60°, a source on the *left*).
pub fn ear_delay_sec(azimuth: f32, ear: Ear, head_radius: f32, speed: f32) -> f32 {
    let on_ear_side = azimuth.sin().signum() * ear.side() >= 0.0;
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

/// Pinna-notch center frequency (Hz) for an elevation in radians: rises
/// from 6 kHz at 0° to 10 kHz at +90°, falls toward 2 kHz below the head
/// (negative elevations). Clamped to a sane band.
pub fn elevation_notch_hz(elevation_rad: f32) -> f32 {
    (6000.0 + 4000.0 * elevation_rad.sin()).clamp(1500.0, 11_000.0)
}

/// Pinna-notch depth (dB) for an elevation: `−8·|sin(el)|`, so 0° is an
/// exact passthrough (the notch only shapes spectra when the source leaves
/// the horizontal plane).
pub fn elevation_notch_depth_db(elevation_rad: f32) -> f32 {
    -8.0 * elevation_rad.sin().abs()
}

/// A documented pinna-notch biquad (RBJ peaking-EQ form with negative
/// gain): center `[`elevation_notch_hz`]`, `Q = 2.0`, depth
/// [`elevation_notch_depth_db`]. The coefficients are recomputed at block
/// rate with one-pole frequency smoothing (same pattern as
/// [`HeadShadow::set_target`]); a zero-depth (0° elevation) target is an
/// exact passthrough and skips per-sample work.
#[derive(Debug, Clone, Copy)]
pub struct ElevationNotch {
    /// Whether the notch is active (non-zero depth). Passthrough otherwise.
    active: bool,
    /// Smoothed center frequency (Hz).
    freq: f32,
    /// Sample-rate constant.
    fs: f32,
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Default for ElevationNotch {
    fn default() -> Self {
        Self::new()
    }
}

impl ElevationNotch {
    pub fn new() -> Self {
        Self {
            active: false,
            freq: 6000.0,
            fs: 48_000.0,
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Control path: set the sample rate.
    pub fn prepare(&mut self, sample_rate: f32) {
        self.fs = sample_rate.max(1.0);
        self.set_target(0.0, 1.0);
    }

    /// Set the elevation target (radians). `smooth = 1.0` snaps the
    /// frequency exactly; otherwise the center glides one-pole. A 0° target
    /// deactivates (exact passthrough).
    pub fn set_target(&mut self, elevation_rad: f32, smooth: f32) {
        let depth_db = elevation_notch_depth_db(elevation_rad);
        let freq = elevation_notch_hz(elevation_rad);
        if depth_db.abs() < 1e-4 {
            // Passthrough: zero-depth notch is exactly H(z) = 1.
            self.active = false;
            return;
        }
        let f = if smooth >= 1.0 {
            freq
        } else {
            self.freq + smooth * (freq - self.freq)
        };
        self.freq = f;
        self.active = true;
        // RBJ peaking EQ with negative gain (notch), Q = 2.0.
        let a = 10.0_f32.powf(depth_db / 40.0);
        let w0 = std::f32::consts::TAU * f / self.fs.max(1.0);
        let cos_w0 = w0.cos();
        let q = 2.0;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        self.b0 = (1.0 + alpha * a) / a0;
        self.b1 = (-2.0 * cos_w0) / a0;
        self.b2 = (1.0 - alpha * a) / a0;
        self.a1 = (-2.0 * cos_w0) / a0;
        self.a2 = (1.0 - alpha / a) / a0;
    }

    /// Whether the notch is active (non-zero depth at the current target).
    pub fn active(&self) -> bool {
        self.active
    }

    /// Filter one sample. Exact passthrough when inactive.
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        if !self.active {
            return x;
        }
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Maximum impulse-response taps a [`HrtfDataset`] may carry (bounds the
/// renderer's per-path FIR rings).
pub const MAX_HRTF_TAPS: usize = 128;

/// A grid of measured (or synthetic) head-related impulse responses:
/// `azimuths` (degrees, sorted, ascending within [0, 360)) × `elevations`
/// (degrees, sorted, ascending within [−90, 90]) × `Ear::COUNT` × `taps`.
///
/// Flat storage, `[az][el][ear][tap]`; the renderer interpolates
/// bilinearly in (azimuth, elevation) so any continuous direction gets a
/// smooth, deterministic HRTF. The dataset is loaded on the control path
/// and read-only on the audio path.
#[derive(Debug, Clone)]
pub struct HrtfDataset {
    azimuths: Vec<f32>,
    elevations: Vec<f32>,
    /// Flat `az × el × 2 × taps`.
    irs: Vec<f32>,
    taps: usize,
}

impl HrtfDataset {
    /// Build a dataset from explicit grids and IRs (flat `az × el × 2 ×
    /// taps`). Control path. `Err` on non-monotonic grids, out-of-range
    /// values, or a tap/IR length mismatch — an invalid dataset must never
    /// reach the audio thread.
    pub fn from_planes(
        azimuths: Vec<f32>,
        elevations: Vec<f32>,
        taps: usize,
        irs: Vec<f32>,
    ) -> Result<Self, &'static str> {
        if azimuths.is_empty() || elevations.is_empty() {
            return Err("hrtf dataset: empty grid");
        }
        if taps == 0 || taps > MAX_HRTF_TAPS {
            return Err("hrtf dataset: taps out of range");
        }
        for w in azimuths.windows(2) {
            if w[0] >= w[1] {
                return Err("hrtf dataset: azimuth grid not strictly ascending");
            }
        }
        for w in elevations.windows(2) {
            if w[0] >= w[1] {
                return Err("hrtf dataset: elevation grid not strictly ascending");
            }
        }
        if azimuths.iter().any(|a| !(0.0..360.0).contains(a))
            || elevations.iter().any(|e| !(-90.0..=90.0).contains(e))
        {
            return Err("hrtf dataset: grid values out of range");
        }
        let want = azimuths.len() * elevations.len() * Ear::COUNT * taps;
        if irs.len() != want {
            return Err("hrtf dataset: IR length mismatch");
        }
        if irs.iter().any(|v| !v.is_finite()) {
            return Err("hrtf dataset: non-finite IR");
        }
        Ok(Self {
            azimuths,
            elevations,
            irs,
            taps,
        })
    }

    /// A **synthetic** dataset discretizing the analytic model (head-shadow
    /// shelf + elevation notch + Woodworth ITD) on a regular grid, so the
    /// FIR path and interpolation are testable without shipping a measured
    /// corpus. Hosts replace it with measured IRs via [`Self::from_planes`].
    pub fn synthetic(sample_rate: u32, taps: usize, az_step_deg: f32, el_step_deg: f32) -> Self {
        let taps = taps.clamp(8, MAX_HRTF_TAPS);
        let mut azimuths = Vec::new();
        let mut a = 0.0f32;
        while a < 360.0 - 1e-4 {
            azimuths.push(a);
            a += az_step_deg;
        }
        let mut elevations = Vec::new();
        let mut e = -90.0f32;
        while e <= 90.0 + 1e-4 {
            elevations.push(e);
            e += el_step_deg;
        }
        let n = azimuths.len() * elevations.len() * Ear::COUNT * taps;
        let mut irs = vec![0.0f32; n];
        let fs = sample_rate.max(1_000) as f32;
        let mut idx = 0usize;
        for &az_deg in &azimuths {
            for &el_deg in &elevations {
                for ear in 0..Ear::COUNT {
                    let e = Ear::from_index(ear);
                    let az = az_deg.to_radians();
                    let el = el_deg.to_radians();
                    let delay =
                        ear_delay_sec(az, e, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND) * fs;
                    let whole = delay.floor() as usize;
                    let frac = delay - whole as f32;
                    // Run the analytic model on an impulse: shelf + notch,
                    // then a 2-tap fractional delay.
                    let mut sh = HeadShadow::new();
                    sh.prepare(fs, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
                    sh.set_target(head_shadow_alpha(az, e), 1.0);
                    let mut notch = ElevationNotch::new();
                    notch.prepare(fs);
                    notch.set_target(el, 1.0);
                    for k in 0..taps {
                        let imp = if k == 0 { 1.0 } else { 0.0 };
                        let mut v = sh.process(imp);
                        v = notch.process(v);
                        if k >= whole {
                            irs[idx + k] = v * (1.0 - frac);
                            if k + 1 < taps {
                                irs[idx + k + 1] += v * frac;
                            }
                        }
                    }
                    idx += taps;
                }
            }
        }
        Self {
            azimuths,
            elevations,
            irs,
            taps,
        }
    }

    pub fn azimuths(&self) -> &[f32] {
        &self.azimuths
    }

    pub fn elevations(&self) -> &[f32] {
        &self.elevations
    }

    /// Impulse-response length (taps) of every entry.
    pub fn taps(&self) -> usize {
        self.taps
    }

    /// The raw IR for a grid point `(az_idx, el_idx, ear)`.
    pub fn ir(&self, az_idx: usize, el_idx: usize, ear: Ear) -> &[f32] {
        let base = (az_idx * self.elevations.len() + el_idx) * Ear::COUNT * self.taps
            + ear.index() * self.taps;
        &self.irs[base..base + self.taps]
    }

    /// Bilinear interpolation in (azimuth, elevation) for `ear`, writing
    /// `taps` values into `out` (must hold ≥ [`Self::taps`]). The azimuth is
    /// wrapped into [0, 360); elevations clamp to the grid edges. Exact at
    /// grid points, linear between them — deterministic and allocation-free.
    pub fn bilinear_interpolate(&self, az_deg: f32, el_deg: f32, ear: Ear, out: &mut [f32]) {
        debug_assert!(out.len() >= self.taps);
        let az = az_deg.rem_euclid(360.0);
        let el = el_deg.clamp(-90.0, 90.0);
        let na = self.azimuths.len();
        let ne = self.elevations.len();
        // Lower grid index + fraction per axis (wrap the azimuth across the
        // 360° seam: the last column's upper neighbor is the first).
        let (ia, fa) = if az <= self.azimuths[0] {
            (0usize, 0.0f32)
        } else {
            let mut lo = 0usize;
            for (i, w) in self.azimuths.iter().enumerate() {
                if *w <= az {
                    lo = i;
                } else {
                    break;
                }
            }
            let hi = (lo + 1) % na;
            let span = (self.azimuths[hi] - self.azimuths[lo])
                .rem_euclid(360.0)
                .max(1e-6);
            (lo, ((az - self.azimuths[lo]) / span).clamp(0.0, 1.0))
        };
        let (ie, fe) = if el <= self.elevations[0] {
            (0usize, 0.0f32)
        } else if el >= self.elevations[ne - 1] {
            (ne - 1, 0.0f32)
        } else {
            let mut lo = 0usize;
            for (i, w) in self.elevations.iter().enumerate() {
                if *w <= el {
                    lo = i;
                } else {
                    break;
                }
            }
            let hi = lo + 1;
            let span = (self.elevations[hi] - self.elevations[lo]).max(1e-6);
            (lo, ((el - self.elevations[lo]) / span).clamp(0.0, 1.0))
        };
        let ia2 = (ia + 1) % na;
        let ie2 = (ie + 1).min(ne - 1);
        let stride = Ear::COUNT * self.taps;
        let base = |ia: usize, ie: usize| (ia * ne + ie) * stride + ear.index() * self.taps;
        let a00 = &self.irs[base(ia, ie)..base(ia, ie) + self.taps];
        let a10 = &self.irs[base(ia2, ie)..base(ia2, ie) + self.taps];
        let a01 = &self.irs[base(ia, ie2)..base(ia, ie2) + self.taps];
        let a11 = &self.irs[base(ia2, ie2)..base(ia2, ie2) + self.taps];
        for k in 0..self.taps {
            let lo = a00[k] + fa * (a10[k] - a00[k]);
            let hi = a01[k] + fa * (a11[k] - a01[k]);
            out[k] = lo + fe * (hi - lo);
        }
    }
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
        // The fold reflects angles past ±π: 300° is physically −60°, so it
        // must fold to 60° (the full ITD), never to 180° (zero).
        let deg = |d: f32| d * PI / 180.0;
        let w300 = woodworth_itd_sec(deg(300.0), DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        let w60 = woodworth_itd_sec(deg(60.0), DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        assert!(
            (w300 - w60).abs() < 1e-6,
            "300° folds to 60°: {w300} vs {w60}"
        );
        let w240 = woodworth_itd_sec(deg(240.0), DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        let w120 = woodworth_itd_sec(deg(120.0), DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        assert!(
            (w240 - w120).abs() < 1e-6,
            "240° folds to 120°: {w240} vs {w120}"
        );
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
        // Wrapped azimuths (0..360° grid convention): 300° is on the LEFT,
        // so the right ear is contralateral and gets the full ITD — the
        // mirror of +60°.
        let deg = |d: f32| d * PI / 180.0;
        let d300_r = ear_delay_sec(
            deg(300.0),
            Ear::Right,
            DEFAULT_HEAD_RADIUS,
            DEFAULT_SPEED_OF_SOUND,
        );
        let d60_l = ear_delay_sec(
            deg(60.0),
            Ear::Left,
            DEFAULT_HEAD_RADIUS,
            DEFAULT_SPEED_OF_SOUND,
        );
        assert!(
            (d300_r - d60_l).abs() < 1e-6,
            "300° R mirrors 60° L: {d300_r} vs {d60_l}"
        );
        assert!(
            ear_delay_sec(
                deg(300.0),
                Ear::Left,
                DEFAULT_HEAD_RADIUS,
                DEFAULT_SPEED_OF_SOUND
            )
            .abs()
                < EPS
        );
        assert!(
            ear_delay_sec(
                deg(240.0),
                Ear::Left,
                DEFAULT_HEAD_RADIUS,
                DEFAULT_SPEED_OF_SOUND
            )
            .abs()
                < EPS
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

    #[test]
    fn elevation_notch_passthrough_at_zero_elevation() {
        // 0° elevation → depth 0 → exact passthrough (the Phase-9 head
        // model behavior is unchanged).
        let mut n = ElevationNotch::new();
        n.prepare(48_000.0);
        n.set_target(0.0, 1.0);
        assert!(!n.active());
        for x in [1.0f32, -0.5, 0.25, -0.125, 1.0] {
            assert_eq!(n.process(x), x);
        }
    }

    #[test]
    fn elevation_notch_attenuates_its_center_frequency() {
        // el = +60° → f_notch = 6k + 4k·sin60 ≈ 9464 Hz, depth ≈ −6.9 dB.
        // A sine at the notch center is attenuated; a low-frequency sine
        // passes at unity.
        let el = 60f32.to_radians();
        let f = elevation_notch_hz(el);
        let mut n = ElevationNotch::new();
        n.prepare(48_000.0);
        n.set_target(el, 1.0);
        assert!(n.active());
        // Measure the impulse response's DFT at the notch center and at a
        // low frequency (the notch's spectral effect, not a time-domain
        // transient).
        let mut ir = [0.0f32; 256];
        for (k, slot) in ir.iter_mut().enumerate() {
            *slot = n.process(if k == 0 { 1.0 } else { 0.0 });
        }
        let at_center = dft_magnitude_at(&ir, f, 48_000.0);
        let at_low = dft_magnitude_at(&ir, 300.0, 48_000.0);
        let expect_center = 10.0_f32.powf(elevation_notch_depth_db(el) / 20.0); // ≈ 0.45
        assert!(
            (at_center - expect_center).abs() < 0.15,
            "center {at_center} vs {expect_center}"
        );
        assert!((at_low - 1.0).abs() < 0.03, "low passthrough {at_low}");
        // Depth scales with |el|: steeper at +90° than at +30°.
        let d90 = elevation_notch_depth_db(90f32.to_radians());
        let d30 = elevation_notch_depth_db(30f32.to_radians());
        assert!(d90 < d30, "{d90} dB < {d30} dB");
    }

    /// DFT magnitude at one frequency (test helper).
    fn dft_magnitude_at(ir: &[f32], freq_hz: f32, fs: f32) -> f32 {
        let w = std::f32::consts::TAU * freq_hz / fs;
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (k, &v) in ir.iter().enumerate() {
            let phase = w * k as f32;
            re += v * phase.cos();
            im -= v * phase.sin();
        }
        (re * re + im * im).sqrt()
    }

    #[test]
    fn synthetic_dataset_carries_itd_and_elevation_notch() {
        let ds = HrtfDataset::synthetic(48_000, 64, 15.0, 15.0);
        // A source at az 90°: the right (ipsilateral) ear's IR peaks near
        // tap 0; the left (contralateral) ear's IR is delayed by the full
        // Woodworth ITD (~31.5 samples).
        let az90 = 90f32;
        let az_idx = ds
            .azimuths()
            .iter()
            .position(|a| (*a - az90).abs() < 1e-3)
            .unwrap();
        let el_idx = ds.elevations().iter().position(|e| e.abs() < 1e-3).unwrap();
        let ir_r = ds.ir(az_idx, el_idx, Ear::Right);
        let ir_l = ds.ir(az_idx, el_idx, Ear::Left);
        let argmax = |ir: &[f32]| -> usize {
            ir.iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .map(|(i, _)| i)
                .unwrap()
        };
        assert!(argmax(ir_r) <= 2, "ipsilateral right ear near tap 0");
        let itd =
            woodworth_itd_sec(FRAC_PI_2, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND) * 48_000.0;
        let got = argmax(ir_l) as f32;
        assert!(
            (got - itd).abs() <= 3.0,
            "contralateral left ear delayed {got} vs {itd}"
        );
        // Elevation notch: the el = +60° IR has a deep null near its notch
        // center; the el = 0° IR (no notch) does not.
        let el60_idx = ds
            .elevations()
            .iter()
            .position(|e| (*e - 60.0).abs() < 1e-3)
            .unwrap();
        let f_notch = elevation_notch_hz(60f32.to_radians());
        let h_el0 = dft_magnitude_at(ds.ir(az_idx, el_idx, Ear::Right), f_notch, 48_000.0);
        let h_el60 = dft_magnitude_at(ds.ir(az_idx, el60_idx, Ear::Right), f_notch, 48_000.0);
        assert!(
            h_el60 < h_el0 * 0.6,
            "notch at el 60 ({h_el60}) vs el 0 ({h_el0})"
        );
    }

    #[test]
    fn bilinear_is_exact_at_grid_points_and_blends_between() {
        let ds = HrtfDataset::synthetic(48_000, 64, 15.0, 15.0);
        let mut out = [0.0f32; 64];
        let el0 = ds.elevations().iter().position(|e| e.abs() < 1e-3).unwrap();
        let el15 = ds
            .elevations()
            .iter()
            .position(|e| (*e - 15.0).abs() < 1e-3)
            .unwrap();
        let el30 = ds
            .elevations()
            .iter()
            .position(|e| (*e - 30.0).abs() < 1e-3)
            .unwrap();
        // Exact at a grid point (az 0°, el 0°).
        ds.bilinear_interpolate(0.0, 0.0, Ear::Left, &mut out);
        assert_eq!(out, ds.ir(0, el0, Ear::Left));
        // Midpoint between az 0 and 15 at el 0: 50/50 blend.
        ds.bilinear_interpolate(7.5, 0.0, Ear::Left, &mut out);
        let a = ds.ir(0, el0, Ear::Left);
        let b = ds.ir(1, el0, Ear::Left);
        for k in 0..64 {
            let want = 0.5 * a[k] + 0.5 * b[k];
            assert!((out[k] - want).abs() < 1e-4, "midpoint tap {k}");
        }
        // Bilinear in elevation too: 50/50 between the 0° and 15° rows.
        ds.bilinear_interpolate(0.0, 7.5, Ear::Left, &mut out);
        let e0 = ds.ir(0, el0, Ear::Left);
        let e1 = ds.ir(0, el15, Ear::Left);
        for k in 0..64 {
            let want = 0.5 * e0[k] + 0.5 * e1[k];
            assert!((out[k] - want).abs() < 1e-4, "elevation blend tap {k}");
        }
        // A +30°-only blend differs from the horizontal rows (the notch
        // moved): bilinear(0, 30) = the el-30 grid IR exactly.
        ds.bilinear_interpolate(0.0, 30.0, Ear::Left, &mut out);
        assert_eq!(out, ds.ir(0, el30, Ear::Left));
    }

    #[test]
    fn bilinear_wraps_the_azimuth_continuously() {
        let ds = HrtfDataset::synthetic(48_000, 64, 15.0, 15.0);
        let mut a = [0.0f32; 64];
        let mut b = [0.0f32; 64];
        // −0.1° and 359.9° are the same direction (wrap), and both sit
        // between the last grid azimuth (345°) and the first (0°).
        ds.bilinear_interpolate(-0.1, 0.0, Ear::Left, &mut a);
        ds.bilinear_interpolate(359.9, 0.0, Ear::Left, &mut b);
        for k in 0..64 {
            assert!((a[k] - b[k]).abs() < 1e-6, "wrap tap {k}");
        }
        let el0 = ds.elevations().iter().position(|e| e.abs() < 1e-3).unwrap();
        let last = ds.azimuths().len() - 1;
        let lo = ds.ir(last, el0, Ear::Left);
        let hi = ds.ir(0, el0, Ear::Left);
        // 359.9 is 0.0667° before 0: mostly the last grid IR, slightly the
        // first.
        let f = (359.9 - ds.azimuths()[last]) / (360.0 - ds.azimuths()[last]);
        for k in 0..64 {
            let want = lo[k] + f * (hi[k] - lo[k]);
            assert!((b[k] - want).abs() < 1e-4, "wrap blend tap {k}");
        }
    }

    #[test]
    fn dataset_validation_rejects_bad_grids() {
        assert!(HrtfDataset::from_planes(vec![], vec![0.0], 16, vec![0.0; 32]).is_err());
        assert!(HrtfDataset::from_planes(vec![0.0, 0.0], vec![0.0], 16, vec![0.0; 64]).is_err());
        assert!(HrtfDataset::from_planes(vec![0.0, 15.0], vec![0.0], 16, vec![0.0; 63]).is_err());
        assert!(
            HrtfDataset::from_planes(vec![0.0, 15.0], vec![0.0], 16, vec![f32::NAN; 64]).is_err()
        );
        // Valid grid passes.
        assert!(
            HrtfDataset::from_planes(vec![0.0, 15.0], vec![0.0, 15.0], 16, vec![0.0; 128]).is_ok()
        );
    }
}
