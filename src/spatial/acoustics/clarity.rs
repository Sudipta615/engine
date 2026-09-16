//! Acoustic clarity metrics: C50, C80, D50, and Center Time (Ts) (§11.1, Item 34).
//!
//! Implements ISO 3382-1 speech and music clarity energy ratios.

use super::impulse::find_direct_peak;
use serde::{Deserialize, Serialize};

/// Acoustic clarity and definition metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClarityMetrics {
    /// C50 speech clarity index (dB).
    pub c50_db: f64,
    /// C80 music clarity index (dB).
    pub c80_db: f64,
    /// D50 definition (0.0 to 1.0, or 0% to 100%).
    pub d50: f64,
    /// Center time Ts in milliseconds.
    pub center_time_ms: f64,
}

/// Calculates C50, C80, D50, and center time Ts from an impulse response.
pub fn compute_clarity(ir: &[f32], sample_rate: f64) -> ClarityMetrics {
    if ir.is_empty() || sample_rate <= 0.0 {
        return ClarityMetrics {
            c50_db: 0.0,
            c80_db: 0.0,
            d50: 1.0,
            center_time_ms: 0.0,
        };
    }

    // Align to direct arrival peak
    let (peak_idx, _) = find_direct_peak(ir);
    let sample_slice = &ir[peak_idx..];
    let n = sample_slice.len();

    let split_50ms = ((0.050 * sample_rate).round() as usize).min(n);
    let split_80ms = ((0.080 * sample_rate).round() as usize).min(n);

    let mut energy_early_50 = 0.0f64;
    let mut energy_late_50 = 0.0f64;
    let mut energy_early_80 = 0.0f64;
    let mut energy_late_80 = 0.0f64;
    let mut total_energy = 0.0f64;
    let mut weighted_time_sum = 0.0f64;

    for (i, &s) in sample_slice.iter().enumerate() {
        let e = (s as f64).powi(2);
        total_energy += e;
        let t_ms = (i as f64 / sample_rate) * 1000.0;
        weighted_time_sum += t_ms * e;

        if i < split_50ms {
            energy_early_50 += e;
        } else {
            energy_late_50 += e;
        }

        if i < split_80ms {
            energy_early_80 += e;
        } else {
            energy_late_80 += e;
        }
    }

    let c50_db = if energy_late_50 > 1e-12 && energy_early_50 > 1e-12 {
        10.0 * (energy_early_50 / energy_late_50).log10()
    } else if energy_late_50 <= 1e-12 {
        60.0 // Near-infinite clarity
    } else {
        -60.0
    };

    let c80_db = if energy_late_80 > 1e-12 && energy_early_80 > 1e-12 {
        10.0 * (energy_early_80 / energy_late_80).log10()
    } else if energy_late_80 <= 1e-12 {
        60.0
    } else {
        -60.0
    };

    let d50 = if total_energy > 1e-12 {
        (energy_early_50 / total_energy).clamp(0.0, 1.0)
    } else {
        1.0
    };

    let center_time_ms = if total_energy > 1e-12 {
        weighted_time_sum / total_energy
    } else {
        0.0
    };

    ClarityMetrics {
        c50_db,
        c80_db,
        d50,
        center_time_ms,
    }
}
