use crate::dsp::{
    dither::{Dither, DitherType},
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// Dither and output quantization node.
pub struct DitherNode {
    pub dither: Dither,
}

impl DitherNode {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            dither: Dither::with_sample_rate(DitherType::Triangular, 16, sample_rate as u32),
        }
    }
}

impl DspNode for DitherNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "dither",
            channel_support: StageChannelSupport::AllChannels,
            position: "output conversion",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: false,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.dither.is_enabled()
    }

    fn reset(&mut self) {
        self.dither.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.dither = Dither::with_sample_rate(DitherType::Triangular, 16, sample_rate as u32);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        let left = &mut front[0];
        let right = &mut rest[0];
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or) = self.dither.process(left[i], right[i]);
            left[i] = ol;
            right[i] = or;
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        let left = &mut front[0];
        let right = &mut rest[0];
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or) = self.dither.process_f64(left[i], right[i]);
            left[i] = ol;
            right[i] = or;
        }
    }
}
