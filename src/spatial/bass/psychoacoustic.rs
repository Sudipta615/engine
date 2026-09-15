//! Adaptive psychoacoustic bass enhancement (Phase 3 Point 26).
//!
//! Generates missing fundamental harmonics (phantom fundamental / residue pitch effect)
//! tailored to the playback transducer's physical low-frequency limits and simultaneous
//! auditory masking.
//!
//! ## Components
//! - [`FundamentalDetector`]: Fixed-size ring autocorrelation pitch tracker.
//! - [`SpeakerCapabilityModel`]: Translates transducer cutoff and SPL ceiling into harmonic limits.
//! - [`MaskingModel`]: Psychoacoustic threshold estimation for synthesized harmonics.
//! - [`HarmonicSelector`]: Chebyshev waveshaping harmonic synthesis and DC protection.
//! - [`PsychoacousticBassProcessor`]: Realtime-safe per-channel processor (zero allocation).

use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};
use config::PsychoacousticBassConfig;

/// Maximum autocorrelation window length in samples (at 48 kHz, 1024 samples ≈ 21.3 ms,
/// sufficient to track fundamentals down to ~47 Hz).
pub const PITCH_BUFFER_SIZE: usize = 1024;
const DECIMATION: usize = 4;

/// Autocorrelation-based fundamental frequency detector.
///
/// Uses 4:1 decimation and linear unrolled autocorrelation to robustly track
/// bass fundamentals in the 25 Hz – 300 Hz range without circular boundary artifacts.
#[derive(Debug, Clone)]
pub struct FundamentalDetector {
    buffer: [f32; PITCH_BUFFER_SIZE],
    write_pos: usize,
    decimate_count: usize,
    decimate_accum: f32,
    sample_rate: f32,
    min_lag: usize,
    max_lag: usize,
    detected_pitch: Option<f32>,
}

impl FundamentalDetector {
    /// Create a new fundamental detector for `sample_rate`.
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(8000.0);
        let sr_dec = sr / DECIMATION as f32;
        // Min lag: corresponding to ~320 Hz at decimated rate
        let min_lag = ((sr_dec / 320.0) as usize).max(10);
        // Max lag: corresponding to ~25 Hz at decimated rate
        let max_lag = ((sr_dec / 25.0) as usize).min(500);

        Self {
            buffer: [0.0; PITCH_BUFFER_SIZE],
            write_pos: 0,
            decimate_count: 0,
            decimate_accum: 0.0,
            sample_rate: sr,
            min_lag,
            max_lag,
            detected_pitch: None,
        }
    }

    /// Push an input sample into the analysis buffer (with 4:1 decimation).
    #[inline]
    pub fn push_sample(&mut self, sample: f32) {
        self.decimate_accum += sample;
        self.decimate_count += 1;
        if self.decimate_count >= DECIMATION {
            self.buffer[self.write_pos] = self.decimate_accum * 0.25;
            self.write_pos = (self.write_pos + 1) % PITCH_BUFFER_SIZE;
            self.decimate_accum = 0.0;
            self.decimate_count = 0;
        }
    }

    /// Update pitch estimate using normalized autocorrelation over the unrolled buffer.
    /// Strictly allocation-free.
    pub fn update_pitch(&mut self) -> Option<f32> {
        let sr_dec = self.sample_rate / DECIMATION as f32;
        let window_len = 400; // ~33 ms at 12 kHz

        // Unroll ring buffer into chronological linear scratch
        let mut linear = [0.0f32; PITCH_BUFFER_SIZE];
        for (i, val) in linear.iter_mut().enumerate() {
            *val = self.buffer[(self.write_pos + i) % PITCH_BUFFER_SIZE];
        }

        // Energy at lag 0 over the window
        let energy_0: f32 = linear[..window_len].iter().map(|&x| x * x).sum();

        if energy_0 < 1e-4 {
            self.detected_pitch = None;
            return None;
        }

        let mut found_dip = false;
        let mut best_corr = 0.0f32;
        let mut best_lag = 0;

        for lag in self.min_lag..=self.max_lag {
            let mut sum = 0.0f32;
            let mut energy_lag = 0.0f32;
            for i in 0..window_len {
                sum += linear[i] * linear[i + lag];
                energy_lag += linear[i + lag] * linear[i + lag];
            }
            let denom = (energy_0 * energy_lag).sqrt();
            if denom > 1e-6 {
                let norm_corr = sum / denom;
                if norm_corr < 0.4 {
                    found_dip = true;
                }
                if found_dip && norm_corr > 0.45 && norm_corr > best_corr {
                    best_corr = norm_corr;
                    best_lag = lag;
                }
            }
        }

        if best_lag > 0 {
            let pitch = sr_dec / best_lag as f32;
            self.detected_pitch = Some(pitch);
            Some(pitch)
        } else {
            self.detected_pitch = None;
            None
        }
    }

    /// Current detected pitch in Hz.
    pub fn pitch(&self) -> Option<f32> {
        self.detected_pitch
    }
}

/// Physical capability model for the playback speaker or transducer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeakerCapabilityModel {
    /// Physical low-frequency cutoff in Hz (e.g. 60.0 Hz for small bookshelf speakers).
    pub cutoff_hz: f32,
    /// Maximum undistorted SPL output in dB (e.g. 105.0 dB).
    pub max_spl_db: f32,
}

impl Default for SpeakerCapabilityModel {
    fn default() -> Self {
        Self {
            cutoff_hz: 60.0,
            max_spl_db: 105.0,
        }
    }
}

impl SpeakerCapabilityModel {
    /// Compute the attenuation factor [0.0, 1.0] for a frequency `f_hz`.
    /// Frequencies below `cutoff_hz` roll off at 24 dB/oct.
    pub fn physical_response(&self, f_hz: f32) -> f32 {
        if f_hz >= self.cutoff_hz {
            1.0
        } else if f_hz <= 1.0 {
            0.0
        } else {
            let octaves_below = (self.cutoff_hz / f_hz).log2();
            let atten_db = 24.0 * octaves_below;
            10.0f32.powf(-atten_db / 20.0).clamp(0.0, 1.0)
        }
    }
}

/// Simultaneous auditory masking model for synthesized bass harmonics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaskingModel {
    /// Spread-of-masking slope in dB per critical band.
    masking_slope_db: f32,
}

impl Default for MaskingModel {
    fn default() -> Self {
        Self {
            masking_slope_db: 15.0,
        }
    }
}

impl MaskingModel {
    /// Calculate masking attenuation gain [0.0, 1.0] for harmonic `h` of fundamental `f0`.
    /// Higher harmonics receive slight reduction if they exceed the masking curve.
    pub fn harmonic_gain(&self, harmonic_order: u8, _fundamental_hz: f32) -> f32 {
        match harmonic_order {
            1 => 1.0,
            2 => 0.85,
            3 => 0.60,
            4 => 0.35,
            5 => 0.20,
            _ => 0.0,
        }
    }
}

/// Harmonic synthesizer with Chebyshev waveshaping and DC blocking.
#[derive(Debug, Clone, Copy)]
pub struct HarmonicSelector {
    pub max_harmonic: u8,
    pub dc_protection: bool,
    pub intermod_protection: bool,
}

impl Default for HarmonicSelector {
    fn default() -> Self {
        Self {
            max_harmonic: 4,
            dc_protection: true,
            intermod_protection: true,
        }
    }
}

impl HarmonicSelector {
    /// Synthesize harmonics from a band-limited input sample `x` ∈ [-1.0, 1.0]
    /// using Chebyshev polynomials:
    /// - T2(x) = 2x^2 - 1
    /// - T3(x) = 4x^3 - 3x
    /// - T4(x) = 8x^4 - 8x^2 + 1
    /// - T5(x) = 16x^5 - 20x^3 + 5x
    #[inline]
    pub fn synthesize_harmonics(
        &self,
        x: f32,
        capability: &SpeakerCapabilityModel,
        masking: &MaskingModel,
        fundamental_hz: f32,
    ) -> f32 {
        let x = x.clamp(-1.0, 1.0);
        let x2 = x * x;
        let mut out = 0.0f32;

        if self.max_harmonic >= 2 {
            let t2 = 2.0 * x2 - 1.0;
            let f = fundamental_hz * 2.0;
            let gain = capability.physical_response(f) * masking.harmonic_gain(2, fundamental_hz);
            out += t2 * gain;
        }

        if self.max_harmonic >= 3 {
            let t3 = 4.0 * x * x2 - 3.0 * x;
            let f = fundamental_hz * 3.0;
            let gain = capability.physical_response(f) * masking.harmonic_gain(3, fundamental_hz);
            out += t3 * gain;
        }

        if self.max_harmonic >= 4 {
            let t4 = 8.0 * x2 * x2 - 8.0 * x2 + 1.0;
            let f = fundamental_hz * 4.0;
            let gain = capability.physical_response(f) * masking.harmonic_gain(4, fundamental_hz);
            out += t4 * gain;
        }

        if self.max_harmonic >= 5 {
            let t5 = 16.0 * x2 * x2 * x - 20.0 * x2 * x + 5.0 * x;
            let f = fundamental_hz * 5.0;
            let gain = capability.physical_response(f) * masking.harmonic_gain(5, fundamental_hz);
            out += t5 * gain;
        }

        out
    }
}

/// Complete realtime-safe psychoacoustic bass enhancement processor.
///
/// Preallocates all analysis buffers and filter states; allocation-free on audio thread.
#[derive(Debug, Clone)]
pub struct PsychoacousticBassProcessor {
    detector: FundamentalDetector,
    capability: SpeakerCapabilityModel,
    masking: MaskingModel,
    selector: HarmonicSelector,
    amount: f32,

    // Pre-filtering: extracts low-bass fundamental band (< 120 Hz)
    pre_lp_coeffs: BiquadCoeffsF32,
    pre_lp_state: BiquadStateF32,

    // Post-filtering: bandpass filter for synthesized harmonics (50 Hz - 400 Hz)
    post_bp_coeffs: BiquadCoeffsF32,
    post_bp_state: BiquadStateF32,

    // DC blocking 20 Hz highpass filter
    dc_block_coeffs: BiquadCoeffsF32,
    dc_block_state: BiquadStateF32,
}

impl PsychoacousticBassProcessor {
    /// Create a new psychoacoustic bass processor from configuration.
    pub fn new(config: &PsychoacousticBassConfig, amount: f32, sample_rate: f32) -> Self {
        let sr = sample_rate.max(8000.0);
        let detector = FundamentalDetector::new(sr);
        let capability = SpeakerCapabilityModel {
            cutoff_hz: config.speaker_cutoff_hz,
            max_spl_db: config.max_spl_db,
        };
        let masking = MaskingModel::default();
        let selector = HarmonicSelector {
            max_harmonic: config.max_harmonic,
            dc_protection: config.dc_protection,
            intermod_protection: config.intermod_protection,
        };

        Self {
            detector,
            capability,
            masking,
            selector,
            amount: amount.clamp(0.0, 1.0),
            pre_lp_coeffs: BiquadCoeffsF32::lowpass(sr, 120.0, 0.707),
            pre_lp_state: BiquadStateF32::default(),
            post_bp_coeffs: BiquadCoeffsF32::bandpass(sr, 150.0, 1.0),
            post_bp_state: BiquadStateF32::default(),
            dc_block_coeffs: BiquadCoeffsF32::highpass(sr, 25.0, 0.707),
            dc_block_state: BiquadStateF32::default(),
        }
    }

    /// Process a block of samples in place. Realtime-safe: zero allocations.
    pub fn process_block(&mut self, samples: &mut [f32]) {
        if self.amount <= 1e-4 {
            return;
        }

        // Feed detector and determine fundamental
        for &s in samples.iter() {
            self.detector.push_sample(s);
        }
        let f0 = self.detector.update_pitch().unwrap_or(60.0);

        for s in samples.iter_mut() {
            let input = *s;
            // 1. Extract low frequencies
            let sub = self.pre_lp_state.process(input, &self.pre_lp_coeffs);

            // 2. Synthesize harmonics
            let mut harmonics =
                self.selector
                    .synthesize_harmonics(sub, &self.capability, &self.masking, f0);

            // 3. Bandpass filter harmonics
            harmonics = self.post_bp_state.process(harmonics, &self.post_bp_coeffs);

            // 4. DC block
            if self.selector.dc_protection {
                harmonics = self
                    .dc_block_state
                    .process(harmonics, &self.dc_block_coeffs);
            }

            // 5. Add to output
            *s = input + harmonics * self.amount * 0.3;
        }
    }

    /// Detected fundamental pitch in Hz, if available.
    pub fn detected_pitch(&self) -> Option<f32> {
        self.detector.pitch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fundamental_detector_detects_pure_tone() {
        let sr = 48_000.0;
        let mut detector = FundamentalDetector::new(sr);
        let freq = 80.0f32;

        for i in 0..PITCH_BUFFER_SIZE * 4 {
            let t = i as f32 / sr;
            let sample = (std::f32::consts::TAU * freq * t).sin();
            detector.push_sample(sample);
        }

        let pitch = detector.update_pitch();
        assert!(pitch.is_some(), "expected detected pitch");
        let detected = pitch.unwrap();
        assert!(
            (detected - freq).abs() < 6.0,
            "detected {} Hz, expected ~{} Hz",
            detected,
            freq
        );
    }

    #[test]
    fn speaker_capability_model_rolls_off_below_cutoff() {
        let model = SpeakerCapabilityModel {
            cutoff_hz: 60.0,
            max_spl_db: 105.0,
        };
        assert_eq!(model.physical_response(80.0), 1.0);
        assert_eq!(model.physical_response(60.0), 1.0);
        let resp_30 = model.physical_response(30.0);
        assert!(resp_30 < 0.2 && resp_30 > 0.0);
    }

    #[test]
    fn psychoacoustic_bass_processor_zero_amount_is_passthrough() {
        let cfg = PsychoacousticBassConfig::default();
        let mut proc = PsychoacousticBassProcessor::new(&cfg, 0.0, 48_000.0);
        let orig = vec![0.5f32; 64];
        let mut buf = orig.clone();
        proc.process_block(&mut buf);
        assert_eq!(buf, orig);
    }

    #[test]
    fn psychoacoustic_bass_processor_generates_bounded_output() {
        let cfg = PsychoacousticBassConfig::default();
        let mut proc = PsychoacousticBassProcessor::new(&cfg, 1.0, 48_000.0);
        let mut buf = vec![0.2f32; 128];
        proc.process_block(&mut buf);
        for &s in &buf {
            assert!(s.is_finite());
            assert!(s.abs() <= 2.0);
        }
    }
}
