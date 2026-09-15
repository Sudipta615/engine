//! Creative Time-Domain Effects: Comb Filter & Ping-Pong Delay (Item 32).

use serde::{Deserialize, Serialize};

/// Feedback and feedforward comb filter with frequency damping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CombFilter {
    buffer: Vec<f32>,
    pos: usize,
    pub delay_samples: f32,
    pub feedback: f32,
    pub damping: f32,
    damp_state: f32,
}

impl CombFilter {
    pub fn new(max_delay_samples: usize, delay_samples: f32, feedback: f32, damping: f32) -> Self {
        let cap = max_delay_samples.max(64) + 4;
        Self {
            buffer: vec![0.0; cap],
            pos: 0,
            delay_samples: delay_samples.clamp(1.0, (cap - 2) as f32),
            feedback: feedback.clamp(-0.999, 0.999),
            damping: damping.clamp(0.0, 0.99),
            damp_state: 0.0,
        }
    }

    /// Process in-place across an audio plane. Guaranteed zero allocation.
    pub fn process_plane(&mut self, plane: &mut [f32]) {
        let cap = self.buffer.len();
        for sample in plane.iter_mut() {
            let read_pos = (self.pos as f32 - self.delay_samples + cap as f32) % (cap as f32);
            let idx0 = read_pos.floor() as usize % cap;
            let idx1 = (idx0 + 1) % cap;
            let frac = read_pos.fract();

            let delayed = self.buffer[idx0] * (1.0 - frac) + self.buffer[idx1] * frac;

            // Damping one-pole filter in feedback loop
            self.damp_state = delayed * (1.0 - self.damping) + self.damp_state * self.damping;

            let in_val = *sample;
            let out_val = in_val + delayed;
            let fb_val = in_val + self.damp_state * self.feedback;

            self.buffer[self.pos] = fb_val;
            self.pos = (self.pos + 1) % cap;

            *sample = out_val;
        }
    }

    /// Reset internal delay line and damping filter.
    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.pos = 0;
        self.damp_state = 0.0;
    }
}

/// Stereo ping-pong cross-feedback delay with damping and wet/dry mix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingPongDelay {
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    pos: usize,
    pub delay_samples: usize,
    pub feedback: f32,
    pub damping: f32,
    pub wet: f32,
    pub dry: f32,
    damp_l: f32,
    damp_r: f32,
}

impl PingPongDelay {
    pub fn new(max_delay_samples: usize, delay_samples: usize, feedback: f32) -> Self {
        let cap = max_delay_samples.max(64) + 4;
        Self {
            buf_l: vec![0.0; cap],
            buf_r: vec![0.0; cap],
            pos: 0,
            delay_samples: delay_samples.clamp(1, cap - 2),
            feedback: feedback.clamp(0.0, 0.98),
            damping: 0.2,
            wet: 0.5,
            dry: 1.0,
            damp_l: 0.0,
            damp_r: 0.0,
        }
    }

    /// Process stereo channels in-place. Guaranteed zero allocation.
    pub fn process_stereo(&mut self, left: &mut [f32], right: &mut [f32]) {
        let cap = self.buf_l.len();
        let frames = left.len().min(right.len());

        for i in 0..frames {
            let in_l = left[i];
            let in_r = right[i];

            let read_idx = (self.pos + cap - self.delay_samples) % cap;
            let delayed_l = self.buf_l[read_idx];
            let delayed_r = self.buf_r[read_idx];

            // One-pole damping
            self.damp_l = delayed_l * (1.0 - self.damping) + self.damp_l * self.damping;
            self.damp_r = delayed_r * (1.0 - self.damping) + self.damp_r * self.damping;

            // Cross-feedback: L feeds R, R feeds L
            self.buf_l[self.pos] = in_l + self.damp_r * self.feedback;
            self.buf_r[self.pos] = in_r + self.damp_l * self.feedback;

            self.pos = (self.pos + 1) % cap;

            left[i] = in_l * self.dry + delayed_l * self.wet;
            right[i] = in_r * self.dry + delayed_r * self.wet;
        }
    }

    /// Reset internal delay memory.
    pub fn reset(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.pos = 0;
        self.damp_l = 0.0;
        self.damp_r = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_comb_filter_impulse_response() {
        let mut comb = CombFilter::new(1000, 100.0, 0.5, 0.0);
        let mut block = [0.0f32; 350];
        block[0] = 1.0; // impulse

        comb.process_plane(&mut block);

        assert_eq!(block[0], 1.0); // direct
        assert!((block[100] - 1.0).abs() < 1e-3); // first delayed tap
        assert!((block[200] - 0.5).abs() < 1e-3); // second tap with 0.5 feedback
        assert!((block[300] - 0.25).abs() < 1e-3); // third tap with 0.25 feedback
    }

    #[test]
    fn test_ping_pong_cross_feedback() {
        let mut pp = PingPongDelay::new(1000, 50, 0.5);
        pp.damping = 0.0;
        pp.dry = 0.0;
        pp.wet = 1.0;

        let mut left = [0.0f32; 150];
        let mut right = [0.0f32; 150];
        left[0] = 1.0; // pulse only on Left

        pp.process_stereo(&mut left, &mut right);

        // Left receives the pulse initially into buf_l
        // At sample 50, delayed_l emerges on Left
        assert!((left[50] - 1.0).abs() < 1e-3);
        // And at sample 50, buf_l's output feeds into Right's buffer!
        // So at sample 100, delayed_r emerges on Right with feedback!
        assert!((right[100] - 0.5).abs() < 1e-3);
    }
}
