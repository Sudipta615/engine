//! Logarithmic sine sweep generation, matched inverse filtering, and deconvolution (§11.1, Item 34).
//!
//! Implements Angelo Farina's logarithmic sine sweep method for acoustic impulse response capture.

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Configuration for logarithmic sine sweep generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogSweepConfig {
    /// Start frequency in Hz (typically 20.0 Hz).
    pub start_freq_hz: f64,
    /// Stop frequency in Hz (typically 20000.0 Hz or Nyquist).
    pub stop_freq_hz: f64,
    /// Duration of the sweep in seconds.
    pub duration_secs: f64,
    /// Sample rate in Hz.
    pub sample_rate: f64,
    /// Fade in/out duration in seconds to prevent boundary clicks.
    pub fade_duration_secs: f64,
}

impl Default for LogSweepConfig {
    fn default() -> Self {
        Self {
            start_freq_hz: 20.0,
            stop_freq_hz: 20000.0,
            duration_secs: 5.0,
            sample_rate: 48000.0,
            fade_duration_secs: 0.05,
        }
    }
}

/// Generates a logarithmic sine sweep according to Farina's method.
pub fn generate_log_sweep(config: &LogSweepConfig) -> Vec<f32> {
    let num_samples = (config.duration_secs * config.sample_rate).round() as usize;
    if num_samples == 0 {
        return Vec::new();
    }

    let w1 = 2.0 * PI * config.start_freq_hz;
    let w2 = 2.0 * PI * config.stop_freq_hz;
    let t_total = config.duration_secs;
    let log_ratio = (w2 / w1).ln();

    let mut sweep = Vec::with_capacity(num_samples);
    let fade_samples = (config.fade_duration_secs * config.sample_rate).round() as usize;

    for i in 0..num_samples {
        let t = i as f64 / config.sample_rate;
        // Farina: phi(t) = w1 * T / ln(w2/w1) * (exp(t/T * ln(w2/w1)) - 1)
        let phi = (w1 * t_total / log_ratio) * ((t / t_total * log_ratio).exp() - 1.0);
        let mut sample = phi.sin() as f32;

        // Apply cosine taper fade-in / fade-out
        if i < fade_samples && fade_samples > 0 {
            let ramp = 0.5 * (1.0 - (PI * i as f64 / fade_samples as f64).cos());
            sample *= ramp as f32;
        } else if i + fade_samples >= num_samples && fade_samples > 0 {
            let remaining = num_samples - 1 - i;
            let ramp = 0.5 * (1.0 - (PI * remaining as f64 / fade_samples as f64).cos());
            sample *= ramp as f32;
        }

        sweep.push(sample);
    }

    sweep
}

/// Generates the matched inverse filter for a logarithmic sine sweep.
///
/// Time-reverses the sweep and applies 6 dB/octave attenuation ($1/f$) envelope
/// so that convolution of sweep and inverse filter yields an ideal Dirac delta.
pub fn generate_inverse_filter(sweep: &[f32], config: &LogSweepConfig) -> Vec<f32> {
    let n = sweep.len();
    if n == 0 {
        return Vec::new();
    }

    let mut inverse = vec![0.0f32; n];
    let w1 = 2.0 * PI * config.start_freq_hz;
    let w2 = 2.0 * PI * config.stop_freq_hz;
    let log_ratio = (w2 / w1).ln();

    for i in 0..n {
        let t = i as f64 / config.sample_rate;
        // Amplitude scaling envelope: exp(-t / T * ln(w2/w1))
        let modulation = (-t / config.duration_secs * log_ratio).exp();
        // Time reversal
        let reversed_sample = sweep[n - 1 - i] as f64;
        inverse[i] = (reversed_sample * modulation) as f32;
    }

    // Normalize so that direct convolution has unit impulse peak
    let energy: f64 = inverse.iter().map(|&x| (x as f64).powi(2)).sum();
    if energy > 0.0 {
        let norm = 1.0 / energy.sqrt();
        for x in inverse.iter_mut() {
            *x = (*x as f64 * norm) as f32;
        }
    }

    inverse
}

/// Deconvolves a recorded response with the inverse filter to extract the impulse response.
pub fn deconvolve_sweep(recording: &[f32], inverse_filter: &[f32]) -> Vec<f32> {
    if recording.is_empty() || inverse_filter.is_empty() {
        return Vec::new();
    }

    // Direct linear convolution for precision
    let out_len = recording.len() + inverse_filter.len() - 1;
    let mut ir = vec![0.0f32; out_len];

    for (i, &rec) in recording.iter().enumerate() {
        if rec.abs() < 1e-9 {
            continue;
        }
        let rec_val = rec;
        for (j, &inv) in inverse_filter.iter().enumerate() {
            ir[i + j] += rec_val * inv;
        }
    }

    // Normalize peak to unity if non-zero
    let max_val = ir.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
    if max_val > 1e-9 {
        let scale = 1.0 / max_val;
        for x in ir.iter_mut() {
            *x *= scale;
        }
    }

    ir
}
