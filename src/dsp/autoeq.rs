//! AutoEQ — measurement-to-target EQ generation (§9.3).
//!
//! The pipeline is: **measurement → target → deviation → PEQ bands →
//! preamp**, where the output is an ordinary [`config::EqPreset`] that the
//! existing `ParametricEq::from_preset` path consumes — AutoEQ introduces no
//! new DSP stage.
//!
//! # Determinism (Gate B)
//!
//! [`AutoEq::optimize`] is a pure function of its inputs: the same
//! measurement, target, and parameters always produce byte-identical output.
//! No RNG, no iterative solvers with floating-point convergence, no
//! time-dependent state. The acceptance suite pins this with a golden hash.
//!
//! # Grid model
//!
//! Deviations are computed on a log-spaced frequency grid (default 1/6
//! octave, 20 Hz – 20 kHz → 64 bands — the same ladder `MAX_EQ_BANDS`
//! supports). Each grid band becomes a peaking filter (shelves at the
//! endpoints) with the bandwidth-derived Q; gains are clamped to
//! [`AUTO_EQ_MAX_GAIN_DB`]. The preamp is derived from the *generated*
//! curve's peak combined boost via the EQ's own magnitude estimator, so the
//! limiter never has to absorb the curve.

use config::{EqBandConfig, EqPreset, FilterType};

/// Default analysis grid low edge (Hz).
pub const AUTO_EQ_GRID_F_MIN: f32 = 20.0;
/// Default analysis grid high edge (Hz).
pub const AUTO_EQ_GRID_F_MAX: f32 = 20_000.0;
/// Default grid density: 6 bands per octave (1/6-octave → 64 bands).
pub const AUTO_EQ_BANDS_PER_OCTAVE: f32 = 6.0;
/// Maximum per-band gain the optimizer will emit (dB).
pub const AUTO_EQ_MAX_GAIN_DB: f32 = 15.0;
/// Bands whose |gain| falls below this are emitted disabled (dB).
pub const AUTO_EQ_MIN_BAND_GAIN_DB: f32 = 0.1;
/// Sample rate used for the preamp magnitude estimate.
pub const AUTO_EQ_ESTIMATE_SAMPLE_RATE: f32 = 48_000.0;

/// A target response curve to match against a measurement.
///
/// `Custom` takes (frequency Hz, gain dB) breakpoints, log-interpolated in
/// the frequency axis; `Flat` is a 0 dB reference. The two Harman variants
/// are **documented approximations** of the published 2018 targets (the
/// well-known bass shelf, neutral midrange, and treble shape) — they are
/// deterministic and useful as starting points, but an exact measured target
/// should be supplied via [`TargetCurve::Custom`].
#[derive(Debug, Clone, PartialEq)]
pub enum TargetCurve {
    Flat,
    /// Harman 2018 over-ear target (approximate breakpoints).
    HarmanHeadphone2018,
    /// Harman 2018 IEM target (approximate breakpoints).
    HarmanIem2018,
    /// Explicit (Hz, dB) breakpoints.
    Custom(Vec<(f32, f32)>),
}

impl TargetCurve {
    /// Harman 2018 over-ear target as (Hz, dB) breakpoints. Approximate:
    /// ≈ +6 dB bass shelf below ~100 Hz, neutral midrange, a gentle presence
    /// rise near 3–5 kHz, and a small treble shelf — the shape published by
    /// Olive, Welti & Khould (2017).
    fn harman_over_ear() -> Vec<(f32, f32)> {
        vec![
            (20.0, 6.0),
            (31.5, 6.0),
            (50.0, 5.8),
            (80.0, 5.3),
            (105.0, 5.0),
            (160.0, 3.8),
            (250.0, 2.5),
            (400.0, 1.2),
            (630.0, 0.3),
            (1000.0, 0.0),
            (1600.0, 0.2),
            (2500.0, 1.0),
            (4000.0, 2.0),
            (6300.0, 0.5),
            (8000.0, 0.8),
            (10000.0, 1.5),
            (14000.0, 2.5),
            (20000.0, 1.0),
        ]
    }

    /// Harman 2018 IEM target as (Hz, dB) breakpoints. Approximate: a flatter
    /// bass shelf (~+4 dB to ~250 Hz), neutral midrange, mild presence and a
    /// small treble rise — the shape published by Olive et al. (2016).
    fn harman_iem() -> Vec<(f32, f32)> {
        vec![
            (20.0, 4.0),
            (50.0, 4.0),
            (100.0, 4.0),
            (160.0, 3.5),
            (250.0, 3.0),
            (400.0, 1.8),
            (630.0, 0.8),
            (1000.0, 0.0),
            (1600.0, 0.0),
            (2500.0, 0.5),
            (4000.0, 1.0),
            (6300.0, 0.0),
            (8000.0, 0.5),
            (10000.0, 1.0),
            (14000.0, 1.5),
            (20000.0, 0.5),
        ]
    }

    fn breakpoints(&self) -> Option<Vec<(f32, f32)>> {
        match self {
            Self::Flat => None,
            Self::HarmanHeadphone2018 => Some(Self::harman_over_ear()),
            Self::HarmanIem2018 => Some(Self::harman_iem()),
            Self::Custom(points) => Some(points.clone()),
        }
    }

    /// Target gain in dB at `freq_hz` (log-frequency interpolation between
    /// breakpoints; flat outside the breakpoint range).
    pub fn eval_db(&self, freq_hz: f32) -> f32 {
        let Some(points) = self.breakpoints() else {
            return 0.0;
        };
        interp_db_log(&points, freq_hz)
    }
}

/// Interpolate (Hz, dB) breakpoints in the log-frequency domain. Returns the
/// nearest endpoint value outside the breakpoint range. Non-finite or
/// non-positive input frequencies return the first breakpoint's value.
fn interp_db_log(points: &[(f32, f32)], freq_hz: f32) -> f32 {
    if points.is_empty() || !freq_hz.is_finite() || freq_hz <= 0.0 {
        return points.first().map(|p| p.1).unwrap_or(0.0);
    }
    if freq_hz <= points[0].0 {
        return points[0].1;
    }
    let last = points.last().unwrap();
    if freq_hz >= last.0 {
        return last.1;
    }
    let log_f = freq_hz.ln();
    for w in points.windows(2) {
        let (f0, g0) = w[0];
        let (f1, g1) = w[1];
        if freq_hz >= f0 && freq_hz <= f1 {
            if f1 <= f0 {
                return g0;
            }
            let t = (log_f - f0.ln()) / (f1.ln() - f0.ln());
            return g0 + t * (g1 - g0);
        }
    }
    points[0].1
}

/// A measured frequency response: (frequency Hz, gain dB) points.
///
/// The measurement is stored unsorted and evaluated through
/// log-frequency interpolation ([`Self::eval_db`]); the raw points are kept
/// for diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct FrequencyResponse {
    points: Vec<(f32, f32)>,
}

impl FrequencyResponse {
    /// Build a response, dropping non-finite points and points with
    /// non-positive frequency, then sorting by frequency.
    pub fn new(points: Vec<(f32, f32)>) -> Self {
        let mut clean: Vec<(f32, f32)> = points
            .into_iter()
            .filter(|(f, g)| f.is_finite() && g.is_finite() && *f > 0.0)
            .collect();
        clean.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        Self { points: clean }
    }

    /// Measured gain in dB at `freq_hz` (log interpolation).
    pub fn eval_db(&self, freq_hz: f32) -> f32 {
        interp_db_log(&self.points, freq_hz)
    }

    /// The raw measurement points (sorted, validated).
    pub fn points(&self) -> &[(f32, f32)] {
        &self.points
    }
}

/// Optimizer parameters. `Default` matches the module-level constants.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoEqParams {
    pub target: TargetCurve,
    /// Maximum per-band gain magnitude in dB.
    pub max_gain_db: f32,
    /// Optional deviation smoothing width in octaves (box average in the
    /// log-frequency grid); `None` disables smoothing.
    pub smoothing_octaves: Option<f32>,
    /// Bands with |gain| below this are emitted disabled (dB).
    pub min_band_gain_db: f32,
    /// Analysis grid low edge (Hz).
    pub grid_f_min: f32,
    /// Analysis grid high edge (Hz).
    pub grid_f_max: f32,
    /// Grid density (bands per octave).
    pub bands_per_octave: f32,
    /// Sample rate used for the preamp magnitude estimate.
    pub estimate_sample_rate: f32,
}

impl Default for AutoEqParams {
    fn default() -> Self {
        Self {
            target: TargetCurve::Flat,
            max_gain_db: AUTO_EQ_MAX_GAIN_DB,
            smoothing_octaves: Some(1.0 / 6.0),
            min_band_gain_db: AUTO_EQ_MIN_BAND_GAIN_DB,
            grid_f_min: AUTO_EQ_GRID_F_MIN,
            grid_f_max: AUTO_EQ_GRID_F_MAX,
            bands_per_octave: AUTO_EQ_BANDS_PER_OCTAVE,
            estimate_sample_rate: AUTO_EQ_ESTIMATE_SAMPLE_RATE,
        }
    }
}

/// The deterministic result of an AutoEQ run.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoEqResult {
    /// The generated EQ preset — consumable by `ParametricEq::from_preset`.
    pub preset: EqPreset,
    /// Recommended preamp in dB (negative of the generated curve's peak
    /// combined boost; 0 when the curve only cuts).
    pub preamp_db: f32,
    /// Maximum absolute raw deviation over the grid (dB).
    pub max_deviation_db: f32,
    /// RMS raw deviation over the grid (dB) — the fit's residual error.
    pub rms_error_db: f32,
    /// Generated bands as (frequency Hz, gain dB) for diagnostics.
    pub bands: Vec<(f32, f32)>,
}

/// The AutoEQ optimizer.
pub struct AutoEq;

impl AutoEq {
    /// Compute the EQ preset that matches `measurement` to
    /// `params.target`. Deterministic and allocation-bounded.
    pub fn optimize(
        name: &str,
        measurement: &FrequencyResponse,
        params: &AutoEqParams,
    ) -> AutoEqResult {
        // ── 1. Build the log grid ─────────────────────────────────────────
        let octaves = (params.grid_f_max / params.grid_f_min).max(1.0).log2();
        let n_bands = (octaves * params.bands_per_octave).ceil() as usize;
        let n_bands = n_bands.clamp(1, 64);
        let mut grid_freqs: Vec<f32> = (0..n_bands)
            .map(|i| params.grid_f_min * (2.0f32).powf(i as f32 / params.bands_per_octave))
            .collect();
        // Ensure the top edge is included as the last band.
        grid_freqs[n_bands - 1] = params.grid_f_max.min(20_000.0);

        // ── 2. Deviation = target − measured ─────────────────────────────
        let mut deviation: Vec<f32> = grid_freqs
            .iter()
            .map(|&f| params.target.eval_db(f) - measurement.eval_db(f))
            .collect();

        // ── 3. Optional smoothing (box average in band index space) ──────
        if let Some(width) = params.smoothing_octaves {
            let width = width.max(0.0);
            let half = (width * params.bands_per_octave / 2.0).round() as usize;
            if half > 0 {
                let smoothed = deviation.clone();
                for (i, dev) in deviation.iter_mut().enumerate().take(n_bands) {
                    let lo = i.saturating_sub(half);
                    let hi = (i + half).min(n_bands - 1);
                    let sum: f32 = smoothed[lo..=hi].iter().sum();
                    *dev = sum / (hi - lo + 1) as f32;
                }
            }
        }

        let max_gain = params.max_gain_db.max(0.0);

        // ── 4. Clamp and emit bands ──────────────────────────────────────
        let q = band_q(params.bands_per_octave);
        let last = n_bands - 1;
        let mut bands: Vec<EqBandConfig> = Vec::with_capacity(n_bands);
        let mut generated: Vec<(f32, f32)> = Vec::with_capacity(n_bands);
        for (i, &f) in grid_freqs.iter().enumerate() {
            let g = deviation[i].clamp(-max_gain, max_gain);
            generated.push((f, g));
            bands.push(EqBandConfig {
                enabled: g.abs() >= params.min_band_gain_db,
                filter_type: if i == 0 {
                    FilterType::LowShelf
                } else if i == last {
                    FilterType::HighShelf
                } else {
                    FilterType::Peaking
                },
                frequency: f,
                gain_db: g,
                q,
            });
        }

        // ── 5. Preamp from the generated curve's peak boost ──────────────
        let preset = EqPreset {
            name: name.to_string(),
            output_device_pattern: None,
            preamp_db: 0.0,
            bands: bands.clone(),
        };
        let compiled =
            crate::dsp::equalizer::ParametricEq::from_preset(params.estimate_sample_rate, &preset);
        let peak_boost = compiled.combined_max_gain_db(params.estimate_sample_rate);
        let preamp_db = if peak_boost > 0.0 { -peak_boost } else { 0.0 };
        let preset = EqPreset {
            preamp_db,
            ..preset
        };

        // ── 6. Fit metrics on the RAW deviation ──────────────────────────
        let max_deviation_db = deviation.iter().fold(0.0f32, |acc, d| acc.max(d.abs()));
        let rms_error_db =
            (deviation.iter().map(|d| d * d).sum::<f32>() / deviation.len().max(1) as f32).sqrt();

        AutoEqResult {
            preset,
            preamp_db,
            max_deviation_db,
            rms_error_db,
            bands: generated,
        }
    }
}

/// Bandwidth-derived Q for a grid density of `bands_per_octave` bands.
fn band_q(bands_per_octave: f32) -> f32 {
    let b = 1.0 / bands_per_octave as f64;
    (1.0 / (2.0 * (2.0f64.ln() * b / 2.0).sinh())) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic synthetic measurement with a known +4 dB shelf below
    /// 200 Hz and a flat response above — a clean test target.
    fn shelf_measurement() -> FrequencyResponse {
        FrequencyResponse::new(vec![
            (20.0, -4.0),
            (50.0, -4.0),
            (100.0, -4.0),
            (150.0, -4.0),
            (200.0, -2.0),
            (300.0, -0.5),
            (500.0, 0.0),
            (1000.0, 0.0),
            (2000.0, 0.0),
            (5000.0, 0.0),
            (10000.0, 0.0),
            (20000.0, 0.0),
        ])
    }

    #[test]
    fn flat_measurement_flat_target_is_zero_curve() {
        let m = FrequencyResponse::new(vec![
            (20.0, 0.0),
            (100.0, 0.0),
            (1000.0, 0.0),
            (10000.0, 0.0),
            (20000.0, 0.0),
        ]);
        let result = AutoEq::optimize(
            "flat",
            &m,
            &AutoEqParams {
                target: TargetCurve::Flat,
                smoothing_octaves: None,
                ..Default::default()
            },
        );
        for b in &result.preset.bands {
            assert!(
                b.gain_db.abs() < 1e-3,
                "flat → flat must be 0 dB, got {}",
                b.gain_db
            );
        }
        assert!(result.preamp_db.abs() < 1e-3);
        assert!(result.max_deviation_db < 1e-3);
        assert!(result.rms_error_db < 1e-3);
    }

    #[test]
    fn known_shelf_is_reproduced_within_tolerance() {
        let m = shelf_measurement();
        let result = AutoEq::optimize(
            "shelf",
            &m,
            &AutoEqParams {
                target: TargetCurve::Flat,
                smoothing_octaves: None,
                ..Default::default()
            },
        );
        // The measurement has a +4 dB shelf below ~200 Hz (measured −4, target
        // 0), so the generated low-shelf band should be ≈ +4 dB.
        let shelf = &result.preset.bands[0];
        assert_eq!(shelf.filter_type, FilterType::LowShelf);
        assert!(
            (shelf.gain_db - 4.0).abs() < 0.5,
            "low shelf should be ≈ +4 dB, got {}",
            shelf.gain_db
        );
        // Bands well above the shelf (≳ 640 Hz on the 1/6-octave grid) are
        // ≈ unity. The transition region between 200–600 Hz legitimately
        // carries partial gain from the log-interpolated shelf edge.
        for (i, b) in result.preset.bands.iter().enumerate().skip(30) {
            assert!(
                b.gain_db.abs() < 0.75,
                "band {i} ({} Hz) should be ≈ 0, got {}",
                b.frequency,
                b.gain_db
            );
        }
        // Preamp must offset the curve's own boost.
        assert!(
            result.preamp_db <= -3.0,
            "preamp should reserve the shelf boost, got {}",
            result.preamp_db
        );
        assert!(result.preamp_db > -8.0);
        // Fit metrics are sane.
        assert!(result.max_deviation_db >= 3.5);
        assert!(result.rms_error_db > 0.0);
    }

    #[test]
    fn output_is_deterministic() {
        let m = shelf_measurement();
        let params = AutoEqParams::default();
        let a = AutoEq::optimize("det", &m, &params);
        let b = AutoEq::optimize("det", &m, &params);
        assert_eq!(a.preset, b.preset);
        assert_eq!(a, b);
    }

    #[test]
    fn gain_is_clamped_to_max() {
        // A measurement with a huge dip → the optimizer must clamp the boost.
        let m = FrequencyResponse::new(vec![
            (20.0, -40.0),
            (100.0, -40.0),
            (500.0, -40.0),
            (1000.0, 0.0),
            (20000.0, 0.0),
        ]);
        let result = AutoEq::optimize(
            "clamp",
            &m,
            &AutoEqParams {
                target: TargetCurve::Flat,
                smoothing_octaves: None,
                max_gain_db: 12.0,
                ..Default::default()
            },
        );
        for b in &result.preset.bands {
            assert!(b.gain_db <= 12.0 + 1e-3, "clamped, got {}", b.gain_db);
        }
        assert!(
            result.preset.bands.iter().any(|b| b.gain_db > 11.0),
            "deep dip should hit the clamp"
        );
    }

    #[test]
    fn garbage_measurement_never_panics() {
        let m = FrequencyResponse::new(vec![
            (f32::NAN, 1.0),
            (-5.0, 2.0),
            (0.0, 3.0),
            (f32::INFINITY, 4.0),
            (1000.0, f32::NAN),
        ]);
        assert!(m.points().is_empty());
        let result = AutoEq::optimize(
            "garbage",
            &m,
            &AutoEqParams {
                target: TargetCurve::HarmanHeadphone2018,
                ..Default::default()
            },
        );
        // Everything finite, curve present.
        for b in &result.preset.bands {
            assert!(b.gain_db.is_finite());
            assert!(b.frequency.is_finite() && b.frequency > 0.0);
        }
        assert!(result.preamp_db.is_finite());
        assert!(result.max_deviation_db.is_finite());
        assert!(result.rms_error_db.is_finite());
    }

    #[test]
    fn target_curves_are_smooth_and_deterministic() {
        for target in [
            TargetCurve::Flat,
            TargetCurve::HarmanHeadphone2018,
            TargetCurve::HarmanIem2018,
            TargetCurve::Custom(vec![(20.0, 2.0), (1000.0, 0.0), (20000.0, 3.0)]),
        ] {
            let mut prev: Option<f32> = None;
            for i in 0..=40 {
                let f = 20.0 * (2.0f32).powf(i as f32 / 4.0);
                let db = target.eval_db(f);
                assert!(db.is_finite());
                if let Some(p) = prev {
                    // 1/4-octave steps of a smooth curve must not jump wildly.
                    assert!((db - p).abs() < 6.0, "target curve must be smooth");
                }
                prev = Some(db);
            }
            // Determinism: same eval twice.
            assert_eq!(target.eval_db(1000.0), target.eval_db(1000.0));
        }
        // Flat is exactly 0.
        assert_eq!(TargetCurve::Flat.eval_db(12345.0), 0.0);
    }

    #[test]
    fn custom_target_log_interpolation_is_exact() {
        let t = TargetCurve::Custom(vec![(100.0, 0.0), (1000.0, 20.0)]);
        // Log interpolation: 316.2 Hz (one decade below 1000, midpoint in log)
        // → ≈ 10 dB. Endpoints are exact.
        assert!((t.eval_db(100.0)).abs() < 1e-5);
        assert!((t.eval_db(1000.0) - 20.0).abs() < 1e-5);
        assert!((t.eval_db(316.22776) - 10.0).abs() < 0.25);
        // Outside the range: nearest endpoint.
        assert!((t.eval_db(50.0)).abs() < 1e-5);
        assert!((t.eval_db(5000.0) - 20.0).abs() < 1e-5);
    }

    #[test]
    fn custom_target_pulls_curve_toward_it() {
        // Measurement is flat 0; target has a +6 dB low shelf → generated
        // curve should be ≈ +6 dB low, ≈ 0 elsewhere.
        let m = FrequencyResponse::new(vec![
            (20.0, 0.0),
            (100.0, 0.0),
            (1000.0, 0.0),
            (10000.0, 0.0),
            (20000.0, 0.0),
        ]);
        let target = TargetCurve::Custom(vec![
            (20.0, 6.0),
            (100.0, 6.0),
            (1000.0, 0.0),
            (20000.0, 0.0),
        ]);
        let result = AutoEq::optimize(
            "custom",
            &m,
            &AutoEqParams {
                target,
                smoothing_octaves: None,
                ..Default::default()
            },
        );
        assert!(
            (result.preset.bands[0].gain_db - 6.0).abs() < 0.75,
            "low shelf should approach the +6 dB target, got {}",
            result.preset.bands[0].gain_db
        );
        assert!(result.preamp_db <= -4.0);
    }

    #[test]
    fn smoothing_reduces_spikiness() {
        let m = FrequencyResponse::new(vec![
            (20.0, 0.0),
            (1000.0, -12.0), // single narrow notch
            (20000.0, 0.0),
        ]);
        let rough = AutoEq::optimize(
            "rough",
            &m,
            &AutoEqParams {
                target: TargetCurve::Flat,
                smoothing_octaves: None,
                ..Default::default()
            },
        );
        let smooth = AutoEq::optimize(
            "smooth",
            &m,
            &AutoEqParams {
                target: TargetCurve::Flat,
                smoothing_octaves: Some(1.0 / 3.0),
                ..Default::default()
            },
        );
        let rough_peak = rough
            .bands
            .iter()
            .fold(0.0f32, |acc, (_, g)| acc.max(g.abs()));
        let smooth_peak = smooth
            .bands
            .iter()
            .fold(0.0f32, |acc, (_, g)| acc.max(g.abs()));
        assert!(
            smooth_peak < rough_peak,
            "smoothing must attenuate a narrow notch: {smooth_peak:.2} vs {rough_peak:.2}"
        );
    }
}
