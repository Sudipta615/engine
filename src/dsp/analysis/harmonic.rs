//! Harmonic and Tonality Analysis Metrics (Item 33).

use serde::{Deserialize, Serialize};

/// Harmonic features computed from audio analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarmonicFeatures {
    /// Harmonicity ratio in [0.0, 1.0]: energy in harmonic components vs total energy.
    pub harmonicity: f32,
    /// Tonality index in [0.0, 1.0]: 1.0 for a pure sinusoid, 0.0 for pure noise.
    pub tonality: f32,
}

/// Compute harmonicity and tonality from audio samples and spectral flatness.
pub fn compute_harmonic_features(samples: &[f32], spectral_flatness: f32) -> HarmonicFeatures {
    if samples.len() < 64 {
        return HarmonicFeatures {
            harmonicity: 0.0,
            tonality: 0.0,
        };
    }

    // Tonality from spectral flatness (Wiener entropy): tonality = 1 - flatness
    let tonality = (1.0 - spectral_flatness).clamp(0.0, 1.0);

    // Compute autocorrelation peak over pitch lags (lag 20 to min(samples.len()/2, 500))
    let n = samples.len();
    let max_lag = (n / 2).min(500);
    let min_lag = 20;

    let mut r0 = 0.0f32;
    for &x in samples {
        r0 += x * x;
    }

    if r0 < 1e-9 {
        return HarmonicFeatures {
            harmonicity: 0.0,
            tonality: 0.0,
        };
    }

    let mut max_r = 0.0f32;
    for lag in min_lag..max_lag {
        let mut r = 0.0f32;
        for i in 0..(n - lag) {
            r += samples[i] * samples[i + lag];
        }
        if r > max_r {
            max_r = r;
        }
    }

    let harmonicity = (max_r / r0).clamp(0.0, 1.0);

    HarmonicFeatures {
        harmonicity,
        tonality,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_harmonic_features_sine_wave() {
        let sr = 48000.0;
        let mut sine = vec![0.0f32; 1024];
        for (i, x) in sine.iter_mut().enumerate() {
            *x = (2.0 * std::f32::consts::PI * 440.0 * (i as f32) / sr).sin();
        }

        let feat = compute_harmonic_features(&sine, 0.01);
        assert!(feat.tonality > 0.9);
        assert!(feat.harmonicity > 0.8);
    }
}
