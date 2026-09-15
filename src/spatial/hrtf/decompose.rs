//! Decomposed HRIR representation: ITD, minimum-phase spectrum, and excess-phase allpass (spec §47–48, §62).

use super::corpus::HrtfCorpus;
use crate::dsp::correction::phase::{excess_allpass_spectrum, minimum_phase_ir, Spectrum};

/// Decomposed components of a single-ear head-related impulse response.
#[derive(Debug, Clone, PartialEq)]
pub struct HrirComponents {
    /// Onset delay in samples (fractional).
    pub itd_samples: f32,
    /// Minimum-phase reconstructed impulse response.
    pub min_phase_ir: Vec<f32>,
    /// Excess-phase allpass impulse response.
    pub excess_phase_allpass: Vec<f32>,
}

/// Detect the arrival onset of an impulse response using threshold-based onset detection.
/// Finds the first sample exceeding `threshold` of the peak amplitude, with parabolic sub-sample refinement.
pub fn detect_onset_samples(ir: &[f32], threshold: f32) -> f32 {
    if ir.is_empty() {
        return 0.0;
    }
    let peak = ir.iter().fold(0.0f32, |acc, &x| acc.max(x.abs()));
    if peak <= 1e-9 {
        return 0.0;
    }

    let cut = peak * threshold.clamp(0.01, 0.99);
    for (i, &val) in ir.iter().enumerate() {
        if val.abs() >= cut {
            if i > 0 && i + 1 < ir.len() {
                let y0 = ir[i - 1].abs();
                let y1 = ir[i].abs();
                let y2 = ir[i + 1].abs();
                let denom = (y0 - 2.0 * y1 + y2).abs();
                if denom > 1e-6 {
                    let delta = (y0 - y2) / (2.0 * denom);
                    return (i as f32 + delta.clamp(-0.5, 0.5)).max(0.0);
                }
            }
            return i as f32;
        }
    }
    0.0
}

/// Extract arrival delays and ITD between left and right ear impulse responses.
/// Returns `(left_onset_samples, right_onset_samples)`.
pub fn extract_itd(ir_left: &[f32], ir_right: &[f32]) -> (f32, f32) {
    const ONSET_THRESHOLD: f32 = 0.15;
    (
        detect_onset_samples(ir_left, ONSET_THRESHOLD),
        detect_onset_samples(ir_right, ONSET_THRESHOLD),
    )
}

/// Compute minimum-phase impulse response matching the magnitude of `ir`.
pub fn minimum_phase_from_ir(ir: &[f32], sample_rate: f64) -> Vec<f32> {
    if ir.is_empty() {
        return Vec::new();
    }
    let n = (ir.len() * 2).max(32).next_power_of_two();
    let samples_f64: Vec<f64> = ir.iter().map(|&x| x as f64).collect();

    let spectrum = match Spectrum::from_time_with_len(&samples_f64, n, sample_rate) {
        Ok(s) => s,
        Err(_) => return ir.to_vec(),
    };

    let min_f64 = match minimum_phase_ir(&spectrum) {
        Ok(m) => m,
        Err(_) => return ir.to_vec(),
    };

    let mut out: Vec<f32> = min_f64.iter().take(ir.len()).map(|&x| x as f32).collect();
    // Preserve overall energy
    let e_orig: f32 = ir.iter().map(|&x| x * x).sum();
    let e_min: f32 = out.iter().map(|&x| x * x).sum();
    if e_min > 1e-12 && e_orig > 1e-12 {
        let scale = (e_orig / e_min).sqrt();
        for v in &mut out {
            *v *= scale;
        }
    }
    out
}

/// Compute excess-phase allpass impulse response from `ir`.
pub fn excess_phase_from_ir(ir: &[f32], sample_rate: f64) -> Vec<f32> {
    if ir.is_empty() {
        return Vec::new();
    }
    let n = (ir.len() * 2).max(32).next_power_of_two();
    let samples_f64: Vec<f64> = ir.iter().map(|&x| x as f64).collect();

    let spectrum = match Spectrum::from_time_with_len(&samples_f64, n, sample_rate) {
        Ok(s) => s,
        Err(_) => return vec![1.0],
    };

    let allpass_spec = match excess_allpass_spectrum(&spectrum) {
        Ok(s) => s,
        Err(_) => return vec![1.0],
    };

    allpass_spec
        .to_time()
        .iter()
        .take(ir.len())
        .map(|&x| x as f32)
        .collect()
}

/// Decompose a single ear impulse response into ITD, minimum-phase IR, and excess-phase allpass.
pub fn decompose_single_ir(ir: &[f32], sample_rate: f64) -> HrirComponents {
    let itd_samples = detect_onset_samples(ir, 0.15);
    let min_phase_ir = minimum_phase_from_ir(ir, sample_rate);
    let excess_phase_allpass = excess_phase_from_ir(ir, sample_rate);
    HrirComponents {
        itd_samples,
        min_phase_ir,
        excess_phase_allpass,
    }
}

/// Decompose an entire HRTF corpus into per-measurement left/right components.
pub fn decompose_corpus(corpus: &HrtfCorpus) -> Vec<(HrirComponents, HrirComponents)> {
    let fs = corpus.sample_rate.max(1) as f64;
    corpus
        .measurements
        .iter()
        .map(|m| {
            let left = decompose_single_ir(&m.left, fs);
            let right = decompose_single_ir(&m.right, fs);
            (left, right)
        })
        .collect()
}

/// Reconstruct impulse response from decomposed components.
pub fn reconstruct_ir(components: &HrirComponents, out_len: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; out_len];
    let d = components.itd_samples;
    let whole = d.floor() as usize;
    let frac = d - whole as f32;

    for (k, &v) in components.min_phase_ir.iter().enumerate() {
        let pos = whole + k;
        if pos < out_len {
            out[pos] += v * (1.0 - frac);
        }
        if pos + 1 < out_len {
            out[pos + 1] += v * frac;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_itd_pure_delay() {
        let mut l = vec![0.0f32; 64];
        let mut r = vec![0.0f32; 64];
        l[5] = 1.0;
        r[17] = 1.0;

        let (dl, dr) = extract_itd(&l, &r);
        assert!((dl - 5.0).abs() < 0.2);
        assert!((dr - 17.0).abs() < 0.2);
        assert!(((dr - dl) - 12.0).abs() < 0.2);
    }

    #[test]
    fn minimum_phase_preserves_energy() {
        let mut ir = vec![0.0f32; 64];
        ir[10] = 0.8;
        ir[15] = -0.5;
        ir[20] = 0.2;

        let min_ir = minimum_phase_from_ir(&ir, 48_000.0);
        assert_eq!(min_ir.len(), ir.len());

        let e_orig: f32 = ir.iter().map(|&x| x * x).sum();
        let e_min: f32 = min_ir.iter().map(|&x| x * x).sum();
        assert!((e_orig - e_min).abs() < 1e-3);
    }

    #[test]
    fn decompose_and_reconstruct() {
        let mut ir = vec![0.0f32; 64];
        ir[4] = 1.0;
        let comp = decompose_single_ir(&ir, 48_000.0);
        assert!(comp.itd_samples >= 3.0 && comp.itd_samples <= 5.0);
        let recon = reconstruct_ir(&comp, 64);
        assert_eq!(recon.len(), 64);
        let peak_idx = recon
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap();
        assert!((3..=5).contains(&peak_idx));
    }
}
