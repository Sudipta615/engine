use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::decode::ChannelLayout;
use crate::dsp::{
    channel_trim::ChannelTrimmer,
    convolution::ConvolutionEngine,
    crossfade::TrackMixer,
    crossfeed::Crossfeed,
    equalizer::{EqBandParams, EqFilterType, ParametricEq, MAX_EQ_BANDS},
    gain::{FadeProcessor, GainProcessor},
    limiter::LookaheadLimiter,
    loudness::{LoudnessMode, LoudnessNormalizer},
    multiband_compressor::MultibandCompressor,
    stereo::StereoEnhancer,
    timestretch::TimeStretcher,
};
pub use config::PrecisionMode;
use config::{EngineConfig, LoudnessMode as ConfigLoudnessMode, PerformanceMode};

mod controls;
mod format;
mod process;
#[cfg(test)]
mod tests;

pub use format::{
    BitPerfectReport, BitPerfectResult, EngineStats, LatencyReport, OutputSampleFormat, VolumePath,
};

// ── Stage-capability declarations ────────────────────────────────────────────

/// Channel capability of a DSP stage: whether it can process any channel
/// count or is wired stereo-only (in the `>2`-channel path such stages run on
/// the front L/R pair only, which is a *documented* limitation, never a
/// silent channel drop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageChannelSupport {
    /// The stage is channel-agnostic and runs on every channel.
    AllChannels,
    /// The stage owns stereo-linked or stereo-only state; in the
    /// multichannel passthrough path it processes the front L/R pair only.
    StereoOnly,
}

/// Precision requirement of a DSP stage (spec §24). `Any` means the stage
/// accepts the pipeline's native f32/f64 stream without a precision
/// preference of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagePrecision {
    /// The stage prefers or requires f32 (e.g. the WSOLA correlation core).
    F32,
    /// The stage prefers or requires f64 (e.g. R128 loudness measurement).
    F64,
    /// The stage is precision-agnostic; it follows the pipeline's f32/f64
    /// signal path.
    Any,
}

/// One row of the DSP graph capability table (spec §24: every node describes
/// channel requirements, precision, statefulness, realtime safety,
/// bit-perfect compatibility, and sample-rate sensitivity).
#[derive(Debug, Clone, Copy)]
pub struct DspStageCapability {
    /// Stable stage identifier (also the config key, where one exists).
    pub name: &'static str,
    pub channel_support: StageChannelSupport,
    /// Where the stage sits in the signal path.
    pub position: &'static str,
    /// Whether the stage keeps streaming state between blocks (vs. a pure
    /// per-sample function of its input).
    pub stateful: bool,
    /// Whether the stage's steady-state processing is allocation-free and
    /// lock-free (safe to run on the realtime audio thread).
    pub realtime_safe: bool,
    /// Whether activating this stage preserves bit-perfect transport.
    /// False for every sample-altering stage (EQ, volume, dither, SRC, ...);
    /// true only for stages that are bypassed in bit-perfect mode or that
    /// cannot change sample values by construction.
    pub bit_perfect_compatible: bool,
    /// Whether the stage's behavior depends on the sample rate (coefficient
    /// tables, delay lines, time constants).
    pub sample_rate_sensitive: bool,
    /// Precision requirement / preference of the stage.
    pub precision: StagePrecision,
}

/// The authoritative stage capability table (§5): every DSP graph stage with
/// its channel capability and position in the chain. Exposed so consumers
/// (UI, docs, diagnostics) can reason about what a `>2`-channel stream
/// actually passes through without duplicating the knowledge in code.
#[doc = ""]
/// One live row of the DSP graph: static capability metadata merged with
/// the stage's current runtime state (spec §19, §24).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DspNodeInfo {
    /// Stage identifier (matches [`DspStageCapability::name`]).
    pub name: &'static str,
    /// Whether the stage is currently in the signal path (enabled, not
    /// bypassed by bit-perfect/DoP mode, and not folded into another node).
    pub active: bool,
    /// Deterministic latency this stage adds to the audio path (ms), 0 when
    /// inactive.
    pub latency_ms: f32,
    /// Tail: audio the stage still emits after its input stops (ms), 0 when
    /// inactive.
    pub tail_ms: f32,
}

pub static DSP_STAGE_CAPABILITIES: &[DspStageCapability] = &[
    DspStageCapability {
        name: "channel_trim",
        channel_support: StageChannelSupport::AllChannels,
        position: "pre-mix (MC path only)",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "channel_eq",
        channel_support: StageChannelSupport::AllChannels,
        position: "pre-mix (MC path only)",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "bass_management",
        channel_support: StageChannelSupport::AllChannels,
        position: "pre-mix (MC path only)",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "channel_mix",
        channel_support: StageChannelSupport::AllChannels,
        position: "decode/output boundary",
        stateful: false,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: false,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "out_preamp",
        channel_support: StageChannelSupport::AllChannels,
        position: "pre-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "out_loudness",
        channel_support: StageChannelSupport::AllChannels,
        position: "pre-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::F64,
    },
    DspStageCapability {
        name: "mixer",
        channel_support: StageChannelSupport::StereoOnly,
        position: "pre-post-mix boundary",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "eq",
        channel_support: StageChannelSupport::StereoOnly,
        position: "post-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "multiband_compressor",
        channel_support: StageChannelSupport::StereoOnly,
        position: "post-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "convolution",
        channel_support: StageChannelSupport::StereoOnly,
        position: "post-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "balance",
        channel_support: StageChannelSupport::StereoOnly,
        position: "post-mix",
        stateful: false,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: false,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "crossfeed",
        channel_support: StageChannelSupport::StereoOnly,
        position: "post-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "stereo_enhancer",
        channel_support: StageChannelSupport::StereoOnly,
        position: "post-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "timestretch",
        channel_support: StageChannelSupport::StereoOnly,
        position: "post-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::F32,
    },
    DspStageCapability {
        name: "volume",
        channel_support: StageChannelSupport::AllChannels,
        position: "post-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "seek_fade",
        channel_support: StageChannelSupport::AllChannels,
        position: "post-mix",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "resampler",
        channel_support: StageChannelSupport::StereoOnly,
        position: "output domain",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::Any,
    },
    DspStageCapability {
        name: "limiter",
        channel_support: StageChannelSupport::AllChannels,
        position: "output domain",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: true,
        precision: StagePrecision::F32,
    },
    DspStageCapability {
        name: "dither",
        channel_support: StageChannelSupport::AllChannels,
        position: "output conversion",
        stateful: true,
        realtime_safe: true,
        bit_perfect_compatible: false,
        sample_rate_sensitive: false,
        precision: StagePrecision::Any,
    },
];

const VOLUME_RAMP_DURATION_MS: f32 = 10.0;
pub(crate) const PREAMP_RAMP_DURATION_MS: f32 = VOLUME_RAMP_DURATION_MS;

pub struct DspPipeline {
    // Pre-mix chain
    pub out_preamp: GainProcessor,
    pub out_loudness: LoudnessNormalizer,
    pub in_preamp: GainProcessor,
    pub in_loudness: LoudnessNormalizer,

    pub mixer: TrackMixer,

    // Post-mix chain
    pub eq: ParametricEq,
    pub multiband_compressor: MultibandCompressor,
    pub convolution: Box<ConvolutionEngine>,
    pub balance_gain_l: f32,
    pub balance_gain_r: f32,
    pub crossfeed: Crossfeed,
    pub stereo_enhancer: StereoEnhancer,
    pub timestretcher: TimeStretcher,
    pub volume: GainProcessor,
    pub seek_fade: FadeProcessor,

    // Final safety stage — runs in the output domain (after resampling and
    // crossfade mixing), not as part of the post-mix chain above.
    pub limiter: LookaheadLimiter,

    /// Multichannel management stage: per-channel gain/delay/polarity trim,
    /// routing, LFE management, mains high-pass, and per-channel EQ. Runs on
    /// **every** channel of the `>2`-channel passthrough path, before the
    /// pre-mix chain, and is bypassed in bit-perfect / DoP modes like every
    /// other user DSP stage.
    pub channel_trim: ChannelTrimmer,
    /// Active multichannel layout, used to derive LFE-role channel indices
    /// for [`Self::channel_trim`].
    multichannel_layout: ChannelLayout,

    sample_rate: f32,
    performance_mode: PerformanceMode,
    speed: f32,
    balance: f32,
    midside_eq_enabled: bool,
    volume_fade_ms: f32,
    /// DSP precision mode (f32 or f64 signal path).
    precision_mode: PrecisionMode,
    /// When true, all DSP stages are bypassed for transparent bit-perfect output.
    bit_perfect: bool,
    /// When true, the ENTIRE chain is bypassed — a pure passthrough used for
    /// DSD-over-PCM (DoP) bitstreams, where even volume/fade scaling would
    /// corrupt the 24-bit DoP words. Unlike `bit_perfect`, nothing is applied.
    dop_bypass: bool,
    /// Scratch buffers for promoting f32 blocks to f64 in Quality mode.
    /// Moved out of `self` (via `mem::take`) during block processing to avoid
    /// aliasing the `&mut self` chain calls; the allocation is retained across
    /// calls so the hot path stays allocation-free after warm-up.
    scratch_f64_l: Vec<f64>,
    scratch_f64_r: Vec<f64>,
    /// Multichannel de-interleave scratch (one plane per channel, up to
    /// [`MAX_CHANNELS`]). Reused across calls so the >2-channel path stays
    /// allocation-free after warm-up.
    scratch_mc: Vec<Vec<f32>>,
}

impl DspPipeline {
    pub fn from_config(config: &EngineConfig, sample_rate: f32) -> Self {
        let num_bands = config.eq.bands.len().max(10).min(MAX_EQ_BANDS);
        let eq = ParametricEq::new(num_bands, sample_rate);
        let loudness_out = LoudnessNormalizer::new(sample_rate);
        let loudness_in = LoudnessNormalizer::new(sample_rate);
        let limiter = LookaheadLimiter::new(sample_rate);
        let stereo_enhancer = StereoEnhancer::new();
        let crossfeed = Crossfeed::new(sample_rate);
        let multiband_compressor = MultibandCompressor::new(sample_rate);
        let convolution = ConvolutionEngine::new(sample_rate, 8192);
        let mixer = TrackMixer::new(sample_rate);
        let mut timestretcher = TimeStretcher::new(sample_rate);
        // Apply the configured quality tier (window/hop/search parameters).
        // Control-path call at construction; buffers are already sized for
        // the highest tier so no realtime storage is reallocated.
        timestretcher.set_quality(config.timestretch_quality);
        let preamp_out = GainProcessor::with_ramp(1.0, PREAMP_RAMP_DURATION_MS, sample_rate);
        let preamp_in = GainProcessor::with_ramp(1.0, PREAMP_RAMP_DURATION_MS, sample_rate);
        let volume = GainProcessor::with_ramp(1.0, config.volume_fade_ms as f32, sample_rate);
        let seek_fade = FadeProcessor::new(config.seek_fade_ms as f32, sample_rate);
        let channel_trim = ChannelTrimmer::new(sample_rate);

        let mut pipeline = Self {
            out_preamp: preamp_out,
            out_loudness: loudness_out,
            in_preamp: preamp_in,
            in_loudness: loudness_in,
            eq,
            multiband_compressor,
            convolution: Box::new(convolution),
            balance_gain_l: 1.0,
            balance_gain_r: 1.0,
            crossfeed,
            stereo_enhancer,
            timestretcher,
            limiter,
            channel_trim,
            multichannel_layout: ChannelLayout::Stereo,
            volume,
            seek_fade,
            mixer,
            sample_rate,
            performance_mode: config.performance_mode,
            speed: 1.0,
            balance: 0.0,
            midside_eq_enabled: false,
            volume_fade_ms: config.volume_fade_ms as f32,
            precision_mode: config.precision_mode,
            bit_perfect: false,
            dop_bypass: false,
            scratch_f64_l: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_f64_r: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_mc: (0..MAX_CHANNELS)
                .map(|_| vec![0.0; MAX_AUDIO_BLOCK_FRAMES])
                .collect(),
        };

        pipeline.apply_config(config);
        pipeline
    }

    /// Sync DSP pipeline configuration from an [`EngineConfig`].
    pub fn apply_config(&mut self, config: &EngineConfig) {
        self.eq.set_enabled(config.eq.enabled);
        self.eq.set_preamp_db(config.eq.preamp_db);
        self.eq.set_post_gain_db(config.eq.post_gain_db);
        self.eq.set_headroom_db(config.eq.headroom_db);

        for (i, band_cfg) in config.eq.bands.iter().enumerate() {
            if i >= self.eq.num_bands() {
                break;
            }
            let filter_type = match band_cfg.filter_type {
                config::FilterType::Peaking => EqFilterType::Peaking,
                config::FilterType::LowShelf => EqFilterType::LowShelf,
                config::FilterType::HighShelf => EqFilterType::HighShelf,
                config::FilterType::LowPass => EqFilterType::LowPass,
                config::FilterType::HighPass => EqFilterType::HighPass,
                config::FilterType::Bandpass => EqFilterType::Bandpass,
                config::FilterType::Notch => EqFilterType::Notch,
                config::FilterType::AllPass => EqFilterType::AllPass,
            };
            self.eq.set_band(
                i,
                EqBandParams {
                    enabled: band_cfg.enabled,
                    filter_type,
                    frequency: band_cfg.frequency,
                    gain_db: band_cfg.gain_db,
                    q: band_cfg.q,
                },
            );
        }
        self.eq.set_auto_headroom(config.eq.auto_headroom);

        let mode = match config.loudness.mode {
            ConfigLoudnessMode::Off => LoudnessMode::Off,
            ConfigLoudnessMode::TrackReplayGain => LoudnessMode::TrackReplayGain,
            ConfigLoudnessMode::AlbumReplayGain => LoudnessMode::AlbumReplayGain,
            ConfigLoudnessMode::EbuR128 => LoudnessMode::EbuR128,
        };
        self.out_loudness.set_mode(mode);
        self.out_loudness
            .set_target_lufs(config.loudness.target_lufs);
        self.out_loudness.set_true_peak_guard(
            config.loudness.true_peak_guard,
            config.loudness.true_peak_dbtp,
        );
        self.out_loudness.set_gain_clamps(
            config.loudness.max_boost_db,
            config.loudness.max_attenuation_db,
        );
        self.in_loudness.set_mode(mode);
        self.in_loudness
            .set_target_lufs(config.loudness.target_lufs);
        self.in_loudness.set_true_peak_guard(
            config.loudness.true_peak_guard,
            config.loudness.true_peak_dbtp,
        );
        self.in_loudness.set_gain_clamps(
            config.loudness.max_boost_db,
            config.loudness.max_attenuation_db,
        );

        self.limiter.set_enabled(config.limiter.enabled);
        self.limiter.set_lookahead(config.limiter.lookahead_ms);
        self.limiter.set_attack(config.limiter.attack_ms);
        self.limiter.set_release(config.limiter.release_ms);
        self.limiter.set_ceiling_db(config.limiter.ceiling_db);
        if config.limiter.soft_clip {
            self.limiter
                .set_mode(crate::dsp::limiter::LimiterMode::Saturate);
        } else {
            self.limiter
                .set_mode(crate::dsp::limiter::LimiterMode::Transparent);
        }

        self.stereo_enhancer
            .set_enabled(config.stereo_enhancer.enabled);
        self.stereo_enhancer.set_width(config.stereo_enhancer.width);

        self.crossfeed.set_enabled(config.crossfeed.enabled);
        self.crossfeed.set_custom_params(
            config.crossfeed.custom_freq,
            config.crossfeed.custom_q,
            config.crossfeed.custom_delay_ms,
        );
        self.crossfeed.set_profile(config.crossfeed.profile);

        self.multiband_compressor
            .set_enabled(config.multiband_compressor.enabled);
        self.multiband_compressor.set_band_params(
            0,
            config.multiband_compressor.low_band.threshold_db,
            config.multiband_compressor.low_band.ratio,
            config.multiband_compressor.low_band.attack_ms,
            config.multiband_compressor.low_band.release_ms,
            config.multiband_compressor.low_band.makeup_gain_db,
        );
        self.multiband_compressor.set_band_params(
            1,
            config.multiband_compressor.mid_band.threshold_db,
            config.multiband_compressor.mid_band.ratio,
            config.multiband_compressor.mid_band.attack_ms,
            config.multiband_compressor.mid_band.release_ms,
            config.multiband_compressor.mid_band.makeup_gain_db,
        );
        self.multiband_compressor.set_band_params(
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
            self.multiband_compressor.set_band_features(
                band,
                cfg.knee_db,
                cfg.detector,
                cfg.stereo_link,
            );
        }

        if config.convolution.enabled {
            self.convolution.set_enabled(true);
            self.convolution.set_wet_mix(config.convolution.wet_mix);
            if let Some(ref ir_path) = config.convolution.ir_path {
                let path = std::path::Path::new(ir_path);
                if path.exists() {
                    let _ = self.convolution.load_ir_from_file(path);
                }
            }
        } else {
            self.convolution.set_enabled(false);
        }

        self.mixer.set_curve(config.crossfade.curve.into());
        self.mixer
            .set_duration_ms(config.crossfade.duration_ms, self.sample_rate);
        self.mixer.set_enabled(config.crossfade.enabled);

        self.channel_trim
            .set_config(&config.channel_trim, self.sample_rate);
        self.channel_trim
            .set_channel_eq(&config.channel_eq, self.sample_rate);
        self.channel_trim
            .set_bass_management(&config.bass_management, self.sample_rate);
        self.channel_trim.set_routing(&config.channel_routing);
        // Bass management supplies the shared crossover when LFE is enabled
        // but has no explicit LFE cutoff. An explicit LFE value always wins,
        // so users can intentionally choose asymmetric crossover points.
        let mut lfe_config = config.lfe.clone();
        if config.bass_management.enabled && lfe_config.crossover_hz.is_none() && lfe_config.enabled
        {
            lfe_config.crossover_hz = Some(config.bass_management.crossover_hz);
        }
        self.channel_trim.set_lfe(&lfe_config);
        self.channel_trim.set_lfe_channels(
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

    /// Declare the active multichannel layout. Used to derive LFE-role
    /// channel indices for the channel-trim stage (LFE gain is applied only
    /// to channels whose semantic role is LFE).
    pub fn set_multichannel_layout(&mut self, layout: &ChannelLayout) {
        self.multichannel_layout = layout.clone();
        self.channel_trim.set_lfe_channels(
            layout
                .channel_ids()
                .iter()
                .enumerate()
                .filter(|(_, id)| **id == crate::decode::ChannelId::Lfe)
                .map(|(i, _)| i)
                .collect(),
        );
    }

    fn apply_performance_mode(&mut self) {
        if self.performance_mode == PerformanceMode::LowPower {
            // Battery-saver mode: disable the most CPU-hungry DSP stages.
            // Dither is NOT disabled here — the actual quantization-time
            // dither happens in the output callbacks and is driven by
            // `config.dither_enabled`, which the engine owns.
            self.stereo_enhancer.set_enabled(false);
            self.crossfeed.set_enabled(false);
            self.convolution.set_enabled(false);
            self.limiter.enable_true_peak(false);
        }
    }
}
