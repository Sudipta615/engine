use crate::dsp::{
    graph::node::DspNode,
    loudness::LoudnessNormalizer,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// Loudness normalization node (EBU R128 / ReplayGain / Smart Normalization).
pub struct LoudnessNode {
    pub name: &'static str,
    pub position: &'static str,
    pub normalizer: LoudnessNormalizer,
}

impl LoudnessNode {
    pub fn new(name: &'static str, position: &'static str, sample_rate: f32) -> Self {
        Self {
            name,
            position,
            normalizer: LoudnessNormalizer::new(sample_rate),
        }
    }
}

impl DspNode for LoudnessNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: self.name,
            channel_support: StageChannelSupport::AllChannels,
            position: self.position,
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::F64,
        }
    }

    fn is_active(&self) -> bool {
        self.normalizer.is_enabled()
    }

    fn reset(&mut self) {
        self.normalizer.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.normalizer.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        if channels == 2 {
            let (l, r) = planes.split_at_mut(1);
            self.normalizer.process_block(l[0], r[0]);
        } else {
            let (first, rest) = planes.split_at_mut(1);
            if let Some(second) = rest.first_mut() {
                for (l, r) in first[0].iter_mut().zip(second.iter_mut()) {
                    let (ol, or) = self.normalizer.process(*l, *r);
                    *l = ol;
                    *r = or;
                }
            } else {
                for l in first[0].iter_mut() {
                    let (ol, _) = self.normalizer.process(*l, *l);
                    *l = ol;
                }
            }
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        if channels == 2 {
            let (l, r) = planes.split_at_mut(1);
            self.normalizer.process_block_f64(l[0], r[0]);
        } else {
            let (first, rest) = planes.split_at_mut(1);
            if let Some(second) = rest.first_mut() {
                for (l, r) in first[0].iter_mut().zip(second.iter_mut()) {
                    let (ol, or) = self.normalizer.process_f64(*l, *r);
                    *l = ol;
                    *r = or;
                }
            } else {
                for l in first[0].iter_mut() {
                    let (ol, _) = self.normalizer.process_f64(*l, *l);
                    *l = ol;
                }
            }
        }
    }
}
