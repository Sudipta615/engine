//! Block entry points — the `process_*` methods of [`DspGraph`],
//! forwarded verbatim (the Phase-47 shadow bit-compare was removed with
//! the legacy plan source in Phase 48).

use super::Graph2Engine;

impl Graph2Engine {
    /// The standard stereo block. Mirrors `DspGraph::process_block`.
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        self.inner.process_block(left, right);
    }

    /// The crossfade-pair block: primary tuple + secondary tuple. Mirrors
    /// `DspGraph::process_block_inputs`.
    pub fn process_block_inputs(
        &mut self,
        primary: (&mut [f32], &mut [f32]),
        secondary: (&mut [f32], &mut [f32]),
    ) {
        self.inner.process_block_inputs(primary, secondary);
    }

    /// The lanes block: primary + per-lane pairs (slices). Mirrors
    /// `DspGraph::process_block_lanes`.
    pub fn process_block_lanes(
        &mut self,
        primary: (&mut [f32], &mut [f32]),
        lanes: &mut [(&mut [f32], &mut [f32])],
    ) {
        self.inner.process_block_lanes(primary, lanes);
    }

    /// The crossfade + lanes block. Mirrors
    /// `DspGraph::process_block_crossfade_with_lanes`.
    pub fn process_block_crossfade_with_lanes(
        &mut self,
        primary: (&mut [f32], &mut [f32]),
        incoming: (&mut [f32], &mut [f32]),
        lanes: &mut [(&mut [f32], &mut [f32])],
    ) {
        self.inner
            .process_block_crossfade_with_lanes(primary, incoming, lanes);
    }

    /// The multichannel block (interleaved). Mirrors
    /// `DspGraph::process_block_multichannel`.
    pub fn process_block_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        self.inner.process_block_multichannel(interleaved, channels);
    }

    /// Per-frame final limiter (output domain). Mirrors
    /// `DspGraph::process_final_limiter`.
    pub fn process_final_limiter(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.inner.process_final_limiter(left, right)
    }

    /// Block final limiter. Mirrors `DspGraph::process_final_limiter_block`.
    pub fn process_final_limiter_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        self.inner.process_final_limiter_block(left, right);
    }

    /// Multichannel final limiter. Mirrors
    /// `DspGraph::process_final_limiter_multichannel`.
    pub fn process_final_limiter_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        self.inner
            .process_final_limiter_multichannel(interleaved, channels);
    }

    /// The f64 stereo block (Quality precision). Mirrors
    /// `DspGraph::process_block_f64`.
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        self.inner.process_block_f64(left, right);
    }

    /// Flush the final limiter's release tail. Mirrors
    /// `DspGraph::flush_final_limiter`.
    pub fn flush_final_limiter(&mut self) -> Vec<(f32, f32)> {
        self.inner.flush_final_limiter()
    }

    /// Flush the multichannel final limiter. Mirrors
    /// `DspGraph::flush_final_limiter_multichannel`.
    pub fn flush_final_limiter_multichannel(&mut self, channels: usize) -> Vec<f32> {
        self.inner.flush_final_limiter_multichannel(channels)
    }
}
