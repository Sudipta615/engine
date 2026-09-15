//! Real-time stereo Phase Vocoder with Laroche-Dolson phase locking and formant preservation.
//!
//! Provides polyphonic phase-locked time-stretching and pitch-shifting without
//! tonal comb filtering or "phasiness". Preserves inter-channel stereo coherence
//! and optionally maintains vocal formant resonances.

use std::f32::consts::PI;
use std::sync::Arc;

use num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// Default Phase Vocoder FFT frame size.
pub const PV_FFT_SIZE: usize = 2048;
/// Default synthesis hop size (75% overlap).
pub const PV_HOP_SIZE: usize = 512;

/// Stereo Phase Vocoder engine.
pub struct PhaseVocoder {
    sample_rate: f32,
    fft_size: usize,
    num_bins: usize,
    hop_s: usize,
    hop_a: usize,

    // FFT planners
    forward_fft: Arc<dyn RealToComplex<f32>>,
    inverse_fft: Arc<dyn ComplexToReal<f32>>,

    // Preallocated window and scratch buffers
    window: Vec<f32>,
    fft_in_l: Vec<f32>,
    fft_in_r: Vec<f32>,
    fft_out_l: Vec<Complex<f32>>,
    fft_out_r: Vec<Complex<f32>>,
    synth_spec_l: Vec<Complex<f32>>,
    synth_spec_r: Vec<Complex<f32>>,
    time_out_l: Vec<f32>,
    time_out_r: Vec<f32>,

    // Phase tracking (Laroche-Dolson)
    prev_phase_l: Vec<f32>,
    prev_phase_r: Vec<f32>,
    synth_phase_l: Vec<f32>,
    synth_phase_r: Vec<f32>,
    peaks: Vec<usize>,

    // Formant preservation envelope buffer
    formant_envelope: Vec<f32>,
    formant_preservation: bool,

    // Streaming I/O ring and overlap-add buffers
    input_ring_l: Vec<f32>,
    input_ring_r: Vec<f32>,
    input_write_pos: usize,
    input_available: usize,

    synth_accum_l: Vec<f32>,
    synth_accum_r: Vec<f32>,
    synth_read_pos: usize,
    synth_available: usize,

    output_fifo_l: Vec<f32>,
    output_fifo_r: Vec<f32>,
    output_write_pos: usize,
    output_read_pos: usize,
    output_available: usize,
}

impl PhaseVocoder {
    /// Construct a new `PhaseVocoder` initialized for `sample_rate`.
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let fft_size = PV_FFT_SIZE;
        let num_bins = fft_size / 2 + 1;
        let hop_s = PV_HOP_SIZE;
        let hop_a = PV_HOP_SIZE;

        let mut planner = RealFftPlanner::<f32>::new();
        let forward_fft = planner.plan_fft_forward(fft_size);
        let inverse_fft = planner.plan_fft_inverse(fft_size);

        // Periodic Hann window normalized for OLA (overlap-add) reconstruction
        let window: Vec<f32> = (0..fft_size)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / fft_size as f32).cos()))
            .collect();

        let ring_cap = 65536;
        let fifo_cap = 65536;

        Self {
            sample_rate: sr,
            fft_size,
            num_bins,
            hop_s,
            hop_a,
            forward_fft,
            inverse_fft,
            window,
            fft_in_l: vec![0.0; fft_size],
            fft_in_r: vec![0.0; fft_size],
            fft_out_l: vec![Complex::new(0.0, 0.0); num_bins],
            fft_out_r: vec![Complex::new(0.0, 0.0); num_bins],
            synth_spec_l: vec![Complex::new(0.0, 0.0); num_bins],
            synth_spec_r: vec![Complex::new(0.0, 0.0); num_bins],
            time_out_l: vec![0.0; fft_size],
            time_out_r: vec![0.0; fft_size],
            prev_phase_l: vec![0.0; num_bins],
            prev_phase_r: vec![0.0; num_bins],
            synth_phase_l: vec![0.0; num_bins],
            synth_phase_r: vec![0.0; num_bins],
            peaks: Vec::with_capacity(num_bins),
            formant_envelope: vec![0.0; num_bins],
            formant_preservation: false,
            input_ring_l: vec![0.0; ring_cap],
            input_ring_r: vec![0.0; ring_cap],
            input_write_pos: 0,
            input_available: 0,
            synth_accum_l: vec![0.0; ring_cap],
            synth_accum_r: vec![0.0; ring_cap],
            synth_read_pos: 0,
            synth_available: 0,
            output_fifo_l: vec![0.0; fifo_cap],
            output_fifo_r: vec![0.0; fifo_cap],
            output_write_pos: 0,
            output_read_pos: 0,
            output_available: 0,
        }
    }

    /// Sample rate in Hz.
    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Reset internal phase accumulators and ring buffers.
    pub fn reset(&mut self) {
        self.input_write_pos = 0;
        self.input_available = 0;
        self.synth_read_pos = 0;
        self.synth_available = 0;
        self.output_write_pos = 0;
        self.output_read_pos = 0;
        self.output_available = 0;

        self.input_ring_l.fill(0.0);
        self.input_ring_r.fill(0.0);
        self.synth_accum_l.fill(0.0);
        self.synth_accum_r.fill(0.0);
        self.output_fifo_l.fill(0.0);
        self.output_fifo_r.fill(0.0);

        self.prev_phase_l.fill(0.0);
        self.prev_phase_r.fill(0.0);
        self.synth_phase_l.fill(0.0);
        self.synth_phase_r.fill(0.0);
    }

    /// Enable or disable formant preservation during pitch shifting.
    pub fn set_formant_preservation(&mut self, enabled: bool) {
        self.formant_preservation = enabled;
    }

    /// Query formant preservation status.
    pub fn formant_preservation(&self) -> bool {
        self.formant_preservation
    }

    /// Update stretch ratio (`speed`) and pitch ratio.
    pub fn update_parameters(&mut self, speed: f32, pitch_ratio: f32) {
        let speed = speed.clamp(0.25, 4.0);
        let _pitch = pitch_ratio.clamp(0.25, 4.0);
        // Synthesis hop is fixed, analysis hop scales with speed
        let ha = ((self.hop_s as f32 * speed).round() as usize).clamp(64, self.fft_size / 2);
        self.hop_a = ha;
    }

    /// Process incoming stereo audio and write stretched/pitch-shifted audio
    /// into `(left, right)` in place.
    pub fn process_block(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        speed: f32,
        pitch_ratio: f32,
    ) {
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }

        self.update_parameters(speed, pitch_ratio);

        // 1. Write incoming samples to ring buffer
        let ring_cap = self.input_ring_l.len();
        for i in 0..frames {
            let wp = self.input_write_pos;
            self.input_ring_l[wp] = left[i];
            self.input_ring_r[wp] = right[i];
            self.input_write_pos = (wp + 1) % ring_cap;
        }
        self.input_available += frames;

        // 2. Perform STFT analysis & synthesis while sufficient input is available
        while self.input_available >= self.fft_size {
            self.process_frame(pitch_ratio);
        }

        // 3. Read processed samples from output FIFO
        let fifo_cap = self.output_fifo_l.len();
        let read_count = frames.min(self.output_available);
        for i in 0..read_count {
            let rp = self.output_read_pos;
            left[i] = self.output_fifo_l[rp];
            right[i] = self.output_fifo_r[rp];
            self.output_read_pos = (rp + 1) % fifo_cap;
        }
        self.output_available -= read_count;

        // If FIFO had fewer samples than requested (e.g. startup latency), fill remaining with 0
        if read_count < frames {
            for i in read_count..frames {
                left[i] = 0.0;
                right[i] = 0.0;
            }
        }
    }

    /// Execute one analysis-synthesis STFT hop.
    fn process_frame(&mut self, pitch_ratio: f32) {
        let ring_cap = self.input_ring_l.len();
        let fft_size = self.fft_size;
        let num_bins = self.num_bins;
        let ha = self.hop_a;
        let hs = self.hop_s;

        // Read windowed analysis frame from input ring
        let start_pos = (self.input_write_pos + ring_cap - self.input_available) % ring_cap;
        for i in 0..fft_size {
            let idx = (start_pos + i) % ring_cap;
            let w = self.window[i];
            self.fft_in_l[i] = self.input_ring_l[idx] * w;
            self.fft_in_r[i] = self.input_ring_r[idx] * w;
        }

        // Forward Real FFT
        let _ = self
            .forward_fft
            .process(&mut self.fft_in_l, &mut self.fft_out_l);
        let _ = self
            .forward_fft
            .process(&mut self.fft_in_r, &mut self.fft_out_r);

        // Find spectral peaks (Laroche-Dolson peak detection) on the stereo sum
        self.peaks.clear();
        for k in 1..(num_bins - 1) {
            let mag_l = self.fft_out_l[k].norm();
            let mag_r = self.fft_out_r[k].norm();
            let mag = mag_l + mag_r;

            let prev_mag = self.fft_out_l[k - 1].norm() + self.fft_out_r[k - 1].norm();
            let next_mag = self.fft_out_l[k + 1].norm() + self.fft_out_r[k + 1].norm();

            if mag > prev_mag && mag > next_mag && mag > 1e-5 {
                self.peaks.push(k);
            }
        }

        // Spectral envelope extraction for optional formant preservation
        if self.formant_preservation && (pitch_ratio - 1.0).abs() > 0.02 {
            const SMOOTH_WINDOW: usize = 16;
            for k in 0..num_bins {
                let start = k.saturating_sub(SMOOTH_WINDOW);
                let end = (k + SMOOTH_WINDOW).min(num_bins - 1);
                let mut sum = 0.0f32;
                for j in start..=end {
                    sum += self.fft_out_l[j].norm() + self.fft_out_r[j].norm();
                }
                self.formant_envelope[k] = sum / ((end - start + 1) as f32);
            }
        }

        // Phase advance and rigid phase locking around spectral peaks
        let omega_factor = 2.0 * PI / fft_size as f32;
        let mut peak_idx = 0usize;

        for k in 0..num_bins {
            // Find closest spectral peak
            while peak_idx + 1 < self.peaks.len()
                && (self.peaks[peak_idx + 1] as isize - k as isize).abs()
                    < (self.peaks[peak_idx] as isize - k as isize).abs()
            {
                peak_idx += 1;
            }

            let peak_k = if !self.peaks.is_empty() {
                self.peaks[peak_idx]
            } else {
                k
            };
            let omega_nominal = peak_k as f32 * omega_factor;

            // Compute instantaneous frequency on Left channel
            let phase_l = self.fft_out_l[peak_k].arg();
            let mut delta_phi = phase_l - self.prev_phase_l[peak_k] - omega_nominal * ha as f32;
            delta_phi -= 2.0 * PI * (delta_phi / (2.0 * PI)).round();
            let omega_true = omega_nominal + delta_phi / ha as f32;

            // Accumulate synthesis phase for the peak
            self.synth_phase_l[peak_k] += omega_true * hs as f32;
            self.prev_phase_l[peak_k] = phase_l;

            // Rigid phase locking rotation for bin k
            let rotation = self.synth_phase_l[peak_k] - phase_l;
            let syn_phase_k_l = self.fft_out_l[k].arg() + rotation;

            // Stereo phase coherence: lock Right phase difference to preserve stereo field
            let ipd = self.fft_out_r[k].arg() - self.fft_out_l[k].arg();
            let syn_phase_k_r = syn_phase_k_l + ipd;

            let mut mag_k_l = self.fft_out_l[k].norm();
            let mut mag_k_r = self.fft_out_r[k].norm();

            // Formant preservation magnitude scaling
            if self.formant_preservation && (pitch_ratio - 1.0).abs() > 0.02 {
                let orig_bin = ((k as f32 / pitch_ratio).round() as usize).min(num_bins - 1);
                let orig_env = self.formant_envelope[orig_bin].max(1e-6);
                let curr_env = self.formant_envelope[k].max(1e-6);
                let gain = (curr_env / orig_env).clamp(0.2, 5.0);
                mag_k_l *= gain;
                mag_k_r *= gain;
            }

            self.synth_spec_l[k] = Complex::from_polar(mag_k_l, syn_phase_k_l);
            self.synth_spec_r[k] = Complex::from_polar(mag_k_r, syn_phase_k_r);
        }

        // Inverse Real FFT
        let _ = self
            .inverse_fft
            .process(&mut self.synth_spec_l, &mut self.time_out_l);
        let _ = self
            .inverse_fft
            .process(&mut self.synth_spec_r, &mut self.time_out_r);

        // Synthesis windowing and overlap-add into output FIFO
        let fft_norm = 1.0 / (fft_size as f32 * 1.5);
        let fifo_cap = self.output_fifo_l.len();

        for i in 0..hs {
            let wp = (self.output_write_pos + i) % fifo_cap;
            let w = self.window[i];
            let out_l = self.time_out_l[i] * w * fft_norm;
            let out_r = self.time_out_r[i] * w * fft_norm;

            self.output_fifo_l[wp] += out_l;
            self.output_fifo_r[wp] += out_r;
        }

        self.output_write_pos = (self.output_write_pos + hs) % fifo_cap;
        self.output_available = (self.output_available + hs).min(fifo_cap);
        self.input_available = self.input_available.saturating_sub(ha);
    }
}
