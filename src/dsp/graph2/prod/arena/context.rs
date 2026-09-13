use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};

/// Pre-allocated scratch buffers ensuring the graph execution never performs dynamic allocations on the real-time audio thread.
#[derive(Debug)]
pub struct GraphScratch {
    /// Stereo f64 scratch channels for Quality-mode precision promotion.
    pub scratch_f64_l: Vec<f64>,
    pub scratch_f64_r: Vec<f64>,
    /// Multichannel planar scratch channels for de-interleaving and channel routing (up to [`MAX_CHANNELS`]).
    pub scratch_mc: Vec<Vec<f32>>,
}

impl Default for GraphScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphScratch {
    /// Allocate fixed-size scratch buffers sized to worst-case block frames.
    pub fn new() -> Self {
        Self {
            scratch_f64_l: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_f64_r: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_mc: (0..MAX_CHANNELS)
                .map(|_| vec![0.0; MAX_AUDIO_BLOCK_FRAMES])
                .collect(),
        }
    }

    /// Reset all scratch buffer contents to zero.
    pub fn clear(&mut self) {
        self.scratch_f64_l.fill(0.0);
        self.scratch_f64_r.fill(0.0);
        for plane in &mut self.scratch_mc {
            plane.fill(0.0);
        }
    }
}
