/// Concrete sample container negotiated with the output device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum OutputSampleFormat {
    F32,
    F64,
    I16,
    U16,
    /// Signed 24-bit-in-32: 24 valid bits left-justified in a 32-bit
    /// container (WAVEFORMATEXTENSIBLE with `wValidBitsPerSample = 24`).
    I24Le,
    I32,
    #[default]
    Unknown,
}

impl OutputSampleFormat {
    /// Convert a depth-only description conservatively.
    ///
    /// A 32-bit stream may be f32, f64, or signed integer PCM; without the
    /// container/format bit this API cannot identify it, so it deliberately
    /// returns `Unknown` instead of claiming f32. Callers with negotiated
    /// format information should pass [`OutputSampleFormat`] directly or use
    /// [`Self::from_bit_depth_and_float`].
    pub fn from_bit_depth(bits: u32) -> Self {
        match bits {
            16 => Self::I16,
            24 => Self::I24Le,
            _ => Self::Unknown,
        }
    }

    /// Resolve a depth when the caller also knows whether the container is
    /// floating-point. This is still intentionally narrower than the
    /// format-aware API because it cannot distinguish f32 from f64 or all
    /// integer container variants.
    pub fn from_bit_depth_and_float(bits: u32, is_float: bool) -> Self {
        match (bits, is_float) {
            (32, true) => Self::F32,
            (64, true) => Self::F64,
            (32, false) => Self::I32,
            (16, false) => Self::I16,
            (24, false) => Self::I24Le,
            _ => Self::Unknown,
        }
    }

    pub fn bit_depth(self) -> Option<u32> {
        match self {
            Self::F32 | Self::I32 | Self::F64 => Some(32),
            Self::I24Le => Some(24),
            Self::I16 | Self::U16 => Some(16),
            Self::Unknown => None,
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::I16 => "i16",
            Self::U16 => "u16",
            Self::I24Le => "i24le",
            Self::I32 => "i32",
            Self::Unknown => "unknown",
        }
    }
}

/// Coarse product-facing verdict derived from the samples-vs-transport split.
///
/// - [`BitPerfect`](Self::BitPerfect) — the sample sequence is untouched **and**
///   the transport is verified direct/exclusive (no hidden OS mixer).
/// - [`Dsp`](Self::Dsp) — the sample sequence is provably modified by an active
///   DSP/gain/resampling stage.
/// - [`Unknown`](Self::Unknown) — bit-perfect cannot be proven (unknown source
///   or output precision, or the access mode is not verified). A boolean alone
///   cannot express this third state; the UI/API must not claim bit-perfect
///   from heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitPerfectResult {
    /// Samples untouched AND transport verified direct/exclusive.
    BitPerfect,
    /// The sample sequence is provably modified by the active signal path.
    Dsp,
    /// Bit-perfect cannot be proven (unknown precision or unverified access).
    #[default]
    Unknown,
}

/// Which volume path is currently in use (spec §12 — "Expose actual volume
/// path in diagnostics"). Bit-perfect output requires `None` or a hardware
/// path that leaves the sample sequence untouched; `Software` multiplies
/// samples and breaks bit-exactness even at unity when a fade/ramp is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VolumePath {
    /// No volume applied anywhere (unity; hardware path with no gain stage).
    #[default]
    None,
    /// Endpoint/hardware volume control (samples untouched).
    Hardware,
    /// Software DSP gain (samples multiplied).
    Software,
}

/// Diagnostic breakdown of every condition evaluated for bit-perfect output.
///
/// This is the engine's **single** bit-perfect verdict: `EngineStats.bit_perfect`
/// is always derived from `is_bit_perfect` — there is no second, parallel
/// truth system.
///
/// Known limitations (checked here or enforced upstream):
/// - The source must be ≤ 24 bits to round-trip through the f32 pipeline
///   exactly (a 32-bit integer source is NOT bit-exact after f32 conversion;
///   see `format_conversion_lossless`).
/// - Multichannel sources are downmixed to stereo at the decoder boundary,
///   so bit-perfect is only meaningful for 1–2 channel sources.
/// - Dither is applied only when the output quantizes to fewer bits than the
///   source, which the `bit_depth_not_truncated` condition already forbids.
#[derive(Debug, Clone, Default)]
pub struct BitPerfectReport {
    /// Overall bit-perfect verdict (true only when all required conditions hold).
    pub is_bit_perfect: bool,
    /// Software volume is unity (0 dB) and no seek/transition fade in progress.
    pub volume_unity: bool,
    /// EQ (parametric & graphic) is bypassed or all gains flat (0 dB).
    pub eq_bypassed: bool,
    /// Limiter, compressor, convolution, stereo width, crossfeed, and
    /// loudness are all bypassed. Aggregate of the per-stage flags below.
    pub dynamics_bypassed: bool,
    /// Multiband compressor bypassed.
    pub compressor_bypassed: bool,
    /// Convolution / FIR bypassed (or no IR loaded).
    pub convolution_bypassed: bool,
    /// Crossfeed bypassed.
    pub crossfeed_bypassed: bool,
    /// Stereo width/expansion bypassed (or width is exactly 1.0).
    pub stereo_bypassed: bool,
    /// Safety limiter bypassed.
    pub limiter_bypassed: bool,
    /// Loudness normalization (ReplayGain / R128 / smart) bypassed.
    pub loudness_bypassed: bool,
    /// Resampler is inactive (source rate == output rate, speed == 1.0).
    pub resampler_bypassed: bool,
    /// Source sample rate matches device hardware rate exactly.
    pub sample_rate_matched: bool,
    /// Source bit depth is NOT truncated by the output container: the output
    /// can represent every source bit. ("Matches" would be too strong — a
    /// 24-bit source in a 32-bit container is fine; the name says what the
    /// check actually verifies.)
    pub bit_depth_not_truncated: bool,
    /// The known source encoding can be represented exactly in the engine's
    /// pipeline and the concrete output container does not quantize it.
    pub format_conversion_lossless: bool,
    /// Concrete output container used for the verdict.
    pub output_format: OutputSampleFormat,
    /// Output was opened in exclusive/direct mode (no OS mixing).
    pub output_exclusive: bool,
    /// **Bit-perfect samples**: the sample sequence is provably identical to
    /// the source — unity volume, no DSP, no SRC, lossless container
    /// conversion, no dither. Distinct from [`Self::bit_perfect_transport`]:
    /// a stream can have an untouched sample path while the transport is
    /// shared/unverified, and vice versa.
    pub bit_perfect_samples: bool,
    /// **Bit-perfect transport**: the digital output transport is provably
    /// direct/exclusive/bitstream with no hidden mixer and no fallback.
    pub bit_perfect_transport: bool,
    /// Access mode requested by the user/config.
    pub access_requested: config::OutputAccessMode,
    /// Access mode actually negotiated with the driver.
    pub access_actual: config::OutputAccessMode,
    /// Whether direct, unmixed hardware access was verified.
    pub access_verified: bool,
    /// True when the output fell back to a lower-fidelity path (e.g. shared
    /// after an exclusive request) — such a path can never be bit-perfect.
    pub fallback_occurred: bool,
    /// Source channel count (1–2 for a bit-perfect-able stream; multichannel
    /// sources are downmixed at the decoder boundary and cannot be bit-exact).
    pub source_channels: u32,
    /// Output channel count.
    pub output_channels: u32,
    /// The source decoder reported a lossless codec (FLAC / ALAC / WAV PCM /
    /// APE / DSD). A lossy source is never bit-perfect by definition.
    pub decoder_lossless: bool,
    /// A crossfade / gapless transition is currently blending two tracks:
    /// the mixer multiplies samples, so bit-exactness is broken even at
    /// unity volume.
    pub crossfade_active: bool,
    /// TPDF dither is being applied at the output quantization boundary
    /// (integer output container + dither enabled). Dither perturbs every
    /// sample by design.
    pub dither_active: bool,
    /// Volume path in use (none / hardware / software DSP).
    pub volume_path: VolumePath,
    /// Coarse product-facing verdict derived from the two booleans above.
    pub result: BitPerfectResult,
    /// First reason why bit-perfect condition failed, if any.
    pub reason: Option<String>,
}

impl BitPerfectReport {
    /// Finalize the verdict after the engine fills the engine-owned fields
    /// (source/output channels, decoder losslessness, crossfade state,
    /// dither state, volume path).
    ///
    /// Dither and crossfade perturb the sample sequence by design, so they
    /// invalidate `bit_perfect_samples` even when every pipeline condition
    /// holds; the coarse `result` and the `is_bit_perfect` flag are then
    /// re-derived from the two halves.
    pub fn finalize_with_engine_state(&mut self) {
        if self.dither_active || self.crossfade_active {
            self.bit_perfect_samples = false;
            if self.reason.is_none() {
                self.reason = Some(if self.dither_active {
                    "Dither is active at the integer quantization boundary".to_string()
                } else {
                    "Crossfade is blending two tracks".to_string()
                });
            }
        }
        self.is_bit_perfect = self.bit_perfect_samples && self.bit_perfect_transport;
        self.result = if self.is_bit_perfect {
            BitPerfectResult::BitPerfect
        } else if self.bit_perfect_samples {
            BitPerfectResult::Unknown
        } else {
            BitPerfectResult::Dsp
        };
    }
}

/// End-to-end latency breakdown across all pipeline stages.
///
/// This is the authoritative graph-level latency model: every term is
/// expressed in the **output domain** (milliseconds at the output sample
/// rate) and `total_latency_ms` is the single number the engine reports as
/// its end-to-end latency. Terms are gathered from the component that owns
/// them — the limiter's lookahead + detector group delay, the convolution's
/// partition delay, the resampler's filter group delay, and the output
/// buffering (ring buffer fill + negotiated device buffer).
#[derive(Debug, Clone, Default)]
pub struct LatencyReport {
    /// Total safety-limiter delay in ms: the predictive lookahead window
    /// **plus** the detector's group delay (the FIR true-peak detector adds
    /// ~0.66 ms at 48 kHz when enabled).
    ///
    /// The limiter is the final safety stage and runs after the resampler, so
    /// this is already expressed in the output domain and needs no source-rate
    /// scaling.
    pub limiter_lookahead_ms: f32,
    /// Detector-only component of [`Self::limiter_lookahead_ms`] (the FIR
    /// true-peak detector's group delay; 0 for sample-peak detection).
    /// Informational — already included in the total.
    pub limiter_detector_delay_ms: f32,
    /// Uniform partitioned convolution delay (ms).
    pub convolution_latency_ms: f32,
    /// Crossfeed delay-line latency (ms); 0 when the crossfeed is disabled.
    /// The low-pass biquads add phase delay too, but the ring-buffer delay is
    /// the deterministic term worth reporting.
    pub crossfeed_delay_ms: f32,
    /// WSOLA analysis-lookahead latency of the time-stretcher (ms); 0 when
    /// the stretcher is inactive (speed/pitch at unity).
    pub timestretch_latency_ms: f32,
    /// Resampler filter group delay (ms), straight from the resampler's own
    /// `output_delay()`. Zero when the resampler is bypassed/passthrough.
    pub resampler_latency_ms: f32,
    /// SPSC frame buffer fill latency (ms).
    pub ring_buffer_latency_ms: f32,
    /// OS audio output device buffer latency (ms) — the negotiated callback
    /// buffer size when the backend reports one; a backend target estimate
    /// when the driver could not report a buffer size (see
    /// `OutputInfo::buffer_size_estimated`).
    pub output_device_latency_ms: f32,
    /// Total end-to-end latency (sum of all stages above) in ms.
    pub total_latency_ms: f32,
}

/// Rich diagnostic snapshot populated by the engine each tick.
/// Intended for a diagnostic panel / bit-perfect indicator in the UI.
#[derive(Debug, Clone, Default)]
pub struct EngineStats {
    // ── Decoder ──────────────────────────────────────────────────────────
    pub decoder_format: String, // e.g. "FLAC 24-bit 96 kHz"
    pub source_bit_depth: u32,
    pub source_sample_rate: u32,

    // ── DSP ──────────────────────────────────────────────────────────────
    pub dsp_precision: String, // "f64" or "f32"
    pub eq_enabled: bool,
    /// Whether automatic EQ headroom is active (reserving the curve's peak
    /// boost as pre-EQ attenuation).
    pub eq_auto_headroom: bool,
    pub compressor_enabled: bool,
    pub limiter_enabled: bool,
    /// Current limiter gain reduction in dB (≤ 0; 0 = no reduction).
    pub limiter_gain_reduction_db: f32,
    /// Whether the signal chain is bypassed for bit-perfect output.
    pub bit_perfect: bool,
    /// If not bit-perfect, the first reason why.
    pub bit_perfect_reason: Option<String>,
    /// Full 6-condition bit-perfect breakdown report.
    pub bit_perfect_report: BitPerfectReport,

    // ── Output ───────────────────────────────────────────────────────────
    pub output_backend: String, // "WASAPI Exclusive" / "ALSA hw:0"
    pub output_sample_rate: u32,
    pub output_bit_depth: u32,
    pub output_format: String,
    pub output_is_exclusive: bool,
    pub output_is_fallback: bool,

    // ── Resampler ────────────────────────────────────────────────────────
    pub resampler_active: bool,
    /// Effective (actually running) quality name — a coarse alias for
    /// [`Self::resampler_effective_quality`].
    pub resampler_quality: String,
    /// The quality profile the user requested.
    pub resampler_requested_quality: String,
    /// The quality profile actually running (may be lower after a
    /// construction-failure fallback).
    pub resampler_effective_quality: String,
    /// True when the running resampler is NOT the requested quality.
    pub resampler_quality_fell_back: bool,

    // ── Metering & Latency ────────────────────────────────────────────────
    pub true_peak_dbtp: f32,
    /// Hard-clipped samples observed in the current reporting window (≈2 s).
    pub clip_count: u32,
    /// Device underruns observed in the current reporting window (≈2 s).
    pub underruns: u32,
    pub output_latency_ms: f32,
    pub latency_report: LatencyReport,

    // ── Timing & Ring Buffer Diagnostics ─────────────────────────────────
    pub dsp_time_us: u64,
    pub worst_dsp_time_us: u64,
    pub total_tick_time_us: u64,
    pub worst_tick_time_us: u64,
    /// Current ring-buffer fill as a fraction of capacity (0.0–1.0).
    pub buffer_fill_ratio: f32,
    pub buffer_capacity_frames: usize,
    pub buffer_available_frames: usize,
    /// Cumulative device underruns since engine start — the total number of
    /// times the audio consumer was starved of samples.
    pub starvation_count: u32,
    /// Engine-thread deadline misses (tick latency > 50 ms while playing) in
    /// the current reporting window.
    pub deadline_miss_count: u32,
}
