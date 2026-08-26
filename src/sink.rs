//! Pluggable sample sink — the bridge that delivers processed audio to the
//! host application (or to the DAC).
//!
//! The engine's decode loop pushes interleaved f32 samples into a
//! [`SampleSink`] after the resampler and final safety limiter. Different
//! sinks implement different destinations:
//!
//! - [`DacSink`] — the default: pushes into a lock-free [`FixedFrameBuffer`]
//!   ring that a hardware output backend drains to the DAC.
//! - [`NoopSink`] — discards all samples. Useful for headless test harnesses,
//!   loudness scanners, and batch analyzers that only need `PlaybackInfo`
//!   telemetry (position, loudness, stats) without driving actual audio.
//! - [`VecSink`] — collects samples into a `Vec<f32>`. Useful for
//!   off-line analysis, unit tests, and simple audio-file writers.
//!
//! Hosts that need custom destinations (network broadcast, shared-memory
//! visualizer, disk writer) implement the trait and pass it to
//! [`crate::engine::AudioEngine::with_sink`] or inject it after construction.
//!
//! # Realtime safety
//!
//! `push_interleaved` is called from the decode loop — which is the
//! engine's tick thread, not a hardware audio callback — so it may block
//! briefly, but it must not allocate on the steady-state hot path (the
//! trait contract documents this).
//!
//! # Channel count
//!
//! The sink receives samples at the **output channel count** (negotiated
//! with the DAC or set by the host). The engine handles stereo downmix
//! / multichannel passthrough before samples reach the sink.

use std::sync::Arc;

use crate::buffer::FixedFrameBuffer;

/// A destination for processed, interleaved f32 audio samples.
///
/// The engine calls `push_interleaved` after the resampler and final
/// safety limiter. The returned `usize` is the number of **frames**
/// accepted (not samples); the engine retries the remainder on the next
/// tick when the sink reports fewer frames than provided.
pub trait SampleSink: Send {
    /// Push `samples.len() / channels` interleaved f32 frames into the sink.
    ///
    /// Returns the number of frames actually accepted. A return value less
    /// than `samples.len() / channels` means the sink is full or paused;
    /// the engine preserves the unwritten tail and resubmits it.
    ///
    /// # Contract
    ///
    /// - Must be allocation-free in the steady state (warm-up allocations
    ///   for resize are acceptable on the first few calls).
    /// - Must not panic on valid (finite, ±1.0 clamped) samples.
    /// - `channels` ≥ 1, `samples.len()` is a multiple of `channels`.
    fn push_interleaved(&self, samples: &[f32], channels: usize) -> usize;

    /// Discard any buffered samples (called on seek, stop, track change).
    fn reset(&self) {}

    /// Number of frames currently buffered and not yet consumed by the
    /// downstream consumer (for latency estimation). The default returns 0.
    fn buffered_frames(&self) -> usize {
        0
    }
}

// ── DacSink ────────────────────────────────────────────────────────────────

/// The default sink: pushes samples into the engine's lock-free SPSC ring
/// buffer, from which a hardware output backend (ALSA hw:, WASAPI exclusive,
/// CoreAudio hog, ASIO, or cpal shared) drains them to the DAC.
///
/// This is the same `FixedFrameBuffer` the engine used before the
/// `SampleSink` abstraction was introduced — zero observable change for
/// existing hosts.
pub struct DacSink {
    buffer: Arc<FixedFrameBuffer>,
}

impl DacSink {
    pub fn new(buffer: Arc<FixedFrameBuffer>) -> Self {
        Self { buffer }
    }

    /// Access the underlying ring buffer (e.g. so output backends can
    /// attach to it).
    pub fn ring(&self) -> &Arc<FixedFrameBuffer> {
        &self.buffer
    }
}

impl SampleSink for DacSink {
    fn push_interleaved(&self, samples: &[f32], _channels: usize) -> usize {
        // Only whole interleaved frames are pushed: a partially-filled ring
        // would otherwise leave a trailing partial frame in the buffer that
        // misaligns the consumer's frame grouping (a latent data-corruption
        // bug when the ring is nearly full).
        self.buffer.push_frames_interleaved(samples, _channels)
    }

    fn reset(&self) {
        self.buffer.reset();
    }

    fn buffered_frames(&self) -> usize {
        self.buffer.available()
    }
}

// ── NoopSink ───────────────────────────────────────────────────────────────

/// A sink that accepts and discards every sample. Use this when you want
/// the engine to decode and run DSP (for loudness measurement, position
/// tracking, stats) without driving a hardware output.
///
/// The return value is always the full frame count — the engine never
/// stalls against a full ring buffer.
pub struct NoopSink;

impl SampleSink for NoopSink {
    fn push_interleaved(&self, samples: &[f32], channels: usize) -> usize {
        samples.len() / channels.max(1)
    }
}

// ── VecSink ────────────────────────────────────────────────────────────────

/// A sink that collects every sample into a `Vec<f32>`. Useful for
/// off-line analysis, unit tests, and simple file writers.
///
/// The inner `Vec` is protected by a `Mutex` so the host can drain
/// it from another thread. For the lowest overhead, prefer `NoopSink`
/// and inspect the engine's telemetry instead.
pub struct VecSink {
    buffer: std::sync::Mutex<Vec<f32>>,
}

impl VecSink {
    pub fn new() -> Self {
        Self {
            buffer: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Take all accumulated samples (clears the internal buffer).
    pub fn take(&self) -> Vec<f32> {
        let mut buf = self.buffer.lock().unwrap();
        std::mem::take(&mut *buf)
    }

    /// Append accumulated samples without clearing.
    pub fn clone_samples(&self) -> Vec<f32> {
        self.buffer.lock().unwrap().clone()
    }
}

impl Default for VecSink {
    fn default() -> Self {
        Self::new()
    }
}

impl SampleSink for VecSink {
    fn push_interleaved(&self, samples: &[f32], _channels: usize) -> usize {
        let mut buf = self.buffer.lock().unwrap();
        buf.extend_from_slice(samples);
        samples.len() / _channels.max(1)
    }

    fn reset(&self) {
        self.buffer.lock().unwrap().clear();
    }
}
