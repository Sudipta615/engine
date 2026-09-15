//! Real-time stereo TimeStretcher and Pitch-Shifter.
//!
//! Provides high-fidelity, pitch-independent playback speed alteration (`TimeStretch`)
//! and tempo-independent pitch adjustment (`PitchShift`).
//!
//! Supports WSOLA, Phase-Locked Phase Vocoder (Laroche-Dolson), and adaptive Hybrid
//! modes with transient protection.

use std::f32::consts::PI;

use config::TimeStretchQuality;

use super::config_types::{TimeStretchConfig, TimeStretchMode};
use super::phase_vocoder::PhaseVocoder;
use super::transient::TransientDetector;
use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;

/// Polyphase interpolation table size for the pitch-shift resampler.
pub(crate) const PITCH_PHASES: usize = 64;
/// Number of windowed-sinc taps for the pitch interpolator (must be even).
pub(crate) const PITCH_TAPS: usize = 16;
/// Future taps read from the output FIFO (`0..PITCH_TAPS_HALF`).
pub(crate) const PITCH_TAPS_HALF: usize = PITCH_TAPS / 2;
/// Past taps kept in a rolling history ring (`PITCH_TAPS_HALF - 1`).
pub(crate) const PITCH_HISTORY_LEN: usize = PITCH_TAPS_HALF - 1;

/// 4-term Blackman-Harris window evaluated over `x ∈ (-m, m)`; zero outside.
#[inline]
fn blackman_harris4(x: f32, m: f32) -> f32 {
    if x <= -m || x >= m {
        return 0.0;
    }
    let u = (x + m) / (2.0 * m);
    let two_pi = 2.0 * PI;
    const A0: f32 = 0.35875;
    const A1: f32 = 0.48829;
    const A2: f32 = 0.14128;
    const A3: f32 = 0.01168;
    A0 - A1 * (two_pi * u).cos() + A2 * (2.0 * two_pi * u).cos() - A3 * (3.0 * two_pi * u).cos()
}

/// Precompute a Hann window of the given length.
fn hann_window(len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / (len as f32)).cos()))
        .collect()
}

/// Real-time stereo time-stretcher and pitch-shifter.
pub struct TimeStretcher {
    pub(crate) sample_rate: f32,
    pub(crate) config: TimeStretchConfig,
    pub(crate) enabled: bool,

    // Target parameters with smooth ramping
    pub(crate) target_speed: f32,
    pub(crate) current_speed: f32,
    pub(crate) target_pitch_ratio: f32,
    pub(crate) current_pitch_ratio: f32,

    // Window table (precomputed)
    pub(crate) window: Vec<f32>,

    // Preallocated ring buffers for streaming input
    pub(crate) input_ring_l: Vec<f32>,
    pub(crate) input_ring_r: Vec<f32>,
    pub(crate) input_write_pos: usize,
    pub(crate) input_available: usize,

    // Synthesis overlap-add accumulation buffer
    pub(crate) synth_accum_l: Vec<f32>,
    pub(crate) synth_accum_r: Vec<f32>,
    pub(crate) synth_accum_pos: usize,

    // Output FIFO buffer
    pub(crate) output_fifo_l: Vec<f32>,
    pub(crate) output_fifo_r: Vec<f32>,
    pub(crate) output_read_pos: usize,
    pub(crate) output_available: usize,

    // Fractional analysis position
    pub(crate) analysis_pos: f64,
    pub(crate) prev_offset: isize,

    // Scratch buffers for cross-correlation
    pub(crate) scratch_target_l: Vec<f32>,
    pub(crate) scratch_target_r: Vec<f32>,

    /// Pre-allocated f32 scratch for the f64 processing path.
    pub(crate) scratch_f64_l: Vec<f32>,
    pub(crate) scratch_f64_r: Vec<f32>,

    // Resampler state for pitch-shifting
    pub(crate) resample_phase: f64,
    pub(crate) interp_table: Vec<f32>,
    pub(crate) pitch_history_l: Vec<f32>,
    pub(crate) pitch_history_r: Vec<f32>,
    pub(crate) pitch_history_len: usize,

    // Phase Vocoder & Hybrid processing extensions
    pub(crate) mode: TimeStretchMode,
    pub(crate) pv: PhaseVocoder,
    pub(crate) transient: TransientDetector,
    pub(crate) formant_preservation: bool,
}

impl TimeStretcher {
    /// Create a new `TimeStretcher` for the given sample rate.
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            44100.0
        };
        let config = TimeStretchConfig::default();
        let win_size = config.window_size;
        let window = hann_window(win_size);

        let (max_win, max_hop, max_search) = TimeStretchQuality::High.params();
        let input_cap = (max_win + max_search * 2 + max_hop * 4)
            .next_power_of_two()
            .max(65536);
        let fifo_cap = (max_win * 8).next_power_of_two().max(32768);
        let scratch_f64_cap = MAX_AUDIO_BLOCK_FRAMES;

        let mut interp_table = vec![0.0f32; PITCH_PHASES * PITCH_TAPS];
        for p in 0..PITCH_PHASES {
            let t = p as f32 / PITCH_PHASES as f32;
            let row = p * PITCH_TAPS;
            let mut sum = 0.0f32;
            for tap in 0..PITCH_TAPS {
                let offset = tap as f32 - (PITCH_TAPS_HALF as f32 - 1.0);
                let d = t - offset;
                let sinc = if d.abs() < 1e-6 {
                    1.0
                } else {
                    (PI * d).sin() / (PI * d)
                };
                let coefficient = sinc * blackman_harris4(d, PITCH_TAPS_HALF as f32);
                interp_table[row + tap] = coefficient;
                sum += coefficient;
            }
            for tap in 0..PITCH_TAPS {
                interp_table[row + tap] /= sum;
            }
        }

        let pv = PhaseVocoder::new(sample_rate);
        let transient = TransientDetector::new(sample_rate);

        Self {
            sample_rate,
            config,
            enabled: false,
            target_speed: 1.0,
            current_speed: 1.0,
            target_pitch_ratio: 1.0,
            current_pitch_ratio: 1.0,
            window,
            input_ring_l: vec![0.0f32; input_cap],
            input_ring_r: vec![0.0f32; input_cap],
            input_write_pos: 0,
            input_available: 0,
            synth_accum_l: vec![0.0f32; max_win * 2],
            synth_accum_r: vec![0.0f32; max_win * 2],
            synth_accum_pos: 0,
            output_fifo_l: vec![0.0f32; fifo_cap],
            output_fifo_r: vec![0.0f32; fifo_cap],
            output_read_pos: 0,
            output_available: 0,
            analysis_pos: 0.0,
            prev_offset: 0,
            scratch_target_l: vec![0.0f32; max_win],
            scratch_target_r: vec![0.0f32; max_win],
            scratch_f64_l: vec![0.0f32; scratch_f64_cap],
            scratch_f64_r: vec![0.0f32; scratch_f64_cap],
            resample_phase: 0.0,
            interp_table,
            pitch_history_l: vec![0.0f32; PITCH_HISTORY_LEN],
            pitch_history_r: vec![0.0f32; PITCH_HISTORY_LEN],
            pitch_history_len: 0,
            mode: TimeStretchMode::Wsola,
            pv,
            transient,
            formant_preservation: false,
        }
    }

    /// Enable or disable the time-stretcher.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset();
        }
    }

    /// Change the quality tier.
    pub fn set_quality(&mut self, quality: TimeStretchQuality) {
        if self.config.quality == quality {
            return;
        }
        self.config.set_quality(quality);
        self.window = hann_window(self.config.window_size);
        self.reset();
    }

    /// The active quality tier.
    pub fn quality(&self) -> TimeStretchQuality {
        self.config.quality
    }

    /// Whether the time-stretcher is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled && (self.current_speed != 1.0 || self.current_pitch_ratio != 1.0)
    }

    /// Set processing mode (WSOLA, PhaseVocoder, Hybrid).
    pub fn set_mode(&mut self, mode: TimeStretchMode) {
        self.mode = mode;
        self.config.mode = mode;
    }

    /// Get current processing mode.
    pub fn mode(&self) -> TimeStretchMode {
        self.mode
    }

    /// Enable or disable formant preservation.
    pub fn set_formant_preservation(&mut self, enabled: bool) {
        self.formant_preservation = enabled;
        self.config.formant_preservation = enabled;
        self.pv.set_formant_preservation(enabled);
    }

    /// Query formant preservation status.
    pub fn formant_preservation(&self) -> bool {
        self.formant_preservation
    }

    /// Algorithmic latency in milliseconds.
    pub fn latency_ms(&self) -> f32 {
        if !self.is_enabled() || self.sample_rate <= 0.0 {
            0.0
        } else {
            let lookahead = self.config.window_size / 2 + self.config.search_range;
            lookahead as f32 / self.sample_rate * 1000.0
        }
    }

    /// Algorithmic latency in samples.
    pub fn latency_samples(&self) -> usize {
        if !self.is_enabled() {
            0
        } else {
            self.config.window_size / 2 + self.config.search_range
        }
    }

    /// Set playback speed multiplier (0.25 to 4.0).
    pub fn set_speed(&mut self, speed: f32) {
        let clamped = speed.clamp(0.25, 4.0);
        self.target_speed = clamped;
        self.config.speed = clamped;
        if !self.enabled {
            self.current_speed = clamped;
        }
        if (clamped - 1.0).abs() > 0.001 || (self.current_pitch_ratio - 1.0).abs() > 0.001 {
            self.enabled = true;
        }
    }

    /// Get current speed multiplier.
    pub fn speed(&self) -> f32 {
        self.target_speed
    }

    /// Sample rate in Hz.
    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Update sample rate.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if sample_rate.is_finite() && sample_rate > 0.0 {
            self.sample_rate = sample_rate;
        }
    }

    /// Set pitch shift in semitones (-24.0 to +24.0).
    pub fn set_pitch_semitones(&mut self, semitones: f32) {
        let clamped = semitones.clamp(-24.0, 24.0);
        self.config.pitch_semitones = clamped;
        let ratio = 2.0_f32.powf(clamped / 12.0);
        self.target_pitch_ratio = ratio;
        if !self.enabled {
            self.current_pitch_ratio = ratio;
        }
        if clamped.abs() > 0.001 || (self.current_speed - 1.0).abs() > 0.001 {
            self.enabled = true;
        }
    }

    /// Set pitch ratio directly (0.25 to 4.0).
    pub fn set_pitch_ratio(&mut self, ratio: f32) {
        let clamped = ratio.clamp(0.25, 4.0);
        self.target_pitch_ratio = clamped;
        self.config.pitch_semitones = 12.0 * clamped.log2();
        if !self.enabled {
            self.current_pitch_ratio = clamped;
        }
        if (clamped - 1.0).abs() > 0.001 || (self.current_speed - 1.0).abs() > 0.001 {
            self.enabled = true;
        }
    }

    /// Current pitch ratio.
    pub fn pitch_ratio(&self) -> f32 {
        self.target_pitch_ratio
    }

    /// Get current pitch shift in semitones.
    pub fn pitch_semitones(&self) -> f32 {
        self.config.pitch_semitones
    }

    /// Reset all internal buffers and state.
    pub fn reset(&mut self) {
        self.input_ring_l.fill(0.0);
        self.input_ring_r.fill(0.0);
        self.input_write_pos = 0;
        self.input_available = 0;

        self.synth_accum_l.fill(0.0);
        self.synth_accum_r.fill(0.0);
        self.synth_accum_pos = 0;

        self.output_fifo_l.fill(0.0);
        self.output_fifo_r.fill(0.0);
        self.output_read_pos = 0;
        self.output_available = 0;

        self.analysis_pos = 0.0;
        self.prev_offset = 0;
        self.resample_phase = 0.0;
        self.pitch_history_l.fill(0.0);
        self.pitch_history_r.fill(0.0);
        self.pitch_history_len = 0;
        self.current_speed = self.target_speed;
        self.current_pitch_ratio = self.target_pitch_ratio;

        self.pv.reset();
        self.transient.reset();
    }

    fn push_input(&mut self, left: &[f32], right: &[f32]) -> usize {
        let requested = left.len().min(right.len());
        let cap = self.input_ring_l.len();
        let n = requested.min(cap.saturating_sub(self.input_available));
        for i in 0..n {
            let idx = (self.input_write_pos + i) % cap;
            self.input_ring_l[idx] = left[i];
            self.input_ring_r[idx] = right[i];
        }
        self.input_write_pos = (self.input_write_pos + n) % cap;
        self.input_available += n;
        n
    }

    #[inline]
    fn get_input_sample(&self, offset: usize) -> (f32, f32) {
        debug_assert!(
            offset < self.input_available,
            "get_input_sample: offset {} out of bounds (available={})",
            offset,
            self.input_available
        );
        let cap = self.input_ring_l.len();
        let start = (self.input_write_pos + cap - (self.input_available % cap)) % cap;
        let idx = (start + offset) % cap;
        (self.input_ring_l[idx], self.input_ring_r[idx])
    }

    fn process_wsola_hop(&mut self) -> bool {
        let win_size = self.config.window_size;
        let hop_size = self.config.hop_size;
        let search_range = self.config.search_range;

        if (self.target_speed - self.current_speed).abs() > 0.0001 {
            self.current_speed += 0.05 * (self.target_speed - self.current_speed);
        } else {
            self.current_speed = self.target_speed;
        }
        if (self.target_pitch_ratio - self.current_pitch_ratio).abs() > 0.0001 {
            self.current_pitch_ratio += 0.05 * (self.target_pitch_ratio - self.current_pitch_ratio);
        } else {
            self.current_pitch_ratio = self.target_pitch_ratio;
        }

        let effective_stretch = (self.current_speed / self.current_pitch_ratio).clamp(0.25, 4.0);
        let analysis_hop = hop_size as f64 * effective_stretch as f64;

        let needed_input = win_size + hop_size + search_range * 2;
        if self.input_available < needed_input {
            return false;
        }

        let nominal_pos = self.analysis_pos;
        let max_start = self.input_available.saturating_sub(win_size + hop_size);
        let nominal_clamped = (nominal_pos.round() as isize).clamp(0, max_start as isize) as usize;
        let search_lo = nominal_clamped.saturating_sub(search_range);
        let search_hi = (nominal_clamped + search_range).min(max_start);

        let search_min = search_lo as isize - nominal_clamped as isize;
        let search_max = search_hi as isize - nominal_clamped as isize;

        let mut best_k = 0isize;
        let mut best_corr = f32::NEG_INFINITY;

        let mut k = search_min;
        while k <= search_max {
            let abs_start = (nominal_clamped as isize + k) as usize;
            let mut corr = 0.0f32;
            let mut norm_cand = 0.0001f32;
            let mut norm_prev = 0.0001f32;

            for j in (0..win_size).step_by(4) {
                let offset = abs_start + j;
                if offset >= self.input_available {
                    break;
                }
                let (cl, cr) = self.get_input_sample(offset);
                let (pl, pr) = (self.scratch_target_l[j], self.scratch_target_r[j]);

                let cand = cl + cr;
                let prev = pl + pr;
                corr += cand * prev;
                norm_cand += cand * cand;
                norm_prev += prev * prev;
            }

            let norm = (norm_cand * norm_prev).sqrt();
            let score = if norm > 0.0 { corr / norm } else { 0.0 };

            if score > best_corr {
                best_corr = score;
                best_k = k;
            }
            k += 2;
        }

        for dk in [-1isize, 1isize] {
            let k_refined = best_k + dk;
            if k_refined >= search_min && k_refined <= search_max {
                let abs_start = (nominal_clamped as isize + k_refined) as usize;
                let mut corr = 0.0f32;
                let mut norm_cand = 0.0001f32;
                let mut norm_prev = 0.0001f32;
                for j in (0..win_size).step_by(2) {
                    let offset = abs_start + j;
                    if offset >= self.input_available {
                        break;
                    }
                    let (cl, cr) = self.get_input_sample(offset);
                    let (pl, pr) = (self.scratch_target_l[j], self.scratch_target_r[j]);
                    let cand = cl + cr;
                    let prev = pl + pr;
                    corr += cand * prev;
                    norm_cand += cand * cand;
                    norm_prev += prev * prev;
                }
                let norm = (norm_cand * norm_prev).sqrt();
                let score = if norm > 0.0 { corr / norm } else { 0.0 };
                if score > best_corr {
                    best_corr = score;
                    best_k = k_refined;
                }
            }
        }

        let abs_best = (nominal_clamped as isize + best_k) as usize;

        if self.output_available + hop_size > self.output_fifo_l.len() {
            return false;
        }

        let accum_len = self.synth_accum_l.len();
        for j in 0..win_size {
            let offset = abs_best + j;
            let (sl, sr) = if offset < self.input_available {
                self.get_input_sample(offset)
            } else {
                (0.0, 0.0)
            };
            let w = self.window[j];
            let acc_idx = (self.synth_accum_pos + j) % accum_len;

            self.synth_accum_l[acc_idx] += sl * w;
            self.synth_accum_r[acc_idx] += sr * w;

            let target_offset = abs_best + hop_size + j;
            let (tsl, tsr) = if target_offset < self.input_available {
                self.get_input_sample(target_offset)
            } else {
                (0.0, 0.0)
            };
            self.scratch_target_l[j] = tsl;
            self.scratch_target_r[j] = tsr;
        }

        let fifo_cap = self.output_fifo_l.len();
        let fifo_write_start = (self.output_read_pos + self.output_available) % fifo_cap;

        let norm_factor = if hop_size > 0 && win_size > 0 {
            1.0 / (win_size as f32 / (hop_size as f32 * 2.0))
        } else {
            1.0
        };

        for j in 0..hop_size {
            let acc_idx = (self.synth_accum_pos + j) % accum_len;
            let out_idx = (fifo_write_start + j) % fifo_cap;

            let ol = self.synth_accum_l[acc_idx] * norm_factor;
            let or_ = self.synth_accum_r[acc_idx] * norm_factor;

            self.output_fifo_l[out_idx] = ol;
            self.output_fifo_r[out_idx] = or_;

            self.synth_accum_l[acc_idx] = 0.0;
            self.synth_accum_r[acc_idx] = 0.0;
        }

        self.synth_accum_pos = (self.synth_accum_pos + hop_size) % accum_len;
        self.output_available += hop_size;

        let advance = (analysis_hop.round() as usize).max(1);
        let advance = advance.min(self.input_available);
        self.input_available = self.input_available.saturating_sub(advance);
        self.analysis_pos = (nominal_pos + analysis_hop - advance as f64).max(0.0);
        self.prev_offset = best_k;

        true
    }

    #[inline]
    pub(crate) fn interpolate(
        &self,
        phase: usize,
        history: &[f32],
        history_len: usize,
        fifo: &[f32],
        fifo_cap: usize,
        base_idx: usize,
    ) -> f32 {
        let row = &self.interp_table[phase * PITCH_TAPS..(phase + 1) * PITCH_TAPS];
        let mut acc = 0.0f32;
        for (tap, &coefficient) in row.iter().enumerate() {
            let offset = tap as isize - (PITCH_TAPS_HALF as isize - 1);
            let sample = if offset < 0 {
                let k = (-offset) as usize;
                if k <= history_len {
                    history[history.len() - k]
                } else {
                    0.0
                }
            } else {
                fifo[(base_idx + offset as usize) % fifo_cap]
            };
            acc += coefficient * sample;
        }
        acc
    }

    /// Process stereo block with time-stretching and pitch-shifting.
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block_limited(&mut left[start..end], &mut right[start..end]);
                start = end;
            }
            return;
        }
        self.process_block_limited(left, right);
    }

    fn process_block_limited(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }

        // Fast passthrough when inactive / unity
        if !self.enabled
            || ((self.current_speed - 1.0).abs() < 0.001
                && (self.target_speed - 1.0).abs() < 0.001
                && (self.current_pitch_ratio - 1.0).abs() < 0.001
                && (self.target_pitch_ratio - 1.0).abs() < 0.001
                && self.output_available == 0)
        {
            return;
        }

        // Check active mode
        match self.mode {
            TimeStretchMode::PhaseVocoder => {
                self.pv
                    .process_block(left, right, self.target_speed, self.target_pitch_ratio);
                return;
            }
            TimeStretchMode::Hybrid => {
                let is_transient = self.transient.process_block(left, right) > 0.0;
                let is_pitch_shift = (self.target_pitch_ratio - 1.0).abs() > 0.05;
                if !is_transient && is_pitch_shift {
                    self.pv
                        .process_block(left, right, self.target_speed, self.target_pitch_ratio);
                    return;
                }
            }
            TimeStretchMode::Wsola => {}
        }

        let accepted = self.push_input(&left[..n], &right[..n]);
        if accepted < n {
            log::warn!(
                "TimeStretcher input ring full; dropping {} input frames to preserve realtime safety",
                n - accepted
            );
        }

        let pitch_ratio = self.current_pitch_ratio as f64;
        let is_pitch_shifted = (pitch_ratio - 1.0).abs() > 0.001;

        let needed_fifo =
            ((n as f64 * pitch_ratio.max(1.0)).ceil() as usize) + self.config.hop_size * 2;
        while self.output_available < needed_fifo {
            if !self.process_wsola_hop() {
                break;
            }
        }

        let fifo_cap = self.output_fifo_l.len();
        let mut read_idx = self.output_read_pos;

        for i in 0..n {
            if is_pitch_shifted {
                if self.output_available >= PITCH_TAPS_HALF {
                    let base_idx = read_idx;
                    let phase =
                        (self.resample_phase * PITCH_PHASES as f64).floor() as usize % PITCH_PHASES;

                    left[i] = self.interpolate(
                        phase,
                        &self.pitch_history_l,
                        self.pitch_history_len,
                        &self.output_fifo_l,
                        fifo_cap,
                        base_idx,
                    );
                    right[i] = self.interpolate(
                        phase,
                        &self.pitch_history_r,
                        self.pitch_history_len,
                        &self.output_fifo_r,
                        fifo_cap,
                        base_idx,
                    );

                    self.resample_phase += pitch_ratio;
                    let advance = self.resample_phase.floor() as usize;
                    self.resample_phase -= advance as f64;
                    let advance_clamped = advance.min(self.output_available);

                    for step in 0..advance_clamped {
                        let idx = (read_idx + step) % fifo_cap;
                        for h in 0..PITCH_HISTORY_LEN - 1 {
                            self.pitch_history_l[h] = self.pitch_history_l[h + 1];
                            self.pitch_history_r[h] = self.pitch_history_r[h + 1];
                        }
                        self.pitch_history_l[PITCH_HISTORY_LEN - 1] = self.output_fifo_l[idx];
                        self.pitch_history_r[PITCH_HISTORY_LEN - 1] = self.output_fifo_r[idx];
                        self.pitch_history_len =
                            (self.pitch_history_len + 1).min(PITCH_HISTORY_LEN);
                    }
                    read_idx = (read_idx + advance_clamped) % fifo_cap;
                    self.output_available = self.output_available.saturating_sub(advance_clamped);
                } else {
                    left[i] = 0.0;
                    right[i] = 0.0;
                }
            } else {
                if self.output_available > 0 {
                    left[i] = self.output_fifo_l[read_idx];
                    right[i] = self.output_fifo_r[read_idx];
                    read_idx = (read_idx + 1) % fifo_cap;
                    self.output_available = self.output_available.saturating_sub(1);
                } else {
                    left[i] = 0.0;
                    right[i] = 0.0;
                }
            }
        }

        self.output_read_pos = read_idx;
    }

    /// Process a stereo block in f64 precision.
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        let n = left.len().min(right.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block_f64(&mut left[start..end], &mut right[start..end]);
                start = end;
            }
            return;
        }
        self.process_block_f64_limited(left, right);
    }

    fn process_block_f64_limited(&mut self, left: &mut [f64], right: &mut [f64]) {
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }

        if !self.enabled
            || ((self.current_speed - 1.0).abs() < 0.001
                && (self.target_speed - 1.0).abs() < 0.001
                && (self.current_pitch_ratio - 1.0).abs() < 0.001
                && (self.target_pitch_ratio - 1.0).abs() < 0.001
                && self.output_available == 0)
        {
            return;
        }

        let mut scratch_l = std::mem::take(&mut self.scratch_f64_l);
        let mut scratch_r = std::mem::take(&mut self.scratch_f64_r);

        debug_assert!(n <= MAX_AUDIO_BLOCK_FRAMES);
        debug_assert!(scratch_l.len() >= n && scratch_r.len() >= n);

        for i in 0..n {
            scratch_l[i] = left[i] as f32;
            scratch_r[i] = right[i] as f32;
        }

        self.process_block(&mut scratch_l[..n], &mut scratch_r[..n]);

        for i in 0..n {
            left[i] = scratch_l[i] as f64;
            right[i] = scratch_r[i] as f64;
        }

        self.scratch_f64_l = scratch_l;
        self.scratch_f64_r = scratch_r;
    }
}
