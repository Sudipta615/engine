//! Typed node accessors over the arena for construction / lifecycle / report
//! code. Call sites migrate from `self.volume` (field) to `self.volume()`
//! with a mechanical one-line change; the arena slot for each node kind is
//! fixed by [`node_id`], so the `unreachable!` arms are construction-invariant.

use super::*;

impl DspGraph {
    // ── Mix bus (pre-mix chain) ────────────────────────────────────────────

    pub fn mix(&self) -> &MixBusNode {
        match &self.active.nodes[node_id::MIX] {
            GraphNode::Mix(n) => n,
            _ => unreachable!("arena slot MIX holds a MixBusNode"),
        }
    }

    pub fn mix_mut(&mut self) -> &mut MixBusNode {
        match &mut self.active.nodes[node_id::MIX] {
            GraphNode::Mix(n) => n,
            _ => unreachable!("arena slot MIX holds a MixBusNode"),
        }
    }

    // ── Pre-mix accessors — input-0 / input-1 aliases into the mix bus ────
    //
    // Backward-compatible with the Phase-1 named pre-mix accessors: input 0
    // is the outgoing (primary) chain, input 1 the incoming (secondary)
    // chain. The bus owns N inputs, so these read the first two.

    pub fn out_preamp(&self) -> &GainNode {
        &self.mix().inputs[0].preamp
    }

    pub fn out_preamp_mut(&mut self) -> &mut GainNode {
        &mut self.mix_mut().inputs[0].preamp
    }

    pub fn out_loudness(&self) -> &LoudnessNode {
        &self.mix().inputs[0].loudness
    }

    pub fn out_loudness_mut(&mut self) -> &mut LoudnessNode {
        &mut self.mix_mut().inputs[0].loudness
    }

    pub fn in_preamp(&self) -> &GainNode {
        &self.mix().inputs[1].preamp
    }

    pub fn in_preamp_mut(&mut self) -> &mut GainNode {
        &mut self.mix_mut().inputs[1].preamp
    }

    pub fn in_loudness(&self) -> &LoudnessNode {
        &self.mix().inputs[1].loudness
    }

    pub fn in_loudness_mut(&mut self) -> &mut LoudnessNode {
        &mut self.mix_mut().inputs[1].loudness
    }

    // ── Post-mix Chain ─────────────────────────────────────────────────────

    pub fn eq(&self) -> &EqNode {
        match &self.active.nodes[node_id::EQ] {
            GraphNode::Eq(n) => n,
            _ => unreachable!("arena slot EQ holds an EqNode"),
        }
    }

    pub fn eq_mut(&mut self) -> &mut EqNode {
        match &mut self.active.nodes[node_id::EQ] {
            GraphNode::Eq(n) => n,
            _ => unreachable!("arena slot EQ holds an EqNode"),
        }
    }

    pub fn dynamics(&self) -> &DynamicsNode {
        match &self.active.nodes[node_id::DYNAMICS] {
            GraphNode::Dynamics(n) => n,
            _ => unreachable!("arena slot DYNAMICS holds a DynamicsNode"),
        }
    }

    pub fn dynamics_mut(&mut self) -> &mut DynamicsNode {
        match &mut self.active.nodes[node_id::DYNAMICS] {
            GraphNode::Dynamics(n) => n,
            _ => unreachable!("arena slot DYNAMICS holds a DynamicsNode"),
        }
    }

    pub fn convolution(&self) -> &ConvolutionNode {
        match &self.active.nodes[node_id::CONVOLUTION] {
            GraphNode::Convolution(n) => n,
            _ => unreachable!("arena slot CONVOLUTION holds a ConvolutionNode"),
        }
    }

    pub fn convolution_mut(&mut self) -> &mut ConvolutionNode {
        match &mut self.active.nodes[node_id::CONVOLUTION] {
            GraphNode::Convolution(n) => n,
            _ => unreachable!("arena slot CONVOLUTION holds a ConvolutionNode"),
        }
    }

    pub fn balance(&self) -> &BalanceNode {
        match &self.active.nodes[node_id::BALANCE] {
            GraphNode::Balance(n) => n,
            _ => unreachable!("arena slot BALANCE holds a BalanceNode"),
        }
    }

    pub fn balance_mut(&mut self) -> &mut BalanceNode {
        match &mut self.active.nodes[node_id::BALANCE] {
            GraphNode::Balance(n) => n,
            _ => unreachable!("arena slot BALANCE holds a BalanceNode"),
        }
    }

    pub fn crossfeed(&self) -> &CrossfeedNode {
        match &self.active.nodes[node_id::CROSSFEED] {
            GraphNode::Crossfeed(n) => n,
            _ => unreachable!("arena slot CROSSFEED holds a CrossfeedNode"),
        }
    }

    pub fn crossfeed_mut(&mut self) -> &mut CrossfeedNode {
        match &mut self.active.nodes[node_id::CROSSFEED] {
            GraphNode::Crossfeed(n) => n,
            _ => unreachable!("arena slot CROSSFEED holds a CrossfeedNode"),
        }
    }

    pub fn stereo(&self) -> &StereoNode {
        match &self.active.nodes[node_id::STEREO] {
            GraphNode::Stereo(n) => n,
            _ => unreachable!("arena slot STEREO holds a StereoNode"),
        }
    }

    pub fn stereo_mut(&mut self) -> &mut StereoNode {
        match &mut self.active.nodes[node_id::STEREO] {
            GraphNode::Stereo(n) => n,
            _ => unreachable!("arena slot STEREO holds a StereoNode"),
        }
    }

    pub fn timestretch(&self) -> &TimeStretchNode {
        match &self.active.nodes[node_id::TIMESTRETCH] {
            GraphNode::TimeStretch(n) => n,
            _ => unreachable!("arena slot TIMESTRETCH holds a TimeStretchNode"),
        }
    }

    pub fn timestretch_mut(&mut self) -> &mut TimeStretchNode {
        match &mut self.active.nodes[node_id::TIMESTRETCH] {
            GraphNode::TimeStretch(n) => n,
            _ => unreachable!("arena slot TIMESTRETCH holds a TimeStretchNode"),
        }
    }

    pub fn volume(&self) -> &GainNode {
        match &self.active.nodes[node_id::VOLUME] {
            GraphNode::Volume(n) => n,
            _ => unreachable!("arena slot VOLUME holds a GainNode"),
        }
    }

    pub fn volume_mut(&mut self) -> &mut GainNode {
        match &mut self.active.nodes[node_id::VOLUME] {
            GraphNode::Volume(n) => n,
            _ => unreachable!("arena slot VOLUME holds a GainNode"),
        }
    }

    pub fn seek_fade(&self) -> &SeekFadeNode {
        match &self.active.nodes[node_id::SEEK_FADE] {
            GraphNode::SeekFade(n) => n,
            _ => unreachable!("arena slot SEEK_FADE holds a SeekFadeNode"),
        }
    }

    pub fn seek_fade_mut(&mut self) -> &mut SeekFadeNode {
        match &mut self.active.nodes[node_id::SEEK_FADE] {
            GraphNode::SeekFade(n) => n,
            _ => unreachable!("arena slot SEEK_FADE holds a SeekFadeNode"),
        }
    }

    // ── Routing & Multichannel ─────────────────────────────────────────────

    pub fn routing(&self) -> &RoutingNode {
        match &self.active.nodes[node_id::ROUTING] {
            GraphNode::Routing(n) => n,
            _ => unreachable!("arena slot ROUTING holds a RoutingNode"),
        }
    }

    pub fn routing_mut(&mut self) -> &mut RoutingNode {
        match &mut self.active.nodes[node_id::ROUTING] {
            GraphNode::Routing(n) => n,
            _ => unreachable!("arena slot ROUTING holds a RoutingNode"),
        }
    }

    // ── Output Domain & Safety ─────────────────────────────────────────────

    pub fn limiter(&self) -> &LimiterNode {
        match &self.active.nodes[node_id::LIMITER] {
            GraphNode::Limiter(n) => n,
            _ => unreachable!("arena slot LIMITER holds a LimiterNode"),
        }
    }

    pub fn limiter_mut(&mut self) -> &mut LimiterNode {
        match &mut self.active.nodes[node_id::LIMITER] {
            GraphNode::Limiter(n) => n,
            _ => unreachable!("arena slot LIMITER holds a LimiterNode"),
        }
    }

    pub fn dither_mut(&mut self) -> &mut DitherNode {
        match &mut self.active.nodes[node_id::DITHER] {
            GraphNode::Dither(n) => n,
            _ => unreachable!("arena slot DITHER holds a DitherNode"),
        }
    }
}
