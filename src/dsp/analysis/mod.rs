//! # Spectral & Psychoacoustic Analysis Layer (Item 33).
//!
//! Provides reusable signal-analysis features driving adaptive DSP, metering,
//! and diagnostics:
//! - **Spectral**: Centroid, Spread, Flux, Rolloff, Flatness, Sub-Bass ratio.
//! - **Temporal**: Crest factor, Dynamic range, Transient density, True-peak density.
//! - **Harmonic**: Harmonicity, Tonality index.
//!
//! ### Realtime Safety
//! Analysis runs at control rate with zero allocations during processing blocks.
//! All FFT engines, scratch buffers, and window tables are preallocated during `new()`.

pub mod harmonic;
pub mod spectral;
pub mod temporal;

pub use harmonic::{compute_harmonic_features, HarmonicFeatures};
pub use spectral::{compute_spectral_features, SpectralFeatures};
pub use temporal::{compute_temporal_features, TemporalFeatures};

use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Comprehensive snapshot of all 12 psychoacoustic and spectral features.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisSnapshot {
    pub spectral: SpectralFeatures,
    pub temporal: TemporalFeatures,
    pub harmonic: HarmonicFeatures,
}

/// Analysis engine running STFT and feature calculations.
pub struct AnalysisEngine {
    sample_rate: f32,
    fft_size: usize,
    window: Vec<f32>,
    fft_forward: Arc<dyn RealToComplex<f32>>,
    fft_in: Vec<f32>,
    fft_out: Vec<num_complex::Complex<f32>>,
    mags: Vec<f32>,
    prev_mags: Vec<f32>,
    prev_energy: f32,
}

impl AnalysisEngine {
    pub fn new(sample_rate: f32, fft_size: usize) -> Self {
        let n = fft_size.max(256).next_power_of_two();
        let mut planner = RealFftPlanner::<f32>::new();
        let fft_forward = planner.plan_fft_forward(n);

        // Precompute Hann window
        let mut window = vec![0.0f32; n];
        for (i, w) in window.iter_mut().enumerate() {
            *w = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos());
        }

        let num_bins = n / 2 + 1;

        Self {
            sample_rate: sample_rate.max(1.0),
            fft_size: n,
            window,
            fft_forward,
            fft_in: vec![0.0; n],
            fft_out: vec![num_complex::Complex::default(); num_bins],
            mags: vec![0.0; num_bins],
            prev_mags: vec![0.0; num_bins],
            prev_energy: 0.0,
        }
    }

    /// Process an audio block and compute the complete analysis snapshot. Guaranteed zero allocations.
    pub fn process_block(&mut self, samples: &[f32]) -> AnalysisSnapshot {
        let (temporal, new_energy) =
            compute_temporal_features(samples, self.prev_energy, self.sample_rate);
        self.prev_energy = new_energy;

        // Window and compute FFT
        let copy_len = samples.len().min(self.fft_size);
        for (dst, (&s, &w)) in self.fft_in[..copy_len]
            .iter_mut()
            .zip(samples.iter().zip(&self.window))
        {
            *dst = s * w;
        }
        for dst in &mut self.fft_in[copy_len..self.fft_size] {
            *dst = 0.0;
        }

        let _ = self
            .fft_forward
            .process(&mut self.fft_in, &mut self.fft_out);

        // Compute magnitude spectrum
        for (m, c) in self.mags.iter_mut().zip(self.fft_out.iter()) {
            *m = c.norm();
        }

        let bin_hz = self.sample_rate / (self.fft_size as f32);
        let spectral = compute_spectral_features(&self.mags, &self.prev_mags, bin_hz, 0.85);
        self.prev_mags.copy_from_slice(&self.mags);

        let harmonic = compute_harmonic_features(samples, spectral.flatness);

        AnalysisSnapshot {
            spectral,
            temporal,
            harmonic,
        }
    }

    /// The configured FFT size.
    pub fn fft_size(&self) -> usize {
        self.fft_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analysis_engine_sine_tone() {
        let sr = 48000.0;
        let mut engine = AnalysisEngine::new(sr, 1024);

        // 1000 Hz sine
        let mut sine = vec![0.0f32; 1024];
        for (i, x) in sine.iter_mut().enumerate() {
            *x = 0.8 * (2.0 * std::f32::consts::PI * 1000.0 * (i as f32) / sr).sin();
        }

        let snap = engine.process_block(&sine);

        // Centroid should be around 1000 Hz
        assert!((snap.spectral.centroid_hz - 1000.0).abs() < 100.0);
        // Pure tone has low flatness and high tonality
        assert!(snap.spectral.flatness < 0.15);
        assert!(snap.harmonic.tonality > 0.85);
        // Crest factor ≈ 3 dB
        assert!((snap.temporal.crest_factor_db - 3.01).abs() < 0.5);
    }
}
