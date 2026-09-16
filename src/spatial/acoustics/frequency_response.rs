//! Log-spaced frequency response analysis and fractional octave smoothing (§11.1, Item 34).
//!
//! Provides magnitude spectrum calculation, logarithmic frequency binning, and
//! standard 1/1, 1/3, 1/6, 1/12, and 1/24 octave smoothing filters.

use realfft::num_complex::Complex32;
use realfft::RealFftPlanner;
use serde::{Deserialize, Serialize};

/// Fractional octave smoothing bandwidth setting.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum OctaveSmoothing {
    None,
    Octave1_1,
    Octave1_3,
    Octave1_6,
    Octave1_12,
    Octave1_24,
}

impl OctaveSmoothing {
    /// Fractional bandwidth $Q = 1 / N$ in octaves.
    pub fn bandwidth_octaves(&self) -> f64 {
        match self {
            Self::None => 0.0,
            Self::Octave1_1 => 1.0,
            Self::Octave1_3 => 1.0 / 3.0,
            Self::Octave1_6 => 1.0 / 6.0,
            Self::Octave1_12 => 1.0 / 12.0,
            Self::Octave1_24 => 1.0 / 24.0,
        }
    }
}

/// Frequency response curve representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrequencyResponse {
    pub frequencies: Vec<f64>,
    pub magnitude_db: Vec<f64>,
    pub smoothing: OctaveSmoothing,
}

/// Computes the raw FFT magnitude frequency response of an impulse response.
pub fn compute_magnitude_response(ir: &[f32], sample_rate: f64) -> FrequencyResponse {
    let mut fft_size = 1024;
    while fft_size < ir.len() {
        fft_size <<= 1;
    }

    let mut in_buf = vec![0.0f32; fft_size];
    in_buf[..ir.len()].copy_from_slice(ir);

    let num_bins = fft_size / 2 + 1;
    let mut planner = RealFftPlanner::<f32>::new();
    let rfft = planner.plan_fft_forward(fft_size);
    let mut spec = vec![Complex32::new(0.0, 0.0); num_bins];
    let _ = rfft.process(&mut in_buf, &mut spec);

    let bin_step = sample_rate / fft_size as f64;
    let mut frequencies = Vec::with_capacity(num_bins);
    let mut magnitude_db = Vec::with_capacity(num_bins);

    for (bin, s) in spec.iter().enumerate() {
        frequencies.push(bin as f64 * bin_step);
        let mag = (s.re * s.re + s.im * s.im).sqrt() as f64;
        let db = if mag > 1e-9 {
            20.0 * mag.log10()
        } else {
            -180.0
        };
        magnitude_db.push(db);
    }

    FrequencyResponse {
        frequencies,
        magnitude_db,
        smoothing: OctaveSmoothing::None,
    }
}

/// Applies fractional octave smoothing to a frequency response curve.
pub fn smooth_frequency_response(
    response: &FrequencyResponse,
    smoothing: OctaveSmoothing,
) -> FrequencyResponse {
    let oct_bw = smoothing.bandwidth_octaves();
    if oct_bw <= 0.0 || response.frequencies.len() < 2 {
        return response.clone();
    }

    let n = response.frequencies.len();
    let mut smoothed_db = Vec::with_capacity(n);
    let ratio = 2.0f64.powf(oct_bw / 2.0);

    for (i, &fc) in response.frequencies.iter().enumerate() {
        if fc <= 1e-3 {
            smoothed_db.push(response.magnitude_db[i]);
            continue;
        }

        let f_low = fc / ratio;
        let f_high = fc * ratio;

        let mut sum_power = 0.0;
        let mut count = 0;

        for (j, &f) in response.frequencies.iter().enumerate() {
            if f >= f_low && f <= f_high {
                // Convert dB to linear power for physical energy averaging
                let lin_power = 10.0f64.powf(response.magnitude_db[j] / 10.0);
                sum_power += lin_power;
                count += 1;
            }
        }

        if count > 0 {
            let avg_power = sum_power / count as f64;
            let avg_db = if avg_power > 1e-12 {
                10.0 * avg_power.log10()
            } else {
                -180.0
            };
            smoothed_db.push(avg_db);
        } else {
            smoothed_db.push(response.magnitude_db[i]);
        }
    }

    FrequencyResponse {
        frequencies: response.frequencies.clone(),
        magnitude_db: smoothed_db,
        smoothing,
    }
}
