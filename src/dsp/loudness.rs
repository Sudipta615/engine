//! Loudness measurement and normalisation (EBU R128 / ReplayGain)
//!
//! Implements loudness normalisation that applies gain adjustments based on
//! pre-computed loudness metadata. The normaliser runs in the playback pipeline
//! and applies smooth gain transitions.

use crate::decode::ChannelLayout;
use crate::dsp::true_peak::TruePeakMeter;
use std::f32::consts::PI;

/// Loudness normalisation mode
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LoudnessMode {
    #[default]
    Off,
    TrackReplayGain,
    AlbumReplayGain,
    EbuR128,
}

/// Loudness metadata for a track (pre-computed during scanning)
#[derive(Debug, Clone, Copy, Default)]
pub struct LoudnessMetadata {
    /// ReplayGain track gain in dB
    pub replaygain_track_db: Option<f32>,
    /// ReplayGain album gain in dB
    pub replaygain_album_db: Option<f32>,
    /// ReplayGain track peak (linear)
    pub replaygain_track_peak: Option<f32>,
    /// ReplayGain album peak (linear)
    pub replaygain_album_peak: Option<f32>,
    /// EBU R128 integrated loudness in LUFS
    pub ebu_r128_loudness: Option<f32>,
    /// EBU R128 true peak in dBTP
    pub ebu_r128_peak: Option<f32>,
}

/// Second-order high shelf (stage 1 of K-weighting)
///
/// Uses the DeMan coefficients from ITU-R BS.1770-4: the RBJ-cookbook
/// shelf response does not match the ITU-specified response, so the
/// shelf is implemented as a biquad in transposed direct form II.
#[derive(Debug, Clone, Copy)]
struct KWeightStage1 {
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
    fn new(sample_rate: f32) -> Self {
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
        let vb = vh.powf(0.499_666_774_155);
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
    fn process(&mut self, sample: f32, ch: usize) -> f32 {
        let out = sample * self.b0 + self.z1[ch];
        self.z1[ch] = crate::buffer::flush_denormal(sample * self.b1 - out * self.a1 + self.z2[ch]);
        self.z2[ch] = crate::buffer::flush_denormal(sample * self.b2 - out * self.a2);
        out
    }
}

/// Second-order high-pass (stage 2 of K-weighting)
#[derive(Debug, Clone, Copy)]
struct KWeightStage2 {
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
    fn new(sample_rate: f32) -> Self {
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
    fn process(&mut self, sample: f32, ch: usize) -> f32 {
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
    current_gain_linear: f32,
    /// Target gain (linear, computed from metadata)
    target_gain_linear: f32,
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
            for c in 0..ch {
                planes[c][i] *= g;
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
const ABSOLUTE_GATE_LUFS: f32 = -70.0;

/// Relative gate offset: −10 LU below ungated mean.
const RELATIVE_GATE_OFFSET_LU: f32 = -10.0;

/// Momentary block duration: 400 ms.
const MOMENTARY_BLOCK_SECS: f32 = 0.400;

/// Momentary block hop: 75% overlap = 100 ms interval.
const MOMENTARY_HOP_SECS: f32 = 0.100;

/// Short-term window duration: 3 s.
const SHORT_TERM_WINDOW_SECS: f32 = 3.0;

/// Output of a single `LoudnessMeter::snapshot()` call.
#[derive(Debug, Clone, Default)]
pub struct LoudnessMeasurement {
    /// Momentary LUFS (400 ms block ending now).
    pub momentary_lufs: f32,
    /// Short-term LUFS (3 s window ending now).
    pub short_term_lufs: f32,
    /// Integrated LUFS since last `reset()` (gated per BS.1770-4).
    pub integrated_lufs: f32,
    /// Loudness Range in LU (10th–95th percentile of gated short-term blocks).
    ///
    /// **Check `lra_valid` before using this value.** When `lra_valid` is `false`,
    /// `lra_lu` is 0.0 (undefined) because not enough short-term blocks have
    /// accumulated yet (typically requires ~6 s of above-gate audio).
    pub lra_lu: f32,
    /// Whether `lra_lu` was computed from genuine multi-window short-term data.
    ///
    /// `false` when:
    /// - The track is shorter than ~6 s, OR
    /// - The gated short-term history has < 2 blocks, OR
    /// - The signal has been below the absolute gate (−70 LUFS) throughout.
    ///
    /// The EBU R128 standard defines LRA only for programme material with
    /// sufficient duration; returning a fabricated value for short tracks
    /// would be misleading.
    pub lra_valid: bool,
    /// Instantaneous true-peak estimate (linear, not in dBTP yet).
    pub true_peak_linear: f32,
}

impl LoudnessMeasurement {
    /// True peak in dBTP.
    pub fn true_peak_dbtp(&self) -> f32 {
        if self.true_peak_linear > 0.0 {
            20.0 * self.true_peak_linear.log10()
        } else {
            f32::NEG_INFINITY
        }
    }
}

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
        let recent_mean = self.recent_mean(n);
        recent_mean
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
        for v in &mut self.short_term_ring {
            *v = f32::NEG_INFINITY;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_k_weight_stage1_shelf_response() {
        // BS.1770-4 (DeMan) stage-1 high shelf: +0.67 dB at 1 kHz (below the
        // 1682 Hz corner), approaching +4 dB well above the corner.
        let sr = 48000.0f32;
        for (freq, expected_db, tol) in
            [(1000.0, 0.67, 0.4), (5000.0, 3.9, 0.6), (10000.0, 4.0, 0.4)]
        {
            let mut s1 = KWeightStage1::new(sr);
            let n = 48000 * 5;
            let mut sum_sq = 0.0f64;
            let mut sum_raw = 0.0f64;
            for i in 0..n {
                let s = (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin();
                let k = s1.process(s, 0);
                sum_sq += (k as f64) * (k as f64);
                sum_raw += (s as f64) * (s as f64);
            }
            let gain_db = 10.0 * (sum_sq / sum_raw).log10();
            assert!(
                (gain_db - expected_db).abs() < tol,
                "stage-1 shelf gain at {} Hz: expected ~{} dB, got {:.2} dB",
                freq,
                expected_db,
                gain_db
            );
        }
    }

    #[test]
    fn test_meter_channel_sum_calibration() {
        // BS.1770-4 channel-sum semantics for identical stereo input.
        let sr = 48000.0f32;
        let mut meter = LoudnessMeter::new(sr, 2);
        let n = 48000 * 5;
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
            meter.process_stereo(s, s);
        }
        let m = meter.snapshot();
        assert!(
            m.integrated_lufs.is_finite(),
            "meter integrated must be finite"
        );
        // Stereo full-scale 1 kHz sine ≈ -0.02 LUFS (channel sum, not average)
        assert!(
            (m.integrated_lufs - (-0.02)).abs() < 0.8,
            "stereo full-scale 1 kHz should measure near -0.02 LUFS, got {:.2}",
            m.integrated_lufs
        );
    }

    #[test]
    fn test_channel_sum_stereo_vs_mono() {
        // BS.1770-4 sums channel energies: identical stereo content measures
        // exactly 10*log10(2) ≈ 3.01 LU louder than mono.
        let sr = 48000.0f32;
        let mut mono = LoudnessMeter::new(sr, 1);
        let mut stereo = LoudnessMeter::new(sr, 2);
        let n = 48000 * 3;
        let mut mono_samp = Vec::with_capacity(n);
        let mut stereo_samp = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr).sin();
            mono_samp.push(s);
            stereo_samp.extend_from_slice(&[s, s]);
        }
        mono.process_interleaved(&mono_samp, 1);
        stereo.process_interleaved(&stereo_samp, 2);
        let mono_lufs = mono.snapshot().integrated_lufs;
        let stereo_lufs = stereo.snapshot().integrated_lufs;
        let delta = stereo_lufs - mono_lufs;
        assert!(
            (delta - 3.01).abs() < 0.3,
            "stereo should be ~3.01 LU louder than mono, got {:.2} ({:.2} vs {:.2})",
            delta,
            mono_lufs,
            stereo_lufs
        );
    }

    #[test]
    fn test_multichannel_measurement_and_semantic_weights() {
        // 5.1-style 6-channel input must be measurable (filter state is kept
        // per channel, up to MAX_CHANNELS).
        let sr = 48000.0f32;
        let mut meter = LoudnessMeter::new(sr, 6);
        let n = 48000;
        let mut samples = Vec::with_capacity(n * 6);
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
            samples.extend_from_slice(&[s, s, s, 0.0, s, s]);
        }
        meter.process_interleaved(&samples, 6);
        let m = meter.snapshot();
        assert!(m.integrated_lufs.is_finite());

        // Semantic weighting: LFE must be excluded from integration. A
        // 5.1 signal whose only energy sits in the LFE slot must measure as
        // effectively silent — no raw-index arithmetic can express this; it
        // requires knowing that slot 3 *is* the LFE channel.
        let mut lfe_only = LoudnessMeter::new(sr, 6);
        lfe_only.set_channel_layout(&ChannelLayout::FivePointOne);
        let lfe_samp: Vec<f32> = std::iter::repeat([0.0f32, 0.0, 0.0, 0.9, 0.0, 0.0])
            .take(n)
            .flatten()
            .collect();
        lfe_only.process_interleaved(&lfe_samp, 6);
        let lfe_lufs = lfe_only.snapshot().integrated_lufs;
        assert!(
            lfe_lufs < -60.0 || !lfe_lufs.is_finite(),
            "LFE must be excluded from integration, got {lfe_lufs:.2} LUFS"
        );
    }

    #[test]
    fn test_off_mode_passthrough() {
        let mut norm = LoudnessNormalizer::new(44100.0);
        norm.set_mode(LoudnessMode::Off);
        let (l, r) = norm.process(0.5, 0.5);
        assert!((l - 0.5).abs() < 1e-5);
        assert!((r - 0.5).abs() < 1e-5);
    }

    #[test]
    fn test_replay_gain_attenuation() {
        let mut norm = LoudnessNormalizer::new(44100.0);
        norm.set_mode(LoudnessMode::TrackReplayGain);
        let meta = LoudnessMetadata {
            replaygain_track_db: Some(-5.0), // Loud track, RG says -5dB (reduce volume)
            replaygain_track_peak: Some(0.95),
            ..Default::default()
        };
        norm.set_track_metadata(&meta);
        for _ in 0..10000 {
            norm.process(0.5, 0.5);
        }
        let (l, _r) = norm.process(0.5, 0.5);
        // With correct ReplayGain sign: rg + preamp = -5.0 + 0.0 = -5.0 dB (attenuation)
        // A loud track should be attenuated, so output should be less than input
        assert!(
            l < 0.5,
            "Loud track should be attenuated by ReplayGain, got {}",
            l
        );
        assert!(
            l > 0.01,
            "Should still be audible after attenuation, got {}",
            l
        );
    }

    #[test]
    fn test_ebu_r128_normalization() {
        let mut norm = LoudnessNormalizer::new(44100.0);
        norm.set_mode(LoudnessMode::EbuR128);
        norm.set_target_lufs(-23.0);
        let meta = LoudnessMetadata {
            ebu_r128_loudness: Some(-30.0), // Quiet track
            ebu_r128_peak: Some(-3.0),
            ..Default::default()
        };
        norm.set_track_metadata(&meta);
        for _ in 0..10000 {
            norm.process(0.1, 0.1);
        }
        let (l, _r) = norm.process(0.1, 0.1);
        // Should be boosted (7dB = -23 - (-30))
        assert!(l > 0.1, "Quiet track should be boosted, got {}", l);
    }

    #[test]
    fn test_gain_smoothing() {
        let mut norm = LoudnessNormalizer::new(44100.0);
        norm.set_mode(LoudnessMode::EbuR128);
        let meta = LoudnessMetadata {
            ebu_r128_loudness: Some(-20.0),
            ebu_r128_peak: Some(-1.0),
            ..Default::default()
        };
        norm.set_track_metadata(&meta);
        let mut prev_gain = norm.current_gain_linear;
        for _ in 0..1000 {
            norm.process(0.5, 0.5);
            let delta = (norm.current_gain_linear - prev_gain).abs();
            assert!(delta < 0.1, "Gain should change smoothly");
            prev_gain = norm.current_gain_linear;
        }
    }

    #[test]
    fn test_gain_clamps_bound_boost_and_attenuation() {
        let mut norm = LoudnessNormalizer::new(44100.0);
        norm.set_mode(LoudnessMode::EbuR128);
        norm.set_target_lufs(-23.0);
        norm.set_gain_clamps(Some(3.0), Some(-6.0));

        // A very quiet track wants a large +12 dB boost; the clamp must cap it
        // at +3 dB.
        let meta = LoudnessMetadata {
            ebu_r128_loudness: Some(-35.0),
            ebu_r128_peak: Some(-30.0),
            ..Default::default()
        };
        norm.set_track_metadata(&meta);
        let boost_db = 20.0 * norm.target_gain_linear.log10();
        assert!(
            (boost_db - 3.0).abs() < 0.01,
            "boost must be clamped to +3 dB, got {boost_db:.3} dB"
        );

        // A very loud track wants a large −12 dB cut; the clamp must cap it at
        // −6 dB.
        let meta = LoudnessMetadata {
            ebu_r128_loudness: Some(-11.0),
            ebu_r128_peak: Some(-2.0),
            ..Default::default()
        };
        norm.set_track_metadata(&meta);
        let atten_db = 20.0 * norm.target_gain_linear.log10();
        assert!(
            (atten_db - (-6.0)).abs() < 0.01,
            "attenuation must be clamped to −6 dB, got {atten_db:.3} dB"
        );
    }

    #[test]
    fn test_gain_clamps_unlimited_by_default() {
        let mut norm = LoudnessNormalizer::new(44100.0);
        norm.set_mode(LoudnessMode::EbuR128);
        norm.set_target_lufs(-23.0);
        // No clamps set (None): the full gain must be applied.
        let meta = LoudnessMetadata {
            ebu_r128_loudness: Some(-33.0), // +10 dB boost
            ebu_r128_peak: Some(-30.0),
            ..Default::default()
        };
        norm.set_track_metadata(&meta);
        let boost_db = 20.0 * norm.target_gain_linear.log10();
        assert!(
            (boost_db - 10.0).abs() < 0.05,
            "default must be unlimited (full +10 dB), got {boost_db:.3} dB"
        );
    }

    #[test]
    fn test_true_peak_guard() {
        let mut norm = LoudnessNormalizer::new(44100.0);
        norm.set_mode(LoudnessMode::TrackReplayGain);
        norm.set_true_peak_guard(true, -1.0);

        let meta = LoudnessMetadata {
            replaygain_track_db: Some(10.0),
            replaygain_track_peak: Some(0.8),
            ..Default::default()
        };
        norm.set_track_metadata(&meta);
        let guarded_gain = norm.target_gain_linear;

        norm.set_true_peak_guard(false, -1.0);
        norm.set_track_metadata(&meta);
        let unguarded_gain = norm.target_gain_linear;

        assert!(
            guarded_gain <= unguarded_gain,
            "True peak guard should reduce gain when needed"
        );
    }

    // \u2500\u2500 LoudnessMeter tests \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500

    #[test]
    fn test_loudness_meter_silence_below_absolute_gate() {
        // EBU R128 §3.1: blocks below -70 LUFS must be excluded from integration.
        let mut meter = LoudnessMeter::new(44100.0, 2);
        // Feed 5s of silence
        let silence = vec![0.0f32; 44100 * 5 * 2];
        meter.process_interleaved(&silence, 2);
        let m = meter.snapshot();
        // Integrated loudness of silence should be -inf or extremely quiet
        assert!(
            m.integrated_lufs < -69.0 || !m.integrated_lufs.is_finite(),
            "Silence must be below absolute gate, got {}",
            m.integrated_lufs
        );
    }

    #[test]
    fn test_loudness_meter_sine_1khz() {
        // A 1 kHz sine at amplitude 0.1 should produce a finite integrated LUFS.
        let sr = 44100.0f32;
        let mut meter = LoudnessMeter::new(sr, 2);
        let samples: Vec<f32> = (0..44100 * 4)
            .flat_map(|i| {
                let s = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
                [s, s]
            })
            .collect();
        meter.process_interleaved(&samples, 2);
        let m = meter.snapshot();
        // Should be a finite LUFS value well below 0 LUFS
        assert!(m.integrated_lufs.is_finite(), "Should produce finite LUFS");
        assert!(m.integrated_lufs < 0.0, "Should be negative LUFS");
        assert!(
            m.integrated_lufs > -60.0,
            "0.1 amplitude not that quiet: {}",
            m.integrated_lufs
        );
    }

    #[test]
    fn test_loudness_meter_reset() {
        let sr = 44100.0f32;
        let mut meter = LoudnessMeter::new(sr, 2);
        let signal: Vec<f32> = (0..44100)
            .flat_map(|i| {
                let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr).sin();
                [s, s]
            })
            .collect();
        meter.process_interleaved(&signal, 2);
        meter.reset();
        let m = meter.snapshot();
        // After reset, integrated LUFS should be non-finite (no blocks accumulated)
        assert!(
            !m.integrated_lufs.is_finite() || m.integrated_lufs < -60.0,
            "After reset, integrated LUFS should be effectively silent"
        );
    }
}
