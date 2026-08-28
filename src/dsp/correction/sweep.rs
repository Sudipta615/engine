//! S1 — sweep measurement kit: Farina exponential sine sweep, deconvolution,
//! harmonic separation, and measurement SNR.
//!
//! The measurement model ([Farina 2000]): play an ESS, record it, and divide
//! the recording by the sweep's spectrum (regularized inverse filter) to get
//! the impulse response. Nonlinearity in the device under test maps into
//! impulses at *known negative pre-delays* `−T·ln(k)/ln(f2/f1)` for the k-th
//! harmonic — so distortion separates from the linear response in time, and
//! the tail of the analysis buffer is pure noise by construction.
//!
//! All of this is control-thread DSP (`f64`, heap-happy, run once per
//! measurement); the realtime path never sees it.

use num_complex::Complex;

use super::phase::{excess_allpass_spectrum, phase_slope_delay_samples, Spectrum};
use super::{Cfft, CorrectionError};

/// Highest harmonic order `analyze_harmonics` will gate, and the order whose
/// negative pre-delay reserves the end of the analysis buffer as the
/// noise-only region.
pub const MAX_ANALYSIS_HARMONIC_ORDER: u32 = 8;

/// Length of the linear-response analysis window, as a fraction of the sweep
/// duration (the window that contains the direct sound + room tail).
const IR_WINDOW_FRACTION: f64 = 0.25;

/// Guard before the detected peak (samples at 1 kHz ⇒ 4 ms) when gating.
const PRE_GUARD_SECS: f64 = 0.004;

/// Guard between analysis regions (harmonic zone ↔ noise zone).
const REGION_GUARD_SECS: f64 = 0.05;

/// Half-width of each harmonic's gating window, as a fraction of the sweep
/// duration.
const HARMONIC_GATE_FRACTION: f64 = 0.02;

/// How far (samples) the sub-sample phase-slope estimate may pull the
/// integer peak index before it is distrusted.
const DELAY_TRUST_RADIUS_SAMPLES: f64 = 256.0;

/// Configuration for an exponential sine sweep measurement.
#[derive(Debug, Clone)]
pub struct EssConfig {
    /// Sample rate (Hz).
    pub sample_rate: f64,
    /// Sweep duration (s). Product range is 10–60 s; anything in
    /// 0.25–120 s is accepted so short synthetic measurements stay fast.
    pub duration_secs: f64,
    /// Start frequency (Hz).
    pub f_start: f64,
    /// End frequency (Hz); must be below Nyquist.
    pub f_end: f64,
    /// Peak amplitude in (0, 1].
    pub amplitude: f64,
    /// Half-cosine fade in/out at each end (s) to contain spectral
    /// splatter. The deconvolution divides by the *actual* generated
    /// reference, so fades do not bias the recovered response.
    pub fade_secs: f64,
    /// Pre-emphasis: amplitude-modulate the sweep by `√(f(t)/f_end)` so the
    /// spectral energy density is flat instead of pink, buying HF
    /// measurement SNR. The peak amplitude stays at `amplitude`.
    pub pre_emphasis: bool,
}

impl Default for EssConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            duration_secs: 10.0,
            f_start: 20.0,
            f_end: 20_000.0,
            amplitude: 0.5,
            fade_secs: 0.05,
            pre_emphasis: false,
        }
    }
}

/// A generated exponential sine sweep plus its exact reference data.
#[derive(Debug, Clone)]
pub struct EssSweep {
    config: EssConfig,
    samples: Vec<f64>,
    log_rate: f64, // ln(f_end / f_start)
}

impl EssSweep {
    /// Generate the sweep.
    ///
    /// # Errors
    /// [`CorrectionError::InvalidConfig`] on a rate/duration/band/amplitude
    /// outside the documented ranges.
    pub fn new(config: EssConfig) -> Result<Self, CorrectionError> {
        let fs = config.sample_rate;
        if !(fs.is_finite() && fs >= 8_000.0) {
            return Err(CorrectionError::InvalidConfig {
                what: "sample rate",
                message: format!("{fs} is below the 8 kHz minimum"),
            });
        }
        if !(0.25..=120.0).contains(&config.duration_secs) {
            return Err(CorrectionError::InvalidConfig {
                what: "duration",
                message: format!("{} s is outside 0.25–120 s", config.duration_secs),
            });
        }
        if !(config.f_start.is_finite() && config.f_start >= 1.0) {
            return Err(CorrectionError::InvalidConfig {
                what: "start frequency",
                message: format!("{} Hz is below 1 Hz", config.f_start),
            });
        }
        if !(config.f_end.is_finite() && config.f_end < 0.5 * fs) {
            return Err(CorrectionError::InvalidConfig {
                what: "end frequency",
                message: format!("{} Hz must be below Nyquist ({})", config.f_end, 0.5 * fs),
            });
        }
        if config.f_end <= config.f_start * 4.0 {
            return Err(CorrectionError::InvalidConfig {
                what: "frequency band",
                message: format!(
                    "band {}–{} Hz is narrower than two octaves",
                    config.f_start, config.f_end
                ),
            });
        }
        if !(config.amplitude.is_finite() && config.amplitude > 0.0 && config.amplitude <= 1.0) {
            return Err(CorrectionError::InvalidConfig {
                what: "amplitude",
                message: format!("{} is outside (0, 1]", config.amplitude),
            });
        }
        if !(config.fade_secs.is_finite()
            && config.fade_secs >= 0.0
            && 2.0 * config.fade_secs < config.duration_secs)
        {
            return Err(CorrectionError::InvalidConfig {
                what: "fade length",
                message: format!(
                    "{} s fades must be ≥ 0 and total < duration",
                    config.fade_secs
                ),
            });
        }

        let duration = config.duration_secs;
        let log_rate = (config.f_end / config.f_start).ln();
        // s(t) = sin(K·(e^{t/L} − 1)) with instantaneous frequency
        // ω(t) = (K/L)·e^{t/L} ⇒ L = T/ln(f2/f1), K = L·ω1.
        let big_l = duration / log_rate;
        let k = big_l * std::f64::consts::TAU * config.f_start;
        let n = (duration * fs).round() as usize;
        let fade = config.fade_secs.min(0.5 * duration);
        let fade_n = (fade * fs) as usize;
        let pre_emph = config.pre_emphasis;
        let amplitude = config.amplitude;
        let f_start = config.f_start;
        let f_end = config.f_end;

        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / fs;
            let mut env = 1.0;
            if fade_n > 0 {
                if i < fade_n {
                    env *= 0.5 - 0.5 * (std::f64::consts::PI * i as f64 / fade_n as f64).cos();
                } else if i + fade_n > n {
                    let d = ((n - i) as f64 / fade_n as f64).clamp(0.0, 1.0);
                    env *= 0.5 - 0.5 * (std::f64::consts::PI * d).cos();
                }
            }
            if pre_emph {
                let f_t = f_start * (t / big_l).exp();
                env *= (f_t / f_end).max(f_start / f_end).sqrt();
            }
            samples.push(amplitude * env * (k * ((t / big_l).exp() - 1.0)).sin());
        }
        // Keep the exact peak at `amplitude` even with pre-emphasis.
        if pre_emph {
            let peak = samples.iter().fold(0.0_f64, |m, &x| m.max(x.abs()));
            if peak > 0.0 {
                let scale = amplitude / peak;
                for s in &mut samples {
                    *s *= scale;
                }
            }
        }

        Ok(Self {
            config,
            samples,
            log_rate,
        })
    }

    /// The configuration this sweep was generated from.
    pub fn config(&self) -> &EssConfig {
        &self.config
    }

    /// The generated reference samples (what the DAC should play).
    pub fn samples(&self) -> &[f64] {
        &self.samples
    }

    /// Sweep length in samples.
    pub fn length_samples(&self) -> usize {
        self.samples.len()
    }

    /// Negative pre-delay, in samples, of the `order`-th harmonic impulse
    /// relative to the linear response (Farina: `−T·ln k / ln(f2/f1)`).
    pub fn harmonic_offset_samples(&self, order: u32) -> f64 {
        if order < 2 {
            return 0.0;
        }
        -self.config.duration_secs * self.config.sample_rate * (order as f64).ln() / self.log_rate
    }
}

/// The deconvolved measurement of one recording.
pub struct ImpulseResponse {
    /// Complex deconvolved response (`n` circular samples). The physical IR
    /// is the real part of the fundamental window; harmonic impulses sit at
    /// the negative-time (wrapped) offsets and are excluded by the windows.
    pub samples: Vec<Complex<f64>>,
    /// Analysis FFT length.
    pub n: usize,
    /// Recording sample rate (Hz).
    pub sample_rate: f64,
    /// Detected direct-sound arrival, sub-sample (samples). Integer peak
    /// search reconciled with the excess-phase slope.
    pub pre_delay: f64,

    arrival: usize,
    ir_window: (usize, usize),
    noise_window: (usize, usize),
    noise_gain_rms: f64,
}

impl ImpulseResponse {
    /// The `[start, end)` window holding the linear response (direct sound
    /// and tail).
    pub fn ir_window(&self) -> (usize, usize) {
        self.ir_window
    }

    /// The `[start, end)` noise-only region (after the linear window,
    /// before the wrapped harmonic zone).
    pub fn noise_window(&self) -> (usize, usize) {
        self.noise_window
    }

    /// RMS gain of the inverse filter: white recording noise of RMS `σ`
    /// becomes deconvolved noise of RMS `σ·noise_gain_rms()`. Used by
    /// [`noise_floor_db`] to refer the deconvolved noise back to the
    /// recording domain.
    pub fn noise_gain_rms(&self) -> f64 {
        self.noise_gain_rms
    }

    /// The physical (linear-response) impulse response as real samples:
    /// the real part of the fundamental window in circular buffer order.
    /// This is the S2 conditioner's input when a live measurement lands
    /// (Phase 7 S5 `MeasureRoom`); the conditioner detects the onset
    /// itself, so no pre-rotation is needed.
    pub fn real_ir(&self) -> Vec<f64> {
        let (w0, w1) = self.ir_window;
        self.samples[w0.min(self.n)..w1.min(self.n)]
            .iter()
            .map(|c| c.re)
            .collect()
    }
}

/// Deconvolve a recorded sweep into its impulse response.
///
/// The inverse filter is the regularized matched inverse
/// `G = S* / (|S|² + ε)` with `ε = 10⁻¹⁰·max|S|²`, capped at `100/max|S|` so
/// bins outside the sweep band cannot blow the noise up. Pre-delay is
/// detected by integer peak search reconciled with the sub-sample
/// excess-phase slope (robust to room resonances: the minimum-phase part is
/// divided out before the slope fit).
///
/// # Errors
/// [`CorrectionError::InvalidConfig`] if `recorded` is shorter than the
/// sweep.
pub fn deconvolve(recorded: &[f64], sweep: &EssSweep) -> Result<ImpulseResponse, CorrectionError> {
    if recorded.len() < sweep.length_samples() {
        return Err(CorrectionError::InvalidConfig {
            what: "recording",
            message: format!(
                "{} samples is shorter than the {}-sample sweep",
                recorded.len(),
                sweep.length_samples()
            ),
        });
    }

    let fs = sweep.config().sample_rate;
    let n = recorded
        .len()
        .max(sweep.length_samples())
        .next_power_of_two();

    // Forward transforms of the actual reference and the recording.
    let mut s_buf: Vec<Complex<f64>> = sweep
        .samples()
        .iter()
        .map(|&x| Complex::new(x, 0.0))
        .collect();
    s_buf.resize(n, Complex::new(0.0, 0.0));
    let mut y_buf: Vec<Complex<f64>> = recorded
        .iter()
        .take(n)
        .map(|&x| Complex::new(x, 0.0))
        .collect();
    y_buf.resize(n, Complex::new(0.0, 0.0));
    let cfft = Cfft::new(n);
    cfft.forward(&mut s_buf);
    cfft.forward(&mut y_buf);

    // Regularized matched inverse with an out-of-band magnitude cap.
    let s_max2 = s_buf.iter().map(|c| c.norm_sqr()).fold(0.0_f64, f64::max);
    let eps = 1e-10 * s_max2;
    let g_cap = 100.0 / s_max2.sqrt();
    let mut g_energy = 0.0_f64;
    let mut h = vec![Complex::new(0.0, 0.0); n];
    for k in 0..n {
        let s = s_buf[k];
        let s2 = s.norm_sqr();
        let mut g = s.conj() / (s2 + eps);
        let g2 = g.norm_sqr();
        if g2 > g_cap * g_cap {
            g *= g_cap / g2.sqrt();
        }
        g_energy += g.norm_sqr();
        h[k] = y_buf[k] * g;
    }
    let noise_gain_rms = (g_energy / n as f64).sqrt();

    cfft.inverse(&mut h);

    // Integer arrival: peak of the (real-part dominated) fundamental.
    let arrival = h
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.norm().total_cmp(&b.1.norm()))
        .map(|(i, _)| i)
        .unwrap_or(0);

    // Sub-sample refinement: gate to the fundamental window, divide out the
    // minimum-phase part, fit the excess-phase slope.
    let pre_guard = (PRE_GUARD_SECS * fs) as usize;
    let ir_len = (IR_WINDOW_FRACTION * sweep.config().duration_secs * fs) as usize;
    let gate_start = arrival.saturating_sub(pre_guard);
    let gate_end = (arrival + ir_len).min(n);
    let mut gated = vec![Complex::new(0.0, 0.0); n];
    gated[gate_start..gate_end].copy_from_slice(&h[gate_start..gate_end]);
    let gated_spec = Spectrum::from_bins(gated, fs)?;
    let fit = excess_allpass_spectrum(&gated_spec)
        .map(|x| phase_slope_delay_samples(&x, sweep.config().f_start, sweep.config().f_end))
        .unwrap_or(arrival as f64);

    let pre_delay = {
        let d = fit - arrival as f64;
        let half = n as f64 / 2.0;
        let d = d - half * (d / half).round();
        if d.abs() <= DELAY_TRUST_RADIUS_SAMPLES {
            arrival as f64 + d
        } else {
            arrival as f64
        }
    };

    // Analysis windows.
    let ir_window = (gate_start, gate_end);
    let harmonic_reserve = (sweep
        .harmonic_offset_samples(MAX_ANALYSIS_HARMONIC_ORDER)
        .abs()
        + REGION_GUARD_SECS * fs) as usize;
    let noise_start = (gate_end + (REGION_GUARD_SECS * fs) as usize).min(n);
    let noise_end = n.saturating_sub(harmonic_reserve);
    let noise_window = if noise_start < noise_end {
        (noise_start, noise_end)
    } else {
        (0, 0)
    };

    Ok(ImpulseResponse {
        samples: h,
        n,
        sample_rate: fs,
        pre_delay,
        arrival,
        ir_window,
        noise_window,
        noise_gain_rms,
    })
}

/// One gated harmonic impulse measurement.
#[derive(Debug, Clone, Copy)]
pub struct Harmonic {
    /// Harmonic order (2 = 2nd harmonic distortion, …).
    pub order: u32,
    /// Predicted negative pre-delay offset (samples) that was gated.
    pub offset_samples: f64,
    /// Peak level of the harmonic impulse relative to the fundamental
    /// (dB, ≤ 0 for a well-behaved device).
    pub level_db: f64,
}

/// Per-harmonic distortion report from a deconvolved measurement.
#[derive(Debug, Clone)]
pub struct HarmonicReport {
    /// One entry per analyzed order, ascending from 2.
    pub harmonics: Vec<Harmonic>,
    /// Peak level of the fundamental (linear) response, dBFS.
    pub fundamental_peak_db: f64,
}

/// Gate each harmonic's predicted pre-delay window and report its level
/// relative to the fundamental response.
///
/// Orders run `2..=max_order` (clamped to
/// [`MAX_ANALYSIS_HARMONIC_ORDER`]).
pub fn analyze_harmonics(ir: &ImpulseResponse, sweep: &EssSweep, max_order: u32) -> HarmonicReport {
    let fs = sweep.config().sample_rate;
    let gate = (HARMONIC_GATE_FRACTION * sweep.config().duration_secs * fs) as usize;
    let (w0, w1) = ir.ir_window;
    let fundamental = ir.samples[w0.min(ir.n)..w1.min(ir.n)]
        .iter()
        .map(|c| c.norm())
        .fold(0.0_f64, f64::max);
    let fundamental_db = 20.0 * fundamental.max(1e-12).log10();

    let mut harmonics = Vec::new();
    for order in 2..=max_order.min(MAX_ANALYSIS_HARMONIC_ORDER) {
        let offset = sweep.harmonic_offset_samples(order);
        let center_f = ir.arrival as f64 + offset;
        let center = center_f.rem_euclid(ir.n as f64) as usize;
        let mut peak = 0.0_f64;
        for d in 0..=gate {
            for idx in [(center + d) % ir.n, (center + ir.n - d % ir.n) % ir.n] {
                peak = peak.max(ir.samples[idx].norm());
            }
        }
        let level_db = if fundamental > 1e-12 {
            20.0 * (peak / fundamental).log10()
        } else {
            f64::NEG_INFINITY
        };
        harmonics.push(Harmonic {
            order,
            offset_samples: offset,
            level_db,
        });
    }

    HarmonicReport {
        harmonics,
        fundamental_peak_db: fundamental_db,
    }
}

/// Estimated noise floor of the *recording*, in dBFS.
///
/// The deconvolved noise per-sample RMS is `σ·noise_gain_rms()` for
/// recording noise of RMS `σ`; measuring the noise-only region and dividing
/// the filter's gain back out recovers `σ`. Stochastic but tight: the
/// chirp-shaped inverse filter makes consecutive deconvolved noise samples
/// nearly uncorrelated, so a moderate window average converges to well
/// under a dB.
///
/// # Errors
/// [`CorrectionError::InvalidConfig`] if the recording leaves no noise-only
/// region (too short relative to the sweep).
pub fn noise_floor_db(ir: &ImpulseResponse) -> Result<f64, CorrectionError> {
    let (a, b) = ir.noise_window;
    if b <= a {
        return Err(CorrectionError::InvalidConfig {
            what: "recording",
            message: "no noise-only region: recording too short for the harmonic reserve".into(),
        });
    }
    let power = ir.samples[a..b].iter().map(|c| c.norm_sqr()).sum::<f64>() / (b - a) as f64;
    let gain2 = ir.noise_gain_rms * ir.noise_gain_rms;
    let sigma = (power / gain2.max(1e-30)).sqrt();
    Ok(20.0 * sigma.max(1e-12).log10())
}

/// Usable SNR of the measurement, in dB: the fundamental response's peak
/// level above the recording's estimated noise floor.
///
/// # Errors
/// As [`noise_floor_db`].
pub fn estimate_snr_db(ir: &ImpulseResponse) -> Result<f64, CorrectionError> {
    let (w0, w1) = ir.ir_window;
    let peak = ir.samples[w0.min(ir.n)..w1.min(ir.n)]
        .iter()
        .map(|c| c.norm())
        .fold(0.0_f64, f64::max);
    let peak_db = 20.0 * peak.max(1e-12).log10();
    Ok(peak_db - noise_floor_db(ir)?)
}
