//! Construction & configuration for [`DspGraph`].

use super::*;

impl DspGraph {
    /// Construct a new DSP Graph from an [`EngineConfig`] and sample rate.
    pub fn from_config(config: &EngineConfig, sample_rate: f32) -> Self {
        let num_bands = config.eq.bands.len().max(10);
        let mut timestretch = TimeStretchNode::new(sample_rate);
        timestretch
            .stretcher
            .set_quality(config.timestretch_quality);

        let mut graph = Self {
            out_preamp: GainNode::new(
                "out_preamp",
                "pre-mix",
                PREAMP_RAMP_DURATION_MS,
                sample_rate,
            ),
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
            volume: GainNode::new(
                "volume",
                "post-mix",
                config.volume_fade_ms as f32,
                sample_rate,
            ),
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
            ConfigLoudnessMode::Off => crate::dsp::LoudnessMode::Off,
            ConfigLoudnessMode::TrackReplayGain => crate::dsp::LoudnessMode::TrackReplayGain,
            ConfigLoudnessMode::AlbumReplayGain => crate::dsp::LoudnessMode::AlbumReplayGain,
            ConfigLoudnessMode::EbuR128 => crate::dsp::LoudnessMode::EbuR128,
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
        self.limiter
            .limiter
            .set_lookahead(config.limiter.lookahead_ms);
        self.limiter.limiter.set_attack(config.limiter.attack_ms);
        self.limiter.limiter.set_release(config.limiter.release_ms);
        self.limiter
            .limiter
            .set_ceiling_db(config.limiter.ceiling_db);
        if config.limiter.soft_clip {
            self.limiter
                .limiter
                .set_mode(crate::dsp::limiter::LimiterMode::Saturate);
        } else {
            self.limiter
                .limiter
                .set_mode(crate::dsp::limiter::LimiterMode::Transparent);
        }

        self.stereo
            .enhancer
            .set_enabled(config.stereo_enhancer.enabled);
        self.stereo.enhancer.set_width(config.stereo_enhancer.width);

        self.crossfeed
            .crossfeed
            .set_enabled(config.crossfeed.enabled);
        self.crossfeed.crossfeed.set_custom_params(
            config.crossfeed.custom_freq,
            config.crossfeed.custom_q,
            config.crossfeed.custom_delay_ms,
        );
        self.crossfeed
            .crossfeed
            .set_profile(config.crossfeed.profile);

        self.dynamics
            .compressor
            .set_enabled(config.multiband_compressor.enabled);
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
            self.convolution
                .engine
                .set_wet_mix(config.convolution.wet_mix);
            if let Some(ref ir_path) = config.convolution.ir_path {
                let path = std::path::Path::new(ir_path);
                if path.exists() {
                    let _ = self.convolution.engine.load_ir_from_file(path);
                }
            }
        } else {
            self.convolution.engine.set_enabled(false);
        }

        self.routing
            .trimmer
            .set_config(&config.channel_trim, self.sample_rate);
        self.routing
            .trimmer
            .set_channel_eq(&config.channel_eq, self.sample_rate);
        self.routing
            .trimmer
            .set_bass_management(&config.bass_management, self.sample_rate);
        self.routing.trimmer.set_routing(&config.channel_routing);

        let mut lfe_config = config.lfe.clone();
        if config.bass_management.enabled && lfe_config.crossover_hz.is_none() && lfe_config.enabled
        {
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
}
