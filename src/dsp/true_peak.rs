//! Shared true-peak measurement (ITU-R BS.1770-4 §2.3).
//!
//! The ITU recommendation defines **true peak** as the maximum of the
//! *reconstructed* waveform — i.e. the peaks a DAC's reconstruction filter
//! would produce — not the largest discrete sample.  A near-Nyquist signal
//! can overshoot its largest sample by several dB, so a sample-domain meter
//! is not a true-peak meter.
//!
//! This module owns the engine's single 4× polyphase FIR true-peak detector.
//! It is consumed by:
//!
//! - the [`crate::dsp::limiter::LookaheadLimiter`] peak envelope detector,
//! - the [`crate::dsp::LoudnessMeter`],
//! - the offline loudness scanner (`crate::decode::scanner`),
//!
//! so there is deliberately exactly **one** definition of "true peak" in the
//! engine.  Playback diagnostics that report dBTP always come from this
//! implementation.
//!
//! # Filter design
//!
//! The prototype is a [`TRUE_PEAK_FIR_TAPS`]-tap Kaiser-windowed sinc low-pass
//! designed in the 4× domain (i.e. at 4 × the source sample rate) with:
//!
//! | quantity | target |
//! |---|---|
//! | passband edge | 5/48 cycles/sample (20 kHz at a 48 kHz baseband) |
//! | stopband edge | 6/48 cycles/sample (24 kHz at a 48 kHz baseband) |
//! | passband ripple | < 0.01 dB |
//! | stopband attenuation | ≥ 100 dB |
//!
//! The coefficients are normalised so each polyphase branch has unity DC
//! gain (the prototype sums to [`FIR_BRANCHES`] = 4).  The design is generated
//! once at runtime and shared, so the meter's hot path performs no per-sample
//! coefficient construction.
//!
//! # Detector delay
//!
//! The prototype is linear phase, so its group delay is
//! `(N - 1) / 2` output samples = `(N - 1) / 8` input samples.
//! [`detector_delay_samples`] exposes that delay (rounded up) so consumers
//! that need sample-accurate alignment — the lookahead limiter — can add it
//! to their own delay line instead of relying on the lookahead window to
//! absorb the offset.

use std::sync::OnceLock;

use crate::dsp_utils::flush_denormal_f64;

/// Number of taps in the prototype 4× interpolation FIR. A multiple of
/// [`FIR_BRANCHES`] so every polyphase branch has the same tap count.
pub const TRUE_PEAK_FIR_TAPS: usize = 400;

/// Number of polyphase branches (= oversampling factor).
pub const FIR_BRANCHES: usize = 4;
/// Taps per polyphase branch.
pub const BRANCH_TAPS: usize = TRUE_PEAK_FIR_TAPS / FIR_BRANCHES;

/// Reference to the prototype coefficients (for tests / diagnostics).
///
/// Coefficients are computed on first use and cached for the life of the
/// process; the returned slice is `'static`.
pub fn prototype_coefficients() -> &'static [f64] {
    static PROTO: OnceLock<Vec<f64>> = OnceLock::new();
    PROTO.get_or_init(design_prototype).as_slice()
}

/// Group delay of the linear-phase detector in *input* samples (rounded up).
///
/// The prototype has `(N - 1) / 2` output samples of group delay at the 4×
/// rate, i.e. `(N - 1) / 8` input samples; rounding up to `N / (2·branches)`
/// means a limiter that adds this to its lookahead delay never runs short.
pub const fn detector_delay_samples() -> usize {
    TRUE_PEAK_FIR_TAPS / (2 * FIR_BRANCHES)
}

/// Design the Kaiser-windowed sinc prototype at runtime.
fn design_prototype() -> Vec<f64> {
    // 4× interpolation low-pass. Normalised to the 4× rate:
    //   passband 0 .. 5/48 cyc/sample  (20 kHz @ 48 kHz baseband)
    //   stopband 6/48 .. 0.5           (24 kHz @ 48 kHz baseband)
    // Target 120 dB stopband so the measured attenuation clears 100 dB with
    // comfortable margin (window design is approximate).
    let passband_edge = 5.0 / 48.0;
    let stopband_edge = 6.0 / 48.0;
    let cutoff = (passband_edge + stopband_edge) / 2.0;
    let attenuation_db = 120.0;
    let beta = 0.1102 * (attenuation_db - 8.7);

    let n = TRUE_PEAK_FIR_TAPS;
    let center = (n - 1) as f64 / 2.0;
    let i0_beta = bessel_i0(beta);

    let mut h = Vec::with_capacity(n);
    for i in 0..n {
        let x = i as f64 - center;
        // Ideal low-pass impulse response for cutoff (cycles/sample):
        // 2·fc·sinc(2·fc·x).
        let ideal = if x.abs() < 1e-12 {
            2.0 * cutoff
        } else {
            (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
        };
        let window = bessel_i0(beta * (1.0 - (x / center).powi(2)).sqrt()) / i0_beta;
        h.push(ideal * window);
    }

    // A 4× interpolator inserts three zero samples per input sample, so the
    // prototype must have DC gain 4.0 to be unity in the passband.
    let sum: f64 = h.iter().sum();
    let scale = (FIR_BRANCHES as f64) / sum;
    for v in &mut h {
        *v *= scale;
    }
    h
}

/// Modified Bessel function of the first kind, order zero.
///
/// `I0(x) = Σ_k ((x/2)^k / k!)²` — sufficient for the Kaiser window argument
/// range used here (|x| ≤ ~12.3).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0_f64;
    let mut term = 1.0_f64;
    let half = x / 2.0;
    let mut k = 1.0_f64;
    loop {
        term *= half / k;
        let add = term * term;
        sum += add;
        if add <= 1e-18 * sum {
            break;
        }
        k += 1.0;
    }
    sum
}

/// Per-channel 4× polyphase FIR true-peak detector.
///
/// Keeps a circular history of `BRANCH_TAPS` samples and, for each input
/// sample, evaluates all 4 polyphase interpolation points, returning the
/// maximum absolute value among them (and the sample itself).  It also
/// tracks running maxima for metering.
#[derive(Clone)]
pub struct TruePeakMeter {
    buf: [f64; BRANCH_TAPS],
    pos: usize,
    max_true_peak_linear: f64,
    max_sample_peak_linear: f64,
}

impl Default for TruePeakMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl TruePeakMeter {
    pub const fn new() -> Self {
        Self {
            buf: [0.0; BRANCH_TAPS],
            pos: 0,
            max_true_peak_linear: 0.0,
            max_sample_peak_linear: 0.0,
        }
    }

    /// Feed one sample (f64) and return the 4×-oversampled true-peak
    /// magnitude for this sample: `max(|sample|, |4 polyphase points|)`.
    #[inline]
    pub fn process_sample(&mut self, sample: f64) -> f64 {
        self.buf[self.pos] = sample;
        self.pos = (self.pos + 1) % BRANCH_TAPS;

        let proto = prototype_coefficients();
        let mut max_abs = sample.abs();
        for branch in 0..FIR_BRANCHES {
            let mut acc = 0.0_f64;
            for tap in 0..BRANCH_TAPS {
                let coeff_idx = branch + tap * FIR_BRANCHES;
                let buf_idx = (self.pos + BRANCH_TAPS - 1 - tap) % BRANCH_TAPS;
                acc += self.buf[buf_idx] * proto[coeff_idx];
            }
            max_abs = max_abs.max(acc.abs());
        }
        max_abs = flush_denormal_f64(max_abs);

        self.max_sample_peak_linear = self.max_sample_peak_linear.max(sample.abs());
        self.max_true_peak_linear = self.max_true_peak_linear.max(max_abs);
        max_abs
    }

    /// Reset the filter history and running maxima.
    pub fn reset(&mut self) {
        self.buf = [0.0; BRANCH_TAPS];
        self.pos = 0;
        self.max_true_peak_linear = 0.0;
        self.max_sample_peak_linear = 0.0;
    }

    /// Reset only the running maxima (keeps filter history).
    pub fn reset_peak_meters(&mut self) {
        self.max_true_peak_linear = 0.0;
        self.max_sample_peak_linear = 0.0;
    }

    /// Maximum true peak (linear) observed since the last reset.
    pub fn max_true_peak_linear(&self) -> f64 {
        self.max_true_peak_linear
    }

    /// Maximum sample peak (linear) observed since the last reset.
    pub fn max_sample_peak_linear(&self) -> f64 {
        self.max_sample_peak_linear
    }

    /// Maximum true peak in dBTP since the last reset (‑144 dB floor).
    pub fn max_true_peak_dbtp(&self) -> f32 {
        if self.max_true_peak_linear > 0.0 {
            (20.0 * self.max_true_peak_linear.log10()).max(-144.0) as f32
        } else {
            -144.0
        }
    }

    /// Maximum sample peak in dBFS since the last reset (‑144 dB floor).
    pub fn max_sample_peak_db(&self) -> f32 {
        if self.max_sample_peak_linear > 0.0 {
            (20.0 * self.max_sample_peak_linear.log10()).max(-144.0) as f32
        } else {
            -144.0
        }
    }
}

/// DC gain of each polyphase branch (each should be ~1.0).
pub fn branch_dc_gains() -> [f64; FIR_BRANCHES] {
    let proto = prototype_coefficients();
    let mut gains = [0.0_f64; FIR_BRANCHES];
    for branch in 0..FIR_BRANCHES {
        let mut sum = 0.0_f64;
        for tap in 0..BRANCH_TAPS {
            sum += proto[branch + tap * FIR_BRANCHES];
        }
        gains[branch] = sum;
    }
    gains
}

/// Theoretical frequency response of the prototype filter at `freq_hz` in a
/// `sample_rate`-Hz system.  Returns `(magnitude_linear, phase_radians)`.
///
/// The prototype operates at 4× the given rate; frequencies are interpreted
/// in that 4× domain (so `freq = sample_rate / 2` is baseband Nyquist and
/// `freq = 2 × sample_rate` is the 4× Nyquist).
pub fn frequency_response(freq_hz: f32, sample_rate: f32) -> (f64, f64) {
    if sample_rate <= 0.0 || freq_hz < 0.0 {
        return (1.0, 0.0);
    }
    let fs_4x = (sample_rate as f64) * (FIR_BRANCHES as f64);
    let w = 2.0 * std::f64::consts::PI * (freq_hz as f64) / fs_4x;

    let mut re = 0.0_f64;
    let mut im = 0.0_f64;
    for (n, &h) in prototype_coefficients().iter().enumerate() {
        let angle = -w * (n as f64);
        re += h * angle.cos();
        im += h * angle.sin();
    }
    let mag = (re * re + im * im).sqrt();
    let phase = im.atan2(re);
    (mag, phase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dc_gain_and_symmetry() {
        let proto = prototype_coefficients();
        assert_eq!(proto.len(), TRUE_PEAK_FIR_TAPS);
        for i in 0..proto.len() / 2 {
            assert!(
                (proto[i] - proto[proto.len() - 1 - i]).abs() < 1e-12,
                "asymmetric filter at tap {i}"
            );
        }
        let total: f64 = proto.iter().sum();
        assert!((total - FIR_BRANCHES as f64).abs() < 1e-9);
        for (i, &g) in branch_dc_gains().iter().enumerate() {
            assert!((g - 1.0).abs() < 1e-4, "branch {i} DC gain {g}");
        }
    }

    #[test]
    fn test_dc_passthrough() {
        let mut m = TruePeakMeter::new();
        // Prime the filter with the constant, then reset the meters so the
        // step-response transient of the FIR is excluded — the steady-state
        // reconstruction must be exact.
        for _ in 0..(BRANCH_TAPS * 2) {
            m.process_sample(0.5);
        }
        m.reset_peak_meters();
        for _ in 0..200 {
            m.process_sample(0.5);
        }
        assert!(
            (m.max_true_peak_linear() - 0.5).abs() < 1e-4,
            "DC true peak drifted: {}",
            m.max_true_peak_linear()
        );
    }

    #[test]
    fn test_detects_intersample_peak() {
        // fs/4 sine, 45° phase: sample peak = 0.7071, true peak ≈ 1.0.
        let sr = 48000.0f64;
        let mut m = TruePeakMeter::new();
        for i in 0..200 {
            let s = (2.0 * std::f64::consts::PI * 12000.0 * i as f64 / sr
                + std::f64::consts::FRAC_PI_4)
                .sin();
            m.process_sample(s);
        }
        assert!(m.max_sample_peak_linear() < 0.72);
        assert!(
            m.max_true_peak_linear() > 0.95,
            "true peak must overshoot samples"
        );
    }

    #[test]
    fn test_reset_clears_peaks() {
        let mut m = TruePeakMeter::new();
        for _ in 0..(BRANCH_TAPS * 2) {
            m.process_sample(0.9);
        }
        assert!(m.max_true_peak_linear() > 0.8);
        m.reset_peak_meters();
        assert_eq!(m.max_true_peak_linear(), 0.0);
        assert_eq!(m.max_sample_peak_linear(), 0.0);
    }
}
