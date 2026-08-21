pub mod context;
pub mod node;
pub mod nodes;
#[cfg(test)]
pub mod tests;

use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::decode::ChannelLayout;
use crate::dsp::pipeline::{
    DspNodeInfo, PrecisionMode, DSP_STAGE_CAPABILITIES,
};
use config::{EngineConfig, LoudnessMode as ConfigLoudnessMode, PerformanceMode};

pub use context::GraphScratch;
pub use node::DspNode;
pub use nodes::*;

const VOLUME_RAMP_DURATION_MS: f32 = 10.0;
const PREAMP_RAMP_DURATION_MS: f32 = VOLUME_RAMP_DURATION_MS;

/// The central DSP Graph executing the statically compiled signal processing chain.
///
/// Implements the target conceptual model:
/// ```text
/// DspGraph
///   ├── Gain (Preamp / SeekFade / Volume)
///   ├── Loudness (LoudnessNormalizer)
///   ├── EQ (ParametricEq / Mid-Side)
///   ├── Dynamics (MultibandCompressor)
///   ├── FIR/Convolution (ConvolutionEngine)
///   ├── Routing (ChannelTrimmer / Bass Management / Matrix)
///   ├── Crossfeed (Crossfeed)
///   ├── Stereo (StereoEnhancer)
///   ├── Time/Pitch (TimeStretcher)
///   ├── Volume (GainProcessor)
///   ├── Resampler (AudioResampler adapter)
///   ├── Limiter (LookaheadLimiter)
///   └── Dither/Conversion (Dither adapter)
/// ```
///
/// Features:
/// - Explicit node descriptor metadata via [`DspNode::capability`]
/// - Automated latency & tail tracking
/// - Zero dynamic allocations on the real-time audio thread
/// - Transparent bit-perfect & DoP bypass execution plans
pub struct DspGraph {
    // ── Pre-mix Chain ──
    pub out_preamp: GainNode,
    pub out_loudness: LoudnessNode,
    pub in_preamp: GainNode,
    pub in_loudness: LoudnessNode,

    // ── Post-mix Chain ──
    pub eq: EqNode,
    pub dynamics: DynamicsNode,
    pub convolution: ConvolutionNode,
    pub balance: BalanceNode,
    pub crossfeed: CrossfeedNode,
    pub stereo: StereoNode,
    pub timestretch: TimeStretchNode,
    pub volume: GainNode,
    pub seek_fade: SeekFadeNode,

    // ── Routing & Multichannel ──
    pub routing: RoutingNode,
    pub multichannel_layout: ChannelLayout,

    // ── Output Domain & Safety ──
    pub resampler: ResamplerNode,
    pub limiter: LimiterNode,
    pub dither: DitherNode,

    // ── Graph State & Control ──
    sample_rate: f32,
    speed: f32,
    volume_fade_ms: f32,
    precision_mode: PrecisionMode,
    performance_mode: PerformanceMode,
    bit_perfect: bool,
    dop_bypass: bool,

    // ── Pre-allocated Scratch Arena ──
    scratch: GraphScratch,
}

impl DspGraph {
    /// Construct a new DSP Graph from an [`EngineConfig`] and sample rate.
    pub fn from_config(config: &EngineConfig, sample_rate: f32) -> Self {
        let num_bands = config.eq.bands.len().max(10);
        let mut timestretch = TimeStretchNode::new(sample_rate);
        timestretch.stretcher.set_quality(config.timestretch_quality);

        let mut graph = Self {
            out_preamp: GainNode::new("out_preamp", "pre-mix", PREAMP_RAMP_DURATION_MS, sample_rate),
            out_loudness: LoudnessNode::new("out_loudness", "pre-mix", sample_rate),
            in_preamp: GainNode::new("in_preamp", "pre-mix", PREAMP_RAMP_DURATION_MS, sample_rate),
            in_loudness: LoudnessNode::new("in_loudness", "pre-mix", sample_rate),

            eq: EqNode::new(num_bands, sample_rate),
            dynamics: DynamicsNode::new(sample_rate),
            convolution: ConvolutionNode::new(sample_rate, 8192),
            balance: BalanceNode::new(),
            crossfeed: CrossfeedNode::new(sample_rate),
            stereo: StereoNode::new(),
            timestretch,
            volume: GainNode::new("volume", "post-mix", config.volume_fade_ms as f32, sample_rate),
            seek_fade: SeekFadeNode::new(config.seek_fade_ms as f32, sample_rate),

            routing: RoutingNode::new(sample_rate),
            multichannel_layout: ChannelLayout::Stereo,

            resampler: ResamplerNode::new(sample_rate, sample_rate),
            limiter: LimiterNode::new(sample_rate),
            dither: DitherNode::new(sample_rate),

            sample_rate,
            speed: 1.0,
            volume_fade_ms: config.volume_fade_ms as f32,
            precision_mode: config.precision_mode,
            performance_mode: config.performance_mode,
            bit_perfect: false,
            dop_bypass: false,

            scratch: GraphScratch::new(),
        };

        graph.apply_config(config);
        graph
    }

    /// Sync the graph configuration from an [`EngineConfig`].
    pub fn apply_config(&mut self, config: &EngineConfig) {
        self.eq.eq.set_enabled(config.eq.enabled);
        self.eq.eq.set_preamp_db(config.eq.preamp_db);
        self.eq.eq.set_post_gain_db(config.eq.post_gain_db);
        self.eq.eq.set_headroom_db(config.eq.headroom_db);

        for (i, band_cfg) in config.eq.bands.iter().enumerate() {
            if i >= self.eq.eq.num_bands() {
                break;
            }
            let filter_type = match band_cfg.filter_type {
                config::FilterType::Peaking => crate::dsp::equalizer::EqFilterType::Peaking,
                config::FilterType::LowShelf => crate::dsp::equalizer::EqFilterType::LowShelf,
                config::FilterType::HighShelf => crate::dsp::equalizer::EqFilterType::HighShelf,
                config::FilterType::LowPass => crate::dsp::equalizer::EqFilterType::LowPass,
                config::FilterType::HighPass => crate::dsp::equalizer::EqFilterType::HighPass,
                config::FilterType::Bandpass => crate::dsp::equalizer::EqFilterType::Bandpass,
                config::FilterType::Notch => crate::dsp::equalizer::EqFilterType::Notch,
                config::FilterType::AllPass => crate::dsp::equalizer::EqFilterType::AllPass,
            };
            self.eq.eq.set_band(
                i,
                crate::dsp::equalizer::EqBandParams {
                    enabled: band_cfg.enabled,
                    filter_type,
                    frequency: band_cfg.frequency,
                    gain_db: band_cfg.gain_db,
                    q: band_cfg.q,
                },
            );
        }
        self.eq.eq.set_auto_headroom(config.eq.auto_headroom);

        let mode = match config.loudness.mode {
            ConfigLoudnessMode::Off => crate::dsp::loudness::LoudnessMode::Off,
            ConfigLoudnessMode::TrackReplayGain => crate::dsp::loudness::LoudnessMode::TrackReplayGain,
            ConfigLoudnessMode::AlbumReplayGain => crate::dsp::loudness::LoudnessMode::AlbumReplayGain,
            ConfigLoudnessMode::EbuR128 => crate::dsp::loudness::LoudnessMode::EbuR128,
        };
        for loud in [&mut self.out_loudness, &mut self.in_loudness] {
            loud.normalizer.set_mode(mode);
            loud.normalizer.set_target_lufs(config.loudness.target_lufs);
            loud.normalizer.set_true_peak_guard(
                config.loudness.true_peak_guard,
                config.loudness.true_peak_dbtp,
            );
            loud.normalizer.set_gain_clamps(
                config.loudness.max_boost_db,
                config.loudness.max_attenuation_db,
            );
        }

        self.limiter.limiter.set_enabled(config.limiter.enabled);
        self.limiter.limiter.set_lookahead(config.limiter.lookahead_ms);
        self.limiter.limiter.set_attack(config.limiter.attack_ms);
        self.limiter.limiter.set_release(config.limiter.release_ms);
        self.limiter.limiter.set_ceiling_db(config.limiter.ceiling_db);
        if config.limiter.soft_clip {
            self.limiter
                .limiter
                .set_mode(crate::dsp::limiter::LimiterMode::Saturate);
        } else {
            self.limiter
                .limiter
                .set_mode(crate::dsp::limiter::LimiterMode::Transparent);
        }

        self.stereo.enhancer.set_enabled(config.stereo_enhancer.enabled);
        self.stereo.enhancer.set_width(config.stereo_enhancer.width);

        self.crossfeed.crossfeed.set_enabled(config.crossfeed.enabled);
        self.crossfeed.crossfeed.set_custom_params(
            config.crossfeed.custom_freq,
            config.crossfeed.custom_q,
            config.crossfeed.custom_delay_ms,
        );
        self.crossfeed.crossfeed.set_profile(config.crossfeed.profile);

        self.dynamics.compressor.set_enabled(config.multiband_compressor.enabled);
        self.dynamics.compressor.set_band_params(
            0,
            config.multiband_compressor.low_band.threshold_db,
            config.multiband_compressor.low_band.ratio,
            config.multiband_compressor.low_band.attack_ms,
            config.multiband_compressor.low_band.release_ms,
            config.multiband_compressor.low_band.makeup_gain_db,
        );
        self.dynamics.compressor.set_band_params(
            1,
            config.multiband_compressor.mid_band.threshold_db,
            config.multiband_compressor.mid_band.ratio,
            config.multiband_compressor.mid_band.attack_ms,
            config.multiband_compressor.mid_band.release_ms,
            config.multiband_compressor.mid_band.makeup_gain_db,
        );
        self.dynamics.compressor.set_band_params(
            2,
            config.multiband_compressor.high_band.threshold_db,
            config.multiband_compressor.high_band.ratio,
            config.multiband_compressor.high_band.attack_ms,
            config.multiband_compressor.high_band.release_ms,
            config.multiband_compressor.high_band.makeup_gain_db,
        );
        for (band, cfg) in [
            (0usize, &config.multiband_compressor.low_band),
            (1usize, &config.multiband_compressor.mid_band),
            (2usize, &config.multiband_compressor.high_band),
        ] {
            self.dynamics.compressor.set_band_features(
                band,
                cfg.knee_db,
                cfg.detector,
                cfg.stereo_link,
            );
        }

        if config.convolution.enabled {
            self.convolution.engine.set_enabled(true);
            self.convolution.engine.set_wet_mix(config.convolution.wet_mix);
            if let Some(ref ir_path) = config.convolution.ir_path {
                let path = std::path::Path::new(ir_path);
                if path.exists() {
                    let _ = self.convolution.engine.load_ir_from_file(path);
                }
            }
        } else {
            self.convolution.engine.set_enabled(false);
        }

        self.routing.trimmer.set_config(&config.channel_trim, self.sample_rate);
        self.routing.trimmer.set_channel_eq(&config.channel_eq, self.sample_rate);
        self.routing.trimmer.set_bass_management(&config.bass_management, self.sample_rate);
        self.routing.trimmer.set_routing(&config.channel_routing);

        let mut lfe_config = config.lfe.clone();
        if config.bass_management.enabled && lfe_config.crossover_hz.is_none() && lfe_config.enabled {
            lfe_config.crossover_hz = Some(config.bass_management.crossover_hz);
        }
        self.routing.trimmer.set_lfe(&lfe_config);
        self.routing.trimmer.set_lfe_channels(
            self.multichannel_layout
                .channel_ids()
                .iter()
                .enumerate()
                .filter(|(_, id)| **id == crate::decode::ChannelId::Lfe)
                .map(|(i, _)| i)
                .collect(),
        );

        self.performance_mode = config.performance_mode;
        self.precision_mode = config.precision_mode;
        self.apply_performance_mode();
    }

    fn apply_performance_mode(&mut self) {
        if self.performance_mode == PerformanceMode::LowPower {
            self.stereo.enhancer.set_enabled(false);
            self.crossfeed.crossfeed.set_enabled(false);
            self.convolution.engine.set_enabled(false);
            self.limiter.limiter.enable_true_peak(false);
        }
    }

    /// Update sample rate across all graph nodes.
    pub fn update_sample_rate(&mut self, sample_rate: f32) {
        if self.sample_rate == sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.out_preamp.prepare(sample_rate, MAX_CHANNELS);
        self.in_preamp.prepare(sample_rate, MAX_CHANNELS);
        self.out_loudness.prepare(sample_rate, MAX_CHANNELS);
        self.in_loudness.prepare(sample_rate, MAX_CHANNELS);
        self.eq.prepare(sample_rate, 2);
        self.dynamics.prepare(sample_rate, 2);
        self.convolution.prepare(sample_rate, 2);
        self.crossfeed.prepare(sample_rate, 2);
        self.stereo.prepare(sample_rate, 2);
        self.timestretch.prepare(sample_rate, 2);
        self.volume.prepare(sample_rate, MAX_CHANNELS);
        self.seek_fade.prepare(sample_rate, MAX_CHANNELS);
        self.routing.prepare(sample_rate, MAX_CHANNELS);
        self.limiter.prepare(sample_rate, MAX_CHANNELS);
        self.dither.prepare(sample_rate, MAX_CHANNELS);
    }

    /// Reset internal state across all nodes.
    pub fn reset(&mut self) {
        self.out_preamp.reset();
        self.in_preamp.reset();
        self.out_loudness.reset();
        self.in_loudness.reset();
        self.eq.reset();
        self.dynamics.reset();
        self.convolution.reset();
        self.balance.reset();
        self.crossfeed.reset();
        self.stereo.reset();
        self.timestretch.reset();
        self.volume.reset();
        self.seek_fade.reset();
        self.routing.reset();
        self.limiter.reset();
        self.dither.reset();
    }

    /// Reset filter state only without touching volume ramps.
    pub fn reset_filters_only(&mut self) {
        self.out_preamp.reset();
        self.in_preamp.reset();
        self.out_loudness.reset();
        self.in_loudness.reset();
        self.eq.reset();
        self.dynamics.reset();
        self.convolution.reset();
        self.crossfeed.reset();
        self.stereo.reset();
        self.timestretch.reset();
        self.routing.reset();
        self.limiter.reset();
    }

    pub fn set_precision_mode(&mut self, mode: PrecisionMode) {
        self.precision_mode = mode;
    }

    pub fn precision_mode(&self) -> PrecisionMode {
        self.precision_mode
    }

    pub fn set_bit_perfect(&mut self, enabled: bool) {
        self.bit_perfect = enabled;
        if enabled {
            self.volume.processor.set_gain(1.0);
            self.volume.processor.snap();
            self.seek_fade.fade.reset();
        }
    }

    pub fn is_bit_perfect(&self) -> bool {
        self.bit_perfect
    }

    pub fn set_dop_bypass(&mut self, enabled: bool) {
        self.dop_bypass = enabled;
    }

    pub fn is_dop_bypass(&self) -> bool {
        self.dop_bypass
    }

    pub fn set_multichannel_layout(&mut self, layout: &ChannelLayout) {
        self.multichannel_layout = layout.clone();
        self.routing.trimmer.set_lfe_channels(
            layout
                .channel_ids()
                .iter()
                .enumerate()
                .filter(|(_, id)| **id == crate::decode::ChannelId::Lfe)
                .map(|(i, _)| i)
                .collect(),
        );
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed.clamp(0.25, 4.0);
        self.timestretch.stretcher.set_speed(self.speed);
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn volume_fade_ms(&self) -> f32 {
        self.volume_fade_ms
    }

    pub fn set_volume_fade_ms(&mut self, ms: f32) {
        self.volume_fade_ms = ms;
    }

    // ── Signal Processing Plans ───────────────────────────────────────────

    /// Process a block of stereo frames in-place.
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block(&mut left[start..end], &mut right[start..end]);
                start = end;
            }
            return;
        }
        if n == 0 || self.dop_bypass || self.bit_perfect {
            return;
        }

        match self.precision_mode {
            PrecisionMode::Performance => {
                self.process_outgoing_block(left, right);
                self.process_post_mix_block(left, right);
            }
            PrecisionMode::Quality => {
                let mut l64 = std::mem::take(&mut self.scratch.scratch_f64_l);
                let mut r64 = std::mem::take(&mut self.scratch.scratch_f64_r);
                for i in 0..n {
                    l64[i] = left[i] as f64;
                    r64[i] = right[i] as f64;
                }
                self.process_outgoing_block_f64(&mut l64[..n], &mut r64[..n]);
                self.process_post_mix_block_f64(&mut l64[..n], &mut r64[..n]);
                for i in 0..n {
                    left[i] = l64[i] as f32;
                    right[i] = r64[i] as f32;
                }
                self.scratch.scratch_f64_l = l64;
                self.scratch.scratch_f64_r = r64;
            }
        }
    }

    /// Process a block of stereo frames in f64 precision in-place.
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
        if n == 0 || self.dop_bypass || self.bit_perfect {
            return;
        }
        self.process_outgoing_block_f64(left, right);
        self.process_post_mix_block_f64(left, right);
    }

    /// Process the pre-mix outgoing chain (preamp + loudness).
    #[inline]
    pub fn process_outgoing_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bit_perfect {
            return;
        }
        self.out_preamp.processor.process_block_stereo(left, right);
        self.out_loudness.normalizer.process_block(left, right);
    }

    #[inline]
    pub fn process_outgoing_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bit_perfect {
            return;
        }
        self.out_preamp.processor.process_block_stereo_f64(left, right);
        self.out_loudness.normalizer.process_block_f64(left, right);
    }

    /// Process the pre-mix incoming chain (preamp + loudness).
    #[inline]
    pub fn process_incoming_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bit_perfect {
            return;
        }
        self.in_preamp.processor.process_block_stereo(left, right);
        self.in_loudness.normalizer.process_block(left, right);
    }

    #[inline]
    pub fn process_incoming_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bit_perfect {
            return;
        }
        self.in_preamp.processor.process_block_stereo_f64(left, right);
        self.in_loudness.normalizer.process_block_f64(left, right);
    }

    /// Process the post-mix chain over stereo channels in f32.
    pub fn process_post_mix_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bit_perfect {
            return;
        }
        let n = left.len().min(right.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_post_mix_block(&mut left[start..end], &mut right[start..end]);
                start = end;
            }
            return;
        }
        if n == 0 {
            return;
        }

        match self.precision_mode {
            PrecisionMode::Performance => self.process_post_mix_block_f32(left, right),
            PrecisionMode::Quality => {
                let mut l64 = std::mem::take(&mut self.scratch.scratch_f64_l);
                let mut r64 = std::mem::take(&mut self.scratch.scratch_f64_r);
                for i in 0..n {
                    l64[i] = left[i] as f64;
                    r64[i] = right[i] as f64;
                }
                self.process_post_mix_block_f64(&mut l64[..n], &mut r64[..n]);
                for i in 0..n {
                    left[i] = l64[i] as f32;
                    right[i] = r64[i] as f32;
                }
                self.scratch.scratch_f64_l = l64;
                self.scratch.scratch_f64_r = r64;
            }
        }
    }

    fn process_post_mix_block_f32(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if self.eq.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.eq.process(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.eq.process_block(left, right);
        }
        self.dynamics.compressor.process_block(left, right);
        self.convolution.engine.process_block(left, right);
        let bl = self.balance.balance_gain_l;
        let br = self.balance.balance_gain_r;
        for i in 0..n {
            left[i] *= bl;
            right[i] *= br;
        }
        self.crossfeed.crossfeed.process_block(left, right);
        self.stereo.enhancer.process_block(left, right);
        self.timestretch.stretcher.process_block(left, right);
        self.volume.processor.process_block_stereo(left, right);
        self.seek_fade.fade.process_block(left, right);
    }

    /// Process the post-mix chain in native f64.
    pub fn process_post_mix_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bit_perfect {
            return;
        }
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }
        if self.eq.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.eq.process_f64(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.eq.process_block_f64(left, right);
        }
        self.dynamics.compressor.process_block_f64(left, right);
        self.convolution.engine.process_block_f64(left, right);
        let bl = self.balance.balance_gain_l as f64;
        let br = self.balance.balance_gain_r as f64;
        for i in 0..n {
            left[i] *= bl;
            right[i] *= br;
        }
        self.crossfeed.crossfeed.process_block_f64(left, right);
        self.stereo.enhancer.process_block_f64(left, right);
        self.timestretch.stretcher.process_block_f64(left, right);
        self.volume.processor.process_block_stereo_f64(left, right);
        self.seek_fade.fade.process_block_f64(left, right);
    }

    /// Process an interleaved block of `channels`-channel frames in place.
    pub fn process_block_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        if channels == 0 || channels > MAX_CHANNELS {
            return;
        }
        let n = interleaved.len() / channels;
        if n == 0 {
            return;
        }
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block_multichannel(
                    &mut interleaved[start * channels..end * channels],
                    channels,
                );
                start = end;
            }
            return;
        }
        if self.dop_bypass || self.bit_perfect {
            return;
        }

        let mut planes = std::mem::take(&mut self.scratch.scratch_mc);

        if channels <= 2 {
            for i in 0..n {
                let base = i * channels;
                planes[0][i] = interleaved[base];
                planes[1][i] = if channels == 2 {
                    interleaved[base + 1]
                } else {
                    interleaved[base]
                };
            }
            {
                let (front, rest) = planes.split_at_mut(1);
                self.process_block(&mut front[0][..n], &mut rest[0][..n]);
            }
            for i in 0..n {
                let base = i * channels;
                interleaved[base] = planes[0][i];
                if channels == 2 {
                    interleaved[base + 1] = planes[1][i];
                }
            }
            self.scratch.scratch_mc = planes;
            return;
        }

        // Multichannel de-interleave
        for ch in 0..channels {
            for i in 0..n {
                planes[ch][i] = interleaved[i * channels + ch];
            }
        }

        if !self.bit_perfect {
            self.routing
                .trimmer
                .process_planes(&mut planes[..channels], channels, n);
        }

        if self.bit_perfect {
            self.volume
                .processor
                .process_planes(&mut planes[..channels], channels, n);
            self.seek_fade
                .fade
                .process_planes(&mut planes[..channels], channels, n);
        } else {
            self.out_preamp
                .processor
                .process_planes(&mut planes[..channels], channels, n);
            self.out_loudness
                .normalizer
                .process_planes(&mut planes[..channels], channels, n);

            // Stereo filter chain on front L/R
            {
                let (front, rest) = planes.split_at_mut(1);
                self.process_post_mix_front_filters(&mut front[0][..n], &mut rest[0][..n]);
            }

            self.volume
                .processor
                .process_planes(&mut planes[..channels], channels, n);
            self.seek_fade
                .fade
                .process_planes(&mut planes[..channels], channels, n);
        }

        // Re-interleave
        for ch in 0..channels {
            for i in 0..n {
                interleaved[i * channels + ch] = planes[ch][i];
            }
        }
        self.scratch.scratch_mc = planes;
    }

    fn process_post_mix_front_filters(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }
        if self.eq.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.eq.process(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.eq.process_block(left, right);
        }
        self.dynamics.compressor.process_block(left, right);
        self.convolution.engine.process_block(left, right);
        let bl = self.balance.balance_gain_l;
        let br = self.balance.balance_gain_r;
        for i in 0..n {
            left[i] *= bl;
            right[i] *= br;
        }
        self.crossfeed.crossfeed.process_block(left, right);
        self.stereo.enhancer.process_block(left, right);
        self.timestretch.stretcher.process_block(left, right);
    }

    // ── Limiter Execution ─────────────────────────────────────────────────

    pub fn process_final_limiter(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.dop_bypass || self.bit_perfect {
            (left, right)
        } else {
            self.limiter.limiter.process(left, right)
        }
    }

    pub fn process_final_limiter_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.dop_bypass || self.bit_perfect {
            return;
        }
        self.limiter.limiter.process_block(left, right);
    }

    pub fn flush_final_limiter(&mut self) -> Vec<(f32, f32)> {
        self.limiter.limiter.flush()
    }

    // ── Latency & Tail Queries ────────────────────────────────────────────

    /// Snapshot dynamic graph nodes for diagnostics and UI telemetry.
    pub fn graph_nodes(&self) -> Vec<DspNodeInfo> {
        let bypassed = self.bit_perfect || self.dop_bypass;
        let mc_channels = self.multichannel_layout.channel_count();
        let mut nodes = Vec::with_capacity(DSP_STAGE_CAPABILITIES.len());

        for cap in DSP_STAGE_CAPABILITIES {
            let (active, latency_ms, tail_ms) = if bypassed {
                (false, 0.0, 0.0)
            } else {
                match cap.name {
                    "channel_trim" => (self.routing.trimmer.is_active(mc_channels), 0.0, 0.0),
                    "channel_eq" | "bass_management" | "channel_mix" => (false, 0.0, 0.0),
                    "out_preamp" => (self.out_preamp.is_active(), 0.0, 0.0),
                    "in_preamp" => (self.in_preamp.is_active(), 0.0, 0.0),
                    "out_loudness" => (self.out_loudness.is_active(), 0.0, 0.0),
                    "in_loudness" => (self.in_loudness.is_active(), 0.0, 0.0),
                    "mixer" => (false, 0.0, 0.0),
                    "eq" => (self.eq.is_active(), 0.0, 0.0),
                    "multiband_compressor" => (self.dynamics.is_active(), 0.0, 0.0),
                    "convolution" => {
                        if self.convolution.is_active() {
                            let latency_ms = self.convolution.engine.latency_ms();
                            let ir_len = self.convolution.engine.num_partitions()
                                * self.convolution.engine.block_size();
                            let ir_len_ms = ir_len as f32 / self.sample_rate * 1000.0;
                            (true, latency_ms, (ir_len_ms - latency_ms).max(0.0))
                        } else {
                            (false, 0.0, 0.0)
                        }
                    }
                    "balance" => (self.balance.is_active(), 0.0, 0.0),
                    "crossfeed" => {
                        if self.crossfeed.is_active() {
                            let d = self.crossfeed.crossfeed.latency_ms();
                            (true, d, d)
                        } else {
                            (false, 0.0, 0.0)
                        }
                    }
                    "stereo_enhancer" => (self.stereo.is_active(), 0.0, 0.0),
                    "timestretch" => {
                        let active = self.timestretch.is_active();
                        let latency = if active {
                            self.timestretch.stretcher.latency_ms()
                        } else {
                            0.0
                        };
                        (active, latency, 0.0)
                    }
                    "volume" => (self.volume.is_active(), 0.0, 0.0),
                    "seek_fade" => (self.seek_fade.is_active(), 0.0, 0.0),
                    "limiter" => {
                        let active = self.limiter.is_active();
                        let lookahead = if active {
                            self.limiter.limiter.lookahead_ms()
                        } else {
                            0.0
                        };
                        let tail = if active {
                            self.limiter.limiter.release_ms()
                        } else {
                            0.0
                        };
                        (active, lookahead, tail)
                    }
                    "resampler" | "dither" => (false, 0.0, 0.0),
                    _ => (false, 0.0, 0.0),
                }
            };
            nodes.push(DspNodeInfo {
                name: cap.name,
                active,
                latency_ms,
                tail_ms,
            });
        }
        nodes
    }

    /// Total deterministic graph latency in milliseconds (output domain).
    pub fn total_latency_ms(&self) -> f32 {
        if self.bit_perfect || self.dop_bypass {
            return 0.0;
        }
        let mut total = 0.0;
        if self.crossfeed.is_active() {
            total += self.crossfeed.crossfeed.latency_ms();
        }
        if self.timestretch.is_active() {
            total += self.timestretch.stretcher.latency_ms();
        }
        if self.limiter.is_active() {
            total += self.limiter.limiter.lookahead_ms() + self.limiter.limiter.detector_delay_ms();
        }
        if self.convolution.is_active() {
            total += self.convolution.engine.latency_ms();
        }
        total
    }
}
