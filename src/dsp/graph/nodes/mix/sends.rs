//! Phase 5 S2/S3 — the aux bus: a parallel accumulator for the slots'
//! post-fader sends, with a return into the master and the Phase-6 insert
//! seam.
//!
//! Sends are tapped inside the sum loops ([`super::sum`]) — each sending
//! slot's post-gain contribution is accumulated here; the return is applied
//! once per block after all slot sums, so the aux content joins the master
//! *before* the post-mix chain (EQ / dynamics / limiter) runs downstream.
//!
//! Realtime contract: two preallocated stereo planes, zeroed once per block;
//! no allocation, no locks. Disabled (`enabled = false`), a zero return
//! gain, or a block with no sends writes nothing — bit-exact.

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;

use super::SlotMeters;

/// The aux accumulator: stereo planes + return path + the insert seam.
/// (Not `Clone`/`Debug`-derived: the insert engine is neither.)
pub(crate) struct AuxBus {
    /// Whether the aux bus is active. Disabled = no zeroing, no taps, no
    /// return — bit-exact.
    pub(crate) enabled: bool,
    /// Linear return gain from the accumulator into the master.
    pub(crate) return_gain: f32,
    /// Whether any slot tapped into this block (the return is skipped when
    /// nothing was accumulated, so enabled-but-idle is still bit-exact).
    pub(crate) written: bool,
    /// Stereo accumulator planes, preallocated and zeroed once per block.
    pub(crate) planes: [Vec<f32>; 2],
    /// Peak / RMS metering over the accumulated send sum (Phase 5 S3),
    /// published like a slot's meters.
    pub(crate) meters: SlotMeters,
    /// Phase-6 insert seam: a global effect (convolution / reverb) will ride
    /// between the accumulator and the return. `None` in Phase 5; dead until
    /// the insert lands, by design.
    #[allow(dead_code)]
    pub(crate) insert: Option<crate::dsp::convolution::ConvolutionEngine>,
}

impl AuxBus {
    pub(crate) fn new() -> Self {
        Self {
            enabled: false,
            return_gain: 1.0,
            written: false,
            planes: [
                vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
                vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            ],
            meters: SlotMeters::default(),
            insert: None,
        }
    }

    /// Zero the accumulator for a fresh block. Called only when `enabled`.
    pub(crate) fn clear(&mut self, frames: usize) {
        self.planes[0][..frames].fill(0.0);
        self.planes[1][..frames].fill(0.0);
        self.written = false;
    }

    /// Apply the aux return into the master planes' front pair (channels 0/1;
    /// an MC master receives the stereo aux on its front pair). Skipped when
    /// disabled, idle, or the return gain is zero. `duck` is the aux duck
    /// gain (Phase 5 S3): 1.0 when the aux is not a duck target.
    pub(crate) fn return_into(&self, planes: &mut [&mut [f32]], frames: usize, duck: f32) {
        if !self.enabled || !self.written || self.return_gain == 0.0 || planes.len() < 2 {
            return;
        }
        let g = self.return_gain * duck;
        let (l, r) = planes.split_at_mut(1);
        let out_l = &mut l[0][..frames];
        let out_r = &mut r[0][..frames];
        for i in 0..frames {
            out_l[i] += self.planes[0][i] * g;
            out_r[i] += self.planes[1][i] * g;
        }
    }

    /// f64 twin of [`Self::return_into`]: the f32 aux planes are promoted at
    /// the sum (the f64 bus path keeps the aux in f32).
    pub(crate) fn return_into_f64(&self, planes: &mut [&mut [f64]], frames: usize, duck: f32) {
        if !self.enabled || !self.written || self.return_gain == 0.0 || planes.len() < 2 {
            return;
        }
        let g = self.return_gain as f64 * duck as f64;
        let (l, r) = planes.split_at_mut(1);
        let out_l = &mut l[0][..frames];
        let out_r = &mut r[0][..frames];
        for i in 0..frames {
            out_l[i] += self.planes[0][i] as f64 * g;
            out_r[i] += self.planes[1][i] as f64 * g;
        }
    }

    /// Per-block peak/RMS metering over the accumulated planes (Phase 5 S3).
    pub(crate) fn compute_meters(&mut self, frames: usize) {
        if !self.enabled || frames == 0 {
            return;
        }
        let mut peak = 0.0f32;
        let mut sum_sq = 0.0f32;
        for plane in self.planes.iter() {
            for &v in plane.iter().take(frames) {
                let a = v.abs();
                if a > peak {
                    peak = a;
                }
                sum_sq += v * v;
            }
        }
        let eps = 1e-12f32;
        self.meters = SlotMeters {
            peak_db: 20.0 * (peak.max(eps)).log10(),
            rms_db: 20.0 * ((sum_sq / (2.0 * frames as f32)).max(eps).sqrt()).log10(),
        };
    }
}
