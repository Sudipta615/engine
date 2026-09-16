//! Multi-point acoustic measurement spatial averaging (§11.2, Item 35).
//!
//! Provides spatial averaging across multiple measurement positions
//! to avoid narrow position-dependent acoustic comb-filtering artifacts.

use crate::spatial::acoustics::frequency_response::FrequencyResponse;
use serde::{Deserialize, Serialize};

/// Strategy for combining multi-point measurement frequency responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum SpatialAverageStrategy {
    /// Arithmetic average of decibel magnitude curves.
    MagnitudeDecibels,
    /// RMS acoustic energy / power averaging (physically correct acoustic power sum).
    #[default]
    PowerEnergy,
    /// Energy-weighted spatial averaging (assigns higher priority to the central sweet-spot).
    WeightedEnergy(Vec<f64>),
}

/// Spatially averages multiple frequency response measurements into a single representative response.
pub fn average_frequency_responses(
    responses: &[FrequencyResponse],
    strategy: SpatialAverageStrategy,
) -> Option<FrequencyResponse> {
    if responses.is_empty() {
        return None;
    }
    if responses.len() == 1 {
        return Some(responses[0].clone());
    }

    let num_bins = responses[0].frequencies.len();
    let num_resp = responses.len();
    let mut averaged_db = Vec::with_capacity(num_bins);

    match strategy {
        SpatialAverageStrategy::MagnitudeDecibels => {
            for bin in 0..num_bins {
                let sum_db: f64 = responses.iter().map(|r| r.magnitude_db[bin]).sum();
                averaged_db.push(sum_db / num_resp as f64);
            }
        }
        SpatialAverageStrategy::PowerEnergy => {
            let inv_n = 1.0 / num_resp as f64;
            for bin in 0..num_bins {
                let mut sum_power = 0.0f64;
                for r in responses {
                    let db = r.magnitude_db[bin];
                    let power = 10.0f64.powf(db / 10.0);
                    sum_power += power;
                }
                let avg_power = sum_power * inv_n;
                let db = if avg_power > 1e-12 {
                    10.0 * avg_power.log10()
                } else {
                    -180.0
                };
                averaged_db.push(db);
            }
        }
        SpatialAverageStrategy::WeightedEnergy(weights) => {
            let total_weight: f64 = weights.iter().take(num_resp).sum();
            let norm_weight = if total_weight > 1e-6 {
                1.0 / total_weight
            } else {
                1.0 / num_resp as f64
            };

            for bin in 0..num_bins {
                let mut sum_power = 0.0f64;
                for (i, r) in responses.iter().enumerate() {
                    let w = weights.get(i).copied().unwrap_or(1.0);
                    let db = r.magnitude_db[bin];
                    let power = 10.0f64.powf(db / 10.0);
                    sum_power += w * power;
                }
                let avg_power = sum_power * norm_weight;
                let db = if avg_power > 1e-12 {
                    10.0 * avg_power.log10()
                } else {
                    -180.0
                };
                averaged_db.push(db);
            }
        }
    }

    Some(FrequencyResponse {
        frequencies: responses[0].frequencies.clone(),
        magnitude_db: averaged_db,
        smoothing: responses[0].smoothing,
    })
}
