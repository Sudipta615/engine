//! FFT-based Partitioned Convolution Engine for room correction / impulse response processing.
//!
//! Uses Uniform Partitioned Overlap-Add (UP-OLA) for real-time convolution.
//! Long impulse responses are segmented into small uniform partitions (e.g. 512 samples).
//! This bounds latency to a single partition block (e.g. ~11.6 ms at 44.1 kHz) rather
//! than the full IR length, and distributes FFT computation evenly across audio frames.
//!
//! Supports stereo IR files (independent convolution per channel) and mono IRs.
//!
//! The engine is real-time safe: all FFT buffers, partition vectors, and workspaces
//! are pre-allocated, ensuring ZERO heap allocation on the audio thread.

use std::path::Path;
use std::sync::Arc;

use num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConvolutionError {
    #[error("Convolution engine not enabled")]
    NotEnabled,
    #[error("No impulse response loaded")]
    NoIrLoaded,
    #[error("IR too long: {0} samples (max {1})")]
    IrTooLong(usize, usize),
    #[error("Failed to load IR file: {0}")]
    FileLoad(String),
    #[error("FFT error: {0}")]
    Fft(String),
}

/// Default partition block size (B).
/// FFT size is 2 * PARTITION_SIZE (1024).
pub const DEFAULT_PARTITION_SIZE: usize = 512;

/// Uniform Partitioned Overlap-Add Convolution Engine.
pub struct ConvolutionEngine {
    /// Whether convolution is enabled
    enabled: bool,
    /// Wet/dry mix (0.0 = fully dry, 1.0 = fully wet)
    wet_mix: f32,
    /// Smoothed wet/dry mix used during live processing
    current_wet_mix: f32,
    /// Sample rate
    sample_rate: f32,
    /// Maximum IR length allowed
    max_ir_length: usize,

    /// Partition block size (B)
    block_size: usize,
    /// FFT size (2 * B)
    fft_size: usize,
    /// Number of partitions P = ceil(ir_length / B)
    num_partitions: usize,

    /// Forward FFT planner (size 2B, f32)
    fft_forward: Arc<dyn RealToComplex<f32>>,
    /// Inverse FFT planner (size 2B, f32)
    fft_inverse: Arc<dyn ComplexToReal<f32>>,
    /// Forward FFT planner (size 2B, f64)
    fft_forward_f64: Arc<dyn RealToComplex<f64>>,
    /// Inverse FFT planner (size 2B, f64)
    fft_inverse_f64: Arc<dyn ComplexToReal<f64>>,

    /// Pre-computed frequency spectra of IR partitions (Left / Mono, f32)
    ir_partitions_left: Vec<Vec<Complex<f32>>>,
    /// Pre-computed frequency spectra of IR partitions (Right, f32)
    ir_partitions_right: Option<Vec<Vec<Complex<f32>>>>,
    /// Pre-computed frequency spectra of IR partitions (Left / Mono, f64)
    ir_partitions_left_f64: Vec<Vec<Complex<f64>>>,
    /// Pre-computed frequency spectra of IR partitions (Right, f64)
    ir_partitions_right_f64: Option<Vec<Vec<Complex<f64>>>>,

    /// Circular buffer of recent input spectra: [0..P)
    input_spectra_left: Vec<Vec<Complex<f32>>>,
    input_spectra_right: Vec<Vec<Complex<f32>>>,
    input_spectra_left_f64: Vec<Vec<Complex<f64>>>,
    input_spectra_right_f64: Vec<Vec<Complex<f64>>>,
    input_spectrum_idx: usize,
    input_spectrum_idx_f64: usize,

    /// Input accumulation buffer (time domain, per channel, size B)
    input_buffer_left: Vec<f32>,
    input_buffer_right: Vec<f32>,
    input_count: usize,
    input_buffer_left_f64: Vec<f64>,
    input_buffer_right_f64: Vec<f64>,
    input_count_f64: usize,

    /// Overlap tail from previous IFFT (size B)
    tail_left: Vec<f32>,
    tail_right: Vec<f32>,
    tail_left_f64: Vec<f64>,
    tail_right_f64: Vec<f64>,

    /// Output FIFO buffer for streaming samples (size 2B)
    output_fifo_left: Vec<f32>,
    output_fifo_right: Vec<f32>,
    output_read_pos: usize,
    output_available: usize,
    output_fifo_left_f64: Vec<f64>,
    output_fifo_right_f64: Vec<f64>,
    output_read_pos_f64: usize,
    output_available_f64: usize,

    // ── Pre-allocated workspaces (Zero-allocation on hot path) ────────────
    fft_workspace_input_left: Vec<f32>,
    fft_workspace_output_left: Vec<Complex<f32>>,
    fft_workspace_input_right: Vec<f32>,
    fft_workspace_output_right: Vec<Complex<f32>>,
    acc_spectrum_left: Vec<Complex<f32>>,
    acc_spectrum_right: Vec<Complex<f32>>,
    ifft_workspace_spectrum_left: Vec<Complex<f32>>,
    ifft_workspace_output_left: Vec<f32>,
    ifft_workspace_spectrum_right: Vec<Complex<f32>>,
    ifft_workspace_output_right: Vec<f32>,

    fft_workspace_input_left_f64: Vec<f64>,
    fft_workspace_output_left_f64: Vec<Complex<f64>>,
    fft_workspace_input_right_f64: Vec<f64>,
    fft_workspace_output_right_f64: Vec<Complex<f64>>,
    acc_spectrum_left_f64: Vec<Complex<f64>>,
    acc_spectrum_right_f64: Vec<Complex<f64>>,
    ifft_workspace_spectrum_left_f64: Vec<Complex<f64>>,
    ifft_workspace_output_left_f64: Vec<f64>,
    ifft_workspace_spectrum_right_f64: Vec<Complex<f64>>,
    ifft_workspace_output_right_f64: Vec<f64>,

    ir_loaded: bool,
    ir_loaded_sample_rate: Option<f32>,
    ir_needs_reload: bool,
    dropped_frames: u64,

    // Legacy fields maintained for test inspection / API compatibility
    pub overlap_left: Vec<f32>,
    pub overlap_right: Vec<f32>,
}

impl ConvolutionEngine {
    /// Create a new partitioned convolution engine with pre-allocated buffers.
    pub fn new(sample_rate: f32, max_ir_length: usize) -> Self {
        let block_size = DEFAULT_PARTITION_SIZE
            .min(max_ir_length.next_power_of_two())
            .max(64);
        let fft_size = block_size * 2;
        let spectrum_len = fft_size / 2 + 1;
        let max_partitions = max_ir_length.div_ceil(block_size);
        let max_partitions = max_partitions.max(1);

        let mut planner_f32 = RealFftPlanner::<f32>::new();
        let fft_forward = planner_f32.plan_fft_forward(fft_size);
        let fft_inverse = planner_f32.plan_fft_inverse(fft_size);

        let mut planner_f64 = RealFftPlanner::<f64>::new();
        let fft_forward_f64 = planner_f64.plan_fft_forward(fft_size);
        let fft_inverse_f64 = planner_f64.plan_fft_inverse(fft_size);

        let fft_out_len = fft_forward.make_output_vec().len();
        let ifft_out_len = fft_inverse.make_output_vec().len();

        let mut input_spectra_left = Vec::with_capacity(max_partitions);
        let mut input_spectra_right = Vec::with_capacity(max_partitions);
        let mut input_spectra_left_f64 = Vec::with_capacity(max_partitions);
        let mut input_spectra_right_f64 = Vec::with_capacity(max_partitions);
        for _ in 0..max_partitions {
            input_spectra_left.push(vec![Complex::new(0.0f32, 0.0f32); spectrum_len]);
            input_spectra_right.push(vec![Complex::new(0.0f32, 0.0f32); spectrum_len]);
            input_spectra_left_f64.push(vec![Complex::new(0.0f64, 0.0f64); spectrum_len]);
            input_spectra_right_f64.push(vec![Complex::new(0.0f64, 0.0f64); spectrum_len]);
        }

        Self {
            enabled: false,
            wet_mix: 1.0,
            current_wet_mix: 1.0,
            sample_rate,
            max_ir_length,
            block_size,
            fft_size,
            num_partitions: 1,
            fft_forward,
            fft_inverse,
            fft_forward_f64,
            fft_inverse_f64,
            ir_partitions_left: Vec::new(),
            ir_partitions_right: None,
            ir_partitions_left_f64: Vec::new(),
            ir_partitions_right_f64: None,
            input_spectra_left,
            input_spectra_right,
            input_spectra_left_f64,
            input_spectra_right_f64,
            input_spectrum_idx: 0,
            input_spectrum_idx_f64: 0,
            input_buffer_left: vec![0.0; block_size],
            input_buffer_right: vec![0.0; block_size],
            input_count: 0,
            input_buffer_left_f64: vec![0.0; block_size],
            input_buffer_right_f64: vec![0.0; block_size],
            input_count_f64: 0,
            tail_left: vec![0.0; block_size],
            tail_right: vec![0.0; block_size],
            tail_left_f64: vec![0.0; block_size],
            tail_right_f64: vec![0.0; block_size],
            output_fifo_left: vec![0.0; fft_size],
            output_fifo_right: vec![0.0; fft_size],
            output_read_pos: 0,
            output_available: 0,
            output_fifo_left_f64: vec![0.0; fft_size],
            output_fifo_right_f64: vec![0.0; fft_size],
            output_read_pos_f64: 0,
            output_available_f64: 0,

            fft_workspace_input_left: vec![0.0; fft_size],
            fft_workspace_output_left: vec![Complex::new(0.0, 0.0); fft_out_len],
            fft_workspace_input_right: vec![0.0; fft_size],
            fft_workspace_output_right: vec![Complex::new(0.0, 0.0); fft_out_len],
            acc_spectrum_left: vec![Complex::new(0.0, 0.0); spectrum_len],
            acc_spectrum_right: vec![Complex::new(0.0, 0.0); spectrum_len],
            ifft_workspace_spectrum_left: vec![Complex::new(0.0, 0.0); spectrum_len],
            ifft_workspace_output_left: vec![0.0; ifft_out_len],
            ifft_workspace_spectrum_right: vec![Complex::new(0.0, 0.0); spectrum_len],
            ifft_workspace_output_right: vec![0.0; ifft_out_len],

            fft_workspace_input_left_f64: vec![0.0; fft_size],
            fft_workspace_output_left_f64: vec![Complex::new(0.0, 0.0); fft_out_len],
            fft_workspace_input_right_f64: vec![0.0; fft_size],
            fft_workspace_output_right_f64: vec![Complex::new(0.0, 0.0); fft_out_len],
            acc_spectrum_left_f64: vec![Complex::new(0.0, 0.0); spectrum_len],
            acc_spectrum_right_f64: vec![Complex::new(0.0, 0.0); spectrum_len],
            ifft_workspace_spectrum_left_f64: vec![Complex::new(0.0, 0.0); spectrum_len],
            ifft_workspace_output_left_f64: vec![0.0; ifft_out_len],
            ifft_workspace_spectrum_right_f64: vec![Complex::new(0.0, 0.0); spectrum_len],
            ifft_workspace_output_right_f64: vec![0.0; ifft_out_len],

            ir_loaded: false,
            ir_loaded_sample_rate: None,
            ir_needs_reload: false,
            dropped_frames: 0,

            overlap_left: vec![0.0; fft_size * 2],
            overlap_right: vec![0.0; fft_size * 2],
        }
    }

    /// Load an impulse response from stereo or mono f32 samples.
    pub fn load_ir_from_samples(
        &mut self,
        ir_samples: &[(f32, f32)],
    ) -> Result<(), ConvolutionError> {
        if ir_samples.is_empty() {
            return Err(ConvolutionError::FileLoad(
                "IR contains no samples".to_string(),
            ));
        }
        let len = ir_samples.len().min(self.max_ir_length);
        let bs = self.block_size;
        let num_parts = len.div_ceil(bs);
        self.num_partitions = num_parts.max(1);

        // Check if IR is stereo
        let stride = (len / 512).max(1);
        let is_stereo = ir_samples[..len]
            .iter()
            .step_by(stride)
            .any(|(l, r)| (l - r).abs() > 1e-5);

        let mut partitions_l = Vec::with_capacity(self.num_partitions);
        let mut partitions_r = if is_stereo {
            Some(Vec::with_capacity(self.num_partitions))
        } else {
            None
        };
        let mut partitions_l_f64 = Vec::with_capacity(self.num_partitions);
        let mut partitions_r_f64 = if is_stereo {
            Some(Vec::with_capacity(self.num_partitions))
        } else {
            None
        };

        // Prepare each partition: take B samples, zero-pad to 2B, compute FFT
        let mut part_time_l = vec![0.0f32; self.fft_size];
        let mut part_time_r = vec![0.0f32; self.fft_size];
        let mut part_time_l_f64 = vec![0.0f64; self.fft_size];
        let mut part_time_r_f64 = vec![0.0f64; self.fft_size];

        for p in 0..self.num_partitions {
            part_time_l.fill(0.0);
            part_time_r.fill(0.0);
            part_time_l_f64.fill(0.0);
            part_time_r_f64.fill(0.0);
            let start = p * bs;
            let end = (start + bs).min(len);
            for (idx, i) in (start..end).enumerate() {
                part_time_l[idx] = ir_samples[i].0;
                part_time_l_f64[idx] = ir_samples[i].0 as f64;
                if is_stereo {
                    part_time_r[idx] = ir_samples[i].1;
                    part_time_r_f64[idx] = ir_samples[i].1 as f64;
                }
            }

            let spec_l = self.forward_fft(&part_time_l)?;
            partitions_l.push(spec_l);
            let spec_l_f64 = self.forward_fft_f64(&part_time_l_f64)?;
            partitions_l_f64.push(spec_l_f64);

            if let Some(ref mut pr) = partitions_r {
                let spec_r = self.forward_fft(&part_time_r)?;
                pr.push(spec_r);
            }
            if let Some(ref mut pr_f64) = partitions_r_f64 {
                let spec_r_f64 = self.forward_fft_f64(&part_time_r_f64)?;
                pr_f64.push(spec_r_f64);
            }
        }

        self.ir_partitions_left = partitions_l;
        self.ir_partitions_right = partitions_r;
        self.ir_partitions_left_f64 = partitions_l_f64;
        self.ir_partitions_right_f64 = partitions_r_f64;

        // Ensure input spectra history buffer has enough slots
        let spectrum_len = self.fft_size / 2 + 1;
        while self.input_spectra_left.len() < self.num_partitions {
            self.input_spectra_left
                .push(vec![Complex::new(0.0, 0.0); spectrum_len]);
            self.input_spectra_right
                .push(vec![Complex::new(0.0, 0.0); spectrum_len]);
            self.input_spectra_left_f64
                .push(vec![Complex::new(0.0, 0.0); spectrum_len]);
            self.input_spectra_right_f64
                .push(vec![Complex::new(0.0, 0.0); spectrum_len]);
        }

        self.ir_loaded = true;
        self.ir_loaded_sample_rate = Some(self.sample_rate);
        self.ir_needs_reload = false;
        self.reset();
        Ok(())
    }

    /// Load an impulse response from stereo or mono f64 samples with native 64-bit precision.
    pub fn load_ir_from_samples_f64(
        &mut self,
        ir_samples: &[(f64, f64)],
    ) -> Result<(), ConvolutionError> {
        if ir_samples.is_empty() {
            return Err(ConvolutionError::FileLoad(
                "IR contains no samples".to_string(),
            ));
        }
        let len = ir_samples.len().min(self.max_ir_length);
        let bs = self.block_size;
        let num_parts = len.div_ceil(bs);
        self.num_partitions = num_parts.max(1);

        let stride = (len / 512).max(1);
        let is_stereo = ir_samples[..len]
            .iter()
            .step_by(stride)
            .any(|(l, r)| (l - r).abs() > 1e-9);

        let mut partitions_l = Vec::with_capacity(self.num_partitions);
        let mut partitions_r = if is_stereo {
            Some(Vec::with_capacity(self.num_partitions))
        } else {
            None
        };
        let mut partitions_l_f64 = Vec::with_capacity(self.num_partitions);
        let mut partitions_r_f64 = if is_stereo {
            Some(Vec::with_capacity(self.num_partitions))
        } else {
            None
        };

        let mut part_time_l = vec![0.0f32; self.fft_size];
        let mut part_time_r = vec![0.0f32; self.fft_size];
        let mut part_time_l_f64 = vec![0.0f64; self.fft_size];
        let mut part_time_r_f64 = vec![0.0f64; self.fft_size];

        for p in 0..self.num_partitions {
            part_time_l.fill(0.0);
            part_time_r.fill(0.0);
            part_time_l_f64.fill(0.0);
            part_time_r_f64.fill(0.0);
            let start = p * bs;
            let end = (start + bs).min(len);
            for (idx, i) in (start..end).enumerate() {
                part_time_l[idx] = ir_samples[i].0 as f32;
                part_time_l_f64[idx] = ir_samples[i].0;
                if is_stereo {
                    part_time_r[idx] = ir_samples[i].1 as f32;
                    part_time_r_f64[idx] = ir_samples[i].1;
                }
            }

            let spec_l = self.forward_fft(&part_time_l)?;
            partitions_l.push(spec_l);
            let spec_l_f64 = self.forward_fft_f64(&part_time_l_f64)?;
            partitions_l_f64.push(spec_l_f64);

            if let Some(ref mut pr) = partitions_r {
                let spec_r = self.forward_fft(&part_time_r)?;
                pr.push(spec_r);
            }
            if let Some(ref mut pr_f64) = partitions_r_f64 {
                let spec_r_f64 = self.forward_fft_f64(&part_time_r_f64)?;
                pr_f64.push(spec_r_f64);
            }
        }

        self.ir_partitions_left = partitions_l;
        self.ir_partitions_right = partitions_r;
        self.ir_partitions_left_f64 = partitions_l_f64;
        self.ir_partitions_right_f64 = partitions_r_f64;

        let spectrum_len = self.fft_size / 2 + 1;
        while self.input_spectra_left.len() < self.num_partitions {
            self.input_spectra_left
                .push(vec![Complex::new(0.0, 0.0); spectrum_len]);
            self.input_spectra_right
                .push(vec![Complex::new(0.0, 0.0); spectrum_len]);
            self.input_spectra_left_f64
                .push(vec![Complex::new(0.0, 0.0); spectrum_len]);
            self.input_spectra_right_f64
                .push(vec![Complex::new(0.0, 0.0); spectrum_len]);
        }

        self.ir_loaded = true;
        self.ir_loaded_sample_rate = Some(self.sample_rate);
        self.ir_needs_reload = false;
        self.reset();
        Ok(())
    }

    /// Load an impulse response from a WAV or FLAC file using symphonia.
    pub fn load_ir_from_file(&mut self, path: &Path) -> Result<(), ConvolutionError> {
        use symphonia::core::{
            codecs::audio::{AudioDecoderOptions, CODEC_ID_NULL_AUDIO},
            formats::{probe::Hint, FormatOptions},
            io::MediaSourceStream,
            meta::MetadataOptions,
        };

        let file = std::fs::File::open(path).map_err(|e| {
            ConvolutionError::FileLoad(format!("Cannot open {}: {}", path.display(), e))
        })?;

        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let format_opts = FormatOptions::default();
        let metadata_opts = MetadataOptions::default();
        let decoder_opts = AudioDecoderOptions::default();

        let mut format_reader = symphonia::default::get_probe()
            .probe(&hint, mss, format_opts, metadata_opts)
            .map_err(|e| ConvolutionError::FileLoad(format!("Probe failed: {}", e)))?;

        let track = format_reader
            .tracks()
            .iter()
            .find(|t| {
                t.codec_params
                    .as_ref()
                    .and_then(|cp| cp.audio())
                    .is_some_and(|a| a.codec != CODEC_ID_NULL_AUDIO)
            })
            .ok_or_else(|| ConvolutionError::FileLoad("No audio track found".to_string()))?;

        let track_id = track.id;
        let audio_params = track
            .codec_params
            .as_ref()
            .and_then(|cp| cp.audio())
            .ok_or_else(|| {
                ConvolutionError::FileLoad("No audio codec parameters found".to_string())
            })?;

        let mut decoder = symphonia::default::get_codecs()
            .make_audio_decoder(audio_params, &decoder_opts)
            .map_err(|e| ConvolutionError::FileLoad(format!("Cannot create decoder: {}", e)))?;

        let mut ir_samples: Vec<(f32, f32)> = Vec::new();
        let mut temp_interleaved: Vec<f32> = Vec::new();

        while let Ok(Some(packet)) = format_reader.next_packet() {
            if packet.track_id != track_id {
                continue;
            }

            if let Ok(decoded) = decoder.decode(&packet) {
                let planes = decoded.num_planes();
                temp_interleaved.clear();
                decoded.copy_to_vec_interleaved(&mut temp_interleaved);
                if planes == 1 {
                    for &s in &temp_interleaved {
                        ir_samples.push((s, s));
                    }
                } else if planes >= 2 {
                    for chunk in temp_interleaved.chunks_exact(planes) {
                        ir_samples.push((chunk[0], chunk[1]));
                    }
                }
            }

            if ir_samples.len() > self.max_ir_length {
                ir_samples.truncate(self.max_ir_length);
                break;
            }
        }

        if ir_samples.is_empty() {
            return Err(ConvolutionError::FileLoad(
                "IR file contains no samples".to_string(),
            ));
        }

        self.load_ir_from_samples(&ir_samples)
    }

    fn forward_fft(&self, input: &[f32]) -> Result<Vec<Complex<f32>>, ConvolutionError> {
        let mut input_vec = input.to_vec();
        let mut output = self.fft_forward.make_output_vec();
        self.fft_forward
            .process(&mut input_vec, &mut output)
            .map_err(|e| ConvolutionError::Fft(e.to_string()))?;
        Ok(output)
    }

    fn forward_fft_f64(&self, input: &[f64]) -> Result<Vec<Complex<f64>>, ConvolutionError> {
        let mut input_vec = input.to_vec();
        let mut output = self.fft_forward_f64.make_output_vec();
        self.fft_forward_f64
            .process(&mut input_vec, &mut output)
            .map_err(|e| ConvolutionError::Fft(e.to_string()))?;
        Ok(output)
    }

    /// Process a single stereo sample through the partitioned convolution engine.
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.enabled || !self.ir_loaded {
            return (left, right);
        }

        self.input_buffer_left[self.input_count] = left;
        self.input_buffer_right[self.input_count] = right;
        self.input_count += 1;

        if self.input_count >= self.block_size {
            self.process_partition_block();
        }

        if self.output_available > 0 {
            let out_l = self.output_fifo_left[self.output_read_pos];
            let out_r = self.output_fifo_right[self.output_read_pos];
            self.output_read_pos += 1;
            self.output_available -= 1;
            if self.output_available == 0 {
                self.output_read_pos = 0;
            }

            if (self.current_wet_mix - self.wet_mix).abs() > 1e-5 {
                self.current_wet_mix += 0.001 * (self.wet_mix - self.current_wet_mix);
            } else {
                self.current_wet_mix = self.wet_mix;
            }
            let mix = self.current_wet_mix;
            let mixed_l = left * (1.0 - mix) + out_l * mix;
            let mixed_r = right * (1.0 - mix) + out_r * mix;
            (mixed_l, mixed_r)
        } else {
            let mix = self.current_wet_mix;
            (left * (1.0 - mix), right * (1.0 - mix))
        }
    }

    /// Process a stereo sample pair via native f64 uniform partitioned convolution.
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if !self.enabled || !self.ir_loaded {
            return (left, right);
        }

        self.input_buffer_left_f64[self.input_count_f64] = left;
        self.input_buffer_right_f64[self.input_count_f64] = right;
        self.input_count_f64 += 1;

        if self.input_count_f64 >= self.block_size {
            self.process_partition_block_f64();
        }

        if self.output_available_f64 > 0 {
            let out_l = self.output_fifo_left_f64[self.output_read_pos_f64];
            let out_r = self.output_fifo_right_f64[self.output_read_pos_f64];
            self.output_read_pos_f64 += 1;
            self.output_available_f64 -= 1;
            if self.output_available_f64 == 0 {
                self.output_read_pos_f64 = 0;
            }

            if (self.current_wet_mix - self.wet_mix).abs() > 1e-5 {
                self.current_wet_mix += 0.001 * (self.wet_mix - self.current_wet_mix);
            } else {
                self.current_wet_mix = self.wet_mix;
            }
            let mix = self.current_wet_mix as f64;
            let mixed_l = left * (1.0 - mix) + out_l * mix;
            let mixed_r = right * (1.0 - mix) + out_r * mix;
            (mixed_l, mixed_r)
        } else {
            let mix = self.current_wet_mix as f64;
            (left * (1.0 - mix), right * (1.0 - mix))
        }
    }

    /// Uniform Partitioned Overlap-Add Block Processing (f32).
    fn process_partition_block(&mut self) {
        let bs = self.block_size;
        let fft_size = self.fft_size;
        let p_count = self.num_partitions;
        let spec_len = fft_size / 2 + 1;

        self.fft_workspace_input_left[..bs].copy_from_slice(&self.input_buffer_left[..bs]);
        self.fft_workspace_input_left[bs..fft_size].fill(0.0);
        self.fft_workspace_input_right[..bs].copy_from_slice(&self.input_buffer_right[..bs]);
        self.fft_workspace_input_right[bs..fft_size].fill(0.0);

        let fft_forward = Arc::clone(&self.fft_forward);
        let _ = fft_forward.process(
            &mut self.fft_workspace_input_left,
            &mut self.fft_workspace_output_left,
        );
        let _ = fft_forward.process(
            &mut self.fft_workspace_input_right,
            &mut self.fft_workspace_output_right,
        );

        let cur_idx = self.input_spectrum_idx;
        self.input_spectra_left[cur_idx][..spec_len]
            .copy_from_slice(&self.fft_workspace_output_left[..spec_len]);
        self.input_spectra_right[cur_idx][..spec_len]
            .copy_from_slice(&self.fft_workspace_output_right[..spec_len]);

        self.acc_spectrum_left.fill(Complex::new(0.0, 0.0));
        self.acc_spectrum_right.fill(Complex::new(0.0, 0.0));

        let is_stereo = self.ir_partitions_right.is_some();

        for p in 0..p_count {
            let hist_idx = (cur_idx + p_count - p) % p_count;
            let x_l = &self.input_spectra_left[hist_idx];
            let x_r = &self.input_spectra_right[hist_idx];
            let h_l = &self.ir_partitions_left[p];

            for k in 0..spec_len {
                self.acc_spectrum_left[k] += x_l[k] * h_l[k];
            }

            if is_stereo {
                if let Some(ref parts_r) = self.ir_partitions_right {
                    let h_r = &parts_r[p];
                    for k in 0..spec_len {
                        self.acc_spectrum_right[k] += x_r[k] * h_r[k];
                    }
                }
            } else {
                for k in 0..spec_len {
                    self.acc_spectrum_right[k] += x_r[k] * h_l[k];
                }
            }
        }

        let fft_inverse = Arc::clone(&self.fft_inverse);
        let copy_len = spec_len.min(self.ifft_workspace_spectrum_left.len());
        self.ifft_workspace_spectrum_left[..copy_len]
            .copy_from_slice(&self.acc_spectrum_left[..copy_len]);
        self.ifft_workspace_spectrum_right[..copy_len]
            .copy_from_slice(&self.acc_spectrum_right[..copy_len]);

        let _ = fft_inverse.process(
            &mut self.ifft_workspace_spectrum_left,
            &mut self.ifft_workspace_output_left,
        );
        let _ = fft_inverse.process(
            &mut self.ifft_workspace_spectrum_right,
            &mut self.ifft_workspace_output_right,
        );

        let scale = 1.0 / fft_size as f32;
        for s in self.ifft_workspace_output_left.iter_mut() {
            *s *= scale;
        }
        for s in self.ifft_workspace_output_right.iter_mut() {
            *s *= scale;
        }

        for i in 0..bs {
            let out_l = self.ifft_workspace_output_left[i] + self.tail_left[i];
            let out_r = self.ifft_workspace_output_right[i] + self.tail_right[i];
            self.output_fifo_left[i] = out_l;
            self.output_fifo_right[i] = out_r;
            self.tail_left[i] = self.ifft_workspace_output_left[bs + i];
            self.tail_right[i] = self.ifft_workspace_output_right[bs + i];
        }

        self.output_read_pos = 0;
        self.output_available = bs;

        self.input_spectrum_idx = (self.input_spectrum_idx + 1) % p_count;
        self.input_count = 0;
    }

    /// Uniform Partitioned Overlap-Add Block Processing (native f64).
    fn process_partition_block_f64(&mut self) {
        let bs = self.block_size;
        let fft_size = self.fft_size;
        let p_count = self.num_partitions;
        let spec_len = fft_size / 2 + 1;

        self.fft_workspace_input_left_f64[..bs].copy_from_slice(&self.input_buffer_left_f64[..bs]);
        self.fft_workspace_input_left_f64[bs..fft_size].fill(0.0);
        self.fft_workspace_input_right_f64[..bs]
            .copy_from_slice(&self.input_buffer_right_f64[..bs]);
        self.fft_workspace_input_right_f64[bs..fft_size].fill(0.0);

        let fft_forward = Arc::clone(&self.fft_forward_f64);
        let _ = fft_forward.process(
            &mut self.fft_workspace_input_left_f64,
            &mut self.fft_workspace_output_left_f64,
        );
        let _ = fft_forward.process(
            &mut self.fft_workspace_input_right_f64,
            &mut self.fft_workspace_output_right_f64,
        );

        let cur_idx = self.input_spectrum_idx_f64;
        self.input_spectra_left_f64[cur_idx][..spec_len]
            .copy_from_slice(&self.fft_workspace_output_left_f64[..spec_len]);
        self.input_spectra_right_f64[cur_idx][..spec_len]
            .copy_from_slice(&self.fft_workspace_output_right_f64[..spec_len]);

        self.acc_spectrum_left_f64.fill(Complex::new(0.0, 0.0));
        self.acc_spectrum_right_f64.fill(Complex::new(0.0, 0.0));

        let is_stereo = self.ir_partitions_right_f64.is_some();

        for p in 0..p_count {
            let hist_idx = (cur_idx + p_count - p) % p_count;
            let x_l = &self.input_spectra_left_f64[hist_idx];
            let x_r = &self.input_spectra_right_f64[hist_idx];
            let h_l = &self.ir_partitions_left_f64[p];

            for k in 0..spec_len {
                self.acc_spectrum_left_f64[k] += x_l[k] * h_l[k];
            }

            if is_stereo {
                if let Some(ref parts_r) = self.ir_partitions_right_f64 {
                    let h_r = &parts_r[p];
                    for k in 0..spec_len {
                        self.acc_spectrum_right_f64[k] += x_r[k] * h_r[k];
                    }
                }
            } else {
                for k in 0..spec_len {
                    self.acc_spectrum_right_f64[k] += x_r[k] * h_l[k];
                }
            }
        }

        let fft_inverse = Arc::clone(&self.fft_inverse_f64);
        let copy_len = spec_len.min(self.ifft_workspace_spectrum_left_f64.len());
        self.ifft_workspace_spectrum_left_f64[..copy_len]
            .copy_from_slice(&self.acc_spectrum_left_f64[..copy_len]);
        self.ifft_workspace_spectrum_right_f64[..copy_len]
            .copy_from_slice(&self.acc_spectrum_right_f64[..copy_len]);

        let _ = fft_inverse.process(
            &mut self.ifft_workspace_spectrum_left_f64,
            &mut self.ifft_workspace_output_left_f64,
        );
        let _ = fft_inverse.process(
            &mut self.ifft_workspace_spectrum_right_f64,
            &mut self.ifft_workspace_output_right_f64,
        );

        let scale = 1.0 / fft_size as f64;
        for s in self.ifft_workspace_output_left_f64.iter_mut() {
            *s *= scale;
        }
        for s in self.ifft_workspace_output_right_f64.iter_mut() {
            *s *= scale;
        }

        for i in 0..bs {
            let out_l = self.ifft_workspace_output_left_f64[i] + self.tail_left_f64[i];
            let out_r = self.ifft_workspace_output_right_f64[i] + self.tail_right_f64[i];
            self.output_fifo_left_f64[i] = out_l;
            self.output_fifo_right_f64[i] = out_r;
            self.tail_left_f64[i] = self.ifft_workspace_output_left_f64[bs + i];
            self.tail_right_f64[i] = self.ifft_workspace_output_right_f64[bs + i];
        }

        self.output_read_pos_f64 = 0;
        self.output_available_f64 = bs;

        self.input_spectrum_idx_f64 = (self.input_spectrum_idx_f64 + 1) % p_count;
        self.input_count_f64 = 0;
    }

    /// Process a batch of stereo frames through the convolution engine
    pub fn process_batch(&mut self, frames: &mut [(f32, f32)]) {
        for frame in frames.iter_mut() {
            *frame = self.process(frame.0, frame.1);
        }
    }

    /// Process a block of stereo frames in place. Hoists the enabled/IR
    /// checks out of the per-frame loop; the internal partitioned-convolution
    /// state is still advanced per sample.
    #[inline]
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if !self.enabled || !self.ir_loaded {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process(left[i], right[i]);
            left[i] = ol;
            right[i] = or_;
        }
    }

    /// Process a block of stereo frames in native f64 precision. Hoists the
    /// enabled/IR checks out of the per-frame loop.
    #[inline]
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if !self.enabled || !self.ir_loaded {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i], right[i]);
            left[i] = ol;
            right[i] = or_;
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_wet_mix(&mut self, mix: f32) {
        self.wet_mix = mix.clamp(0.0, 1.0);
    }

    pub fn wet_mix(&self) -> f32 {
        self.wet_mix
    }

    pub fn is_ir_loaded(&self) -> bool {
        self.ir_loaded
    }

    pub fn ir_needs_reload(&self) -> bool {
        self.ir_needs_reload
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn num_partitions(&self) -> usize {
        self.num_partitions
    }

    /// Latency of the partitioned convolution engine in samples.
    /// In UP-OLA this is exactly one partition block (`block_size`).
    pub fn latency_samples(&self) -> usize {
        self.block_size
    }

    /// Latency of the partitioned convolution engine in milliseconds (0 if disabled/no IR).
    pub fn latency_ms(&self) -> f32 {
        if self.enabled && self.ir_loaded && self.sample_rate > 0.0 {
            self.block_size as f32 / self.sample_rate * 1000.0
        } else {
            0.0
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if (self.sample_rate - sample_rate).abs() > 0.01 {
            log::info!(
                "ConvolutionEngine sample rate changed: {:.0} Hz -> {:.0} Hz.",
                self.sample_rate,
                sample_rate
            );
            self.sample_rate = sample_rate;
            if self.ir_loaded {
                if let Some(loaded_rate) = self.ir_loaded_sample_rate {
                    self.ir_needs_reload = (loaded_rate - sample_rate).abs() > 0.01;
                } else {
                    self.ir_needs_reload = true;
                }
            }
            self.reset();
        }
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// Reset all processing state (keeps loaded IR).
    pub fn reset(&mut self) {
        self.input_count = 0;
        self.input_spectrum_idx = 0;
        self.output_read_pos = 0;
        self.output_available = 0;
        self.input_buffer_left.fill(0.0);
        self.input_buffer_right.fill(0.0);
        self.tail_left.fill(0.0);
        self.tail_right.fill(0.0);
        self.output_fifo_left.fill(0.0);
        self.output_fifo_right.fill(0.0);

        self.input_count_f64 = 0;
        self.input_spectrum_idx_f64 = 0;
        self.output_read_pos_f64 = 0;
        self.output_available_f64 = 0;
        self.input_buffer_left_f64.fill(0.0);
        self.input_buffer_right_f64.fill(0.0);
        self.tail_left_f64.fill(0.0);
        self.tail_right_f64.fill(0.0);
        self.output_fifo_left_f64.fill(0.0);
        self.output_fifo_right_f64.fill(0.0);

        for spec in &mut self.input_spectra_left {
            spec.fill(Complex::new(0.0, 0.0));
        }
        for spec in &mut self.input_spectra_right {
            spec.fill(Complex::new(0.0, 0.0));
        }
        for spec in &mut self.input_spectra_left_f64 {
            spec.fill(Complex::new(0.0, 0.0));
        }
        for spec in &mut self.input_spectra_right_f64 {
            spec.fill(Complex::new(0.0, 0.0));
        }

        self.fft_workspace_input_left.fill(0.0);
        self.fft_workspace_output_left.fill(Complex::new(0.0, 0.0));
        self.fft_workspace_input_right.fill(0.0);
        self.fft_workspace_output_right.fill(Complex::new(0.0, 0.0));
        self.ifft_workspace_spectrum_left
            .fill(Complex::new(0.0, 0.0));
        self.ifft_workspace_output_left.fill(0.0);
        self.ifft_workspace_spectrum_right
            .fill(Complex::new(0.0, 0.0));
        self.ifft_workspace_output_right.fill(0.0);

        self.fft_workspace_input_left_f64.fill(0.0);
        self.fft_workspace_output_left_f64
            .fill(Complex::new(0.0, 0.0));
        self.fft_workspace_input_right_f64.fill(0.0);
        self.fft_workspace_output_right_f64
            .fill(Complex::new(0.0, 0.0));
        self.ifft_workspace_spectrum_left_f64
            .fill(Complex::new(0.0, 0.0));
        self.ifft_workspace_output_left_f64.fill(0.0);
        self.ifft_workspace_spectrum_right_f64
            .fill(Complex::new(0.0, 0.0));
        self.ifft_workspace_output_right_f64.fill(0.0);

        self.dropped_frames = 0;
        self.current_wet_mix = self.wet_mix;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convolution_creation() {
        let engine = ConvolutionEngine::new(44100.0, 1024);
        assert!(!engine.is_enabled());
        assert!(!engine.is_ir_loaded());
    }

    #[test]
    fn test_convolution_passthrough_when_disabled() {
        let mut engine = ConvolutionEngine::new(44100.0, 1024);
        let (l, r) = engine.process(0.5, 0.3);
        assert!((l - 0.5).abs() < 1e-5);
        assert!((r - 0.3).abs() < 1e-5);
    }

    #[test]
    fn test_convolution_load_mono_ir() {
        let mut engine = ConvolutionEngine::new(44100.0, 512);
        let ir: Vec<(f32, f32)> = vec![(1.0, 1.0)];
        engine.load_ir_from_samples(&ir).unwrap();
        assert!(engine.is_ir_loaded());
    }

    #[test]
    fn test_convolution_load_stereo_ir() {
        let mut engine = ConvolutionEngine::new(44100.0, 512);
        let ir: Vec<(f32, f32)> = vec![(1.0, 0.5), (0.3, 0.7), (0.1, 0.2)];
        engine.load_ir_from_samples(&ir).unwrap();
        assert!(engine.is_ir_loaded());
    }

    #[test]
    fn test_convolution_wet_mix() {
        let mut engine = ConvolutionEngine::new(44100.0, 512);
        engine.set_wet_mix(0.5);
        assert!((engine.wet_mix() - 0.5).abs() < 1e-5);
        engine.set_wet_mix(-0.1);
        assert!(engine.wet_mix() >= 0.0);
        engine.set_wet_mix(1.5);
        assert!(engine.wet_mix() <= 1.0);
    }

    #[test]
    fn test_convolution_reset() {
        let mut engine = ConvolutionEngine::new(44100.0, 512);
        let ir: Vec<(f32, f32)> = vec![(1.0, 1.0)];
        engine.load_ir_from_samples(&ir).unwrap();
        engine.set_enabled(true);
        engine.reset();
        assert!(engine.is_ir_loaded());
        assert!(engine.is_enabled());
        assert_eq!(engine.input_count, 0);
    }

    #[test]
    fn test_convolution_partitioned_delta_impulse() {
        // Delta impulse IR: output should match input after partition latency
        let mut engine = ConvolutionEngine::new(44100.0, 512);
        let mut ir = vec![(0.0f32, 0.0f32); 256];
        ir[0] = (1.0, 1.0); // Delta at t=0
        engine.load_ir_from_samples(&ir).unwrap();
        engine.set_enabled(true);
        engine.set_wet_mix(1.0);

        let bs = engine.block_size();
        // Warm up / flush one block
        for _ in 0..bs {
            engine.process(1.0, 1.0);
        }
        // Next block should output 1.0
        for _ in 0..bs {
            let (l, r) = engine.process(1.0, 1.0);
            assert!((l - 1.0).abs() < 1e-4, "Expected ~1.0, got {}", l);
            assert!((r - 1.0).abs() < 1e-4, "Expected ~1.0, got {}", r);
        }
    }

    #[test]
    fn test_convolution_long_ir_partitioning() {
        // 2048 sample IR with block size 512 -> 4 partitions
        let mut engine = ConvolutionEngine::new(44100.0, 2048);
        let ir = vec![(0.01f32, 0.01f32); 2048];
        engine.load_ir_from_samples(&ir).unwrap();
        assert_eq!(engine.num_partitions(), 4);
    }

    #[test]
    fn test_convolution_partitioned_delta_impulse_f64() {
        let mut engine = ConvolutionEngine::new(44100.0, 512);
        let mut ir = vec![(0.0f64, 0.0f64); 256];
        ir[0] = (1.0, 1.0);
        engine.load_ir_from_samples_f64(&ir).unwrap();
        engine.set_enabled(true);
        engine.set_wet_mix(1.0);

        let bs = engine.block_size();
        for _ in 0..bs {
            engine.process_f64(1.0, 1.0);
        }
        for _ in 0..bs {
            let (l, r) = engine.process_f64(1.0, 1.0);
            assert!((l - 1.0).abs() < 1e-10, "Expected ~1.0, got {}", l);
            assert!((r - 1.0).abs() < 1e-10, "Expected ~1.0, got {}", r);
        }
    }
}
