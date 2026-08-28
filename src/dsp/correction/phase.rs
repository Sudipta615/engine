//! S3 — phase machinery: minimum / linear / hybrid phase rendering.
//!
//! Everything a correction IR needs to move between the magnitude domain
//! (where measurement and target curves live) and the time domain (where the
//! `CorrectionNode` will consume it):
//!
//! * **Minimum phase** — cepstral (Hilbert transform of the log-magnitude)
//!   construction. A min-phase render has the same magnitude as its source
//!   with the smallest possible causal phase; zero added latency.
//! * **Excess phase** — the complementary allpass `H / H_min`, flat in
//!   magnitude by construction. For a room that is a minimum-phase filter
//!   plus a pure delay, the excess allpass *is* that delay, which is what
//!   makes it the robust latency detector for [`super::sweep`].
//! * **Linear phase** — a symmetric FIR (constant group delay `n/2`) for
//!   listeners who accept the latency in exchange for a phase-flat passband.
//! * **Hybrid phase** — the exact minimum-phase IR delayed by τ₀ (two
//!   crossover cycles): minimum-phase behavior below the crossover, a
//!   near-constant τ₀ group delay above it (where the correction is
//!   spectrally smooth), and a magnitude response bit-identical to the
//!   min render at every frequency — the shift is linear, so |DTFT| is
//!   preserved exactly.
//!
//! Control-thread DSP: heap-happy `f64` FFT work run once per render, never
//! on the audio path.

use num_complex::Complex;

use super::{Cfft, CorrectionError, MAG_FLOOR_AMP};

/// Phase rendering mode for a correction IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseMode {
    /// Cepstral minimum phase: zero added latency, phase-dispersive.
    Minimum,
    /// Symmetric FIR: constant group delay `n/2`, phase-flat magnitude.
    Linear,
    /// Minimum phase below the crossover, linear phase above — rendered
    /// as the exact minimum-phase IR delayed by τ₀ samples (two crossover
    /// cycles), so the magnitude is bit-identical to the min render while
    /// the group delay sits at ≈ τ₀ where the correction is smooth. The
    /// declared latency is that delay (see [`RenderedIr`]).
    Hybrid,
}

/// A full complex FFT spectrum with its sample rate attached.
///
/// Length is always a power of two ≥ 16. Constructors cover the three ways
/// the correction chain produces spectra: from real time data
/// ([`Spectrum::from_time`]/[`Spectrum::from_time_with_len`]), from a
/// half-spectrum magnitude with zero phase ([`Spectrum::from_magnitude_db`]),
/// and from raw complex bins ([`Spectrum::from_bins`]).
#[derive(Debug, Clone)]
pub struct Spectrum {
    bins: Vec<Complex<f64>>,
    sample_rate: f64,
}

impl Spectrum {
    /// Build from raw complex bins (full FFT length).
    ///
    /// # Errors
    /// [`CorrectionError::InvalidConfig`] unless `bins.len()` is a power of
    /// two ≥ 16 and `sample_rate` is a positive finite rate.
    pub fn from_bins(bins: Vec<Complex<f64>>, sample_rate: f64) -> Result<Self, CorrectionError> {
        validate_fft_len(bins.len())?;
        validate_rate(sample_rate)?;
        Ok(Self { bins, sample_rate })
    }

    /// Forward-FFT real time-domain samples, zero-padded to the next power
    /// of two (≥ 16).
    pub fn from_time(samples: &[f64], sample_rate: f64) -> Self {
        let n = samples.len().max(16).next_power_of_two();
        Self::from_time_with_len(samples, n, sample_rate).expect("n is a valid power of two")
    }

    /// Forward-FFT real time-domain samples zero-padded to exactly `n`.
    ///
    /// # Errors
    /// [`CorrectionError::InvalidConfig`] unless `n` is a power of two ≥ 16
    /// at least `samples.len()`, and `sample_rate` is positive.
    pub fn from_time_with_len(
        samples: &[f64],
        n: usize,
        sample_rate: f64,
    ) -> Result<Self, CorrectionError> {
        validate_fft_len(n)?;
        validate_rate(sample_rate)?;
        if samples.len() > n {
            return Err(CorrectionError::InvalidConfig {
                what: "time-domain length",
                message: format!("{} samples exceed FFT length {n}", samples.len()),
            });
        }
        let mut bins: Vec<Complex<f64>> = samples.iter().map(|&x| Complex::new(x, 0.0)).collect();
        bins.resize(n, Complex::new(0.0, 0.0));
        Cfft::new(n).forward(&mut bins);
        Ok(Self { bins, sample_rate })
    }

    /// Synthesize a zero-phase (hermitian) spectrum from a half-spectrum
    /// magnitude in dB (`len == n/2 + 1`, DC..=Nyquist).
    ///
    /// # Errors
    /// [`CorrectionError::InvalidConfig`] on a bad `n`, rate, or magnitude
    /// length.
    pub fn from_magnitude_db(
        mag_db: &[f64],
        n: usize,
        sample_rate: f64,
    ) -> Result<Self, CorrectionError> {
        validate_fft_len(n)?;
        validate_rate(sample_rate)?;
        validate_half_magnitude(mag_db, n)?;
        let mut bins = vec![Complex::new(0.0, 0.0); n];
        for (j, &db) in mag_db.iter().enumerate() {
            let m = amplitude(db);
            bins[j] = Complex::new(m, 0.0);
            if j > 0 && j < n / 2 {
                bins[n - j] = Complex::new(m, 0.0);
            }
        }
        Ok(Self { bins, sample_rate })
    }

    /// FFT length.
    pub fn n(&self) -> usize {
        self.bins.len()
    }

    /// Session sample rate (Hz).
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Full complex bins.
    pub fn bins(&self) -> &[Complex<f64>] {
        &self.bins
    }

    /// Half-spectrum magnitude in dB (DC..=Nyquist, `n/2 + 1` bins),
    /// floored at [`MAG_FLOOR_AMP`].
    pub fn magnitude_db(&self) -> Vec<f64> {
        (0..=self.n() / 2)
            .map(|j| magnitude_db(self.bins[j]))
            .collect()
    }

    /// Normalized inverse FFT; returns the real part (`n` samples).
    pub fn to_time(&self) -> Vec<f64> {
        let mut bins = self.bins.clone();
        Cfft::new(self.n()).inverse(&mut bins);
        bins.iter().map(|c| c.re).collect()
    }
}

/// Parameters for rendering a magnitude response into a time-domain IR.
#[derive(Debug, Clone)]
pub struct RenderParams {
    /// Session sample rate (Hz).
    pub sample_rate: f64,
    /// Rendered IR length in samples — the FFT length, a power of two ≥ 16.
    pub ir_len_samples: usize,
    /// Phase rendering mode.
    pub phase_mode: PhaseMode,
    /// Hybrid crossover (Hz); ignored by the other modes.
    pub hybrid_crossover_hz: f64,
}

impl Default for RenderParams {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            ir_len_samples: 4096,
            phase_mode: PhaseMode::Minimum,
            hybrid_crossover_hz: 1_000.0,
        }
    }
}

/// A rendered time-domain correction IR.
#[derive(Debug, Clone)]
pub struct RenderedIr {
    /// Real IR samples (`ir_len_samples` of them, causal by construction).
    pub samples: Vec<f64>,
    /// FFT length the IR was rendered at.
    pub n: usize,
    /// Session sample rate (Hz).
    pub sample_rate: f64,
    /// Declared latency in samples: 0 for minimum phase, `n/2` for linear,
    /// the hybrid linear-branch delay for hybrid.
    pub delay_samples: f64,
    /// The mode that produced this IR.
    pub mode: PhaseMode,
}

/// Validate that `n` is a usable FFT length (power of two ≥ 16).
fn validate_fft_len(n: usize) -> Result<(), CorrectionError> {
    if n < 16 || !n.is_power_of_two() {
        return Err(CorrectionError::InvalidConfig {
            what: "FFT length",
            message: format!("{n} is not a power of two >= 16"),
        });
    }
    Ok(())
}

/// Validate a positive finite sample rate.
fn validate_rate(sample_rate: f64) -> Result<(), CorrectionError> {
    if !(sample_rate.is_finite() && sample_rate > 0.0) {
        return Err(CorrectionError::InvalidConfig {
            what: "sample rate",
            message: format!("{sample_rate} is not a positive finite rate"),
        });
    }
    Ok(())
}

/// Validate a half-spectrum magnitude's length against FFT length `n`.
fn validate_half_magnitude(mag_db: &[f64], n: usize) -> Result<(), CorrectionError> {
    if mag_db.len() != n / 2 + 1 {
        return Err(CorrectionError::InvalidConfig {
            what: "magnitude length",
            message: format!(
                "expected {} bins for n = {n}, got {}",
                n / 2 + 1,
                mag_db.len()
            ),
        });
    }
    Ok(())
}

/// Amplitude for a dB value, floored at [`MAG_FLOOR_AMP`].
fn amplitude(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0).max(MAG_FLOOR_AMP)
}

/// Magnitude in dB of a complex bin, floored at −180 dBFS-equivalent.
fn magnitude_db(c: Complex<f64>) -> f64 {
    20.0 * c.norm().max(MAG_FLOOR_AMP).log10()
}

/// Complex log-spectrum of the minimum-phase system matching `mag_db`
/// (half-spectrum magnitude, `n/2 + 1` bins).
///
/// Classic cepstral construction: the real cepstrum of the log-magnitude is
/// folded onto its causal half (×2 on the positive side, DC and Nyquist kept
/// at weight 1) and re-transformed. `Re(C)` reproduces the input
/// log-magnitude exactly; `Im(C)` is the unwrapped minimum-phase response —
/// continuous by construction, never an `arg()` of a wrapped phasor.
fn cepstral_log_spectrum(mag_db: &[f64], n: usize) -> Result<Vec<Complex<f64>>, CorrectionError> {
    validate_fft_len(n)?;
    validate_half_magnitude(mag_db, n)?;

    // Even-real log-magnitude spectrum (full length).
    let mut log_mag = vec![Complex::new(0.0, 0.0); n];
    for (j, &db) in mag_db.iter().enumerate() {
        let v = amplitude(db).ln();
        log_mag[j] = Complex::new(v, 0.0);
        if j > 0 && j < n / 2 {
            log_mag[n - j] = Complex::new(v, 0.0);
        }
    }

    // Real cepstrum (normalized inverse of an even-real spectrum is even-real).
    let cfft = Cfft::new(n);
    cfft.inverse(&mut log_mag);

    // Fold onto the causal half: d[0] = c[0], d[k] = 2c[k] for 0<k<n/2,
    // d[n/2] = c[n/2], d[k>n/2] = 0.
    let mut folded = vec![Complex::new(0.0, 0.0); n];
    folded[0] = Complex::new(log_mag[0].re, 0.0);
    for k in 1..n / 2 {
        folded[k] = Complex::new(2.0 * log_mag[k].re, 0.0);
    }
    folded[n / 2] = Complex::new(log_mag[n / 2].re, 0.0);

    cfft.forward(&mut folded);
    Ok(folded)
}

/// Minimum-phase spectrum matching `source`'s magnitude.
///
/// `Re(log H_min)` equals the source log-magnitude exactly; `Im(log H_min)`
/// is the continuous unwrapped minimum phase.
///
/// # Errors
/// [`CorrectionError::InvalidConfig`] if `source` has a bad FFT length.
pub fn minimum_phase_spectrum(source: &Spectrum) -> Result<Spectrum, CorrectionError> {
    let log_spec = cepstral_log_spectrum(&source.magnitude_db(), source.n())?;
    let bins = log_spec.iter().map(|c| c.exp()).collect();
    Ok(Spectrum {
        bins,
        sample_rate: source.sample_rate,
    })
}

/// Time-domain minimum-phase IR matching `source`'s magnitude (causal,
/// length `source.n()`).
///
/// # Errors
/// [`CorrectionError::InvalidConfig`] if `source` has a bad FFT length.
pub fn minimum_phase_ir(source: &Spectrum) -> Result<Vec<f64>, CorrectionError> {
    Ok(minimum_phase_spectrum(source)?.to_time())
}

/// Excess-phase allpass `X = H / H_min` — flat magnitude, carrying exactly
/// the phase structure that is *not* minimum-phase (pure delays,
/// reflections).
///
/// Bins where `H` sits at the magnitude floor yield `X ≈ 0`, so
/// |X|-weighted estimators naturally ignore dead bins.
///
/// # Errors
/// [`CorrectionError::InvalidConfig`] if `source` has a bad FFT length.
pub fn excess_allpass_spectrum(source: &Spectrum) -> Result<Spectrum, CorrectionError> {
    let log_spec = cepstral_log_spectrum(&source.magnitude_db(), source.n())?;
    let bins: Vec<Complex<f64>> = source
        .bins()
        .iter()
        .zip(log_spec.iter())
        .map(|(&h, c)| {
            let h_min = c.exp();
            if h_min.norm() > MAG_FLOOR_AMP {
                h / h_min
            } else {
                Complex::new(0.0, 0.0)
            }
        })
        .collect();
    Ok(Spectrum {
        bins,
        sample_rate: source.sample_rate,
    })
}

/// Per-bin group delay of `source` in samples, via adjacent-bin local phase
/// differences (bins `1..n/2`; DC and Nyquist copy their neighbours).
///
/// Local differences avoid any global unwrapping. Dead bins (magnitude at
/// the floor) are marked [`f64::NAN`] so callers can skip or clamp them.
///
/// Note: for delays approaching `n/2` samples the per-bin phase step
/// reaches π and any phase-difference method degenerates — use
/// [`group_delay_exact_samples`], which is unwrap-free by construction.
pub fn group_delay_samples_per_bin(source: &Spectrum) -> Vec<f64> {
    let n = source.n();
    let fs = source.sample_rate;
    let bins = source.bins();
    let to_samples = fs * n as f64 / std::f64::consts::TAU;
    let mut gd = vec![0.0; n / 2 + 1];
    for j in 1..n / 2 {
        let d_phi = bins[j + 1].arg() - bins[j].arg();
        let d_phi = if d_phi > std::f64::consts::PI {
            d_phi - std::f64::consts::TAU
        } else if d_phi < -std::f64::consts::PI {
            d_phi + std::f64::consts::TAU
        } else {
            d_phi
        };
        let dead = bins[j].norm() <= MAG_FLOOR_AMP || bins[j + 1].norm() <= MAG_FLOOR_AMP;
        gd[j] = if dead { f64::NAN } else { -d_phi * to_samples };
    }
    gd[0] = gd[1];
    gd[n / 2] = gd[n / 2 - 1];
    gd
}

/// Exact per-bin group delay of `source` in samples via the frequency
/// derivative: `GD = −Im(H′/H)` with `H′ = FFT(−i·t·h)`.
///
/// Unwrap-free and exact for *any* delay — including the `n/2` symmetric
/// FIR where phase-difference methods alias. The logarithmic derivative is
/// additive, so the minimum/excess split satisfies
/// `GD(H_min·X) = GD(H_min) + GD(X)` exactly. Dead bins (magnitude at the
/// floor) are marked [`f64::NAN`].
pub fn group_delay_exact_samples(source: &Spectrum) -> Vec<f64> {
    let n = source.n();
    let h_time = source.to_time();
    let mut derivative: Vec<Complex<f64>> = h_time
        .iter()
        .enumerate()
        .map(|(t, &x)| Complex::new(0.0, -(t as f64) * x))
        .collect();
    Cfft::new(n).forward(&mut derivative);
    source
        .bins()
        .iter()
        .zip(derivative.iter())
        .map(|(&h, &dh)| {
            if h.norm() <= MAG_FLOOR_AMP {
                f64::NAN
            } else {
                -(dh / h).im
            }
        })
        .collect()
}

/// Weighted phase-slope latency estimate, in samples, of `source` over the
/// band `[f_lo, f_hi]`.
///
/// Fits `φ(j) = φ0 + slope·j` by |bin|²-weighted least squares over the
/// sequentially unwrapped phase and converts the slope to a delay. For an
/// excess-phase allpass this yields the pure delay to sub-sample accuracy
/// even in the presence of deep magnitude notches — dead bins contribute
/// zero weight.
pub fn phase_slope_delay_samples(source: &Spectrum, f_lo: f64, f_hi: f64) -> f64 {
    let n = source.n();
    let fs = source.sample_rate;
    if !fs.is_finite() || fs <= 0.0 || f_hi <= f_lo {
        return 0.0;
    }
    let j_lo = ((f_lo * n as f64 / fs).ceil() as usize).clamp(1, n / 2 - 2);
    let j_hi = ((f_hi * n as f64 / fs).floor() as usize).clamp(j_lo + 1, n / 2 - 1);

    // Sequential unwrap.
    let mut phase = Vec::with_capacity(j_hi - j_lo + 1);
    let mut prev = source.bins()[j_lo].arg();
    phase.push(prev);
    for j in j_lo + 1..=j_hi {
        let mut p = source.bins()[j].arg();
        while p - prev > std::f64::consts::PI {
            p -= std::f64::consts::TAU;
        }
        while p - prev < -std::f64::consts::PI {
            p += std::f64::consts::TAU;
        }
        prev = p;
        phase.push(p);
    }

    // Weighted least squares over j.
    let (mut sw, mut sj, mut sjj, mut sp, mut sjp) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (i, &p) in phase.iter().enumerate() {
        let j = (j_lo + i) as f64;
        let w = source.bins()[j_lo + i].norm_sqr();
        sw += w;
        sj += w * j;
        sjj += w * j * j;
        sp += w * p;
        sjp += w * j * p;
    }
    let denom = sw * sjj - sj * sj;
    if denom.abs() <= f64::EPSILON || sw <= f64::EPSILON {
        return 0.0;
    }
    let slope = (sw * sjp - sj * sp) / denom; // rad per bin
    -slope * n as f64 / std::f64::consts::TAU
}

/// Render a half-spectrum magnitude (`n/2 + 1` bins, DC..=Nyquist) into a
/// causal time-domain IR using `params.phase_mode`.
///
/// * [`PhaseMode::Minimum`] — cepstral minimum phase, zero latency.
/// * [`PhaseMode::Linear`] — zero-phase synthesis circularly shifted by
///   `n/2`; exact constant group delay `n/2`.
/// * [`PhaseMode::Hybrid`] — the exact minimum-phase IR linearly delayed
///   by `τ0 = ceil(2·fs/fc)` samples (two cycles of the crossover). The
///   linear shift preserves the magnitude response bit-for-bit at every
///   frequency, so the hybrid is magnitude-identical to the min render;
///   the group delay becomes `GD_min + τ0`, converging to ≈ τ0 where the
///   correction is spectrally smooth (the linear-phase branch).
///
/// # Errors
/// [`CorrectionError::InvalidConfig`] on a bad length, magnitude size,
/// sample rate, or (hybrid) crossover frequency.
pub fn render_from_magnitude_db(
    mag_db: &[f64],
    params: &RenderParams,
) -> Result<RenderedIr, CorrectionError> {
    let n = params.ir_len_samples;
    validate_fft_len(n)?;
    validate_rate(params.sample_rate)?;
    validate_half_magnitude(mag_db, n)?;
    let fs = params.sample_rate;

    match params.phase_mode {
        PhaseMode::Minimum => {
            let src = Spectrum::from_magnitude_db(mag_db, n, fs)?;
            let samples = minimum_phase_ir(&src)?;
            Ok(RenderedIr {
                samples,
                n,
                sample_rate: fs,
                delay_samples: 0.0,
                mode: PhaseMode::Minimum,
            })
        }
        PhaseMode::Linear => {
            let src = Spectrum::from_magnitude_db(mag_db, n, fs)?;
            let zero_phase = src.to_time();
            // Circular shift by n/2 makes the symmetric FIR causal; the
            // spectrum then carries an exact −ω·n/2 linear phase.
            let mut samples = vec![0.0; n];
            for (i, &v) in zero_phase.iter().enumerate() {
                samples[(i + n / 2) % n] = v;
            }
            Ok(RenderedIr {
                samples,
                n,
                sample_rate: fs,
                delay_samples: n as f64 / 2.0,
                mode: PhaseMode::Linear,
            })
        }
        PhaseMode::Hybrid => {
            let fc = params.hybrid_crossover_hz;
            let nyquist = fs / 2.0;
            if !fc.is_finite() || !(20.0..=nyquist / 4.0).contains(&fc) {
                return Err(CorrectionError::InvalidConfig {
                    what: "hybrid crossover",
                    message: format!("{fc} Hz is outside 20..=nyquist/4 ({})", nyquist / 4.0),
                });
            }
            let tau0 = (((2.0 * fs / fc).ceil()) as usize).max(8);

            // Hybrid = the exact minimum-phase IR delayed by τ0 samples (a
            // LINEAR shift — not circular, which would rotate the spectrum
            // in frequency). A linear shift preserves the magnitude
            // response bit-for-bit at every frequency (|DTFT| is
            // shift-invariant), so the hybrid's magnitude is identical to
            // the min render's — the phase-mode magnitude contract holds
            // exactly, including between the IR's grid bins. The group
            // delay becomes GD_min + τ0: minimum-phase behavior below the
            // crossover (bass keeps its transient alignment, just delayed
            // by τ0), converging to a near-constant τ0 where the correction
            // is spectrally smooth — the linear-phase branch above the
            // crossover, continuous to within the min-phase ripple.
            //
            // Minimum-phase IRs are energy-delay-minimized, so the last
            // τ0 samples dropped by the shift carry no correction content
            // (the rendered IR is finite-support); the truncation is
            // lossless in practice.
            let src = Spectrum::from_magnitude_db(mag_db, n, fs)?;
            let min_ir = minimum_phase_ir(&src)?;
            let mut samples = vec![0.0; n];
            samples[tau0..].copy_from_slice(&min_ir[..n - tau0]);
            Ok(RenderedIr {
                samples,
                n,
                sample_rate: fs,
                delay_samples: tau0 as f64,
                mode: PhaseMode::Hybrid,
            })
        }
    }
}
