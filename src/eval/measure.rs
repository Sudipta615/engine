//! Measurement primitives shared by the quality-evaluation suites.
//!
//! Pure, deterministic functions on sample buffers — no audio I/O, no
//! allocation beyond what a caller provides (the FFT-free measurements are
//! O(n) Goertzel-style DFT bins). Each returns a `f64` in documented units so
//! a [`super::CheckResult`] can attach a nominal + tolerance.

/// Root-mean-square of a buffer (linear amplitude, `0.0` for empty).
pub fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt()
}

/// Peak absolute amplitude (linear).
pub fn peak(samples: &[f32]) -> f64 {
    samples
        .iter()
        .map(|&s| s.abs() as f64)
        .fold(0.0f64, f64::max)
}

/// Linear → dB (`-inf` for non-positive, `-200`-floored for display safety).
pub fn db(linear: f64) -> f64 {
    if linear > 1e-18 {
        20.0 * linear.log10()
    } else if linear > 0.0 {
        -200.0
    } else {
        f64::NEG_INFINITY
    }
}

/// Amplitude (linear peak) of the component of `samples` at `freq` Hz, via a
/// single DFT bin (Goertzel-like). Deterministic and window-free (the whole
/// buffer is the analysis window), matching the fidelity-suite convention.
pub fn sine_amplitude(samples: &[f32], sample_rate: f64, freq: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let n = samples.len();
    let t = std::f64::consts::TAU * freq / sample_rate;
    let mut re = 0.0;
    let mut im = 0.0;
    for (k, &x) in samples.iter().enumerate() {
        let x = x as f64;
        re += x * (t * k as f64).cos();
        im += x * (t * k as f64).sin();
    }
    2.0 * (re * re + im * im).sqrt() / n as f64
}

/// Total-harmonic-distortion **plus noise** of a steady `freq` tone as a
/// linear fraction (`0.0` = clean, `1.0` = 100% distortion). The fundamental
/// is located by a DFT bin and the residual (total power − fundamental
/// power) is ratioed against it. Returns `0.0` when no fundamental is
/// present (nothing to distort against).
pub fn thd_plus_n(samples: &[f32], sample_rate: f64, freq: f64) -> f64 {
    let amp = sine_amplitude(samples, sample_rate, freq);
    if amp <= 1e-9 {
        return 0.0;
    }
    let total_rms = rms(samples);
    let fund_rms = amp / std::f64::consts::SQRT_2;
    let residual = (total_rms * total_rms - fund_rms * fund_rms)
        .max(0.0)
        .sqrt();
    residual / fund_rms
}

/// Fraction of sample positions where `a` and `b` differ (`0.0` = bit exact,
/// `1.0` = completely different). Lengths need not match; the shorter bounds
/// the comparison.
pub fn mismatch_fraction(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut mismatches = 0usize;
    for i in 0..n {
        // `!==` on the exact f32 bits — a bit-perfect stage must match exactly.
        if f32::from_bits(a[i].to_bits()) != f32::from_bits(b[i].to_bits()) {
            mismatches += 1;
        }
    }
    mismatches as f64 / n as f64
}

/// Magnitude (dB) of a filter's impulse response at `freq` Hz via a DTFT bin.
/// `ir` is the (truncated, well-decayed) impulse response. Use for linear
/// stages (EQ biquads) where the transfer function is measured directly.
pub fn ir_magnitude_db(ir: &[f32], sample_rate: f64, freq: f64) -> f64 {
    let t = std::f64::consts::TAU * freq / sample_rate;
    let mut re = 0.0;
    let mut im = 0.0;
    for (k, &x) in ir.iter().enumerate() {
        let x = x as f64;
        re += x * (t * k as f64).cos();
        im += x * (t * k as f64).sin();
    }
    db((re * re + im * im).sqrt())
}

/// Peak magnitude (dB) of `ir` over `freqs` — the classic way to read a
/// peaking filter's centre gain without depending on where its exact centre
/// bin falls.
pub fn ir_peak_magnitude_db(
    ir: &[f32],
    sample_rate: f64,
    freqs: impl IntoIterator<Item = f64>,
) -> f64 {
    freqs
        .into_iter()
        .map(|f| ir_magnitude_db(ir, sample_rate, f))
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Linear peak of `samples` in dB relative to `ceiling_linear` (≤ 0 means at
/// or below the ceiling). Used for true-peak / limiter-ceiling compliance.
pub fn peak_error_db(samples: &[f32], ceiling_linear: f64) -> f64 {
    db(peak(samples) / ceiling_linear.max(1e-18))
}

/// Phase (degrees, `[-180, 180]`) of a filter's impulse response at `freq`,
/// from the DTFT bin. Used for phase-deviation checks on linear stages.
pub fn ir_phase_deg(ir: &[f32], sample_rate: f64, freq: f64) -> f64 {
    let t = std::f64::consts::TAU * freq / sample_rate;
    let mut re = 0.0;
    let mut im = 0.0;
    for (k, &x) in ir.iter().enumerate() {
        let x = x as f64;
        re += x * (t * k as f64).cos();
        im += x * (t * k as f64).sin();
    }
    im.atan2(re).to_degrees()
}

/// Error of `got` vs `want` (same length) as dB relative to the larger signal
/// peak — the impulse-response comparison used for a rendered acoustic path
/// against a naive-direct reference. `0 → -inf` (perfect); a `floor_db`
/// clamps the reported value so an exact match prints as a finite number.
pub fn peak_error_db_between(got: &[f32], want: &[f32], floor_db: f64) -> f64 {
    let n = got.len().min(want.len());
    if n == 0 {
        return -floor_db;
    }
    let ref_peak = got.iter().zip(want).fold(0.0f64, |m, (g, w)| {
        m.max(g.abs() as f64).max(w.abs() as f64)
    });
    let mut worst = 0.0f64;
    for i in 0..n {
        let e = (got[i] as f64 - want[i] as f64).abs();
        if e > worst {
            worst = e;
        }
    }
    let err_db = db(worst / ref_peak.max(1e-18));
    if err_db < -floor_db {
        -floor_db
    } else {
        err_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(sr: f64, freq: f64, secs: f64) -> Vec<f32> {
        let n = (sr * secs) as usize;
        (0..n)
            .map(|i| 0.5 * (std::f64::consts::TAU * freq * i as f64 / sr).sin() as f32)
            .collect()
    }

    #[test]
    fn sine_amplitude_recovers_a_tone_without_leakage_artifacts() {
        let sr = 48_000.0;
        let x = tone(sr, 1_000.0, 1.0);
        let a = sine_amplitude(&x, sr, 1_000.0);
        assert!((a - 0.5).abs() < 1e-3, "amplitude {a}");
        // An off-bin frequency reads near zero.
        let e = sine_amplitude(&x, sr, 1_237.0);
        assert!(e < 0.05, "off-bin leakage {e}");
    }

    #[test]
    fn thd_plus_n_of_a_clean_tone_is_near_zero() {
        let sr = 48_000.0;
        let x = tone(sr, 1_000.0, 1.0);
        assert!(thd_plus_n(&x, sr, 1_000.0) < 1e-6);
    }

    #[test]
    fn mismatch_fraction_is_bit_sensitive() {
        let a = tone(48_000.0, 1_000.0, 0.1);
        let b = a.clone();
        assert_eq!(mismatch_fraction(&a, &b), 0.0);
        let mut c = a.clone();
        c[10] = c[10].next_up();
        assert!(mismatch_fraction(&a, &c) > 0.0);
    }

    #[test]
    fn ir_magnitude_detects_a_peaking_boost() {
        use crate::dsp::biquad::{BiquadCoeffsF64, BiquadStateF64};
        let sr = 48_000.0f64;
        let coeffs = BiquadCoeffsF64::peaking(sr as f32, 1_000.0, 6.0, 1.0);
        let mut state = BiquadStateF64::default();
        let n = 8_192;
        let mut ir = Vec::with_capacity(n);
        for k in 0..n {
            let x = if k == 0 { 1.0 } else { 0.0 };
            ir.push(state.process(x, &coeffs) as f32);
        }
        let peak_gain = ir_peak_magnitude_db(&ir, sr, (100..=2_000).step_by(25).map(|f| f as f64));
        assert!(
            (peak_gain - 6.0).abs() < 0.5,
            "peaking peak gain {peak_gain:.3} dB"
        );
        let far = ir_magnitude_db(&ir, sr, 8_000.0);
        assert!(far.abs() < 0.3, "far-band gain {far:.3} dB");
    }
}
