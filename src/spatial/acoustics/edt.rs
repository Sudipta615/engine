//! Early Decay Time (EDT) acoustic analysis (§11.1, Item 34).
//!
//! Evaluates the initial 0 dB to -10 dB slope of the Schroeder decay curve,
//! extrapolated to 60 dB according to ISO 3382.

use super::rt60::schroeder_decay_curve;
use serde::{Deserialize, Serialize};

/// Early Decay Time metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdtMetrics {
    /// EDT in seconds.
    pub edt_seconds: f64,
    /// Regression correlation coefficient ($R^2$).
    pub r_squared: f64,
}

/// Computes the Early Decay Time (EDT) from an impulse response.
pub fn compute_edt(ir: &[f32], sample_rate: f64) -> EdtMetrics {
    let edc = schroeder_decay_curve(ir);

    let mut t_vals = Vec::new();
    let mut y_vals = Vec::new();

    for (i, &db) in edc.iter().enumerate() {
        if (-10.0..=0.0).contains(&db) {
            let t = i as f64 / sample_rate;
            t_vals.push(t);
            y_vals.push(db);
        }
    }

    let n = t_vals.len() as f64;
    if n < 2.0 {
        return EdtMetrics {
            edt_seconds: 0.0,
            r_squared: 0.0,
        };
    }

    let sum_t: f64 = t_vals.iter().sum();
    let sum_y: f64 = y_vals.iter().sum();
    let sum_tt: f64 = t_vals.iter().map(|&t| t * t).sum();
    let sum_ty: f64 = t_vals.iter().zip(y_vals.iter()).map(|(&t, &y)| t * y).sum();

    let denom = n * sum_tt - sum_t * sum_t;
    if denom.abs() < 1e-12 {
        return EdtMetrics {
            edt_seconds: 0.0,
            r_squared: 0.0,
        };
    }

    let slope = (n * sum_ty - sum_t * sum_y) / denom;
    let intercept = (sum_y - slope * sum_t) / n;

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

    let edt = if slope < -1e-6 { -60.0 / slope } else { 0.0 };

    EdtMetrics {
        edt_seconds: edt.max(0.0),
        r_squared,
    }
}
