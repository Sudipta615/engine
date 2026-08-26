//! Real-time WSOLA (Waveform Similarity Overlap-Add) Time-Stretcher and Pitch-Shifter.
//!
//! Provides high-fidelity, pitch-independent playback speed alteration (`TimeStretch`)
//! and tempo-independent pitch adjustment (`PitchShift`).
//!
//! ## Algorithm Overview
//!
//! WSOLA works in the time domain by overlapping windowed segments of the input audio
//! with optimal alignment determined by cross-correlation. This preserves pitch, transient
//! clarity, and phase coherence without the phase-smearing or "phasiness" common in basic
//! phase vocoders.
//!
//! - **Time-Stretch**: Adjusts synthesis/analysis hop ratio (`Ha = Hs * speed`) while keeping pitch invariant.
//! - **Pitch-Shift**: Combines WSOLA time-stretching with resampling by `1/pitch_ratio` so tempo remains
//!   constant while pitch is transposed.
//! - **Varispeed**: Resampling only (pitch changes proportionally with speed).
//!
//! ## Real-time Safety and Precision Architecture
//!
//! All buffers are pre-allocated during `new()`. The audio thread
//! (`process_block`, `process_block_f64`) performs **zero heap allocations** during steady state:
//! - The WSOLA synthesis core operates internally in high-throughput `f32` precision (optimized for
//!   cache locality and SIMD correlation).
//! - `process_block_f64` provides an allocation-free 64-bit pipeline interface using persistent
//!   `scratch_f64_l / scratch_f64_r` vectors, converting to `f32` for WSOLA synthesis and
//!   promoting the processed audio back to `f64`.
//! - Cross-correlation scratch uses `scratch_target_l / scratch_target_r` pre-allocated at
//!   construction.
//!
//! ## Buffer index correctness
//!
//! `get_input_sample(offset)` reads from the ring buffer starting at the oldest valid
//! sample. The search loop in `process_wsola_hop` clamps `search_min` and `search_max`
//! to the valid sample window so the search can never read stale data outside the live
//! portion of the ring buffer.

use std::f32::consts::PI;

use config::TimeStretchQuality;

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;

/// Default synthesis frame window size in samples (approx 20-30 ms at 44.1/48 kHz).
pub const DEFAULT_WSOLA_WINDOW_SIZE: usize = 1024;
/// Default synthesis hop size (75% overlap).
pub const DEFAULT_WSOLA_HOP_SIZE: usize = 256;
/// Search range for waveform similarity alignment (in samples).
pub const DEFAULT_WSOLA_SEARCH_RANGE: usize = 128;

/// Polyphase interpolation table size for the pitch-shift resampler.
///
/// The pitch stage resamples the WSOLA synthesis output; a higher phase count
/// quantizes the fractional read position more finely without changing the
/// per-sample cost (a single table row dot-product).
const PITCH_PHASES: usize = 64;
/// Number of windowed-sinc taps for the pitch interpolator (must be even).
const PITCH_TAPS: usize = 16;
/// Future taps read from the output FIFO (`0..PITCH_TAPS_HALF`).
const PITCH_TAPS_HALF: usize = PITCH_TAPS / 2;
/// Past taps kept in a rolling history ring (`PITCH_TAPS_HALF - 1`).
const PITCH_HISTORY_LEN: usize = PITCH_TAPS_HALF - 1;

/// 4-term Blackman-Harris window evaluated over `x ∈ (-m, m)`; zero outside.
///
/// Used as the window for the polyphase windowed-sinc interpolator. The
/// 4-term Blackman-Harris window gives a far deeper stopband than a Hann or
/// Blackman window, which keeps the interpolator's imaging/aliasing products
/// well below audibility at extreme pitch ratios.
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

/// Precompute a Hann window of the given length (used for the WSOLA
/// synthesis frame; must match the active window length).
fn hann_window(len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / (len as f32)).cos()))
        .collect()
}

/// Operating configuration for time-stretching and pitch-shifting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeStretchConfig {
    /// Playback speed multiplier (0.25 to 4.0, default 1.0).
    pub speed: f32,
    /// Pitch shift in semitones (-24.0 to +24.0, default 0.0).
    pub pitch_semitones: f32,
    /// Quality tier (spec §22). Maps to the three parameters below via
    /// [`TimeStretchQuality::params`]; tuning them by hand is still possible
    /// but the tier is the supported surface.
    pub quality: TimeStretchQuality,
    /// Window size in samples.
    pub window_size: usize,
    /// Synthesis hop size in samples.
    pub hop_size: usize,
    /// Search delta range in samples.
    pub search_range: usize,
}

impl Default for TimeStretchConfig {
    fn default() -> Self {
        Self::for_quality(TimeStretchQuality::Balanced)
    }
}

impl TimeStretchConfig {
    /// Build a config from a quality tier, resolving its concrete WSOLA
    /// parameters. Speed/pitch default to unity (stretcher inactive).
    pub fn for_quality(quality: TimeStretchQuality) -> Self {
        let (window_size, hop_size, search_range) = quality.params();
        Self {
            speed: 1.0,
            pitch_semitones: 0.0,
            quality,
            window_size,
            hop_size,
            search_range,
        }
    }

    /// Apply a quality tier to an existing config (updates the three
    /// parameters in place).
    pub fn set_quality(&mut self, quality: TimeStretchQuality) {
        self.quality = quality;
        let (window_size, hop_size, search_range) = quality.params();
        self.window_size = window_size;
        self.hop_size = hop_size;
        self.search_range = search_range;
    }
}

/// Real-time stereo WSOLA time-stretcher and pitch-shifter.
pub struct TimeStretcher {
    sample_rate: f32,
    config: TimeStretchConfig,
    enabled: bool,

    // Target parameters with smooth ramping
    target_speed: f32,
    current_speed: f32,
    target_pitch_ratio: f32,
    current_pitch_ratio: f32,

    // Window and normalization tables (precomputed)
    window: Vec<f32>,

    // Preallocated ring buffers for streaming input
    input_ring_l: Vec<f32>,
    input_ring_r: Vec<f32>,
    input_write_pos: usize,
    input_available: usize,

    // Synthesis overlap-add accumulation buffer
    synth_accum_l: Vec<f32>,
    synth_accum_r: Vec<f32>,
    synth_accum_pos: usize,

    // Output FIFO buffer
    output_fifo_l: Vec<f32>,
    output_fifo_r: Vec<f32>,
    output_read_pos: usize,
    output_available: usize,

    // Fractional analysis position
    analysis_pos: f64,
    prev_offset: isize,

    // Scratch buffers for cross-correlation (pre-allocated, no alloc on audio thread)
    scratch_target_l: Vec<f32>,
    scratch_target_r: Vec<f32>,

    /// Pre-allocated f32 scratch for the f64 processing path.
    /// Avoids `vec![0.0; n]` allocation on every `process_block_f64` call
    /// (which would violate real-time safety on the audio callback thread).
    scratch_f64_l: Vec<f32>,
    scratch_f64_r: Vec<f32>,

    // Resampler state for pitch-shifting (fractional phase position)
    resample_phase: f64,

    /// Precomputed polyphase windowed-sinc table for pitch interpolation.
    /// Row `phase` (0..PITCH_PHASES) holds PITCH_TAPS coefficients; tap `i`
    /// reads the sample at offset `i - (PITCH_TAPS_HALF - 1)` relative to the
    /// current FIFO read position (negative = past, non-negative = future).
    interp_table: Vec<f32>,
    /// Rolling history of the most recently consumed FIFO samples (oldest
    /// first) so the interpolator's past taps never read stale ring data.
    pitch_history_l: Vec<f32>,
    pitch_history_r: Vec<f32>,
    pitch_history_len: usize,
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

        // Hann window precomputation for the active (default) window length.
        let window = hann_window(win_size);

        // Buffer sizing is done for the LARGEST quality tier (High) so that
        // `set_quality` never needs to reallocate the realtime rings — a
        // control-path tier change stays allocation-free on the audio thread.
        let (max_win, max_hop, max_search) = TimeStretchQuality::High.params();
        let input_cap = (max_win + max_search * 2 + max_hop * 4)
            .next_power_of_two()
            .max(65536);
        let fifo_cap = (max_win * 8).next_power_of_two().max(32768);

        // The public block contract is shared by every realtime DSP stage.
        // Keep the scratch length fixed so an oversized callback cannot trigger
        // an allocation from the audio thread.
        let scratch_f64_cap = MAX_AUDIO_BLOCK_FRAMES;

        // Precompute the polyphase windowed-sinc interpolation table (one row
        // per fractional phase). Rows are normalized to unity DC gain so a
        // constant signal interpolates exactly.
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
        }
    }

    /// Enable or disable the time-stretcher. When disabled and speed=1.0 and pitch=0,
    /// audio passes through completely unmodified.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset();
        }
    }

    /// Change the quality tier (spec §22). A real algorithmic change: the
    /// WSOLA window length, overlap ratio, and similarity-search range all
    /// follow the tier (see [`TimeStretchQuality::params`]).
    ///
    /// # Realtime safety
    ///
    /// Buffers are pre-sized for the highest tier at construction, so this
    /// call performs **no reallocation of the realtime rings**. The Hann
    /// window table is recomputed and the streaming alignment state is reset
    /// (same contract as `set_enabled(false)`), so this is a control-path
    /// call — never invoke it from the audio callback.
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

    /// Algorithmic latency of the WSOLA analysis window in milliseconds
    /// (0 when inactive). The stretcher must look ahead `window/2 + search
    /// range` samples to align each analysis frame, so the first usable
    /// output sample trails the input by that much. This is the deterministic
    /// buffer term; the overlap-add synthesis adds no further fixed delay.
    pub fn latency_ms(&self) -> f32 {
        if !self.is_enabled() || self.sample_rate <= 0.0 {
            0.0
        } else {
            let lookahead = self.config.window_size / 2 + self.config.search_range;
            lookahead as f32 / self.sample_rate * 1000.0
        }
    }

    /// Set playback speed multiplier (e.g. 0.5 = half speed, 2.0 = double speed).
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

    /// Update the sample-rate metadata without resetting the streaming
    /// alignment/FIFO state. Recovery and device-rate changes must not throw
    /// away a partially assembled WSOLA window.
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

    /// Set pitch ratio directly (e.g. 1.0 = unchanged, 2.0 = 1 octave up).
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

    /// Get current pitch shift in semitones.
    pub fn pitch_semitones(&self) -> f32 {
        self.config.pitch_semitones
    }

    /// Reset all internal buffers and alignment state.
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
    }

    /// Push input samples into the fixed-capacity ring buffer.
    ///
    /// The previous implementation grew this ring when a slow stretch ratio
    /// accumulated lookahead. Growth is not safe on an audio thread, so the
    /// bounded API accepts only the space currently available and lets the
    /// caller's normal underflow policy handle the remainder.
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

    /// Read an input sample at `offset` samples from the oldest valid sample in the ring.
    ///
    /// `offset` must satisfy `0 <= offset < input_available`. Callers are responsible
    /// for clamping before calling (see `process_wsola_hop` for the search bounds).
    #[inline]
    fn get_input_sample(&self, offset: usize) -> (f32, f32) {
        debug_assert!(
            offset < self.input_available,
            "get_input_sample: offset {} out of bounds (available={})",
            offset,
            self.input_available
        );
        let cap = self.input_ring_l.len();
        // The oldest sample lives at write_pos - input_available (mod cap).
        let start = (self.input_write_pos + cap - (self.input_available % cap)) % cap;
        let idx = (start + offset) % cap;
        (self.input_ring_l[idx], self.input_ring_r[idx])
    }

    /// Perform one WSOLA synthesis hop.
    ///
    /// ## Search range clamping
    ///
    /// The search origin is the current nominal analysis position. We look for
    /// the best alignment in `[nominal - search_range, nominal + search_range]`,
    /// but clamp to ensure the full window `[base, base + win_size)` always falls
    /// within `[0, input_available)`. This prevents stale/zero data from being
    /// included in the cross-correlation.
    fn process_wsola_hop(&mut self) -> bool {
        let win_size = self.config.window_size;
        let hop_size = self.config.hop_size;
        let search_range = self.config.search_range;

        // Smooth speed and pitch ratio transitions
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

        // Effective WSOLA stretch factor:
        // - Time-stretch: Ha = Hs * speed
        // - Pitch-shift: WSOLA pre-stretches time by pitch_ratio (Ha = Hs / pitch_ratio),
        //   so subsequent resampler speedup (resample_phase += pitch_ratio) restores original tempo.
        // - Combined: Ha = Hs * (speed / pitch_ratio).
        let effective_stretch = (self.current_speed / self.current_pitch_ratio).clamp(0.25, 4.0);
        let analysis_hop = hop_size as f64 * effective_stretch as f64;

        let needed_input = win_size + hop_size + search_range * 2;
        if self.input_available < needed_input {
            return false;
        }

        // Nominal position of the analysis window start (in ring-relative coords).
        let nominal_pos = self.analysis_pos;

        let max_start = self.input_available.saturating_sub(win_size + hop_size);
        let nominal_clamped = (nominal_pos.round() as isize).clamp(0, max_start as isize) as usize;
        let search_lo = nominal_clamped.saturating_sub(search_range);
        let search_hi = (nominal_clamped + search_range).min(max_start);

        // Convert to relative signed offsets from nominal_clamped for the search loop.
        let search_min = search_lo as isize - nominal_clamped as isize;
        let search_max = search_hi as isize - nominal_clamped as isize;

        // Search for optimal offset in [search_min, search_max] by normalized cross-correlation.
        let mut best_k = 0isize;
        let mut best_corr = f32::NEG_INFINITY;

        // Step by 2 for speed, refine near best peak
        let mut k = search_min;
        while k <= search_max {
            let abs_start = (nominal_clamped as isize + k) as usize;
            let mut corr = 0.0f32;
            let mut norm_cand = 0.0001f32;
            let mut norm_prev = 0.0001f32;

            // Sample similarity over window with step of 4 for low latency
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

        // Fine refinement around best_k (full-resolution pass over ±1 neighbours)
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

        // Do not mutate synthesis state unless the output FIFO has room for
        // the complete hop. This keeps a full FIFO from causing duplicated
        // overlap-add data on the next attempt.
        if self.output_available + hop_size > self.output_fifo_l.len() {
            return false;
        }

        // Window and overlap-add into synthesis buffer
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

            // Store the natural continuation (shifted by hop_size) as the target for the next similarity search
            let target_offset = abs_best + hop_size + j;
            let (tsl, tsr) = if target_offset < self.input_available {
                self.get_input_sample(target_offset)
            } else {
                (0.0, 0.0)
            };
            self.scratch_target_l[j] = tsl;
            self.scratch_target_r[j] = tsr;
        }

        // Produce `hop_size` samples from synthesis accumulator into the
        // fixed-capacity output FIFO.
        let fifo_cap = self.output_fifo_l.len();
        let fifo_write_start = (self.output_read_pos + self.output_available) % fifo_cap;

        // Normalization factor for Hann window with 75% overlap (win_size / hop_size = 4)
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

            // Clear consumed accumulator slice
            self.synth_accum_l[acc_idx] = 0.0;
            self.synth_accum_r[acc_idx] = 0.0;
        }

        self.synth_accum_pos = (self.synth_accum_pos + hop_size) % accum_len;
        self.output_available += hop_size;

        // Advance input buffer by analysis hop
        let advance = (analysis_hop.round() as usize).max(1);
        let advance = advance.min(self.input_available);
        self.input_available = self.input_available.saturating_sub(advance);
        self.analysis_pos = (nominal_pos + analysis_hop - advance as f64).max(0.0);
        self.prev_offset = best_k;

        true
    }

    /// Interpolate one channel at fractional phase `phase` (0..PITCH_PHASES).
    ///
    /// This is the pitch-shift resampler's reconstruction filter: a
    /// precomputed 16-tap Blackman-Harris-windowed sinc (polyphase table),
    /// replacing the former 4-point Hermite cubic. The wider window and deep
    /// window sidelobes substantially reduce the interpolation error and
    /// high-frequency roll-off that the cubic exhibits at extreme pitch
    /// ratios, while the precomputed rows keep the hot path to a single
    /// dot-product.
    ///
    /// Tap `i` of the row reads offset `i - (PITCH_TAPS_HALF - 1)` relative to
    /// `base_idx`: negative offsets are *past* samples, served from `history`
    /// (the most recently consumed FIFO samples, oldest first, `history_len`
    /// valid entries) so they are never stale ring data; non-negative offsets
    /// are *future* samples read from `fifo`. While the history is still
    /// warming up, missing past samples are zero (silence before the stream).
    #[inline]
    fn interpolate(
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

    /// Process stereo block with WSOLA time-stretching and pitch-shifting.
    ///
    /// Oversized caller blocks are split before entering the bounded core.
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

        // Push new incoming audio without allowing ring growth.
        let accepted = self.push_input(&left[..n], &right[..n]);
        if accepted < n {
            log::warn!(
                "TimeStretcher input ring full; dropping {} input frames to preserve realtime safety",
                n - accepted
            );
        }

        let pitch_ratio = self.current_pitch_ratio as f64;
        let is_pitch_shifted = (pitch_ratio - 1.0).abs() > 0.001;

        // Ensure enough output is synthesized in the FIFO for this block
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
                // Need PITCH_TAPS_HALF future samples for the windowed-sinc
                // interpolator (past taps come from the history ring).
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
                    // The skipped FIFO samples become the interpolator's past:
                    // push them into the rolling history ring (oldest first).
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
                    // Underflow fill
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
                    // Underflow fill
                    left[i] = 0.0;
                    right[i] = 0.0;
                }
            }
        }

        self.output_read_pos = read_idx;
    }

    /// Process a stereo block in f64 precision.
    /// Oversized caller blocks are split before entering the bounded core.
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

    /// Process a stereo block in f64 precision.
    ///
    /// ## Architecture & Precision
    ///
    /// The WSOLA core operates internally in f32 precision (optimal for cache locality
    /// and SIMD time-domain cross-correlation). This method provides a 64-bit pipeline
    /// interface by converting incoming f64 samples to/from pre-allocated f32 scratch buffers.
    ///
    /// ## Real-time safety
    ///
    /// This method uses pre-allocated `scratch_f64_l` / `scratch_f64_r` fields.
    /// To avoid borrow checker aliasing on `&mut self` without cloning or allocating,
    /// the buffers are temporarily moved using `std::mem::take` and restored after
    /// processing. Zero heap allocation occurs on the audio thread during steady-state processing.
    fn process_block_f64_limited(&mut self, left: &mut [f64], right: &mut [f64]) {
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

        // Temporarily take the pre-allocated scratch buffers to avoid borrow checker conflicts
        // when calling `process_block` (&mut self). `std::mem::take` on Vec is zero-alloc.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_tiers_map_to_distinct_wsola_parameters() {
        // The tier must be a real algorithmic change, not a label.
        assert_eq!(TimeStretchQuality::Low.params(), (512, 128, 64));
        assert_eq!(TimeStretchQuality::Balanced.params(), (1024, 256, 128));
        assert_eq!(TimeStretchQuality::High.params(), (2048, 512, 256));

        // Latency scales monotonically with the tier once active.
        let mut low = TimeStretcher::new(48_000.0);
        low.set_quality(TimeStretchQuality::Low);
        let mut balanced = TimeStretcher::new(48_000.0);
        balanced.set_quality(TimeStretchQuality::Balanced);
        let mut high = TimeStretcher::new(48_000.0);
        high.set_quality(TimeStretchQuality::High);
        for s in [&mut low, &mut balanced, &mut high] {
            s.set_speed(1.5);
        }
        let l = low.latency_ms();
        let b = balanced.latency_ms();
        let h = high.latency_ms();
        assert!(
            l > 0.0 && l < b && b < h,
            "latency must increase with tier: low={l:.2} balanced={b:.2} high={h:.2}"
        );
    }

    #[test]
    fn quality_change_does_not_reallocate_realtime_storage() {
        let mut stretcher = TimeStretcher::new(48_000.0);
        let input_cap = stretcher.input_ring_l.capacity();
        let output_cap = stretcher.output_fifo_l.capacity();
        let scratch_cap = stretcher.scratch_f64_l.capacity();

        // Sweep the full tier range; the realtime rings must keep their
        // (High-sized) allocations.
        for tier in [
            TimeStretchQuality::Low,
            TimeStretchQuality::Balanced,
            TimeStretchQuality::High,
            TimeStretchQuality::Low,
        ] {
            stretcher.set_quality(tier);
            assert_eq!(stretcher.input_ring_l.capacity(), input_cap);
            assert_eq!(stretcher.output_fifo_l.capacity(), output_cap);
            assert_eq!(stretcher.scratch_f64_l.capacity(), scratch_cap);
        }
        assert_eq!(stretcher.quality(), TimeStretchQuality::Low);
        assert_eq!(stretcher.config.window_size, 512);
    }

    #[test]
    fn oversized_blocks_do_not_grow_realtime_storage() {
        let mut stretcher = TimeStretcher::new(44_100.0);
        stretcher.set_speed(2.0);
        let input_capacity = stretcher.input_ring_l.capacity();
        let output_capacity = stretcher.output_fifo_l.capacity();
        let scratch_capacity = stretcher.scratch_f64_l.capacity();

        let n = MAX_AUDIO_BLOCK_FRAMES * 2 + 17;
        let mut left = vec![0.0f32; n];
        let mut right = vec![0.0f32; n];
        stretcher.process_block(&mut left, &mut right);

        assert_eq!(stretcher.input_ring_l.capacity(), input_capacity);
        assert_eq!(stretcher.output_fifo_l.capacity(), output_capacity);
        assert_eq!(stretcher.scratch_f64_l.capacity(), scratch_capacity);
    }

    #[test]
    fn f64_oversized_blocks_do_not_grow_realtime_storage() {
        let mut stretcher = TimeStretcher::new(44_100.0);
        stretcher.set_speed(2.0);
        let scratch_capacity = stretcher.scratch_f64_l.capacity();
        let n = MAX_AUDIO_BLOCK_FRAMES + 1;
        let mut left = vec![0.0f64; n];
        let mut right = vec![0.0f64; n];
        stretcher.process_block_f64(&mut left, &mut right);
        assert_eq!(stretcher.scratch_f64_l.capacity(), scratch_capacity);
    }

    /// Drive the WSOLA core with a steady stereo sine across the full
    /// speed × pitch envelope. The bounded core must never grow its realtime
    /// storage, must never emit NaN/inf, and must actually produce signal
    /// (a starvation bug that only ever zero-fills would be caught here).
    fn run_extreme_combo(speed: f32, semitones: f32) {
        let mut stretcher = TimeStretcher::new(48_000.0);
        stretcher.set_speed(speed);
        stretcher.set_pitch_semitones(semitones);
        // (The unity 1.0×/0 st combination is a legal passthrough and must
        // still produce finite, bounded output through this same loop.)

        let input_cap = stretcher.input_ring_l.capacity();
        let output_cap = stretcher.output_fifo_l.capacity();
        let scratch_cap = stretcher.scratch_f64_l.capacity();

        const BLOCK: usize = 128;
        const BLOCKS: usize = 64;
        let mut left = [0.0f32; BLOCK];
        let mut right = [0.0f32; BLOCK];
        let mut phase = 0.0f32;
        let mut energy = 0.0f64;

        for _ in 0..BLOCKS {
            for i in 0..BLOCK {
                let s = (phase * std::f32::consts::TAU).sin() * 0.5;
                left[i] = s;
                right[i] = s * 0.8;
                phase = (phase + 440.0 / 48_000.0).fract();
            }
            stretcher.process_block(&mut left, &mut right);
            for i in 0..BLOCK {
                assert!(
                    left[i].is_finite() && right[i].is_finite(),
                    "non-finite output at {speed}x / {semitones}st"
                );
                assert!(
                    left[i].abs() <= 8.0 && right[i].abs() <= 8.0,
                    "unbounded output {:.2} at {speed}x / {semitones}st",
                    left[i]
                );
                energy += (left[i] as f64) * (left[i] as f64);
                energy += (right[i] as f64) * (right[i] as f64);
            }
        }

        // Realtime storage must remain fixed regardless of how extreme the
        // stretch/pitch combination is.
        assert_eq!(stretcher.input_ring_l.capacity(), input_cap);
        assert_eq!(stretcher.output_fifo_l.capacity(), output_cap);
        assert_eq!(stretcher.scratch_f64_l.capacity(), scratch_cap);

        // A 0.5-amplitude sine over 64 × 128 samples carries ~1000 units of
        // energy per channel. Requiring a fraction of that guarantees the
        // processor is synthesizing audio, not perpetually zero-filling.
        assert!(
            energy > 100.0,
            "output has implausibly low energy ({energy:.1}) at {speed}x / {semitones}st"
        );
    }

    #[test]
    fn extreme_speed_pitch_combinations_stay_finite_and_bounded() {
        for speed in [0.25f32, 0.5, 1.0, 2.0, 4.0] {
            for semitones in [-24.0f32, -12.0, 0.0, 12.0, 24.0] {
                run_extreme_combo(speed, semitones);
            }
        }
    }

    #[test]
    fn pitch_only_extremes_produce_finite_bounded_output() {
        // Isolate the pitch-resampling path (speed = 1.0) at the limits.
        for semitones in [-24.0f32, -23.5, -1.0, 1.0, 23.5, 24.0] {
            run_extreme_combo(1.0, semitones);
        }
    }

    /// Reference 4-point Hermite cubic (the interpolation the pitch stage
    /// previously used), kept here purely to quantify the improvement.
    fn hermite4_ref(s0: f32, s1: f32, s2: f32, s3: f32, t: f32) -> f32 {
        let c0 = s1;
        let c1 = 0.5 * (s2 - s0);
        let c2 = s0 - 2.5 * s1 + 2.0 * s2 - 0.5 * s3;
        let c3 = 0.5 * (s3 - s0) + 1.5 * (s1 - s2);
        ((c3 * t + c2) * t + c1) * t + c0
    }

    #[test]
    fn polyphase_interpolator_is_dc_exact_and_band_accurate() {
        let stretcher = TimeStretcher::new(48_000.0);
        let fifo_cap = 4096usize;
        let mut fifo = vec![0.0f32; fifo_cap];

        // DC: with the history ring warmed up, every phase must reproduce the
        // constant exactly (rows are normalized to unity DC gain). A cold
        // history (silence before the stream) legitimately under-weights, so
        // this check uses the warm state.
        fifo.fill(0.75);
        let warm_history = vec![0.75f32; PITCH_HISTORY_LEN];
        for phase in 0..PITCH_PHASES {
            let got =
                stretcher.interpolate(phase, &warm_history, PITCH_HISTORY_LEN, &fifo, fifo_cap, 10);
            assert!(
                (got - 0.75).abs() < 1e-5,
                "phase {phase}: DC gain must be unity, got {got}"
            );
        }

        // Band-limited sine: measure the interpolation error across every
        // phase for both the new windowed-sinc and the old Hermite cubic.
        let sr = 48_000.0f32;
        for (label, freq) in [("2 kHz", 2_000.0f32), ("8 kHz", 8_000.0f32)] {
            let omega = 2.0 * std::f32::consts::PI * freq / sr;
            for (i, slot) in fifo.iter_mut().take(fifo_cap).enumerate() {
                *slot = (omega * i as f32).sin();
            }
            let mut sinc_err = 0.0f32;
            let mut hermite_err = 0.0f32;
            for base in 64..(fifo_cap - PITCH_TAPS_HALF - 1) {
                let mut history = vec![0.0f32; PITCH_HISTORY_LEN];
                for k in 1..=PITCH_HISTORY_LEN {
                    history[PITCH_HISTORY_LEN - k] = fifo[base - k];
                }
                for phase in 0..PITCH_PHASES {
                    let t = base as f32 + phase as f32 / PITCH_PHASES as f32;
                    let want = (omega * t).sin();
                    let got = stretcher.interpolate(
                        phase,
                        &history,
                        PITCH_HISTORY_LEN,
                        &fifo,
                        fifo_cap,
                        base,
                    );
                    let h = hermite4_ref(
                        fifo[base - 1],
                        fifo[base],
                        fifo[base + 1],
                        fifo[base + 2],
                        phase as f32 / PITCH_PHASES as f32,
                    );
                    sinc_err = sinc_err.max((got - want).abs());
                    hermite_err = hermite_err.max((h - want).abs());
                }
            }
            assert!(
                sinc_err < 1e-3,
                "{label}: windowed-sinc error {sinc_err:.6} too high"
            );
            assert!(
                sinc_err < hermite_err * 0.5,
                "{label}: windowed-sinc ({sinc_err:.6}) must beat Hermite ({hermite_err:.6})"
            );
        }
    }
}
