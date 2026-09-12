//! Shared node-processing kernels (v3.50, Phase 45 S3).
//!
//! One set of per-node **processing kernels** — the sample math every
//! Graph 2.0 node runs per block — used by *both* executors: the offline
//! executor (`super::ops`) and the realtime executor
//! (`crate::dsp::graph2::rt`). The contract: offline and realtime renders
//! go through the exact same arithmetic, so divergence between them is
//! structurally impossible — a topology that renders one way offline
//! cannot render another way on the audio thread.
//!
//! The kernels are written **allocation-parameterized**: each takes the
//! caller-owned buffers it writes into (the offline executor passes
//! freshly grown `Vec`s; the realtime executor passes preallocated
//! scratch), so the math itself is free of heap traffic by construction.

use super::super::node::{SourceParams, TestSignal};

/// Kernel lengths ≥ this many taps route a `Convolution` node through the
/// partitioned-FFT engine instead of the exact direct path. Below it, direct
/// convolution is both cheaper and *exact* (byte-equal to the reference), and
/// the engine's UP-OLA latency (one 512-sample partition) is already longer
/// than the front delay the node must present — so only genuinely long IRs
/// where O(N·M) blows up get the fast path.
pub const CONVOLUTION_FFT_THRESHOLD: usize = 512;

/// One scheduled gain step: frames `[0, local)` keep the old gain, frames
/// `[local, block)` use `gain` — the sample-accurate form the timeline
/// scheduler fires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainStep {
    pub gain: f32,
    pub local: usize,
}

/// Apply a `Gain` node to one block.
///
/// * `step` — an optional scheduled step at a local frame index; the
///   returned value carries the persisted gain for subsequent blocks.
/// * `automation` — an optional per-block `(start, end)` ramp that sweeps
///   the gain smoothly when no explicit step is pending.
///
/// Returns the gain value the *next* block should use as its base (the
/// stepped value once a step has applied, otherwise the passed base).
pub fn kernel_gain(
    in_plane: &[f32],
    out: &mut [f32],
    base_gain: f32,
    step: Option<GainStep>,
    automation: Option<(f32, f32)>,
) -> f32 {
    match step {
        Some(s) => {
            let local = s.local.min(out.len());
            for (i, o) in out.iter_mut().enumerate() {
                let g = if i >= local { s.gain } else { base_gain };
                *o = in_plane[i] * g;
            }
            s.gain
        }
        None => match automation {
            Some((start, end)) => {
                let ramp = end - start;
                let denom = (out.len().saturating_sub(1)).max(1) as f32;
                for (i, o) in out.iter_mut().enumerate() {
                    let t = i as f32 / denom;
                    *o = in_plane[i] * (start + ramp * t);
                }
                base_gain
            }
            None => {
                for (o, &s) in out.iter_mut().zip(in_plane.iter()) {
                    *o = s * base_gain;
                }
                base_gain
            }
        },
    }
}

/// Sum N input planes into one output plane (the `Mix` fan-in kernel).
/// `out` must be zeroed by the caller (unconnected inputs are simply
/// absent from `inputs`).
pub fn kernel_mix(inputs: &[&[f32]], out: &mut [f32]) {
    for plane in inputs {
        for (o, &s) in out.iter_mut().zip(plane.iter()) {
            *o += s;
        }
    }
}

/// Read/write a delay ring for one block (the `Delay` kernel): `buf` is a
/// ring of `buf.len()` samples at cursor `pos`; reads happen before writes
/// at each frame.
pub fn kernel_delay(in_plane: &[f32], out: &mut [f32], buf: &mut [f32], pos: &mut usize) {
    let len = buf.len();
    if len == 0 {
        out.copy_from_slice(in_plane);
        return;
    }
    let mut p = *pos;
    for i in 0..out.len() {
        out[i] = buf[p];
        buf[p] = in_plane[i];
        p = (p + 1) % len;
    }
    *pos = p;
}

/// Generate one block of a test signal (the `Source` kernel). `phase` is
/// the oscillator phase at block start (mutated to the phase at block
/// end); `fired` tracks whether the impulse has been emitted.
pub fn kernel_source(
    out: &mut [f32],
    params: &SourceParams,
    sample_rate: f32,
    phase: &mut f32,
    fired: &mut bool,
) {
    match params.signal {
        TestSignal::Impulse => {
            out.fill(0.0);
            if !*fired {
                out[0] = 1.0;
                *fired = true;
            }
        }
        TestSignal::Sine => {
            let step = 2.0 * std::f32::consts::PI * params.frequency_hz / sample_rate;
            for (i, s) in out.iter_mut().enumerate() {
                *s = (*phase + step * i as f32).sin();
            }
            *phase = (*phase + step * out.len() as f32) % (2.0 * std::f32::consts::PI);
        }
        TestSignal::Silence => out.fill(0.0),
    }
}

/// Full linear convolution of one block with an FIR kernel
/// (`len(x) + len(h) − 1` samples), writing into caller-owned `y` — the
/// shared form used by both executors (offline passes a fresh `Vec`,
/// realtime passes preallocated scratch).
pub fn direct_convolve_into(x: &[f32], h: &[f32], y: &mut Vec<f32>) {
    y.clear();
    y.resize(x.len() + h.len().saturating_sub(1), 0.0);
    for (i, &xi) in x.iter().enumerate() {
        for (j, &hj) in h.iter().enumerate() {
            y[i + j] += xi * hj;
        }
    }
}

/// Full linear convolution of one block with an FIR kernel, allocating the
/// output — the offline form (the offline executor's contract allows
/// growth; realtime passes preallocated scratch to [`direct_convolve_into`]).
pub fn direct_convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
    let mut y = Vec::with_capacity(x.len() + h.len().saturating_sub(1));
    direct_convolve_into(x, h, &mut y);
    y
}

/// Pad/truncate a measured per-ear HRIR to `taps` so the node's rendered IR
/// length matches its reported latency exactly (zero-pad a shorter dataset,
/// truncate a longer one), writing into caller-owned `out`.
pub fn to_ir_taps_into(ir: &[f32], taps: usize, out: &mut Vec<f32>) {
    let taps = taps.max(1);
    out.clear();
    out.extend_from_slice(&ir[..ir.len().min(taps)]);
    if out.len() < taps {
        out.resize(taps, 0.0);
    }
}

/// The allocating form of [`to_ir_taps_into`] (offline path).
pub fn to_ir_taps(ir: Vec<f32>, taps: usize) -> Vec<f32> {
    let mut v = Vec::new();
    to_ir_taps_into(&ir, taps, &mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_kernel_matches_offline_math() {
        let input = [0.5f32, 1.0, -0.25, 0.75];
        let mut out = [0.0f32; 4];

        // Static gain.
        assert_eq!(kernel_gain(&input, &mut out, 2.0, None, None), 2.0);
        assert_eq!(out, [1.0, 2.0, -0.5, 1.5]);

        // Step at local 2: frames [0,2) at base 0.5, then 2.0.
        assert_eq!(
            kernel_gain(
                &input,
                &mut out,
                0.5,
                Some(GainStep {
                    gain: 2.0,
                    local: 2
                }),
                None
            ),
            2.0
        );
        assert_eq!(out, [0.25, 0.5, -0.5, 1.5]);

        // Automation ramps start→end across the block.
        assert_eq!(
            kernel_gain(&input, &mut out, 0.5, None, Some((0.0, 3.0))),
            0.5
        );
        assert_eq!(out, [0.0, 1.0, -0.5, 2.25]);
    }

    #[test]
    fn mix_kernel_sums_planes() {
        let a = [1.0f32, 2.0];
        let b = [10.0f32, 20.0];
        let mut out = [0.0f32; 2];
        kernel_mix(&[&a, &b], &mut out);
        assert_eq!(out, [11.0, 22.0]);
    }

    #[test]
    fn delay_kernel_reads_before_write() {
        let input = [9.0f32, 8.0, 7.0];
        let mut buf = vec![0.0f32; 4];
        let mut out = [0.0f32; 3];
        let mut pos = 0usize;
        kernel_delay(&input, &mut out, &mut buf, &mut pos);
        assert_eq!(out, [0.0, 0.0, 0.0]);
        // The ring wraps: reads resume at the unwritten slot 3, then the
        // oldest samples 9, 8.
        kernel_delay(&[1.0, 1.0, 1.0], &mut out, &mut buf, &mut pos);
        assert_eq!(out, [0.0, 9.0, 8.0]);
    }

    #[test]
    fn source_kernel_impulse_fires_once() {
        let mut out = [0.0f32; 4];
        let mut phase = 0.0f32;
        let mut fired = false;
        let impulse = SourceParams {
            signal: TestSignal::Impulse,
            frequency_hz: 0.0,
        };
        kernel_source(&mut out, &impulse, 48_000.0, &mut phase, &mut fired);
        assert_eq!(out, [1.0, 0.0, 0.0, 0.0]);
        out.fill(0.0);
        kernel_source(&mut out, &impulse, 48_000.0, &mut phase, &mut fired);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn convolve_helpers_agree() {
        let x = [1.0f32, 2.0];
        let h = [3.0f32, 0.5, 1.0];
        let a = direct_convolve(&x, &h);
        let mut b = Vec::new();
        direct_convolve_into(&x, &h, &mut b);
        assert_eq!(a, b);
        // y[0]=1·3, y[1]=1·0.5+2·3, y[2]=1·1+2·0.5, y[3]=2·1.
        assert_eq!(a, [3.0, 6.5, 2.0, 2.0]);
    }

    #[test]
    fn ir_taps_pad_and_truncate() {
        assert_eq!(to_ir_taps(vec![1.0, 2.0], 4), vec![1.0, 2.0, 0.0, 0.0]);
        assert_eq!(to_ir_taps(vec![1.0, 2.0, 3.0, 4.0], 2), vec![1.0, 2.0]);
        let mut scratch = Vec::new();
        to_ir_taps_into(&[5.0, 6.0], 3, &mut scratch);
        assert_eq!(scratch, vec![5.0, 6.0, 0.0]);
    }
}
