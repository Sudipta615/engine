//! Room-correction measurement and processing infrastructure (spec Point 25).
//!
//! This module is control-path infrastructure: the measurement sweep generation,
//! impulse response capture, analysis, and correction filter computation all run
//! outside the audio callback. Only the final [`correction::RoomCorrectionProcessor`]
//! wraps a pre-prepared `ConvolutionEngine` and is realtime-safe.
//!
//! ## Typical workflow
//!
//! 1. **Measurement**: generate a log-sweep via [`measurement::generate_log_sweep`]
//!    and its matching inverse filter via [`measurement::generate_inverse_filter`].
//!    Play the sweep through the system, capture the recording, then deconvolve via
//!    [`measurement::capture_impulse_response`] to recover the room's IR.
//!
//! 2. **Analysis**: call [`analysis::analyze_ir`] to compute frequency response,
//!    energy-time curve, RT60, phase and group delay.
//!
//! 3. **Correction**: call [`correction::compute_correction_filter`] to derive a
//!    correction FIR filter from the analysis and a target curve. Wrap it in a
//!    [`correction::RoomCorrectionProcessor`] and insert it into the DSP graph.
//!
//! All `Vec`-allocating functions are explicitly **control-path only** (marked
//! as such in their docs). The `RoomCorrectionProcessor::process_block` method is
//! realtime-safe (zero allocation after `prepare`).

pub mod analysis;
pub mod correction;
pub mod filter_synth;
pub mod measurement;
pub mod profile;
pub mod spatial_average;
pub mod target_curve;
pub mod validation;

pub use analysis::{analyze_ir, RoomCorrectionTarget, RoomIrAnalysis};
pub use correction::{
    compute_correction_filter, CorrectionMode, RoomCorrectionFilter, RoomCorrectionProcessor,
};
pub use filter_synth::{
    fit_parametric_eq, synthesize_channel_correction, BiquadFitBand, FilterSynthConfig,
};
pub use measurement::{
    capture_impulse_response, generate_inverse_filter, generate_log_sweep, SweepConfig,
};
pub use profile::{ChannelCorrectionMetrics, CorrectionProfile};
pub use spatial_average::{average_frequency_responses, SpatialAverageStrategy};
pub use target_curve::{TargetCurve, TargetCurveKind};
pub use validation::{
    validate_biquad_bands_stability, validate_correction_filter, validate_multichannel_consistency,
    CorrectionValidationReport,
};
