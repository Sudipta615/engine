//! Block entry points — the `process_*` methods of [`DspGraph`],
//! forwarded verbatim, with the Phase-47 shadow bit-compare (when a twin
//! is attached).
//!
//! The active engine's output always stands: the shadow is rendered on
//! copies, compared sample-for-sample (`f32::to_bits`, NaN payloads
//! included), and a mismatch only bumps the diagnostic counter — the
//! production stream is never altered or interrupted by the verification.
//!
//! Shadow copies allocate — by design: shadow mode is a diagnostic that
//! must stay off the production audio path (`graph2_shadow_verify` is
//! default-off; CI fidelity runs enable it). With the flag off every
//! entry point is a plain forward, allocation-free exactly like
//! `DspGraph`.

use super::Graph2Engine;

impl Graph2Engine {
    /// Compare one channel plane bit-exactly (NaN payloads included);
    /// bump the mismatch counter on the first divergence.
    fn shadow_compare_plane(&mut self, active: &[f32], shadow: &[f32]) {
        if active.len() != shadow.len() {
            self.shadow_mismatches += 1;
            return;
        }
        for (a, s) in active.iter().zip(shadow.iter()) {
            if a.to_bits() != s.to_bits() {
                self.shadow_mismatches += 1;
                return;
            }
        }
    }

    /// The standard stereo block. Mirrors `DspGraph::process_block`.
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        let Some(shadow) = &mut self.shadow else {
            self.inner.process_block(left, right);
            return;
        };
        let mut a_l = left.to_vec();
        let mut a_r = right.to_vec();
        self.inner.process_block(&mut a_l, &mut a_r);
        let mut s_l = left.to_vec();
        let mut s_r = right.to_vec();
        shadow.process_block(&mut s_l, &mut s_r);
        self.shadow_compare_plane(&a_l, &s_l);
        self.shadow_compare_plane(&a_r, &s_r);
        self.shadow_blocks += 1;
        left.copy_from_slice(&a_l);
        right.copy_from_slice(&a_r);
    }

    /// The crossfade-pair block: primary tuple + secondary tuple. Mirrors
    /// `DspGraph::process_block_inputs`.
    pub fn process_block_inputs(
        &mut self,
        primary: (&mut [f32], &mut [f32]),
        secondary: (&mut [f32], &mut [f32]),
    ) {
        let Some(shadow) = &mut self.shadow else {
            self.inner.process_block_inputs(primary, secondary);
            return;
        };
        let mut a_p = (primary.0.to_vec(), primary.1.to_vec());
        let mut a_s = (secondary.0.to_vec(), secondary.1.to_vec());
        self.inner
            .process_block_inputs((&mut a_p.0, &mut a_p.1), (&mut a_s.0, &mut a_s.1));
        let mut s_p = (primary.0.to_vec(), primary.1.to_vec());
        let mut s_s = (secondary.0.to_vec(), secondary.1.to_vec());
        shadow.process_block_inputs((&mut s_p.0, &mut s_p.1), (&mut s_s.0, &mut s_s.1));
        self.shadow_compare_plane(&a_p.0, &s_p.0);
        self.shadow_compare_plane(&a_p.1, &s_p.1);
        self.shadow_blocks += 1;
        primary.0.copy_from_slice(&a_p.0);
        primary.1.copy_from_slice(&a_p.1);
        secondary.0.copy_from_slice(&a_s.0);
        secondary.1.copy_from_slice(&a_s.1);
    }

    /// The lanes block: primary + per-lane pairs (slices). Mirrors
    /// `DspGraph::process_block_lanes`.
    pub fn process_block_lanes(
        &mut self,
        primary: (&mut [f32], &mut [f32]),
        lanes: &mut [(&mut [f32], &mut [f32])],
    ) {
        let Some(shadow) = &mut self.shadow else {
            self.inner.process_block_lanes(primary, lanes);
            return;
        };
        let mut a_p = (primary.0.to_vec(), primary.1.to_vec());
        let mut a_lanes: Vec<(Vec<f32>, Vec<f32>)> = lanes
            .iter()
            .map(|(l, r)| (l.to_vec(), r.to_vec()))
            .collect();
        {
            let mut refs: Vec<(&mut [f32], &mut [f32])> = a_lanes
                .iter_mut()
                .map(|(l, r)| (l.as_mut_slice(), r.as_mut_slice()))
                .collect();
            self.inner
                .process_block_lanes((&mut a_p.0, &mut a_p.1), &mut refs);
        }
        let mut s_p = (primary.0.to_vec(), primary.1.to_vec());
        let mut s_lanes: Vec<(Vec<f32>, Vec<f32>)> = lanes
            .iter()
            .map(|(l, r)| (l.to_vec(), r.to_vec()))
            .collect();
        {
            let mut refs: Vec<(&mut [f32], &mut [f32])> = s_lanes
                .iter_mut()
                .map(|(l, r)| (l.as_mut_slice(), r.as_mut_slice()))
                .collect();
            shadow.process_block_lanes((&mut s_p.0, &mut s_p.1), &mut refs);
        }
        self.shadow_compare_plane(&a_p.0, &s_p.0);
        self.shadow_compare_plane(&a_p.1, &s_p.1);
        self.shadow_blocks += 1;
        primary.0.copy_from_slice(&a_p.0);
        primary.1.copy_from_slice(&a_p.1);
        for (dst, (l, r)) in lanes.iter_mut().zip(a_lanes) {
            dst.0.copy_from_slice(&l);
            dst.1.copy_from_slice(&r);
        }
    }

    /// The crossfade + lanes block. Mirrors
    /// `DspGraph::process_block_crossfade_with_lanes`.
    pub fn process_block_crossfade_with_lanes(
        &mut self,
        primary: (&mut [f32], &mut [f32]),
        incoming: (&mut [f32], &mut [f32]),
        lanes: &mut [(&mut [f32], &mut [f32])],
    ) {
        let Some(shadow) = &mut self.shadow else {
            self.inner
                .process_block_crossfade_with_lanes(primary, incoming, lanes);
            return;
        };
        let mut a_p = (primary.0.to_vec(), primary.1.to_vec());
        let mut a_i = (incoming.0.to_vec(), incoming.1.to_vec());
        let mut a_lanes: Vec<(Vec<f32>, Vec<f32>)> = lanes
            .iter()
            .map(|(l, r)| (l.to_vec(), r.to_vec()))
            .collect();
        {
            let mut refs: Vec<(&mut [f32], &mut [f32])> = a_lanes
                .iter_mut()
                .map(|(l, r)| (l.as_mut_slice(), r.as_mut_slice()))
                .collect();
            self.inner.process_block_crossfade_with_lanes(
                (&mut a_p.0, &mut a_p.1),
                (&mut a_i.0, &mut a_i.1),
                &mut refs,
            );
        }
        let mut s_p = (primary.0.to_vec(), primary.1.to_vec());
        let mut s_i = (incoming.0.to_vec(), incoming.1.to_vec());
        let mut s_lanes: Vec<(Vec<f32>, Vec<f32>)> = lanes
            .iter()
            .map(|(l, r)| (l.to_vec(), r.to_vec()))
            .collect();
        {
            let mut refs: Vec<(&mut [f32], &mut [f32])> = s_lanes
                .iter_mut()
                .map(|(l, r)| (l.as_mut_slice(), r.as_mut_slice()))
                .collect();
            shadow.process_block_crossfade_with_lanes(
                (&mut s_p.0, &mut s_p.1),
                (&mut s_i.0, &mut s_i.1),
                &mut refs,
            );
        }
        self.shadow_compare_plane(&a_p.0, &s_p.0);
        self.shadow_compare_plane(&a_p.1, &s_p.1);
        self.shadow_blocks += 1;
        primary.0.copy_from_slice(&a_p.0);
        primary.1.copy_from_slice(&a_p.1);
        incoming.0.copy_from_slice(&a_i.0);
        incoming.1.copy_from_slice(&a_i.1);
        for (dst, (l, r)) in lanes.iter_mut().zip(a_lanes) {
            dst.0.copy_from_slice(&l);
            dst.1.copy_from_slice(&r);
        }
    }

    /// The multichannel block (interleaved). Mirrors
    /// `DspGraph::process_block_multichannel`.
    pub fn process_block_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        let Some(shadow) = &mut self.shadow else {
            self.inner.process_block_multichannel(interleaved, channels);
            return;
        };
        let mut a = interleaved.to_vec();
        self.inner.process_block_multichannel(&mut a, channels);
        let mut s = interleaved.to_vec();
        shadow.process_block_multichannel(&mut s, channels);
        self.shadow_compare_plane(&a, &s);
        self.shadow_blocks += 1;
        interleaved.copy_from_slice(&a);
    }

    /// Per-frame final limiter (output domain). Mirrors
    /// `DspGraph::process_final_limiter`. Not shadow-compared: the final
    /// limiter is output-domain state shared by the endpoint workers —
    /// the shadow's limiter stays in lock-step through the identical
    /// command stream, and the mix-path block compares cover the A/B.
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
    /// `DspGraph::process_block_f64`. Not shadow-compared: the f64 path is
    /// the pipeline-oracle equivalence surface, exercised by the fidelity
    /// suites; shadow mode compares the production f32 path.
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
