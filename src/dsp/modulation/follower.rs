//! Audio Envelope Follower for modulation (Item 31).

use serde::{Deserialize, Serialize};

/// Detection mode for envelope following.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FollowerMode {
    Peak,
    Rms,
}

/// Envelope follower tracking audio dynamics into a modulation signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvelopeFollower {
    pub mode: FollowerMode,
    pub attack_s: f32,
    pub release_s: f32,
    pub gain_boost: f32,
    sample_rate: f32,
    envelope: f32,
    rms_sum: f32,
}

impl Default for EnvelopeFollower {
    fn default() -> Self {
        Self::new(48000.0, 0.005, 0.05)
    }
}

impl EnvelopeFollower {
    pub fn new(sample_rate: f32, attack_s: f32, release_s: f32) -> Self {
        Self {
            mode: FollowerMode::Peak,
            attack_s: attack_s.max(0.0001),
            release_s: release_s.max(0.0001),
            gain_boost: 1.0,
            sample_rate: sample_rate.max(1.0),
            envelope: 0.0,
            rms_sum: 0.0,
        }
    }

    /// Process a block of audio and write the detected envelope into `out`. Zero allocation.
    pub fn process_block(&mut self, input: &[f32], out: &mut [f32]) {
        let att_coeff = (-1.0 / (self.attack_s * self.sample_rate)).exp();
        let rel_coeff = (-1.0 / (self.release_s * self.sample_rate)).exp();

        for (&x, dst) in input.iter().zip(out.iter_mut()) {
            let target = match self.mode {
                FollowerMode::Peak => x.abs(),
                FollowerMode::Rms => {
                    self.rms_sum += 0.01 * (x * x - self.rms_sum);
                    self.rms_sum.sqrt()
                }
            } * self.gain_boost;

            if target > self.envelope {
                self.envelope = target + att_coeff * (self.envelope - target);
            } else {
                self.envelope = target + rel_coeff * (self.envelope - target);
            }

            *dst = self.envelope.clamp(0.0, 1.0);
        }
    }

    /// Current envelope level.
    pub fn level(&self) -> f32 {
        self.envelope
    }

    /// Reset follower state to 0.
    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.rms_sum = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_follower_attack_and_release() {
        let mut fol = EnvelopeFollower::new(48000.0, 0.001, 0.01);
        let mut audio = [0.0f32; 1000];
        audio[..500].fill(0.8); // burst of sound

        let mut env = [0.0f32; 1000];
        fol.process_block(&audio, &mut env);

        // Rises towards 0.8
        assert!(env[400] > 0.7);
        // Decays back towards 0
        assert!(env[999] < env[400]);
    }
}
