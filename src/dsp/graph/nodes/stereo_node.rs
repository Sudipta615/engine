use crate::dsp::{
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
    stereo::StereoEnhancer,
};

/// Stereo width enhancer node (Mid/Side stereo image processing).
pub struct StereoNode {
    pub enhancer: StereoEnhancer,
}

impl Default for StereoNode {
    fn default() -> Self {
        Self::new()
    }
}

impl StereoNode {
    pub fn new() -> Self {
        Self {
            enhancer: StereoEnhancer::new(),
        }
    }
}

impl DspNode for StereoNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "stereo_enhancer",
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
        self.enhancer.is_enabled()
    }

    fn reset(&mut self) {
        self.enhancer.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.enhancer.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.enhancer.process_block(front[0], rest[0]);
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.enhancer.process_block_f64(front[0], rest[0]);
    }
}
