use crate::dsp::{
    crossfeed::Crossfeed,
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// Crossfeed node for natural headphone spatialization.
pub struct CrossfeedNode {
    pub crossfeed: Crossfeed,
    pub sample_rate: f32,
}

impl CrossfeedNode {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            crossfeed: Crossfeed::new(sample_rate),
            sample_rate,
        }
    }
}

impl DspNode for CrossfeedNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "crossfeed",
            channel_support: StageChannelSupport::StereoOnly,
            position: "post-mix",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.crossfeed.is_enabled()
    }

    fn latency_samples(&self) -> usize {
        if self.is_active() {
            (self.crossfeed.latency_ms() * 0.001 * self.sample_rate).round() as usize
        } else {
            0
        }
    }

    fn tail_samples(&self) -> usize {
        self.latency_samples()
    }

    fn reset(&mut self) {
        self.crossfeed.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.sample_rate = sample_rate;
        self.crossfeed.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.crossfeed.process_block(front[0], rest[0]);
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.crossfeed.process_block_f64(front[0], rest[0]);
    }
}
