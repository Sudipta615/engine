//! Dithering for bit-depth reduction
//!
//! When reducing bit depth (e.g. floating-point → 16-bit integer), quantization
//! error introduces harmonic distortion.  Dither decorrelates this error from the
//! signal, replacing distortion with spectrally-flat noise.
//!
//! ## Mode selection
//!
//! | Mode                 | Recommended for                                      |
//! |----------------------|------------------------------------------------------|
//! | `None`               | 32-bit float output (no quantization)                |
//! | `Triangular`         | 16-bit / 24-bit integer output (default)             |
//! | `HighPassTriangular` | 16-bit; pushes dither noise to high frequencies      |
//! | `Shibata`            | 16-bit; minimum perceptual noise (psychoacoustic)    |
//! | `Rectangular`        | Debug / measurement only                              |
//! | `NoiseShaped`        | **DEPRECATED** — use `Triangular` instead            |
//!
//! ## Float-output guard
//!
//! When the hardware output format is `f32` or `f64`, no quantization occurs
//! and dither MUST NOT be applied (it would add audible noise for no benefit).
//! Call [`Dither::set_output_is_float`] with `true` in this case to engage
//! the automatic bypass.

/// Dither mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DitherType {
    /// No dithering — fastest, but introduces quantization distortion at low levels.
    None,
    /// Rectangular PDF dither: one uniform random sample per channel.
    /// Suitable only for debug/measurement.  Prefer `Triangular` for production.
    Rectangular,
    /// Triangular PDF dither (TPDF): sum of two independent rectangular
    /// sources.  Eliminates all harmonic distortion from quantization.
    /// **Recommended for all production use.**
    Triangular,
    /// High-pass TPDF: difference of two consecutive rectangular samples.
    /// Spectrally-white dither energy is shaped towards Nyquist, keeping
    /// low-frequency noise floor lower than plain TPDF.
    HighPassTriangular,
    /// Shibata F-weighted IIR noise shaping.
    ///
    /// Uses a 9-tap error-feedback FIR with psychoacoustically optimised
    /// coefficients for 44.1 kHz and 48 kHz.  This achieves the lowest
    /// perceptible noise floor of all modes.
    Shibata,
    /// First-order error-feedback noise shaping.
    ///
    /// **DEPRECATED.**  Retained for backward compatibility only.
    /// New code should use `Triangular`.
    #[deprecated(
        since = "0.22.0",
        note = "Undefined transfer function; use DitherType::Triangular instead."
    )]
    NoiseShaped,
}

/// Shibata F-weighted noise shaping coefficients for 44.1 kHz (9-tap IIR error feedback).
/// Derived from Shibata's psychoacoustically optimised design.
const SHIBATA_COEFFS_44100: [f32; 9] = [
    2.0860, -2.5061, 2.1855, -1.7032, 1.0982, -0.5671, 0.2376, -0.0669, 0.0101,
];

/// Shibata F-weighted noise shaping coefficients for 48 kHz.
const SHIBATA_COEFFS_48000: [f32; 9] = [
    2.2374, -2.7120, 2.3784, -1.8640, 1.2035, -0.6267, 0.2625, -0.0740, 0.0119,
];

/// Dither processor.
///
/// Owned by [`crate::output::format_converter::AudioFormatConverter`] which
/// applies it exactly once, immediately before the integer-quantization step.
///
/// The `Dither` struct inside `DspPipeline` is deprecated and will be removed
/// in a future release.
#[derive(Debug, Clone)]
pub struct Dither {
    dither_type: DitherType,
    bit_depth: u32,
    /// Previous quantization error for noise shaping (left channel)
    shape_left: f32,
    /// Previous quantization error for noise shaping (right channel)
    shape_right: f32,
    /// PRNG state for left channel (xorshift64)
    rng_state_left: u64,
    /// PRNG state for right channel (xorshift64)
    rng_state_right: u64,
    enabled: bool,
    /// When true, the output format is f32 or f64 (no quantization).
    /// Dither is unconditionally disabled regardless of all other settings.
    output_is_float: bool,
    // HP-TPDF state: previous rectangular sample for each channel
    hp_prev_left: f32,
    hp_prev_right: f32,
    // Shibata 9-tap IIR error history for left and right
    shibata_err_left: [f32; 9],
    shibata_err_right: [f32; 9],
    shibata_err_pos: usize,
    // Active Shibata coefficients (set from sample_rate at construction)
    shibata_coeffs: [f32; 9],
}

impl Dither {
    /// Create a new dither processor.
    ///
    /// # Arguments
    /// * `dither_type` — The dither algorithm to use
    /// * `bit_depth`   — Target bit depth (1–32). Dither is a no-op at ≥ 32.
    pub fn new(dither_type: DitherType, bit_depth: u32) -> Self {
        Self::with_sample_rate(dither_type, bit_depth, 44100)
    }

    /// Create a dither processor with sample-rate-aware Shibata coefficients.
    pub fn with_sample_rate(dither_type: DitherType, bit_depth: u32, sample_rate: u32) -> Self {
        let shibata_coeffs = if sample_rate <= 46000 {
            SHIBATA_COEFFS_44100
        } else {
            SHIBATA_COEFFS_48000
        };
        Self {
            dither_type,
            bit_depth: bit_depth.clamp(1, 32),
            shape_left: 0.0,
            shape_right: 0.0,
            rng_state_left: Self::random_seed(),
            rng_state_right: Self::random_seed().wrapping_add(0xDEADBEEF_12345678),
            enabled: dither_type != DitherType::None,
            output_is_float: false,
            hp_prev_left: 0.0,
            hp_prev_right: 0.0,
            shibata_err_left: [0.0; 9],
            shibata_err_right: [0.0; 9],
            shibata_err_pos: 0,
            shibata_coeffs,
        }
    }

    /// Mark the output format as floating-point (f32 or f64).
    ///
    /// When `true`, all dither is unconditionally disabled — no quantization
    /// occurs in a float output path, so adding noise would be harmful.
    pub fn set_output_is_float(&mut self, is_float: bool) {
        self.output_is_float = is_float;
    }

    #[inline]
    fn is_active(&self) -> bool {
        self.enabled
            && !self.output_is_float
            && self.dither_type != DitherType::None
            && self.bit_depth < 32
    }

    /// Compute Shibata noise-shaping error-feedback value for one channel.
    #[inline]
    fn shibata_feedback(&self, err: &[f32; 9], pos: usize) -> f32 {
        let mut sum = 0.0f32;
        for k in 0..9 {
            let idx = (pos + 9 - 1 - k) % 9;
            sum += self.shibata_coeffs[k] * err[idx];
        }
        sum
    }

    fn random_seed() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let instance_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x12345678_9ABCDEF0);
        let seed = ns
            .wrapping_add(instance_id.wrapping_mul(0x9E3779B97F4A7C15))
            .wrapping_mul(0x5851F42D4C957F2D);
        if seed == 0 {
            0x12345678_9ABCDEF0
        } else {
            seed
        }
    }

    #[inline]
    fn next_random(state: &mut u64) -> u64 {
        if *state == 0 {
            *state = 0x12345678_9ABCDEF0;
        }
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[inline]
    fn next_random_f32(state: &mut u64) -> f32 {
        let bits = Self::next_random(state);
        let top24 = (bits >> (64 - 24)) as f32 + 0.5;
        top24 * (1.0 / 8388608.0) - 1.0
    }

    #[inline]
    fn next_random_f64(state: &mut u64) -> f64 {
        let bits = Self::next_random(state);
        let top53 = (bits >> (64 - 53)) as f64 + 0.5;
        top53 * (1.0 / 9007199254740992.0) - 1.0
    }

    /// Process a stereo sample pair with dithering and quantization.
    ///
    /// Returns the dithered and quantized sample pair, clamped to [-1.0, 1.0].
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.is_active() {
            return (left, right);
        }

        let quant_steps = 1u64 << (self.bit_depth - 1);
        let quant_steps_f = quant_steps as f32;
        let half_lsb = 0.5 / quant_steps_f;

        #[allow(deprecated)]
        let (dithered_l, dithered_r) = match self.dither_type {
            DitherType::None => (left, right),

            DitherType::Rectangular => {
                let nl = Self::next_random_f32(&mut self.rng_state_left) * half_lsb;
                let nr = Self::next_random_f32(&mut self.rng_state_right) * half_lsb;
                (left + nl, right + nr)
            }

            DitherType::Triangular => {
                let nl = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let nr = (Self::next_random_f32(&mut self.rng_state_right)
                    + Self::next_random_f32(&mut self.rng_state_right))
                    * half_lsb;
                (left + nl, right + nr)
            }

            DitherType::HighPassTriangular => {
                // HP-TPDF: noise = current_rect - prev_rect (difference of successive rectangulars).
                // Spectral null at DC, bump at Nyquist — lowers audible noise floor.
                let cur_l = Self::next_random_f32(&mut self.rng_state_left);
                let cur_r = Self::next_random_f32(&mut self.rng_state_right);
                let nl = (cur_l - self.hp_prev_left) * half_lsb;
                let nr = (cur_r - self.hp_prev_right) * half_lsb;
                self.hp_prev_left = cur_l;
                self.hp_prev_right = cur_r;
                (left + nl, right + nr)
            }

            DitherType::Shibata => {
                // Shibata F-weighted noise shaping: TPDF + 9-tap IIR error feedback.
                let noise_l = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let noise_r = (Self::next_random_f32(&mut self.rng_state_right)
                    + Self::next_random_f32(&mut self.rng_state_right))
                    * half_lsb;

                let pos = self.shibata_err_pos;
                let feedback_l = self.shibata_feedback(&self.shibata_err_left, pos);
                let feedback_r = self.shibata_feedback(&self.shibata_err_right, pos);

                let shaped_l = left + noise_l - feedback_l;
                let shaped_r = right + noise_r - feedback_r;

                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;

                // Store quantization error for next feedback iteration
                self.shibata_err_left[pos] = q_l - shaped_l;
                self.shibata_err_right[pos] = q_r - shaped_r;
                self.shibata_err_pos = (pos + 1) % 9;

                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }

            DitherType::NoiseShaped => {
                // Retained for backward compatibility only — see deprecation note.
                let nl = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let nr = (Self::next_random_f32(&mut self.rng_state_right)
                    + Self::next_random_f32(&mut self.rng_state_right))
                    * half_lsb;
                let shaped_l = left + nl - self.shape_left * 0.5;
                let shaped_r = right + nr - self.shape_right * 0.5;
                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;
                self.shape_left = q_l - shaped_l + self.shape_left * 0.5;
                self.shape_right = q_r - shaped_r + self.shape_right * 0.5;
                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }
        };

        let ql = (dithered_l * quant_steps_f).round() / quant_steps_f;
        let qr = (dithered_r * quant_steps_f).round() / quant_steps_f;
        (ql.clamp(-1.0, 1.0), qr.clamp(-1.0, 1.0))
    }

    /// Process a stereo f64 sample pair with full 64-bit precision dithering and quantization.
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if !self.is_active() {
            return (left, right);
        }

        let quant_steps = 1u64 << (self.bit_depth - 1);
        let quant_steps_f = quant_steps as f64;
        let half_lsb = 0.5 / quant_steps_f;

        #[allow(deprecated)]
        let (dithered_l, dithered_r) = match self.dither_type {
            DitherType::None => (left, right),

            DitherType::Rectangular => {
                let nl = Self::next_random_f64(&mut self.rng_state_left) * half_lsb;
                let nr = Self::next_random_f64(&mut self.rng_state_right) * half_lsb;
                (left + nl, right + nr)
            }

            DitherType::Triangular => {
                let nl = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let nr = (Self::next_random_f64(&mut self.rng_state_right)
                    + Self::next_random_f64(&mut self.rng_state_right))
                    * half_lsb;
                (left + nl, right + nr)
            }

            DitherType::HighPassTriangular => {
                let cur_l = Self::next_random_f64(&mut self.rng_state_left) as f32;
                let cur_r = Self::next_random_f64(&mut self.rng_state_right) as f32;
                let nl = (cur_l - self.hp_prev_left) as f64 * half_lsb;
                let nr = (cur_r - self.hp_prev_right) as f64 * half_lsb;
                self.hp_prev_left = cur_l;
                self.hp_prev_right = cur_r;
                (left + nl, right + nr)
            }

            DitherType::Shibata => {
                let noise_l = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let noise_r = (Self::next_random_f64(&mut self.rng_state_right)
                    + Self::next_random_f64(&mut self.rng_state_right))
                    * half_lsb;

                let pos = self.shibata_err_pos;
                let feedback_l = self.shibata_feedback(&self.shibata_err_left, pos) as f64;
                let feedback_r = self.shibata_feedback(&self.shibata_err_right, pos) as f64;

                let shaped_l = left + noise_l - feedback_l;
                let shaped_r = right + noise_r - feedback_r;

                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;

                self.shibata_err_left[pos] = (q_l - shaped_l) as f32;
                self.shibata_err_right[pos] = (q_r - shaped_r) as f32;
                self.shibata_err_pos = (pos + 1) % 9;

                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }

            DitherType::NoiseShaped => {
                let nl = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let nr = (Self::next_random_f64(&mut self.rng_state_right)
                    + Self::next_random_f64(&mut self.rng_state_right))
                    * half_lsb;
                let shaped_l = left + nl - (self.shape_left as f64) * 0.5;
                let shaped_r = right + nr - (self.shape_right as f64) * 0.5;
                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;
                self.shape_left = (q_l - shaped_l + (self.shape_left as f64) * 0.5) as f32;
                self.shape_right = (q_r - shaped_r + (self.shape_right as f64) * 0.5) as f32;
                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }
        };

        let ql = (dithered_l * quant_steps_f).round() / quant_steps_f;
        let qr = (dithered_r * quant_steps_f).round() / quant_steps_f;
        (ql.clamp(-1.0, 1.0), qr.clamp(-1.0, 1.0))
    }

    /// Process a single (mono) sample with dithering and quantization.
    #[inline]
    pub fn process_mono(&mut self, sample: f32) -> f32 {
        if !self.is_active() {
            return sample;
        }

        let quant_steps = 1u64 << (self.bit_depth - 1);
        let quant_steps_f = quant_steps as f32;
        let half_lsb = 0.5 / quant_steps_f;

        #[allow(deprecated)]
        match self.dither_type {
            DitherType::None => sample,
            DitherType::Rectangular => {
                let noise = Self::next_random_f32(&mut self.rng_state_left) * half_lsb;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::Triangular => {
                let noise = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::HighPassTriangular => {
                let cur = Self::next_random_f32(&mut self.rng_state_left);
                let noise = (cur - self.hp_prev_left) * half_lsb;
                self.hp_prev_left = cur;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::Shibata => {
                let noise = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let pos = self.shibata_err_pos;
                let feedback = self.shibata_feedback(&self.shibata_err_left, pos);
                let shaped = sample + noise - feedback;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.shibata_err_left[pos] = q - shaped;
                self.shibata_err_pos = (pos + 1) % 9;
                q.clamp(-1.0, 1.0)
            }
            DitherType::NoiseShaped => {
                let noise = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let shaped = sample + noise - self.shape_left * 0.5;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.shape_left = q - shaped + self.shape_left * 0.5;
                q.clamp(-1.0, 1.0)
            }
        }
    }

    /// Process a single (mono) f64 sample with dithering and quantization.
    #[inline]
    pub fn process_mono_f64(&mut self, sample: f64) -> f64 {
        if !self.is_active() {
            return sample;
        }

        let quant_steps = 1u64 << (self.bit_depth - 1);
        let quant_steps_f = quant_steps as f64;
        let half_lsb = 0.5 / quant_steps_f;

        #[allow(deprecated)]
        match self.dither_type {
            DitherType::None => sample,
            DitherType::Rectangular => {
                let noise = Self::next_random_f64(&mut self.rng_state_left) * half_lsb;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::Triangular => {
                let noise = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::HighPassTriangular => {
                let cur = Self::next_random_f64(&mut self.rng_state_left) as f32;
                let noise = (cur - self.hp_prev_left) as f64 * half_lsb;
                self.hp_prev_left = cur;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::Shibata => {
                let noise = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let pos = self.shibata_err_pos;
                let feedback = self.shibata_feedback(&self.shibata_err_left, pos) as f64;
                let shaped = sample + noise - feedback;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.shibata_err_left[pos] = (q - shaped) as f32;
                self.shibata_err_pos = (pos + 1) % 9;
                q.clamp(-1.0, 1.0)
            }
            DitherType::NoiseShaped => {
                let noise = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let shaped = sample + noise - (self.shape_left as f64) * 0.5;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.shape_left = (q - shaped + (self.shape_left as f64) * 0.5) as f32;
                q.clamp(-1.0, 1.0)
            }
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Whether dithering is currently active (enabled, non-float output, bit depth < 32).
    pub fn is_enabled(&self) -> bool {
        self.is_active()
    }

    pub fn bit_depth(&self) -> u32 {
        self.bit_depth
    }
    pub fn dither_type(&self) -> DitherType {
        self.dither_type
    }

    pub fn reset(&mut self) {
        self.shape_left = 0.0;
        self.shape_right = 0.0;
        self.hp_prev_left = 0.0;
        self.hp_prev_right = 0.0;
        self.shibata_err_left = [0.0; 9];
        self.shibata_err_right = [0.0; 9];
        self.shibata_err_pos = 0;
        self.rng_state_left = Self::random_seed();
        self.rng_state_right = Self::random_seed().wrapping_add(0xDEADBEEF_12345678);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dither_output_bounded() {
        let mut dither = Dither::new(DitherType::Triangular, 16);
        for _ in 0..10000 {
            let (l, r) = dither.process(0.5, -0.5);
            assert!(l.abs() <= 1.0);
            assert!(r.abs() <= 1.0);
        }
    }

    #[test]
    fn test_no_dither_at_high_bit_depth() {
        let mut dither = Dither::new(DitherType::Triangular, 32);
        let (l, r) = dither.process(0.5, 0.5);
        assert!((l - 0.5).abs() < 1e-5);
        assert!((r - 0.5).abs() < 1e-5);
    }

    #[test]
    fn test_tpdf_statistics() {
        let mut dither = Dither::new(DitherType::Triangular, 16);
        let n = 100000;
        let mut sum = 0.0;
        for _ in 0..n {
            let (l, _) = dither.process(0.0, 0.0);
            sum += l;
        }
        let mean = sum / n as f32;
        assert!(
            mean.abs() < 0.001,
            "TPDF mean should be near zero, got {}",
            mean
        );
    }
}
