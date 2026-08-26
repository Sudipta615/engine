//! Construction & configuration for [`DspGraph`].
//!
//! Phase 2: the arena + plans now live in a swappable [`swap::GraphGeneration`].
//! [`GraphGeneration::build`] constructs a fresh generation on the control
//! path (allocation is fine there); [`DspGraph::from_config`] installs it as
//! the initial active generation, and [`DspGraph::reconfigure`] builds a new
//! one and publishes it for the audio thread to swap in at the next block
//! boundary — the live-reconfig entry point.

use super::*;
use std::sync::Arc;

/// Access a typed node inside a generation being configured.
macro_rules! gen_node {
    ($gen:expr, $slot:expr, $variant:ident) => {
        match &mut $gen.nodes[$slot] {
            GraphNode::$variant(n) => n,
            _ => unreachable!(
                "generation arena slot {} holds {}",
                $slot,
                stringify!($variant)
            ),
        }
    };
}

impl GraphGeneration {
    /// Construct a fresh generation from config: build the canonical node
    /// arena, compile the plans, apply the config, and replay the given
    /// user state (volume target / balance / speed / fade ramp) so a reconfig
    /// does not snap the listener's settings. The state is an immutable
    /// snapshot — the builder does not need (or touch) a live control bus.
    pub(super) fn build_with_state(
        config: &EngineConfig,
        sample_rate: f32,
        layout: &ChannelLayout,
        user: UserState,
    ) -> Box<GraphGeneration> {
        let num_bands = config.eq.bands.len().max(10);
        let mut timestretch = TimeStretchNode::new(sample_rate);
        timestretch
            .stretcher
            .set_quality(config.timestretch_quality);

        // Build the node arena. Order MUST match the `node_id` slot table in
        // mod.rs — the execution plans reference stages by index.
        let nodes = vec![
            GraphNode::OutPreamp(GainNode::new(
                "out_preamp",
                "pre-mix",
                PREAMP_RAMP_DURATION_MS,
                sample_rate,
            )),
            GraphNode::OutLoudness(LoudnessNode::new("out_loudness", "pre-mix", sample_rate)),
            GraphNode::InPreamp(GainNode::new(
                "in_preamp",
                "pre-mix",
                PREAMP_RAMP_DURATION_MS,
                sample_rate,
            )),
            GraphNode::InLoudness(LoudnessNode::new("in_loudness", "pre-mix", sample_rate)),
            GraphNode::Eq(EqNode::new(num_bands, sample_rate)),
            GraphNode::Dynamics(DynamicsNode::new(sample_rate)),
            GraphNode::Convolution(ConvolutionNode::new(sample_rate, 8192)),
            GraphNode::Balance(BalanceNode::new()),
            GraphNode::Crossfeed(CrossfeedNode::new(sample_rate)),
            GraphNode::Stereo(StereoNode::new()),
            GraphNode::TimeStretch(timestretch),
            GraphNode::Volume(GainNode::new(
                "volume",
                "post-mix",
                user.volume_fade_ms,
                sample_rate,
            )),
            GraphNode::SeekFade(SeekFadeNode::new(config.seek_fade_ms as f32, sample_rate)),
            GraphNode::Routing(RoutingNode::new(sample_rate)),
            GraphNode::Resampler(ResamplerNode::new(sample_rate, sample_rate)),
            GraphNode::Limiter(LimiterNode::new(sample_rate)),
            GraphNode::Dither(DitherNode::new(sample_rate)),
        ];

        // Arena-order contract: every `node_id` slot must hold the node kind
        // its table entry claims. Debug-only; also keeps the slot constants
        // referenced so the table cannot silently drift from the arena.
        debug_assert!(matches!(
            nodes[node_id::OUT_PREAMP],
            GraphNode::OutPreamp(_)
        ));
        debug_assert!(matches!(
            nodes[node_id::OUT_LOUDNESS],
            GraphNode::OutLoudness(_)
        ));
        debug_assert!(matches!(nodes[node_id::IN_PREAMP], GraphNode::InPreamp(_)));
        debug_assert!(matches!(
            nodes[node_id::IN_LOUDNESS],
            GraphNode::InLoudness(_)
        ));
        debug_assert!(matches!(nodes[node_id::EQ], GraphNode::Eq(_)));
        debug_assert!(matches!(nodes[node_id::DYNAMICS], GraphNode::Dynamics(_)));
        debug_assert!(matches!(
            nodes[node_id::CONVOLUTION],
            GraphNode::Convolution(_)
        ));
        debug_assert!(matches!(nodes[node_id::BALANCE], GraphNode::Balance(_)));
        debug_assert!(matches!(nodes[node_id::CROSSFEED], GraphNode::Crossfeed(_)));
        debug_assert!(matches!(nodes[node_id::STEREO], GraphNode::Stereo(_)));
        debug_assert!(matches!(
            nodes[node_id::TIMESTRETCH],
            GraphNode::TimeStretch(_)
        ));
        debug_assert!(matches!(nodes[node_id::VOLUME], GraphNode::Volume(_)));
        debug_assert!(matches!(nodes[node_id::SEEK_FADE], GraphNode::SeekFade(_)));
        debug_assert!(matches!(nodes[node_id::ROUTING], GraphNode::Routing(_)));
        debug_assert!(matches!(nodes[node_id::RESAMPLER], GraphNode::Resampler(_)));
        debug_assert!(matches!(nodes[node_id::LIMITER], GraphNode::Limiter(_)));
        debug_assert!(matches!(nodes[node_id::DITHER], GraphNode::Dither(_)));

        let mut gen = GraphGeneration {
            node_ids: GraphGeneration::canonical_ids(nodes.len()),
            plans: PlanSet::compile(),
            nodes,
        };
        gen.apply_config(config, sample_rate, layout);

        // User-state replay: a fresh generation inherits the listener's
        // volume / balance / speed from the control bus (seeded with defaults
        // at construction, mirroring the Phase-1 semantics where volume is
        // user state that survives track changes).
        gen_node!(gen, node_id::VOLUME, Volume)
            .processor
            .set_gain(user.volume);
        gen_node!(gen, node_id::BALANCE, Balance).set_balance(user.balance);
        gen_node!(gen, node_id::TIMESTRETCH, TimeStretch)
            .stretcher
            .set_speed(user.speed);

        Box::new(gen)
    }

    /// Public constructor for the control side: build a fresh generation
    /// from config with DEFAULT user state (unity volume / balance / speed,
    /// the config's volume-fade ramp).
    ///
    /// The production path ([`DspGraph::reconfigure`]) inherits live user
    /// state from the control bus instead; this entry point is for hosts and
    /// tests that want a clean configuration to publish. No control bus is
    /// created, so building is cheap.
    pub fn from_config(
        config: &EngineConfig,
        sample_rate: f32,
        layout: &ChannelLayout,
    ) -> Box<GraphGeneration> {
        Self::build_with_state(
            config,
            sample_rate,
            layout,
            UserState {
                volume_fade_ms: config.volume_fade_ms as f32,
                ..UserState::default()
            },
        )
    }

    /// Sync a generation's nodes from an [`EngineConfig`]. Control path only.
    fn apply_config(&mut self, config: &EngineConfig, sample_rate: f32, layout: &ChannelLayout) {
        {
            let eq = gen_node!(self, node_id::EQ, Eq);
            eq.eq.set_enabled(config.eq.enabled);
            eq.eq.set_preamp_db(config.eq.preamp_db);
            eq.eq.set_post_gain_db(config.eq.post_gain_db);
            eq.eq.set_headroom_db(config.eq.headroom_db);
        }

        {
            let eq = gen_node!(self, node_id::EQ, Eq);
            for (i, band_cfg) in config.eq.bands.iter().enumerate() {
                if i >= eq.eq.num_bands() {
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
                eq.eq.set_band(
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
            eq.eq.set_auto_headroom(config.eq.auto_headroom);
        }

        let mode = match config.loudness.mode {
            ConfigLoudnessMode::Off => crate::dsp::LoudnessMode::Off,
            ConfigLoudnessMode::TrackReplayGain => crate::dsp::LoudnessMode::TrackReplayGain,
            ConfigLoudnessMode::AlbumReplayGain => crate::dsp::LoudnessMode::AlbumReplayGain,
            ConfigLoudnessMode::EbuR128 => crate::dsp::LoudnessMode::EbuR128,
        };
        {
            let loud = gen_node!(self, node_id::OUT_LOUDNESS, OutLoudness);
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
        {
            let loud = gen_node!(self, node_id::IN_LOUDNESS, InLoudness);
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

        {
            let limiter = gen_node!(self, node_id::LIMITER, Limiter);
            limiter.limiter.set_enabled(config.limiter.enabled);
            limiter.limiter.set_lookahead(config.limiter.lookahead_ms);
            limiter.limiter.set_attack(config.limiter.attack_ms);
            limiter.limiter.set_release(config.limiter.release_ms);
            limiter.limiter.set_ceiling_db(config.limiter.ceiling_db);
            if config.limiter.soft_clip {
                limiter
                    .limiter
                    .set_mode(crate::dsp::limiter::LimiterMode::Saturate);
            } else {
                limiter
                    .limiter
                    .set_mode(crate::dsp::limiter::LimiterMode::Transparent);
            }
        }

        {
            let stereo = gen_node!(self, node_id::STEREO, Stereo);
            stereo.enhancer.set_enabled(config.stereo_enhancer.enabled);
            stereo.enhancer.set_width(config.stereo_enhancer.width);
        }

        {
            let crossfeed = gen_node!(self, node_id::CROSSFEED, Crossfeed);
            crossfeed.crossfeed.set_enabled(config.crossfeed.enabled);
            crossfeed.crossfeed.set_custom_params(
                config.crossfeed.custom_freq,
                config.crossfeed.custom_q,
                config.crossfeed.custom_delay_ms,
            );
            crossfeed.crossfeed.set_profile(config.crossfeed.profile);
        }

        {
            let dynamics = gen_node!(self, node_id::DYNAMICS, Dynamics);
            dynamics
                .compressor
                .set_enabled(config.multiband_compressor.enabled);
            dynamics.compressor.set_band_params(
                0,
                config.multiband_compressor.low_band.threshold_db,
                config.multiband_compressor.low_band.ratio,
                config.multiband_compressor.low_band.attack_ms,
                config.multiband_compressor.low_band.release_ms,
                config.multiband_compressor.low_band.makeup_gain_db,
            );
            dynamics.compressor.set_band_params(
                1,
                config.multiband_compressor.mid_band.threshold_db,
                config.multiband_compressor.mid_band.ratio,
                config.multiband_compressor.mid_band.attack_ms,
                config.multiband_compressor.mid_band.release_ms,
                config.multiband_compressor.mid_band.makeup_gain_db,
            );
            dynamics.compressor.set_band_params(
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
                dynamics.compressor.set_band_features(
                    band,
                    cfg.knee_db,
                    cfg.detector,
                    cfg.stereo_link,
                );
            }
        }

        {
            let conv = gen_node!(self, node_id::CONVOLUTION, Convolution);
            if config.convolution.enabled {
                conv.engine.set_enabled(true);
                conv.engine.set_wet_mix(config.convolution.wet_mix);
                if let Some(ref ir_path) = config.convolution.ir_path {
                    let path = std::path::Path::new(ir_path);
                    if path.exists() {
                        let _ = conv.engine.load_ir_from_file(path);
                    }
                }
            } else {
                conv.engine.set_enabled(false);
            }
        }

        {
            let routing = gen_node!(self, node_id::ROUTING, Routing);
            routing
                .trimmer
                .set_config(&config.channel_trim, sample_rate);
            routing
                .trimmer
                .set_channel_eq(&config.channel_eq, sample_rate);
            routing
                .trimmer
                .set_bass_management(&config.bass_management, sample_rate);
            routing.trimmer.set_routing(&config.channel_routing);
        }

        {
            let mut lfe_config = config.lfe.clone();
            if config.bass_management.enabled
                && lfe_config.crossover_hz.is_none()
                && lfe_config.enabled
            {
                lfe_config.crossover_hz = Some(config.bass_management.crossover_hz);
            }
            let routing = gen_node!(self, node_id::ROUTING, Routing);
            routing.trimmer.set_lfe(&lfe_config);
            let lfe_channels: Vec<usize> = layout
                .channel_ids()
                .iter()
                .enumerate()
                .filter(|(_, id)| **id == crate::decode::ChannelId::Lfe)
                .map(|(i, _)| i)
                .collect();
            routing.trimmer.set_lfe_channels(lfe_channels);
        }

        // Low-power mode disables the expensive nodes (folded in from the
        // former `apply_performance_mode`).
        if config.performance_mode == PerformanceMode::LowPower {
            let stereo = gen_node!(self, node_id::STEREO, Stereo);
            stereo.enhancer.set_enabled(false);
            let crossfeed = gen_node!(self, node_id::CROSSFEED, Crossfeed);
            crossfeed.crossfeed.set_enabled(false);
            let conv = gen_node!(self, node_id::CONVOLUTION, Convolution);
            conv.engine.set_enabled(false);
            let limiter = gen_node!(self, node_id::LIMITER, Limiter);
            limiter.limiter.enable_true_peak(false);
        }
    }
}

impl DspGraph {
    /// Construct a new DSP Graph from an [`EngineConfig`] and sample rate.
    pub fn from_config(config: &EngineConfig, sample_rate: f32) -> Self {
        let bus = Arc::new(ControlBus::new(config.volume_fade_ms as f32));
        let active = GraphGeneration::build_with_state(
            config,
            sample_rate,
            &ChannelLayout::Stereo,
            UserState {
                volume_fade_ms: config.volume_fade_ms as f32,
                ..UserState::default()
            },
        );

        Self {
            active,
            bus,
            multichannel_layout: ChannelLayout::Stereo,
            sample_rate,
            speed: 1.0,
            volume_fade_ms: config.volume_fade_ms as f32,
            precision_mode: config.precision_mode,
            performance_mode: config.performance_mode,
            bit_perfect: false,
            dop_bypass: false,
            scratch: GraphScratch::new(),
        }
    }

    /// Apply a config to the active generation directly (control path; used
    /// at construction and by single-threaded callers). For live reconfig
    /// during playback, use [`Self::reconfigure`], which swaps glitch-free.
    pub fn apply_config(&mut self, config: &EngineConfig) {
        self.active
            .apply_config(config, self.sample_rate, &self.multichannel_layout);
        self.precision_mode = config.precision_mode;
        self.performance_mode = config.performance_mode;
        self.volume_fade_ms = config.volume_fade_ms as f32;
    }

    /// Live reconfiguration: build a fresh generation from `config` (with the
    /// sticky user state replayed) and publish it for the audio thread to
    /// swap in at the next block boundary. Control path — safe to call while
    /// the graph is processing on another thread via the returned handle's
    /// [`GraphControlHandle::publish_generation`]; here it also syncs the
    /// shell-level mode fields.
    pub fn reconfigure(&mut self, config: &EngineConfig) {
        let sample_rate = self.sample_rate;
        let layout = self.multichannel_layout.clone();
        let gen =
            GraphGeneration::build_with_state(config, sample_rate, &layout, self.bus.snapshot());
        self.precision_mode = config.precision_mode;
        self.performance_mode = config.performance_mode;
        self.volume_fade_ms = config.volume_fade_ms as f32;
        self.control_handle().publish_generation(gen);
    }
}
