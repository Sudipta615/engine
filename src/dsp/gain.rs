//! Gain and fade processing — smooth volume control and track fade-in/fade-out

use crate::buffer::AudioFrame;
use crate::dsp::float::AudioFloat;

/// Simple gain/volume control with smooth ramping to avoid zipper noise.
///
/// Generic over `T: AudioFloat` (defaults to `f32`).
#[derive(Debug, Clone, Copy)]
pub struct GainProcessor<T: AudioFloat = f32> {
    pub gain: T,
    pub target_gain: T,
    /// Linear interpolation speed per sample (0.0–1.0)
    pub slew_rate: T,
}

pub type GainProcessorF32 = GainProcessor<f32>;
pub type GainProcessorF64 = GainProcessor<f64>;

impl<T: AudioFloat> GainProcessor<T> {
    pub fn new() -> Self {
        Self {
            gain: T::one(),
            target_gain: T::one(),
            slew_rate: T::from_f64(0.001),
        }
    }

    /// Create with specific initial gain and ramp time
    pub fn with_ramp(initial_gain: f32, ramp_time_ms: f32, sample_rate: f32) -> Self {
        let slew_rate = if ramp_time_ms > 0.0 && sample_rate > 0.0 {
            1.0 / (ramp_time_ms * 0.001 * sample_rate)
        } else {
            1.0
        };
        Self {
            gain: T::from_f32(initial_gain),
            target_gain: T::from_f32(initial_gain),
            slew_rate: T::from_f64(slew_rate as f64),
        }
    }

    /// Set target gain (smooth transition)
    pub fn set_gain(&mut self, gain: f32) {
        self.target_gain = T::from_f32(gain.clamp(0.0, 1.0));
    }

    /// Set target gain directly with precision T
    pub fn set_gain_t(&mut self, gain: T) {
        self.target_gain = gain.clamp(T::zero(), T::one());
    }

    /// Get current gain value as f32
    pub fn current_gain(&self) -> f32 {
        self.gain.to_f32()
    }

    /// Get current gain value as T
    pub fn current_gain_t(&self) -> T {
        self.gain
    }

    /// Set the slew rate — higher values mean faster transitions
    pub fn set_slew_rate(&mut self, rate: f32) {
        self.slew_rate = T::from_f32(rate.clamp(0.0001, 1.0));
    }

    /// Process a frame with smooth gain ramping
    pub fn process_frame(&mut self, frame: &mut AudioFrame) {
        self.gain += (self.target_gain - self.gain) * self.slew_rate;
        if (self.gain - self.target_gain).abs() < T::from_f64(1e-6) {
            self.gain = self.target_gain;
        }
        let g = self.gain.to_f32();
        for i in 0..frame.num_channels as usize {
            frame.channels[i] *= g;
        }
    }

    /// Process a stereo sample pair with smooth gain ramping
    #[inline]
    pub fn process_stereo(&mut self, left: T, right: T) -> (T, T) {
        self.gain += (self.target_gain - self.gain) * self.slew_rate;
        if (self.gain - self.target_gain).abs() < T::from_f64(1e-6) {
            self.gain = self.target_gain;
        }
        (left * self.gain, right * self.gain)
    }

    /// Process a single sample
    #[inline]
    pub fn process_sample(&mut self, sample: T) -> T {
        self.gain += (self.target_gain - self.gain) * self.slew_rate;
        if (self.gain - self.target_gain).abs() < T::from_f64(1e-6) {
            self.gain = self.target_gain;
        }
        sample * self.gain
    }

    /// Process per-channel planar blocks in place with smooth gain ramping.
    #[inline]
    pub fn process_planes(&mut self, planes: &mut [Vec<f32>], channels: usize, frames: usize) {
        let ch = channels.min(planes.len());
        for i in 0..frames {
            self.gain += (self.target_gain - self.gain) * self.slew_rate;
            if (self.gain - self.target_gain).abs() < T::from_f64(1e-6) {
                self.gain = self.target_gain;
            }
            let g = self.gain.to_f32();
            for plane in planes.iter_mut().take(ch) {
                plane[i] *= g;
            }
        }
    }

    /// Process a block of stereo frames in place, advancing the gain ramp
    /// identically to per-frame processing.
    #[inline]
    pub fn process_block_stereo(&mut self, left: &mut [T], right: &mut [T]) {
        let n = left.len().min(right.len());
        for i in 0..n {
            let (l, r) = self.process_stereo(left[i], right[i]);
            left[i] = l;
            right[i] = r;
        }
    }

    /// Immediately snap gain to the target (no ramp)
    pub fn snap(&mut self) {
        self.gain = self.target_gain;
    }

    /// Check if the gain has converged to the target
    pub fn is_settled(&self) -> bool {
        (self.gain - self.target_gain).abs() < T::from_f64(1e-5)
    }

    /// Reset gain to unity
    pub fn reset(&mut self) {
        self.gain = T::one();
        self.target_gain = T::one();
    }
}

impl GainProcessor<f32> {
    /// Process a stereo sample pair in f64 precision with smooth gain ramping.
    /// Advances the exact same slew rate and ramp state as process_stereo.
    #[inline]
    pub fn process_stereo_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        self.gain += (self.target_gain - self.gain) * self.slew_rate;
        if (self.gain - self.target_gain).abs() < 1e-6 {
            self.gain = self.target_gain;
        }
        let g = self.gain as f64;
        (left * g, right * g)
    }

    /// Process a block of stereo frames in f64 precision, advancing the
    /// exact same slew rate and ramp state as per-frame processing.
    #[inline]
    pub fn process_block_stereo_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        let n = left.len().min(right.len());
        for i in 0..n {
            let (l, r) = self.process_stereo_f64(left[i], right[i]);
            left[i] = l;
            right[i] = r;
        }
    }
}

impl<T: AudioFloat> Default for GainProcessor<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Current state of a fade
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FadeState {
    #[default]
    Idle,
    FadingIn,
    FadingOut,
    FadedOut,
}

/// Smooth fade-in/fade-out processor for track transitions and pause/resume
pub struct FadeProcessor {
    gain: f32,
    increment_per_sample: f32,
    pub state: FadeState,
    total_samples: u64,
    samples_processed: u64,
    sample_rate: f32,
    /// Default fade duration in seconds
    default_duration_secs: f32,
}

impl FadeProcessor {
    /// Create a new fade processor with a default fade duration
    pub fn new(fade_time_ms: f32, sample_rate: f32) -> Self {
        Self {
            gain: 1.0,
            increment_per_sample: 0.0,
            state: FadeState::Idle,
            total_samples: 0,
            samples_processed: 0,
            sample_rate,
            default_duration_secs: fade_time_ms / 1000.0,
        }
    }

    /// Begin a fade-in using the default duration
    pub fn fade_in(&mut self) {
        self.fade_in_duration(self.default_duration_secs);
    }

    /// Begin a fade-in over the given duration in seconds
    pub fn fade_in_duration(&mut self, duration_secs: f32) {
        self.total_samples = (duration_secs * self.sample_rate) as u64;
        self.samples_processed = 0;
        self.gain = 0.0;
        self.increment_per_sample = if self.total_samples > 0 {
            1.0 / self.total_samples as f32
        } else {
            1.0
        };
        self.state = FadeState::FadingIn;
    }

    /// Begin a fade-out using the default duration
    pub fn fade_out(&mut self) {
        self.fade_out_duration(self.default_duration_secs);
    }

    /// Begin a fade-out over the given duration in seconds
    pub fn fade_out_duration(&mut self, duration_secs: f32) {
        self.total_samples = (duration_secs * self.sample_rate) as u64;
        self.samples_processed = 0;
        self.gain = 1.0;
        self.increment_per_sample = if self.total_samples > 0 {
            -1.0 / self.total_samples as f32
        } else {
            -1.0
        };
        self.state = FadeState::FadingOut;
    }

    /// Whether the fade-out has completed (output is silent)
    pub fn is_faded_out(&self) -> bool {
        self.state == FadeState::FadedOut
    }

    /// Get the current effective (equal-power cosine) gain curve value.
    /// Cosine S-curve has zero derivative at t=0 and t=1, preventing clicks.
    #[inline]
    pub fn current_curve_gain(&self) -> f32 {
        match self.state {
            FadeState::Idle => self.gain,
            FadeState::FadedOut => 0.0,
            FadeState::FadingIn | FadeState::FadingOut => {
                let t = self.gain.clamp(0.0, 1.0);
                0.5 * (1.0 - (std::f32::consts::PI * t).cos())
            }
        }
    }

    /// Process a stereo sample pair
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.state == FadeState::Idle && (self.gain - 1.0).abs() < 1e-6 {
            return (left, right);
        }
        let g = self.current_curve_gain();
        let out_l = left * g;
        let out_r = right * g;
        self.advance(1);
        (out_l, out_r)
    }

    /// Process a stereo sample pair in native f64 precision
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if self.state == FadeState::Idle && (self.gain - 1.0).abs() < 1e-6 {
            return (left, right);
        }
        let g = self.current_curve_gain() as f64;
        let out_l = left * g;
        let out_r = right * g;
        self.advance(1);
        (out_l, out_r)
    }

    /// Process per-channel planar blocks in place with smooth fade curve progression.
    #[inline]
    pub fn process_planes(&mut self, planes: &mut [Vec<f32>], channels: usize, frames: usize) {
        if self.state == FadeState::Idle && (self.gain - 1.0).abs() < 1e-6 {
            return;
        }
        let ch = channels.min(planes.len());
        for i in 0..frames {
            let g = self.current_curve_gain();
            for plane in planes.iter_mut().take(ch) {
                plane[i] *= g;
            }
            self.advance(1);
            if self.state == FadeState::Idle && (self.gain - 1.0).abs() < 1e-6 {
                break;
            }
        }
    }

    /// Process a block of stereo frames in place. Hoists the idle check out
    /// of the per-frame loop; the fade curve is still advanced per sample.
    ///
    /// Once the fade completes mid-block, the remaining frames pass through
    /// unchanged — mirroring per-frame semantics, where the next call
    /// short-circuits at entry. (Without this, `advance()` would keep
    /// accumulating the raw gain while in the Idle state.)
    #[inline]
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.state == FadeState::Idle && (self.gain - 1.0).abs() < 1e-6 {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let g = self.current_curve_gain();
            left[i] *= g;
            right[i] *= g;
            self.advance(1);
            if self.state == FadeState::Idle && (self.gain - 1.0).abs() < 1e-6 {
                return;
            }
        }
    }

    /// Process a block of stereo frames in native f64 precision. Hoists the
    /// idle check out of the per-frame loop. See [`Self::process_block`].
    #[inline]
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.state == FadeState::Idle && (self.gain - 1.0).abs() < 1e-6 {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let g = self.current_curve_gain() as f64;
            left[i] *= g;
            right[i] *= g;
            self.advance(1);
            if self.state == FadeState::Idle && (self.gain - 1.0).abs() < 1e-6 {
                return;
            }
        }
    }

    /// Process an audio frame
    pub fn process_frame(&mut self, frame: &mut AudioFrame) {
        if self.state == FadeState::Idle && (self.gain - 1.0).abs() < 1e-6 {
            return;
        }
        let g = self.current_curve_gain();
        for i in 0..frame.num_channels as usize {
            frame.channels[i] *= g;
        }
        self.advance(1);
    }

    /// Get the current gain value
    pub fn gain(&self) -> f32 {
        self.current_curve_gain()
    }

    fn advance(&mut self, n: u64) {
        self.samples_processed += n;

        // Compute gain from the exact position ratio using f64 arithmetic
        // instead of accumulating an f32 increment per sample. This
        // eliminates floating-point drift for very long fades (millions of
        // samples) where repeated f32 addition would accumulate error.
        if self.total_samples > 0 {
            let progress =
                (self.samples_processed as f64 / self.total_samples as f64).clamp(0.0, 1.0);
            self.gain = match self.state {
                FadeState::FadingIn => progress as f32,
                FadeState::FadingOut => (1.0 - progress) as f32,
                _ => self.gain,
            };
        } else {
            // Fallback for zero-length fades (e.g. cancel with total_samples = 0).
            self.gain += self.increment_per_sample * n as f32;
        }

        match self.state {
            FadeState::FadingIn
                if self.gain >= 1.0 || self.samples_processed >= self.total_samples =>
            {
                self.gain = 1.0;
                self.state = FadeState::Idle;
            }
            FadeState::FadingOut
                if self.gain <= 0.0 || self.samples_processed >= self.total_samples =>
            {
                self.gain = 0.0;
                self.state = FadeState::FadedOut;
            }
            _ => {}
        }
    }

    pub fn cancel(&mut self, gain: f32) {
        self.gain = gain.clamp(0.0, 1.0);
        self.increment_per_sample = 0.0;
        self.total_samples = 0; // prevent advance() from snapping
        self.samples_processed = 0;
        self.state = FadeState::Idle; // hold at cancel gain
    }

    pub fn reset(&mut self) {
        self.gain = 1.0;
        self.increment_per_sample = 0.0;
        self.state = FadeState::Idle;
        self.total_samples = 0;
        self.samples_processed = 0;
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gain_processor_smooth() {
        let mut gp = GainProcessor::<f32>::new();
        gp.set_slew_rate(0.5);
        gp.set_gain(0.5);
        let (l, _r) = gp.process_stereo(1.0, 1.0);
        assert!(l < 1.0 && l > 0.5);
    }

    #[test]
    fn test_gain_processor_f64() {
        let mut gp = GainProcessor::<f64>::new();
        gp.set_slew_rate(0.5);
        gp.set_gain(0.5);
        let (l, _r) = gp.process_stereo(1.0, 1.0);
        assert!(l < 1.0 && l > 0.5);
    }

    #[test]
    fn test_gain_processor_snap() {
        let mut gp = GainProcessor::<f32>::new();
        gp.set_gain(0.0);
        gp.snap();
        assert!(gp.is_settled());
    }

    #[test]
    fn test_gain_with_ramp() {
        let gp = GainProcessor::<f32>::with_ramp(0.5, 10.0, 44100.0);
        assert!((gp.gain - 0.5).abs() < 1e-5);
        assert!((gp.current_gain() - 0.5).abs() < 1e-5);
    }

    #[test]
    fn test_fade_in_completes() {
        let mut fade = FadeProcessor::new(1.0, 44100.0);
        fade.fade_in();
        assert_eq!(fade.state, FadeState::FadingIn);
        for _ in 0..50000 {
            fade.process(1.0, 1.0);
        }
        assert_eq!(fade.state, FadeState::Idle);
    }

    #[test]
    fn test_fade_out_completes() {
        let mut fade = FadeProcessor::new(1.0, 44100.0);
        fade.fade_out();
        for _ in 0..50000 {
            fade.process(1.0, 1.0);
        }
        assert!(fade.is_faded_out());
    }

    #[test]
    fn test_fade_no_click() {
        let mut fade = FadeProcessor::new(10.0, 44100.0);
        fade.fade_out();
        let mut prev = 1.0;
        for _ in 0..5000 {
            let (l, _r) = fade.process(1.0, 1.0);
            let delta = (l - prev).abs();
            assert!(delta < 0.05, "Fade should be smooth, delta={}", delta);
            prev = l;
        }
    }

    #[test]
    fn test_fade_exact_ratio_no_drift() {
        // 10-second fade at 192 kHz (1,920,000 samples)
        let sample_rate = 192_000.0;
        let mut fade = FadeProcessor::new(10_000.0, sample_rate);
        fade.fade_in();
        // At half-way point, progress and curve gain should be exact
        for _ in 0..960_000 {
            fade.process(1.0, 1.0);
        }
        assert!((fade.gain() - 0.5).abs() < 1e-4);
        // Process rest
        for _ in 0..960_000 {
            fade.process(1.0, 1.0);
        }
        assert_eq!(fade.state, FadeState::Idle);
        assert!((fade.gain() - 1.0).abs() < 1e-6);
    }
}
