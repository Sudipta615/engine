//! Acoustic measurement and room analysis subsystem (§11.1, Item 34).
//!
//! Provides logarithmic sine sweep and MLS deconvolution, windowing,
//! transfer function estimation, frequency/phase/group-delay response,
//! RT60/EDT reverberation decay, clarity metrics (C50/C80), and ETC envelopes.

pub mod clarity;
pub mod edt;
pub mod etc;
pub mod frequency_response;
pub mod group_delay;
pub mod impulse;
pub mod mls;
pub mod phase_response;
pub mod rt60;
pub mod sweep;
pub mod transfer_function;

pub use clarity::{compute_clarity, ClarityMetrics};
pub use edt::{compute_edt, EdtMetrics};
pub use etc::{compute_etc, EnergyTimeCurve};
pub use frequency_response::{
    compute_magnitude_response, smooth_frequency_response, FrequencyResponse, OctaveSmoothing,
};
pub use group_delay::{compute_group_delay, GroupDelay};
pub use impulse::{
    apply_window, extract_direct_sound, find_direct_peak, generate_window, normalize_peak,
    WindowType,
};
pub use mls::{deconvolve_mls, fast_hadamard_transform, generate_mls, MlsOrder};
pub use phase_response::{compute_phase_response, unwrap_phase, PhaseResponse};
pub use rt60::{compute_rt60, schroeder_decay_curve, Rt60Metrics};
pub use sweep::{deconvolve_sweep, generate_inverse_filter, generate_log_sweep, LogSweepConfig};
pub use transfer_function::{estimate_transfer_function, TransferFunctionEstimate};

use serde::{Deserialize, Serialize};

/// Comprehensive acoustic evaluation report for a measured impulse response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcousticReport {
    /// Peak direct arrival sample index.
    pub direct_peak_index: usize,
    /// Peak direct arrival amplitude.
    pub direct_peak_amplitude: f32,
    /// RT60 metrics (T20, T30).
    pub rt60: Rt60Metrics,
    /// Early Decay Time metrics.
    pub edt: EdtMetrics,
    /// Acoustic clarity metrics (C50, C80, D50, Center Time).
    pub clarity: ClarityMetrics,
    /// 1/3-octave smoothed frequency response.
    pub frequency_response_1_3: FrequencyResponse,
    /// Frequency-dependent group delay.
    pub group_delay: GroupDelay,
}

/// Analyzes an acoustic impulse response and computes a complete structured acoustic report.
pub fn analyze_acoustics(ir: &[f32], sample_rate: f64) -> AcousticReport {
    let (direct_peak_index, direct_peak_amplitude) = find_direct_peak(ir);
    let rt60 = compute_rt60(ir, sample_rate);
    let edt = compute_edt(ir, sample_rate);
    let clarity = compute_clarity(ir, sample_rate);

    let raw_freq_resp = compute_magnitude_response(ir, sample_rate);
    let frequency_response_1_3 =
        smooth_frequency_response(&raw_freq_resp, OctaveSmoothing::Octave1_3);

    let phase_resp = compute_phase_response(ir, sample_rate);
    let group_delay = compute_group_delay(&phase_resp);

    AcousticReport {
        direct_peak_index,
        direct_peak_amplitude,
        rt60,
        edt,
        clarity,
        frequency_response_1_3,
        group_delay,
    }
}
