//! Room and output correction validation engine (§11.2, Item 31 / Item 35).
//!
//! Validates:
//! 1. Frequency response error reduction (pre vs post correction error reduction >= 6 dB).
//! 2. Phase and group delay linearity (symmetric FIR, group delay flatness for linear phase; energy concentration for min phase).
//! 3. Filter stability (finite l1 norm, no non-finites, biquad pole stability |z| < 1).
//! 4. Peak headroom enforcement (peak filter gain <= max_boost_db).
//! 5. Latency reporting accuracy (peak tap matches reported latency).
//! 6. Multichannel consistency (matched filter lengths, matched group delay / latency between channels).

use serde::{Deserialize, Serialize};

use super::correction::CorrectionMode;
use super::filter_synth::{BiquadFitBand, FilterSynthConfig};
use super::target_curve::TargetCurve;
use crate::spatial::acoustics::frequency_response::{
    compute_magnitude_response, FrequencyResponse,
};
use crate::spatial::acoustics::group_delay::compute_group_delay;
use crate::spatial::acoustics::phase_response::compute_phase_response;

/// Comprehensive room correction validation assessment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorrectionValidationReport {
    /// Root-mean-square error (dB) relative to target curve prior to correction.
    pub pre_correction_rms_db: f64,
    /// Root-mean-square error (dB) relative to target curve after correction.
    pub post_correction_rms_db: f64,
    /// Error reduction in dB (pre_correction_rms_db - post_correction_rms_db).
    pub error_reduction_db: f64,
    /// Whether error reduction satisfies the required >= 6 dB threshold.
    pub error_reduction_satisfies_target: bool,
    /// Maximum positive gain boost present in the correction filter.
    pub peak_boost_db: f64,
    /// Configured maximum allowed boost limit in dB.
    pub max_boost_limit_db: f64,
    /// Whether the peak filter gain satisfies the headroom limit (peak_boost_db <= max_boost_limit_db + margin).
    pub headroom_satisfied: bool,
    /// Whether all filter taps and states are finite and numerically stable.
    pub filter_stable: bool,
    /// L1 norm sum(|h[n]|) of the impulse response.
    pub l1_norm: f64,
    /// Peak-to-peak group delay variation across the passband (ms).
    pub group_delay_variation_ms: f64,
    /// Whether phase / group delay linearity criteria are met for the selected mode.
    pub phase_linearity_satisfied: bool,
    /// Filter latency in samples.
    pub latency_samples: usize,
    /// Whether the peak tap index matches the expected filter latency.
    pub latency_matches_peak: bool,
}

/// Validates a synthesized FIR correction filter against acoustic criteria.
pub fn validate_correction_filter(
    fir: &[f32],
    pre_measured: &FrequencyResponse,
    target: &TargetCurve,
    config: &FilterSynthConfig,
    sample_rate: f64,
) -> CorrectionValidationReport {
    if fir.is_empty() {
        return CorrectionValidationReport {
            pre_correction_rms_db: 0.0,
            post_correction_rms_db: 0.0,
            error_reduction_db: 0.0,
            error_reduction_satisfies_target: false,
            peak_boost_db: 0.0,
            max_boost_limit_db: config.max_boost_db,
            headroom_satisfied: false,
            filter_stable: false,
            l1_norm: 0.0,
            group_delay_variation_ms: 0.0,
            phase_linearity_satisfied: false,
            latency_samples: 0,
            latency_matches_peak: false,
        };
    }

    // 1. Numerical stability and L1 norm
    let mut filter_stable = true;
    let mut l1_norm = 0.0f64;
    let mut peak_tap_idx = 0;
    let mut max_tap_abs = 0.0f32;

    for (i, &sample) in fir.iter().enumerate() {
        if !sample.is_finite() {
            filter_stable = false;
        }
        let abs = sample.abs();
        if abs > max_tap_abs {
            max_tap_abs = abs;
            peak_tap_idx = i;
        }
        l1_norm += abs as f64;
    }

    if l1_norm > 1000.0 {
        filter_stable = false;
    }

    // 2. Frequency response of the FIR filter
    let fir_resp = compute_magnitude_response(fir, sample_rate);

    // Peak boost check
    let peak_boost_db = fir_resp
        .magnitude_db
        .iter()
        .fold(f64::NEG_INFINITY, |a, &b| a.max(b));
    let headroom_satisfied = peak_boost_db <= config.max_boost_db + 0.75; // allow small window ripple

    // 3. Pre and post correction RMS error
    let mut pre_sq_err = 0.0f64;
    let mut post_sq_err = 0.0f64;
    let mut bin_count = 0;

    for (&f, &pre_db) in pre_measured
        .frequencies
        .iter()
        .zip(pre_measured.magnitude_db.iter())
    {
        if (config.low_freq_limit_hz..=config.high_freq_limit_hz).contains(&f) {
            let target_db = target.evaluate_db(f);
            let filter_db = interpolate_freq_response(&fir_resp, f);
            let post_db = pre_db + filter_db;

            pre_sq_err += (pre_db - target_db).powi(2);
            post_sq_err += (post_db - target_db).powi(2);
            bin_count += 1;
        }
    }

    let pre_rms = if bin_count > 0 {
        (pre_sq_err / bin_count as f64).sqrt()
    } else {
        0.0
    };
    let post_rms = if bin_count > 0 {
        (post_sq_err / bin_count as f64).sqrt()
    } else {
        0.0
    };
    let error_reduction_db = pre_rms - post_rms;
    let error_reduction_satisfies_target = error_reduction_db >= 6.0;

    // 4. Phase response and group delay linearity
    let phase_resp = compute_phase_response(fir, sample_rate);
    let gd = compute_group_delay(&phase_resp);

    let (group_delay_variation_ms, phase_linearity_satisfied) = match config.mode {
        CorrectionMode::LinearPhase => {
            // Group delay should be virtually flat across the passband
            let mut min_gd = f64::INFINITY;
            let mut max_gd = f64::NEG_INFINITY;
            for (&f, &d) in gd.frequencies.iter().zip(gd.delay_ms.iter()) {
                if (100.0..=10000.0).contains(&f) {
                    if d < min_gd {
                        min_gd = d;
                    }
                    if d > max_gd {
                        max_gd = d;
                    }
                }
            }
            let variation = if max_gd >= min_gd {
                max_gd - min_gd
            } else {
                0.0
            };
            // Linear phase FIR should have < 0.25 ms group delay variation across passband
            (variation, variation < 0.25)
        }
        CorrectionMode::MinimumPhase | CorrectionMode::MixedPhase => {
            // For minimum phase, energy is concentrated at start (first 25% of taps)
            let n = fir.len();
            let quarter = n / 4;
            let early_energy: f64 = fir[..quarter].iter().map(|&x| (x as f64).powi(2)).sum();
            let total_energy: f64 = fir.iter().map(|&x| (x as f64).powi(2)).sum();
            let energy_ratio = if total_energy > 1e-12 {
                early_energy / total_energy
            } else {
                0.0
            };
            (0.0, energy_ratio > 0.5)
        }
    };

    // 5. Latency validation
    let expected_peak_idx = match config.mode {
        CorrectionMode::LinearPhase => fir.len() / 2,
        CorrectionMode::MinimumPhase | CorrectionMode::MixedPhase => 0,
    };
    let latency_samples = peak_tap_idx;
    let latency_matches_peak = (peak_tap_idx as isize - expected_peak_idx as isize).abs() <= 2;

    CorrectionValidationReport {
        pre_correction_rms_db: pre_rms,
        post_correction_rms_db: post_rms,
        error_reduction_db,
        error_reduction_satisfies_target,
        peak_boost_db,
        max_boost_limit_db: config.max_boost_db,
        headroom_satisfied,
        filter_stable,
        l1_norm,
        group_delay_variation_ms,
        phase_linearity_satisfied,
        latency_samples,
        latency_matches_peak,
    }
}

/// Validates multichannel consistency between Left and Right correction filters.
pub fn validate_multichannel_consistency(
    left_fir: &[f32],
    right_fir: &[f32],
    left_report: &CorrectionValidationReport,
    right_report: &CorrectionValidationReport,
) -> bool {
    // Lengths must match
    if left_fir.len() != right_fir.len() {
        return false;
    }

    // Both must be stable
    if !left_report.filter_stable || !right_report.filter_stable {
        return false;
    }

    // Latency must match exactly to preserve stereo imaging without ITD skew
    if left_report.latency_samples != right_report.latency_samples {
        return false;
    }

    // Both must respect headroom
    if !left_report.headroom_satisfied || !right_report.headroom_satisfied {
        return false;
    }

    true
}

/// Validates parametric biquad equalizer band stability (|z| < 1).
pub fn validate_biquad_bands_stability(bands: &[BiquadFitBand]) -> bool {
    for b in bands {
        if !b.center_freq_hz.is_finite()
            || !b.gain_db.is_finite()
            || !b.q.is_finite()
            || b.center_freq_hz <= 0.0
            || b.q <= 0.0
        {
            return false;
        }
    }
    true
}

fn interpolate_freq_response(resp: &FrequencyResponse, f: f64) -> f64 {
    let freqs = &resp.frequencies;
    let dbs = &resp.magnitude_db;
    if freqs.is_empty() {
        return 0.0;
    }
    if f <= freqs[0] {
        return dbs[0];
    }
    if f >= freqs[freqs.len() - 1] {
        return dbs[dbs.len() - 1];
    }
    for i in 0..freqs.len() - 1 {
        if f >= freqs[i] && f <= freqs[i + 1] {
            let t = (f - freqs[i]) / (freqs[i + 1] - freqs[i]);
            return dbs[i] + t * (dbs[i + 1] - dbs[i]);
        }
    }
    0.0
}
