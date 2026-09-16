//! Binaural head model — Woodworth ITD + Duda-Martens head shadow (spec §47–48, §62).
//!
//! Submodules:
//! - [`quality`]: Quality tiers and convolution strategy selection (Point 20).
//! - [`corpus`]: Corpus data structures, JSON I/O, and mesh type classification (Point 21).
//! - [`interpolate`]: Spherical triangulation and barycentric interpolation (Point 18).
//! - [`decompose`]: ITD, minimum phase, and excess phase decomposition (Point 19).
//! - [`dataset`]: Preallocated storage, synthetic generation, and multi-mesh runtime interpolation.

pub mod corpus;
pub mod dataset;
pub mod decompose;
pub mod interpolate;
pub mod profile;
pub mod quality;

pub use corpus::{
    load_hrtf_corpus_json, resample_impulse, save_hrtf_corpus_json, HrtfCorpus, HrtfLoadError,
    HrtfLoadOptions, HrtfMeasurement, HrtfMeshKind, HrtfNormalize,
};
pub use dataset::{Ear, HrtfDataset};
pub use decompose::{
    decompose_corpus, decompose_single_ir, detect_onset_samples, excess_phase_from_ir, extract_itd,
    minimum_phase_from_ir, reconstruct_ir, HrirComponents,
};
pub use interpolate::{
    barycentric_sphere, triangulate_sphere, SphericalHrtfInterpolator, SphericalTriangle,
};
pub use profile::{
    AnthropometricMetadata, HrtfInterpolationMethod, HrtfLatencyAlignment, HrtfPersonalization,
    HrtfPhaseMode, HrtfProfile, HrtfProfileManager, HrtfQualityMetrics, HrtfSubjectInfo,
    HrtfSubjectType,
};
pub use quality::{HrtfConvStrategy, HrtfQualityMode, DEFAULT_HRTF_TAPS, MAX_HRTF_TAPS};

/// Default head radius (m) — the classic 8.75 cm half-head-width.
pub const DEFAULT_HEAD_RADIUS: f32 = 0.0875;

/// Default speed of sound (m/s).
pub const DEFAULT_SPEED_OF_SOUND: f32 = 343.0;

/// Woodworth ITD magnitude (seconds) for a source `θ` off the straight-ahead axis.
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

/// The ITD delay (seconds) applied to `ear`'s signal for a source at `azimuth`.
pub fn ear_delay_sec(azimuth: f32, ear: Ear, head_radius: f32, speed: f32) -> f32 {
    let on_ear_side = azimuth.sin().signum() * ear.side() >= 0.0;
    if on_ear_side {
        0.0
    } else {
        woodworth_itd_sec(azimuth, head_radius, speed)
    }
}

/// Duda-Martens head-shadow coefficient `α` for `ear`: `1.05 + 0.95·sin(φ)`.
#[inline]
pub fn head_shadow_alpha(azimuth: f32, ear: Ear) -> f32 {
    1.05 + 0.95 * (azimuth * ear.side()).sin()
}

/// The maximum Woodworth ITD over all azimuths (seconds): `(a/c)(π/2 + 1)`.
pub fn max_itd_sec(head_radius: f32, speed: f32) -> f32 {
    let a = head_radius.max(0.05);
    let c = speed.max(1.0);
    a / c * (std::f32::consts::FRAC_PI_2 + 1.0)
}

/// A Duda-Martens head-shadow shelf (first-order, bilinear-transformed).
#[derive(Debug, Clone, Copy)]
pub struct HeadShadow {
    alpha: f32,
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

    /// Set up sample-rate constant from `f₀ = c/(2πa)`.
    pub fn prepare(&mut self, sample_rate: f32, head_radius: f32, speed: f32) {
        let f0 = speed.max(1.0) / (std::f32::consts::TAU * head_radius.max(0.05));
        self.k = sample_rate.max(1.0) / (std::f32::consts::PI * f0);
        self.set_alpha(self.alpha);
    }

    fn set_alpha(&mut self, a: f32) {
        self.alpha = a;
        let k = self.k;
        let d = 1.0 + k;
        self.b0 = (1.0 + a * k) / d;
        self.b1 = (1.0 - a * k) / d;
        self.a1 = (1.0 - k) / d;
    }

    /// Current smoothed HF asymptote.
    pub fn alpha(&self) -> f32 {
        self.alpha
    }

    /// Advance one-pole smoothing toward `target`.
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

/// Fractional (linearly interpolated) read from a ring buffer.
#[inline]
pub fn read_delayed(ring: &[f32], cursor: usize, delay_samples: f32, len: usize) -> f32 {
    let l = len.max(2) as f32;
    let d = delay_samples.clamp(0.0, l - 1.0);
    let i = d.floor() as usize;
    let f = d - i as f32;
    let a_idx = (cursor + len - i) % len;
    let b_idx = if a_idx == 0 { len - 1 } else { a_idx - 1 };
    let a = ring[a_idx];
    let b = ring[b_idx];
    a + f * (b - a)
}

/// Pinna-notch center frequency (Hz) for an elevation in radians.
pub fn elevation_notch_hz(elevation_rad: f32) -> f32 {
    (6000.0 + 4000.0 * elevation_rad.sin()).clamp(1500.0, 11_000.0)
}

/// Pinna-notch depth (dB) for an elevation in radians.
pub fn elevation_notch_depth_db(elevation_rad: f32) -> f32 {
    -8.0 * elevation_rad.sin().abs()
}

/// A pinna-notch biquad (RBJ peaking-EQ form with negative gain).
#[derive(Debug, Clone, Copy)]
pub struct ElevationNotch {
    active: bool,
    freq: f32,
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

    pub fn prepare(&mut self, sample_rate: f32) {
        self.fs = sample_rate.max(1.0);
        self.set_target(0.0, 1.0);
    }

    pub fn set_target(&mut self, elevation_rad: f32, smooth: f32) {
        let depth_db = elevation_notch_depth_db(elevation_rad);
        let freq = elevation_notch_hz(elevation_rad);
        if depth_db.abs() < 1e-4 {
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

    pub fn active(&self) -> bool {
        self.active
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};

    const EPS: f32 = 1e-4;

    #[test]
    fn woodworth_values_pin_closed_form() {
        assert!(woodworth_itd_sec(0.0, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND).abs() < EPS);
        let max = max_itd_sec(DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        let at_ear = woodworth_itd_sec(FRAC_PI_2, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
        assert!((at_ear - max).abs() < EPS);
        assert!((max - 0.0875 / 343.0 * (FRAC_PI_2 + 1.0)).abs() < 1e-6);
        assert!(woodworth_itd_sec(PI, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND).abs() < EPS);
        assert!(
            woodworth_itd_sec(FRAC_PI_4, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND)
                - 0.0875 / 343.0 * (FRAC_PI_4.sin() + FRAC_PI_4).abs()
                < 1e-7
        );
        let mut prev = 0.0f32;
        for deg in (0..=90).step_by(10) {
            let itd = woodworth_itd_sec(
                deg as f32 * PI / 180.0,
                DEFAULT_HEAD_RADIUS,
                DEFAULT_SPEED_OF_SOUND,
            );
            assert!(itd >= prev - 1e-7);
            prev = itd;
        }
    }

    #[test]
    fn ear_delay_contralateral_only() {
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
    }

    #[test]
    fn head_shadow_alpha_values() {
        assert!((head_shadow_alpha(0.0, Ear::Left) - 1.05).abs() < EPS);
        assert!((head_shadow_alpha(0.0, Ear::Right) - 1.05).abs() < EPS);
        assert!((head_shadow_alpha(FRAC_PI_2, Ear::Right) - 2.0).abs() < EPS);
        assert!((head_shadow_alpha(FRAC_PI_2, Ear::Left) - 0.1).abs() < EPS);
    }

    #[test]
    fn shelf_dc_gain_is_unity() {
        for &alpha in &[0.1f32, 1.05, 2.0] {
            let mut sh = HeadShadow::new();
            sh.prepare(48_000.0, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
            sh.set_target(alpha, 1.0);
            let mut y = 0.0f32;
            for _ in 0..200 {
                y = sh.process(1.0);
            }
            assert!((y - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    fn elevation_notch_passthrough_at_zero_elevation() {
        let mut n = ElevationNotch::new();
        n.prepare(48_000.0);
        n.set_target(0.0, 1.0);
        assert!(!n.active());
        for x in [1.0f32, -0.5, 0.25] {
            assert_eq!(n.process(x), x);
        }
    }

    #[test]
    fn read_delayed_interpolates_linearly() {
        let ring = [0.0f32, 10.0, 20.0, 30.0];
        assert!((read_delayed(&ring, 3, 1.5, 4) - 15.0).abs() < 1e-6);
        assert!((read_delayed(&ring, 3, 0.0, 4) - 30.0).abs() < 1e-6);
    }

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
            mesh_hint: None,
        };
        let dir = std::env::temp_dir().join("shadow_hrtf_corpus_test_mod.json");
        save_hrtf_corpus_json(&dir, &corpus).expect("saves");
        let loaded = load_hrtf_corpus_json(&dir).expect("loads");
        let _ = std::fs::remove_file(&dir);
        assert_eq!(loaded.sample_rate, corpus.sample_rate);
        assert_eq!(loaded.measurements.len(), 4);
    }

    #[test]
    fn irregular_corpus_loads_and_interpolates() {
        // 3 non-Cartesian measurements
        let corpus = HrtfCorpus {
            sample_rate: 48_000,
            source: Some("irregular-test".into()),
            measurements: vec![
                meas(0.0, 0.0, 16, 1.0),
                meas(90.0, 0.0, 16, 2.0),
                meas(45.0, 45.0, 16, 3.0),
            ],
            mesh_hint: Some(HrtfMeshKind::IrregularMesh),
        };
        let ds = HrtfDataset::from_corpus(
            &corpus,
            &HrtfLoadOptions {
                taps: 16,
                target_sample_rate: 48_000,
                normalize: HrtfNormalize::None,
            },
        )
        .expect("irregular corpus loads via spherical interpolator");

        assert_eq!(ds.mesh_kind(), HrtfMeshKind::IrregularMesh);
        let mut out = [0.0f32; 16];
        ds.bilinear_interpolate(45.0, 20.0, Ear::Left, &mut out);
        for &val in &out {
            assert!(val.is_finite());
        }
    }
}
