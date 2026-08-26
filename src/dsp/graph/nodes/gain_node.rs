use crate::dsp::{
    gain::{FadeProcessor, FadeState, GainProcessor},
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// Gain node for preamps and output volume control.
pub struct GainNode {
    pub name: &'static str,
    pub processor: GainProcessor,
    pub position: &'static str,
    pub ramp_duration_ms: f32,
}

impl GainNode {
    pub fn new(name: &'static str, position: &'static str, ramp_ms: f32, sample_rate: f32) -> Self {
        Self {
            name,
            position,
            ramp_duration_ms: ramp_ms,
            processor: GainProcessor::with_ramp(1.0, ramp_ms, sample_rate),
        }
    }
}

impl DspNode for GainNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: self.name,
            channel_support: StageChannelSupport::AllChannels,
            position: self.position,
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        (self.processor.current_gain() - 1.0).abs() > 1e-4
            || (self.processor.target_gain - 1.0).abs() > 1e-4
    }

    fn reset(&mut self) {
        self.processor.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        let slew = if self.ramp_duration_ms > 0.0 && sample_rate > 0.0 {
            1.0 / (self.ramp_duration_ms * 0.001 * sample_rate)
        } else {
            1.0
        };
        self.processor.set_slew_rate(slew);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        let frames = planes[0].len();
        if channels == 2 {
            let (l, r) = planes.split_at_mut(1);
            self.processor.process_block_stereo(l[0], r[0]);
        } else {
            for i in 0..frames {
                let g = self.processor.process_sample(1.0);
                for plane in planes.iter_mut() {
                    plane[i] *= g;
                }
            }
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        let frames = planes[0].len();
        if channels == 2 {
            let (l, r) = planes.split_at_mut(1);
            self.processor.process_block_stereo_f64(l[0], r[0]);
        } else {
            for i in 0..frames {
                let g = self.processor.process_sample(1.0) as f64;
                for plane in planes.iter_mut() {
                    plane[i] *= g;
                }
            }
        }
    }
}

/// Seek & transition fade node.
pub struct SeekFadeNode {
    pub fade: FadeProcessor,
}

impl SeekFadeNode {
    pub fn new(duration_ms: f32, sample_rate: f32) -> Self {
        Self {
            fade: FadeProcessor::new(duration_ms, sample_rate),
        }
    }
}

impl DspNode for SeekFadeNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "seek_fade",
            channel_support: StageChannelSupport::AllChannels,
            position: "post-mix",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.fade.state != FadeState::Idle || (self.fade.gain() - 1.0).abs() > 1e-4
    }

    fn reset(&mut self) {
        self.fade.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.fade.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        let frames = planes[0].len();
        if channels == 2 {
            let (l, r) = planes.split_at_mut(1);
            self.fade.process_block(l[0], r[0]);
        } else {
            for i in 0..frames {
                let (g, _) = self.fade.process(1.0, 1.0);
                for plane in planes.iter_mut() {
                    plane[i] *= g;
                }
            }
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        let frames = planes[0].len();
        if channels == 2 {
            let (l, r) = planes.split_at_mut(1);
            self.fade.process_block_f64(l[0], r[0]);
        } else {
            for i in 0..frames {
                let (g, _) = self.fade.process_f64(1.0, 1.0);
                for plane in planes.iter_mut() {
                    plane[i] *= g;
                }
            }
        }
    }
}

/// Balance node for stereo Left/Right panning.
pub struct BalanceNode {
    pub balance: f32,
    pub balance_gain_l: f32,
    pub balance_gain_r: f32,
}

impl BalanceNode {
    pub fn new() -> Self {
        Self {
            balance: 0.0,
            balance_gain_l: 1.0,
            balance_gain_r: 1.0,
        }
    }

    pub fn set_balance(&mut self, balance: f32) {
        self.balance = balance.clamp(-1.0, 1.0);
        if self.balance >= 0.0 {
            self.balance_gain_l = 1.0 - self.balance;
            self.balance_gain_r = 1.0;
        } else {
            self.balance_gain_l = 1.0;
            self.balance_gain_r = 1.0 + self.balance;
        }
    }
}

impl Default for BalanceNode {
    fn default() -> Self {
        Self::new()
    }
}

impl DspNode for BalanceNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "balance",
            channel_support: StageChannelSupport::StereoOnly,
            position: "post-mix",
            stateful: false,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: false,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.balance != 0.0
    }

    fn reset(&mut self) {}

    fn prepare(&mut self, _sample_rate: f32, _max_channels: usize) {}

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if planes.len() < 2 {
            return;
        }
        let bl = self.balance_gain_l;
        let br = self.balance_gain_r;
        let (l, r) = planes.split_at_mut(1);
        for (a, b) in l[0].iter_mut().zip(r[0].iter_mut()) {
            *a *= bl;
            *b *= br;
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if planes.len() < 2 {
            return;
        }
        let bl = self.balance_gain_l as f64;
        let br = self.balance_gain_r as f64;
        let (l, r) = planes.split_at_mut(1);
        for (a, b) in l[0].iter_mut().zip(r[0].iter_mut()) {
            *a *= bl;
            *b *= br;
        }
    }
}
