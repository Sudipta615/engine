//! Deterministic processing and numerical equivalence classifications.
//!
//! Provides formal classifications for output reproducibility across processor
//! architectures (x86 SSE2 vs AVX2 vs AVX-512 vs ARM NEON vs scalar) and
//! arithmetic precisions (f32 vs f64).

use serde::{Deserialize, Serialize};

/// Result of evaluating two audio signals for equivalence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EquivalenceClass {
    /// 100% bit-exact bit-for-bit equality (`f32::to_bits()` match exactly for every sample).
    BitExact,
    /// Numerically equivalent: differences are confined to floating-point rounding/FMA.
    NumericallyEquivalent {
        /// Maximum absolute sample-domain difference.
        max_delta: f64,
        /// Signal-to-noise ratio in dB.
        snr_db: f64,
    },
    /// Perceptually equivalent: differences are inaudible under psychoacoustic thresholds.
    PerceptuallyEquivalent {
        /// Maximum absolute sample-domain difference.
        max_delta: f64,
        /// Signal-to-noise ratio in dB.
        snr_db: f64,
    },
    /// Divergent: the outputs differ measurably and exceed perceptual tolerances.
    Divergent {
        /// Maximum absolute sample-domain difference.
        max_delta: f64,
        /// Signal-to-noise ratio in dB.
        snr_db: f64,
    },
}

impl EquivalenceClass {
    /// Returns true if the outputs are bit-exact.
    pub fn is_bit_exact(&self) -> bool {
        matches!(self, Self::BitExact)
    }

    /// Returns true if the outputs are at least numerically equivalent.
    pub fn is_numerically_equivalent(&self) -> bool {
        matches!(self, Self::BitExact | Self::NumericallyEquivalent { .. })
    }

    /// Returns true if the outputs satisfy the given mode requirement.
    pub fn satisfies(&self, mode: DeterministicMode) -> bool {
        match mode {
            DeterministicMode::StrictBitExact => self.is_bit_exact(),
            DeterministicMode::Numerical => self.is_numerically_equivalent(),
            DeterministicMode::Perceptual => !matches!(self, Self::Divergent { .. }),
        }
    }
}

/// Target reproducibility mode enforced on DSP stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeterministicMode {
    /// Requires 100% bit-exact reproducibility across all runs and platforms.
    StrictBitExact,
    /// Permits legitimate floating-point rounding/FMA variations (e.g. SNR > 120 dB).
    #[default]
    Numerical,
    /// Permits psychoacoustically inaudible variations (e.g. SNR > 80 dB).
    Perceptual,
}

/// Compares two audio buffers and classifies their equivalence.
pub fn compare_buffers(
    reference: &[f32],
    test: &[f32],
    max_numeric_delta: f64,
    min_numeric_snr_db: f64,
) -> EquivalenceClass {
    let n = reference.len().min(test.len());
    if n == 0 {
        return EquivalenceClass::BitExact;
    }

    // Step 1: Check for 100% bit-exact match
    let mut bit_exact = true;
    for i in 0..n {
        if reference[i].to_bits() != test[i].to_bits() {
            bit_exact = false;
            break;
        }
    }
    if bit_exact && reference.len() == test.len() {
        return EquivalenceClass::BitExact;
    }

    // Step 2: Compute numerical metrics (max delta, signal power, noise power)
    let mut max_delta = 0.0f64;
    let mut signal_power = 0.0f64;
    let mut noise_power = 0.0f64;

    for i in 0..n {
        let r = reference[i] as f64;
        let t = test[i] as f64;
        let delta = (r - t).abs();
        if delta > max_delta {
            max_delta = delta;
        }
        signal_power += r * r;
        noise_power += delta * delta;
    }

    let snr_db = if noise_power <= 1e-24 {
        200.0 // Effective infinite SNR
    } else if signal_power <= 1e-24 {
        0.0
    } else {
        10.0 * (signal_power / noise_power).log10()
    };

    if max_delta <= max_numeric_delta && snr_db >= min_numeric_snr_db {
        EquivalenceClass::NumericallyEquivalent { max_delta, snr_db }
    } else if max_delta <= 0.05 && snr_db >= 80.0 {
        EquivalenceClass::PerceptuallyEquivalent { max_delta, snr_db }
    } else {
        EquivalenceClass::Divergent { max_delta, snr_db }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_buffers_are_bit_exact() {
        let a = vec![0.1f32, -0.5, 0.75, 0.0];
        let b = vec![0.1f32, -0.5, 0.75, 0.0];
        let eq = compare_buffers(&a, &b, 1e-6, 120.0);
        assert_eq!(eq, EquivalenceClass::BitExact);
        assert!(eq.satisfies(DeterministicMode::StrictBitExact));
    }

    #[test]
    fn test_slight_rounding_is_numerically_equivalent() {
        let a = vec![1.0f32; 1024];
        let mut b = a.clone();
        b[10] += 1e-7; // tiny rounding difference

        let eq = compare_buffers(&a, &b, 1e-5, 100.0);
        assert!(matches!(eq, EquivalenceClass::NumericallyEquivalent { .. }));
        assert!(!eq.satisfies(DeterministicMode::StrictBitExact));
        assert!(eq.satisfies(DeterministicMode::Numerical));
    }

    #[test]
    fn test_large_divergence() {
        let a = vec![1.0f32; 100];
        let b = vec![0.0f32; 100];
        let eq = compare_buffers(&a, &b, 1e-5, 100.0);
        assert!(matches!(eq, EquivalenceClass::Divergent { .. }));
        assert!(!eq.satisfies(DeterministicMode::Perceptual));
    }
}
