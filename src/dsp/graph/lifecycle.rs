//! Graph lifecycle: sample-rate updates, resets, mode toggles, and the small
//! getters/setters for [`DspGraph`].

use super::*;

impl DspGraph {
    /// Update sample rate across all graph nodes.
    pub fn update_sample_rate(&mut self, sample_rate: f32) {
        if self.sample_rate == sample_rate {
            return;
        }
        let old_rate = self.sample_rate;
        self.sample_rate = sample_rate;
        // The mix bus owns every input's pre-mix chain.
        self.mix_mut().prepare(sample_rate, MAX_CHANNELS);
        self.eq_mut().prepare(sample_rate, 2);
        self.dynamics_mut().prepare(sample_rate, 2);
        self.convolution_mut().prepare(sample_rate, 2);
        self.crossfeed_mut().prepare(sample_rate, 2);
        self.stereo_mut().prepare(sample_rate, 2);
        self.timestretch_mut().prepare(sample_rate, 2);
        self.volume_mut().prepare(sample_rate, MAX_CHANNELS);
        self.seek_fade_mut().prepare(sample_rate, MAX_CHANNELS);
        self.routing_mut().prepare(sample_rate, MAX_CHANNELS);
        self.limiter_mut().prepare(sample_rate, MAX_CHANNELS);
        self.dither_mut().prepare(sample_rate, MAX_CHANNELS);
        self.correction_mut().prepare(sample_rate, MAX_CHANNELS);
        // Phase 17: the spatial master re-prepares its head model on a rate
        // change (delay lines and filters are rate-dependent).
        self.spatial_mut().prepare(sample_rate, MAX_CHANNELS);
        // Rescale an in-progress bus transition without resetting it (the
        // envelope's normalized progress is preserved, mirroring
        // `TrackMixer::rescale_sample_rate`).
        self.mix_mut().rescale_sample_rate(old_rate, sample_rate);
    }

    /// Reset internal state across all nodes.
    ///
    /// Matches `DspPipeline::reset` semantics: volume is USER state, not
    /// filter state, so it is intentionally NOT reset here (a track change
    /// must not snap the listener's volume to unity). The graph's volume
    /// processor previously reset with the filters; the equivalence harness
    /// (`tests/fidelity/graph_pipeline_equivalence.rs`) pins this alignment.
    pub fn reset(&mut self) {
        self.mix_mut().reset();
        self.eq_mut().reset();
        self.dynamics_mut().reset();
        self.convolution_mut().reset();
        self.balance_mut().reset();
        self.crossfeed_mut().reset();
        self.stereo_mut().reset();
        self.timestretch_mut().reset();
        self.seek_fade_mut().reset();
        self.routing_mut().reset();
        self.limiter_mut().reset();
        self.dither_mut().reset();
        self.correction_mut().reset();
    }

    /// Reset filter state only without touching volume ramps.
    pub fn reset_filters_only(&mut self) {
        self.mix_mut().reset_filters_only();
        self.eq_mut().reset();
        self.dynamics_mut().reset();
        self.convolution_mut().reset();
        self.crossfeed_mut().reset();
        self.stereo_mut().reset();
        self.timestretch_mut().reset();
        self.routing_mut().reset();
        self.limiter_mut().reset();
        self.correction_mut().reset();
    }

    pub fn set_precision_mode(&mut self, mode: PrecisionMode) {
        self.precision_mode = mode;
    }

    pub fn precision_mode(&self) -> PrecisionMode {
        self.precision_mode
    }

    /// Toggle bit-perfect transport. Queued: the flag and its side effects
    /// (volume snap, seek-fade reset) apply at the next block boundary, so
    /// the call is safe from any thread that holds a control handle.
    pub fn set_bit_perfect(&self, enabled: bool) {
        self.control_handle()
            .enqueue(NodeId::SHELL.0, NodeCmd::SetBitPerfect(enabled));
    }

    pub fn is_bit_perfect(&self) -> bool {
        self.bit_perfect
    }

    pub fn set_dop_bypass(&self, enabled: bool) {
        self.control_handle()
            .enqueue(NodeId::SHELL.0, NodeCmd::SetDoPBypass(enabled));
    }

    pub fn is_dop_bypass(&self) -> bool {
        self.dop_bypass
    }

    pub fn set_multichannel_layout(&mut self, layout: &ChannelLayout) {
        self.multichannel_layout = layout.clone();
        self.routing_mut().trimmer.set_lfe_channels(
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

    pub fn set_speed(&self, speed: f32) {
        self.control_handle()
            .enqueue(NodeId::SHELL.0, NodeCmd::SetSpeed(speed));
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Capacity reserved for the f64 promotion scratch used by realtime
    /// block processing. Exposed for diagnostics and allocation regression
    /// tests; it must not change during playback (mirrors the pipeline's
    /// accessor).
    pub fn realtime_scratch_capacity(&self) -> usize {
        self.scratch
            .scratch_f64_l
            .capacity()
            .min(self.scratch.scratch_f64_r.capacity())
    }

    pub fn volume_fade_ms(&self) -> f32 {
        self.volume_fade_ms
    }

    pub fn set_volume_fade_ms(&mut self, ms: f32) {
        self.volume_fade_ms = ms;
        self.bus.set_user_fade_ms(ms);
    }
}
