use crate::dsp::{
    equalizer::{ParametricEq, MAX_EQ_BANDS},
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// Equalizer node (Parametric EQ with Mid/Side support).
pub struct EqNode {
    pub eq: ParametricEq,
    pub midside_enabled: bool,
}

impl EqNode {
    pub fn new(num_bands: usize, sample_rate: f32) -> Self {
        let bands = num_bands.max(10).min(MAX_EQ_BANDS);
        Self {
            eq: ParametricEq::new(bands, sample_rate),
            midside_enabled: false,
        }
    }
}

impl DspNode for EqNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "eq",
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
        self.eq.is_enabled()
    }

    fn reset(&mut self) {
        self.eq.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.eq.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        let left = &mut front[0];
        let right = &mut rest[0];
        let n = left.len().min(right.len());

        if self.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.process(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.process_block(left, right);
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

        if self.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.process_f64(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.process_block_f64(left, right);
        }
    }
}
