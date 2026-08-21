use config::CrossfeedProfile;

use super::biquad::{BiquadCoeffs, BiquadState};

/// Headphone Crossfeed DSP node
/// Reduces listening fatigue on hard-panned stereo tracks by blending
/// low-pass filtered and delayed audio from the opposite channel.
pub struct Crossfeed {
    enabled: bool,
    level: f32,
    profile: CrossfeedProfile,
    custom_freq: f32,
    custom_q: f32,
    custom_delay_ms: f32,
    sample_rate: f32,

    /// Biquad coefficients for the crossfeed low-pass filter
    coeffs: BiquadCoeffs<f64>,
    /// State for the Left-to-Right crossfeed filter
    state_lr: BiquadState<f64>,
    /// State for the Right-to-Left crossfeed filter
    state_rl: BiquadState<f64>,

    /// Delay ring buffer for Left-to-Right
    delay_lr: Vec<f64>,
    /// Delay ring buffer for Right-to-Left
    delay_rl: Vec<f64>,
    delay_pos: usize,
    delay_len: usize,
}

impl Crossfeed {
    pub fn new(sample_rate: f32) -> Self {
        let mut cf = Self {
            enabled: false,
            level: 1.0,
            profile: CrossfeedProfile::Bauer,
            custom_freq: 700.0,
            custom_q: 0.707,
            custom_delay_ms: 0.3,
            sample_rate,
            coeffs: BiquadCoeffs::identity(),
            state_lr: BiquadState::default(),
            state_rl: BiquadState::default(),
            delay_lr: Vec::new(),
            delay_rl: Vec::new(),
            delay_pos: 0,
            delay_len: 0,
        };
        cf.update_params();
        cf
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Fixed delay-line latency of the crossfeed network in milliseconds
    /// (0 when disabled). The low-pass biquads add phase delay too, but the
    /// *buffer* delay is the deterministic term worth accounting for in the
    /// graph latency model.
    pub fn latency_ms(&self) -> f32 {
        if !self.enabled || self.sample_rate <= 0.0 {
            0.0
        } else {
            self.delay_len as f32 / self.sample_rate * 1000.0
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            if !enabled {
                self.reset();
            }
        }
    }

    pub fn set_profile(&mut self, profile: CrossfeedProfile) {
        if self.profile != profile {
            self.profile = profile;
            self.update_params();
        }
    }

    pub fn set_level(&mut self, level: f32) {
        self.level = level.clamp(0.0, 1.0);
    }

    pub fn set_custom_params(&mut self, freq: f32, q: f32, delay_ms: f32) {
        self.custom_freq = freq;
        self.custom_q = q;
        self.custom_delay_ms = delay_ms;
        if self.profile == CrossfeedProfile::Custom {
            self.update_params();
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if (self.sample_rate - sample_rate).abs() > 0.01 {
            self.sample_rate = sample_rate;
            self.update_params();
            self.reset();
        }
    }

    fn update_params(&mut self) {
        let (freq, q, delay_ms) = match self.profile {
            CrossfeedProfile::Bauer => (700.0, 0.5, 0.3),
            CrossfeedProfile::ChuMoy => (700.0, 0.707, 0.25),
            CrossfeedProfile::Jmeier => (600.0, 0.6, 0.35),
            CrossfeedProfile::Custom => {
                let freq = if self.custom_freq <= 0.0 || !self.custom_freq.is_finite() {
                    log::warn!(
                        "Crossfeed custom_freq {} is invalid; clamping to 700.0",
                        self.custom_freq
                    );
                    700.0
                } else {
                    self.custom_freq
                };
                let q = if self.custom_q <= 0.0 || !self.custom_q.is_finite() {
                    log::warn!(
                        "Crossfeed custom_q {} is invalid; clamping to 0.707",
                        self.custom_q
                    );
                    0.707
                } else {
                    self.custom_q
                };
                let delay_ms = if self.custom_delay_ms < 0.0 || !self.custom_delay_ms.is_finite() {
                    log::warn!(
                        "Crossfeed custom_delay_ms {} is invalid; clamping to 0.3",
                        self.custom_delay_ms
                    );
                    0.3
                } else {
                    self.custom_delay_ms
                };
                (freq, q, delay_ms)
            }
        };

        self.coeffs = BiquadCoeffs::lowpass(self.sample_rate, freq, q);

        // Calculate delay in samples
        self.delay_len = ((delay_ms / 1000.0) * self.sample_rate) as usize;
        if self.delay_len == 0 {
            self.delay_len = 1; // Minimum 1 sample to avoid 0-capacity ring buffer logic
        }

        if self.delay_lr.len() != self.delay_len {
            self.delay_lr = vec![0.0; self.delay_len];
            self.delay_rl = vec![0.0; self.delay_len];
            self.delay_pos = 0;
        }
    }

    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.enabled || self.level <= 0.001 {
            return (left, right);
        }

        let (ol, or_) = self.process_f64(left as f64, right as f64);
        (ol as f32, or_ as f32)
    }

    /// Process a stereo sample pair in native f64 precision.
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if !self.enabled || self.level <= 0.001 {
            return (left, right);
        }

        // Apply low-pass filter to each channel to create the crossfeed signal
        let filtered_to_right = self.state_lr.process(left, &self.coeffs);
        let filtered_to_left = self.state_rl.process(right, &self.coeffs);

        // Process delay line
        let cross_to_right = self.delay_lr[self.delay_pos];
        let cross_to_left = self.delay_rl[self.delay_pos];

        self.delay_lr[self.delay_pos] = filtered_to_right;
        self.delay_rl[self.delay_pos] = filtered_to_left;

        self.delay_pos += 1;
        if self.delay_pos >= self.delay_len {
            self.delay_pos = 0;
        }

        // Blend the crossfeed signals
        let cross_level = 0.5 * (self.level as f64);
        let direct_level = 1.0 - cross_level;

        let out_l = (left * direct_level) + (cross_to_left * cross_level);
        let out_r = (right * direct_level) + (cross_to_right * cross_level);

        (out_l, out_r)
    }

    /// Process a block of stereo frames in place. Hoists the enabled/level
    /// checks out of the per-frame loop.
    #[inline]
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if !self.enabled || self.level <= 0.001 {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i] as f64, right[i] as f64);
            left[i] = ol as f32;
            right[i] = or_ as f32;
        }
    }

    /// Process a block of stereo frames in native f64 precision. Hoists the
    /// enabled/level checks out of the per-frame loop.
    #[inline]
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if !self.enabled || self.level <= 0.001 {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i], right[i]);
            left[i] = ol;
            right[i] = or_;
        }
    }

    pub fn reset(&mut self) {
        self.state_lr.reset();
        self.state_rl.reset();
        self.delay_lr.fill(0.0);
        self.delay_rl.fill(0.0);
        self.delay_pos = 0;
    }
}
