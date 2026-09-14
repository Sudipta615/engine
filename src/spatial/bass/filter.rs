//! Bass management crossover filters, delay compensation, and phase alignment.
//!
//! Provides Linkwitz-Riley (LR2, LR4, LR8) and Butterworth (12, 24, 48 dB/oct)
//! crossover filtering for mains high-pass and subwoofer low-pass paths,
//! sample-accurate delay compensation (0..50 ms), and phase alignment.

use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};
use config::{CrossoverFilterType, CrossoverSlope};

/// Maximum delay buffer capacity in samples (50 ms at 192 kHz = 9600 samples).
pub const MAX_SUB_DELAY_SAMPLES: usize = 9600;

/// Cascaded biquad crossover filter for mains high-pass or subwoofer low-pass.
#[derive(Debug, Clone)]
pub struct CrossoverFilter {
    coeffs: Vec<BiquadCoeffsF32>,
    states: Vec<BiquadStateF32>,
    sample_rate: f32,
    freq: f32,
    slope: CrossoverSlope,
    filter_type: CrossoverFilterType,
    is_highpass: bool,
}

impl CrossoverFilter {
    /// Construct a new crossover filter.
    pub fn new(
        sample_rate: f32,
        freq: f32,
        slope: CrossoverSlope,
        filter_type: CrossoverFilterType,
        is_highpass: bool,
    ) -> Self {
        let mut filter = Self {
            coeffs: Vec::new(),
            states: Vec::new(),
            sample_rate: sample_rate.max(1.0),
            freq: freq.clamp(20.0, 500.0),
            slope,
            filter_type,
            is_highpass,
        };
        filter.recompute_coeffs();
        filter
    }

    /// Update filter parameters and recompute coefficients.
    pub fn update(
        &mut self,
        sample_rate: f32,
        freq: f32,
        slope: CrossoverSlope,
        filter_type: CrossoverFilterType,
    ) {
        self.sample_rate = sample_rate.max(1.0);
        self.freq = freq.clamp(20.0, 500.0);
        self.slope = slope;
        self.filter_type = filter_type;
        self.recompute_coeffs();
    }

    /// Recompute cascaded biquad coefficients based on slope and filter type.
    fn recompute_coeffs(&mut self) {
        let sr = self.sample_rate;
        let fc = self.freq;
        let is_hp = self.is_highpass;

        // Determine Q values for each second-order section
        let q_values: Vec<f32> = match (self.filter_type, self.slope) {
            // Butterworth alignments:
            // 2nd order: single section, Q = 1/sqrt(2) = 0.7071
            (CrossoverFilterType::Butterworth, CrossoverSlope::Db12) => {
                vec![std::f32::consts::FRAC_1_SQRT_2]
            }
            // 4th order: 2 sections with Butterworth pole pairs
            (CrossoverFilterType::Butterworth, CrossoverSlope::Db24) => {
                vec![0.5411961, 1.306_563]
            }
            // 8th order: 4 sections
            (CrossoverFilterType::Butterworth, CrossoverSlope::Db48) => {
                vec![0.5097956, 0.6013449, 0.8999762, 2.5629155]
            }

            // Linkwitz-Riley alignments:
            // LR2 (12 dB/oct): critically damped 2nd order (Q = 0.5)
            (CrossoverFilterType::LinkwitzRiley, CrossoverSlope::Db12) => {
                vec![0.5]
            }
            // LR4 (24 dB/oct): two cascaded 2nd-order Butterworth filters (Q = 1/sqrt(2) each)
            (CrossoverFilterType::LinkwitzRiley, CrossoverSlope::Db24) => {
                vec![
                    std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                ]
            }
            // LR8 (48 dB/oct): two cascaded 4th-order Butterworth filters (4 sections)
            (CrossoverFilterType::LinkwitzRiley, CrossoverSlope::Db48) => {
                vec![0.5411961, 1.306_563, 0.5411961, 1.306_563]
            }
        };

        self.coeffs.clear();
        for &q in &q_values {
            let c = if is_hp {
                BiquadCoeffsF32::highpass(sr, fc, q)
            } else {
                BiquadCoeffsF32::lowpass(sr, fc, q)
            };
            self.coeffs.push(c);
        }

        if self.states.len() != self.coeffs.len() {
            self.states = vec![BiquadStateF32::default(); self.coeffs.len()];
        }
    }

    /// Process a single sample through all cascaded biquad stages.
    #[inline]
    pub fn process_sample(&mut self, mut sample: f32) -> f32 {
        for (state, coeff) in self.states.iter_mut().zip(self.coeffs.iter()) {
            sample = state.process(sample, coeff);
        }
        sample
    }

    /// In-place block processing.
    #[inline]
    pub fn process_block(&mut self, block: &mut [f32]) {
        for s in block.iter_mut() {
            *s = self.process_sample(*s);
        }
    }

    /// Reset filter state memories.
    pub fn reset(&mut self) {
        for state in &mut self.states {
            state.reset();
        }
    }
}

/// Subwoofer delay line for distance / arrival time compensation (0..50 ms).
#[derive(Debug, Clone)]
pub struct SubwooferDelay {
    buffer: Vec<f32>,
    write_idx: usize,
    delay_samples: f32,
    sample_rate: f32,
}

impl SubwooferDelay {
    pub fn new(sample_rate: f32, delay_ms: f32) -> Self {
        let mut delay = Self {
            buffer: vec![0.0; MAX_SUB_DELAY_SAMPLES],
            write_idx: 0,
            delay_samples: 0.0,
            sample_rate: sample_rate.max(1.0),
        };
        delay.set_delay_ms(delay_ms, sample_rate);
        delay
    }

    /// Set delay time in milliseconds (clamped to 0.0 .. 50.0 ms).
    pub fn set_delay_ms(&mut self, delay_ms: f32, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        let max_ms = (MAX_SUB_DELAY_SAMPLES - 2) as f32 / self.sample_rate * 1000.0;
        let clamped_ms = delay_ms.clamp(0.0, max_ms.min(50.0));
        self.delay_samples = clamped_ms * 0.001 * self.sample_rate;
    }

    /// Process a single sample through the delay line with linear interpolation.
    #[inline]
    pub fn process_sample(&mut self, sample: f32) -> f32 {
        if self.delay_samples <= 0.001 {
            return sample;
        }

        let cap = self.buffer.len();
        self.buffer[self.write_idx] = sample;

        let delay_int = self.delay_samples.floor() as usize;
        let delay_frac = self.delay_samples - delay_int as f32;

        let read_idx0 = (self.write_idx + cap - delay_int) % cap;
        let read_idx1 = (read_idx0 + cap - 1) % cap;

        let s0 = self.buffer[read_idx0];
        let s1 = self.buffer[read_idx1];
        let out = s0 + delay_frac * (s1 - s0);

        self.write_idx = (self.write_idx + 1) % cap;
        out
    }

    /// Process a block in-place.
    #[inline]
    pub fn process_block(&mut self, block: &mut [f32]) {
        for s in block.iter_mut() {
            *s = self.process_sample(*s);
        }
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_idx = 0;
    }
}

/// Subwoofer phase alignment (0..180° rotation plus polarity inversion).
#[derive(Debug, Clone)]
pub struct SubwooferPhase {
    allpass_coeffs: BiquadCoeffsF32,
    allpass_state: BiquadStateF32,
    phase_degrees: f32,
    polarity_invert: bool,
}

impl SubwooferPhase {
    pub fn new(
        sample_rate: f32,
        crossover_hz: f32,
        phase_degrees: f32,
        polarity_invert: bool,
    ) -> Self {
        let mut phase = Self {
            allpass_coeffs: BiquadCoeffsF32::identity(),
            allpass_state: BiquadStateF32::default(),
            phase_degrees: phase_degrees.clamp(0.0, 180.0),
            polarity_invert,
        };
        phase.recompute(sample_rate, crossover_hz);
        phase
    }

    pub fn set_parameters(
        &mut self,
        sample_rate: f32,
        crossover_hz: f32,
        phase_degrees: f32,
        polarity_invert: bool,
    ) {
        self.phase_degrees = phase_degrees.clamp(0.0, 180.0);
        self.polarity_invert = polarity_invert;
        self.recompute(sample_rate, crossover_hz);
    }

    fn recompute(&mut self, sample_rate: f32, crossover_hz: f32) {
        if self.phase_degrees <= 0.5 {
            self.allpass_coeffs = BiquadCoeffsF32::identity();
            return;
        }

        // Q for allpass controls the phase transition steepness at the crossover frequency
        // Mapping phase_degrees [0, 180] to allpass filter parameters
        let q = (self.phase_degrees / 90.0).clamp(0.1, 5.0);
        self.allpass_coeffs = BiquadCoeffsF32::allpass(sample_rate, crossover_hz, q);
    }

    #[inline]
    pub fn process_sample(&mut self, sample: f32) -> f32 {
        let rotated = if self.phase_degrees > 0.5 {
            self.allpass_state.process(sample, &self.allpass_coeffs)
        } else {
            sample
        };

        if self.polarity_invert {
            -rotated
        } else {
            rotated
        }
    }

    #[inline]
    pub fn process_block(&mut self, block: &mut [f32]) {
        for s in block.iter_mut() {
            *s = self.process_sample(*s);
        }
    }

    pub fn reset(&mut self) {
        self.allpass_state.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lr4_crossover_sum_is_flat_at_crossover() {
        let sr = 48000.0;
        let fc = 80.0;
        let mut hp = CrossoverFilter::new(
            sr,
            fc,
            CrossoverSlope::Db24,
            CrossoverFilterType::LinkwitzRiley,
            true,
        );
        let mut lp = CrossoverFilter::new(
            sr,
            fc,
            CrossoverSlope::Db24,
            CrossoverFilterType::LinkwitzRiley,
            false,
        );

        // Feed test tone at crossover frequency
        let n_samples = 3000;
        let mut max_hp_lp_diff = 0.0f32;
        let mut max_sum = 0.0f32;

        for i in 0..n_samples {
            let t = i as f32 / sr;
            let input = (2.0 * std::f32::consts::PI * fc * t).sin();
            let y_hp = hp.process_sample(input);
            let y_lp = lp.process_sample(input);

            // After transient settles, LR4 HP and LP are exactly in phase with each other:
            // At fc, both outputs are identical (-0.5 * input), summing to -input (unity magnitude, 180° phase)
            if i > 2000 {
                let hp_lp_diff = (y_hp - y_lp).abs();
                if hp_lp_diff > max_hp_lp_diff {
                    max_hp_lp_diff = hp_lp_diff;
                }
                let sum = (y_hp + y_lp).abs();
                if sum > max_sum {
                    max_sum = sum;
                }
            }
        }

        // Both sections are in phase (difference between HP and LP is negligible)
        assert!(
            max_hp_lp_diff < 0.05,
            "LR4 HP and LP should be in phase at fc, diff: {}",
            max_hp_lp_diff
        );
        // The sum magnitude at fc reaches peak amplitude 1.0 (flat 0 dB magnitude)
        assert!(
            (max_sum - 1.0).abs() < 0.05,
            "LR4 sum magnitude should be unity (1.0), got: {}",
            max_sum
        );
    }

    #[test]
    fn subwoofer_delay_delays_signal() {
        let sr = 48000.0;
        let delay_ms = 10.0; // 480 samples
        let mut delay = SubwooferDelay::new(sr, delay_ms);

        let mut impulse = vec![0.0f32; 600];
        impulse[0] = 1.0;

        delay.process_block(&mut impulse);

        // Peak should be at sample 480
        let (peak_idx, &peak_val) = impulse
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .unwrap();

        assert_eq!(peak_idx, 480);
        assert!((peak_val - 1.0).abs() < 1e-4);
    }

    #[test]
    fn subwoofer_phase_inverts_polarity() {
        let mut phase = SubwooferPhase::new(48000.0, 80.0, 0.0, true);
        assert_eq!(phase.process_sample(0.5), -0.5);
    }
}
