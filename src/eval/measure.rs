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

/// Signal-to-Noise Ratio (SNR) in decibels.
pub fn snr_db(signal: &[f32], noise: &[f32]) -> f64 {
    let s_rms = rms(signal);
    let n_rms = rms(noise);
    if n_rms <= 1e-12 {
        120.0
    } else if s_rms <= 1e-12 {
        -120.0
    } else {
        20.0 * (s_rms / n_rms).log10()
    }
}

/// Intermodulation distortion (IMD) via SMPTE / twin-tone excitation (Item 34).
/// Ratio of modulation sidebands (|f2 - f1|, |f2 + f1|) to the fundamental f2.
pub fn intermod_distortion_smpte(samples: &[f32], sample_rate: f64, f1: f64, f2: f64) -> f64 {
    let a_f2 = sine_amplitude(samples, sample_rate, f2);
    if a_f2 <= 1e-9 {
        return 0.0;
    }
    let lower_sideband = sine_amplitude(samples, sample_rate, (f2 - f1).abs());
    let upper_sideband = sine_amplitude(samples, sample_rate, f2 + f1);
    let sideband_rms = (lower_sideband * lower_sideband + upper_sideband * upper_sideband).sqrt();
    sideband_rms / a_f2
}

/// Phase in radians at `freq` Hz of an impulse response via DTFT bin.
pub fn ir_phase_rad(ir: &[f32], sample_rate: f64, freq: f64) -> f64 {
    let t = std::f64::consts::TAU * freq / sample_rate;
    let mut re = 0.0;
    let mut im = 0.0;
    for (k, &x) in ir.iter().enumerate() {
        let x = x as f64;
        re += x * (t * k as f64).cos();
        im += x * (t * k as f64).sin();
    }
    (-im).atan2(re)
}

/// Numerical group delay in samples (-d(phi)/d(omega)) at `freq` Hz (Item 34).
pub fn group_delay_samples(ir: &[f32], sample_rate: f64, freq: f64) -> f64 {
    let delta_f = 1.0; // 1 Hz step
    let f_low = (freq - delta_f).max(1.0);
    let f_high = (freq + delta_f).min(sample_rate * 0.49);
    let phi_low = ir_phase_rad(ir, sample_rate, f_low);
    let phi_high = ir_phase_rad(ir, sample_rate, f_high);
    let delta_omega = std::f64::consts::TAU * (f_high - f_low) / sample_rate;
    let mut diff = phi_high - phi_low;
    while diff > std::f64::consts::PI {
        diff -= std::f64::consts::TAU;
    }
    while diff < -std::f64::consts::PI {
        diff += std::f64::consts::TAU;
    }
    -diff / delta_omega
}

/// Cross-correlation interaural time difference (ITD) in fractional samples between two channels.
pub fn itd_error_samples(left: &[f32], right: &[f32]) -> f64 {
    let n = left.len().min(right.len());
    if n < 32 {
        return 0.0;
    }
    let max_lag = 128.min(n / 2) as isize;
    let mut best_lag = 0isize;
    let mut max_corr = -f64::INFINITY;

    for lag in -max_lag..=max_lag {
        let mut corr = 0.0f64;
        for (i, &l) in left.iter().enumerate().take(n) {
            let j = i as isize + lag;
            if j >= 0 && (j as usize) < n {
                corr += (l as f64) * (right[j as usize] as f64);
            }
        }
        if corr > max_corr {
            max_corr = corr;
            best_lag = lag;
        }
    }
    best_lag as f64
}

/// Interaural level difference (ILD) in decibels between two channels.
pub fn ild_error_db(left: &[f32], right: &[f32]) -> f64 {
    let l_rms = rms(left);
    let r_rms = rms(right);
    if l_rms <= 1e-9 && r_rms <= 1e-9 {
        0.0
    } else if r_rms <= 1e-9 {
        100.0
    } else if l_rms <= 1e-9 {
        -100.0
    } else {
        20.0 * (l_rms / r_rms).log10()
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

    #[test]
    fn snr_and_imd_and_spatial_metrics_evaluate_correctly() {
        let sr = 48_000.0;
        let signal = tone(sr, 1_000.0, 0.1);
        let noise = vec![0.001f32; signal.len()];
        let snr = snr_db(&signal, &noise);
        assert!(snr > 40.0, "SNR: {snr} dB");

        // Zero noise has 120 dB SNR
        let zero_noise = vec![0.0f32; signal.len()];
        let snr_inf = snr_db(&signal, &zero_noise);
        assert_eq!(snr_inf, 120.0);

        // IMD on pure two-tone sum should be very low
        let f1 = 60.0;
        let f2 = 7_000.0;
        let mut smpte_two_tone = vec![0.0f32; 48000];
        for (i, sample) in smpte_two_tone.iter_mut().enumerate() {
            let t = i as f64 / sr;
            *sample = (0.8 * (2.0 * std::f64::consts::PI * f1 * t).sin()
                + 0.2 * (2.0 * std::f64::consts::PI * f2 * t).sin()) as f32;
        }
        let imd = intermod_distortion_smpte(&smpte_two_tone, sr, f1, f2);
        assert!(imd < 0.01, "Clean signal IMD should be < 1%, got {imd}");

        // Unit impulse delay = 0
        let mut ir = vec![0.0f32; 128];
        ir[0] = 1.0;
        let phase = ir_phase_rad(&ir, sr, 1_000.0);
        assert!(phase.abs() < 1e-4, "phase {phase}");
        let gd = group_delay_samples(&ir, sr, 1_000.0);
        assert!(gd.abs() < 1e-3, "group delay {gd}");

        // Delay impulse by 5 samples
        let mut ir_delayed = vec![0.0f32; 128];
        ir_delayed[5] = 1.0;
        let gd5 = group_delay_samples(&ir_delayed, sr, 1_000.0);
        assert!((gd5 - 5.0).abs() < 0.1, "delayed group delay {gd5}");

        // ITD & ILD
        let mut left = vec![0.0f32; 64];
        let mut right = vec![0.0f32; 64];
        left[10] = 1.0;
        right[12] = 1.0;
        let itd_lag = itd_error_samples(&left, &right);
        assert!((itd_lag - 2.0).abs() < 0.1, "ITD lag {itd_lag}");

        let left_amp = vec![1.0f32; 100];
        let right_amp = vec![0.5f32; 100]; // -6.02 dB
        let ild_db = ild_error_db(&left_amp, &right_amp);
        assert!((ild_db - 6.0206).abs() < 0.1, "ILD db {ild_db}");
    }
}
