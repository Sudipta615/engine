//! Multichannel command handlers — channel mix, policy, trim, routing,
//! channel EQ, LFE, bass management.

use log::info;

use super::super::AudioEngine;

impl AudioEngine {
    pub(super) fn handle_set_channel_mix(&mut self, cfg: config::ChannelMixConfig) {
        info!(
            "Channel mix config updated: enabled={}, template={:?}",
            cfg.enabled, cfg.template
        );
        self.config.channel_mix = cfg;
    }

    pub(super) fn handle_set_channel_policy(&mut self, policy: config::ChannelPolicy) {
        info!("Channel policy set to {:?}", policy);
        self.config.channel_policy = policy;
    }

    pub(super) fn handle_set_channel_trim(&mut self, cfg: config::ChannelTrimConfig) {
        info!(
            "Channel trim config updated: enabled={}, entries={}",
            cfg.enabled,
            cfg.entries.len()
        );
        self.config.channel_trim = cfg.clone();
        let sr = self.graph.sample_rate();
        self.graph.routing_mut().trimmer.set_config(&cfg, sr);
    }

    pub(super) fn handle_set_channel_routing(&mut self, cfg: config::ChannelRoutingConfig) {
        info!("Channel routing config updated: enabled={}", cfg.enabled);
        self.config.channel_routing = cfg.clone();
        self.graph.routing_mut().trimmer.set_routing(&cfg);
    }

    pub(super) fn handle_set_channel_eq(&mut self, cfg: config::ChannelEqConfig) {
        info!(
            "Channel EQ config updated: enabled={}, entries={}",
            cfg.enabled,
            cfg.entries.len()
        );
        self.config.channel_eq = cfg.clone();
        let sr = self.graph.sample_rate();
        self.graph.routing_mut().trimmer.set_channel_eq(&cfg, sr);
    }

    pub(super) fn handle_set_lfe_config(&mut self, cfg: config::LfeConfig) {
        info!(
            "LFE config updated: enabled={}, gain_db={:.1}, crossover={:?}",
            cfg.enabled, cfg.gain_db, cfg.crossover_hz
        );
        self.config.lfe = cfg.clone();
        let mut lfe = cfg;
        if self.config.bass_management.enabled && lfe.crossover_hz.is_none() && lfe.enabled {
            lfe.crossover_hz = Some(self.config.bass_management.crossover_hz);
        }
        self.graph.routing_mut().trimmer.set_lfe(&lfe);
    }

    pub(super) fn handle_set_bass_management(&mut self, cfg: config::BassManagementConfig) {
        info!(
            "Bass management updated: enabled={}, crossover={}Hz",
            cfg.enabled, cfg.crossover_hz
        );
        self.config.bass_management = cfg.clone();
        let sr = self.graph.sample_rate();
        self.graph
            .routing_mut()
            .trimmer
            .set_bass_management(&cfg, sr);
        let mut lfe = self.config.lfe.clone();
        if cfg.enabled && lfe.crossover_hz.is_none() && lfe.enabled {
            lfe.crossover_hz = Some(cfg.crossover_hz);
        }
        self.graph.routing_mut().trimmer.set_lfe(&lfe);
    }
}
