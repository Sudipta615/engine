//! Graph lifecycle: sample-rate updates, resets, mode toggles, and the small
//! getters/setters for [`DspGraph`].

use super::*;

impl DspGraph {
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
}
