//! Energy-Time Curve (ETC) and analytic envelope analysis (§11.1, Item 34).
//!
//! Extracts instantaneous squared-energy and analytic Hilbert envelope curves in decibels.

use serde::{Deserialize, Serialize};

/// Energy-Time Curve representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnergyTimeCurve {
    /// Time axis in milliseconds.
    pub time_ms: Vec<f64>,
    /// Instantaneous energy envelope in dBFS ($10 \log_{10}(h^2(t))$).
    pub etc_db: Vec<f64>,
}

/// Computes the Energy-Time Curve (ETC) in decibels from an impulse response.
pub fn compute_etc(ir: &[f32], sample_rate: f64) -> EnergyTimeCurve {
    let n = ir.len();
    if n == 0 || sample_rate <= 0.0 {
        return EnergyTimeCurve {
            time_ms: Vec::new(),
            etc_db: Vec::new(),
        };
    }

    let mut time_ms = Vec::with_capacity(n);
    let mut etc_db = Vec::with_capacity(n);

    // Peak energy for relative normalization
    let max_sq = ir
        .iter()
        .map(|&x| (x as f64).powi(2))
        .fold(0.0f64, f64::max);
    let inv_max = if max_sq > 1e-12 { 1.0 / max_sq } else { 1.0 };

    for (i, &s) in ir.iter().enumerate() {
        let t = (i as f64 / sample_rate) * 1000.0;
        time_ms.push(t);

        let e = (s as f64).powi(2) * inv_max;
        let db = if e > 1e-12 { 10.0 * e.log10() } else { -120.0 };
        etc_db.push(db);
    }

    EnergyTimeCurve { time_ms, etc_db }
}
