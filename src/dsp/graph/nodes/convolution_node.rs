use crate::dsp::{
    convolution::ConvolutionEngine,
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// FIR / Convolution node (Uniform partitioned FFT reverb/cabinet engine).
pub struct ConvolutionNode {
    pub engine: Box<ConvolutionEngine>,
}

impl ConvolutionNode {
    pub fn new(sample_rate: f32, max_ir_len: usize) -> Self {
        Self {
            engine: Box::new(ConvolutionEngine::new(sample_rate, max_ir_len)),
        }
    }
}

impl DspNode for ConvolutionNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "convolution",
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
        self.engine.is_enabled() && self.engine.is_ir_loaded()
    }

    fn latency_samples(&self) -> usize {
        if self.is_active() {
            self.engine.block_size()
        } else {
            0
        }
    }

    fn tail_samples(&self) -> usize {
        if self.is_active() {
            let ir_len = self.engine.num_partitions() * self.engine.block_size();
            ir_len.saturating_sub(self.latency_samples())
        } else {
            0
        }
    }

    fn reset(&mut self) {
        self.engine.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.engine.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.engine.process_block(front[0], rest[0]);
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        self.engine.process_block_f64(front[0], rest[0]);
    }
}
