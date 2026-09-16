//! Dual-channel FFT transfer function and acoustic coherence estimation (§11.1, Item 34).
//!
//! Computes H1(f) transfer functions, cross-power spectral densities, and
//! spectral coherence metrics using overlapped windowed FFT averaging.

use realfft::num_complex::Complex32;
use realfft::RealFftPlanner;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Spectral transfer function and coherence estimation results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferFunctionEstimate {
    /// Center frequencies for each FFT bin (Hz).
    pub frequencies: Vec<f64>,
    /// Magnitude response in dBFS ($20 \log_{10} |H(f)|$).
    pub magnitude_db: Vec<f64>,
    /// Phase response in radians ($-\pi .. +\pi$).
    pub phase_rad: Vec<f64>,
    /// Spectral coherence function ($0.0 \le C_{xy}(f) \le 1.0$).
    pub coherence: Vec<f64>,
    /// Sample rate in Hz.
    pub sample_rate: f64,
}

/// Computes the H1 transfer function and coherence between input signal $x$ and output signal $y$.
pub fn estimate_transfer_function(
    input: &[f32],
    output: &[f32],
    sample_rate: f64,
    fft_size: usize,
) -> TransferFunctionEstimate {
    assert!(fft_size.is_power_of_two(), "FFT size must be power of two");
    let num_bins = fft_size / 2 + 1;

    let mut planner = RealFftPlanner::<f32>::new();
    let rfft = planner.plan_fft_forward(fft_size);

    let mut s_xx = vec![0.0f64; num_bins];
    let mut s_yy = vec![0.0f64; num_bins];
    let mut s_xy_re = vec![0.0f64; num_bins];
    let mut s_xy_im = vec![0.0f64; num_bins];

    // Hann window
    let mut window = vec![0.0f32; fft_size];
    for (i, w) in window.iter_mut().enumerate() {
        *w = (0.5 * (1.0 - (2.0 * PI * i as f64 / (fft_size - 1) as f64).cos())) as f32;
    }

    let hop_size = fft_size / 2; // 50% overlap
    let total_samples = input.len().min(output.len());
    let mut block_count = 0;

    let mut in_buf = vec![0.0f32; fft_size];
    let mut out_buf = vec![0.0f32; fft_size];
    let mut in_spec = vec![Complex32::new(0.0, 0.0); num_bins];
    let mut out_spec = vec![Complex32::new(0.0, 0.0); num_bins];

    let mut offset = 0;
    while offset + fft_size <= total_samples {
        // Window input
        for i in 0..fft_size {
            in_buf[i] = input[offset + i] * window[i];
            out_buf[i] = output[offset + i] * window[i];
        }

        let _ = rfft.process(&mut in_buf, &mut in_spec);
        let _ = rfft.process(&mut out_buf, &mut out_spec);

        for bin in 0..num_bins {
            let x = in_spec[bin];
            let y = out_spec[bin];

            // Sxx = X * X*
            let p_xx = (x.re * x.re + x.im * x.im) as f64;
            // Syy = Y * Y*
            let p_yy = (y.re * y.re + y.im * y.im) as f64;
            // Sxy = Y * X* = (y.re + j y.im) * (x.re - j x.im)
            let p_xy_re = (y.re * x.re + y.im * x.im) as f64;
            let p_xy_im = (y.im * x.re - y.re * x.im) as f64;

            s_xx[bin] += p_xx;
            s_yy[bin] += p_yy;
            s_xy_re[bin] += p_xy_re;
            s_xy_im[bin] += p_xy_im;
        }

        block_count += 1;
        offset += hop_size;
    }

    if block_count == 0 {
        // Fallback for too short signals: return identity
        return TransferFunctionEstimate {
            frequencies: vec![0.0],
            magnitude_db: vec![0.0],
            phase_rad: vec![0.0],
            coherence: vec![1.0],
            sample_rate,
        };
    }

    let bin_freq_step = sample_rate / fft_size as f64;
    let mut frequencies = Vec::with_capacity(num_bins);
    let mut magnitude_db = Vec::with_capacity(num_bins);
    let mut phase_rad = Vec::with_capacity(num_bins);
    let mut coherence = Vec::with_capacity(num_bins);

    for bin in 0..num_bins {
        frequencies.push(bin as f64 * bin_freq_step);

        let p_xx = s_xx[bin];
        let p_yy = s_yy[bin];
        let xy_re = s_xy_re[bin];
        let xy_im = s_xy_im[bin];

        if p_xx > 1e-12 {
            // H1 = Sxy / Sxx
            let h_re = xy_re / p_xx;
            let h_im = xy_im / p_xx;
            let mag = (h_re * h_re + h_im * h_im).sqrt();
            let db = if mag > 1e-9 {
                20.0 * mag.log10()
            } else {
                -180.0
            };
            magnitude_db.push(db);
            phase_rad.push(h_im.atan2(h_re));
        } else {
            magnitude_db.push(-180.0);
            phase_rad.push(0.0);
        }

        // Coherence = |Sxy|^2 / (Sxx * Syy)
        let num = xy_re * xy_re + xy_im * xy_im;
        let denom = p_xx * p_yy;
        if denom > 1e-12 {
            let coh = (num / denom).clamp(0.0, 1.0);
            coherence.push(coh);
        } else {
            coherence.push(0.0);
        }
    }

    TransferFunctionEstimate {
        frequencies,
        magnitude_db,
        phase_rad,
        coherence,
        sample_rate,
    }
}
