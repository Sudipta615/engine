use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::dsp::{
    channel_trim::ChannelTrimmer,
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// Multichannel routing and calibration node (Trim, delay, polarity, channel EQ, bass management, and matrix routing).
pub struct RoutingNode {
    pub trimmer: ChannelTrimmer,
    pub channel_count: usize,
    scratch: Vec<Vec<f32>>,
}

impl RoutingNode {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            trimmer: ChannelTrimmer::new(sample_rate),
            channel_count: 2,
            scratch: (0..MAX_CHANNELS)
                .map(|_| vec![0.0; MAX_AUDIO_BLOCK_FRAMES])
                .collect(),
        }
    }
}

impl DspNode for RoutingNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "channel_trim",
            channel_support: StageChannelSupport::AllChannels,
            position: "pre-mix (MC path only)",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.trimmer.is_active(self.channel_count)
    }

    fn reset(&mut self) {}

    fn prepare(&mut self, sample_rate: f32, max_channels: usize) {
        self.channel_count = max_channels;
        self.trimmer.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        let channels = planes.len().min(MAX_CHANNELS);
        if channels == 0 || !self.trimmer.is_active(channels) {
            return;
        }
        let frames = planes[0].len().min(MAX_AUDIO_BLOCK_FRAMES);

        for ch in 0..channels {
            self.scratch[ch][..frames].copy_from_slice(&planes[ch][..frames]);
        }

        self.trimmer
            .process_planes(&mut self.scratch[..channels], channels, frames);

        for ch in 0..channels {
            planes[ch][..frames].copy_from_slice(&self.scratch[ch][..frames]);
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        let channels = planes.len().min(MAX_CHANNELS);
        if channels == 0 || !self.trimmer.is_active(channels) {
            return;
        }
        let frames = planes[0].len().min(MAX_AUDIO_BLOCK_FRAMES);

        for ch in 0..channels {
            for i in 0..frames {
                self.scratch[ch][i] = planes[ch][i] as f32;
            }
        }

        self.trimmer
            .process_planes(&mut self.scratch[..channels], channels, frames);

        for ch in 0..channels {
            for i in 0..frames {
                planes[ch][i] = self.scratch[ch][i] as f64;
            }
        }
    }
}
