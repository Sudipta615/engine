//! Phase response analysis, phase unwrapping, and minimum phase extraction (§11.1, Item 34).
//!
//! Provides spectral phase calculation, 2π jump unwrapping, and Hilbert-based minimum phase decomposition.

use realfft::num_complex::Complex32;
use realfft::RealFftPlanner;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Spectral phase response data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseResponse {
    pub frequencies: Vec<f64>,
    pub wrapped_phase_rad: Vec<f64>,
    pub unwrapped_phase_rad: Vec<f64>,
}

/// Unwraps phase discontinuities exceeding $\pi$ radians.
pub fn unwrap_phase(phase: &[f64]) -> Vec<f64> {
    if phase.is_empty() {
        return Vec::new();
    }

    let mut unwrapped = Vec::with_capacity(phase.len());
    unwrapped.push(phase[0]);
    let mut offset = 0.0;

    for i in 1..phase.len() {
        let diff = phase[i] - phase[i - 1];
        if diff > PI {
            offset -= 2.0 * PI;
        } else if diff < -PI {
            offset += 2.0 * PI;
        }
        unwrapped.push(phase[i] + offset);
    }

    unwrapped
}

/// Extracts unwrapped phase response from an impulse response.
pub fn compute_phase_response(ir: &[f32], sample_rate: f64) -> PhaseResponse {
    let mut fft_size = 1024;
    while fft_size < ir.len() {
        fft_size <<= 1;
    }

    let mut in_buf = vec![0.0f32; fft_size];
    in_buf[..ir.len()].copy_from_slice(ir);

    let num_bins = fft_size / 2 + 1;
    let mut planner = RealFftPlanner::<f32>::new();
    let rfft = planner.plan_fft_forward(fft_size);
    let mut spec = vec![Complex32::new(0.0, 0.0); num_bins];
    let _ = rfft.process(&mut in_buf, &mut spec);

    let bin_step = sample_rate / fft_size as f64;
    let mut frequencies = Vec::with_capacity(num_bins);
    let mut wrapped = Vec::with_capacity(num_bins);

    for (bin, s) in spec.iter().enumerate() {
        frequencies.push(bin as f64 * bin_step);
        let p = (s.im as f64).atan2(s.re as f64);
        wrapped.push(p);
    }

    let unwrapped = unwrap_phase(&wrapped);

    PhaseResponse {
        frequencies,
        wrapped_phase_rad: wrapped,
        unwrapped_phase_rad: unwrapped,
    }
}
