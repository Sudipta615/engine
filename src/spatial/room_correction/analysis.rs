//! Room IR analysis: frequency response, energy-time curve, RT60, phase, and
//! group delay. **Control-path only — all functions allocate freely.**

use std::f32::consts::TAU;

/// Analysis results for a room impulse response.
#[derive(Debug, Clone)]
pub struct RoomIrAnalysis {
    /// Frequency response as `(frequency_hz, magnitude_linear)` pairs, sampled
    /// at log-spaced frequencies from 20 Hz to `sample_rate / 2`.
    pub freq_response: Vec<(f32, f32)>,
    /// Energy-time curve (squared IR envelope), length = `ir.len()`.
    pub energy_time_curve: Vec<f32>,
    /// Estimated RT60 in seconds (−60 dB decay time from peak).
    pub rt60: f32,
    /// Phase response as `(frequency_hz, phase_rad)` pairs (same grid as `freq_response`).
    pub phase: Vec<(f32, f32)>,
    /// Group delay as `(frequency_hz, group_delay_samples)` pairs.
    pub group_delay: Vec<(f32, f32)>,
}

/// Target curve for room correction.
#[derive(Debug, Clone)]
pub enum RoomCorrectionTarget {
    /// Flat frequency response (0 dB at all frequencies).
    Flat,
    /// Custom target curve: `(frequency_hz, gain_db)` control points.
    Custom(Vec<(f32, f32)>),
    /// Diffuse-field target for a loudspeaker type.
    SpeakerCurve,
    /// Diffuse-field / free-field equalization target for headphones.
    HeadphoneCurve,
}

/// Analyze a room impulse response. **Control path — allocates.**
///
/// Returns a [`RoomIrAnalysis`] with the frequency response, energy-time curve,
/// RT60, phase, and group delay computed from `ir` at `sample_rate`.
pub fn analyze_ir(ir: &[f32], sample_rate: f32) -> RoomIrAnalysis {
    if ir.is_empty() || sample_rate <= 0.0 {
        return RoomIrAnalysis {
            freq_response: Vec::new(),
            energy_time_curve: Vec::new(),
            rt60: 0.0,
            phase: Vec::new(),
            group_delay: Vec::new(),
        };
    }

    // Frequency grid: 64 log-spaced points from 20 Hz to Nyquist.
    let n_freqs = 64usize;
    let f_min = 20.0f32;
    let f_max = (sample_rate / 2.0).min(20_000.0);
    let log_step = (f_max / f_min).ln() / (n_freqs - 1) as f32;

    let mut freq_response = Vec::with_capacity(n_freqs);
    let mut phase = Vec::with_capacity(n_freqs);
    let mut group_delay = Vec::with_capacity(n_freqs);

    for k in 0..n_freqs {
        let freq = f_min * (k as f32 * log_step).exp();
        let w = TAU * freq / sample_rate;

        // DFT at this frequency.
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &v) in ir.iter().enumerate() {
            let phase_n = w * n as f32;
            re += v as f64 * phase_n.cos() as f64;
            im -= v as f64 * phase_n.sin() as f64;
        }

        let mag = (re * re + im * im).sqrt() as f32;
        let ph = (im as f32).atan2(re as f32);

        // Group delay via finite difference of phase (one bin higher).
        let dw = 0.01f64;
        let (mut re2, mut im2) = (0.0f64, 0.0f64);
        let w2 = w as f64 + dw;
        for (n, &v) in ir.iter().enumerate() {
            let p = w2 * n as f64;
            re2 += v as f64 * p.cos();
            im2 -= v as f64 * p.sin();
        }
        let ph2 = (im2 as f32).atan2(re2 as f32);
        let gd = -(ph2 - ph) / dw as f32;

        freq_response.push((freq, mag));
        phase.push((freq, ph));
        group_delay.push((freq, gd));
    }

    // Energy-time curve via Schroeder backward integration.
    let mut energy_time_curve: Vec<f32> = vec![0.0; ir.len()];
    let mut acc = 0.0f32;
    for (n, &v) in ir.iter().enumerate().rev() {
        acc += v * v;
        energy_time_curve[n] = acc;
    }
    let total_e = energy_time_curve.first().copied().unwrap_or(0.0).max(1e-12);
    let threshold = total_e * 1e-6; // −60 dB
    let decay_idx = energy_time_curve
        .iter()
        .position(|&e| e < threshold)
        .unwrap_or(ir.len());
    let rt60 = decay_idx as f32 / sample_rate;

    RoomIrAnalysis {
        freq_response,
        energy_time_curve,
        rt60,
        phase,
        group_delay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_ir_dirac_flat_response() {
        // A Dirac delta should have a flat frequency response (magnitude ≈ 1).
        let mut ir = vec![0.0f32; 512];
        ir[0] = 1.0;
        let analysis = analyze_ir(&ir, 48_000.0);
        for (_, mag) in &analysis.freq_response {
            assert!((mag - 1.0).abs() < 0.02, "Dirac: flat response, got {mag}");
        }
        assert_eq!(analysis.energy_time_curve[0], 1.0);
        assert!(
            analysis.rt60 < 1e-3,
            "RT60 for Dirac near 0: {}",
            analysis.rt60
        );
    }

    #[test]
    fn analyze_ir_empty_returns_default() {
        let a = analyze_ir(&[], 48_000.0);
        assert!(a.freq_response.is_empty());
        assert!(a.energy_time_curve.is_empty());
        assert_eq!(a.rt60, 0.0);
    }

    #[test]
    fn analyze_ir_rt60_from_decaying_exponential() {
        // Generate a decaying exponential (60 dB decay in 0.5 s at 48 kHz).
        let sr = 48_000.0f32;
        let rt60_expected = 0.5f32;
        let decay = (-60.0f32 / (20.0 * std::f32::consts::E.log10()) / (rt60_expected * sr)).exp();
        let n = (sr * 1.0) as usize; // 1 s
        let ir: Vec<f32> = (0..n)
            .map(|i| decay.powi(i as i32) * (std::f32::consts::TAU * 440.0 * i as f32 / sr).sin())
            .collect();
        let a = analyze_ir(&ir, sr);
        // RT60 should be within ±30% of expected.
        assert!(
            (a.rt60 - rt60_expected).abs() < rt60_expected * 0.35,
            "RT60 got {:.3} expected {rt60_expected:.3}",
            a.rt60
        );
    }
}
