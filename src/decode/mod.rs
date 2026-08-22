use std::path::Path;

use crate::buffer::MAX_CHANNELS;

pub mod ape;
pub mod codecs;
pub mod cue;
pub mod decoder;
pub mod dsd;
pub mod loudness_cache;
#[cfg(feature = "codec-opus")]
pub mod opus;
pub mod scanner;
pub mod symphonia_decoder;
#[cfg(feature = "codec-tta")]
pub mod tta;
#[cfg(feature = "codec-wavpack")]
pub mod wavpack;

#[cfg(feature = "codec-ape")]
pub use ape::ApeDecoder;
pub use codecs::{
    all_codecs, capability, for_codec_string, for_extension, Codec, CodecCapability, CodecStatus,
    CodecSupportLevel,
};
pub use cue::{CueIndex, CueParseError, CueSheet, CueTrack};
pub use decoder::{Decoder, DsdDecoder};
pub use dsd::{
    DopPacker, DsdBlock, DsdError, DsdPcmBlock, DsdRate, DsdReader, DsdToPcmDecimator,
    DsdWireFormat, NativeDsdPacker,
};
#[cfg(feature = "codec-opus")]
pub use opus::OpusSource;
pub use scanner::{scan_track_loudness, LoudnessScanResult};
pub use symphonia_decoder::{
    downmix_interleaved_to_stereo, extract_loudness_metadata_symphonia, DecodeError, DecodeInfo,
    DecodedChunk, SymphoniaDecoder,
};
#[cfg(feature = "codec-tta")]
pub use tta::TtaDecoder;
#[cfg(feature = "codec-wavpack")]
pub use wavpack::WavpackDecoder;

// ── Format-routing metadata extractors ───────────────────────────────────────
//
// The standalone metadata extractors below dispatch by file extension so a
// single entry point serves every codec: Ogg Opus tags can only be read by
// the Opus backend (`opus-decoder`/`ogg`), everything else by Symphonia's
// probe. Callers should use these instead of reaching into the per-backend
// modules.

/// True when the path is an Ogg Opus file and the `codec-opus` feature is
/// enabled (OpusTags are not readable through Symphonia's probe).
#[allow(dead_code)]
fn is_opus_path(path: &Path) -> bool {
    #[cfg(feature = "codec-opus")]
    {
        return path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("opus"));
    }
    #[cfg(not(feature = "codec-opus"))]
    {
        let _ = path;
        false
    }
}

/// Extract title, artist, album, duration (seconds), and a formatted
/// duration string. Routes `.opus` to the Opus backend, everything else to
/// Symphonia.
pub fn extract_track_metadata(path: &Path) -> (String, String, String, f64, String) {
    #[cfg(feature = "codec-opus")]
    if is_opus_path(path) {
        return opus::extract_track_metadata(path);
    }
    symphonia_decoder::extract_track_metadata(path)
}

/// Extract ReplayGain / EBU R128 loudness metadata from file tags. Routes
/// `.opus` to the Opus backend (OpusTags), everything else to Symphonia.
pub fn extract_loudness_metadata(path: &Path) -> crate::dsp::loudness::LoudnessMetadata {
    #[cfg(feature = "codec-opus")]
    if is_opus_path(path) {
        return opus::extract_loudness_metadata(path);
    }
    symphonia_decoder::extract_loudness_metadata_symphonia(path)
}

// ── Gapless Playback ─────────────────────────────────────────────────────────

/// Encoder/container gapless framing metadata.
///
/// Extracted from MP3 (Xing/LAME/iTunSMPB), AAC (iTunSMPB/ASC priming),
/// FLAC (TOTAL_SAMPLES − logical_frames), and Ogg Vorbis headers.
///
/// All frame counts are in **source-sample-rate** domain (not output rate).
#[derive(Debug, Clone, Default)]
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
            format!("{}", self.actual.label())
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
#[derive(Debug, Clone, Default)]
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

/// Semantic audio channel identifier (follows ITU-R BS.2051 / AES67).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelId {
    FrontLeft,
    FrontRight,
    Center,
    Lfe,
    SideLeft,
    SideRight,
    RearLeft,
    RearRight,
    BackCenter,
    TopFrontLeft,
    TopFrontRight,
    TopRearLeft,
    TopRearRight,
    Unknown(u8),
}

/// Semantic multi-channel layout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ChannelLayout {
    Mono,
    #[default]
    Stereo,
    TwoPointOne,    // FL FR LFE
    ThreePointZero, // FL FR C
    ThreePointOne,  // FL FR C LFE
    FourPointZero,  // FL FR SL SR
    FourPointOne,   // FL FR LFE SL SR
    FivePointZero,  // FL FR C SL SR
    FivePointOne,   // FL FR C LFE SL SR
    SixPointOne,    // FL FR C LFE SL SR BC
    SevenPointZero, // FL FR C SL SR RL RR
    SevenPointOne,  // FL FR C LFE SL SR RL RR
    /// 7.1.4: FL FR C LFE SL SR RL RR + four overheads (TFL TFR TRL TRR).
    SevenPointOneFour,
    Custom(Vec<ChannelId>),
}

impl ChannelLayout {
    /// Number of channels in this layout.
    pub fn channel_count(&self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::TwoPointOne => 3,
            Self::ThreePointZero => 3,
            Self::ThreePointOne => 4,
            Self::FourPointZero => 4,
            Self::FourPointOne => 5,
            Self::FivePointZero => 5,
            Self::FivePointOne => 6,
            Self::SixPointOne => 7,
            Self::SevenPointZero => 7,
            Self::SevenPointOne => 8,
            Self::SevenPointOneFour => 12,
            Self::Custom(ids) => ids.len(),
        }
    }

    /// The ordered semantic channel IDs for this layout.
    ///
    /// The order follows the conventional WAV / Symphonia channel ordering
    /// (FL, FR, C, LFE, SL, SR, RL, RR).  Downmixers, loudness weighting and
    /// channel mappers should derive channel *semantics* from this list
    /// instead of assuming `channel[2]` means "center" etc.
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        match self {
            Self::Mono => vec![ChannelId::FrontLeft],
            Self::Stereo => vec![ChannelId::FrontLeft, ChannelId::FrontRight],
            Self::TwoPointOne => vec![ChannelId::FrontLeft, ChannelId::FrontRight, ChannelId::Lfe],
            Self::ThreePointZero => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
            ],
            Self::ThreePointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
            ],
            Self::FourPointZero => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::SideLeft,
                ChannelId::SideRight,
            ],
            Self::FourPointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
            ],
            Self::FivePointZero => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::SideLeft,
                ChannelId::SideRight,
            ],
            Self::FivePointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
            ],
            Self::SixPointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
                ChannelId::BackCenter,
            ],
            Self::SevenPointZero => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::SideLeft,
                ChannelId::SideRight,
                ChannelId::RearLeft,
                ChannelId::RearRight,
            ],
            Self::SevenPointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
                ChannelId::RearLeft,
                ChannelId::RearRight,
            ],
            Self::SevenPointOneFour => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
                ChannelId::RearLeft,
                ChannelId::RearRight,
                ChannelId::TopFrontLeft,
                ChannelId::TopFrontRight,
                ChannelId::TopRearLeft,
                ChannelId::TopRearRight,
            ],
            Self::Custom(ids) => ids.clone(),
        }
    }

    /// Index of the first channel with the given semantic role, if present.
    pub fn position_of(&self, id: ChannelId) -> Option<usize> {
        self.channel_ids().iter().position(|c| *c == id)
    }

    /// Build a `ChannelLayout` from a raw channel count using the
    /// conventional WAV/Symphonia channel ordering.
    pub fn from_count(n: usize) -> Self {
        match n {
            1 => Self::Mono,
            2 => Self::Stereo,
            3 => Self::ThreePointZero,
            4 => Self::FourPointZero,
            5 => Self::FivePointZero,
            6 => Self::FivePointOne,
            7 => Self::SevenPointZero,
            8 => Self::SevenPointOne,
            12 => Self::SevenPointOneFour,
            _ => Self::Custom((0..n as u8).map(ChannelId::Unknown).collect()),
        }
    }
}

// ── Explicit multichannel upmix/downmix templates ────────────────────────────

fn set_mix_gain(
    matrix: &mut [[f32; MAX_CHANNELS]; MAX_CHANNELS],
    source: Option<usize>,
    destination: Option<usize>,
    gain: f32,
) {
    if let (Some(src), Some(dst)) = (source, destination) {
        if src < MAX_CHANNELS && dst < MAX_CHANNELS {
            matrix[src][dst] += gain;
        }
    }
}

fn role_identity(
    source_layout: &ChannelLayout,
    target_layout: &ChannelLayout,
    matrix: &mut [[f32; MAX_CHANNELS]; MAX_CHANNELS],
) {
    for id in source_layout.channel_ids() {
        set_mix_gain(
            matrix,
            source_layout.position_of(id),
            target_layout.position_of(id),
            1.0,
        );
    }
}

/// Build the fixed matrix for a named template. Matrix orientation is
/// `[source][destination]`, matching `ChannelRoutingConfig`.
fn build_mix_matrix(
    source_layout: &ChannelLayout,
    target_layout: &ChannelLayout,
    template: &config::ChannelMixTemplate,
) -> [[f32; MAX_CHANNELS]; MAX_CHANNELS] {
    let mut matrix = [[0.0f32; MAX_CHANNELS]; MAX_CHANNELS];
    let source_count = source_layout.channel_count();
    let target_count = target_layout.channel_count();

    match template {
        config::ChannelMixTemplate::Custom(custom) => {
            if custom.len() == source_count
                && target_count <= MAX_CHANNELS
                && custom.iter().all(|row| row.len() == target_count)
            {
                for src in 0..source_count.min(MAX_CHANNELS) {
                    for dst in 0..target_count.min(MAX_CHANNELS) {
                        matrix[src][dst] = custom[src][dst];
                    }
                }
            } else {
                log::warn!(
                    "ChannelMix custom matrix shape does not match {}x{}; using semantic identity",
                    source_count,
                    target_count
                );
                role_identity(source_layout, target_layout, &mut matrix);
            }
        }
        config::ChannelMixTemplate::StereoToFiveOne
        | config::ChannelMixTemplate::StereoToSevenOne
        | config::ChannelMixTemplate::StereoToSevenPointOneFour
            if source_count == 2 =>
        {
            let fl = source_layout.position_of(ChannelId::FrontLeft).or(Some(0));
            let fr = source_layout.position_of(ChannelId::FrontRight).or(Some(1));
            set_mix_gain(
                &mut matrix,
                fl,
                target_layout.position_of(ChannelId::FrontLeft),
                1.0,
            );
            set_mix_gain(
                &mut matrix,
                fr,
                target_layout.position_of(ChannelId::FrontRight),
                1.0,
            );
            set_mix_gain(
                &mut matrix,
                fl,
                target_layout.position_of(ChannelId::Center),
                std::f32::consts::FRAC_1_SQRT_2,
            );
            set_mix_gain(
                &mut matrix,
                fr,
                target_layout.position_of(ChannelId::Center),
                std::f32::consts::FRAC_1_SQRT_2,
            );
            // Conservative decorrelated-free fill: surrounds/rears receive a
            // half-level copy, while LFE remains silent by design.
            for (src, dst) in [
                (fl, ChannelId::SideLeft),
                (fl, ChannelId::RearLeft),
                (fr, ChannelId::SideRight),
                (fr, ChannelId::RearRight),
            ] {
                set_mix_gain(&mut matrix, src, target_layout.position_of(dst), 0.5);
            }
            if matches!(
                template,
                config::ChannelMixTemplate::StereoToSevenPointOneFour
            ) {
                for (src, dst) in [
                    (fl, ChannelId::TopFrontLeft),
                    (fl, ChannelId::TopRearLeft),
                    (fr, ChannelId::TopFrontRight),
                    (fr, ChannelId::TopRearRight),
                ] {
                    set_mix_gain(
                        &mut matrix,
                        src,
                        target_layout.position_of(dst),
                        0.353_553_4,
                    );
                }
            }
        }
        config::ChannelMixTemplate::FiveOneToStereo
        | config::ChannelMixTemplate::SevenOneToStereo
        | config::ChannelMixTemplate::SevenPointOneFourToStereo
        | config::ChannelMixTemplate::ItuBs775
            if target_count == 2 =>
        {
            // ITU-R BS.775-compatible fold. The named templates deliberately
            // share this conservative matrix; the 7.1.4 variant additionally
            // folds overhead content at a lower level.
            set_mix_gain(
                &mut matrix,
                source_layout.position_of(ChannelId::FrontLeft),
                target_layout.position_of(ChannelId::FrontLeft),
                1.0,
            );
            set_mix_gain(
                &mut matrix,
                source_layout.position_of(ChannelId::FrontRight),
                target_layout.position_of(ChannelId::FrontRight),
                1.0,
            );
            // Center and back-center are shared; lateral/rear speakers stay
            // on their corresponding side so a left-only surround impulse
            // cannot leak into the right output.
            for (id, gain) in [
                (ChannelId::Center, std::f32::consts::FRAC_1_SQRT_2),
                (ChannelId::BackCenter, 0.5),
            ] {
                set_mix_gain(
                    &mut matrix,
                    source_layout.position_of(id),
                    target_layout.position_of(ChannelId::FrontLeft),
                    gain,
                );
                set_mix_gain(
                    &mut matrix,
                    source_layout.position_of(id),
                    target_layout.position_of(ChannelId::FrontRight),
                    gain,
                );
            }
            for (id, destination, gain) in [
                (
                    ChannelId::SideLeft,
                    ChannelId::FrontLeft,
                    std::f32::consts::FRAC_1_SQRT_2,
                ),
                (
                    ChannelId::SideRight,
                    ChannelId::FrontRight,
                    std::f32::consts::FRAC_1_SQRT_2,
                ),
                (ChannelId::RearLeft, ChannelId::FrontLeft, 0.5),
                (ChannelId::RearRight, ChannelId::FrontRight, 0.5),
            ] {
                set_mix_gain(
                    &mut matrix,
                    source_layout.position_of(id),
                    target_layout.position_of(destination),
                    gain,
                );
            }
            if matches!(
                template,
                config::ChannelMixTemplate::SevenPointOneFourToStereo
            ) {
                for (id, destination) in [
                    (ChannelId::TopFrontLeft, ChannelId::FrontLeft),
                    (ChannelId::TopRearLeft, ChannelId::FrontLeft),
                    (ChannelId::TopFrontRight, ChannelId::FrontRight),
                    (ChannelId::TopRearRight, ChannelId::FrontRight),
                ] {
                    set_mix_gain(
                        &mut matrix,
                        source_layout.position_of(id),
                        target_layout.position_of(destination),
                        0.353_553_4,
                    );
                }
            }
            // LFE is never silently mixed into mains.
        }
        _ => role_identity(source_layout, target_layout, &mut matrix),
    }
    matrix
}

/// Mix an interleaved source into an interleaved target using an explicit
/// template. The matrix is built on the stack and all output writes are
/// bounded by the declared layouts; no per-frame allocation occurs.
pub fn mix_interleaved_with_template(
    samples: &[f32],
    source_layout: &ChannelLayout,
    source_channels: usize,
    target_layout: &ChannelLayout,
    template: &config::ChannelMixTemplate,
    output: &mut [f32],
    frames: usize,
) -> usize {
    let source_channels = source_channels.min(MAX_CHANNELS);
    let target_channels = target_layout.channel_count().min(MAX_CHANNELS);
    let actual_frames = (samples.len() / source_channels.max(1))
        .min(frames)
        .min(output.len() / target_channels.max(1));
    let matrix = build_mix_matrix(source_layout, target_layout, template);
    for frame in 0..actual_frames {
        let src_base = frame * source_channels;
        let dst_base = frame * target_channels;
        for dst in 0..target_channels {
            let mut value = 0.0f32;
            for src in 0..source_channels {
                value += samples[src_base + src] * matrix[src][dst];
            }
            output[dst_base + dst] = value;
        }
    }
    actual_frames
}

/// Stereo-plane form used by the decode loop's downmix path.
pub fn mix_interleaved_to_stereo_with_template(
    samples: &[f32],
    source_layout: &ChannelLayout,
    source_channels: usize,
    template: &config::ChannelMixTemplate,
    plane_l: &mut [f32],
    plane_r: &mut [f32],
    frames: usize,
) -> usize {
    let source_channels = source_channels.min(MAX_CHANNELS);
    let actual_frames = (samples.len() / source_channels.max(1))
        .min(frames)
        .min(plane_l.len())
        .min(plane_r.len());
    let matrix = build_mix_matrix(source_layout, &ChannelLayout::Stereo, template);
    for frame in 0..actual_frames {
        let base = frame * source_channels;
        let mut left = 0.0f32;
        let mut right = 0.0f32;
        for src in 0..source_channels {
            left += samples[base + src] * matrix[src][0];
            right += samples[base + src] * matrix[src][1];
        }
        plane_l[frame] = left;
        plane_r[frame] = right;
    }
    actual_frames
}

// ── Source-Precision Sample Payload ──────────────────────────────────────────
//
// NOTE: a native-precision `DecodedSamples { F32, I32 }` enum was introduced
// in an earlier iteration but never wired into the decode pipeline — the
// decoder still returned `Vec<f32>` everywhere, so the I32 variant was dead
// architecture.  It has been removed: half-integrated precision systems tend
// to create subtle format-specific bugs, and the engine's documented signal
// path is `source → f32/f64 → DSP → f32` (see `PrecisionMode`).  If native
// 32-bit integer transport is ever wanted, it must be threaded through
// `DecodedChunk` and the output stage together — not added as a parallel
// enum.
