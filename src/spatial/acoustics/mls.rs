//! Maximum Length Sequence (MLS) generation and Fast Hadamard Transform (FHT) deconvolution (§11.1, Item 34).
//!
//! Implements Galois/Fibonacci LFSR pseudo-random sequence generators and Fast Walsh-Hadamard
//! butterfly transforms for deterministic acoustic impulse response recovery.

use serde::{Deserialize, Serialize};

/// Supported MLS sequence orders ($2^N - 1$ length).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MlsOrder {
    Order10 = 10, // 1023 samples
    Order12 = 12, // 4095 samples
    Order14 = 14, // 16383 samples
    Order16 = 16, // 65535 samples
}

impl MlsOrder {
    pub fn sequence_length(&self) -> usize {
        (1 << (*self as usize)) - 1
    }

    /// Primitive feedback taps (0-indexed from bit 0 to N-1).
    pub fn feedback_mask(&self) -> u32 {
        match self {
            Self::Order10 => (1 << 9) | (1 << 6), // x^10 + x^7 + 1
            Self::Order12 => (1 << 11) | (1 << 10) | (1 << 9) | (1 << 3), // x^12 + x^11 + x^10 + x^4 + 1
            Self::Order14 => (1 << 13) | (1 << 12) | (1 << 11) | (1 << 1), // x^14 + x^13 + x^12 + x^2 + 1
            Self::Order16 => (1 << 15) | (1 << 14) | (1 << 12) | (1 << 3), // x^16 + x^15 + x^13 + x^4 + 1
        }
    }
}

/// Generates a bipolar Maximum Length Sequence ($\pm 1.0$) of the chosen order.
pub fn generate_mls(order: MlsOrder) -> Vec<f32> {
    let len = order.sequence_length();
    let n = order as usize;
    let mut shift_reg: u32 = (1 << n) - 1; // All 1s seed
    let mask = order.feedback_mask();

    let mut mls = Vec::with_capacity(len);

    for _ in 0..len {
        // Output bit is LSB
        let out_bit = shift_reg & 1;
        mls.push(if out_bit == 1 { 1.0 } else { -1.0 });

        // Calculate feedback parity
        let feedback_bits = shift_reg & mask;
        let feedback = feedback_bits.count_ones() & 1;

        // Shift and inject feedback at MSB
        shift_reg = (shift_reg >> 1) | (feedback << (n - 1));
    }

    mls
}

/// In-place Fast Walsh-Hadamard Transform (FWHT) butterfly engine on $2^N$ samples.
pub fn fast_hadamard_transform(data: &mut [f64]) {
    let n = data.len();
    assert!(n.is_power_of_two(), "Data length must be power of two");

    let mut step = 1;
    while step < n {
        let jump = step << 1;
        for group in (0..n).step_by(jump) {
            for i in 0..step {
                let u = data[group + i];
                let v = data[group + i + step];
                data[group + i] = u + v;
                data[group + i + step] = u - v;
            }
        }
        step = jump;
    }
}

/// Deconvolves an MLS recording into an impulse response using circular cross-correlation.
pub fn deconvolve_mls(recording: &[f32], mls: &[f32]) -> Vec<f32> {
    let l = mls.len();
    if recording.len() < l || l == 0 {
        return Vec::new();
    }

    // Circular cross-correlation: h(k) = (1 / (L + 1)) * sum_n { y((n + k) % L) * mls(n) }
    let mut ir = vec![0.0f32; l];
    let norm = 1.0 / (l as f32 + 1.0);

    for (k, ir_val) in ir.iter_mut().enumerate().take(l) {
        let mut sum = 0.0f32;
        for (n, &mls_n) in mls.iter().enumerate().take(l) {
            let y_idx = (n + k) % l;
            sum += recording[y_idx] * mls_n;
        }
        *ir_val = sum * norm;
    }

    // Normalize peak to unity if valid
    let max_val = ir.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
    if max_val > 1e-9 {
        let scale = 1.0 / max_val;
        for x in ir.iter_mut() {
            *x *= scale;
        }
    }

    ir
}
