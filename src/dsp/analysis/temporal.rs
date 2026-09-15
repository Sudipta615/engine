//! Temporal Analysis Metrics: Crest Factor, Dynamic Range, Transients & True-Peak Density (Item 33).

use serde::{Deserialize, Serialize};

/// Temporal features computed from audio time-domain frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalFeatures {
    /// Peak to RMS ratio in decibels ($20 \log_{10}(\text{peak} / \text{RMS})$).
    pub crest_factor_db: f32,
    /// Estimated dynamic range in dB between peak level and background floor.
    pub dynamic_range_db: f32,
    /// Estimated rate of transient onsets per second.
    pub transient_density: f32,
    /// Proportion of frames with true-peak near full scale (within 0.5 dBFS).
    pub true_peak_density: f32,
}

/// Compute temporal features from a block of audio frames.
pub fn compute_temporal_features(
    samples: &[f32],
    prev_energy: f32,
    sample_rate: f32,
) -> (TemporalFeatures, f32) {
    if samples.is_empty() {
        return (
            TemporalFeatures {
                crest_factor_db: 0.0,
                dynamic_range_db: 0.0,
                transient_density: 0.0,
                true_peak_density: 0.0,
            },
            prev_energy,
        );
    }

    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f32;
    let mut near_ceiling_count = 0usize;
    let ceiling_threshold = 10.0f32.powf(-0.5 / 20.0); // -0.5 dBFS

    for &x in samples {
        let abs_x = x.abs();
        if abs_x > peak {
            peak = abs_x;
        }
        sum_sq += x * x;
        if abs_x >= ceiling_threshold {
            near_ceiling_count += 1;
        }
    }

    let n = samples.len() as f32;
    let rms = (sum_sq / n).sqrt();

    let crest_factor_db = if rms > 1e-6 {
        20.0 * (peak / rms).max(1.0).log10()
    } else {
        0.0
    };

    let peak_dbfs = if peak > 1e-6 {
        20.0 * peak.log10()
    } else {
        -100.0
    };

    let rms_dbfs = if rms > 1e-6 {
        20.0 * rms.log10()
    } else {
        -100.0
    };

    let dynamic_range_db = (peak_dbfs - (rms_dbfs - 20.0)).clamp(0.0, 120.0);

    // Simple onset detector: ratio of current block energy to previous block energy
    let current_energy = sum_sq / n;
    let is_transient = if prev_energy > 1e-6 {
        (current_energy / prev_energy) > 2.5
    } else {
        current_energy > 0.01
    };

    let block_duration_sec = n / sample_rate.max(1.0);
    let transient_density = if is_transient && block_duration_sec > 0.0 {
        1.0 / block_duration_sec
    } else {
        0.0
    };

    let true_peak_density = (near_ceiling_count as f32) / n;

    (
        TemporalFeatures {
            crest_factor_db,
            dynamic_range_db,
            transient_density,
            true_peak_density,
        },
        current_energy,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crest_factor_sine_wave() {
        // Pure sine wave: peak = 1.0, RMS = 1/sqrt(2) ≈ 0.707 -> crest factor = 3.01 dB
        let sr = 48000.0;
        let mut sine = vec![0.0f32; 4800];
        for (i, x) in sine.iter_mut().enumerate() {
            *x = (2.0 * std::f32::consts::PI * 1000.0 * (i as f32) / sr).sin();
        }

        let (feat, _) = compute_temporal_features(&sine, 0.5, sr);
        assert!((feat.crest_factor_db - 3.01).abs() < 0.1);
    }
}
