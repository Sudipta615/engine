//! Configuration enums shared across the engine's public API.

use serde::{Deserialize, Serialize};

/// Requested output transport. These are requests, not guarantees: the
/// engine reports the negotiated backend/access state and rejects an
/// exclusive request when the selected backend cannot provide it under the
/// active fallback policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioBackend {
    /// Let the platform choose its default shared output.
    #[default]
    Auto,
    /// Native WASAPI exclusive mode when the native backend is enabled;
    /// cpal's WASAPI path is shared-only and cannot satisfy this request.
    ExclusiveWasapi,
    /// Direct ALSA `hw:`/`plughw:` access on Linux.
    ExclusiveAlsa,
    /// Native CoreAudio hog mode; cpal itself does not implement hog mode.
    ExclusiveCoreAudioHog,
    /// ASIO direct output when the optional ASIO feature is compiled in.
    ExclusiveAsio,
    /// Reserved for an application-provided backend; currently maps to the
    /// platform default shared output in the built-in engine.
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ResamplerQuality {
    Fast,
    #[default]
    Balanced,
    HighQuality,
    /// Longest filter, deepest stopband. Intended for offline-grade
    /// conversion and reference setups; costs the most CPU.
    Ultra,
}

/// Nominal characteristics of each resampler quality tier.
///
/// These are **design targets**: `tests/fidelity/resampler_measurement.rs`
/// measures the realized passband ripple, stopband rejection, and latency of
/// the actual rubato filters each tier maps to, so the numbers below are
/// validated rather than aspirational. rubato 5.0's `Fft` resampler derives
/// the anti-aliasing filter length from `chunk_size / sub_chunks`, so each
/// tier is a genuinely longer filter (≈640 / ≈588 / ≈1029 / ≈2058 taps for
/// 44.1↔48 kHz), with a correspondingly deeper stopband and higher CPU cost.
#[derive(Debug, Clone, PartialEq)]
pub struct ResamplerQualityInfo {
    pub name: &'static str,
    pub cpu_estimate: &'static str,
    pub stopband_attenuation_db: f32,
    pub passband_ripple_db: f32,
    pub typical_latency_ms: f32,
    pub algorithm: &'static str,
}

impl ResamplerQuality {
    pub fn description(&self) -> ResamplerQualityInfo {
        match self {
            Self::Fast => ResamplerQualityInfo {
                name: "Fast",
                cpu_estimate: "Low",
                stopband_attenuation_db: 150.0,
                passband_ripple_db: 0.01,
                typical_latency_ms: 3.3,
                algorithm: "Fft FixedSync::Both (≈320-tap sinc)",
            },
            Self::Balanced => ResamplerQualityInfo {
                name: "Balanced",
                cpu_estimate: "Medium",
                stopband_attenuation_db: 160.0,
                passband_ripple_db: 0.005,
                typical_latency_ms: 6.7,
                algorithm: "Fft FixedSync::Input (≈640-tap sinc)",
            },
            Self::HighQuality => ResamplerQualityInfo {
                name: "High Quality",
                cpu_estimate: "High",
                stopband_attenuation_db: 175.0,
                passband_ripple_db: 0.005,
                typical_latency_ms: 11.7,
                algorithm: "Fft FixedSync::Input (≈1120-tap sinc)",
            },
            Self::Ultra => ResamplerQualityInfo {
                name: "Ultra",
                cpu_estimate: "Very High",
                stopband_attenuation_db: 180.0,
                passband_ripple_db: 0.005,
                typical_latency_ms: 23.3,
                algorithm: "Fft FixedSync::Input (≈2240-tap sinc)",
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PerformanceMode {
    #[default]
    Normal,
    LowPower,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LoudnessMode {
    #[default]
    Off,
    TrackReplayGain,
    AlbumReplayGain,
    EbuR128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FilterType {
    #[default]
    Peaking,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    Notch,
    Bandpass,
    AllPass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CrossfeedProfile {
    #[default]
    Bauer,
    ChuMoy,
    Jmeier,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CrossfadeCurve {
    #[default]
    ConstantPower,
    Linear,
    Exponential,
    Logarithmic,
    SCurve,
}

/// Explicit track-to-track transition mode.
/// `Crossfade` intentionally overlaps audio; `Gapless` preserves the exact
/// logical sample boundary (no overlap, no silence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TransitionMode {
    /// Sample-accurate gapless: hand off at logical EOS with zero silence or overlap.
    #[default]
    Gapless,
    /// Overlapping crossfade (duration set by `CrossfadeConfig::duration_ms`).
    Crossfade,
    /// Fade-out current track, silence gap, fade-in next.
    Fade,
    /// Stop at EOS; do not auto-advance.
    Stop,
}

/// Playback speed mode: whether changing speed also changes pitch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SpeedMode {
    /// Varispeed: pitch changes proportionally with speed (resampling-based).
    #[default]
    Varispeed,
    /// Time-stretch: pitch remains constant regardless of speed (WSOLA algorithm).
    TimeStretch,
    /// Pitch-shift: tempo remains constant, pitch transposed (WSOLA algorithm).
    PitchShift,
}

/// Time-stretch/pitch-shift quality tier (spec §22).
///
/// Maps to real, measurable WSOLA parameter changes — synthesis window
/// length, overlap ratio, and waveform-similarity search range — not to a
/// marketing label. Higher tiers use longer analysis windows and finer
/// alignment search, which improves transient and sustained-tonal fidelity
/// at the cost of CPU and algorithmic latency. See
/// [`TimeStretchQuality::params`] for the exact numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TimeStretchQuality {
    /// Smallest window (512), 25% hop of window, narrow search. Lowest CPU
    /// and algorithmic latency; usable for speech/monologue where
    /// transient alignment is less critical.
    Low,
    /// 1024-sample window, 75% overlap, 128-sample search — the historical
    /// WSOLA defaults. Balanced quality/cost trade-off.
    #[default]
    Balanced,
    /// 2048-sample window, 75% overlap, 256-sample search. Best alignment
    /// for sustained tonal and polyphonic material at roughly double the
    /// search cost and double the algorithmic latency of `Balanced`.
    High,
}

impl TimeStretchQuality {
    /// Concrete WSOLA parameters for this tier as `(window, hop, search)`.
    ///
    /// `window` is the synthesis-frame length in samples, `hop` the
    /// synthesis hop (25% of `window` → 75% overlap-add), and `search` the
    /// ±sample range over which waveform-similarity alignment is evaluated.
    pub fn params(&self) -> (usize, usize, usize) {
        match self {
            Self::Low => (512, 128, 64),
            Self::Balanced => (1024, 256, 128),
            Self::High => (2048, 512, 256),
        }
    }

    /// One-line human description (used by diagnostics, spec §22/§29).
    pub fn description(&self) -> &'static str {
        match self {
            Self::Low => "512-sample window / 128-hop / 64-search — lowest CPU & latency",
            Self::Balanced => "1024-sample window / 256-hop / 128-search — balanced quality",
            Self::High => {
                "2048-sample window / 512-hop / 256-search — best transient & tonal fidelity"
            }
        }
    }
}

/// How DSD audio is handled at the output stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DsdOutput {
    /// Convert DSD to PCM before any further processing (safe default).
    #[default]
    PcmConvert,
    /// DSD-over-PCM (DoP): requires I32 output and a DoP-capable DAC.
    DoP,
    /// Native DSD bitstream output via direct DAC transport.
    ///
    /// Supported today by the native ALSA backend (`ExclusiveAlsa` with a
    /// direct `hw:` node) for DSD_U8/U16/U32 wire formats; on every other
    /// backend the engine negotiates an explicit downgrade — Native → DoP →
    /// PCM conversion — recorded in `DsdTransportReport` (never a silent
    /// fallback).
    ///
    /// Platform status (verified against the `Output` trait contract):
    /// - **ALSA** (exact `hw:` node) — raw native DSD works; `plughw:` is
    ///   rejected for native transport because its conversion plugin is not
    ///   bit-perfect.
    /// - **WASAPI exclusive** — DoP works (I32 container at bit_rate/16);
    ///   raw native DSD has no WASAPI stream format and is not possible.
    /// - **ASIO** (`ExclusiveAsio`, via cpal) — DoP works (the engine's DoP
    ///   path reopens the stream as I32 at bit_rate/16); raw native DSD
    ///   requires a vendor ASIO DSD extension (e.g. the Thesycon-style
    ///   private format) and is not implemented in this build.
    /// - **CoreAudio** (macOS) — no native CoreAudio DSD stream format exists;
    ///   DoP over a hog-mode CoreAudio backend is the target, and is not yet
    ///   implemented (cpal's CoreAudio path is shared-only).
    NativeDsd,
}

/// Per-channel routing policy for multichannel sources.
///
/// ## Architectural limitation
///
/// Multichannel preservation is conditional. The source layout, output
/// layout, DSP channel limit (`MAX_CHANNELS`), and resampler state must all be
/// compatible. When any condition is not met, the engine performs a documented
/// stereo downmix rather than dropping channels silently. The codec's decode
/// capability and the output device's channel capability are reported by
/// separate layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ChannelPolicy {
    /// Always downmix to stereo at the decoder boundary (default behaviour).
    /// Mono sources are duplicated to both channels; multichannel is ITU-R BS.775.
    #[default]
    ForceDownmixStereo,
    /// Request the source channel layout.
    ///
    /// For 1–2 channel sources this preserves the layout. For wider sources it
    /// is preserved only when the output has the same width, the width is
    /// supported by the DSP path, and no resampler is active; otherwise the
    /// engine explicitly downmixes to stereo.
    PassThrough,
    /// Request up to N source channels; downmix if the source has more than N
    /// or if the output/DSP path cannot preserve the requested layout. N is
    /// also bounded by the engine's maximum supported channel count.
    MaxChannels(u8),
    /// Render a spatial scene through a spatial renderer (`SpatialObjects`
    /// mode, spec §6 / Part I §6).
    ///
    /// In this mode the conventional decoded-signal path is routed through
    /// the engine's spatial renderer, which places scene objects onto the
    /// negotiated output layout. This is **opt-in**: the default remains
    /// [`Self::ForceDownmixStereo`], and every other access to the channel
    /// engine is unchanged. Beds/fields and object audio that have no spatial
    /// counterpart fall back to a conventional downmix so no channel is
    /// silently dropped.
    SpatialRender,
}

/// Envelope detector mode for a compressor band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CompressorDetector {
    /// Instantaneous peak follower (classic; reacts to transients).
    #[default]
    Peak,
    /// Windowed RMS level (reacts to program level, not transients).
    Rms,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FallbackPolicy {
    Strict,
    #[default]
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VolumeMode {
    /// Apply gain in the DSP pipeline and never touch endpoint/hardware
    /// volume control (spec §12 "SoftwareOnly").
    #[default]
    SoftwareOnly,
    /// Prefer native endpoint volume control: when the active backend
    /// supports hardware volume, the DSP pipeline runs at unity and the
    /// endpoint owns the level. When it does not (e.g. ASIO), the engine
    /// falls back to software DSP gain instead of failing (spec §12
    /// "HardwarePreferred").
    HardwarePreferred,
    /// Hardware endpoint volume is **required** (spec §12 "HardwareOnly").
    ///
    /// When the active backend has no endpoint volume control, the engine
    /// does **not** fall back to software gain — it leaves the signal
    /// untouched (software pipeline at unity) and reports the failure in
    /// `PlaybackInfo::volume_error`. This is the strict companion to
    /// [`Self::HardwarePreferred`], and the mode that guarantees a
    /// bit-perfect request can never be silently turned into a
    /// sample-modifying software path (§5.1).
    HardwareOnly,
    /// Software gain is explicitly permitted (spec §12 "SoftwareAllowed").
    ///
    /// Runtime behaviour matches [`Self::SoftwareOnly`] (DSP gain, endpoint
    /// never touched); the variant exists to complete the §12 four-mode
    /// contract as an explicit opt-in for setups that accept software
    /// attenuation and therefore do not claim bit-perfect transport.
    SoftwareAllowed,
}

/// Per-output dither policy (spec §10 profile: "Dither: Off / TPDF").
/// `FollowGlobal` uses the engine-wide `config.dither_enabled`; the force
/// variants override it for a specific device profile (e.g. always-on TPDF
/// for a noisy 16-bit DAC, always-off for a 24-bit reference DAC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DitherPolicy {
    /// Use the engine-wide `dither_enabled` setting.
    #[default]
    FollowGlobal,
    /// Force TPDF dither at the integer-quantization boundary.
    ForceOn,
    /// Force dither off for this device.
    ForceOff,
}

/// Fallback strategy when the exact desired sample rate is not supported by the hardware device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RateFallbackPolicy {
    /// Pick the supported rate with minimal numerical difference.
    #[default]
    Nearest,
    /// Prefer the lowest supported rate >= target (avoiding downsampling), falling back to highest supported.
    PreferHigher,
    /// Prefer the highest supported rate <= target, falling back to lowest supported.
    PreferLower,
    /// Prefer rates in the same clock family first, then nearest.
    SameFamilyFirst,
}

/// DSP signal-path precision.
///
/// ## Actual precision guarantees
///
/// **Performance (f32):** the pipeline's sample buffers and the final safety
/// limiter are f32. Most processors still accumulate internally in f64 where
/// it matters (biquad states, convolution accumulation), and the EQ cascade
/// promotes once to f64 at the EQ boundary and demotes once at its exit —
/// there are no per-band precision conversions.
///
/// **Quality (f64):** the entire chain — loudness, EQ cascade, multiband
/// compressor, convolution, crossfeed, stereo, volume, seek fade — runs in
/// f64, demoted to f32 only at the output boundary (and into the final
/// safety limiter, which is deliberately f32 in both modes).
///
/// ## Documented exceptions
///
/// - **Time-stretch / pitch-shift** (`TimeStretcher`): the WSOLA synthesis
///   core operates internally in f32 for cache locality and SIMD
///   cross-correlation in **both** modes; Quality mode converts the f64
///   stream to f32 around that core. The core's interpolation is a
///   precomputed 16-tap windowed-sinc table, so this is a deliberate,
///   documented precision trade-off rather than an accident.
/// - **Resampler** (rubato): f32/f64 follows the precision mode, but the
///   internal FFT engine precision is rubato's own; the configured quality
///   tier (Fast/Balanced/HighQuality) governs its filter characteristics
///   (see [`ResamplerQuality::description`]).
/// - **Final safety limiter**: f32 in both modes (it runs in the output
///   domain after the resampler, where the sample container is f32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PrecisionMode {
    #[default]
    Performance,
    Quality,
}

/// Operating access mode of an audio output stream.
///
/// Lives in the config crate (not the output module) so the DSP pipeline's
/// bit-perfect report — which is compiled in builds without the
/// `audio-output` feature — can carry the real requested/actual access
/// vocabulary instead of a boolean approximation. `src/output/capabilities.rs`
/// re-exports it for API compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum OutputAccessMode {
    /// Shared system mixer (e.g. PulseAudio, PipeWire, WASAPI Shared, CoreAudio Default).
    #[default]
    Shared,
    /// Exclusive mode stream (e.g. WASAPI Exclusive, CoreAudio Hog Mode).
    Exclusive,
    /// Direct hardware access endpoint (e.g. ALSA `hw:X,Y` direct device node).
    DirectHw,
    /// Bitstream pass-through mode for encoded or DSD streams.
    BitstreamPassthrough,
}

impl OutputAccessMode {
    /// True if the access mode bypasses the OS audio mixer for bit-perfect output.
    pub fn is_direct(&self) -> bool {
        matches!(
            self,
            Self::Exclusive | Self::DirectHw | Self::BitstreamPassthrough
        )
    }
}

/// Comprehensive output access state distinguishing requested access, actual
/// negotiated access, and whether direct hardware access was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct OutputAccessState {
    /// Access mode requested by the user/config.
    pub requested: OutputAccessMode,
    /// Access mode actually negotiated with the driver.
    pub actual: OutputAccessMode,
    /// Whether direct, unmixed hardware access was verified.
    pub verified: bool,
}

impl OutputAccessState {
    pub fn is_bit_perfect(&self) -> bool {
        self.actual.is_direct() && self.verified
    }
}
