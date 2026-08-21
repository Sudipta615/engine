//! Lookahead brick-wall limiter — Poweramp-class architecture
//!
//! ## Architecture
//!
//! ```text
//! input
//!   │
//!   ├─► peak envelope scanner (lookahead window)
//!   │         │
//!   │         ▼
//!   │   desired gain (from future max peak)
//!   │         │
//!   │         ▼
//!   │   attack/release smoothing
//!   │         │
//!   ├─► delay line (lookahead_samples)
//!   │         │
//!   └─► delayed audio × gain ──► output
//! ```
//!
//! The key improvement over the previous version is that `desired_gain` is
//! computed from the **maximum peak within the lookahead window**, not from the
//! current sample alone.  This means the gain reduction begins before the
//! loudest transient arrives at the output, not after.
//!
//! ## True-Peak Mode
//!
//! When `TruePeakMode::Fir4x` is active, the peak detector oversamples the
//! signal 4× using a polyphase FIR low-pass filter before computing the
//! envelope maximum.  This catches inter-sample peaks that a DAC's
//! reconstruction filter would produce even when no individual PCM sample
//! exceeds the ceiling.
//!
//! ## Limiter vs Saturation
//!
//! [`LimiterMode::Transparent`] is a clean dynamics protector — gain reduction
//! only, no non-linearity added.
//!
//! [`LimiterMode::Saturate`] applies a smooth exponential soft-clip *after*
//! gain reduction.  This is intentional coloration, not limiter behavior.
//! The UI should present these as separate features: **Limiter** and
//! **Clipper/Saturation**.

use crate::buffer::AudioFrame;
use crate::dsp::true_peak::TruePeakMeter;

// The 4× polyphase FIR true-peak detector lives in `crate::dsp::true_peak`
// and is shared with the loudness meter and the offline scanner, so "true
// peak" means the same thing across the whole engine. It is a
// Kaiser-windowed sinc design with <0.01 dB passband ripple and ≥100 dB
// stopband attenuation (see `crate::dsp::true_peak` for the design spec).

// ─────────────────────────────────────────────────────────────────────────────
// Public enums
// ─────────────────────────────────────────────────────────────────────────────

/// Peak detection mode for the limiter's gain-reduction detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TruePeakMode {
    /// Fast path: peak = max(|L|, |R|) per sample.
    /// No oversampling.  Equivalent to the old `true_peak_enabled = false`.
    #[default]
    SamplePeak,

    /// Proper ITU-R BS.1770-class true-peak: 4× FIR oversampling.
    /// Detects inter-sample peaks the DAC reconstruction filter would produce.
    /// Replaces the old, incorrect "4× linear interpolation" mode.
    Fir4x,
}

/// Post-gain mode: controls what happens after the gain-reduction multiplier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LimiterMode {
    /// Pure dynamics protection — gain reduction only.
    /// The output signal is multiplied by the smoothed gain.
    /// Any residual overshoot is hard-clamped at the ceiling.
    /// **This is the recommended mode for transparent mastering.**
    #[default]
    Transparent,

    /// Intentional coloration — applies an exponential soft-clip curve after
    /// gain reduction.  The limiter's brick-wall protection is slightly
    /// sacrificed for a warmer, rounder transient character.
    ///
    /// Formerly: `soft_clip = true`.  Old callers that passed `soft_clip=true`
    /// are automatically mapped to this variant.
    Saturate,
}

// ─────────────────────────────────────────────────────────────────────────────
// LookaheadLimiter
// ─────────────────────────────────────────────────────────────────────────────

/// Lookahead brick-wall limiter with predictive gain envelope.
///
/// The limiter scans a rolling window of future peak values (within the
/// lookahead delay) to compute the required gain *before* the loudest
/// transient arrives at the output.  This eliminates the overshoot that
/// the previous implementation (attack smoothing from current peak) could
/// produce when attack time ≥ lookahead time.
pub struct LookaheadLimiter {
    // ── Configuration ──────────────────────────────────────────────────────
    ceiling_linear: f32,
    attack_secs: f32,
    release_secs: f32,
    lookahead_secs: f32,
    lookahead_samples: usize,
    sample_rate: f32,
    mode: LimiterMode,
    true_peak_mode: TruePeakMode,
    enabled: bool,

    // ── Delay lines (multichannel, native f64) ──────────────────────────
    delay_lines: [Vec<f64>; crate::buffer::MAX_CHANNELS],
    delay_write_pos: usize,

    // ── Monotonic deque for O(1) sliding-window maximum ───────────────────
    /// Maintains (sample_index, peak_value) pairs in strictly decreasing order.
    /// The front element is always the maximum peak within the lookahead window.
    monotonic_deque: std::collections::VecDeque<(usize, f32)>,
    sample_counter: usize,

    // ── Gain smoothing ─────────────────────────────────────────────────────
    current_gain: f32,
    attack_coeff: f32,
    release_coeff: f32,

    // ── FIR state (for TruePeakMode::Fir4x) ──────────────────────────────
    // Shared `TruePeakMeter` implementation (one per channel) — the same
    // detector the loudness meter and the offline scanner use.
    fir_meters: [TruePeakMeter; crate::buffer::MAX_CHANNELS],

    // ── Running peak metrics ──────────────────────────────────────────────
    /// Maximum true-peak observed since last `reset_peak_meters()`.
    max_true_peak: f32,
    /// Maximum sample peak observed since last `reset_peak_meters()`.
    max_sample_peak: f32,
}

impl LookaheadLimiter {
    /// Return the measured DC gains of the 4 polyphase branches.
    /// For a correctly normalized filter, each branch DC gain equals ~1.000000.
    pub fn fir_branch_dc_gains() -> [f64; 4] {
        crate::dsp::true_peak::branch_dc_gains()
    }

    /// Return a reference to the prototype FIR coefficients (f64).
    pub fn fir_prototype_coefficients() -> &'static [f64] {
        crate::dsp::true_peak::prototype_coefficients()
    }

    /// Calculate the theoretical frequency response of the 64-tap prototype FIR filter
    /// at the given frequency (Hz) and sample rate (Hz).
    /// Returns `(magnitude_linear, phase_radians)`.
    pub fn fir_frequency_response(freq_hz: f32, sample_rate: f32) -> (f64, f64) {
        crate::dsp::true_peak::frequency_response(freq_hz, sample_rate)
    }

    /// Create a new limiter with full configuration.
    pub fn new_with_params(
        sample_rate: f32,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool, // backward compat: maps to LimiterMode
    ) -> Self {
        let mode = if soft_clip {
            LimiterMode::Saturate
        } else {
            LimiterMode::Transparent
        };
        Self::new_with_mode(
            sample_rate,
            lookahead_ms,
            attack_ms,
            release_ms,
            ceiling_db,
            mode,
        )
    }

    /// Full constructor with explicit [`LimiterMode`].
    pub fn new_with_mode(
        sample_rate: f32,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        mode: LimiterMode,
    ) -> Self {
        let lookahead_secs = lookahead_ms / 1000.0;
        let lookahead_samples = ((lookahead_secs * sample_rate).ceil() as usize).max(1);
        let attack_secs = attack_ms / 1000.0;
        let release_secs = release_ms / 1000.0;

        let clamped_ceiling_db = if ceiling_db.is_finite() {
            ceiling_db.clamp(-60.0, 0.0)
        } else {
            -0.3
        };
        let ceiling_linear = 10.0_f32.powf(clamped_ceiling_db / 20.0);

        let attack_coeff = if attack_secs > 0.0 {
            (-1.0_f32 / (attack_secs * sample_rate)).exp()
        } else {
            0.0
        };
        let release_coeff = if release_secs > 0.0 {
            (-1.0_f32 / (release_secs * sample_rate)).exp()
        } else {
            0.0
        };

        let buf_len = (lookahead_samples + 1).next_power_of_two();
        // Sizing invariant is lookahead + 2 entries
        let peak_len = lookahead_samples + 2;

        Self {
            ceiling_linear,
            attack_secs,
            release_secs,
            lookahead_secs,
            lookahead_samples,
            sample_rate,
            mode,
            true_peak_mode: TruePeakMode::SamplePeak,
            enabled: true,
            delay_lines: std::array::from_fn(|_| vec![0.0; buf_len]),
            delay_write_pos: 0,
            monotonic_deque: std::collections::VecDeque::with_capacity(peak_len),
            sample_counter: 0,
            current_gain: 1.0,
            attack_coeff,
            release_coeff,
            fir_meters: std::array::from_fn(|_| TruePeakMeter::new()),
            max_true_peak: 0.0,
            max_sample_peak: 0.0,
        }
    }

    /// Create a new limiter with sensible defaults.
    ///
    /// Defaults:
    /// - lookahead = 5 ms
    /// - attack    = 0.5 ms  (near-instant — envelope scan does the heavy lifting)
    /// - release   = 100 ms
    /// - ceiling   = -0.3 dBFS
    pub fn new(sample_rate: f32) -> Self {
        Self::new_with_params(sample_rate, 5.0, 0.5, 100.0, -0.3, false)
    }

    // ── Configuration setters ─────────────────────────────────────────────

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Current ceiling in dBFS (≤ 0).
    pub fn ceiling_db(&self) -> f32 {
        20.0 * self.ceiling_linear.max(1e-9).log10()
    }

    pub fn set_ceiling_db(&mut self, ceiling_db: f32) {
        let db = if ceiling_db.is_finite() {
            ceiling_db.clamp(-60.0, 0.0)
        } else {
            log::warn!("LookaheadLimiter: non-finite ceiling_db; using -0.3");
            -0.3
        };
        self.ceiling_linear = 10.0_f32.powf(db / 20.0);
    }

    /// Alias for backward compatibility.
    pub fn set_threshold_db(&mut self, threshold_db: f32) {
        self.set_ceiling_db(threshold_db);
    }

    pub fn set_attack(&mut self, attack_ms: f32) {
        if !attack_ms.is_finite() || attack_ms <= 0.0 {
            log::warn!(
                "LookaheadLimiter: invalid attack {} ms; clamping to 0.1 ms",
                attack_ms
            );
            self.attack_secs = 0.0001;
        } else {
            self.attack_secs = attack_ms / 1000.0;
        }
        self.attack_coeff = (-1.0_f32 / (self.attack_secs * self.sample_rate)).exp();
    }

    pub fn set_release(&mut self, release_ms: f32) {
        if !release_ms.is_finite() || release_ms <= 0.0 {
            log::warn!(
                "LookaheadLimiter: invalid release {} ms; clamping to 1 ms",
                release_ms
            );
            self.release_secs = 0.001;
        } else {
            self.release_secs = release_ms / 1000.0;
        }
        self.release_coeff = (-1.0_f32 / (self.release_secs * self.sample_rate)).exp();
    }

    pub fn set_lookahead(&mut self, ms: f32) {
        self.lookahead_secs = ms / 1000.0;
        self.rebuild_buffers();
    }

    /// Release time constant in milliseconds (the limiter's tail: how long
    /// gain reduction decays after the signal stops). Exposed for the DSP
    /// graph latency/tail model (spec §19).
    pub fn release_ms(&self) -> f32 {
        self.release_secs * 1000.0
    }

    /// Set the limiter/saturation mode.
    pub fn set_mode(&mut self, mode: LimiterMode) {
        self.mode = mode;
    }

    /// Backward-compat API: maps `true` → `Saturate`, `false` → `Transparent`.
    pub fn set_soft_clip(&mut self, soft_clip: bool) {
        self.mode = if soft_clip {
            LimiterMode::Saturate
        } else {
            LimiterMode::Transparent
        };
    }

    /// Enable or disable the true-peak FIR oversampling detector.
    ///
    /// When `TruePeakMode::Fir4x` is active, the peak detector runs a
    /// 4× polyphase FIR upsampler before computing the envelope maximum.
    /// This gives accurate inter-sample peak detection per ITU-R BS.1770-4.
    ///
    /// **Note:** The old `true_peak_enabled = true` mode used 4× linear
    /// interpolation, which is NOT EBU R128-compliant.  The new FIR mode is.
    pub fn set_true_peak_mode(&mut self, mode: TruePeakMode) {
        if self.true_peak_mode == mode {
            return;
        }
        self.true_peak_mode = mode;
        // The FIR detector's group delay changes the audio delay line length,
        // so rebuild the lookahead buffers (see `audio_delay_samples`).
        self.rebuild_buffers();
    }

    /// Backward-compat: `enable_true_peak(true)` → `Fir4x`, `false` → `SamplePeak`.
    pub fn enable_true_peak(&mut self, enabled: bool) {
        self.set_true_peak_mode(if enabled {
            TruePeakMode::Fir4x
        } else {
            TruePeakMode::SamplePeak
        });
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether true-peak detection is currently active.
    pub fn true_peak_enabled(&self) -> bool {
        self.true_peak_mode == TruePeakMode::Fir4x
    }

    /// The configured predictive lookahead window in samples (the rolling
    /// future-peak scan), excluding the detector's own group delay.
    pub fn lookahead_window_samples(&self) -> usize {
        self.lookahead_samples
    }

    /// The configured predictive lookahead window in milliseconds.
    pub fn lookahead_window_ms(&self) -> f32 {
        self.lookahead_samples as f32 / self.sample_rate.max(1.0) * 1000.0
    }

    /// Total audio delay in samples (lookahead window + FIR detector group
    /// delay when the FIR true-peak detector is active).
    pub fn lookahead_samples(&self) -> usize {
        self.audio_delay_samples()
    }

    /// Total audio delay in milliseconds (0 if disabled).
    pub fn lookahead_ms(&self) -> f32 {
        if self.enabled {
            self.audio_delay_samples() as f32 / self.sample_rate * 1000.0
        } else {
            0.0
        }
    }

    // ── Core processing ───────────────────────────────────────────────────

    /// Process a stereo sample pair through the lookahead limiter (f32).
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.enabled {
            return (left, right);
        }

        let (ol, or_) = self.process_f64(left as f64, right as f64);
        (ol as f32, or_ as f32)
    }

    /// Process a stereo sample pair in native f64 precision.
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if !self.enabled {
            return (left, right);
        }

        let in_s = [left, right];
        let mut out_s = [0.0f64; 2];
        self.process_sample_multichannel(&in_s, &mut out_s);
        (out_s[0], out_s[1])
    }

    /// Process a single multichannel frame in native f64 precision.
    /// Computes the max peak across all input channels, applies identical
    /// lookahead gain reduction to preserve spatial imaging, and writes
    /// delayed, gain-scaled samples to `out_samples`.
    #[inline]
    pub fn process_sample_multichannel(&mut self, in_samples: &[f64], out_samples: &mut [f64]) {
        let ch = in_samples
            .len()
            .min(out_samples.len())
            .min(crate::buffer::MAX_CHANNELS);
        if ch == 0 {
            return;
        }
        if !self.enabled {
            out_samples[..ch].copy_from_slice(&in_samples[..ch]);
            return;
        }

        // ── 1. Sanitize input and compute current multichannel peak ──────
        let mut sample_peak = 0.0f32;
        let mut fir_peak = 0.0f32;
        let mut clean_in = [0.0f64; crate::buffer::MAX_CHANNELS];

        for i in 0..ch {
            let s = if in_samples[i].is_nan() {
                log::error!(
                    "LookaheadLimiter: NaN input on channel {}, substituting 0.0",
                    i
                );
                0.0
            } else {
                in_samples[i]
            };
            clean_in[i] = s;
            sample_peak = sample_peak.max(s.abs() as f32);
            if self.true_peak_mode == TruePeakMode::Fir4x {
                let p = self.fir_meters[i].process_sample(s) as f32;
                fir_peak = fir_peak.max(p);
            }
        }

        let input_peak = match self.true_peak_mode {
            TruePeakMode::SamplePeak => sample_peak,
            TruePeakMode::Fir4x => fir_peak,
        };

        self.max_sample_peak = self.max_sample_peak.max(sample_peak);
        if self.true_peak_mode == TruePeakMode::Fir4x {
            self.max_true_peak = self.max_true_peak.max(input_peak);
        }

        // ── 2. Update Monotonic Deque for O(1) Sliding-Window Maximum ─────
        let cur_idx = self.sample_counter;
        self.sample_counter = self.sample_counter.wrapping_add(1);

        while let Some(&(_, val)) = self.monotonic_deque.back() {
            if val <= input_peak {
                self.monotonic_deque.pop_back();
            } else {
                break;
            }
        }
        if self.monotonic_deque.len() < self.monotonic_deque.capacity() {
            self.monotonic_deque.push_back((cur_idx, input_peak));
        } else {
            log::error!(
                "Limiter monotonic deque capacity invariant violated; resetting peak history"
            );
            self.monotonic_deque.clear();
            self.monotonic_deque.push_back((cur_idx, input_peak));
        }

        let window_start = cur_idx.saturating_sub(self.lookahead_samples);
        while let Some(&(idx, _)) = self.monotonic_deque.front() {
            if idx < window_start {
                self.monotonic_deque.pop_front();
            } else {
                break;
            }
        }

        let future_max_peak = self
            .monotonic_deque
            .front()
            .map(|&(_, v)| v)
            .unwrap_or(input_peak);

        // ── 3. Compute desired gain from future peak ───────────────────────
        let desired_gain = if future_max_peak > self.ceiling_linear {
            self.ceiling_linear / future_max_peak
        } else {
            1.0
        };

        // ── 4. Smooth gain (attack when reducing, release when recovering) ─
        if desired_gain < self.current_gain {
            self.current_gain =
                desired_gain + (self.current_gain - desired_gain) * self.attack_coeff;
        } else {
            self.current_gain =
                desired_gain + (self.current_gain - desired_gain) * self.release_coeff;
        }
        self.current_gain = crate::buffer::flush_denormal(self.current_gain);
        self.current_gain = self.current_gain.clamp(0.0, 1.0);

        // ── 5. Read from delay lines & write new input ────────────────────
        let delay_len = self.delay_lines[0].len();
        let read_pos =
            (self.delay_write_pos + delay_len - self.audio_delay_samples()) & (delay_len - 1);
        let gain_f64 = self.current_gain as f64;
        let c = self.ceiling_linear as f64;

        for i in 0..ch {
            let delayed = self.delay_lines[i][read_pos];
            self.delay_lines[i][self.delay_write_pos] = clean_in[i];
            let mut out = delayed * gain_f64;
            match self.mode {
                LimiterMode::Transparent => {
                    out = out.clamp(-c, c);
                }
                LimiterMode::Saturate => {
                    out = self.soft_clip_sample(out as f32) as f64;
                }
            }
            out_samples[i] = out;
        }
        self.delay_write_pos = (self.delay_write_pos + 1) & (delay_len - 1);
    }

    /// Process a block of stereo frames in place. Hoists the enabled check
    /// out of the per-frame loop; the lookahead/delay state is still
    /// advanced per sample.
    #[inline]
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if !self.enabled {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i] as f64, right[i] as f64);
            left[i] = ol as f32;
            right[i] = or_ as f32;
        }
    }

    /// Process a block of interleaved multichannel frames in place.
    #[inline]
    pub fn process_block_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        if !self.enabled || channels == 0 {
            return;
        }
        let ch = channels.min(crate::buffer::MAX_CHANNELS);
        let n = interleaved.len() / channels;
        let mut in_s = [0.0f64; crate::buffer::MAX_CHANNELS];
        let mut out_s = [0.0f64; crate::buffer::MAX_CHANNELS];

        for i in 0..n {
            let base = i * channels;
            for c in 0..ch {
                in_s[c] = interleaved[base + c] as f64;
            }
            self.process_sample_multichannel(&in_s[..ch], &mut out_s[..ch]);
            for c in 0..ch {
                interleaved[base + c] = out_s[c] as f32;
            }
        }
    }

    /// Process a block of stereo frames in native f64 precision. Hoists the
    /// enabled check out of the per-frame loop.
    #[inline]
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if !self.enabled {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i], right[i]);
            left[i] = ol;
            right[i] = or_;
        }
    }

    /// Process an audio frame (alternative API).
    pub fn process_frame(&mut self, frame: &mut AudioFrame) {
        let ch = (frame.num_channels as usize)
            .min(crate::buffer::MAX_CHANNELS)
            .max(1);
        let mut in_s = [0.0f64; crate::buffer::MAX_CHANNELS];
        for i in 0..ch {
            in_s[i] = frame.channels[i] as f64;
        }
        let mut out_s = [0.0f64; crate::buffer::MAX_CHANNELS];
        self.process_sample_multichannel(&in_s[..ch], &mut out_s[..ch]);
        for i in 0..ch {
            frame.channels[i] = out_s[i] as f32;
        }
    }

    /// Flush the lookahead delay tail.
    ///
    /// The audio path is delayed by [`Self::audio_delay_samples`] input
    /// samples, so after the final real sample has been fed the delay line
    /// still holds that many unemitted samples.  Feeding the same number of
    /// silence samples advances the write pointer far enough to release them
    /// in order.  Returns the emitted tail as stereo pairs.
    pub fn flush(&mut self) -> Vec<(f32, f32)> {
        if !self.enabled {
            return Vec::new();
        }
        let tail = self.audio_delay_samples();
        let mut out = Vec::with_capacity(tail);
        for _ in 0..tail {
            let (l, r) = self.process_f64(0.0, 0.0);
            out.push((l as f32, r as f32));
        }
        out
    }

    /// Flush the lookahead delay tail for multichannel streams.
    pub fn flush_multichannel(&mut self, channels: usize) -> Vec<f32> {
        if !self.enabled || channels == 0 {
            return Vec::new();
        }
        let ch = channels.min(crate::buffer::MAX_CHANNELS);
        let tail = self.audio_delay_samples();
        let mut out = Vec::with_capacity(tail * channels);
        let in_silence = [0.0f64; crate::buffer::MAX_CHANNELS];
        let mut out_s = [0.0f64; crate::buffer::MAX_CHANNELS];
        for _ in 0..tail {
            self.process_sample_multichannel(&in_silence[..ch], &mut out_s[..ch]);
            for c in 0..ch {
                out.push(out_s[c] as f32);
            }
            for _ in ch..channels {
                out.push(0.0);
            }
        }
        out
    }

    // ── Metering ──────────────────────────────────────────────────────────

    /// Gain reduction in dB (always ≤ 0).
    pub fn gain_reduction_db(&self) -> f32 {
        if self.current_gain > 0.0 {
            (20.0 * self.current_gain.log10()).max(-60.0)
        } else {
            -60.0
        }
    }

    /// Current linear gain.
    pub fn current_gain(&self) -> f32 {
        self.current_gain
    }

    /// Maximum true-peak observed since last `reset_peak_meters()`, in dBTP.
    pub fn max_true_peak_dbtp(&self) -> f32 {
        if self.max_true_peak > 0.0 {
            (20.0 * self.max_true_peak.log10()).max(-144.0)
        } else {
            -144.0
        }
    }

    /// Maximum sample peak observed since last `reset_peak_meters()`, in dBFS.
    pub fn max_sample_peak_db(&self) -> f32 {
        if self.max_sample_peak > 0.0 {
            (20.0 * self.max_sample_peak.log10()).max(-144.0)
        } else {
            -144.0
        }
    }

    /// Reset peak meters.
    pub fn reset_peak_meters(&mut self) {
        self.max_true_peak = 0.0;
        self.max_sample_peak = 0.0;
    }

    // ── State management ──────────────────────────────────────────────────

    pub fn reset(&mut self) {
        for line in &mut self.delay_lines {
            line.fill(0.0);
        }
        self.delay_write_pos = 0;
        self.monotonic_deque.clear();
        self.sample_counter = 0;
        self.current_gain = 1.0;
        for meter in &mut self.fir_meters {
            meter.reset();
        }
        self.max_true_peak = 0.0;
        self.max_sample_peak = 0.0;
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.rebuild_buffers();
        self.attack_coeff = (-1.0_f32 / (self.attack_secs * self.sample_rate)).exp();
        self.release_coeff = (-1.0_f32 / (self.release_secs * self.sample_rate)).exp();
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    fn rebuild_buffers(&mut self) {
        self.lookahead_samples = ((self.lookahead_secs * self.sample_rate).ceil() as usize).max(1);
        // The audio delay line must hold the lookahead window plus the FIR
        // detector's own group delay when Fir4x mode is active.
        let buf_len = (self.audio_delay_samples() + 1).next_power_of_two();
        for line in &mut self.delay_lines {
            line.resize(buf_len, 0.0);
            line.fill(0.0);
        }
        self.delay_write_pos = 0;
        self.monotonic_deque.clear();
        self.monotonic_deque.reserve(self.lookahead_samples + 2);
        self.sample_counter = 0;
        self.current_gain = 1.0;
        for meter in &mut self.fir_meters {
            meter.reset();
        }
    }

    /// Group delay of the active detector in input samples: the FIR's own
    /// group delay in Fir4x mode, zero for the sample-peak detector.
    pub fn detector_delay_samples(&self) -> usize {
        match self.true_peak_mode {
            TruePeakMode::SamplePeak => 0,
            TruePeakMode::Fir4x => crate::dsp::true_peak::detector_delay_samples(),
        }
    }

    /// Detector-only group delay in milliseconds (0 for the sample-peak
    /// detector; the FIR's group delay when the true-peak detector is active).
    pub fn detector_delay_ms(&self) -> f32 {
        self.detector_delay_samples() as f32 / self.sample_rate.max(1.0) * 1000.0
    }

    /// Length of the audio delay line in samples. The gain is computed from
    /// a `lookahead_samples`-wide window, but the audio must additionally be
    /// delayed by the detector's group delay so the predictive gain still
    /// runs *ahead* of the transient that produced it.
    fn audio_delay_samples(&self) -> usize {
        self.lookahead_samples + self.detector_delay_samples()
    }

    #[inline]
    fn soft_clip_sample(&self, sample: f32) -> f32 {
        let abs_sample = sample.abs();
        let limit = self.ceiling_linear;
        let threshold = limit * 0.8;
        if abs_sample <= threshold {
            return sample;
        }
        let over = abs_sample - threshold;
        let range = limit - threshold;
        let saturated = threshold + range * (1.0 - (-over / range).exp());
        sample.signum() * saturated
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_limiter_prevents_clipping() {
        let mut limiter = LookaheadLimiter::new(44100.0);
        for _ in 0..1000 {
            let (l, r) = limiter.process(1.5, 1.5);
            assert!(l.abs() <= 1.5);
            assert!(r.abs() <= 1.5);
        }
    }

    #[test]
    fn test_limiter_passes_quiet_signal() {
        let mut limiter = LookaheadLimiter::new(44100.0);
        for _ in 0..1000 {
            let _ = limiter.process(0.1, 0.1);
        }
        let (l, r) = limiter.process(0.1, 0.1);
        assert!(
            (l - 0.1).abs() < 0.05,
            "Quiet signal should pass through: l={}",
            l
        );
        assert!(
            (r - 0.1).abs() < 0.05,
            "Quiet signal should pass through: r={}",
            r
        );
    }

    #[test]
    fn test_limiter_disabled_passthrough() {
        let mut limiter = LookaheadLimiter::new(44100.0);
        limiter.set_enabled(false);
        let (l, r) = limiter.process(0.5, 0.5);
        assert!((l - 0.5).abs() < 1e-5);
        assert!((r - 0.5).abs() < 1e-5);
    }

    #[test]
    fn test_limiter_flush_returns_delayed_tail() {
        let sr = 48000.0f32;
        let lookahead_ms = 5.0f32;
        let lookahead_samples = ((sr * lookahead_ms / 1000.0).ceil() as usize).max(1);

        let mut limiter = LookaheadLimiter::new_with_mode(
            sr,
            lookahead_ms,
            0.5,
            100.0,
            -0.3,
            LimiterMode::Transparent,
        );
        limiter.set_true_peak_mode(TruePeakMode::SamplePeak);

        // Warm up with silence so the impulse lands well inside the window.
        for _ in 0..lookahead_samples {
            limiter.process(0.0, 0.0);
        }
        // A single quiet impulse that is under the ceiling.
        limiter.process(0.5, 0.5);

        let tail = limiter.flush();
        assert_eq!(
            tail.len(),
            lookahead_samples,
            "flush must emit the whole delay-line tail"
        );
        // The impulse is the last real sample fed, so it must be the last
        // sample flushed (everything after it is silence).
        let last = tail[tail.len() - 1];
        assert!(
            (last.0 - 0.5).abs() < 1e-3,
            "impulse should emerge at the end of the flushed tail, got {}",
            last.0
        );
        assert!((last.1 - 0.5).abs() < 1e-3);
    }

    #[test]
    fn test_monotonic_deque_stays_within_preallocated_capacity() {
        let mut limiter = LookaheadLimiter::new(48_000.0);
        let capacity = limiter.monotonic_deque.capacity();
        for i in 0..100_000 {
            let sample = ((i * 17) % 1000) as f32 / 1000.0;
            limiter.process(sample, -sample);
            assert!(limiter.monotonic_deque.len() <= capacity);
        }
        assert_eq!(limiter.monotonic_deque.capacity(), capacity);
    }

    #[test]
    fn test_limiter_reset() {
        let mut limiter = LookaheadLimiter::new(44100.0);
        limiter.set_ceiling_db(-1.0);
        for _ in 0..100 {
            limiter.process(1.0, 1.0);
        }
        limiter.reset();
        assert!((limiter.current_gain() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_transparent_mode_no_soft_clip() {
        // In Transparent mode, output should be hard-clamped at ceiling, not soft-clipped.
        // Soft-clip gives a value slightly BELOW ceiling for very loud inputs;
        // hard-clamp gives exactly ceiling.
        let ceiling_db = -3.0;
        let ceiling_lin = 10.0_f32.powf(ceiling_db / 20.0); // ≈ 0.707
        let mut limiter = LookaheadLimiter::new_with_mode(
            44100.0,
            5.0,
            0.5,
            100.0,
            ceiling_db,
            LimiterMode::Transparent,
        );
        let mut max_out = 0.0_f32;
        for _ in 0..5000 {
            let (l, _) = limiter.process(2.0, 2.0);
            max_out = max_out.max(l.abs());
        }
        assert!(
            max_out <= ceiling_lin + 1e-4,
            "Output exceeded ceiling: {}",
            max_out
        );
        assert!(
            max_out > ceiling_lin * 0.99,
            "Hard clamp should reach ceiling: {}",
            max_out
        );
    }

    #[test]
    fn test_saturate_mode_applies_soft_clip() {
        let ceiling_db = -3.0;
        let ceiling_lin = 10.0_f32.powf(ceiling_db / 20.0);
        let mut limiter = LookaheadLimiter::new_with_mode(
            44100.0,
            5.0,
            0.5,
            100.0,
            ceiling_db,
            LimiterMode::Saturate,
        );
        let mut last_out = 0.0_f32;
        for _ in 0..5000 {
            let (l, _) = limiter.process(2.0, 2.0);
            last_out = l.abs();
        }
        // Soft-clip asymptote is strictly less than ceiling for extreme input
        assert!(
            last_out <= ceiling_lin + 1e-4,
            "Saturate should not exceed ceiling: {}",
            last_out
        );
    }

    #[test]
    fn test_soft_clip_compat_api() {
        let mut a = LookaheadLimiter::new(44100.0);
        a.set_soft_clip(true);
        assert_eq!(a.mode, LimiterMode::Saturate);
        a.set_soft_clip(false);
        assert_eq!(a.mode, LimiterMode::Transparent);
    }

    #[test]
    fn test_true_peak_fir_mode_triggers_earlier() {
        // A near-Nyquist sine with amplitude 0.95 has sample peaks ≤ 0.95,
        // but can have true-peaks exceeding 0.966 (-0.3 dB) due to inter-sample
        // reconstruction.  The FIR true-peak detector should trigger gain
        // reduction where the sample-peak detector would not.
        let sr = 44100.0_f32;
        let freq = 0.45 * (sr / 2.0);
        let amplitude = 0.95;
        let ceiling_db = -0.3; // ≈ 0.966 linear

        let mut sp_limiter = LookaheadLimiter::new(sr);
        sp_limiter.set_ceiling_db(ceiling_db);
        sp_limiter.set_true_peak_mode(TruePeakMode::SamplePeak);

        let mut tp_limiter = LookaheadLimiter::new(sr);
        tp_limiter.set_ceiling_db(ceiling_db);
        tp_limiter.set_true_peak_mode(TruePeakMode::Fir4x);

        let mut sp_min_gain = 1.0_f32;
        let mut tp_min_gain = 1.0_f32;
        for i in 0..10000 {
            let t = i as f32 / sr;
            let s = amplitude * (2.0 * std::f32::consts::PI * freq * t).sin();
            sp_limiter.process(s, s);
            tp_limiter.process(s, s);
            sp_min_gain = sp_min_gain.min(sp_limiter.current_gain());
            tp_min_gain = tp_min_gain.min(tp_limiter.current_gain());
        }
        // The FIR true-peak detector should have more gain reduction
        // (lower min gain) than the sample-peak detector.
        assert!(
            tp_min_gain <= sp_min_gain,
            "FIR true-peak should trigger more gain reduction: tp={} sp={}",
            tp_min_gain,
            sp_min_gain
        );
    }

    #[test]
    fn test_predictive_envelope_no_overshoot() {
        // With the predictive envelope, feeding a single-sample impulse
        // followed by silence should not produce output that exceeds the ceiling.
        let mut limiter = LookaheadLimiter::new(44100.0);
        limiter.set_ceiling_db(-0.3);
        let ceiling = 10.0_f32.powf(-0.3 / 20.0);

        // Warm up
        for _ in 0..500 {
            limiter.process(0.0, 0.0);
        }
        // Single impulse
        limiter.process(2.0, 2.0);
        // Drain lookahead — check that output never exceeds ceiling
        let mut max_out = 0.0_f32;
        for _ in 0..500 {
            let (l, _) = limiter.process(0.0, 0.0);
            max_out = max_out.max(l.abs());
        }
        assert!(
            max_out <= ceiling + 1e-4,
            "Predictive envelope: output exceeded ceiling after impulse; got {}",
            max_out
        );
    }
}
