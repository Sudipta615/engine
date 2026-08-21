use crate::dsp::{
    graph::node::DspNode,
    multiband_compressor::MultibandCompressor,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// Dynamics node (3-band multiband compressor with soft knee).
pub struct DynamicsNode {
    pub compressor: MultibandCompressor,
}

impl DynamicsNode {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            compressor: MultibandCompressor::new(sample_rate),
        }
    }
}

impl DspNode for DynamicsNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "multiband_compressor",
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
        self.compressor.is_enabled()
    }

    fn reset(&mut self) {
        self.compressor.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.compressor.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.compressor.process_block(front[0], rest[0]);
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.compressor.process_block_f64(front[0], rest[0]);
    }
}
