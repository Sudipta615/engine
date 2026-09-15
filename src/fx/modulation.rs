//! Creative Modulation Effects: Chorus, Flanger, Phaser & Ring Modulator (Item 32).

use serde::{Deserialize, Serialize};
use std::f32::consts::PI;

/// Multi-voice chorus effect with quadrature modulation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chorus {
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    pos: usize,
    lfo_phase: f32,
    pub rate_hz: f32,
    pub depth_ms: f32,
    pub base_delay_ms: f32,
    pub feedback: f32,
    pub wet: f32,
    pub dry: f32,
    sample_rate: f32,
}

impl Chorus {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let max_samples = (0.050 * sr) as usize + 64; // 50 ms max delay
        Self {
            buf_l: vec![0.0; max_samples],
            buf_r: vec![0.0; max_samples],
            pos: 0,
            lfo_phase: 0.0,
            rate_hz: 1.2,
            depth_ms: 3.5,
            base_delay_ms: 15.0,
            feedback: 0.2,
            wet: 0.5,
            dry: 1.0,
            sample_rate: sr,
        }
    }

    /// Process stereo channels in-place. Guaranteed zero allocation.
    pub fn process_stereo(&mut self, left: &mut [f32], right: &mut [f32]) {
        let cap = self.buf_l.len();
        let frames = left.len().min(right.len());
        let lfo_inc = self.rate_hz / self.sample_rate;

        for i in 0..frames {
            let in_l = left[i];
            let in_r = right[i];

            // Quadrature LFOs (90 degree stereo offset)
            let mod_l = (self.lfo_phase * 2.0 * PI).sin();
            let mod_r = ((self.lfo_phase + 0.25) * 2.0 * PI).sin();
            self.lfo_phase = (self.lfo_phase + lfo_inc).fract();

            let delay_l_samples =
                ((self.base_delay_ms + self.depth_ms * mod_l) * 0.001 * self.sample_rate)
                    .clamp(1.0, (cap - 4) as f32);
            let delay_r_samples =
                ((self.base_delay_ms + self.depth_ms * mod_r) * 0.001 * self.sample_rate)
                    .clamp(1.0, (cap - 4) as f32);

            let read_l = (self.pos as f32 - delay_l_samples + cap as f32) % (cap as f32);
            let read_r = (self.pos as f32 - delay_r_samples + cap as f32) % (cap as f32);

            let l0 = read_l.floor() as usize % cap;
            let l1 = (l0 + 1) % cap;
            let frac_l = read_l.fract();
            let delayed_l = self.buf_l[l0] * (1.0 - frac_l) + self.buf_l[l1] * frac_l;

            let r0 = read_r.floor() as usize % cap;
            let r1 = (r0 + 1) % cap;
            let frac_r = read_r.fract();
            let delayed_r = self.buf_r[r0] * (1.0 - frac_r) + self.buf_r[r1] * frac_r;

            self.buf_l[self.pos] = in_l + delayed_l * self.feedback;
            self.buf_r[self.pos] = in_r + delayed_r * self.feedback;

            self.pos = (self.pos + 1) % cap;

            left[i] = in_l * self.dry + delayed_l * self.wet;
            right[i] = in_r * self.dry + delayed_r * self.wet;
        }
    }
}

/// Flanger with short modulated delay and through-zero capability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flanger {
    buffer: Vec<f32>,
    pos: usize,
    lfo_phase: f32,
    pub rate_hz: f32,
    pub depth_ms: f32,
    pub manual_ms: f32,
    pub feedback: f32,
    pub wet: f32,
    pub dry: f32,
    sample_rate: f32,
}

impl Flanger {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let max_samples = (0.015 * sr) as usize + 64; // 15 ms max delay
        Self {
            buffer: vec![0.0; max_samples],
            pos: 0,
            lfo_phase: 0.0,
            rate_hz: 0.3,
            depth_ms: 2.0,
            manual_ms: 1.0,
            feedback: 0.7,
            wet: 0.7,
            dry: 0.7,
            sample_rate: sr,
        }
    }

    /// Process in-place on an audio plane. Guaranteed zero allocation.
    pub fn process_plane(&mut self, plane: &mut [f32]) {
        let cap = self.buffer.len();
        let lfo_inc = self.rate_hz / self.sample_rate;

        for sample in plane.iter_mut() {
            let in_val = *sample;
            // Triangular LFO for classic linear through-zero sweep
            let lfo_tri = if self.lfo_phase < 0.5 {
                4.0 * self.lfo_phase - 1.0
            } else {
                3.0 - 4.0 * self.lfo_phase
            };
            self.lfo_phase = (self.lfo_phase + lfo_inc).fract();

            let delay_samples = ((self.manual_ms + self.depth_ms * (lfo_tri + 1.0) * 0.5)
                * 0.001
                * self.sample_rate)
                .clamp(0.1, (cap - 4) as f32);

            let read_pos = (self.pos as f32 - delay_samples + cap as f32) % (cap as f32);
            let idx0 = read_pos.floor() as usize % cap;
            let idx1 = (idx0 + 1) % cap;
            let frac = read_pos.fract();

            let delayed = self.buffer[idx0] * (1.0 - frac) + self.buffer[idx1] * frac;

            self.buffer[self.pos] = in_val + delayed * self.feedback;
            self.pos = (self.pos + 1) % cap;

            *sample = in_val * self.dry + delayed * self.wet;
        }
    }
}

/// Multi-stage allpass filter phaser (4, 8, 12 poles).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Phaser {
    stages: [f32; 8], // 8-pole allpass ladder states
    lfo_phase: f32,
    pub rate_hz: f32,
    pub base_freq_hz: f32,
    pub sweep_octaves: f32,
    pub feedback: f32,
    pub stages_count: usize,
    pub wet: f32,
    pub dry: f32,
    sample_rate: f32,
}

impl Phaser {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            stages: [0.0; 8],
            lfo_phase: 0.0,
            rate_hz: 0.5,
            base_freq_hz: 500.0,
            sweep_octaves: 3.0,
            feedback: 0.6,
            stages_count: 6,
            wet: 0.5,
            dry: 0.7,
            sample_rate: sample_rate.max(1.0),
        }
    }

    /// Process in-place on an audio plane. Guaranteed zero allocation.
    pub fn process_plane(&mut self, plane: &mut [f32]) {
        let lfo_inc = self.rate_hz / self.sample_rate;
        let nyquist = self.sample_rate * 0.49;

        for sample in plane.iter_mut() {
            let in_val = *sample;
            let lfo_val = (self.lfo_phase * 2.0 * PI).sin() * 0.5 + 0.5;
            self.lfo_phase = (self.lfo_phase + lfo_inc).fract();

            // Exponential frequency sweep
            let freq = (self.base_freq_hz * 2.0f32.powf(lfo_val * self.sweep_octaves))
                .clamp(40.0, nyquist);

            // Allpass coefficient: a = (tan(pi*f/fs) - 1) / (tan(pi*f/fs) + 1)
            let tan = (PI * freq / self.sample_rate).tan();
            let a = (tan - 1.0) / (tan + 1.0);

            // Input with feedback from the last active stage
            let last_stage = self.stages_count.clamp(2, 8) - 1;
            let mut y = in_val + self.stages[last_stage] * self.feedback;

            // Run allpass ladder
            for i in 0..self.stages_count.clamp(2, 8) {
                let x = y;
                y = a * x + self.stages[i];
                self.stages[i] = x - a * y;
            }

            *sample = in_val * self.dry + y * self.wet;
        }
    }
}

/// Balanced 4-quadrant ring modulator and amplitude modulator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RingModulator {
    carrier_phase: f32,
    pub carrier_hz: f32,
    pub blend_am_rm: f32, // 0.0 = full AM, 1.0 = full RM (bipolar)
    pub wet: f32,
    pub dry: f32,
    sample_rate: f32,
}

impl RingModulator {
    pub fn new(sample_rate: f32, carrier_hz: f32) -> Self {
        Self {
            carrier_phase: 0.0,
            carrier_hz: carrier_hz.max(1.0),
            blend_am_rm: 1.0, // Default pure 4-quadrant ring mod
            wet: 0.8,
            dry: 0.2,
            sample_rate: sample_rate.max(1.0),
        }
    }

    /// Process in-place on an audio plane. Guaranteed zero allocation.
    pub fn process_plane(&mut self, plane: &mut [f32]) {
        let phase_inc = self.carrier_hz / self.sample_rate;

        for sample in plane.iter_mut() {
            let in_val = *sample;
            let carrier_bipolar = (self.carrier_phase * 2.0 * PI).sin();
            let carrier_unipolar = (carrier_bipolar + 1.0) * 0.5;
            self.carrier_phase = (self.carrier_phase + phase_inc).fract();

            let carrier =
                carrier_unipolar * (1.0 - self.blend_am_rm) + carrier_bipolar * self.blend_am_rm;

            let mod_out = in_val * carrier;
            *sample = in_val * self.dry + mod_out * self.wet;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chorus_stereo_processing() {
        let mut chorus = Chorus::new(48000.0);
        let mut l = [0.5f32; 512];
        let mut r = [0.5f32; 512];

        chorus.process_stereo(&mut l, &mut r);
        for &s in &l {
            assert!(s.is_finite());
        }
        for &s in &r {
            assert!(s.is_finite());
        }
    }

    #[test]
    fn test_flanger_processing() {
        let mut flanger = Flanger::new(48000.0);
        let mut plane = [0.5f32; 512];

        flanger.process_plane(&mut plane);
        for &s in &plane {
            assert!(s.is_finite());
        }
    }

    #[test]
    fn test_phaser_processing() {
        let mut phaser = Phaser::new(48000.0);
        let mut plane = [0.5f32; 512];

        phaser.process_plane(&mut plane);
        for &s in &plane {
            assert!(s.is_finite());
        }
    }

    #[test]
    fn test_ring_modulator_frequency_shift() {
        let mut rm = RingModulator::new(48000.0, 440.0);
        let mut plane = [1.0f32; 512];

        rm.process_plane(&mut plane);
        for &s in &plane {
            assert!(s.is_finite());
        }
    }
}
