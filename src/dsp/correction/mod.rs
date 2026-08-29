//! Room & headphone correction — measurement-to-correction chain (Phase 7,
//! control-path S1–S4).
//!
//! This module is the **control-thread half** of the Phase 7 evolution
//! ([`docs/EVOLUTION.md`](../../../docs/EVOLUTION.md)): it turns a sweep
//! measurement (or an imported IR file) into a phase-rendered correction
//! impulse response. The sub-stage map:
//!
//! ```text
//! S1 sweep    Farina exponential sine sweep: generation, deconvolution,
//!             harmonic-distortion separation, measurement SNR
//! S2 ir       WAV IR import + conditioning (rumble HPF, tail truncation,
//!             peak normalization)
//! S3 phase    minimum / linear / hybrid phase rendering, group delay
//! S4 derive   smoothed, SNR-weighted, boost-clamped regularized inverse
//! ```
//!
//! Everything here runs **on the control thread**: it is heap-happy, `f64`
//! DSP executed once per measurement or configuration change. Nothing in
//! this module is on the realtime audio path — the S5 `CorrectionNode`
//! (`src/dsp/graph/nodes/correction_node.rs`) consumes the pre-rendered
//! IRs from these functions, so the hot-path contract (no allocation, no
//! locks) is untouched by design.
//!
//! Acceptance suites (spec-first, thresholds pinned in the roadmap):
//! `tests/fidelity/ess_measurement.rs`, `tests/fidelity/minimal_phase.rs`,
//! `tests/fidelity/correction_inverse.rs`, and the graph-level
//! `tests/fidelity/room_correction_pipeline.rs` (S5).

pub mod derive;
pub mod ir;
pub mod phase;
pub mod sweep;
#[cfg(test)]
mod tests;

pub use derive::{
    derive_correction_ir, derive_correction_magnitude_db, CorrectionIrSet, DeriveParams,
    TargetCurve,
};
pub use ir::{read_wav_ir, ConditionedIr, IrConditioner, WavIr};
pub use phase::{
    excess_allpass_spectrum, group_delay_exact_samples, group_delay_samples_per_bin,
    minimum_phase_ir, minimum_phase_spectrum, phase_slope_delay_samples, render_from_magnitude_db,
    PhaseMode, RenderParams, RenderedIr, Spectrum,
};
pub use sweep::{
    analyze_harmonics, deconvolve, estimate_snr_db, noise_floor_db, EssConfig, EssSweep, Harmonic,
    HarmonicReport, ImpulseResponse, MAX_ANALYSIS_HARMONIC_ORDER,
};

use num_complex::Complex;
use thiserror::Error;

/// Errors surfaced by the correction chain.
#[derive(Debug, Error)]
pub enum CorrectionError {
    /// The WAV file could not be parsed as a RIFF/WAVE container.
    #[error("WAV parse error in {path}: {message}")]
    WavParse {
        /// File path (for diagnostics).
        path: String,
        /// What is structurally wrong.
        message: String,
    },
    /// The WAV file parses but uses an unsupported sample format.
    #[error("unsupported WAV format in {path}: {message}")]
    WavFormat {
        /// File path (for diagnostics).
        path: String,
        /// Which format/encoding is unsupported.
        message: String,
    },
    /// The IR sample rate does not match the session rate. Resample first —
    /// the engine integration (S5) owns rate alignment via the existing rate
    /// machinery.
    #[error("IR sample rate {ir_hz} Hz does not match session rate {session_hz} Hz; resample before conditioning")]
    RateMismatch {
        /// The IR's own sample rate.
        ir_hz: f64,
        /// The session's sample rate.
        session_hz: f64,
    },
    /// A parameter or input is invalid (empty, out of range, mis-sized).
    #[error("invalid {what}: {message}")]
    InvalidConfig {
        /// Which parameter/input is invalid.
        what: &'static str,
        /// Why it is invalid.
        message: String,
    },
    /// File I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Amplitude floor used wherever a magnitude enters a `ln`/division: −180 dB.
/// Magnitudes below this are measurement noise, never structure.
pub(crate) const MAG_FLOOR_AMP: f64 = 1e-9;

/// A minimal complex FFT wrapper with normalized inverses.
///
/// The correction chain is control-thread DSP, so planning per instance is
/// fine; callers create one `Cfft` per transform size and reuse it for the
/// forward/inverse pairs of that size.
pub(crate) struct Cfft {
    forward: std::sync::Arc<dyn rustfft::Fft<f64>>,
    inverse: std::sync::Arc<dyn rustfft::Fft<f64>>,
    n: usize,
}

impl Cfft {
    /// Plan forward + inverse transforms of length `n` (`n >= 2`).
    pub fn new(n: usize) -> Self {
        debug_assert!(n >= 2, "FFT length must be >= 2");
        let mut planner = rustfft::FftPlanner::<f64>::new();
        Self {
            forward: planner.plan_fft_forward(n),
            inverse: planner.plan_fft_inverse(n),
            n,
        }
    }

    /// Unnormalized forward transform, in place.
    pub fn forward(&self, buf: &mut [Complex<f64>]) {
        self.forward.process(buf);
    }

    /// Inverse transform normalized by `1/n`, in place.
    pub fn inverse(&self, buf: &mut [Complex<f64>]) {
        self.inverse.process(buf);
        let inv_n = 1.0 / self.n as f64;
        for c in buf.iter_mut() {
            *c *= inv_n;
        }
    }
}

/// Map a config-side correction config into S4 derivation parameters
/// (Phase 7 S5). `snr_db` is the measurement's reported SNR: config-driven
/// derives (no live measurement) pass a confident default; `MeasureRoom`
/// passes the deconvolved `estimate_snr_db` so boosts collapse where the
/// measurement was unreliable.
pub fn derive_params_from_config(
    cfg: &config::CorrectionConfig,
    sample_rate: f64,
    ir_len_samples: usize,
    snr_db: f64,
) -> DeriveParams {
    DeriveParams {
        sample_rate,
        ir_len_samples,
        target: match cfg.target {
            config::CorrectionTarget::Flat => TargetCurve::Flat,
            config::CorrectionTarget::Tilt { db_per_octave } => TargetCurve::Tilt {
                db_per_octave: db_per_octave as f64,
            },
            config::CorrectionTarget::Shelf {
                corner_hz,
                low_gain_db,
                high_gain_db,
                slope_octaves,
            } => TargetCurve::Shelf {
                corner_hz: corner_hz as f64,
                low_gain_db: low_gain_db as f64,
                high_gain_db: high_gain_db as f64,
                slope_octaves: slope_octaves as f64,
            },
        },
        max_boost_db: cfg.max_boost_db as f64,
        smoothing_octaves: cfg.smoothing_octaves as f64,
        snr_db,
        phase_mode: match cfg.phase_mode {
            config::CorrectionPhaseMode::Minimum => PhaseMode::Minimum,
            config::CorrectionPhaseMode::Linear => PhaseMode::Linear,
            config::CorrectionPhaseMode::Hybrid => PhaseMode::Hybrid,
        },
        hybrid_crossover_hz: cfg.hybrid_crossover_hz as f64,
    }
}

/// Linear convolution of two real signals via FFT (control-path utility).
///
/// Returns `a.len() + b.len() - 1` samples (empty inputs yield an empty
/// output). Used by the measurement suites and, later, by offline correction
/// validation; never on the audio path.
pub fn convolve(a: &[f64], b: &[f64]) -> Vec<f64> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let out_len = a.len() + b.len() - 1;
    let n = out_len.next_power_of_two().max(16);
    let cfft = Cfft::new(n);
    let mut fa: Vec<Complex<f64>> = a.iter().map(|&x| Complex::new(x, 0.0)).collect();
    fa.resize(n, Complex::new(0.0, 0.0));
    let mut fb: Vec<Complex<f64>> = b.iter().map(|&x| Complex::new(x, 0.0)).collect();
    fb.resize(n, Complex::new(0.0, 0.0));
    cfft.forward(&mut fa);
    cfft.forward(&mut fb);
    for (x, h) in fa.iter_mut().zip(fb.iter()) {
        *x *= *h;
    }
    cfft.inverse(&mut fa);
    fa.truncate(out_len);
    fa.iter().map(|c| c.re).collect()
}
