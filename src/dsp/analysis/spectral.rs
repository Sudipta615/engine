//! Spectral Analysis Metrics: Centroid, Spread, Flux, Rolloff, Flatness & Sub-Bass (Item 33).

use serde::{Deserialize, Serialize};

/// Spectral features computed from frequency-domain magnitude spectrum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpectralFeatures {
    /// Spectral centroid (Hz): Center of mass of the spectrum.
    pub centroid_hz: f32,
    /// Spectral spread (Hz): Bandwidth / standard deviation around centroid.
    pub spread_hz: f32,
    /// Spectral flux: Normalized rate of change of the spectrum over time.
    pub flux: f32,
    /// Spectral rolloff (Hz): Frequency below which 85% of spectral energy lies.
    pub rolloff_hz: f32,
    /// Spectral flatness (Wiener entropy): 0 = tonal/peaked, 1 = white noise.
    pub flatness: f32,
    /// Ratio of energy below 60 Hz to total energy.
    pub sub_bass_ratio: f32,
}

/// Compute spectral features given FFT magnitude bins, previous magnitude bins, and bin frequency step.
pub fn compute_spectral_features(
    mags: &[f32],
    prev_mags: &[f32],
    bin_hz: f32,
    rolloff_percent: f32,
) -> SpectralFeatures {
    if mags.is_empty() {
        return SpectralFeatures {
            centroid_hz: 0.0,
            spread_hz: 0.0,
            flux: 0.0,
            rolloff_hz: 0.0,
            flatness: 0.0,
            sub_bass_ratio: 0.0,
        };
    }

    let mut sum_mag = 0.0f32;
    let mut sum_weighted = 0.0f32;
    let mut total_power = 0.0f32;
    let mut log_power_sum = 0.0f32;
    let mut sub_bass_power = 0.0f32;
    let mut flux_sum = 0.0f32;

    let sub_bass_cutoff = 60.0f32;

    for (k, &m) in mags.iter().enumerate() {
        let f = k as f32 * bin_hz;
        let p = m * m;

        sum_mag += m;
        sum_weighted += f * m;
        total_power += p;

        if f <= sub_bass_cutoff {
            sub_bass_power += p;
        }

        // Add small epsilon to prevent log(0)
        log_power_sum += (p.max(1e-12)).ln();

        // Flux vs previous magnitude spectrum
        let prev = prev_mags.get(k).copied().unwrap_or(0.0);
        let diff = m - prev;
        flux_sum += diff * diff;
    }

    let centroid = if sum_mag > 1e-9 {
        sum_weighted / sum_mag
    } else {
        0.0
    };

    // Second moment (spread)
    let mut spread_sum = 0.0f32;
    for (k, &m) in mags.iter().enumerate() {
        let f = k as f32 * bin_hz;
        let delta = f - centroid;
        spread_sum += delta * delta * m;
    }
    let spread = if sum_mag > 1e-9 {
        (spread_sum / sum_mag).sqrt()
    } else {
        0.0
    };

    // Spectral Flatness (Wiener entropy): geometric mean / arithmetic mean of power
    let n = mags.len() as f32;
    let geom_mean = (log_power_sum / n).exp();
    let arith_mean = total_power / n;
    let flatness = if arith_mean > 1e-9 {
        (geom_mean / arith_mean).clamp(0.0, 1.0)
    } else {
        0.0
    };

    // Spectral Rolloff: frequency threshold capturing rolloff_percent of energy (e.g. 85%)
    let target_energy = total_power * rolloff_percent.clamp(0.1, 0.99);
    let mut accum_energy = 0.0f32;
    let mut rolloff_hz = 0.0f32;
    for (k, &m) in mags.iter().enumerate() {
        accum_energy += m * m;
        if accum_energy >= target_energy {
            rolloff_hz = k as f32 * bin_hz;
            break;
        }
    }

    let sub_bass_ratio = if total_power > 1e-9 {
        (sub_bass_power / total_power).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let flux = (flux_sum / n).sqrt();

    SpectralFeatures {
        centroid_hz: centroid,
        spread_hz: spread,
        flux,
        rolloff_hz,
        flatness,
        sub_bass_ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spectral_centroid_single_tone() {
        // Single pure tone at bin 10 (10 * 100 Hz = 1000 Hz)
        let mut mags = vec![0.0f32; 64];
        mags[10] = 1.0;
        let prev = vec![0.0f32; 64];

        let feat = compute_spectral_features(&mags, &prev, 100.0, 0.85);
        assert!((feat.centroid_hz - 1000.0).abs() < 1e-3);
        assert!(feat.spread_hz < 1e-3);
    }

    #[test]
    fn test_spectral_flatness_uniform_noise() {
        let mags = vec![1.0f32; 64];
        let prev = vec![1.0f32; 64];

        let feat = compute_spectral_features(&mags, &prev, 100.0, 0.85);
        // Flat magnitude spectrum has flatness ≈ 1.0
        assert!((feat.flatness - 1.0).abs() < 1e-3);
    }
}
