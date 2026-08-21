use crate::dsp::{
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
    timestretch::TimeStretcher,
};

/// Time & Pitch stretching node (WSOLA correlation core).
pub struct TimeStretchNode {
    pub stretcher: TimeStretcher,
}

impl TimeStretchNode {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            stretcher: TimeStretcher::new(sample_rate),
        }
    }
}

impl DspNode for TimeStretchNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "timestretch",
            channel_support: StageChannelSupport::StereoOnly,
            position: "post-mix",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::F32,
        }
    }

    fn is_active(&self) -> bool {
        self.stretcher.is_enabled()
    }

    fn latency_samples(&self) -> usize {
        if self.is_active() {
            (self.stretcher.latency_ms() * 0.001 * self.stretcher.sample_rate()).round() as usize
        } else {
            0
        }
    }

    fn reset(&mut self) {
        self.stretcher.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.stretcher.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.stretcher.process_block(front[0], rest[0]);
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.stretcher.process_block_f64(front[0], rest[0]);
    }
}
