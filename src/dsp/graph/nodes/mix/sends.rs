//! Phase 5 S2/S3 + Phase 6 — the aux bus: a parallel accumulator for the
//! slots' post-fader sends, with an optional global insert (convolution)
//! between the accumulator and the return into the master.
//!
//! Sends are tapped inside the sum loops ([`super::sum`]) — each sending
//! slot's post-gain contribution is accumulated here; the return is applied
//! once per block after all slot sums, so the aux content joins the master
//! *before* the post-mix chain (EQ / dynamics / limiter) runs downstream.
//!
//! Realtime contract: two preallocated stereo planes, zeroed once per block;
//! no allocation, no locks. Disabled (`enabled = false`), a zero return
//! gain, or a block with no sends writes nothing — bit-exact. The insert
//! processes the accumulator planes in place before the return and is
//! skipped unless enabled AND an IR is loaded — also bit-exact.

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::dsp_utils::accumulate_scaled;

use super::SlotMeters;

/// The aux accumulator: stereo planes + return path + the Phase-6 insert.
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
    /// Phase-6 insert: a global convolution (reverb / cabinet) that
    /// processes the accumulator in place before the return. `None` when no
    /// IR has been configured.
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

    /// Configure the insert convolution (Phase 6). Control path — the IR
    /// file load happens here (allocation is legal). `ir_path: None` keeps
    /// the currently loaded IR (only enabled/wet change). A missing or
    /// unreadable IR file logs a warning and leaves the insert inactive
    /// (bit-exact) rather than failing the whole bus.
    pub(crate) fn apply_insert(
        &mut self,
        enabled: bool,
        wet_mix: f32,
        sample_rate: f32,
        ir_path: Option<&str>,
    ) {
        if let Some(path) = ir_path {
            let engine = self.insert.get_or_insert_with(|| {
                crate::dsp::convolution::ConvolutionEngine::new(sample_rate, 8192)
            });
            engine.set_wet_mix(wet_mix);
            let loaded = engine.load_ir_from_file(std::path::Path::new(path));
            if loaded.is_err() {
                log::warn!("Aux insert: failed to load IR '{}'", path);
            }
        } else if let Some(engine) = &mut self.insert {
            engine.set_wet_mix(wet_mix);
        }
        if let Some(engine) = &mut self.insert {
            engine.set_enabled(enabled);
        }
    }

    /// Runtime toggle of the insert (Phase 6): enabled / wet only — the IR
    /// stays as configured (config-time load). No-op when no IR engine
    /// exists yet (nothing to process; stays bit-exact).
    pub(crate) fn set_insert(&mut self, enabled: bool, wet_mix: f32) {
        if let Some(engine) = &mut self.insert {
            engine.set_wet_mix(wet_mix);
            engine.set_enabled(enabled);
        }
    }

    /// Whether the insert will process this block (enabled + IR loaded).
    pub(crate) fn insert_active(&self) -> bool {
        self.insert
            .as_ref()
            .map(|e| e.is_enabled() && e.is_ir_loaded())
            .unwrap_or(false)
    }

    /// Zero the accumulator for a fresh block. Called only when `enabled`.
    pub(crate) fn clear(&mut self, frames: usize) {
        self.planes[0][..frames].fill(0.0);
        self.planes[1][..frames].fill(0.0);
        self.written = false;
    }

    /// Apply the aux return into the master planes' front pair (channels 0/1;
    /// an MC master receives the stereo aux on its front pair). The insert
    /// convolution processes the accumulator planes in place first (Phase 6);
    /// the return itself is a bit-exact element-wise `+=` (SIMD-accelerated,
    /// see [`accumulate_scaled`]). Skipped when disabled, idle, or the
    /// return gain is zero. `duck` is the aux duck gain (Phase 5 S3): 1.0
    /// when the aux is not a duck target.
    pub(crate) fn return_into(&mut self, planes: &mut [&mut [f32]], frames: usize, duck: f32) {
        if !self.enabled || !self.written || self.return_gain == 0.0 || planes.len() < 2 {
            return;
        }
        if self.insert_active() {
            if let Some(engine) = &mut self.insert {
                let (left, right) = self.planes.split_at_mut(1);
                engine.process_block(&mut left[0][..frames], &mut right[0][..frames]);
            }
        }
        let g = self.return_gain * duck;
        let (l, r) = planes.split_at_mut(1);
        let out_l = &mut l[0][..frames];
        let out_r = &mut r[0][..frames];
        accumulate_scaled(out_l, &self.planes[0], g, frames);
        accumulate_scaled(out_r, &self.planes[1], g, frames);
    }

    /// f64 twin of [`Self::return_into`]: the f32 aux planes are promoted at
    /// the sum (the f64 bus path keeps the aux in f32); the insert also runs
    /// in f32.
    pub(crate) fn return_into_f64(&mut self, planes: &mut [&mut [f64]], frames: usize, duck: f32) {
        if !self.enabled || !self.written || self.return_gain == 0.0 || planes.len() < 2 {
            return;
        }
        if self.insert_active() {
            if let Some(engine) = &mut self.insert {
                let (left, right) = self.planes.split_at_mut(1);
                engine.process_block(&mut left[0][..frames], &mut right[0][..frames]);
            }
        }
        // The f64 path keeps the scalar promotion loop (allocation-free);
        // the f32 hot path uses the SIMD accumulate above.
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
