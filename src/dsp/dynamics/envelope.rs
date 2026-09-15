//! Ballistic envelope follower implementation.
//!
//! Provides asymmetric attack/release smoothing with hold-time support,
//! suitable for peak and RMS envelope estimation across dynamics processors.

/// Core ballistic envelope follower.
#[derive(Debug, Clone)]
pub struct BallisticEnvelope {
    sample_rate: f32,
    attack_ms: f32,
    release_ms: f32,
    hold_ms: f32,
    attack_coeff: f32,
    release_coeff: f32,
    hold_samples: usize,
    hold_counter: usize,
    envelope: f32,
}

impl BallisticEnvelope {
    /// Create a new ballistic envelope follower.
    pub fn new(sample_rate: f32, attack_ms: f32, release_ms: f32, hold_ms: f32) -> Self {
        let mut env = Self {
            sample_rate: sample_rate.max(1.0),
            attack_ms: attack_ms.max(0.001),
            release_ms: release_ms.max(0.001),
            hold_ms: hold_ms.max(0.0),
            attack_coeff: 0.0,
            release_coeff: 0.0,
            hold_samples: 0,
            hold_counter: 0,
            envelope: 0.0,
        };
        env.recompute_coeffs();
        env
    }

    /// Update sample rate and recompute timing coefficients.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.recompute_coeffs();
    }

    /// Update attack time in milliseconds.
    pub fn set_attack_ms(&mut self, attack_ms: f32) {
        self.attack_ms = attack_ms.max(0.001);
        self.recompute_coeffs();
    }

    /// Update release time in milliseconds.
    pub fn set_release_ms(&mut self, release_ms: f32) {
        self.release_ms = release_ms.max(0.001);
        self.recompute_coeffs();
    }

    /// Update hold time in milliseconds.
    pub fn set_hold_ms(&mut self, hold_ms: f32) {
        self.hold_ms = hold_ms.max(0.0);
        self.recompute_coeffs();
    }

    /// Reset internal envelope and hold state to zero.
    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.hold_counter = 0;
    }

    /// Advance the envelope with an incoming instantaneous level.
    #[inline]
    pub fn process_sample(&mut self, input_level: f32) -> f32 {
        let x = input_level.abs();
        if x >= self.envelope {
            // Attack phase
            self.envelope = x + self.attack_coeff * (self.envelope - x);
            self.hold_counter = self.hold_samples;
        } else if self.hold_counter > 0 {
            // Hold phase
            self.hold_counter -= 1;
        } else {
            // Release phase
            self.envelope = x + self.release_coeff * (self.envelope - x);
        }

        // Flush subnormal floats
        if self.envelope < 1e-15 {
            self.envelope = 0.0;
        }

        self.envelope
    }

    /// Current envelope value (linear level).
    #[inline]
    pub fn current_value(&self) -> f32 {
        self.envelope
    }

    fn recompute_coeffs(&mut self) {
        let fs = self.sample_rate;
        // Standard one-pole analog-matched attack and release coefficients (t_60 response)
        self.attack_coeff = (-1.0 / (self.attack_ms * 0.001 * fs)).exp();
        self.release_coeff = (-1.0 / (self.release_ms * 0.001 * fs)).exp();
        self.hold_samples = (self.hold_ms * 0.001 * fs).round() as usize;
    }
}
