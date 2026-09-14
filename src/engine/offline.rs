//! Deterministic offline rendering and DSP fidelity verification.
//!
//! # Purpose
//!
//! `OfflineRenderer` processes audio tracks or PCM buffers faster than realtime
//! with zero hardware sleep or OS audio subsystem dependencies. It delivers
//! deterministic, bit-exact rendered output into memory (`Vec<f32>`) and
//! provides quantitative verification helpers (peak level, RMS energy,
//! and delta RMS comparison against golden reference vectors).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use config::EngineConfig;

use crate::buffer::PlaybackState;
use crate::dsp::graph2::prod::Graph2Engine;
use crate::engine::{AudioEngine, EngineError};
use crate::sink::SampleSink;
use crate::source::AudioSource;

/// The rendered audio output resulting from offline processing.
#[derive(Debug, Clone)]
pub struct OfflineRenderResult {
    /// Interleaved output samples.
    pub samples: Vec<f32>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: usize,
    /// Total duration in seconds.
    pub duration_secs: f32,
}

impl OfflineRenderResult {
    /// Number of audio frames rendered (`samples.len() / channels`).
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    /// Maximum absolute peak amplitude across all rendered samples.
    pub fn peak(&self) -> f32 {
        self.samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()))
    }

    /// Root mean square (RMS) signal level across all rendered samples.
    pub fn rms(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = self.samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
        ((sum_sq / self.samples.len() as f64).sqrt()) as f32
    }

    /// Compare rendered output against a reference slice.
    ///
    /// Returns `Ok((max_abs_diff, rms_diff))` on matching length, or `Err(reason)`.
    pub fn diff(&self, reference: &[f32]) -> Result<(f32, f32), String> {
        if self.samples.len() != reference.len() {
            return Err(format!(
                "length mismatch: rendered {} samples vs reference {} samples",
                self.samples.len(),
                reference.len()
            ));
        }

        let mut max_diff = 0.0f32;
        let mut sum_sq_diff = 0.0f64;

        for (&a, &b) in self.samples.iter().zip(reference.iter()) {
            let diff = (a - b).abs();
            if diff > max_diff {
                max_diff = diff;
            }
            sum_sq_diff += (diff as f64) * (diff as f64);
        }

        let rms_diff = (sum_sq_diff / self.samples.len() as f64).sqrt() as f32;
        Ok((max_diff, rms_diff))
    }
}

/// A capturing sample sink that collects all pushed audio frames into memory.
#[derive(Clone)]
struct CapturingSink {
    buffer: Arc<Mutex<Vec<f32>>>,
    channels: Arc<AtomicUsize>,
}

impl CapturingSink {
    fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(Vec::new())),
            channels: Arc::new(AtomicUsize::new(2)),
        }
    }

    fn samples(&self) -> Vec<f32> {
        self.buffer.lock().unwrap().clone()
    }

    fn channels(&self) -> usize {
        self.channels.load(Ordering::Relaxed)
    }
}

impl SampleSink for CapturingSink {
    fn push_interleaved(&self, samples: &[f32], channels: usize) -> usize {
        self.channels.store(channels, Ordering::Relaxed);
        let mut buf = self.buffer.lock().unwrap();
        buf.extend_from_slice(samples);
        samples.len() / channels.max(1)
    }

    fn reset(&self) {
        self.buffer.lock().unwrap().clear();
    }
}

/// Offline audio renderer driving deterministic, faster-than-realtime rendering.
pub struct OfflineRenderer {
    config: EngineConfig,
}

impl OfflineRenderer {
    /// Create a new offline renderer with the specified engine configuration.
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Render an entire [`AudioSource`] through the full audio engine pipeline.
    pub fn render_source(&self, source: &AudioSource) -> Result<OfflineRenderResult, EngineError> {
        let sink = CapturingSink::new();
        let mut engine = AudioEngine::with_sink(self.config.clone(), Box::new(sink.clone()))?;

        engine.load_source(source)?;
        engine.handle_play();

        // Process blocks in a tight loop without wall-clock sleep
        const MAX_ITERATIONS: usize = 2_000_000;
        let mut iterations = 0;

        while engine.current_state() == PlaybackState::Playing && iterations < MAX_ITERATIONS {
            engine.tick();
            iterations += 1;
        }

        let samples = sink.samples();
        let channels = sink.channels();
        let sample_rate = engine.output_sample_rate();
        let duration_secs = engine.duration_secs();

        Ok(OfflineRenderResult {
            samples,
            sample_rate,
            channels,
            duration_secs,
        })
    }

    /// Render an audio file from disk offline.
    pub fn render_file(&self, path: impl AsRef<Path>) -> Result<OfflineRenderResult, EngineError> {
        self.render_source(&AudioSource::File(path.as_ref().to_path_buf()))
    }

    /// Process a raw PCM buffer directly through the Graph2 DSP engine.
    pub fn render_pcm(
        &self,
        input: &[f32],
        channels: usize,
        sample_rate: u32,
    ) -> Result<OfflineRenderResult, EngineError> {
        if channels == 0 {
            return Err(EngineError::Config("channels must be >= 1".into()));
        }

        let mut graph = Graph2Engine::from_config(&self.config, sample_rate as f32);
        if channels > 2 {
            let layout = crate::decode::ChannelLayout::from_count(channels);
            graph.set_multichannel_layout(&layout);
        }

        const BLOCK_FRAMES: usize = 512;
        let total_frames = input.len() / channels;
        let mut output = Vec::with_capacity(input.len());

        if channels == 2 {
            // Stereo processing
            let mut left = Vec::with_capacity(BLOCK_FRAMES);
            let mut right = Vec::with_capacity(BLOCK_FRAMES);

            for chunk in input.chunks(BLOCK_FRAMES * 2) {
                left.clear();
                right.clear();
                for frame in chunk.chunks(2) {
                    left.push(frame[0]);
                    right.push(if frame.len() > 1 { frame[1] } else { frame[0] });
                }

                graph.process_block(&mut left, &mut right);

                for (&l, &r) in left.iter().zip(right.iter()) {
                    output.push(l);
                    output.push(r);
                }
            }
        } else {
            // Multichannel / mono processing
            for chunk in input.chunks(BLOCK_FRAMES * channels) {
                let mut buf = chunk.to_vec();
                graph.process_block_multichannel(&mut buf, channels);
                output.extend_from_slice(&buf);
            }
        }

        let duration_secs = total_frames as f32 / sample_rate as f32;

        Ok(OfflineRenderResult {
            samples: output,
            sample_rate,
            channels,
            duration_secs,
        })
    }
}
