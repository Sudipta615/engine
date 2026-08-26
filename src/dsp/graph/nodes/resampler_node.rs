use crate::dsp::{
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

#[cfg(feature = "resample")]
use crate::dsp::resampler::GenericResampler;

/// Resampler node wrapping the sample rate converter.
pub struct ResamplerNode {
    #[cfg(feature = "resample")]
    pub resampler: Option<GenericResampler>,
    pub source_rate: f32,
    pub output_rate: f32,
}

impl ResamplerNode {
    pub fn new(source_rate: f32, output_rate: f32) -> Self {
        Self {
            #[cfg(feature = "resample")]
            resampler: None,
            source_rate,
            output_rate,
        }
    }
}

impl DspNode for ResamplerNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "resampler",
            channel_support: StageChannelSupport::StereoOnly,
            position: "output domain",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        #[cfg(feature = "resample")]
        {
            self.resampler
                .as_ref()
                .is_some_and(|r| !r.is_passthrough() && !r.is_disabled())
        }
        #[cfg(not(feature = "resample"))]
        {
            false
        }
    }

    fn latency_samples(&self) -> usize {
        #[cfg(feature = "resample")]
        {
            self.resampler.as_ref().map_or(0, |r| r.latency_samples())
        }
        #[cfg(not(feature = "resample"))]
        {
            0
        }
    }

    fn reset(&mut self) {
        #[cfg(feature = "resample")]
        if let Some(ref mut r) = self.resampler {
            r.reset();
        }
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.output_rate = sample_rate;
    }

    fn process_block_f32(&mut self, _planes: &mut [&mut [f32]]) {}

    fn process_block_f64(&mut self, _planes: &mut [&mut [f64]]) {}
}
