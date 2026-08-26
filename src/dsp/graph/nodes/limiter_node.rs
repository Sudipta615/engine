use crate::dsp::{
    graph::node::DspNode,
    limiter::LookaheadLimiter,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// Limiter node (Lookahead safety limiter with True Peak FIR detection).
pub struct LimiterNode {
    pub limiter: LookaheadLimiter,
    pub sample_rate: f32,
}

impl LimiterNode {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            limiter: LookaheadLimiter::new(sample_rate),
            sample_rate,
        }
    }
}

impl DspNode for LimiterNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "limiter",
            channel_support: StageChannelSupport::AllChannels,
            position: "output domain",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::F32,
        }
    }

    fn is_active(&self) -> bool {
        self.limiter.is_enabled()
    }

    fn latency_samples(&self) -> usize {
        if self.is_active() {
            (self.limiter.lookahead_ms() * 0.001 * self.sample_rate).round() as usize
        } else {
            0
        }
    }

    fn tail_samples(&self) -> usize {
        if self.is_active() {
            (self.limiter.release_ms() * 0.001 * self.sample_rate).round() as usize
        } else {
            0
        }
    }

    fn reset(&mut self) {
        self.limiter.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.sample_rate = sample_rate;
        self.limiter.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        let channels = planes.len();
        if channels == 0 {
            return;
        }
        let frames = planes[0].len();
        if channels == 2 {
            let (front, rest) = planes.split_at_mut(1);
            self.limiter.process_block(front[0], rest[0]);
        } else {
            let mut in_s = [0.0f64; crate::buffer::MAX_CHANNELS];
            let mut out_s = [0.0f64; crate::buffer::MAX_CHANNELS];
            for i in 0..frames {
                for (c, s) in planes.iter().enumerate().take(channels) {
                    in_s[c] = s[i] as f64;
                }
                self.limiter
                    .process_sample_multichannel(&in_s[..channels], &mut out_s[..channels]);
                for (c, s) in planes.iter_mut().enumerate().take(channels) {
                    s[i] = out_s[c] as f32;
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
            let (front, rest) = planes.split_at_mut(1);
            self.limiter.process_block_f64(front[0], rest[0]);
        } else {
            let mut in_s = [0.0f64; crate::buffer::MAX_CHANNELS];
            let mut out_s = [0.0f64; crate::buffer::MAX_CHANNELS];
            for i in 0..frames {
                for (c, s) in planes.iter().enumerate().take(channels) {
                    in_s[c] = s[i];
                }
                self.limiter
                    .process_sample_multichannel(&in_s[..channels], &mut out_s[..channels]);
                for (c, s) in planes.iter_mut().enumerate().take(channels) {
                    s[i] = out_s[c];
                }
            }
        }
    }
}
