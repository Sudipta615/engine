use super::types::{
    LoudnessMeasurement, LoudnessMetadata, LoudnessMode, ABSOLUTE_GATE_LUFS, MOMENTARY_BLOCK_SECS,
    MOMENTARY_HOP_SECS, RELATIVE_GATE_OFFSET_LU, SHORT_TERM_WINDOW_SECS,
};
use crate::decode::ChannelLayout;
use crate::dsp::true_peak::TruePeakMeter;
use std::f32::consts::PI;

/// Second-order high shelf (stage 1 of K-weighting)
///
/// Uses the DeMan coefficients from ITU-R BS.1770-4: the RBJ-cookbook
/// shelf response does not match the ITU-specified response, so the
/// shelf is implemented as a biquad in transposed direct form II.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KWeightStage1 {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    // One filter state per channel (up to `MAX_CHANNELS`).
    z1: [f32; 8],
    z2: [f32; 8],
}

impl KWeightStage1 {
    pub(crate) fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            log::warn!(
                "KWeightStage1: invalid sample_rate {:.1}, defaulting to 44100",
                sample_rate
            );
            44100.0
        };
        let f0: f32 = 1_681.974_5;
        let g: f32 = 3.999_843_8; // dB of shelf boost
        let q: f32 = 0.707_175_25;
        let k = (PI * f0 / sample_rate).tan();
        let kk = k * k;
        // Shelf gain is specified in dB: convert to linear voltage gain.
        let vh = 10.0_f32.powf(g / 20.0);
        let vb = vh.powf(0.499_666_78);
        let norm = kk + k / q + 1.0;
        Self {
            b0: (vh + vb * k / q + kk) / norm,
            b1: 2.0 * (kk - vh) / norm,
            b2: (vh - vb * k / q + kk) / norm,
            a1: 2.0 * (kk - 1.0) / norm,
            a2: (1.0 - k / q + kk) / norm,
            z1: [0.0; 8],
            z2: [0.0; 8],
        }
    }

    #[inline]
    pub(crate) fn process(&mut self, sample: f32, ch: usize) -> f32 {
        let out = sample * self.b0 + self.z1[ch];
        self.z1[ch] = crate::buffer::flush_denormal(sample * self.b1 - out * self.a1 + self.z2[ch]);
        self.z2[ch] = crate::buffer::flush_denormal(sample * self.b2 - out * self.a2);
        out
    }
}

/// Second-order high-pass (stage 2 of K-weighting)
#[derive(Debug, Clone, Copy)]
pub(crate) struct KWeightStage2 {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    // One filter state per channel (up to `MAX_CHANNELS`).
    z1: [f32; 8],
    z2: [f32; 8],
}

impl KWeightStage2 {
    pub(crate) fn new(sample_rate: f32) -> Self {
        // L6: Guard against zero or negative sample_rate.
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            log::warn!(
                "KWeightStage2: invalid sample_rate {:.1}, defaulting to 44100",
                sample_rate
            );
            44100.0
        };
        let f0 = 38.135_47;
        let q = 0.500_327_05;
        let k = (PI * f0 / sample_rate).tan();
        let kk = k * k;
        let norm = kk + k / q + 1.0;
        Self {
            b0: 1.0 / norm,
            b1: -2.0 / norm,
            b2: 1.0 / norm,
            a1: 2.0 * (kk - 1.0) / norm,
            a2: (1.0 - k / q + kk) / norm,
            z1: [0.0; 8],
            z2: [0.0; 8],
        }
    }

    #[inline]
    pub(crate) fn process(&mut self, sample: f32, ch: usize) -> f32 {
        let out = sample * self.b0 + self.z1[ch];
        self.z1[ch] = crate::buffer::flush_denormal(sample * self.b1 - out * self.a1 + self.z2[ch]);
        self.z2[ch] = crate::buffer::flush_denormal(sample * self.b2 - out * self.a2);
        out
    }
}

/// Loudness normaliser for playback
///
/// Applies gain adjustments based on pre-computed loudness metadata.
/// Supports ReplayGain (track/album) and EBU R128 modes.
///
/// This is a **gain-application stage only**.  Loudness *measurement* lives
/// in [`LoudnessMeter`] (which implements the full BS.1770-4 gating
/// algorithm and the shared true-peak detector); the normaliser consumes
/// the resulting metadata (`LoudnessMetadata`) and applies a smoothed
/// linear gain.  Keeping one measurement implementation prevents the
/// scanner and the playback chain from silently disagreeing about LUFS.
pub struct LoudnessNormalizer {
    mode: LoudnessMode,
    target_lufs: f32,
    true_peak_guard: bool,
    true_peak_dbtp: f32,
    preamp_db: f32,
    /// Maximum positive gain (boost) in dB; `None` = unlimited.
    max_boost_db: Option<f32>,
    /// Maximum negative gain (attenuation) in dB; `None` = unlimited.
    max_attenuation_db: Option<f32>,
    /// Current applied gain (linear)
    pub(crate) current_gain_linear: f32,
    /// Target gain (linear, computed from metadata)
    pub(crate) target_gain_linear: f32,
    /// Smoothing coefficient for gain changes
    smooth_coeff: f32,
}

impl LoudnessNormalizer {
    /// Create a new normaliser (off by default). `sample_rate` is accepted
    /// for API compatibility; the normaliser contains no rate-dependent
    /// state (measurement filters moved to [`LoudnessMeter`]).
    pub fn new(_sample_rate: f32) -> Self {
        Self {
            mode: LoudnessMode::Off,
            target_lufs: -23.0,
            true_peak_guard: true,
            true_peak_dbtp: -1.0,
            preamp_db: 0.0,
            max_boost_db: None,
            max_attenuation_db: None,
            current_gain_linear: 1.0,
            target_gain_linear: 1.0,
            smooth_coeff: 0.0005,
        }
    }

    /// Set the loudness normalisation mode
    pub fn set_mode(&mut self, mode: LoudnessMode) {
        self.mode = mode;
    }

    /// Whether loudness normalisation is active (not Off)
    pub fn is_enabled(&self) -> bool {
        self.mode != LoudnessMode::Off
    }

    /// Set the target LUFS for EBU R128 mode
    pub fn set_target_lufs(&mut self, target: f32) {
        self.target_lufs = target;
    }

    /// Configure true peak guard
    pub fn set_true_peak_guard(&mut self, enabled: bool, ceiling_dbtp: f32) {
        self.true_peak_guard = enabled;
        self.true_peak_dbtp = ceiling_dbtp;
    }

    /// Configure the gain-range clamps (spec §21 "max boost" /
    /// "max attenuation"). `None` leaves the corresponding bound unlimited.
    pub fn set_gain_clamps(&mut self, max_boost_db: Option<f32>, max_attenuation_db: Option<f32>) {
        self.max_boost_db = max_boost_db.filter(|v| v.is_finite());
        self.max_attenuation_db = max_attenuation_db.filter(|v| v.is_finite());
    }

    /// Set preamp in dB
    pub fn set_preamp_db(&mut self, gain_db: f32) {
        self.preamp_db = gain_db;
    }

    /// Update loudness metadata for the current track, computing gain
    pub fn set_track_metadata(&mut self, meta: &LoudnessMetadata) {
        let safe_rg_track_db = meta.replaygain_track_db.filter(|v| v.is_finite());
        let safe_rg_album_db = meta.replaygain_album_db.filter(|v| v.is_finite());
        let safe_rg_track_peak = meta
            .replaygain_track_peak
            .filter(|v| v.is_finite() && *v >= 0.0);
        let safe_rg_album_peak = meta
            .replaygain_album_peak
            .filter(|v| v.is_finite() && *v >= 0.0);
        let safe_ebu_loudness = meta.ebu_r128_loudness.filter(|v| v.is_finite());
        let safe_ebu_peak = meta.ebu_r128_peak.filter(|v| v.is_finite());

        let gain_db = match self.mode {
            LoudnessMode::Off => 0.0,
            LoudnessMode::TrackReplayGain => safe_rg_track_db
                .map(|rg| rg + self.preamp_db)
                .unwrap_or(0.0),
            LoudnessMode::AlbumReplayGain => safe_rg_album_db
                .map(|rg| rg + self.preamp_db)
                .unwrap_or(0.0),
            LoudnessMode::EbuR128 => safe_ebu_loudness
                .map(|loudness| self.target_lufs - loudness + self.preamp_db)
                .unwrap_or(0.0),
        };

        let gain_db = if gain_db.is_finite() { gain_db } else { 0.0 };

        // Apply true peak guard
        let peak = match self.mode {
            LoudnessMode::TrackReplayGain => safe_rg_track_peak,
            LoudnessMode::AlbumReplayGain => safe_rg_album_peak,
            LoudnessMode::EbuR128 => safe_ebu_peak.map(|p| 10.0_f32.powf(p / 20.0)),
            _ => None,
        };

        let adjusted_gain = if self.true_peak_guard {
            if let Some(peak_linear) = peak {
                if peak_linear > 0.0 {
                    let new_peak_db = 20.0 * peak_linear.log10() + gain_db;
                    if new_peak_db > self.true_peak_dbtp {
                        gain_db - (new_peak_db - self.true_peak_dbtp)
                    } else {
                        gain_db
                    }
                } else {
                    gain_db
                }
            } else {
                gain_db
            }
        } else {
            gain_db
        };

        let mut adjusted_gain = if adjusted_gain.is_finite() {
            adjusted_gain
        } else {
            0.0
        };
        // Gain-range clamps (spec §21): bound the boost and the attenuation
        // independently so a loudness mode can never apply an out-of-range
        // gain, regardless of the metadata.
        if let Some(max_boost) = self.max_boost_db {
            if adjusted_gain > max_boost {
                adjusted_gain = max_boost;
            }
        }
        if let Some(max_attenuation) = self.max_attenuation_db {
            if adjusted_gain < max_attenuation {
                adjusted_gain = max_attenuation;
            }
        }
        self.target_gain_linear = 10.0_f32.powf(adjusted_gain / 20.0);
        if !self.target_gain_linear.is_finite() || self.target_gain_linear <= 0.0 {
            self.target_gain_linear = 1.0;
        }
    }

    /// Process a stereo sample pair with loudness normalisation
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.mode == LoudnessMode::Off {
            return (left, right);
        }
        // Smooth gain transition
        self.current_gain_linear +=
            self.smooth_coeff * (self.target_gain_linear - self.current_gain_linear);
        self.current_gain_linear = crate::buffer::flush_denormal(self.current_gain_linear);
        (
            left * self.current_gain_linear,
            right * self.current_gain_linear,
        )
    }

    /// Process an N-channel audio frame with loudness normalisation.
    ///
    /// Advances the smoothed gain exactly once per frame (the same ramp as
    /// [`Self::process`]) so the multichannel pipeline applies loudness to
    /// center/LFE/surround channels with identical timing to the stereo path.
    #[inline]
    pub fn process_frame(&mut self, frame: &mut crate::buffer::AudioFrame) {
        if self.mode == LoudnessMode::Off {
            return;
        }
        self.current_gain_linear +=
            self.smooth_coeff * (self.target_gain_linear - self.current_gain_linear);
        self.current_gain_linear = crate::buffer::flush_denormal(self.current_gain_linear);
        let g = self.current_gain_linear;
        for ch in 0..frame.num_channels as usize {
            frame.channels[ch] *= g;
        }
    }

    /// Process a stereo sample pair in f64 precision with loudness normalisation.
    /// Advances the smooth gain transition identically.
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if self.mode == LoudnessMode::Off {
            return (left, right);
        }
        self.current_gain_linear +=
            self.smooth_coeff * (self.target_gain_linear - self.current_gain_linear);
        self.current_gain_linear = crate::buffer::flush_denormal(self.current_gain_linear);
        let g = self.current_gain_linear as f64;
        (left * g, right * g)
    }

    /// Process per-channel planar blocks in place with smooth loudness gain ramping.
    #[inline]
    pub fn process_planes(&mut self, planes: &mut [Vec<f32>], channels: usize, frames: usize) {
        if self.mode == LoudnessMode::Off {
            return;
        }
        let ch = channels.min(planes.len());
        for i in 0..frames {
            self.current_gain_linear +=
                self.smooth_coeff * (self.target_gain_linear - self.current_gain_linear);
            self.current_gain_linear = crate::buffer::flush_denormal(self.current_gain_linear);
            let g = self.current_gain_linear;
            for plane in planes.iter_mut().take(ch) {
                plane[i] *= g;
            }
        }
    }

    /// Process a block of stereo frames in place. Hoists the Off-mode check
    /// out of the per-frame loop.
    #[inline]
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.mode == LoudnessMode::Off {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            self.current_gain_linear +=
                self.smooth_coeff * (self.target_gain_linear - self.current_gain_linear);
            self.current_gain_linear = crate::buffer::flush_denormal(self.current_gain_linear);
            let g = self.current_gain_linear;
            left[i] *= g;
            right[i] *= g;
        }
    }

    /// Process a block of stereo frames in f64 precision. Hoists the Off-mode
    /// check out of the per-frame loop.
    #[inline]
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.mode == LoudnessMode::Off {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            self.current_gain_linear +=
                self.smooth_coeff * (self.target_gain_linear - self.current_gain_linear);
            self.current_gain_linear = crate::buffer::flush_denormal(self.current_gain_linear);
            let g = self.current_gain_linear as f64;
            left[i] *= g;
            right[i] *= g;
        }
    }

    /// Get current applied gain in dB (for metering)
    pub fn current_gain_db(&self) -> f32 {
        if self.current_gain_linear > 0.0 {
            20.0 * self.current_gain_linear.log10()
        } else {
            -60.0
        }
    }

    /// Get the target gain in dB — the gain the normalizer is ramping toward
    /// after the most recent `set_track_metadata` call.
    pub fn target_gain_db(&self) -> f32 {
        if self.target_gain_linear > 0.0 {
            20.0 * self.target_gain_linear.log10()
        } else {
            -60.0
        }
    }

    /// Update the sample rate.
    ///
    /// Accepted for API compatibility — the normaliser contains no
    /// rate-dependent state since all measurement (K-weighting, gating,
    /// true peak) moved to [`LoudnessMeter`].
    pub fn set_sample_rate(&mut self, _sample_rate: f32) {}

    /// Reset all state (gain ramps).
    pub fn reset(&mut self) {
        self.current_gain_linear = 1.0;
        self.target_gain_linear = 1.0;
    }
}

/// Per-channel weighting as defined in ITU-R BS.1770-4 for a conventional
/// 5.1 ordering.  Indices: 0=L, 1=R, 2=C, 3=LFE, 4=SL, 5=SR, 6=SBL, 7=SBR.
///
/// [`bs1770_weights_for_layout`] derives the same weights from *semantic*
/// channel positions (BS.1770-4 weights by position, not by index), which
/// is what the meter actually uses — the raw-index constant is kept only
/// as the default for unknown/legacy layouts.
#[allow(dead_code)]
const BS1770_WEIGHTS: [f32; 8] = [1.0, 1.0, 1.0, 0.0, 1.41, 1.41, 1.41, 1.41];

/// BS.1770-4 channel weights derived from a semantic [`ChannelLayout`].
///
/// The standard weights channels by *position*: LFE is excluded (0.0),
/// front L/R/C use 1.0, and every surround/height channel (side, rear,
/// top) uses 1.41.  This stays correct as layouts grow past the
/// conventional 5.1 ordering — e.g. 7.1 (rear surround at 1.41) and
/// immersive/height layouts.
pub fn bs1770_weights_for_layout(layout: &ChannelLayout) -> [f32; 8] {
    use crate::decode::ChannelId;
    let mut weights = [1.0f32; 8];
    for (i, id) in layout.channel_ids().iter().enumerate().take(8) {
        weights[i] = match id {
            ChannelId::Lfe => 0.0,
            ChannelId::FrontLeft | ChannelId::FrontRight | ChannelId::Center => 1.0,
            ChannelId::SideLeft
            | ChannelId::SideRight
            | ChannelId::RearLeft
            | ChannelId::RearRight
            | ChannelId::BackCenter
            | ChannelId::TopFrontLeft
            | ChannelId::TopFrontRight
            | ChannelId::TopRearLeft
            | ChannelId::TopRearRight => 1.41,
            ChannelId::Unknown(_) => 1.0,
        };
    }
    weights
}

/// Absolute gate threshold per EBU R128: −70 LUFS.
/// Full ITU-R BS.1770-4 / EBU R128 loudness meter.
///
/// Call [`LoudnessMeter::process_stereo`] for each sample at audio-thread
/// rate, then call [`LoudnessMeter::snapshot`] at any rate (e.g., every 100
/// ms) to read the current measurement results.
///
/// ## Algorithm
///
/// 1. K-weight each channel independently (two-stage biquad filter).
/// 2. Apply per-channel BS.1770 gain weights.
/// 3. Accumulate mean-square within 400 ms blocks (100 ms hop = 75% overlap).
/// 4. Compute momentary LUFS from the current 400 ms block.
/// 5. Compute short-term LUFS from a 3 s sliding window of 400 ms blocks.
/// 6. Compute integrated LUFS: absolute gate at −70 LUFS → compute ungated
///    mean of passing blocks → relative gate at (ungated_mean − 10 LU) →
///    integrated = mean of all blocks passing both gates.
/// 7. LRA = 95th percentile − 10th percentile of the gated short-term
///    histogram.
pub struct LoudnessMeter {
    sample_rate: f32,

    stage1: KWeightStage1,
    stage2: KWeightStage2,

    // Running sample accumulator for the current 100 ms hop
    block_sum: f64,
    block_samples: u64,
    block_capacity: u64, // samples per 400ms block

    // Hop counter: fires every 100ms
    hop_samples: u64,
    hop_capacity: u64,

    // Rolling 4-segment ring buffer for exact 400 ms momentary energy (4 × 100 ms)
    momentary_ring: [(f64, u64); 4],
    momentary_idx: usize,
    momentary_filled: usize,

    // History of 400 ms block mean-square values (for integrated loudness)
    block_history: Vec<f32>,

    // Circular buffer of recent 100ms segment mean-squares for short-term (3s = 30 × 100ms hops)
    short_term_ring: Vec<f32>,
    short_term_idx: usize,
    short_term_filled: usize,

    // History of short-term loudness (3s window) values for EBU Tech 3342 LRA calculation
    short_term_history: Vec<f32>,

    /// BS.1770-4 channel weights for the current layout (semantic, not
    /// raw index). Rebuilt by `set_channel_layout`.
    channel_weights: [f32; 8],
    /// Per-channel true-peak detectors (shared `TruePeakMeter`
    /// implementation — the same one the limiter and the offline scanner
    /// use, so dBTP means the same thing everywhere).
    true_peak_meters: [TruePeakMeter; 8],
}

impl LoudnessMeter {
    /// Create a new meter for `channels` channels at `sample_rate` Hz.
    ///
    /// The channel count is informational; the meter derives the layout from
    /// the data fed via `process_interleaved`.
    pub fn new(sample_rate: f32, _channels: usize) -> Self {
        let block_capacity = ((MOMENTARY_BLOCK_SECS * sample_rate).round() as u64).max(1);
        let hop_capacity = ((MOMENTARY_HOP_SECS * sample_rate).round() as u64).max(1);
        let short_term_len = ((SHORT_TERM_WINDOW_SECS / MOMENTARY_HOP_SECS).ceil() as usize).max(1);

        Self {
            sample_rate,
            stage1: KWeightStage1::new(sample_rate),
            stage2: KWeightStage2::new(sample_rate),
            block_sum: 0.0,
            block_samples: 0,
            block_capacity,
            hop_samples: 0,
            hop_capacity,
            momentary_ring: [(0.0, 0); 4],
            momentary_idx: 0,
            momentary_filled: 0,
            block_history: Vec::with_capacity(4096),
            short_term_ring: vec![f32::NEG_INFINITY; short_term_len],
            short_term_idx: 0,
            short_term_filled: 0,
            short_term_history: Vec::with_capacity(4096),
            channel_weights: bs1770_weights_for_layout(&ChannelLayout::from_count(_channels)),
            true_peak_meters: std::array::from_fn(|_| TruePeakMeter::new()),
        }
    }

    /// Set the semantic channel layout, rebuilding the BS.1770-4 channel
    /// weights from channel *position* (LFE=0.0, front=1.0, surround=1.41).
    pub fn set_channel_layout(&mut self, layout: &ChannelLayout) {
        self.channel_weights = bs1770_weights_for_layout(layout);
    }

    /// Feed one frame of interleaved PCM (up to 8 channels).
    #[inline]
    pub fn process_interleaved(&mut self, samples: &[f32], n_channels: usize) {
        let n_channels = n_channels.min(8);
        let weights = self.channel_weights;
        for frame in samples.chunks_exact(n_channels) {
            let mut weighted_sum = 0.0f32;
            for (ch, &s) in frame.iter().enumerate().take(n_channels) {
                let w = weights[ch];
                let k_weighted = self.stage2.process(self.stage1.process(s, ch), ch);
                weighted_sum += w * k_weighted * k_weighted;
                // True peak via the shared 4× polyphase FIR oversampler (the
                // same detector the limiter and the offline scanner use).
                self.true_peak_meters[ch].process_sample(s as f64);
            }
            self.block_sum += weighted_sum as f64;
            self.block_samples += 1;
            self.hop_samples += 1;

            // Every 100 ms hop: compute current block mean-square and advance windows
            if self.hop_samples >= self.hop_capacity {
                self.hop_samples = 0;
                self.commit_hop();
            }
        }
    }

    /// Feed a single stereo sample pair.
    #[inline]
    pub fn process_stereo(&mut self, left: f32, right: f32) {
        let buf = [left, right];
        self.process_interleaved(&buf, 2);
    }

    /// Commit one 100ms hop: record exact 400ms sliding window energy and update short-term ring.
    fn commit_hop(&mut self) {
        // Save current 100ms segment into rolling 4-segment ring buffer
        let seg_sum = self.block_sum;
        let seg_samples = self.block_samples;
        self.momentary_ring[self.momentary_idx] = (seg_sum, seg_samples);
        self.momentary_idx = (self.momentary_idx + 1) % 4;
        if self.momentary_filled < 4 {
            self.momentary_filled += 1;
        }

        // Exact 400ms momentary mean-square across the rolling 4-segment window
        let mut total_sum = 0.0f64;
        let mut total_samples = 0u64;
        for i in 0..self.momentary_filled {
            let (s, n) = self.momentary_ring[i];
            total_sum += s;
            total_samples += n;
        }
        let momentary_ms = if total_samples > 0 {
            (total_sum / total_samples as f64) as f32
        } else {
            0.0
        };

        // 100ms segment mean-square for short-term accumulation
        let seg_ms = if seg_samples > 0 {
            (seg_sum / seg_samples as f64) as f32
        } else {
            0.0
        };

        // Accumulate into short-term ring (30 × 100ms = 3s window)
        self.short_term_ring[self.short_term_idx] = seg_ms;
        self.short_term_idx = (self.short_term_idx + 1) % self.short_term_ring.len();
        if self.short_term_filled < self.short_term_ring.len() {
            self.short_term_filled += 1;
        }

        let momentary_lufs = Self::ms_to_lufs(momentary_ms);

        // Add momentary block to integrated history if above absolute gate (-70 LUFS)
        if momentary_lufs > ABSOLUTE_GATE_LUFS {
            self.block_history.push(momentary_ms);
        }

        // Short-term loudness (3s sliding window)
        let short_term_ms = self.short_term_mean();
        let short_term_lufs = Self::ms_to_lufs(short_term_ms);

        // Record short-term loudness for LRA once window is sufficiently populated and above absolute gate
        if self.short_term_filled >= 10 && short_term_lufs > ABSOLUTE_GATE_LUFS {
            self.short_term_history.push(short_term_lufs);
        }

        // Reset hop accumulator for next 100ms interval
        self.block_sum = 0.0;
        self.block_samples = 0;
    }

    /// Average of the `n` most recent 100ms segments.
    fn recent_mean(&self, n: usize) -> f32 {
        let filled = self.short_term_filled.min(n);
        if filled == 0 {
            return 0.0;
        }
        let ring_len = self.short_term_ring.len();
        let mut sum = 0.0f64;
        for i in 0..filled {
            let idx = (self.short_term_idx + ring_len - 1 - i) % ring_len;
            let v = self.short_term_ring[idx];
            if v.is_finite() && v > 0.0 {
                sum += v as f64;
            }
        }
        (sum / filled as f64) as f32
    }

    /// Mean of all values in `self.short_term_ring` (30 × 100ms = 3s short-term).
    fn short_term_mean(&self) -> f32 {
        let n = self.short_term_ring.len();

        self.recent_mean(n)
    }

    /// Convert mean-square to LUFS using the EBU R128 formula.
    /// LKFS = −0.691 + 10 × log10(mean_square)
    fn ms_to_lufs(ms: f32) -> f32 {
        if ms <= 0.0 {
            return f32::NEG_INFINITY;
        }
        -0.691 + 10.0 * ms.log10()
    }

    /// Take a snapshot of all loudness measurements at this moment.
    pub fn snapshot(&self) -> LoudnessMeasurement {
        // Compute current rolling 400ms momentary mean-square
        let mut total_sum = 0.0f64;
        let mut total_samples = 0u64;
        for i in 0..self.momentary_filled {
            let (s, n) = self.momentary_ring[i];
            total_sum += s;
            total_samples += n;
        }
        let momentary_ms = if total_samples > 0 {
            (total_sum / total_samples as f64) as f32
        } else {
            0.0
        };
        let momentary_lufs = Self::ms_to_lufs(momentary_ms);

        let short_term_ms = self.short_term_mean();
        let short_term_lufs = Self::ms_to_lufs(short_term_ms);

        // Integrated loudness with dual-threshold gating (BS.1770-4 §3.2)
        let integrated_lufs = self.compute_integrated();

        let (lra_lu, lra_valid) = self.compute_lra();

        let mut tp = 0.0f64;
        for m in &self.true_peak_meters {
            tp = tp.max(m.max_true_peak_linear());
        }

        LoudnessMeasurement {
            momentary_lufs,
            short_term_lufs,
            integrated_lufs,
            lra_lu,
            lra_valid,
            true_peak_linear: tp as f32,
        }
    }

    /// Compute integrated LUFS using dual-threshold gating (EBU R128 / BS.1770-4 §3.2).
    ///
    /// ## Non-RT contract
    /// This method allocates (filters `block_history` into a new Vec). It is
    /// designed to be called from the metering read path (UI/control thread at
    /// ~10 Hz), **not** from the audio callback thread.
    fn compute_integrated(&self) -> f32 {
        if self.block_history.is_empty() {
            return f32::NEG_INFINITY;
        }

        // Step 1: absolute-gated mean (already filtered when storing to block_history)
        let abs_mean: f64 = self.block_history.iter().map(|&ms| ms as f64).sum::<f64>()
            / self.block_history.len() as f64;
        let abs_mean_lufs = Self::ms_to_lufs(abs_mean as f32);

        // Step 2: relative gate = abs_mean_lufs - 10 LU
        let rel_gate = abs_mean_lufs + RELATIVE_GATE_OFFSET_LU;

        // Step 3: integrate only blocks above relative gate (allocation-free streaming fold)
        let (rel_sum, rel_count) = self
            .block_history
            .iter()
            .copied()
            .filter(|&ms| Self::ms_to_lufs(ms) > rel_gate)
            .fold((0.0f64, 0usize), |(sum, count), ms| {
                (sum + ms as f64, count + 1)
            });

        if rel_count == 0 {
            return f32::NEG_INFINITY;
        }

        let integrated_ms = rel_sum / rel_count as f64;
        Self::ms_to_lufs(integrated_ms as f32)
    }

    /// Compute Loudness Range (LRA) per EBU Tech 3342.
    ///
    /// Returns `(lra_lu, lra_valid)` where `lra_valid` is `false` when the
    /// short-term history has fewer than 2 gated blocks (track too short or
    /// signal below gate). In that case `lra_lu` is 0.0 (undefined) and callers
    /// should not display the value.
    fn compute_lra(&self) -> (f32, bool) {
        if self.short_term_history.len() < 2 {
            // Track is too short or signal was below the absolute gate throughout.
            // LRA is undefined for this programme material — return (0.0, false)
            // rather than fabricating a value from momentary data.
            return (0.0, false);
        }

        // Calculate absolute-gated mean of short-term values in linear energy directly
        let mean_energy: f64 = self
            .short_term_history
            .iter()
            .map(|&lufs| 10.0_f64.powf((lufs as f64 + 0.691) / 10.0))
            .sum::<f64>()
            / self.short_term_history.len() as f64;
        let abs_mean_lufs = Self::ms_to_lufs(mean_energy as f32);

        // Relative gate per EBU Tech 3342: -20 LU below the absolute-gated short-term mean
        let rel_gate = abs_mean_lufs - 20.0;

        let mut gated: Vec<f32> = self
            .short_term_history
            .iter()
            .copied()
            .filter(|&lufs| lufs > rel_gate)
            .collect();

        if gated.len() < 2 {
            return (0.0, false);
        }

        gated.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let low_idx = ((gated.len() as f32 * 0.10).floor() as usize).min(gated.len() - 1);
        let high_idx = ((gated.len() as f32 * 0.95).ceil() as usize).min(gated.len() - 1);

        let lra = (gated[high_idx] - gated[low_idx]).max(0.0);
        (lra, true)
    }

    /// Reset all accumulated state. Call at track boundaries for per-track integrated loudness.
    pub fn reset(&mut self) {
        self.block_sum = 0.0;
        self.block_samples = 0;
        self.hop_samples = 0;
        self.momentary_ring = [(0.0, 0); 4];
        self.momentary_idx = 0;
        self.momentary_filled = 0;
        self.block_history.clear();
        self.short_term_ring.fill(f32::NEG_INFINITY);
        self.short_term_idx = 0;
        self.short_term_filled = 0;
        self.short_term_history.clear();
        // Reset K-weight filter state and true-peak detectors
        self.stage1 = KWeightStage1::new(self.sample_rate);
        self.stage2 = KWeightStage2::new(self.sample_rate);
        for m in &mut self.true_peak_meters {
            m.reset();
        }
    }

    /// Update sample rate (rebuilds filters, resets state).
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        let block_capacity = ((MOMENTARY_BLOCK_SECS * sample_rate).round() as u64).max(1);
        let hop_capacity = ((MOMENTARY_HOP_SECS * sample_rate).round() as u64).max(1);
        self.block_capacity = block_capacity;
        self.hop_capacity = hop_capacity;
        self.reset();
    }
}
