//! Output-domain final safety limiter for [`DspGraph`].

use super::*;

impl DspGraph {
    pub fn process_final_limiter(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.dop_bypass || self.bit_perfect {
            (left, right)
        } else {
            self.limiter_mut().limiter.process(left, right)
        }
    }

    pub fn process_final_limiter_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.dop_bypass || self.bit_perfect {
            return;
        }
        self.limiter_mut().limiter.process_block(left, right);
    }

    pub fn flush_final_limiter(&mut self) -> Vec<(f32, f32)> {
        if self.dop_bypass || self.bit_perfect {
            Vec::new()
        } else {
            self.limiter_mut().limiter.flush()
        }
    }

    pub fn process_final_limiter_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        if self.dop_bypass || self.bit_perfect {
            return;
        }
        self.limiter_mut()
            .limiter
            .process_block_multichannel(interleaved, channels);
    }

    pub fn flush_final_limiter_multichannel(&mut self, channels: usize) -> Vec<f32> {
        if self.dop_bypass || self.bit_perfect {
            Vec::new()
        } else {
            self.limiter_mut().limiter.flush_multichannel(channels)
        }
    }
}
