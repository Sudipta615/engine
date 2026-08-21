//! Crossfade and gapless playback mixer
//!
//! Handles sample-accurate transitions between tracks with configurable
//! crossfade curves. Supports three curve types:
//! - **Linear**: simple linear interpolation (causes volume dip at center)
//! - **Equal-power**: cosine/sine crossfade that preserves perceived loudness
//! - **S-curve**: smoothstep interpolation for the smoothest transition
//!
//! Gapless playback is handled separately via a simpler mechanism that
//! simply switches to the next track's samples at the boundary.

/// Crossfade curve shape
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CrossfadeCurve {
    /// Linear interpolation (causes ~3 dB dip at center)
    Linear,
    /// Equal-power: cos/sin crossfade (preserves perceived loudness)
    EqualPower,
    /// Exponential curve (steeper drop-off, slower rise)
    Exponential,
    /// Logarithmic curve (slower drop-off, steeper rise)
    Logarithmic,
    /// S-curve: smoothstep for the most natural transition
    SCurve,
}

impl From<config::CrossfadeCurve> for CrossfadeCurve {
    fn from(c: config::CrossfadeCurve) -> Self {
        match c {
            config::CrossfadeCurve::ConstantPower => CrossfadeCurve::EqualPower,
            config::CrossfadeCurve::Linear => CrossfadeCurve::Linear,
            config::CrossfadeCurve::Exponential => CrossfadeCurve::Exponential,
            config::CrossfadeCurve::Logarithmic => CrossfadeCurve::Logarithmic,
            config::CrossfadeCurve::SCurve => CrossfadeCurve::SCurve,
        }
    }
}

/// State of the crossfade mixer
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MixerState {
    /// Playing current track only
    PlayingCurrent,
    /// Crossfading from current to next
    Crossfading,
    /// Sequential fade transition: fade-out current → silence gap → fade-in
    /// next (the two tracks are never heard simultaneously).
    Fading,
    /// Playing next track only (after crossfade completes)
    PlayingNext,
    /// No audio (stopped)
    Silent,
}

/// Fade-envelope phase boundaries as fractions of the total transition
/// duration: fade-out over [0, 1/3), silence gap over [1/3, 2/3), fade-in
/// over [2/3, 1]. This matches the documented `TransitionMode::Fade`
/// semantics (fade-out current track → silence gap → fade-in next) and the
/// engine's fade-in gating in `decode_transitioning_stream`, which keeps the
/// next track's head aligned with its own sample 0 at the fade-in.
pub const FADE_OUT_FRACTION: f32 = 1.0 / 3.0;
pub const FADE_GAP_FRACTION: f32 = 2.0 / 3.0;

/// Crossfade configuration
///
/// **Note:** This is the runtime, DSP-internal representation of crossfade
/// settings (duration in frames, curve shape, smart-boundaries flag). It is
/// distinct from `config::CrossfadeConfig`, which is the user-facing,
/// serializable config type (duration in ms, curve enum, etc.). The
/// `config` type is converted to this type at pipeline construction time.
#[derive(Debug, Clone, Copy)]
pub struct CrossfadeConfig {
    /// Duration of the crossfade in frames
    pub duration_frames: usize,
    /// Crossfade curve shape
    pub curve: CrossfadeCurve,
    /// Whether to detect silence boundaries for smarter transitions
    pub smart_boundaries: bool,
}

impl Default for CrossfadeConfig {
    fn default() -> Self {
        Self {
            duration_frames: 88200, // 2 seconds at 44100 Hz
            curve: CrossfadeCurve::EqualPower,
            smart_boundaries: true,
        }
    }
}

/// The crossfade/gapless mixer
///
/// Manages transitions between tracks by crossfading the outgoing track's
/// tail with the incoming track's head. The outgoing tail must be provided
/// in advance (pre-read) for gapless/crossfade operation.
#[derive(Debug, Clone)]
pub struct TrackMixer {
    state: MixerState,
    /// Current position in the crossfade (0 to duration_frames)
    crossfade_pos: usize,
    config: CrossfadeConfig,
    /// Whether crossfade is enabled (when disabled, transitions are gapless only)
    enabled: bool,
    /// Buffer for outgoing track tail (pre-read for gapless/crossfade)
    outgoing_buffer_left: Vec<f32>,
    outgoing_buffer_right: Vec<f32>,
    /// Read position in outgoing buffer
    outgoing_pos: usize,
}

impl TrackMixer {
    /// Create a new mixer with a default 2-second crossfade
    pub fn new(sample_rate: f32) -> Self {
        let default_duration = (2.0 * sample_rate) as usize;
        let pre_cap = default_duration.max(1024);
        Self {
            state: MixerState::Silent,
            crossfade_pos: 0,
            config: CrossfadeConfig {
                duration_frames: default_duration,
                curve: CrossfadeCurve::EqualPower,
                smart_boundaries: true,
            },
            enabled: true,
            outgoing_buffer_left: Vec::with_capacity(pre_cap),
            outgoing_buffer_right: Vec::with_capacity(pre_cap),
            outgoing_pos: 0,
        }
    }

    /// Get the crossfade duration in frames
    pub fn duration_frames(&self) -> usize {
        self.config.duration_frames
    }

    /// Get the crossfade duration in milliseconds (rounded)
    pub fn duration_ms(&self, sample_rate: f32) -> u64 {
        if sample_rate > 0.0 {
            (self.config.duration_frames as f32 / sample_rate * 1000.0).round() as u64
        } else {
            0
        }
    }

    /// Set crossfade duration in milliseconds
    pub fn set_duration_ms(&mut self, duration_ms: u64, sample_rate: f32) {
        if sample_rate <= 0.0 || !sample_rate.is_finite() {
            log::warn!(
                "TrackMixer::set_duration_ms: invalid sample_rate {}",
                sample_rate
            );
            return;
        }
        let frames = (duration_ms as f32 * 0.001 * sample_rate) as usize;
        // Clamp to a minimum of 1 so the crossfade cannot complete instantly.
        self.config.duration_frames = frames.max(1);
    }

    /// Enable or disable the crossfade mixer.
    /// When disabled, the mixer bypasses crossfade and transitions are gapless.
    /// When enabled, the mixer will crossfade between tracks using the
    /// configured curve and duration.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            // If currently crossfading or fading, immediately jump to next track
            if self.state == MixerState::Crossfading || self.state == MixerState::Fading {
                self.state = MixerState::PlayingNext;
                self.outgoing_buffer_left.clear();
                self.outgoing_buffer_right.clear();
                self.outgoing_pos = 0;
            }
        }
    }

    /// Whether the crossfade mixer is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the mixer is currently in the Crossfading state (High #H5).
    pub fn is_crossfading(&self) -> bool {
        self.state == MixerState::Crossfading
    }

    /// Set crossfade curve type
    pub fn set_curve(&mut self, curve: CrossfadeCurve) {
        self.config.curve = curve;
    }

    /// Rescale an in-progress envelope when the output device changes sample
    /// rate. Preserve the normalized progress rather than restarting the
    /// transition or leaving its frame counter in the old time base.
    pub fn rescale_sample_rate(&mut self, old_rate: f32, new_rate: f32) {
        if !old_rate.is_finite()
            || !new_rate.is_finite()
            || old_rate <= 0.0
            || new_rate <= 0.0
            || (old_rate - new_rate).abs() < f32::EPSILON
        {
            return;
        }
        let old_duration = self.config.duration_frames.max(1);
        let progress = self.crossfade_pos as f64 / old_duration as f64;
        let new_duration =
            ((old_duration as f64 * new_rate as f64 / old_rate as f64).round() as usize).max(1);
        self.config.duration_frames = new_duration;
        self.crossfade_pos = (progress * new_duration as f64)
            .round()
            .min(new_duration as f64) as usize;
    }

    /// Start a crossfade to the next track.
    /// If crossfade is disabled, this immediately switches to PlayingNext
    /// for a gapless transition instead of a crossfade.
    pub fn start_crossfade(&mut self) {
        if self.enabled {
            self.state = MixerState::Crossfading;
            self.crossfade_pos = 0;
        } else {
            // Crossfade disabled: gapless transition
            self.state = MixerState::PlayingNext;
            self.outgoing_buffer_left.clear();
            self.outgoing_buffer_right.clear();
            self.outgoing_pos = 0;
        }
    }

    /// Start a sequential fade transition (fade-out → silence gap → fade-in).
    ///
    /// Unlike [`Self::start_crossfade`], the two tracks are never mixed at
    /// the same time: the current track fades to silence, a silence gap
    /// plays, and only then does the next track fade in. Not gated by the
    /// mixer's `enabled` flag — `TransitionMode::Fade` is an explicit user
    /// choice and must always produce a fade regardless of the crossfade
    /// feature toggle.
    pub fn start_fade(&mut self) {
        self.state = MixerState::Fading;
        self.crossfade_pos = 0;
    }

    /// Signal that the current track has started playing
    pub fn start_playing(&mut self) {
        self.state = MixerState::PlayingCurrent;
    }

    /// Set the outgoing track's tail samples (for gapless/crossfade)
    pub fn set_outgoing_tail(&mut self, mut left: Vec<f32>, mut right: Vec<f32>) {
        let min_len = left.len().min(right.len());
        left.truncate(min_len);
        right.truncate(min_len);
        self.outgoing_buffer_left = left;
        self.outgoing_buffer_right = right;
        self.outgoing_pos = 0;
    }

    /// Compute crossfade gains for a given normalized position and curve type.
    ///
    /// Returns `(outgoing_gain, incoming_gain)`.
    /// At `t=0.0`, outgoing is full and incoming is silent.
    /// At `t=1.0`, outgoing is silent and incoming is full.
    #[inline]
    pub fn compute_gains_for_curve(t: f32, curve: CrossfadeCurve) -> (f32, f32) {
        let t = t.clamp(0.0, 1.0);
        match curve {
            CrossfadeCurve::Linear => (1.0 - t, t),
            CrossfadeCurve::EqualPower => {
                // Equal-power crossfade: preserves perceived loudness
                // because cos²(θ) + sin²(θ) = 1
                let cos_t = (std::f32::consts::FRAC_PI_2 * t).cos();
                let sin_t = (std::f32::consts::FRAC_PI_2 * t).sin();
                (cos_t, sin_t)
            }
            CrossfadeCurve::Exponential => {
                let out_gain = (1.0 - t).powf(2.0);
                let in_gain = t.powf(2.0);
                (out_gain, in_gain)
            }
            CrossfadeCurve::Logarithmic => {
                let out_gain = (1.0 - t).sqrt();
                let in_gain = t.sqrt();
                (out_gain, in_gain)
            }
            CrossfadeCurve::SCurve => {
                // S-curve: smoothstep for the most natural transition
                let s = t * t * (3.0 - 2.0 * t);
                (1.0 - s, s)
            }
        }
    }

    /// Compute crossfade gains using the currently configured curve.
    #[inline]
    fn compute_gains(&self, t: f32) -> (f32, f32) {
        Self::compute_gains_for_curve(t, self.config.curve)
    }

    /// Gains for the sequential fade transition at normalized position `t`
    /// (0.0..=1.0): `(outgoing_gain, incoming_gain)`.
    ///
    /// - fade-out phase [0, 1/3): outgoing 1→0, incoming silent;
    /// - gap phase [1/3, 2/3): both silent;
    /// - fade-in phase [2/3, 1]: outgoing silent, incoming 0→1.
    ///
    /// The configured curve shapes each ramp (the same curve family as the
    /// crossfade paths).
    #[inline]
    pub fn compute_fade_gains(t: f32, curve: CrossfadeCurve) -> (f32, f32) {
        let t = t.clamp(0.0, 1.0);
        if t < FADE_OUT_FRACTION {
            // Fade-out: the current track ramps down 1 → 0.
            let t0 = (t / FADE_OUT_FRACTION).clamp(0.0, 1.0);
            let (og, _) = Self::compute_gains_for_curve(t0, curve);
            (og, 0.0)
        } else if t < FADE_GAP_FRACTION {
            // Silence gap: both tracks muted.
            (0.0, 0.0)
        } else {
            // Fade-in: the next track ramps up 0 → 1.
            let t2 = ((t - FADE_GAP_FRACTION) / (1.0 - FADE_GAP_FRACTION)).clamp(0.0, 1.0);
            let (_, ig) = Self::compute_gains_for_curve(t2, curve);
            (0.0, ig)
        }
    }

    /// Mix a stereo sample from current and next tracks during crossfade.
    ///
    /// # Arguments
    /// * `current_left` / `current_right` - Samples from the current (outgoing) track
    /// * `next_left` / `next_right` - Samples from the next (incoming) track
    ///
    /// # Returns
    /// The mixed stereo sample
    #[inline]
    pub fn process(
        &mut self,
        current_left: f32,
        current_right: f32,
        next_left: f32,
        next_right: f32,
    ) -> (f32, f32) {
        match self.state {
            MixerState::PlayingCurrent => (current_left, current_right),
            MixerState::PlayingNext => (next_left, next_right),
            MixerState::Silent => (0.0, 0.0),
            MixerState::Crossfading => {
                let t = if self.config.duration_frames > 0 {
                    self.crossfade_pos as f32 / self.config.duration_frames as f32
                } else {
                    1.0
                };

                let (out_gain, in_gain) = self.compute_gains(t);

                let (out_l, out_r) = if self.outgoing_pos < self.outgoing_buffer_left.len()
                    && self.outgoing_pos < self.outgoing_buffer_right.len()
                {
                    let l = self.outgoing_buffer_left[self.outgoing_pos];
                    let r = self.outgoing_buffer_right[self.outgoing_pos];
                    self.outgoing_pos += 1;
                    (l, r)
                } else {
                    // No pre-buffered tail — use the live outgoing samples
                    // fed in by the engine.
                    (current_left, current_right)
                };

                let mixed_l = out_l * out_gain + next_left * in_gain;
                let mixed_r = out_r * out_gain + next_right * in_gain;

                self.crossfade_pos += 1;
                if self.crossfade_pos >= self.config.duration_frames {
                    self.state = MixerState::PlayingNext;
                    self.outgoing_buffer_left.clear();
                    self.outgoing_buffer_right.clear();
                    self.outgoing_pos = 0;
                }

                (mixed_l, mixed_r)
            }
            MixerState::Fading => {
                let t = if self.config.duration_frames > 0 {
                    self.crossfade_pos as f32 / self.config.duration_frames as f32
                } else {
                    1.0
                };

                let (out_gain, in_gain) = Self::compute_fade_gains(t, self.config.curve);

                let (out_l, out_r) = if self.outgoing_pos < self.outgoing_buffer_left.len()
                    && self.outgoing_pos < self.outgoing_buffer_right.len()
                {
                    let l = self.outgoing_buffer_left[self.outgoing_pos];
                    let r = self.outgoing_buffer_right[self.outgoing_pos];
                    self.outgoing_pos += 1;
                    (l, r)
                } else {
                    // No pre-buffered tail — use the live outgoing samples
                    // fed in by the engine.
                    (current_left, current_right)
                };

                let mixed_l = out_l * out_gain + next_left * in_gain;
                let mixed_r = out_r * out_gain + next_right * in_gain;

                self.crossfade_pos += 1;
                if self.crossfade_pos >= self.config.duration_frames {
                    self.state = MixerState::PlayingNext;
                    self.outgoing_buffer_left.clear();
                    self.outgoing_buffer_right.clear();
                    self.outgoing_pos = 0;
                }

                (mixed_l, mixed_r)
            }
        }
    }

    /// Mix a stereo sample pair via full 64-bit float precision.
    #[inline]
    pub fn process_f64(
        &mut self,
        current_left: f64,
        current_right: f64,
        next_left: f64,
        next_right: f64,
    ) -> (f64, f64) {
        match self.state {
            MixerState::PlayingCurrent => (current_left, current_right),
            MixerState::PlayingNext => (next_left, next_right),
            MixerState::Silent => (0.0, 0.0),
            MixerState::Crossfading => {
                let t = if self.config.duration_frames > 0 {
                    self.crossfade_pos as f64 / self.config.duration_frames as f64
                } else {
                    1.0
                };

                let (out_g, in_g) = Self::compute_gains_for_curve(t as f32, self.config.curve);
                let out_gain = out_g as f64;
                let in_gain = in_g as f64;

                let (out_l, out_r) = if self.outgoing_pos < self.outgoing_buffer_left.len()
                    && self.outgoing_pos < self.outgoing_buffer_right.len()
                {
                    let l = self.outgoing_buffer_left[self.outgoing_pos] as f64;
                    let r = self.outgoing_buffer_right[self.outgoing_pos] as f64;
                    self.outgoing_pos += 1;
                    (l, r)
                } else {
                    (current_left, current_right)
                };

                let mixed_l = out_l * out_gain + next_left * in_gain;
                let mixed_r = out_r * out_gain + next_right * in_gain;

                self.crossfade_pos += 1;
                if self.crossfade_pos >= self.config.duration_frames {
                    self.state = MixerState::PlayingNext;
                    self.outgoing_buffer_left.clear();
                    self.outgoing_buffer_right.clear();
                    self.outgoing_pos = 0;
                }

                (mixed_l, mixed_r)
            }
            MixerState::Fading => {
                let t = if self.config.duration_frames > 0 {
                    self.crossfade_pos as f64 / self.config.duration_frames as f64
                } else {
                    1.0
                };

                let (out_g, in_g) = Self::compute_fade_gains(t as f32, self.config.curve);
                let out_gain = out_g as f64;
                let in_gain = in_g as f64;

                let (out_l, out_r) = if self.outgoing_pos < self.outgoing_buffer_left.len()
                    && self.outgoing_pos < self.outgoing_buffer_right.len()
                {
                    let l = self.outgoing_buffer_left[self.outgoing_pos] as f64;
                    let r = self.outgoing_buffer_right[self.outgoing_pos] as f64;
                    self.outgoing_pos += 1;
                    (l, r)
                } else {
                    (current_left, current_right)
                };

                let mixed_l = out_l * out_gain + next_left * in_gain;
                let mixed_r = out_r * out_gain + next_right * in_gain;

                self.crossfade_pos += 1;
                if self.crossfade_pos >= self.config.duration_frames {
                    self.state = MixerState::PlayingNext;
                    self.outgoing_buffer_left.clear();
                    self.outgoing_buffer_right.clear();
                    self.outgoing_pos = 0;
                }

                (mixed_l, mixed_r)
            }
        }
    }

    /// Process for gapless playback (no crossfade, seamless transition).
    ///
    /// When `next_available` is true, the next track's samples are used
    /// directly, enabling sample-accurate gapless transitions.
    #[inline]
    pub fn process_gapless(
        &mut self,
        current_left: f32,
        current_right: f32,
        next_available: bool,
        next_left: f32,
        next_right: f32,
    ) -> (f32, f32) {
        if next_available {
            (next_left, next_right)
        } else {
            (current_left, current_right)
        }
    }

    /// Mix a block of stereo frames from the current (outgoing) and next
    /// (incoming) tracks. The mixed result is written back into the current
    /// buffers. Hoists the mixer-state dispatch out of the per-frame loop;
    /// a mid-block state transition (crossfade completion) is still handled
    /// correctly because the per-frame `process` is used in the Crossfading
    /// state.
    #[inline]
    pub fn process_block(
        &mut self,
        current_left: &mut [f32],
        current_right: &mut [f32],
        next_left: &[f32],
        next_right: &[f32],
    ) {
        let n = current_left
            .len()
            .min(current_right.len())
            .min(next_left.len())
            .min(next_right.len());
        match self.state {
            MixerState::PlayingCurrent => {}
            MixerState::PlayingNext => {
                current_left[..n].copy_from_slice(&next_left[..n]);
                current_right[..n].copy_from_slice(&next_right[..n]);
            }
            MixerState::Silent => {
                current_left[..n].fill(0.0);
                current_right[..n].fill(0.0);
            }
            MixerState::Crossfading | MixerState::Fading => {
                // Both are frame-stateful envelopes, so the per-frame
                // `process` must be used (a mid-block phase transition is
                // still handled correctly).
                for i in 0..n {
                    let (l, r) = self.process(
                        current_left[i],
                        current_right[i],
                        next_left[i],
                        next_right[i],
                    );
                    current_left[i] = l;
                    current_right[i] = r;
                }
            }
        }
    }

    /// Mix a block of stereo frames in native f64 precision. The mixed result
    /// is written back into the current buffers. See [`Self::process_block`].
    #[inline]
    pub fn process_block_f64(
        &mut self,
        current_left: &mut [f64],
        current_right: &mut [f64],
        next_left: &[f64],
        next_right: &[f64],
    ) {
        let n = current_left
            .len()
            .min(current_right.len())
            .min(next_left.len())
            .min(next_right.len());
        match self.state {
            MixerState::PlayingCurrent => {}
            MixerState::PlayingNext => {
                current_left[..n].copy_from_slice(&next_left[..n]);
                current_right[..n].copy_from_slice(&next_right[..n]);
            }
            MixerState::Silent => {
                current_left[..n].fill(0.0);
                current_right[..n].fill(0.0);
            }
            MixerState::Crossfading | MixerState::Fading => {
                // Both are frame-stateful envelopes, so the per-frame
                // `process_f64` must be used.
                for i in 0..n {
                    let (l, r) = self.process_f64(
                        current_left[i],
                        current_right[i],
                        next_left[i],
                        next_right[i],
                    );
                    current_left[i] = l;
                    current_right[i] = r;
                }
            }
        }
    }

    /// Get the current mixer state
    pub fn state(&self) -> MixerState {
        self.state
    }

    /// Current envelope position in frames, exposed for recovery diagnostics.
    #[cfg(test)]
    pub fn crossfade_position_frames(&self) -> usize {
        self.crossfade_pos
    }

    /// Get the crossfade progress (0.0 to 1.0) if crossfading
    pub fn crossfade_progress(&self) -> Option<f32> {
        if self.state == MixerState::Crossfading && self.config.duration_frames > 0 {
            Some(self.crossfade_pos as f32 / self.config.duration_frames as f32)
        } else {
            None
        }
    }

    /// Reset all mixer state
    pub fn reset(&mut self) {
        self.state = MixerState::Silent;
        self.crossfade_pos = 0;
        self.outgoing_buffer_left.clear();
        self.outgoing_buffer_right.clear();
        self.outgoing_pos = 0;
        // Note: `enabled` and config are NOT reset — they are persistent settings
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_equal_power_crossfade_preserves_energy() {
        // At midpoint (t=0.5), equal power should preserve energy
        let (out_gain, in_gain) =
            TrackMixer::compute_gains_for_curve(0.5, CrossfadeCurve::EqualPower);
        let energy = out_gain * out_gain + in_gain * in_gain;
        assert!(
            (energy - 1.0).abs() < 0.01,
            "Equal power should preserve energy at midpoint, got {}",
            energy
        );
    }

    #[test]
    fn test_linear_crossfade_dips_at_center() {
        let (out_gain, in_gain) = TrackMixer::compute_gains_for_curve(0.5, CrossfadeCurve::Linear);
        // Linear at center: both 0.5
        assert!((out_gain - 0.5).abs() < 0.01);
        assert!((in_gain - 0.5).abs() < 0.01);
        // Perceived energy dips: 0.5² + 0.5² = 0.5 < 1.0
        let energy = out_gain * out_gain + in_gain * in_gain;
        assert!(energy < 0.7, "Linear crossfade should dip at center");
    }

    #[test]
    fn test_s_curve_monotonic() {
        let mut prev_in = 0.0;
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let (_, in_gain) = TrackMixer::compute_gains_for_curve(t, CrossfadeCurve::SCurve);
            assert!(
                in_gain >= prev_in - 1e-5,
                "S-curve should be monotonically increasing"
            );
            prev_in = in_gain;
        }
    }

    #[test]
    fn test_rescale_sample_rate_preserves_crossfade_progress() {
        let mut mixer = TrackMixer::new(48_000.0);
        mixer.set_duration_ms(1_000, 48_000.0);
        mixer.start_crossfade();
        for _ in 0..12_000 {
            mixer.process(0.0, 0.0, 0.0, 0.0);
        }
        let before = mixer.crossfade_progress().expect("active crossfade");
        mixer.rescale_sample_rate(48_000.0, 96_000.0);
        let after = mixer.crossfade_progress().expect("active crossfade");
        assert!((before - after).abs() < 1e-5);
        assert_eq!(mixer.duration_frames(), 96_000);
    }

    #[test]
    fn test_rescale_sample_rate_ignores_invalid_rates() {
        let mut mixer = TrackMixer::new(48_000.0);
        mixer.set_duration_ms(1_000, 48_000.0);
        let duration = mixer.duration_frames();
        mixer.rescale_sample_rate(0.0, 96_000.0);
        assert_eq!(mixer.duration_frames(), duration);
    }

    #[test]
    fn test_crossfade_completion() {
        let mut mixer = TrackMixer::new(44100.0);
        mixer.set_duration_ms(100, 44100.0);
        mixer.start_crossfade();
        let duration = mixer.config.duration_frames;
        for _ in 0..duration + 10 {
            mixer.process(0.5, 0.5, 0.5, 0.5);
        }
        assert_eq!(mixer.state(), MixerState::PlayingNext);
    }

    #[test]
    fn test_process_block_matches_per_frame() {
        let mut frame_mixer = TrackMixer::new(44100.0);
        let mut block_mixer = TrackMixer::new(44100.0);
        frame_mixer.set_duration_ms(500, 44100.0);
        block_mixer.set_duration_ms(500, 44100.0);
        frame_mixer.start_crossfade();
        block_mixer.start_crossfade();

        // Reference: per-frame processing (incl. the completion transition).
        let n = 24000;
        let mut ref_l = vec![0.0f32; n];
        let mut ref_r = vec![0.0f32; n];
        let mut cur_l = vec![0.0f32; n];
        let mut cur_r = vec![0.0f32; n];
        let mut nxt_l = vec![0.0f32; n];
        let mut nxt_r = vec![0.0f32; n];
        for i in 0..n {
            cur_l[i] = 0.6 * (i as f32 * 0.01).sin();
            cur_r[i] = 0.6 * (i as f32 * 0.011).cos();
            nxt_l[i] = 0.5 * (i as f32 * 0.02).cos();
            nxt_r[i] = 0.5 * (i as f32 * 0.021).sin();
            let (l, r) = frame_mixer.process(cur_l[i], cur_r[i], nxt_l[i], nxt_r[i]);
            ref_l[i] = l;
            ref_r[i] = r;
        }

        // Block version: copy the per-frame output into the current buffers.
        let mut got_l = cur_l.clone();
        let mut got_r = cur_r.clone();
        for start in (0..n).step_by(64) {
            let end = (start + 64).min(n);
            block_mixer.process_block(
                &mut got_l[start..end],
                &mut got_r[start..end],
                &nxt_l[start..end],
                &nxt_r[start..end],
            );
        }
        for i in 0..n {
            assert!(
                (got_l[i] - ref_l[i]).abs() < 1e-6,
                "L mismatch at {}: {} vs {}",
                i,
                got_l[i],
                ref_l[i]
            );
            assert!(
                (got_r[i] - ref_r[i]).abs() < 1e-6,
                "R mismatch at {}: {} vs {}",
                i,
                got_r[i],
                ref_r[i]
            );
        }
        assert_eq!(block_mixer.state(), MixerState::PlayingNext);
    }

    #[test]
    fn test_crossfade_start_and_end_gains() {
        // At t=0: outgoing should be full, incoming should be silent
        let (out0, in0) = TrackMixer::compute_gains_for_curve(0.0, CrossfadeCurve::EqualPower);
        assert!((out0 - 1.0).abs() < 1e-5, "Outgoing at start should be 1.0");
        assert!((in0 - 0.0).abs() < 1e-5, "Incoming at start should be 0.0");

        // At t=1: outgoing should be silent, incoming should be full
        let (out1, in1) = TrackMixer::compute_gains_for_curve(1.0, CrossfadeCurve::EqualPower);
        assert!((out1 - 0.0).abs() < 1e-5, "Outgoing at end should be 0.0");
        assert!((in1 - 1.0).abs() < 1e-5, "Incoming at end should be 1.0");
    }

    #[test]
    fn test_gapless_transition() {
        let mut mixer = TrackMixer::new(44100.0);
        // Before next track is available
        let (l, r) = mixer.process_gapless(0.5, 0.5, false, 0.0, 0.0);
        assert!((l - 0.5).abs() < 1e-5);
        assert!((r - 0.5).abs() < 1e-5);

        // After next track is available
        let (l, r) = mixer.process_gapless(0.5, 0.5, true, 0.8, 0.8);
        assert!((l - 0.8).abs() < 1e-5);
        assert!((r - 0.8).abs() < 1e-5);
    }

    #[test]
    fn test_outgoing_buffer_crossfade() {
        let mut mixer = TrackMixer::new(44100.0);
        mixer.set_duration_ms(10, 44100.0); // Very short crossfade for testing

        let tail_len = mixer.config.duration_frames;
        let tail_left: Vec<f32> = (0..tail_len).map(|i| 0.5 - i as f32 * 0.001).collect();
        let tail_right: Vec<f32> = (0..tail_len).map(|i| 0.5 - i as f32 * 0.001).collect();
        mixer.set_outgoing_tail(tail_left, tail_right);
        mixer.start_crossfade();

        // Process through crossfade
        for _ in 0..tail_len + 10 {
            mixer.process(0.0, 0.0, 0.5, 0.5);
        }
        assert_eq!(mixer.state(), MixerState::PlayingNext);
    }

    #[test]
    fn test_fade_transition_envelope_no_overlap() {
        // `TransitionMode::Fade` is a SEQUENTIAL transition: fade-out current
        // track → silence gap → fade-in next. The two tracks are never heard
        // at the same time (no overlap), unlike a crossfade. The total
        // duration is split into equal thirds. Outgoing = 1.0, incoming =
        // 2.0, so the contributing track is unambiguous at every frame.
        let mut mixer = TrackMixer::new(44100.0);
        mixer.set_curve(CrossfadeCurve::Linear);
        mixer.set_duration_ms(300, 44100.0); // 13230 frames
        let total = mixer.duration_frames();
        assert!(total >= 300, "sanity: duration_frames = {total}");
        let fade_out_end = total / 3;
        let gap_end = total * 2 / 3;

        // Drive the whole fade through both the f32 and f64 entry points
        // with separate mixer instances (each process call advances state).
        let mut m32 = TrackMixer::new(44100.0);
        m32.set_curve(CrossfadeCurve::Linear);
        m32.set_duration_ms(300, 44100.0);
        m32.start_fade();
        assert_eq!(m32.state(), MixerState::Fading);
        let mut m64 = TrackMixer::new(44100.0);
        m64.set_curve(CrossfadeCurve::Linear);
        m64.set_duration_ms(300, 44100.0);
        m64.start_fade();

        let mut out_f32: Vec<(f32, f32)> = Vec::with_capacity(total);
        let mut out_f64: Vec<(f64, f64)> = Vec::with_capacity(total);
        for _ in 0..total {
            out_f32.push(m32.process(1.0, 1.0, 2.0, 2.0));
            out_f64.push(m64.process_f64(1.0, 1.0, 2.0, 2.0));
        }
        assert_eq!(m32.state(), MixerState::PlayingNext);
        assert_eq!(m64.state(), MixerState::PlayingNext);

        // Start of fade-out: only the outgoing track, at full gain.
        let (l0, _) = out_f32[0];
        assert!((l0 - 1.0).abs() < 1e-5, "fade-out start: {l0}");
        // Mid fade-out: outgoing between 0 and 1, incoming never mixed in.
        let (lm, _) = out_f32[fade_out_end / 2];
        assert!(lm > 0.2 && lm < 0.99, "mid fade-out: {lm}");
        // End of fade-out: near zero.
        let (le, _) = out_f32[fade_out_end - 1];
        assert!(le.abs() < 0.1, "end of fade-out: {le}");

        // Gap: both tracks silent.
        for i in fade_out_end..gap_end {
            let (l, r) = out_f32[i];
            assert!(l.abs() < 1e-6 && r.abs() < 1e-6, "gap frame {i}: ({l},{r})");
        }

        // Start of fade-in: near zero (incoming ramps from 0).
        let (ls, _) = out_f32[gap_end];
        assert!(ls.abs() < 0.1, "fade-in start: {ls}");
        // Mid fade-in: only the incoming track, between 0 and 2.
        let (lm, _) = out_f32[gap_end + (total - gap_end) / 2];
        assert!(lm > 0.9 && lm < 1.9, "mid fade-in: {lm}");
        // End of fade-in: only the incoming track at full gain.
        let (le, _) = out_f32[total - 1];
        assert!((le - 2.0).abs() < 1e-3, "end of fade-in: {le}");

        // The f64 path must trace the same envelope.
        let (l0, _) = out_f64[0];
        assert!((l0 - 1.0).abs() < 1e-9);
        let (lg, _) = out_f64[fade_out_end + 1];
        assert!(lg.abs() < 1e-9, "gap must be silent in f64: {lg}");
        let (le, _) = out_f64[total - 1];
        // The final frame sits at t = (duration-1)/duration, so the ramp is
        // within one frame of full — assert close, not exact.
        assert!((le - 2.0).abs() < 1e-3, "end of fade-in (f64): {le}");

        // After the fade completes, only the incoming track is output.
        let (l, _) = m32.process(1.0, 1.0, 2.0, 2.0);
        assert!((l - 2.0).abs() < 1e-4);
    }

    #[test]
    fn test_crossfade_uses_live_outgoing_samples_when_no_tail() {
        let mut mixer = TrackMixer::new(44100.0);
        mixer.set_duration_ms(10, 44100.0); // ~441 frames
        mixer.start_crossfade();

        let duration = mixer.config.duration_frames;
        let mid = duration / 2;
        let mut got_mid_l = 0.0_f32;
        for i in 0..duration {
            let (l, _r) = mixer.process(0.5, 0.5, 0.5, 0.5);
            if i + 1 == mid {
                got_mid_l = l;
            }
        }
        let expected = 0.5_f32 * (0.5_f32).sqrt() + 0.5_f32 * (0.5_f32).sqrt();
        assert!(
            (got_mid_l - expected).abs() < 1e-3,
            "F#01 regression: midpoint output should be {expected:.4} (both tracks mixed), got {got_mid_l:.4}. \
             If you see ~0.3536, the outgoing track was silenced again.",
        );
    }
}
