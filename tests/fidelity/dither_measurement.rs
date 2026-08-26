//! Fidelity tests — Dither measurement suite (spec §12 measurement claims)
//!
//! Makes quantitative measurements of the dither/quantization boundary:
//!
//! * **Decorrelation** — without dither, quantization error correlates with
//!   the signal and shows up as harmonic distortion; TPDF dither must
//!   eliminate that distortion (THD collapses to the noise floor);
//! * **Noise amplitude** — the dithered quantization-error RMS of silence
//!   must match the theoretical TPDF value q/√12 ≈ 0.289 LSB;
//! * **Noise shaping** — HP-TPDF and Shibata must tilt the noise spectrum
//!   away from low frequencies relative to flat TPDF;
//! * **Float-output guard** — `set_output_is_float(true)` must be a pure
//!   passthrough (no noise added to a float path);
//! * **Bit-depth scaling** — a 24-bit boundary must measure ~48 dB quieter
//!   than 16-bit (6.02 dB per bit).

use engine::dsp::dither::{Dither, DitherType};

const SR: u32 = 48_000;

fn quant_step(bit_depth: u32) -> f32 {
    2.0 / (1u64 << (bit_depth - 1)) as f32
}

fn rms(samples: &[f32]) -> f32 {
    let sum_sq: f64 = samples.iter().map(|&s| s as f64 * s as f64).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

/// Harmonic amplitude via DFT at k·freq (integer cycles ⇒ exact).
fn harmonic_amp(signal: &[f32], freq: f32, k: u32) -> f32 {
    let n = signal.len();
    let mut re = 0.0f64;
    let mut im = 0.0f64;
    for (i, &s) in signal.iter().enumerate() {
        let phase = 2.0 * std::f64::consts::PI * freq as f64 * k as f64 * i as f64 / SR as f64;
        re += s as f64 * phase.cos();
        im += s as f64 * phase.sin();
    }
    (2.0 * (re * re + im * im).sqrt() / n as f64) as f32
}

fn thd(signal: &[f32], freq: f32) -> f32 {
    let fund = harmonic_amp(signal, freq, 1);
    if fund < 1e-12 {
        return f32::INFINITY;
    }
    let mut dist = 0.0f64;
    for k in 2..=8 {
        let h = harmonic_amp(signal, freq, k) as f64;
        dist += h * h;
    }
    (dist.sqrt() / fund as f64) as f32
}

/// Band-averaged PSD via Hann-windowed FFT: mean per-bin power in
/// [lo_hz, hi_hz]. A single-bin DFT with a rectangular window leaks
/// broadband noise energy into every bin, which masks noise-shaping
/// transfer functions (HP-TPDF/Shibata null the low end by 20+ dB but
/// the leakage floor pins their measured LF power near the flat-TPDF
/// level). The Hann window suppresses sidelobes by ~30 dB, so the
/// measured PSD tracks the true shaping curve; averaging thousands of
/// bins also makes the estimate far tighter than a handful of DFT
/// projections.
fn band_power_psd(signal: &[f32], lo_hz: f32, hi_hz: f32) -> f64 {
    let n = signal.len();
    let mut planner = realfft::RealFftPlanner::<f32>::new();
    let r2c = planner.plan_fft_forward(n);
    let mut windowed: Vec<f32> = Vec::with_capacity(n);
    for (i, &s) in signal.iter().enumerate() {
        let w = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos());
        windowed.push(s * w);
    }
    let mut spectrum = r2c.make_output_vec();
    r2c.process(&mut windowed, &mut spectrum).unwrap();

    let bin_hz = SR as f64 / n as f64;
    let k0 = (lo_hz as f64 / bin_hz).ceil() as usize;
    let k1 = ((hi_hz as f64 / bin_hz).floor() as usize).min(spectrum.len() - 1);
    if k1 < k0 {
        return 0.0;
    }
    let mut power = 0.0f64;
    let mut count = 0usize;
    for c in &spectrum[k0..=k1] {
        power += (c.re as f64) * (c.re as f64) + (c.im as f64) * (c.im as f64);
        count += 1;
    }
    power / count as f64
}

/// H4: quantization without dither correlates the error with the signal
/// (harmonic distortion); TPDF dither decorrelates it. Measured at a
/// low-level sine where the effect is large.
#[test]
fn dither_tpdf_removes_harmonic_distortion() {
    let bit_depth = 16u32;
    let freq = 1000.0_f32;
    let amplitude = 10.0_f32.powf(-60.0 / 20.0); // -60 dBFS ≈ 32.8 LSB at 16 bit
                                                 // 800 cycles: long enough that the (white) dithered noise at each
                                                 // harmonic bin averages far below the distortion threshold.
    let cycles = 800usize;
    let n = (SR as f32 / freq * cycles as f32) as usize;

    let signal: Vec<f32> = (0..n)
        .map(|i| amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / SR as f32).sin())
        .collect();

    // No dither (pure quantization). `DitherType::None` is a documented
    // passthrough (quantization only ever happens *with* dithering), so the
    // undithered reference quantizes directly with the same rounding.
    let quant_steps = 1u64 << (bit_depth - 1);
    let qf = quant_steps as f32;
    let out_plain: Vec<f32> = signal
        .iter()
        .map(|&s| ((s * qf).round() / qf).clamp(-1.0, 1.0))
        .collect();
    let thd_plain = thd(&out_plain, freq);

    // TPDF dither.
    let mut tpdf = Dither::new(DitherType::Triangular, bit_depth);
    let out_tpdf: Vec<f32> = signal.iter().map(|&s| tpdf.process(s, s).0).collect();
    let thd_tpdf = thd(&out_tpdf, freq);

    // The correlated (undithered) error must be dramatically worse.
    assert!(
        thd_plain > 0.001,
        "undithered quantization should show measurable distortion, got {:.4} %",
        thd_plain * 100.0
    );
    assert!(
        thd_tpdf < 0.001,
        "TPDF dither must decorrelate quantization error, THD {:.4} %",
        thd_tpdf * 100.0
    );
    // The dithered "THD" is the white-noise floor leaking into the harmonic
    // bins; it must sit well below the deterministic distortion of the
    // undithered quantization.
    assert!(
        thd_tpdf < thd_plain / 4.0,
        "TPDF THD ({:.4} %) must be far below undithered ({:.4} %)",
        thd_tpdf * 100.0,
        thd_plain * 100.0
    );
}

/// H4: the TPDF-dithered quantization error of silence must have the
/// theoretical RMS of q/√12 (0.2887 LSB), within a 25 % measurement margin.
#[test]
fn dither_tpdf_noise_rms_matches_theory() {
    let bit_depth = 16u32;
    let q = quant_step(bit_depth);
    let n = 1_000_000usize;
    let mut dither = Dither::new(DitherType::Triangular, bit_depth);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(dither.process(0.0, 0.0).0);
    }
    let measured = rms(&out);
    let theory = q / 12.0f32.sqrt(); // q/√12
    assert!(
        (measured / theory - 1.0).abs() < 0.25,
        "TPDF noise RMS {:.3e} vs theory {:.3e} (q={:.3e})",
        measured,
        theory,
        q
    );
}

/// H4: HP-TPDF and Shibata must move noise energy away from low frequencies
/// relative to flat TPDF, while staying within the same total budget.
#[test]
fn dither_noise_shaping_tilts_spectrum() {
    let bit_depth = 16u32;
    // 100k samples keeps the band-PSD measurement fast in debug builds while
    // remaining statistically stable: the Hann-windowed FFT averages ~4k LF
    // and ~10k HF bins, so band-power estimates vary by only ~1-2% run to run
    // despite the dither RNG being wall-clock seeded.
    let n = 100_000usize;

    let mut tpdf = Dither::new(DitherType::Triangular, bit_depth);
    let out_tpdf: Vec<f32> = (0..n).map(|_| tpdf.process(0.0, 0.0).0).collect();

    let mut hp = Dither::new(DitherType::HighPassTriangular, bit_depth);
    let out_hp: Vec<f32> = (0..n).map(|_| hp.process(0.0, 0.0).0).collect();

    let mut shibata = Dither::new(DitherType::Shibata, bit_depth);
    let out_shibata: Vec<f32> = (0..n).map(|_| shibata.process(0.0, 0.0).0).collect();

    let (lf_t, hf_t) = (
        band_power_psd(&out_tpdf, 100.0, 2000.0),
        band_power_psd(&out_tpdf, 18_000.0, 23_000.0),
    );
    let lf_hp = band_power_psd(&out_hp, 100.0, 2000.0);
    let (lf_s, hf_s) = (
        band_power_psd(&out_shibata, 100.0, 2000.0),
        band_power_psd(&out_shibata, 18_000.0, 23_000.0),
    );

    // Flat TPDF has no spectral tilt (LF ≈ HF within 6 dB).
    assert!(
        (lf_t / hf_t).abs() < 4.0,
        "flat TPDF should be roughly spectrally flat, LF/HF ratio {:.2}",
        lf_t / hf_t
    );
    // HP-TPDF nulls the low end: LF power must drop relative to TPDF.
    // The measured ratio bottoms out at ≈1/3, not 0: `process()` quantizes
    // after adding dither, and the rounding error is spectrally white with
    // power q²/12 — exactly half of the TPDF dither power (2·δ²/3 vs δ²/3,
    // δ = half LSB). That flat floor caps any shaping benefit visible in the
    // quantized output at 0.5; the true pre-quantization HP null is ~50 dB.
    assert!(
        lf_hp < lf_t * 0.5,
        "HP-TPDF must cut low-frequency noise: {lf_hp:.2e} vs TPDF {lf_t:.2e}"
    );
    // Shibata has the strongest HF tilt (9-tap F-weighted shaping).
    assert!(
        hf_s > hf_t * 2.0,
        "Shibata must push noise to HF: {hf_s:.2e} vs TPDF {hf_t:.2e}"
    );
    assert!(
        lf_s < lf_t,
        "Shibata must reduce LF noise below TPDF: {lf_s:.2e} vs {lf_t:.2e}"
    );
}

/// H4: a float output path must never be dithered — `set_output_is_float`
/// engages a pure passthrough even with dither configured.
#[test]
fn dither_float_output_guard_is_passthrough() {
    let mut dither = Dither::new(DitherType::Triangular, 16);
    dither.set_output_is_float(true);
    let mut max_diff = 0.0f32;
    let mut x = 0.1234567f32;
    for _ in 0..100_000 {
        x = (x * 1.0001 + 0.5).fract() * 2.0 - 1.0;
        let (l, r) = dither.process(x, -x);
        max_diff = max_diff.max((l - x).abs().max((r + x).abs()));
    }
    assert!(
        max_diff < 1e-6,
        "float-output guard must be exact passthrough, max diff {max_diff:.2e}"
    );
    assert!(
        !dither.is_enabled(),
        "float path must report dither inactive"
    );
}

/// H4: the noise floor must scale with bit depth — 24-bit dithered silence
/// must measure ≈ 48 dB (6.02 dB × 8 bits) below 16-bit.
#[test]
fn dither_noise_floor_scales_with_bit_depth() {
    let n = 1_000_000usize;
    // Noise floor in dBFS: RMS relative to full scale (1.0).
    let measure = |bits: u32| -> f32 {
        let mut d = Dither::new(DitherType::Triangular, bits);
        let out: Vec<f32> = (0..n).map(|_| d.process(0.0, 0.0).0).collect();
        20.0 * rms(&out).log10()
    };
    let db16 = measure(16);
    let db24 = measure(24);
    let delta = db16 - db24;
    assert!(
        (delta - 48.2).abs() < 3.0,
        "16→24 bit noise floor must drop ~48.2 dB (6.02 dB/bit), measured {delta:.2} dB"
    );
}
