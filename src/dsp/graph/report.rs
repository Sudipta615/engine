//! Latency & graph introspection for [`DspGraph`].

use super::*;

impl DspGraph {
    /// Snapshot dynamic graph nodes for diagnostics and UI telemetry.
    pub fn graph_nodes(&self) -> Vec<DspNodeInfo> {
        let bypassed = self.bit_perfect || self.dop_bypass;
        let mc_channels = self.multichannel_layout.channel_count();
        let mut nodes = Vec::with_capacity(DSP_STAGE_CAPABILITIES.len());

        for cap in DSP_STAGE_CAPABILITIES {
            let (active, latency_ms, tail_ms) = if bypassed {
                (false, 0.0, 0.0)
            } else {
                match cap.name {
                    "channel_trim" => (self.routing.trimmer.is_active(mc_channels), 0.0, 0.0),
                    "channel_eq" | "bass_management" | "channel_mix" => (false, 0.0, 0.0),
                    "out_preamp" => (self.out_preamp.is_active(), 0.0, 0.0),
                    "in_preamp" => (self.in_preamp.is_active(), 0.0, 0.0),
                    "out_loudness" => (self.out_loudness.is_active(), 0.0, 0.0),
                    "in_loudness" => (self.in_loudness.is_active(), 0.0, 0.0),
                    "mixer" => (false, 0.0, 0.0),
                    "eq" => (self.eq.is_active(), 0.0, 0.0),
                    "multiband_compressor" => (self.dynamics.is_active(), 0.0, 0.0),
                    "convolution" => {
                        if self.convolution.is_active() {
                            let latency_ms = self.convolution.engine.latency_ms();
                            let ir_len = self.convolution.engine.num_partitions()
                                * self.convolution.engine.block_size();
                            let ir_len_ms = ir_len as f32 / self.sample_rate * 1000.0;
                            (true, latency_ms, (ir_len_ms - latency_ms).max(0.0))
                        } else {
                            (false, 0.0, 0.0)
                        }
                    }
                    "balance" => (self.balance.is_active(), 0.0, 0.0),
                    "crossfeed" => {
                        if self.crossfeed.is_active() {
                            let d = self.crossfeed.crossfeed.latency_ms();
                            (true, d, d)
                        } else {
                            (false, 0.0, 0.0)
                        }
                    }
                    "stereo_enhancer" => (self.stereo.is_active(), 0.0, 0.0),
                    "timestretch" => {
                        let active = self.timestretch.is_active();
                        let latency = if active {
                            self.timestretch.stretcher.latency_ms()
                        } else {
                            0.0
                        };
                        (active, latency, 0.0)
                    }
                    "volume" => (self.volume.is_active(), 0.0, 0.0),
                    "seek_fade" => (self.seek_fade.is_active(), 0.0, 0.0),
                    "limiter" => {
                        let active = self.limiter.is_active();
                        let lookahead = if active {
                            self.limiter.limiter.lookahead_ms()
                        } else {
                            0.0
                        };
                        let tail = if active {
                            self.limiter.limiter.release_ms()
                        } else {
                            0.0
                        };
                        (active, lookahead, tail)
                    }
                    "resampler" | "dither" => (false, 0.0, 0.0),
                    _ => (false, 0.0, 0.0),
                }
            };
            nodes.push(DspNodeInfo {
                name: cap.name,
                active,
                latency_ms,
                tail_ms,
            });
        }
        nodes
    }

    /// Total deterministic graph latency in milliseconds (output domain).
    pub fn total_latency_ms(&self) -> f32 {
        if self.bit_perfect || self.dop_bypass {
            return 0.0;
        }
        let mut total = 0.0;
        if self.crossfeed.is_active() {
            total += self.crossfeed.crossfeed.latency_ms();
        }
        if self.timestretch.is_active() {
            total += self.timestretch.stretcher.latency_ms();
        }
        if self.limiter.is_active() {
            total += self.limiter.limiter.lookahead_ms() + self.limiter.limiter.detector_delay_ms();
        }
        if self.convolution.is_active() {
            total += self.convolution.engine.latency_ms();
        }
        total
    }
}
