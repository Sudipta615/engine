//! ADSR Envelope Generator for modulation (Item 31).

use serde::{Deserialize, Serialize};

/// ADSR envelope generator state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvelopeStage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// Attack-Decay-Sustain-Release Envelope Generator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdsrEnvelope {
    pub attack_s: f32,
    pub decay_s: f32,
    pub sustain_level: f32,
    pub release_s: f32,
    pub sample_rate: f32,
    stage: EnvelopeStage,
    current_level: f32,
    gate: bool,
}

impl Default for AdsrEnvelope {
    fn default() -> Self {
        Self::new(48000.0, 0.01, 0.1, 0.7, 0.2)
    }
}

impl AdsrEnvelope {
    pub fn new(
        sample_rate: f32,
        attack_s: f32,
        decay_s: f32,
        sustain_level: f32,
        release_s: f32,
    ) -> Self {
        Self {
            attack_s: attack_s.max(0.0001),
            decay_s: decay_s.max(0.0001),
            sustain_level: sustain_level.clamp(0.0, 1.0),
            release_s: release_s.max(0.0001),
            sample_rate: sample_rate.max(1.0),
            stage: EnvelopeStage::Idle,
            current_level: 0.0,
            gate: false,
        }
    }

    /// Set gate / trigger state.
    pub fn set_gate(&mut self, gate: bool) {
        if gate && !self.gate {
            // Note on -> Attack
            self.stage = EnvelopeStage::Attack;
        } else if !gate && self.gate {
            // Note off -> Release
            self.stage = EnvelopeStage::Release;
        }
        self.gate = gate;
    }

    /// Current envelope stage.
    pub fn stage(&self) -> EnvelopeStage {
        self.stage
    }

    /// Reset envelope to idle.
    pub fn reset(&mut self) {
        self.stage = EnvelopeStage::Idle;
        self.current_level = 0.0;
        self.gate = false;
    }

    /// Advance envelope by one sample and return level in [0.0, 1.0].
    #[inline]
    pub fn next_sample(&mut self) -> f32 {
        match self.stage {
            EnvelopeStage::Idle => {
                self.current_level = 0.0;
            }
            EnvelopeStage::Attack => {
                let step = 1.0 / (self.attack_s * self.sample_rate);
                self.current_level += step;
                if self.current_level >= 1.0 - 1e-5 {
                    self.current_level = 1.0;
                    self.stage = EnvelopeStage::Decay;
                }
            }
            EnvelopeStage::Decay => {
                let step = (1.0 - self.sustain_level) / (self.decay_s * self.sample_rate);
                self.current_level -= step;
                if self.current_level <= self.sustain_level + 1e-5 {
                    self.current_level = self.sustain_level;
                    self.stage = EnvelopeStage::Sustain;
                }
            }
            EnvelopeStage::Sustain => {
                self.current_level = self.sustain_level;
            }
            EnvelopeStage::Release => {
                let step = self.sustain_level / (self.release_s * self.sample_rate);
                self.current_level -= step;
                if self.current_level <= 1e-5 {
                    self.current_level = 0.0;
                    self.stage = EnvelopeStage::Idle;
                }
            }
        }
        self.current_level
    }

    /// Render a block of envelope output values into `out`. Zero allocation.
    pub fn render_block(&mut self, out: &mut [f32]) {
        for s in out.iter_mut() {
            *s = self.next_sample();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adsr_stages_cycle() {
        let sr = 1000.0; // 1000 samples per sec for easy math
        let mut env = AdsrEnvelope::new(sr, 0.1, 0.1, 0.5, 0.1); // 100 samples each stage

        assert_eq!(env.stage(), EnvelopeStage::Idle);
        env.set_gate(true);
        assert_eq!(env.stage(), EnvelopeStage::Attack);

        let mut buf = [0.0f32; 100];
        env.render_block(&mut buf);
        assert_eq!(env.stage(), EnvelopeStage::Decay);
        assert!((buf[99] - 1.0).abs() < 0.02);

        env.render_block(&mut buf);
        assert_eq!(env.stage(), EnvelopeStage::Sustain);
        assert!((buf[99] - 0.5).abs() < 0.02);

        env.set_gate(false);
        assert_eq!(env.stage(), EnvelopeStage::Release);

        env.render_block(&mut buf);
        assert_eq!(env.stage(), EnvelopeStage::Idle);
        assert_eq!(buf[99], 0.0);
    }
}
