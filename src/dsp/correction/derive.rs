//! S4 — correction derivation: from a conditioned IR to a phase-rendered
//! correction IR set.
//!
//! The chain, all control-thread `f64` DSP:
//!
//! 1. Per-channel spectrum of the conditioned measurement IR.
//! 2. **Octave-fraction smoothing** of the measured magnitude in the power
//!    domain (log-frequency averaging — the standard room-measurement
//!    treatment; narrow measurement artifacts never become correction
//!    boosts).
//! 3. **Target comparison** — flat, tilt (dB/octave), or shelf target.
//! 4. **SNR-weighted regularized inverse** — per-bin Wiener weighting so
//!    boosts collapse where the measurement is unreliable, plus a hard
//!    boost clamp (`max_boost_db`).
//! 5. **Phase rendering** per channel via [`super::phase`], with a single
//!    global safety scale keeping every IR peak below digital full scale so
//!    correction can never clip into the master limiter on its own.

use super::ir::ConditionedIr;
use super::phase::{render_from_magnitude_db, PhaseMode, RenderParams, RenderedIr, Spectrum};
use super::CorrectionError;

/// The desired steady-state response after correction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TargetCurve {
    /// Flat magnitude (0 dB at every frequency).
    Flat,
    /// Linear tilt in log-frequency: `db_per_octave · log2(f / 1 kHz)`.
    /// Negative tilts roll the top off ("dark"), positive tilts brighten.
    Tilt {
        /// Slope in dB per octave.
        db_per_octave: f64,
    },
    /// Smooth shelf between two plateau gains around `corner_hz`, swept
    /// over `slope_octaves` with a raised cosine.
    Shelf {
        /// Center of the transition (Hz).
        corner_hz: f64,
        /// Plateau gain below the transition (dB).
        low_gain_db: f64,
        /// Plateau gain above the transition (dB).
        high_gain_db: f64,
        /// Total transition width (octaves).
        slope_octaves: f64,
    },
}

impl TargetCurve {
    /// Target magnitude at `f_hz` in dB.
    pub fn target_db(&self, f_hz: f64) -> f64 {
        match *self {
            Self::Flat => 0.0,
            Self::Tilt { db_per_octave } => db_per_octave * (f_hz / 1000.0).log2(),
            Self::Shelf {
                corner_hz,
                low_gain_db,
                high_gain_db,
                slope_octaves,
            } => {
                if !(slope_octaves.is_finite() && slope_octaves > 0.0) || corner_hz <= 0.0 {
                    return low_gain_db;
                }
                let x = ((f_hz / corner_hz).log2() / slope_octaves).clamp(-0.5, 0.5);
                let u = x + 0.5; // 0..1 across the transition
                let w = 0.5 * (1.0 - (std::f64::consts::PI * u).cos());
                low_gain_db + (high_gain_db - low_gain_db) * w
            }
        }
    }
}

/// Parameters of the correction derivation.
#[derive(Debug, Clone)]
pub struct DeriveParams {
    /// Session sample rate (Hz).
    pub sample_rate: f64,
    /// Rendered IR length (FFT length, power of two ≥ 16). Every channel
    /// must fit within it. Linear/hybrid latency scales with this.
    pub ir_len_samples: usize,
    /// Desired post-correction response.
    pub target: TargetCurve,
    /// Hard clamp on any correction boost (dB). Cuts are not clamped.
    pub max_boost_db: f64,
    /// Octave-fraction smoothing applied to the measured magnitude before
    /// inversion (power-domain, log-frequency).
    pub smoothing_octaves: f64,
    /// Measurement SNR (dB) feeding the Wiener weighting — pass the value
    /// reported by `super::sweep::estimate_snr_db`. Lower SNR shrinks
    /// boosts toward zero.
    pub snr_db: f64,
    /// Phase mode of the rendered correction IRs.
    pub phase_mode: PhaseMode,
    /// Hybrid crossover (Hz); only used when `phase_mode` is hybrid.
    pub hybrid_crossover_hz: f64,
}

impl Default for DeriveParams {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            ir_len_samples: 4096,
            target: TargetCurve::Flat,
            max_boost_db: 6.0,
            smoothing_octaves: 1.0 / 6.0,
            snr_db: 60.0,
            phase_mode: PhaseMode::Minimum,
            hybrid_crossover_hz: 1_000.0,
        }
    }
}

/// The final correction: one causal IR per channel, ready for a
/// `CorrectionNode` partitioned-convolution bank.
///
/// `PartialEq` is exact (bitwise over the rendered samples): the set is an
/// immutable control-path artifact that is only ever *replaced*, never
/// mutated, so equality means "identical rendering".
#[derive(Debug, Clone, PartialEq)]
pub struct CorrectionIrSet {
    /// One rendered IR per measured channel, in channel order.
    pub channels: Vec<Vec<f64>>,
    /// Session sample rate (Hz).
    pub sample_rate: f64,
    /// Declared latency of the set (samples) — the phase mode's delay.
    pub delay_samples: f64,
    /// Phase mode the IRs were rendered in.
    pub phase_mode: PhaseMode,
    /// Global safety scale applied so the loudest IR peaks below full
    /// scale (`≤ 1.0`; 1.0 when no scaling was needed).
    pub peak_scale: f64,
}

/// Power-domain octave-fraction smoothing of a half-spectrum magnitude.
///
/// Bin `j` is averaged over `[f_j·2^(−o/2), f_j·2^(+o/2)]` in the power
/// domain via a prefix sum (O(n)). DC and Nyquist pass through untouched —
/// correction is never derived at the band edges.
fn octave_smooth_db(mag_db: &[f64], sample_rate: f64, n: usize, octaves: f64) -> Vec<f64> {
    let bins = n / 2 + 1;
    let mut power = vec![0.0_f64; bins + 1]; // prefix sums
    for j in 0..bins {
        power[j + 1] = power[j] + 10.0_f64.powf(mag_db[j] / 10.0);
    }
    let mut out = vec![0.0_f64; bins];
    out[0] = mag_db[0];
    out[bins - 1] = mag_db[bins - 1];
    let half = 0.5 * octaves;
    for (j, value) in out.iter_mut().enumerate().take(bins - 1).skip(1) {
        let f = j as f64 * sample_rate / n as f64;
        let lo_f = f * 2.0_f64.powf(-half);
        let hi_f = f * 2.0_f64.powf(half);
        let lo = ((lo_f * n as f64 / sample_rate).ceil() as usize).clamp(1, bins - 1);
        let hi = ((hi_f * n as f64 / sample_rate).floor() as usize).clamp(1, bins - 1);
        let (lo, hi) = (lo.min(j), hi.max(j));
        let mean = (power[hi + 1] - power[lo]) / (hi - lo + 1) as f64;
        *value = 10.0 * mean.max(1e-30).log10();
    }
    out
}

/// Derive the per-bin correction magnitude (half spectrum, `n/2 + 1` bins)
/// from a measured magnitude.
///
/// `measured_mag_db` must be the half-spectrum magnitude of the conditioned
/// measurement IR at FFT length `n = (len − 1)·2`. DC and Nyquist bins are
/// always left at 0 dB — no correction at the band edges.
///
/// # Errors
/// [`CorrectionError::InvalidConfig`] on a bad magnitude length or
/// parameters.
pub fn derive_correction_magnitude_db(
    measured_mag_db: &[f64],
    params: &DeriveParams,
) -> Result<Vec<f64>, CorrectionError> {
    if measured_mag_db.is_empty() || !(measured_mag_db.len() - 1).is_multiple_of(2) {
        return Err(CorrectionError::InvalidConfig {
            what: "measured magnitude",
            message: format!(
                "length {} is not of the form n/2 + 1",
                measured_mag_db.len()
            ),
        });
    }
    let n = (measured_mag_db.len() - 1) * 2;
    if !n.is_power_of_two() {
        return Err(CorrectionError::InvalidConfig {
            what: "measured magnitude",
            message: format!(
                "length {} does not come from a power-of-two FFT",
                measured_mag_db.len()
            ),
        });
    }
    let bins = n / 2 + 1;
    if !(params.sample_rate.is_finite() && params.sample_rate > 0.0) {
        return Err(CorrectionError::InvalidConfig {
            what: "sample rate",
            message: format!("{} is not a positive finite rate", params.sample_rate),
        });
    }
    if !(params.smoothing_octaves.is_finite() && params.smoothing_octaves > 0.0) {
        return Err(CorrectionError::InvalidConfig {
            what: "smoothing",
            message: format!("{} octaves is not positive", params.smoothing_octaves),
        });
    }
    if !params.max_boost_db.is_finite() {
        return Err(CorrectionError::InvalidConfig {
            what: "max boost",
            message: "max_boost_db must be finite".into(),
        });
    }

    let smoothed = octave_smooth_db(
        measured_mag_db,
        params.sample_rate,
        n,
        params.smoothing_octaves,
    );

    // Per-bin Wiener weight: reliable (loud) bins keep their correction,
    // bins near/below the noise floor collapse toward 0 dB.
    let snr_lin = 10.0_f64.powf(params.snr_db / 10.0).max(1e-12);
    let mut mean_power = 0.0_f64;
    for &smoothed_db in smoothed.iter().take(bins - 1).skip(1) {
        mean_power += 10.0_f64.powf(smoothed_db / 10.0);
    }
    mean_power /= (bins - 2).max(1) as f64;
    let noise_power = mean_power / snr_lin;

    let mut out = vec![0.0_f64; bins];
    for (j, &smoothed_db) in smoothed.iter().enumerate().take(bins - 1).skip(1) {
        let p = 10.0_f64.powf(smoothed_db / 10.0);
        let w = p / (p + noise_power);
        let c = (params
            .target
            .target_db(j as f64 * params.sample_rate / n as f64)
            - smoothed_db)
            * w;
        out[j] = c.min(params.max_boost_db);
    }
    Ok(out)
}

/// Derive the full correction IR set from a conditioned measurement.
///
/// # Errors
/// * [`CorrectionError::InvalidConfig`] on bad parameters, an FFT length
///   shorter than a channel, or channels of differing lengths.
/// * Errors propagated from [`super::phase::render_from_magnitude_db`].
pub fn derive_correction_ir(
    measured: &ConditionedIr,
    params: &DeriveParams,
) -> Result<CorrectionIrSet, CorrectionError> {
    let n = params.ir_len_samples;
    if n < 16 || !n.is_power_of_two() {
        return Err(CorrectionError::InvalidConfig {
            what: "IR length",
            message: format!("{n} is not a power of two >= 16"),
        });
    }
    if measured.channels.is_empty() {
        return Err(CorrectionError::InvalidConfig {
            what: "measurement channels",
            message: "no channels to correct".into(),
        });
    }
    let len = measured.channels[0].len();
    if measured.channels.iter().any(|c| c.len() != len) {
        return Err(CorrectionError::InvalidConfig {
            what: "measurement channels",
            message: "channels have differing lengths".into(),
        });
    }
    if len > n {
        return Err(CorrectionError::InvalidConfig {
            what: "IR length",
            message: format!("channels of {len} samples exceed the {n}-sample render length"),
        });
    }

    let mut channels = Vec::with_capacity(measured.channels.len());
    let mut peak = 0.0_f64;
    let mut delay = 0.0_f64;
    let render_params = RenderParams {
        sample_rate: params.sample_rate,
        ir_len_samples: n,
        phase_mode: params.phase_mode,
        hybrid_crossover_hz: params.hybrid_crossover_hz,
    };

    for ch in &measured.channels {
        let spectrum = Spectrum::from_time_with_len(ch, n, params.sample_rate)?;
        let correction_db = derive_correction_magnitude_db(&spectrum.magnitude_db(), params)?;
        let rendered: RenderedIr = render_from_magnitude_db(&correction_db, &render_params)?;
        delay = rendered.delay_samples;
        for &s in &rendered.samples {
            peak = peak.max(s.abs());
        }
        channels.push(rendered.samples);
    }

    // Safety rail: the set may only ever be attenuated toward full scale.
    let peak_scale = if peak > 0.98 { 0.98 / peak } else { 1.0 };
    if peak_scale < 1.0 {
        for ch in &mut channels {
            for s in ch {
                *s *= peak_scale;
            }
        }
    }

    Ok(CorrectionIrSet {
        channels,
        sample_rate: params.sample_rate,
        delay_samples: delay,
        phase_mode: params.phase_mode,
        peak_scale,
    })
}
