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

use super::math::Vec3;

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
    // Phase 22: one modulo instead of two — `b` is one ring slot behind `a`
    // (mod `len`), so the wrap is a branch, not a division.
    let a_idx = (cursor + len - i) % len;
    let b_idx = if a_idx == 0 { len - 1 } else { a_idx - 1 };
    let a = ring[a_idx];
    let b = ring[b_idx];
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

/// A single measured head-related impulse response: the (unit) direction of
/// the source in the layer's coordinate system (`+X` right, `+Y` front,
/// `+Z` up) and the left / right ear impulse responses at the corpus's
/// recorded sample rate. Directions and IRs are 3D/audio vectors in the
/// layer's purely-cartesian convention — this is the data model a `.sofa`
/// HDF5 export is reduced to before loading (SOFA's `SourcePosition` grid +
/// `Data.IR` / `Data.SamplingRate`).
#[derive(Debug, Clone, PartialEq)]
pub struct HrtfMeasurement {
    /// Unit source direction `[x, y, z]` (`+X` right, `+Y` front, `+Z` up).
    pub direction: [f32; 3],
    /// Left-ear impulse response, time-ordered, at
    /// [`HrtfCorpus::sample_rate`].
    pub left: Vec<f32>,
    /// Right-ear impulse response, time-ordered, at
    /// [`HrtfCorpus::sample_rate`].
    pub right: Vec<f32>,
}

/// A measured HRTF corpus (SOFA-style): the set of measurement directions
/// with their paired impulse responses plus the recording rate and optional
/// provenance. Hosts build this from a measured corpus (or load it from the
/// simple JSON form via [`load_hrtf_corpus_json`]) and hand it to
/// [`HrtfDataset::from_corpus`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HrtfCorpus {
    /// Sample rate the IRs were recorded at (e.g. 44.1 / 48 kHz).
    pub sample_rate: u32,
    /// Optional provenance / corpus name (CIPIC, TU-Berlin, KEMAR, …).
    pub source: Option<String>,
    /// The measurements.
    pub measurements: Vec<HrtfMeasurement>,
}

/// Optional normalization applied to each (left, right) ear pair when
/// loading a corpus —— many raw measured HRTFs are unnormalized; a single
/// peak normalization is the standard first step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HrtfNormalize {
    /// No normalization — keep the raw sample amplitudes.
    #[default]
    None,
    /// Divide every ear pair by its overall peak magnitude (unity peak).
    Peak,
}

/// Controls [`HrtfDataset::from_corpus`]: tap length, target sample rate
/// (resamples the corpus if it differs), and optional normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HrtfLoadOptions {
    /// Impulse-response length in taps (≤ [`MAX_HRTF_TAPS`]); shorter IRs
    /// are zero-padded, longer ones truncated.
    pub taps: usize,
    /// Sample rate to render at; IRs are resampled to this rate if
    /// `corpus.sample_rate` differs.
    pub target_sample_rate: u32,
    /// Optional peak normalization.
    pub normalize: HrtfNormalize,
}

impl Default for HrtfLoadOptions {
    fn default() -> Self {
        Self {
            taps: 64,
            target_sample_rate: 48_000,
            normalize: HrtfNormalize::None,
        }
    }
}

/// Errors from loading a measured HRTF corpus (spec §62 seam). Every failure
/// is a typed error — an invalid corpus must never reach the audio thread.
#[derive(Debug, Clone, PartialEq)]
pub enum HrtfLoadError {
    /// The corpus has no measurements (or no grid after projection).
    Empty,
    /// The impulse-response length is outside the supported range.
    Taps { got: usize, max: usize },
    /// A measurement direction is not a finite unit vector.
    DirectionNonFinite,
    /// The measurement directions do not form a regular `azimuth × elevation`
    /// full Cartesian product (the mesh is irregular), so bilinear
    /// interpolation would be ill-defined — refuse rather than interpolate
    /// wrongly.
    IrregularMesh,
    /// A raw IR contains a non-finite sample.
    NonFiniteIr,
    /// Corpus JSON I/O or parsing failed.
    Json(String),
}

impl std::fmt::Display for HrtfLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HrtfLoadError::Empty => write!(f, "hrtf corpus: no measurements"),
            HrtfLoadError::Taps { got, max } => {
                write!(f, "hrtf corpus: taps {got} out of range (max {max})")
            }
            HrtfLoadError::DirectionNonFinite => {
                write!(f, "hrtf corpus: non-finite or zero measurement direction")
            }
            HrtfLoadError::IrregularMesh => write!(
                f,
                "hrtf corpus: measurement directions are not a regular azimuth × elevation grid",
            ),
            HrtfLoadError::NonFiniteIr => write!(f, "hrtf corpus: non-finite IR sample"),
            HrtfLoadError::Json(e) => write!(f, "hrtf corpus json: {e}"),
        }
    }
}

impl std::error::Error for HrtfLoadError {}

/// Resample a mono impulse response from `src_rate` to `dst_rate` by
/// piecewise-linear interpolation of the samples (adequate for impulse
/// responses; the renderer'll re-convolve them at block rate). `dst_rate`
/// and `src_rate` are ≥ 1. Allocation-free per sample; returns a new vector
/// of length `ceil(len(src)·dst/src)`.
pub(crate) fn resample_impulse(src: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    let (sr, dr) = (src_rate.max(1) as f64, dst_rate.max(1) as f64);
    if (sr - dr).abs() < 1e-9 {
        return src.to_vec();
    }
    let out_len = ((src.len() as f64) * dr / sr).ceil().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let t = n as f64 * sr / dr;
        let i = t.floor() as usize;
        let frac = (t - i as f64) as f32;
        let a = src.get(i).copied().unwrap_or(0.0);
        let b = src.get(i + 1).copied().unwrap_or(0.0);
        out.push(a + frac * (b - a));
    }
    out
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

    /// Build a dataset from a **measured corpus** (SOFA-style): a set of
    /// measurement directions each carrying a left/right impulse response at
    /// `corpus.sample_rate`. This is the seam hosts use to feed real
    /// head-related impulse responses (e.g. from a CIPIC / TU-Berlin / KEMAR
    /// corpus exported out of a `.sofa` HDF5 file into the simple
    /// [`HrtfCorpus`] data model) into the renderer, replacing the synthetic
    /// generator.
    ///
    /// On the control path it: validates every measurement (finite unit
    /// direction, finite non-empty IRs), resamples each IR to
    /// `options.target_sample_rate` if the corpus was recorded at a different
    /// rate (piecewise-linear, allocation-free per call), trims/pads each to
    /// `options.taps` (≤ [`MAX_HRTF_TAPS`]), optionally peak-normalizes each
    /// ear pair, then groups the directions into the regular
    /// `azimuths × elevations` grid the renderer interpolates. The measurement
    /// directions must form a **full Cartesian product** of the distinct
    /// azimuths and elevations present (the common regular SOFA/corpus mesh);
    /// an irregular mesh returns [`HrtfLoadError::IrregularMesh`] with the
    /// offending detail rather than quietly interpolating a wrong grid.
    pub fn from_corpus(
        corpus: &HrtfCorpus,
        options: &HrtfLoadOptions,
    ) -> Result<Self, HrtfLoadError> {
        if corpus.measurements.is_empty() {
            return Err(HrtfLoadError::Empty);
        }
        let taps = options.taps;
        if taps == 0 || taps > MAX_HRTF_TAPS {
            return Err(HrtfLoadError::Taps {
                got: taps,
                max: MAX_HRTF_TAPS,
            });
        }
        let target_rate = options.target_sample_rate.max(1);
        let src_rate = corpus.sample_rate.max(1);
        let need_resample = src_rate != target_rate;

        // Collect the distinct azimuth/elevation grid values (snap float
        // forms of the direction to degrees in the renderer convention).
        let mut azimuths: Vec<f32> = Vec::new();
        let mut elevations: Vec<f32> = Vec::new();
        for m in &corpus.measurements {
            let d = Vec3::new(m.direction[0], m.direction[1], m.direction[2]);
            let dn = d.normalized().ok_or(HrtfLoadError::DirectionNonFinite)?;
            let az = dn.azimuth_rad().to_degrees().rem_euclid(360.0);
            let el = dn.elevation_rad().to_degrees();
            if !az.is_finite() || !el.is_finite() {
                return Err(HrtfLoadError::DirectionNonFinite);
            }
            if let Some(a) = azimuths.iter().find(|&&x| (x - az).abs() < 1e-3) {
                let _ = a;
            } else {
                azimuths.push(az);
            }
            if !elevations.iter().any(|&x| (x - el).abs() < 1e-3) {
                elevations.push(el);
            }
        }
        azimuths.sort_by(|a, b| a.total_cmp(b));
        elevations.sort_by(|a, b| a.total_cmp(b));
        if azimuths.is_empty() || elevations.is_empty() {
            return Err(HrtfLoadError::Empty);
        }
        // Verify a full Cartesian product (regular mesh) so bilinear
        // interpolation is exact: each (az, el) pair must appear exactly once.
        let mut seen = std::collections::HashSet::with_capacity(corpus.measurements.len());
        let mut slabs: Vec<Vec<(Vec<f32>, Vec<f32>)>> = Vec::new();
        for m in &corpus.measurements {
            let d = Vec3::new(m.direction[0], m.direction[1], m.direction[2]);
            let dn = d.normalized().ok_or(HrtfLoadError::DirectionNonFinite)?;
            let az = dn.azimuth_rad().to_degrees().rem_euclid(360.0);
            let el = dn.elevation_rad().to_degrees();
            let ia = azimuths
                .iter()
                .position(|&x| (x - az).abs() < 1e-3)
                .ok_or(HrtfLoadError::IrregularMesh)?;
            let ie = elevations
                .iter()
                .position(|&x| (x - el).abs() < 1e-3)
                .ok_or(HrtfLoadError::IrregularMesh)?;
            let key = ia * elevations.len() + ie;
            if !seen.insert(key) {
                return Err(HrtfLoadError::IrregularMesh);
            }
            while slabs.len() <= ia {
                slabs.push(Vec::new());
            }
            while slabs[ia].len() <= ie {
                slabs[ia].push((Vec::new(), Vec::new()));
            }
            let (left, right) = if need_resample {
                (
                    resample_impulse(&m.left, src_rate, target_rate),
                    resample_impulse(&m.right, src_rate, target_rate),
                )
            } else {
                (m.left.clone(), m.right.clone())
            };
            slabs[ia][ie] = (left, right);
        }
        drop(seen);
        if slabs.iter().any(|col| col.len() != elevations.len()) {
            return Err(HrtfLoadError::IrregularMesh);
        }

        // Normalize (optional) then trim/pad to `taps` and emit flat storage.
        let mut irs: Vec<f32> =
            Vec::with_capacity(azimuths.len() * elevations.len() * Ear::COUNT * taps);
        for &az in &azimuths {
            let ia = azimuths
                .iter()
                .position(|&x| (x - az).abs() < 1e-3)
                .unwrap();
            for &el in &elevations {
                let ie = elevations
                    .iter()
                    .position(|&x| (x - el).abs() < 1e-3)
                    .unwrap();
                let (l, r) = slabs[ia][ie].clone();
                let (mut left, mut right) = (l, r);
                if left.len() > taps {
                    left.truncate(taps);
                } else {
                    left.resize(taps, 0.0);
                }
                if right.len() > taps {
                    right.truncate(taps);
                } else {
                    right.resize(taps, 0.0);
                }
                if matches!(options.normalize, HrtfNormalize::Peak) {
                    let peak = left
                        .iter()
                        .chain(right.iter())
                        .fold(0.0f32, |m, v| m.max(v.abs()))
                        .max(1e-12);
                    for v in left.iter_mut() {
                        *v /= peak;
                    }
                    for v in right.iter_mut() {
                        *v /= peak;
                    }
                }
                if left.iter().any(|v| !v.is_finite()) || right.iter().any(|v| !v.is_finite()) {
                    return Err(HrtfLoadError::NonFiniteIr);
                }
                irs.extend_from_slice(&left);
                irs.extend_from_slice(&right);
            }
        }
        Ok(HrtfDataset {
            azimuths,
            elevations,
            irs,
            taps,
        })
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

/// Save a measured HRTF corpus to a compact JSON file — the portable,
/// pure-Rust interchange form of a `.sofa` HDF5 export (positions + IR
/// grids + rate). Hosts use [`load_hrtf_corpus_json`] to read it back. The
/// engine deliberately avoids HDF5 bindings (not pure Rust); the JSON model
/// carries the same data SOFA's `SourcePosition` / `Data.IR` /
/// `Data.SamplingRate` do.
pub fn save_hrtf_corpus_json(
    path: &std::path::Path,
    corpus: &HrtfCorpus,
) -> Result<(), HrtfLoadError> {
    use serde_json::{json, Value};
    let measurements: Vec<Value> = corpus
        .measurements
        .iter()
        .map(|m| {
            let left: Vec<f64> = m.left.iter().map(|&v| v as f64).collect();
            let right: Vec<f64> = m.right.iter().map(|&v| v as f64).collect();
            json!({ "direction": m.direction, "left": left, "right": right })
        })
        .collect();
    let root = json!({ "sample_rate": corpus.sample_rate, "source": corpus.source, "measurements": measurements });
    let bytes = serde_json::to_vec_pretty(&root).map_err(|e| HrtfLoadError::Json(e.to_string()))?;
    std::fs::write(path, bytes).map_err(|e| HrtfLoadError::Json(e.to_string()))
}

/// Load a measured HRTF corpus from a JSON file written by
/// [`save_hrtf_corpus_json`] (or produced by a `.sofa` → JSON exporter). The
/// corpus is only data; run it through [`HrtfDataset::from_corpus`] to
/// validate and build the renderable grid.
pub fn load_hrtf_corpus_json(path: &std::path::Path) -> Result<HrtfCorpus, HrtfLoadError> {
    use serde_json::Value;
    let text = std::fs::read_to_string(path).map_err(|e| HrtfLoadError::Json(e.to_string()))?;
    let root: Value =
        serde_json::from_str(&text).map_err(|e| HrtfLoadError::Json(e.to_string()))?;
    let sample_rate = root
        .get("sample_rate")
        .and_then(Value::as_u64)
        .map(|r| r as u32)
        .ok_or_else(|| HrtfLoadError::Json("missing sample_rate".into()))?;
    let source = root
        .get("source")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let ms = root
        .get("measurements")
        .and_then(Value::as_array)
        .ok_or_else(|| HrtfLoadError::Json("missing measurements array".into()))?;
    let mut measurements = Vec::with_capacity(ms.len());
    for (i, m) in ms.iter().enumerate() {
        let dir = m
            .get("direction")
            .and_then(Value::as_array)
            .ok_or_else(|| HrtfLoadError::Json(format!("measurement {i}: missing direction")))?;
        if dir.len() != 3 {
            return Err(HrtfLoadError::Json(format!(
                "measurement {i}: direction not 3D"
            )));
        }
        let direction = [
            dir[0].as_f64().unwrap_or(0.0) as f32,
            dir[1].as_f64().unwrap_or(0.0) as f32,
            dir[2].as_f64().unwrap_or(0.0) as f32,
        ];
        let left = m
            .get("left")
            .and_then(Value::as_array)
            .ok_or_else(|| HrtfLoadError::Json(format!("measurement {i}: missing left")))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();
        let right = m
            .get("right")
            .and_then(Value::as_array)
            .ok_or_else(|| HrtfLoadError::Json(format!("measurement {i}: missing right")))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();
        measurements.push(HrtfMeasurement {
            direction,
            left,
            right,
        });
    }
    Ok(HrtfCorpus {
        sample_rate,
        source,
        measurements,
    })
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

    /// Unit direction for `(azimuth, elevation)` in the layer's convention:
    /// `+Y` front, `+X` right, `+Z` up.
    fn unit(az_deg: f32, el_deg: f32) -> [f32; 3] {
        let az = az_deg.to_radians();
        let el = el_deg.to_radians();
        let horiz = el.cos();
        [az.sin() * horiz, az.cos() * horiz, el.sin()]
    }

    fn meas(az: f32, el: f32, taps: usize, seed: f32) -> HrtfMeasurement {
        let lower = (seed * 0.01 + 1.0).abs().max(1e-3);
        HrtfMeasurement {
            direction: unit(az, el),
            left: (0..taps).map(|i| lower + i as f32 * 0.001).collect(),
            right: (0..taps).map(|i| lower + i as f32 * 0.0015 + 0.1).collect(),
        }
    }

    #[test]
    fn from_corpus_builds_grid_and_resamples() {
        // az {0, 90} × el {0, 45} — a full regular product at 96 kHz.
        let corpus = HrtfCorpus {
            sample_rate: 96_000,
            source: Some("test-corpus".into()),
            measurements: vec![
                meas(0.0, 0.0, 128, 1.0),
                meas(90.0, 0.0, 128, 2.0),
                meas(0.0, 45.0, 128, 3.0),
                meas(90.0, 45.0, 128, 4.0),
            ],
        };
        let ds = HrtfDataset::from_corpus(
            &corpus,
            &HrtfLoadOptions {
                taps: 32,
                target_sample_rate: 48_000,
                normalize: HrtfNormalize::None,
            },
        )
        .expect("regular corpus loads");
        assert_eq!(ds.azimuths().len(), 2);
        assert_eq!(ds.elevations().len(), 2);
        assert_eq!(ds.taps(), 32);
        // The resampled rate halves the tap count; taps are then trimmed to 32.
        assert!((ds.azimuths()[0].abs()) < 1e-3);
        assert!((ds.azimuths()[1] - 90.0).abs() < 1e-3);
        assert!((ds.elevations()[0].abs()) < 1e-3);
        assert!((ds.elevations()[1] - 45.0).abs() < 1e-3);
        // Every IR is exactly `taps` long and finite.
        for ia in 0..2 {
            for ie in 0..2 {
                for ear in [Ear::Left, Ear::Right] {
                    assert_eq!(ds.ir(ia, ie, ear).len(), 32);
                    assert!(ds.ir(ia, ie, ear).iter().all(|v| v.is_finite()));
                }
            }
        }
        // The loaded dataset interpolates exactly at its grid points.
        let mut out = [0.0f32; 32];
        ds.bilinear_interpolate(90.0, 45.0, Ear::Left, &mut out);
        let expect = ds.ir(1, 1, Ear::Left);
        for (o, e) in out.iter().zip(expect) {
            assert!((o - e).abs() < 1e-4);
        }
    }

    #[test]
    fn from_corpus_peak_normalizes() {
        let corpus = HrtfCorpus {
            sample_rate: 48_000,
            source: None,
            measurements: vec![
                meas(0.0, 0.0, 16, 4.0),
                meas(90.0, 0.0, 16, 5.0),
                meas(0.0, 45.0, 16, 6.0),
                meas(90.0, 45.0, 16, 7.0),
            ],
        };
        let ds = HrtfDataset::from_corpus(
            &corpus,
            &HrtfLoadOptions {
                taps: 16,
                target_sample_rate: 48_000,
                normalize: HrtfNormalize::Peak,
            },
        )
        .expect("loads");
        let mut peak = 0.0f32;
        for ia in 0..2 {
            for ie in 0..2 {
                for ear in [Ear::Left, Ear::Right] {
                    for &v in ds.ir(ia, ie, ear) {
                        peak = peak.max(v.abs());
                    }
                }
            }
        }
        assert!((peak - 1.0).abs() < 1e-3, "unity peak, got {peak}");
    }

    #[test]
    fn from_corpus_rejects_irregular_mesh() {
        // Three of the four product points, plus a lone measurement at an az
        // (180°, i.e. direction [-1, 0, 0]) that breaks the product.
        let mut corpus = HrtfCorpus {
            sample_rate: 48_000,
            source: None,
            measurements: vec![
                meas(0.0, 0.0, 16, 1.0),
                meas(90.0, 0.0, 16, 2.0),
                meas(0.0, 45.0, 16, 3.0),
                meas(180.0, 0.0, 16, 4.0),
            ],
        };
        assert!(matches!(
            HrtfDataset::from_corpus(&corpus, &HrtfLoadOptions::default()),
            Err(HrtfLoadError::IrregularMesh)
        ));
        // Missing a product point is also irregular.
        corpus.measurements.truncate(3);
        assert!(matches!(
            HrtfDataset::from_corpus(&corpus, &HrtfLoadOptions::default()),
            Err(HrtfLoadError::IrregularMesh)
        ));
    }

    #[test]
    fn from_corpus_rejects_empty_and_bad_taps() {
        let empty = HrtfCorpus {
            sample_rate: 48_000,
            source: None,
            measurements: vec![],
        };
        assert!(matches!(
            HrtfDataset::from_corpus(&empty, &HrtfLoadOptions::default()),
            Err(HrtfLoadError::Empty)
        ));
        let bad_taps = HrtfCorpus {
            sample_rate: 48_000,
            source: None,
            measurements: vec![meas(0.0, 0.0, 16, 1.0)],
        };
        let opts = HrtfLoadOptions {
            taps: 0,
            ..HrtfLoadOptions::default()
        };
        assert!(matches!(
            HrtfDataset::from_corpus(&bad_taps, &opts),
            Err(HrtfLoadError::Taps { .. })
        ));
    }

    #[test]
    fn corpus_json_round_trip() {
        let corpus = HrtfCorpus {
            sample_rate: 48_000,
            source: Some("json-corpus".into()),
            measurements: vec![
                meas(0.0, 0.0, 8, 1.0),
                meas(90.0, 0.0, 8, 2.0),
                meas(0.0, 45.0, 8, 3.0),
                meas(90.0, 45.0, 8, 5.0),
            ],
        };
        let dir = std::env::temp_dir().join("shadow_hrtf_corpus_test.json");
        save_hrtf_corpus_json(&dir, &corpus).expect("saves");
        let loaded = load_hrtf_corpus_json(&dir).expect("loads");
        let _ = std::fs::remove_file(&dir);
        assert_eq!(loaded.sample_rate, corpus.sample_rate);
        assert_eq!(loaded.source, corpus.source);
        assert_eq!(loaded.measurements.len(), 4);
        assert_eq!(
            loaded.measurements[0].direction,
            corpus.measurements[0].direction
        );
        assert_eq!(loaded.measurements[0].left, corpus.measurements[0].left);
        assert_eq!(loaded.measurements[3].right, corpus.measurements[3].right);
        // The loaded corpus feeds from_corpus the same way the original does.
        let a = HrtfDataset::from_corpus(&corpus, &HrtfLoadOptions::default()).expect("orig");
        let b = HrtfDataset::from_corpus(&loaded, &HrtfLoadOptions::default()).expect("loaded");
        assert_eq!(a.azimuths(), b.azimuths());
        assert_eq!(a.elevations(), b.elevations());
    }
}
