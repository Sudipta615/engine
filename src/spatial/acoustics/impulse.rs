//! Impulse response generation, windowing, peak alignment, and conditioning (§11.1, Item 34).
//!
//! Provides Dirac impulses, window functions (Hann, Tukey, Blackman), peak arrival alignment,
//! and energy normalization.

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Supported windowing function types.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum WindowType {
    Rectangular,
    Hann,
    Hamming,
    Blackman,
    /// Tukey window (tapered cosine) with taper ratio alpha in [0.0, 1.0].
    Tukey(f64),
}

/// Generates window weights of the specified type and length.
pub fn generate_window(window_type: WindowType, length: usize) -> Vec<f64> {
    if length == 0 {
        return Vec::new();
    }
    if length == 1 {
        return vec![1.0];
    }

    let mut w = vec![1.0; length];
    let n_minus_1 = (length - 1) as f64;

    match window_type {
        WindowType::Rectangular => {}
        WindowType::Hann => {
            for (i, val) in w.iter_mut().enumerate() {
                *val = 0.5 * (1.0 - (2.0 * PI * i as f64 / n_minus_1).cos());
            }
        }
        WindowType::Hamming => {
            for (i, val) in w.iter_mut().enumerate() {
                *val = 0.54 - 0.46 * (2.0 * PI * i as f64 / n_minus_1).cos();
            }
        }
        WindowType::Blackman => {
            for (i, val) in w.iter_mut().enumerate() {
                let x = 2.0 * PI * i as f64 / n_minus_1;
                *val = 0.42 - 0.5 * x.cos() + 0.08 * (2.0 * x).cos();
            }
        }
        WindowType::Tukey(alpha) => {
            let alpha = alpha.clamp(0.0, 1.0);
            if alpha <= 0.0 {
                return w; // Rectangular
            }
            let edge = (alpha * n_minus_1 / 2.0).round() as usize;
            for i in 0..edge {
                let v = 0.5 * (1.0 + (PI * (2.0 * i as f64 / (alpha * n_minus_1) - 1.0)).cos());
                w[i] = v;
                w[length - 1 - i] = v;
            }
        }
    }

    w
}

/// Applies a window in-place to an impulse response.
pub fn apply_window(ir: &mut [f32], window_type: WindowType) {
    let w = generate_window(window_type, ir.len());
    for (s, &weight) in ir.iter_mut().zip(w.iter()) {
        *s = (*s as f64 * weight) as f32;
    }
}

/// Finds the direct arrival peak index and value.
pub fn find_direct_peak(ir: &[f32]) -> (usize, f32) {
    let mut best_idx = 0;
    let mut max_abs = 0.0f32;
    for (i, &s) in ir.iter().enumerate() {
        let abs = s.abs();
        if abs > max_abs {
            max_abs = abs;
            best_idx = i;
        }
    }
    (best_idx, ir.get(best_idx).copied().unwrap_or(0.0))
}

/// Normalizes an impulse response so that its peak magnitude is exactly 1.0 (0 dBFS).
pub fn normalize_peak(ir: &mut [f32]) {
    let max_abs = ir.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
    if max_abs > 1e-12 {
        let inv = 1.0 / max_abs;
        for s in ir.iter_mut() {
            *s *= inv;
        }
    }
}

/// Trims or isolates the direct sound component up to a time window (e.g. 5 ms)
/// with a smooth Tukey or Hann decay to exclude room reflections.
pub fn extract_direct_sound(ir: &[f32], sample_rate: f32, window_duration_ms: f32) -> Vec<f32> {
    let (peak_idx, _) = find_direct_peak(ir);
    let window_samples = ((window_duration_ms / 1000.0) * sample_rate).round() as usize;
    let pre_samples = (window_samples / 4).min(peak_idx);
    let start_idx = peak_idx - pre_samples;
    let end_idx = (start_idx + window_samples).min(ir.len());

    let mut direct = ir[start_idx..end_idx].to_vec();
    apply_window(&mut direct, WindowType::Tukey(0.25));
    direct
}
