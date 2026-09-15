//! Real-time transient onset detector for adaptive hybrid time-stretching.
//!
//! Uses dual short-time and long-time energy envelopes combined with an onset
//! ratio and hold timer. When a percussive attack or transient burst occurs,
//! the detector flags the frame to preserve time-domain punch and prevent
//! phase vocoder smearing.

/// Real-time transient detector with energy onset tracking and hold protection.
#[derive(Debug, Clone)]
pub struct TransientDetector {
    /// Fast short-time energy tracking envelope (~1.5 ms)
    fast_energy: f32,
    /// Slow long-time energy tracking envelope (~12 ms)
    slow_energy: f32,
    /// Decay factor for fast envelope
    fast_alpha: f32,
    /// Decay factor for slow envelope
    slow_alpha: f32,
    /// Sensitivity threshold for onset ratio
    threshold: f32,
    /// Minimum energy floor to avoid false positives on background silence
    min_energy_floor: f32,
    /// Hold timer remaining in samples
    hold_remaining: usize,
    /// Hold duration in samples (~10 ms)
    hold_samples: usize,
    /// Flag indicating whether the current block/sample is in a transient state
    is_active: bool,
}

impl Default for TransientDetector {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

impl TransientDetector {
    /// Create a new `TransientDetector` tuned for `sample_rate`.
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };

        // Fast time constant: ~1.5 ms
        let fast_alpha = (-1.0 / (0.0015 * sr)).exp();
        // Slow time constant: ~12.0 ms
        let slow_alpha = (-1.0 / (0.0120 * sr)).exp();
        // Hold duration: ~8.0 ms (preserves full attack envelope)
        let hold_samples = (0.008 * sr).round() as usize;

        Self {
            fast_energy: 0.0,
            slow_energy: 0.0,
            fast_alpha,
            slow_alpha,
            threshold: 2.2,
            min_energy_floor: 1e-4,
            hold_remaining: 0,
            hold_samples,
            is_active: false,
        }
    }

    /// Reset internal envelope history and hold counters.
    pub fn reset(&mut self) {
        self.fast_energy = 0.0;
        self.slow_energy = 0.0;
        self.hold_remaining = 0;
        self.is_active = false;
    }

    /// Process a stereo sample pair `(left, right)` and return whether a
    /// transient onset or hold state is active.
    #[inline]
    pub fn process_sample(&mut self, left: f32, right: f32) -> bool {
        let e = 0.5 * (left * left + right * right);

        // Update one-pole energy tracking filters
        self.fast_energy = self.fast_alpha * self.fast_energy + (1.0 - self.fast_alpha) * e;
        self.slow_energy = self.slow_alpha * self.slow_energy + (1.0 - self.slow_alpha) * e;

        let ratio = (self.fast_energy + 1e-7) / (self.slow_energy + 1e-7);

        if ratio > self.threshold && self.fast_energy > self.min_energy_floor {
            self.hold_remaining = self.hold_samples;
        }

        if self.hold_remaining > 0 {
            self.hold_remaining -= 1;
            self.is_active = true;
        } else {
            self.is_active = false;
        }

        self.is_active
    }

    /// Process a block of stereo frames and return the fraction of frames (0.0 to 1.0)
    /// identified as transient.
    pub fn process_block(&mut self, left: &[f32], right: &[f32]) -> f32 {
        let n = left.len().min(right.len());
        if n == 0 {
            return 0.0;
        }
        let mut transient_count = 0usize;
        for i in 0..n {
            if self.process_sample(left[i], right[i]) {
                transient_count += 1;
            }
        }
        transient_count as f32 / n as f32
    }

    /// Query whether a transient is currently active.
    #[inline]
    pub fn is_transient(&self) -> bool {
        self.is_active
    }
}
