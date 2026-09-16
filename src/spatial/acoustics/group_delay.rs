//! Group delay calculation from unwrapped phase response (§11.1, Item 34).
//!
//! Computes frequency-dependent group delay $\tau_g(f) = -d\phi/d\omega$ in milliseconds.

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

use super::phase_response::PhaseResponse;

/// Frequency-dependent group delay curve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupDelay {
    pub frequencies: Vec<f64>,
    /// Group delay in milliseconds (ms).
    pub delay_ms: Vec<f64>,
}

/// Computes group delay in milliseconds from an unwrapped phase response.
pub fn compute_group_delay(phase_resp: &PhaseResponse) -> GroupDelay {
    let freqs = &phase_resp.frequencies;
    let phase = &phase_resp.unwrapped_phase_rad;
    let n = freqs.len();

    if n < 2 {
        return GroupDelay {
            frequencies: freqs.clone(),
            delay_ms: vec![0.0; n],
        };
    }

    let mut delay_ms = Vec::with_capacity(n);

    for i in 0..n {
        let (d_phi, d_f) = if i == 0 {
            (phase[1] - phase[0], freqs[1] - freqs[0])
        } else if i == n - 1 {
            (phase[n - 1] - phase[n - 2], freqs[n - 1] - freqs[n - 2])
        } else {
            (phase[i + 1] - phase[i - 1], freqs[i + 1] - freqs[i - 1])
        };

        let tau_sec = if d_f.abs() > 1e-6 {
            // tau_g = -1/(2*pi) * (d_phi / d_f)
            -d_phi / (2.0 * PI * d_f)
        } else {
            0.0
        };

        // Convert seconds to milliseconds
        delay_ms.push(tau_sec * 1000.0);
    }

    GroupDelay {
        frequencies: freqs.clone(),
        delay_ms,
    }
}
