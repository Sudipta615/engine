use crate::dsp::pipeline::{DspNodeInfo, DspStageCapability};

/// The core trait for all processing nodes in the DSP Graph.
///
/// Every node in the [`DspGraph`](super::DspGraph) describes its channel constraints,
/// precision capabilities, statefulness, latency, tail, real-time safety,
/// bit-perfect compatibility, and sample-rate sensitivity.
///
/// Nodes operate on planar audio slices (where each slice represents one channel of
/// `frames` length), ensuring zero memory allocations during real-time processing.
pub trait DspNode: Send {
    /// Static capability metadata describing the stage's physical characteristics.
    fn capability(&self) -> DspStageCapability;

    /// Node identifier matching [`DspStageCapability::name`].
    fn name(&self) -> &'static str {
        self.capability().name
    }

    /// Whether this node is currently active in the signal path (enabled, not
    /// bypassed, and performing non-identity transforms).
    fn is_active(&self) -> bool;

    /// Deterministic latency introduced by this stage in samples at the current sample rate.
    fn latency_samples(&self) -> usize {
        0
    }

    /// Deterministic latency in milliseconds at the current sample rate.
    fn latency_ms(&self, sample_rate: f32) -> f32 {
        if sample_rate > 0.0 {
            (self.latency_samples() as f32 / sample_rate) * 1000.0
        } else {
            0.0
        }
    }

    /// Ring-down tail length emitted after input stops (in samples at current sample rate).
    fn tail_samples(&self) -> usize {
        0
    }

    /// Ring-down tail length in milliseconds at the current sample rate.
    fn tail_ms(&self, sample_rate: f32) -> f32 {
        if sample_rate > 0.0 {
            (self.tail_samples() as f32 / sample_rate) * 1000.0
        } else {
            0.0
        }
    }

    /// Current node status report combining static metadata with live active/latency state.
    fn node_info(&self, sample_rate: f32) -> DspNodeInfo {
        DspNodeInfo {
            name: self.name(),
            active: self.is_active(),
            latency_ms: if self.is_active() {
                self.latency_ms(sample_rate)
            } else {
                0.0
            },
            tail_ms: if self.is_active() {
                self.tail_ms(sample_rate)
            } else {
                0.0
            },
        }
    }

    /// Reset internal filter state (e.g., on seek or track boundary).
    fn reset(&mut self);

    /// Reset filter state only, leaving persistent user state (volume,
    /// transition envelopes) intact. Defaults to a full reset; nodes whose
    /// user state must survive a filter-only reset override this.
    fn reset_filters_only(&mut self) {
        self.reset();
    }

    /// Prepare the node for a new sample rate and maximum channel count.
    fn prepare(&mut self, sample_rate: f32, max_channels: usize);

    /// Process a planar block of audio in 32-bit floating point precision.
    /// `planes` contains one mutable slice per channel, each of length `frames`.
    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]);

    /// Process a planar block of audio in 64-bit floating point precision (Quality mode).
    ///
    /// For stages that natively operate on `f64`, this is implemented directly.
    /// For stages requiring `f32` (e.g. WSOLA or FIR limiter), the node demotes,
    /// processes in `f32`, and promotes back.
    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]);
}
