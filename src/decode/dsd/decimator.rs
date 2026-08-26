//! DSD-to-PCM decimation: a five-stage 2:1 FIR cascade.

/// One 2:1 FIR stage in the DSD decimation cascade.
///
/// Keeping the stages explicit makes the passband/stopband contract testable:
/// five 2:1 stages provide the overall 32:1 conversion while each stage only
/// needs to reject the next octave of energy.
pub(crate) struct FirStage {
    pub(crate) coeffs: Vec<f32>,
    history: Vec<f32>,
}

impl FirStage {
    const TAPS: usize = 63;

    pub(crate) fn new() -> Self {
        let mut coeffs = Vec::with_capacity(Self::TAPS);
        let center = (Self::TAPS - 1) as f32 * 0.5;
        for i in 0..Self::TAPS {
            let n = i as f32 - center;
            // Half-band low-pass: cutoff at one quarter of the input sampling
            // rate. A Blackman window gives materially better stopband rejection
            // than the former 64-tap single-stage Hann filter.
            let sinc = if n.abs() < 1e-6 {
                0.5
            } else {
                (std::f32::consts::FRAC_PI_2 * n).sin() / (std::f32::consts::PI * n)
            };
            let phase = i as f32 / (Self::TAPS - 1) as f32;
            let window = 0.42 - 0.5 * (2.0 * std::f32::consts::PI * phase).cos()
                + 0.08 * (4.0 * std::f32::consts::PI * phase).cos();
            coeffs.push(sinc * window);
        }
        let sum: f32 = coeffs.iter().sum();
        for coefficient in &mut coeffs {
            *coefficient /= sum;
        }
        Self {
            coeffs,
            history: vec![0.0; Self::TAPS - 1],
        }
    }

    pub(crate) fn reset(&mut self) {
        self.history.fill(0.0);
    }

    pub(crate) fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        output.clear();
        let taps = self.coeffs.len();
        let history_len = self.history.len();
        let total = history_len + input.len();
        let output_count = if total < taps {
            0
        } else {
            (total - taps) / 2 + 1
        };
        // Decoder-owned stage buffers are preallocated at construction. The
        // fallback below only preserves this private helper's historical API
        // for diagnostic vectors; the realtime decimator path never takes it.
        if output_count > output.capacity() {
            output.reserve(output_count - output.capacity());
        }

        for output_index in 0..output_count {
            let start = output_index * 2;
            let mut sum = 0.0f32;
            for tap in 0..taps {
                let position = start + tap;
                let sample = if position < history_len {
                    self.history[position]
                } else {
                    input[position - history_len]
                };
                sum += sample * self.coeffs[tap];
            }
            output.push(sum);
        }

        // Retain exactly the tail needed to make the next call mathematically
        // equivalent to one-shot processing.
        if input.len() >= history_len {
            self.history
                .copy_from_slice(&input[input.len() - history_len..]);
        } else if !input.is_empty() {
            let keep = history_len - input.len();
            self.history.copy_within(history_len - keep.., 0);
            self.history[keep..].copy_from_slice(input);
        }
    }
}

/// DSD-to-PCM decimation engine with a five-stage 2:1 FIR cascade.
///
/// Converts a 1-bit DSD bitstream down to 32-bit float PCM at 88.2 kHz
/// (DSD64) or 176.4 kHz (DSD128): a 32:1 overall decimation.
///
/// # Quantitative response contract
///
/// Each stage is a 63-tap Blackman-windowed half-band low-pass with:
/// - cutoff at 0.25× the stage input rate (half-band),
/// - DC gain 1.0 (0 dB) — the cascade is DC-transparent,
/// - passband ripple < 0.1 dB out to 20 kHz (DSD64),
/// - stopband rejection > 40 dB at 100 kHz (DSD64),
/// - linear phase (symmetric coefficients), giving a constant group delay of
///   961 input samples = 30.03 samples at the 32× output rate.
///
/// The 1-bit stream is mapped to ±2.0 (the standard +6 dB DSD reference
/// scale) before filtering; only the final output stage is clamped to
/// [-1, 1], while the FIR cascade itself runs unclipped so response
/// measurements are not distorted by saturation.
pub struct DsdToPcmDecimator {
    /// Per-channel FIR cascade: `stages[channel][stage]`.
    pub(crate) stages: Vec<Vec<FirStage>>,
    /// Per-channel, per-stage scratch buffers.
    buffers: Vec<Vec<Vec<f32>>>,
    /// Per-channel scratch for unpacked 1-bit DSD samples.
    scratch: Vec<Vec<f32>>,
}

/// Run one channel's five-stage 2:1 FIR cascade.
fn process_channel(
    stages: &mut [FirStage],
    buffers: &mut [Vec<f32>],
    input: &[f32],
    output: &mut Vec<f32>,
) {
    for index in 0..stages.len() {
        if index == 0 {
            stages[index].process(input, &mut buffers[index]);
        } else {
            let (previous, next) = buffers.split_at_mut(index);
            stages[index].process(&previous[index - 1], &mut next[0]);
        }
    }
    output.extend_from_slice(
        buffers
            .last()
            .expect("DSD decimator must have at least one stage"),
    );
}

impl DsdToPcmDecimator {
    const STAGES: usize = 5;

    /// Create a decimator for `channels` channels (32× downsampling).
    pub fn new(channels: usize) -> Self {
        let channels = channels.max(1);
        let stages = (0..channels)
            .map(|_| (0..Self::STAGES).map(|_| FirStage::new()).collect())
            .collect();
        let buffers = (0..channels)
            .map(|_| {
                (0..Self::STAGES)
                    .map(|i| Vec::with_capacity((65536usize >> (i + 1)).max(64)))
                    .collect()
            })
            .collect();
        let scratch = (0..channels).map(|_| Vec::with_capacity(65536)).collect();
        Self {
            stages,
            buffers,
            scratch,
        }
    }

    /// Backward-compatible constructor for the stereo 32× decimator.
    pub fn new_32x() -> Self {
        Self::new(2)
    }

    /// Reset the filter delay lines (used when seeking: pre-seek history
    /// must not bleed into the new position).
    pub fn reset(&mut self) {
        for stage_chain in &mut self.stages {
            for stage in stage_chain {
                stage.reset();
            }
        }
        for channel_buffers in &mut self.buffers {
            for buffer in channel_buffers {
                buffer.clear();
            }
        }
        for scratch in &mut self.scratch {
            scratch.clear();
        }
    }

    /// Decimate one block of raw 1-bit DSD payload (one byte-slice per
    /// channel) to f32 PCM.
    ///
    /// `lsbf` selects the bit order inside each payload byte: `true` for
    /// LSB-first (bit 0 is the earliest DSD sample — the DSF/DFF default),
    /// `false` for MSB-first (DSF `bits_per_sample == 8`). `out` receives one
    /// `Vec<f32>` per channel; each is extended with the freshly decimated
    /// PCM tail. Fewer than 8 bytes per channel produce no output.
    pub fn decimate_channels(
        &mut self,
        channel_bytes: &[&[u8]],
        lsbf: bool,
        out: &mut [&mut Vec<f32>],
    ) {
        let nch = channel_bytes.len().min(out.len()).min(self.stages.len());
        if nch == 0 {
            return;
        }
        let num_bytes = channel_bytes.iter().map(|c| c.len()).min().unwrap_or(0);
        if num_bytes == 0 {
            return;
        }

        // Convert each byte into 8 float samples (+2.0 / -2.0): the standard
        // +6 dB DSD reference mapping (a 1-bit ±1 stream carries half the
        // energy of a full-scale PCM signal, so ±2.0 normalizes it to unity
        // amplitude after the DC-transparent FIR cascade). The final output
        // stage is clamped to [-1, 1] so PCM-range overflow is prevented while
        // the FIR cascade supplies the ultrasonic rejection.
        const DSD_SCALE: f32 = 2.0;
        let required = num_bytes.saturating_mul(8);
        for (ch, scratch) in self.scratch.iter_mut().enumerate().take(nch) {
            if required > scratch.capacity() {
                log::warn!(
                    "DSD block exceeds preallocated decimator capacity ({} samples); dropping block",
                    required
                );
                return;
            }
            scratch.clear();
            for &b in channel_bytes[ch].iter().take(num_bytes) {
                for bit_idx in 0..8u32 {
                    let bit = if lsbf { bit_idx } else { 7 - bit_idx };
                    scratch.push(if b & (1 << bit) != 0 {
                        DSD_SCALE
                    } else {
                        -DSD_SCALE
                    });
                }
            }
        }

        for (ch, scratch) in self.scratch.iter().enumerate().take(nch) {
            process_channel(
                &mut self.stages[ch],
                &mut self.buffers[ch],
                scratch,
                out[ch],
            );
            // The output stage is the only place where the DSD +6 dB
            // intermediate scale is constrained to PCM range. The internal FIR
            // values remain unclipped so response measurements are not
            // distorted by saturation.
            let tail = self.buffers[ch].last().map_or(0, Vec::len);
            let start = out[ch].len().saturating_sub(tail);
            for sample in &mut out[ch][start..] {
                *sample = sample.clamp(-1.0, 1.0);
            }
        }
    }
}
