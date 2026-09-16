//! Reverberation time (RT60, T20, T30) estimation via backward Schroeder integration (§11.1, Item 34).
//!
//! Implements ISO 3382-1 backward integration energy decay curves and linear regression analysis.

use serde::{Deserialize, Serialize};

/// RT60 reverberation analysis metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rt60Metrics {
    /// Early T20 decay extrapolated to 60 dB (seconds).
    pub t20_seconds: f64,
    /// Full T30 decay extrapolated to 60 dB (seconds).
    pub t30_seconds: f64,
    /// Regression correlation coefficient ($R^2$) for T20.
    pub t20_r_squared: f64,
    /// Regression correlation coefficient ($R^2$) for T30.
    pub t30_r_squared: f64,
}

/// Computes the backward Schroeder integration Energy Decay Curve (EDC) in decibels.
pub fn schroeder_decay_curve(ir: &[f32]) -> Vec<f64> {
    let n = ir.len();
    if n == 0 {
        return Vec::new();
    }

    let mut edc = vec![0.0f64; n];
    let mut accum = 0.0f64;

    // Backward cumulative sum of squared samples
    for i in (0..n).rev() {
        let s = ir[i] as f64;
        accum += s * s;
        edc[i] = accum;
    }

    let total_energy = edc[0];
    if total_energy <= 1e-12 {
        return vec![-120.0; n];
    }

    let inv_total = 1.0 / total_energy;
    for val in edc.iter_mut() {
        let normalized = *val * inv_total;
        *val = if normalized > 1e-12 {
            10.0 * normalized.log10()
        } else {
            -120.0
        };
    }

    edc
}

/// Performs linear least-squares regression on (t, edc_db) within a dB evaluation range.
/// Returns (slope_db_per_sec, r_squared).
fn linear_regression(edc_db: &[f64], sample_rate: f64, db_start: f64, db_end: f64) -> (f64, f64) {
    let mut t_vals = Vec::new();
    let mut y_vals = Vec::new();

    for (i, &db) in edc_db.iter().enumerate() {
        if db <= db_start && db >= db_end {
            let t = i as f64 / sample_rate;
            t_vals.push(t);
            y_vals.push(db);
        }
    }

    let n = t_vals.len() as f64;
    if n < 2.0 {
        return (0.0, 0.0);
    }

    let sum_t: f64 = t_vals.iter().sum();
    let sum_y: f64 = y_vals.iter().sum();
    let sum_tt: f64 = t_vals.iter().map(|&t| t * t).sum();
    let sum_ty: f64 = t_vals.iter().zip(y_vals.iter()).map(|(&t, &y)| t * y).sum();

    let denom = n * sum_tt - sum_t * sum_t;
    if denom.abs() < 1e-12 {
        return (0.0, 0.0);
    }

    let slope = (n * sum_ty - sum_t * sum_y) / denom;
    let intercept = (sum_y - slope * sum_t) / n;

    // Calculate R-squared
    let mean_y = sum_y / n;
    let ss_tot: f64 = y_vals.iter().map(|&y| (y - mean_y).powi(2)).sum();
    let ss_res: f64 = t_vals
        .iter()
        .zip(y_vals.iter())
        .map(|(&t, &y)| (y - (slope * t + intercept)).powi(2))
        .sum();

    let r_squared = if ss_tot > 1e-12 {
        (1.0 - (ss_res / ss_tot)).clamp(0.0, 1.0)
    } else {
        1.0
    };

    (slope, r_squared)
}

/// Calculates T20 and T30 reverberation time metrics from an impulse response.
pub fn compute_rt60(ir: &[f32], sample_rate: f64) -> Rt60Metrics {
    let edc = schroeder_decay_curve(ir);

    // T20: evaluated between -5 dB and -25 dB
    let (slope_20, r2_20) = linear_regression(&edc, sample_rate, -5.0, -25.0);
    let t20 = if slope_20 < -1e-6 {
        -60.0 / slope_20
    } else {
        0.0
    };

    // T30: evaluated between -5 dB and -35 dB
    let (slope_30, r2_30) = linear_regression(&edc, sample_rate, -5.0, -35.0);
    let t30 = if slope_30 < -1e-6 {
        -60.0 / slope_30
    } else {
        0.0
    };

    Rt60Metrics {
        t20_seconds: t20.max(0.0),
        t30_seconds: t30.max(0.0),
        t20_r_squared: r2_20,
        t30_r_squared: r2_30,
    }
}
