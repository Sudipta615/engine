//! Gapless metadata, DSD transport reports, raw payloads, and full format descriptors.

use config;

use crate::decode::channel_layout::ChannelLayout;
use crate::decode::dsd::DsdWireFormat;

/// Encoder/container gapless framing metadata.
///
/// Extracted from MP3 (Xing/LAME/iTunSMPB), AAC (iTunSMPB/ASC priming),
/// FLAC (TOTAL_SAMPLES − logical_frames), and Ogg Vorbis headers.
///
/// All frame counts are in **source-sample-rate** domain (not output rate).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GaplessInfo {
    /// Leading silence frames produced by the encoder to prime its filters.
    /// These must be discarded before the logical audio begins.
    pub encoder_delay: u64,
    /// Trailing silence frames appended to the stream to pad the last
    /// encoder frame. These must be suppressed before raising `EndOfStream`.
    pub end_padding: u64,
    /// Same as `encoder_delay` for most formats; may differ for AAC.
    pub priming_frames: u64,
    /// Physical frames − encoder_delay − end_padding.
    /// `None` when the container does not advertise a total frame count.
    pub total_logical_frames: Option<u64>,
}

impl GaplessInfo {
    /// True when any gapless correction is required (i.e., at least one
    /// field is non-zero).
    pub fn needs_correction(&self) -> bool {
        self.encoder_delay > 0 || self.end_padding > 0
    }

    /// Recompute the decoder's gapless trimming state after a seek.
    ///
    /// Decoders (and container seeks) operate in the **physical** stream
    /// coordinate system, while `frames_to_skip` / `logical_frames_remaining`
    /// operate in the **logical** timeline:
    ///
    /// ```text
    /// physical_frame = encoder_delay + logical_frame
    /// ```
    ///
    /// Returns `(frames_to_skip, logical_frames_remaining)` for a seek that
    /// lands at physical frame `target_physical`.
    ///
    /// - If the target is inside the encoder-delay region, the remaining
    ///   delay frames are still discarded (`frames_to_skip > 0`).
    /// - If the target is past the start, no delay frames remain ahead of it
    ///   and `frames_to_skip == 0`.
    /// - `logical_frames_remaining` is always relative to the new logical
    ///   position (`total_logical_frames − target_logical`), never a stale
    ///   value from the pre-seek position.
    pub fn state_after_seek(&self, target_physical_frames: u64) -> (u64, Option<u64>) {
        let target_logical = target_physical_frames.saturating_sub(self.encoder_delay);
        let frames_to_skip = self.encoder_delay.saturating_sub(target_physical_frames);
        let logical_frames_remaining = self
            .total_logical_frames
            .map(|n| n.saturating_sub(target_logical));
        (frames_to_skip, logical_frames_remaining)
    }
}

// ── DSD Transport State ───────────────────────────────────────────────────────

/// The DSD transport actually in use for the current track.
///
/// Distinct from the *requested* [`config::DsdOutput`] policy: the engine
/// negotiates a transport and reports the result explicitly. A requested
/// `NativeDsd` that the device cannot provide must downgrade through an
/// observable [`DsdTransportReport`], never silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DsdTransport {
    /// DSD was decimated to PCM and flows through the normal pipeline.
    #[default]
    PcmConversion,
    /// DSD-over-PCM: raw DSD packed into 24-bit frames.
    Dop,
    /// Native DSD: raw 1-bit bitstream to a DSD-capable DAC.
    Native,
}

impl DsdTransport {
    pub fn label(self) -> &'static str {
        match self {
            Self::PcmConversion => "PCM conversion",
            Self::Dop => "DoP",
            Self::Native => "Native DSD",
        }
    }
}

impl From<config::DsdOutput> for DsdTransport {
    fn from(o: config::DsdOutput) -> Self {
        match o {
            config::DsdOutput::PcmConvert => Self::PcmConversion,
            config::DsdOutput::DoP => Self::Dop,
            config::DsdOutput::NativeDsd => Self::Native,
        }
    }
}

/// Explicit DSD transport negotiation report (§7, §28).
///
/// Every requested→actual downgrade is recorded as a human-readable step so
/// playback state never lies about what reached the DAC:
///
/// ```text
/// DSD source
/// → native DSD unavailable
/// → fallback: DoP / PCM conversion
/// → exact negotiated output
/// ```
#[derive(Debug, Clone, Default)]
pub struct DsdTransportReport {
    /// Transport requested by the user/config.
    pub requested: DsdTransport,
    /// Transport actually negotiated.
    pub actual: DsdTransport,
    /// Ordered, human-readable fallback steps (empty when no fallback).
    pub fallback_steps: Vec<String>,
    /// Negotiated native-DSD wire format, when `actual == Native`.
    pub wire_format: Option<DsdWireFormat>,
    /// DSD bit rate of the source, when known.
    pub bit_rate: Option<u32>,
}

impl DsdTransportReport {
    pub fn new(requested: DsdTransport, actual: DsdTransport) -> Self {
        Self {
            requested,
            actual,
            fallback_steps: Vec::new(),
            wire_format: None,
            bit_rate: None,
        }
    }

    /// Record one fallback step (in chronological order).
    pub fn step(&mut self, step: impl Into<String>) {
        self.fallback_steps.push(step.into());
    }

    pub fn fell_back(&self) -> bool {
        self.requested != self.actual || !self.fallback_steps.is_empty()
    }

    /// One-line summary for diagnostics.
    pub fn summary(&self) -> String {
        if self.fell_back() {
            format!(
                "{} → {} ({})",
                self.requested.label(),
                self.actual.label(),
                self.fallback_steps.join(" → ")
            )
        } else {
            self.actual.label().to_string()
        }
    }
}

// ── Native DSD Raw Payload ───────────────────────────────────────────────────

/// A chunk of raw (still 1-bit) native DSD payload.
///
/// Carried as a sidecar on [`DecodedChunk`] when the DSD decoder runs in
/// native-DSD transport mode: `samples` is empty and `raw_dsd` holds the
/// de-interleaved, bit-order-normalized (LSB-first) per-channel byte planes.
/// The engine routes these bytes to the negotiated DSD transport — they must
/// never pass through the PCM DSP pipeline (§7: "never apply ordinary PCM DSP
/// to a native DSD bitstream").
#[derive(Debug, Clone)]
pub struct RawDsdChunk {
    /// DSD frames (per channel) in this chunk.
    pub frames: u32,
    /// Number of source channels.
    pub channels: usize,
    /// De-interleaved payload: one normalized (LSB-first) byte-slice per
    /// channel; each byte holds 8 DSD samples (bit 0 = earliest sample).
    pub channel_bytes: Vec<Vec<u8>>,
}

// ── Comprehensive Format Descriptor ──────────────────────────────────────────

/// Full audio format descriptor for a decoded stream.
///
/// Supersedes the lean [`DecodeInfo`] struct for richer UI display and
/// bit-perfect chain verification.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AudioFormatInfo {
    pub codec: String,
    pub container: String,
    /// The sample rate the stream actually decodes at (48 kHz for Opus).
    pub sample_rate: u32,
    /// Original recording sample rate where the container advertises one and
    /// it differs from the decode rate (OpusHead `input_sample_rate`); `None`
    /// for codecs whose decode rate is the source rate.
    pub input_sample_rate: Option<u32>,
    pub channels: usize,
    pub channel_layout: ChannelLayout,
    pub bit_depth: Option<u32>,
    /// "i16" / "i24" / "i32" / "f32" / "f64"
    pub sample_format: String,
    pub duration_secs: Option<f64>,
    pub bitrate_kbps: Option<u32>,
    pub gapless: Option<GaplessInfo>,
    pub replaygain_track_db: Option<f32>,
    pub replaygain_album_db: Option<f32>,
    /// EBU R128 integrated loudness from tags (LUFS).
    pub ebu_r128_loudness: Option<f32>,
    /// True peak from tags (dBTP).
    pub true_peak_dbtp: Option<f32>,
    /// True for lossless formats (FLAC, ALAC, WAV PCM, AIFF, APE).
    pub is_lossless: bool,
    /// True for DSD files (DSF, DFF).
    pub is_dsd: bool,
}

// ── Semantic Channel Identifiers ─────────────────────────────────────────────
