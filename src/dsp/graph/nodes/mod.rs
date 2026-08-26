pub mod convolution_node;
pub mod crossfeed_node;
pub mod dither_node;
pub mod dynamics_node;
pub mod eq_node;
pub mod gain_node;
pub mod limiter_node;
pub mod loudness_node;
pub mod mix_node;
pub mod resampler_node;
pub mod routing_node;
pub mod stereo_node;
pub mod timestretch_node;

pub use convolution_node::ConvolutionNode;
pub use crossfeed_node::CrossfeedNode;
pub use dither_node::DitherNode;
pub use dynamics_node::DynamicsNode;
pub use eq_node::EqNode;
pub use gain_node::{BalanceNode, GainNode, SeekFadeNode};
pub use limiter_node::LimiterNode;
pub use loudness_node::LoudnessNode;
pub use mix_node::{MixBusNode, MixInput, MixInputCmd, MixTransitionCmd};
pub use resampler_node::ResamplerNode;
pub use routing_node::RoutingNode;
pub use stereo_node::StereoNode;
pub use timestretch_node::TimeStretchNode;

use super::GraphNode;
use crate::dsp::{graph::node::DspNode, pipeline::DspStageCapability};

impl DspNode for GraphNode {
    fn capability(&self) -> DspStageCapability {
        match self {
            GraphNode::Mix(n) => n.capability(),
            GraphNode::Eq(n) => n.capability(),
            GraphNode::Dynamics(n) => n.capability(),
            GraphNode::Convolution(n) => n.capability(),
            GraphNode::Balance(n) => n.capability(),
            GraphNode::Crossfeed(n) => n.capability(),
            GraphNode::Stereo(n) => n.capability(),
            GraphNode::TimeStretch(n) => n.capability(),
            GraphNode::Volume(n) => n.capability(),
            GraphNode::SeekFade(n) => n.capability(),
            GraphNode::Routing(n) => n.capability(),
            GraphNode::Resampler(n) => n.capability(),
            GraphNode::Limiter(n) => n.capability(),
            GraphNode::Dither(n) => n.capability(),
        }
    }

    fn is_active(&self) -> bool {
        match self {
            GraphNode::Mix(n) => n.is_active(),
            GraphNode::Eq(n) => n.is_active(),
            GraphNode::Dynamics(n) => n.is_active(),
            GraphNode::Convolution(n) => n.is_active(),
            GraphNode::Balance(n) => n.is_active(),
            GraphNode::Crossfeed(n) => n.is_active(),
            GraphNode::Stereo(n) => n.is_active(),
            GraphNode::TimeStretch(n) => n.is_active(),
            GraphNode::Volume(n) => n.is_active(),
            GraphNode::SeekFade(n) => n.is_active(),
            GraphNode::Routing(n) => n.is_active(),
            GraphNode::Resampler(n) => n.is_active(),
            GraphNode::Limiter(n) => n.is_active(),
            GraphNode::Dither(n) => n.is_active(),
        }
    }

    fn latency_samples(&self) -> usize {
        match self {
            GraphNode::Mix(n) => n.latency_samples(),
            GraphNode::Eq(n) => n.latency_samples(),
            GraphNode::Dynamics(n) => n.latency_samples(),
            GraphNode::Convolution(n) => n.latency_samples(),
            GraphNode::Balance(n) => n.latency_samples(),
            GraphNode::Crossfeed(n) => n.latency_samples(),
            GraphNode::Stereo(n) => n.latency_samples(),
            GraphNode::TimeStretch(n) => n.latency_samples(),
            GraphNode::Volume(n) => n.latency_samples(),
            GraphNode::SeekFade(n) => n.latency_samples(),
            GraphNode::Routing(n) => n.latency_samples(),
            GraphNode::Resampler(n) => n.latency_samples(),
            GraphNode::Limiter(n) => n.latency_samples(),
            GraphNode::Dither(n) => n.latency_samples(),
        }
    }

    fn tail_samples(&self) -> usize {
        match self {
            GraphNode::Mix(n) => n.tail_samples(),
            GraphNode::Eq(n) => n.tail_samples(),
            GraphNode::Dynamics(n) => n.tail_samples(),
            GraphNode::Convolution(n) => n.tail_samples(),
            GraphNode::Balance(n) => n.tail_samples(),
            GraphNode::Crossfeed(n) => n.tail_samples(),
            GraphNode::Stereo(n) => n.tail_samples(),
            GraphNode::TimeStretch(n) => n.tail_samples(),
            GraphNode::Volume(n) => n.tail_samples(),
            GraphNode::SeekFade(n) => n.tail_samples(),
            GraphNode::Routing(n) => n.tail_samples(),
            GraphNode::Resampler(n) => n.tail_samples(),
            GraphNode::Limiter(n) => n.tail_samples(),
            GraphNode::Dither(n) => n.tail_samples(),
        }
    }

    fn reset(&mut self) {
        match self {
            GraphNode::Mix(n) => n.reset(),
            GraphNode::Eq(n) => n.reset(),
            GraphNode::Dynamics(n) => n.reset(),
            GraphNode::Convolution(n) => n.reset(),
            GraphNode::Balance(n) => n.reset(),
            GraphNode::Crossfeed(n) => n.reset(),
            GraphNode::Stereo(n) => n.reset(),
            GraphNode::TimeStretch(n) => n.reset(),
            GraphNode::Volume(n) => n.reset(),
            GraphNode::SeekFade(n) => n.reset(),
            GraphNode::Routing(n) => n.reset(),
            GraphNode::Resampler(n) => n.reset(),
            GraphNode::Limiter(n) => n.reset(),
            GraphNode::Dither(n) => n.reset(),
        }
    }

    fn prepare(&mut self, sample_rate: f32, max_channels: usize) {
        match self {
            GraphNode::Mix(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Eq(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Dynamics(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Convolution(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Balance(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Crossfeed(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Stereo(n) => n.prepare(sample_rate, max_channels),
            GraphNode::TimeStretch(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Volume(n) => n.prepare(sample_rate, max_channels),
            GraphNode::SeekFade(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Routing(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Resampler(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Limiter(n) => n.prepare(sample_rate, max_channels),
            GraphNode::Dither(n) => n.prepare(sample_rate, max_channels),
        }
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        match self {
            GraphNode::Mix(n) => n.process_block_f32(planes),
            GraphNode::Eq(n) => n.process_block_f32(planes),
            GraphNode::Dynamics(n) => n.process_block_f32(planes),
            GraphNode::Convolution(n) => n.process_block_f32(planes),
            GraphNode::Balance(n) => n.process_block_f32(planes),
            GraphNode::Crossfeed(n) => n.process_block_f32(planes),
            GraphNode::Stereo(n) => n.process_block_f32(planes),
            GraphNode::TimeStretch(n) => n.process_block_f32(planes),
            GraphNode::Volume(n) => n.process_block_f32(planes),
            GraphNode::SeekFade(n) => n.process_block_f32(planes),
            GraphNode::Routing(n) => n.process_block_f32(planes),
            GraphNode::Resampler(n) => n.process_block_f32(planes),
            GraphNode::Limiter(n) => n.process_block_f32(planes),
            GraphNode::Dither(n) => n.process_block_f32(planes),
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        match self {
            GraphNode::Mix(n) => n.process_block_f64(planes),
            GraphNode::Eq(n) => n.process_block_f64(planes),
            GraphNode::Dynamics(n) => n.process_block_f64(planes),
            GraphNode::Convolution(n) => n.process_block_f64(planes),
            GraphNode::Balance(n) => n.process_block_f64(planes),
            GraphNode::Crossfeed(n) => n.process_block_f64(planes),
            GraphNode::Stereo(n) => n.process_block_f64(planes),
            GraphNode::TimeStretch(n) => n.process_block_f64(planes),
            GraphNode::Volume(n) => n.process_block_f64(planes),
            GraphNode::SeekFade(n) => n.process_block_f64(planes),
            GraphNode::Routing(n) => n.process_block_f64(planes),
            GraphNode::Resampler(n) => n.process_block_f64(planes),
            GraphNode::Limiter(n) => n.process_block_f64(planes),
            GraphNode::Dither(n) => n.process_block_f64(planes),
        }
    }
}
