//! Per-node pipeline state for the Graph 2.0 executors (v3.50 —
//! moved from `exec.rs`).
//!
//! These types own the *streaming* side of each stateful node kind (delay
//! queues, overlap-add accumulators, interpolation history). Both executors
//! share them: the offline executor grows them lazily, while the realtime
//! executor (`crate::dsp::graph2::rt`) preallocates every queue at plan
//! build time — but the math in [`ConvState::push_and_emit`] and
//! [`windowed_sinc`] is one implementation, the shared-kernel contract.

use std::collections::VecDeque;

use crate::dsp::convolution::ConvolutionEngine;
use crate::spatial::acoustic::bake::ACOUSTIC_IR_LEN;

/// Depth (samples) of the raw input-history ring each `Acoustic` node keeps
/// for per-path spectral filtering. Bounded below by room scale (the
/// longest image-source excess delay, several thousand samples at 48 kHz)
/// plus the filter span ([`ACOUSTIC_IR_LEN`]); fixed so the ring fills
/// continuously from session start and never drops history on a scene swap.
pub const ACOUSTIC_HISTORY: usize = ACOUSTIC_IR_LEN + 4096;

/// The per-`Acoustic`-node acoustic rendering state: the object cell
/// currently compiled in (`key`), its per-path spectral filter kernels, and
/// a shared **raw input history ring**.
///
/// The ring holds the *unfiltered* input history, deliberately: delay and
/// filtering are both LTI (they commute), and the raw history is independent
/// of any cell's kernels — so when a scene swap or listener drive retargets
/// the node's cell, only the `paths` kernels change while the ring keeps
/// ringing seamlessly from the continuous session input (the animated-world
/// / golden semantics pinned by v3.35). Each path reads `kernel` convolved
/// against the raw history at its `excess` delay:
/// `out[k] += Σ_j h[j]·x[k − excess − j]`.
#[derive(Debug, Default)]
pub struct AcousticState {
    /// Cache key of the [`crate::spatial::acoustic::bake::BakedObject`]
    /// whose kernels are compiled in `paths`.
    pub(crate) key: Option<(i32, i32, i32)>,
    /// The executor's `acoustic_epoch` these kernels were compiled at; a
    /// bump forces a recompile even when `key` is unchanged (two worlds
    /// can bake the same source cell).
    pub(crate) epoch: u64,
    /// Per non-direct path: `(excess_delay, spectral filter kernel)`.
    pub(crate) paths: Vec<(i64, Vec<f32>)>,
    /// Shared raw input history ring (fixed depth so it never drops session
    /// history on a scene swap).
    pub(crate) raw: Vec<f32>,
    /// Ring write cursor.
    pub(crate) pos: usize,
}

/// The convolution pipeline: the convolved stream delayed by `delay`
/// samples before emission — the algorithmic latency a block-partitioned
/// convolver pays. Initialised with `delay` zeros so the first emitted
/// samples are the convolution at negative indices (zero), exactly
/// `output[k] = (x * h)[k - delay]`.
///
/// Streaming convolution is **overlap-add**: each block's convolved result
/// `y_b` (length `emit + overlap`) ends with `overlap = kernel.len() - 1`
/// samples that continue into the next block, and the next block's leading
/// `overlap` samples must be *added* to them (not dropped — the previous
/// block's tail is the same stream position). The `tail` accumulator
/// carries those addends; only the `emit` final samples of each block are
/// appended to the delay queue, which stays at a constant length, so the
/// pipeline delay never drifts.
#[derive(Debug, Default)]
pub struct ConvState {
    /// The delayed convolution stream; the head (`delay` zeros) makes the
    /// first emitted samples the convolution at negative indices (zero).
    pub(crate) pending: VecDeque<f32>,
    /// The `overlap`-sample addends carried from the previous block.
    pub(crate) tail: Vec<f32>,
}

impl ConvState {
    pub(crate) fn new(delay: usize) -> Self {
        let mut pending = VecDeque::with_capacity(delay + 256);
        pending.extend(std::iter::repeat_n(0.0, delay));
        Self {
            pending,
            tail: Vec::new(),
        }
    }

    /// The realtime constructor: preallocate both the pipeline delay head
    /// and the per-block overlap `tail` (`overlap` frames), so the audio
    /// thread never grows either queue.
    pub(crate) fn with_emit_capacity(delay: usize, emit: usize, overlap: usize) -> Self {
        let mut pending = VecDeque::with_capacity(delay + emit + 16);
        pending.extend(std::iter::repeat_n(0.0, delay));
        Self {
            pending,
            tail: vec![0.0; overlap],
        }
    }

    /// Overlap-add block `y` (length `emit + overlap`) onto the stream,
    /// append its `emit` final samples, and emit the first `emit` samples
    /// (delayed by the initial head). `overlap` is `kernel.len() - 1` — or
    /// `0` for a pass-through ear (no convolution, no addends).
    /// The allocating offline form; the realtime executor calls the
    /// slice-based [`Self::push_and_emit_into`] with preallocated scratch.
    pub(crate) fn push_and_emit(
        &mut self,
        mut y: Vec<f32>,
        overlap: usize,
        emit: usize,
    ) -> Vec<f32> {
        if overlap > 0 {
            let n = overlap.min(y.len());
            if !self.tail.is_empty() {
                for (v, t) in y.iter_mut().zip(self.tail.iter()).take(n) {
                    *v += t;
                }
            }
            let start = y.len().saturating_sub(overlap);
            self.tail = y[start..].to_vec();
        }
        let take = emit.min(y.len());
        self.pending.extend(y.into_iter().take(take));
        let mut out = Vec::with_capacity(emit);
        for _ in 0..emit {
            out.push(self.pending.pop_front().unwrap_or(0.0));
        }
        out
    }

    /// The **realtime** form of [`Self::push_and_emit`]: the exact same
    /// overlap-add math, mutating block `y` in place as the working buffer
    /// (exactly like the allocating form does) and writing the emitted
    /// block into caller-owned `out` (preallocated, `emit` frames). `y`
    /// must be `len(x) + len(h) - 1` frames — the block's full convolved
    /// result. All buffers are preallocated at plan build, so no
    /// allocation happens on the audio path.
    ///
    /// Returns the number of samples written into `out` (always `emit`).
    pub(crate) fn push_and_emit_into(
        &mut self,
        y: &mut [f32],
        overlap: usize,
        emit: usize,
        out: &mut [f32],
    ) -> usize {
        if overlap > 0 {
            let add = overlap.min(y.len());
            if !self.tail.is_empty() {
                for (v, t) in y[..add].iter_mut().zip(self.tail.iter()) {
                    *v += *t;
                }
            }
            // Carry this block's final `overlap` samples for the next one.
            let start = y.len().saturating_sub(overlap);
            self.tail.clear();
            self.tail.extend_from_slice(&y[start..]);
        }
        let take = emit.min(y.len());
        self.pending.extend(y[..take].iter().copied());
        let mut written = 0;
        for o in out.iter_mut().take(emit) {
            *o = self.pending.pop_front().unwrap_or(0.0);
            written += 1;
        }
        written
    }
}

/// Partitioned-FFT convolution state for a long-kernel `Convolution` node,
/// driven by the realtime [`ConvolutionEngine`]. The engine renders long IRs
/// in O(P·2B·log2B) per partition instead of the direct O(N·M), but its
/// UP-OLA latency is one partition `block_size` `B`, **not** the full kernel
/// length. To preserve the node's contract — `output[k] = (x * h)[k - N]`
/// with `N = kernel.len()`, reported by `node_latency` — we seed an
/// `extra_delay` window with `N - B` zeros ahead of the engine stream: the
/// engine emits `o[k] = (x*h)[k - B]`, and popping one sample behind the
/// fresh output yields `o[k - (N - B)] = (x*h)[k - N]`. Delay and convolution
/// are both LTI, so ordering the extra delay *before* emission (equivalently
/// after the engine) is sample-exact.
pub struct FftConvState {
    /// The realtime partitioned overlap-add engine.
    pub(crate) engine: ConvolutionEngine,
    /// `kernel.len() - block_size` leading zeros plus the rolling one-sample-
    /// behind window; each `engine.process` appends and a `pop_front` yields
    /// the sample `N - B` earlier — the node output for that block.
    pub(crate) extra_delay: VecDeque<f32>,
}

impl FftConvState {
    /// Build engine state for a mono `kernel`. `None` if the engine can't
    /// load the IR (caller falls back to the exact direct path).
    pub(crate) fn build(sample_rate: f32, kernel: &[f32]) -> Option<Self> {
        let mut engine = ConvolutionEngine::new(sample_rate, kernel.len());
        let ir: Vec<(f32, f32)> = kernel.iter().map(|&s| (s, s)).collect();
        engine.load_ir_from_samples(&ir).ok()?;
        engine.set_enabled(true);
        engine.set_wet_mix(1.0);
        // The engine's true streaming latency is one partition-minus-one:
        // feeding x[k] returns (x*h)[k - (B - 1)], because the call that
        // completes a block also consumes its first output sample. So the
        // front padding below must be `N - B + 1` for total delay `N`.
        let extra = kernel.len().saturating_sub(engine.block_size()) + 1;
        let mut extra_delay = VecDeque::with_capacity(extra + 512);
        extra_delay.extend(std::iter::repeat_n(0.0, extra));
        Some(Self {
            engine,
            extra_delay,
        })
    }

    /// Feed one block of input and produce the block's node output.
    /// The allocating offline form.
    pub(crate) fn render_block(&mut self, in_plane: &[f32]) -> Vec<f32> {
        let block = in_plane.len();
        let mut out = Vec::with_capacity(block);
        for &x in in_plane {
            let (ol, _) = self.engine.process(x, x);
            self.extra_delay.push_back(ol);
            out.push(self.extra_delay.pop_front().unwrap_or(0.0));
        }
        out
    }
}

/// Streaming state for a `Resampler` node: a windowed-sinc interpolator over
/// an input history ring plus a `quality`-sample output delay, so the node's
/// **reported** taps (`quality`, see `dsp::graph2::latency::node_latency`)
/// equal its actual pipeline delay — the same convention as `Delay` /
/// `Convolution`. The fixed-frame offline executor resamples onto its own
/// frame grid, so `ratio` ≥ 1 (output frames per input frame) is a rate/pitch
/// remap across the same frame count per block: `subpos`, the source position
/// of the next output frame, advances by `1/ratio` per output sample.
pub struct ResamplerState {
    pub(crate) ratio: f32,
    pub(crate) quality: usize,
    /// Appended input samples (source timebase); `base` is the absolute index
    /// of `history[0]`.
    pub(crate) history: VecDeque<f32>,
    pub(crate) base: usize,
    /// Source position (output-frame coordinates) of the next output frame.
    pub(crate) subpos: f32,
    /// The `quality`-sample delay line seeded with zeros (the reported taps).
    pub(crate) pipe: VecDeque<f32>,
}

impl ResamplerState {
    pub(crate) fn new(ratio: f32, quality: usize) -> Self {
        let mut pipe = VecDeque::with_capacity(quality + 16);
        pipe.extend(std::iter::repeat_n(0.0, quality));
        Self {
            ratio: ratio.max(1.0),
            quality: quality.max(1),
            history: VecDeque::new(),
            base: 0,
            subpos: 0.0,
            pipe,
        }
    }

    /// The realtime constructor for a **ratio-1** node. The reachable
    /// window is exactly `2·quality` samples behind the cursor, but the
    /// block is appended **before** the end-of-block trim, so the history
    /// peaks at `2·block + 2·quality + 8` frames — the capacity below
    /// covers that peak so the ring never grows on the audio thread.
    /// (Higher ratios keep an unbounded window on the fixed-grid reader —
    /// [`RtPlan::build`] rejects them.)
    pub(crate) fn with_block(ratio: f32, quality: usize, block: usize) -> Self {
        let q = quality.max(1);
        let window = 2 * q + 2 * block + 32;
        let mut pipe = VecDeque::with_capacity(q + block + 16);
        pipe.extend(std::iter::repeat_n(0.0, q));
        Self {
            ratio: ratio.max(1.0),
            quality: q,
            history: VecDeque::with_capacity(window),
            base: 0,
            subpos: 0.0,
            pipe,
        }
    }

    /// Append the block's input, resample onto the frame grid, and emit one
    /// block delayed by `quality` (the reported taps). Continuity across
    /// blocks comes from the raw history ring + fractional `subpos`; the ring
    /// is trimmed of samples no longer reachable by the interpolation window.
    pub(crate) fn render_block(&mut self, in_plane: &[f32]) -> Vec<f32> {
        self.history.extend(in_plane.iter().copied());
        let block = in_plane.len();
        // The lowest absolute index this block's interpolation window reads is
        // floor(subpos_at_start) - 2·quality. Anything older is dead: trim it
        // after the block. For ratio 1 this keeps the ring at ~2·quality; for
        // ratio > 1 the front advances slower than the append, so the held
        // region grows — bounded by the render length, acceptable offline.
        let lower = (self.subpos.floor() as isize - self.quality as isize * 2 - 8).max(0) as usize;
        let mut out = Vec::with_capacity(block);
        for _ in 0..block {
            let y = windowed_sinc(&self.history, self.base, self.subpos, self.quality);
            self.pipe.push_back(y);
            out.push(self.pipe.pop_front().unwrap_or(0.0));
            self.subpos += 1.0 / self.ratio;
        }
        while self.base < lower {
            if self.history.pop_front().is_none() {
                break;
            }
            self.base += 1;
        }
        out
    }

    /// The **realtime** form of [`Self::render_block`]: the exact same
    /// interpolation + trim math, writing into caller-owned `out`
    /// (preallocated, `block` frames). Only valid for ratio-1 nodes (the
    /// constructor [`Self::with_block`] preallocates the bounded window;
    /// higher ratios are rejected at plan build).
    pub(crate) fn render_block_into(&mut self, in_plane: &[f32], out: &mut [f32]) {
        self.history.extend(in_plane.iter().copied());
        let lower = (self.subpos.floor() as isize - self.quality as isize * 2 - 8).max(0) as usize;
        for o in out.iter_mut() {
            let y = windowed_sinc(&self.history, self.base, self.subpos, self.quality);
            self.pipe.push_back(y);
            *o = self.pipe.pop_front().unwrap_or(0.0);
            self.subpos += 1.0 / self.ratio;
        }
        while self.base < lower {
            if self.history.pop_front().is_none() {
                break;
            }
            self.base += 1;
        }
    }
}

/// One sample of bandlimited interpolation at source position `p` over the
/// appended `history` (whose oldest sample is `base`). A `2·quality`-tap,
/// Hann-windowed sinc reader estimates `x[p]` from its causal neighbours —
/// `|n - p| < quality` contributes — then renormalises by the window so a
/// partially-covered edge doesn't dim the value. `ratio = 1` reproduces the
/// input to interpolator accuracy; `ratio > 1` samples it faster (higher
/// output pitch on the fixed frame grid).
pub(crate) fn windowed_sinc(history: &VecDeque<f32>, base: usize, p: f32, quality: usize) -> f32 {
    let q = quality as f32;
    let i0 = p.floor() as isize;
    let taps = (quality * 2).max(1) as isize;
    let mut acc = 0.0f32;
    let mut wsum = 0.0f32;
    for j in 0..taps {
        let n = i0 - j; // absolute source sample index
        if n < 0 || (n as usize) < base {
            continue; // before the signal start / already trimmed: no weight
        }
        let idx = (n as usize) - base;
        let x = history.get(idx).copied().unwrap_or(0.0);
        let off = n as f32 - p; // = -(j + frac)
        let sinc = if off.abs() < 1e-6 {
            1.0
        } else {
            let a = std::f32::consts::PI * off;
            a.sin() / a
        };
        let win = if off.abs() < q {
            0.5 * (1.0 + (std::f32::consts::PI * off / q).cos())
        } else {
            0.0
        };
        let w = sinc * win;
        acc += x * w;
        wsum += w;
    }
    if wsum.abs() > 1e-12 {
        acc / wsum
    } else {
        0.0
    }
}

/// The two per-ear pipeline queues of an `HRTF` node, both holding the
/// convolution stream back by the longer IR length.
#[derive(Debug, Default)]
pub struct HrtfState {
    pub(crate) left: ConvState,
    pub(crate) right: ConvState,
}

impl HrtfState {
    pub(crate) fn new(delay: usize) -> Self {
        Self {
            left: ConvState::new(delay),
            right: ConvState::new(delay),
        }
    }

    /// The realtime constructor (see [`ConvState::with_emit_capacity`]).
    pub(crate) fn with_emit_capacity(delay: usize, emit: usize) -> Self {
        Self {
            left: ConvState::with_emit_capacity(delay, emit, delay.saturating_sub(1)),
            right: ConvState::with_emit_capacity(delay, emit, delay.saturating_sub(1)),
        }
    }
}

#[derive(Debug, Default)]
pub struct DelayState {
    pub(crate) buf: Vec<f32>,
    pub(crate) pos: usize,
}

#[derive(Debug, Default)]
pub struct SourceState {
    pub(crate) phase: f32,
    pub(crate) fired: bool,
}
