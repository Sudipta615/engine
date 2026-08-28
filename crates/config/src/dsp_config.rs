//! Per-processor DSP configuration structs.

use serde::{Deserialize, Serialize};

use super::enums::{
    CompressorDetector, CrossfadeCurve, CrossfeedProfile, FilterType, LoudnessMode,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossfadeConfig {
    pub enabled: bool,
    pub duration_ms: u64,
    pub curve: CrossfadeCurve,
}

impl Default for CrossfadeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            duration_ms: 2000,
            curve: CrossfadeCurve::ConstantPower,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqBandConfig {
    pub enabled: bool,
    pub filter_type: FilterType,
    pub frequency: f32,
    pub gain_db: f32,
    pub q: f32,
}

impl Default for EqBandConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            filter_type: FilterType::Peaking,
            frequency: 1000.0,
            gain_db: 0.0,
            q: 1.0,
        }
    }
}

/// A named EQ preset, optionally scoped to a specific output device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqPreset {
    /// Human-readable preset name.
    pub name: String,
    /// If `Some`, this preset is automatically selected when the output device
    /// name contains this substring (case-insensitive).
    pub output_device_pattern: Option<String>,
    pub preamp_db: f32,
    pub bands: Vec<EqBandConfig>,
}

impl EqPreset {
    /// Parse an AutoEQ `ParametricEQ.txt` CSV line-format into an `EqPreset`.
    /// Format: `Filter N: ON PK Fc X Hz Gain Y dB Q Z`
    pub fn from_autoeq(name: &str, text: &str) -> Self {
        let mut bands = Vec::new();
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // AutoEQ format: Filter N: ON PK/LSC/HSC Fc Hz Gain dB Q
            // Token layout (split_whitespace):
            //   0:"Filter" 1:"N:" 2:"ON" 3:"PK" 4:"Fc" 5:"<freq>" 6:"Hz"
            //   7:"Gain" 8:"<gain>" 9:"dB" 10:"Q" 11:"<q>"
            if parts.len() < 12 {
                continue;
            }
            if parts.get(2) != Some(&"ON") {
                continue;
            }
            let filter_type = match parts.get(3) {
                Some(&"PK") => FilterType::Peaking,
                Some(&"LSC") => FilterType::LowShelf,
                Some(&"HSC") => FilterType::HighShelf,
                Some(&"LP") => FilterType::LowPass,
                Some(&"HP") => FilterType::HighPass,
                _ => continue,
            };
            let freq: f32 = parts.get(5).and_then(|s| s.parse().ok()).unwrap_or(1000.0);
            let gain: f32 = parts.get(8).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let q: f32 = parts.get(11).and_then(|s| s.parse().ok()).unwrap_or(1.0);
            bands.push(EqBandConfig {
                enabled: true,
                filter_type,
                frequency: freq,
                gain_db: gain,
                q,
            });
        }
        EqPreset {
            name: name.to_string(),
            output_device_pattern: None,
            preamp_db: 0.0,
            bands,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqConfig {
    pub enabled: bool,
    pub preamp_db: f32,
    pub post_gain_db: f32,
    pub headroom_db: f32,
    /// Automatically reserve headroom equal to the EQ curve's peak combined
    /// boost so the limiter never has to absorb the EQ's own gain.
    #[serde(default)]
    pub auto_headroom: bool,
    pub bands: Vec<EqBandConfig>,
    /// Saved presets (serialized alongside the config).
    #[serde(default)]
    pub presets: Vec<EqPreset>,
}

impl EqConfig {
    /// Build a 64-band logarithmically-spaced graphic EQ configuration.
    ///
    /// Bands are spaced from 20 Hz to 20 kHz on a logarithmic scale,
    /// providing 64 independent parametric bands each with a narrow Q (3.0)
    /// suited for graphic-EQ style boosts/cuts.
    pub fn default_64_band() -> Self {
        use std::f32::consts::LN_10;
        let n = 64usize;
        let f_lo: f32 = 20.0;
        let f_hi: f32 = 20_000.0;
        let log_lo = f_lo.ln();
        let log_hi = f_hi.ln();
        let bands = (0..n)
            .map(|i| {
                let t = i as f32 / (n - 1) as f32;
                let freq = (log_lo + t * (log_hi - log_lo)).exp();
                EqBandConfig {
                    enabled: true,
                    filter_type: FilterType::Peaking,
                    frequency: freq,
                    gain_db: 0.0,
                    q: 3.0, // narrow Q for graphic-EQ style
                }
            })
            .collect();
        // Suppress unused variable warning on LN_10
        let _ = LN_10;
        Self {
            enabled: false,
            preamp_db: 0.0,
            post_gain_db: 0.0,
            headroom_db: 0.0,
            auto_headroom: false,
            bands,
            presets: Vec::new(),
        }
    }
}

/// Band layout for the graphic EQ layer (§9.1).
///
/// Every layout is a fixed, documented frequency ladder. The first and last
/// bands compile to shelving filters; the interior bands are peaking filters
/// with a bandwidth-derived Q (see [`GraphicEqLayout::q`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum GraphicEqLayout {
    /// 10-band, 1-octave ISO ladder (31.5 Hz – 16 kHz).
    #[default]
    TenBand,
    /// 15-band, 2/3-octave ISO ladder (25 Hz – 16 kHz).
    FifteenBand,
    /// 31-band, 1/3-octave ISO ladder (20 Hz – 20 kHz).
    ThirtyOneBand,
    /// 32-band, 1/3-octave ISO ladder (16 Hz – 20 kHz).
    ThirtyTwoBand,
    /// 64-band, 1/6-octave ladder (20 Hz – ≈ 29 kHz).
    SixtyFourBand,
    /// Explicit frequency ladder in Hz. The first and last entries become
    /// shelving filters; interior entries become peaking filters. Band count
    /// is clamped to the engine's `MAX_EQ_BANDS` (64).
    Custom(Vec<f32>),
}

impl GraphicEqLayout {
    /// Band spacing in octaves (used to derive the standard Q).
    pub fn bandwidth_octaves(&self) -> f32 {
        match self {
            Self::TenBand => 1.0,
            Self::FifteenBand => 2.0 / 3.0,
            Self::ThirtyOneBand | Self::ThirtyTwoBand | Self::Custom(_) => 1.0 / 3.0,
            Self::SixtyFourBand => 1.0 / 6.0,
        }
    }

    /// Quality factor derived from the band spacing, matching the standard
    /// graphic-EQ convention `Q = 1 / (2·sinh(ln(2)·B/2))` for bandwidth
    /// `B` octaves (the same math the 64-band PEQ constructor uses).
    pub fn q(&self) -> f32 {
        let b = self.bandwidth_octaves() as f64;
        (1.0 / (2.0 * (2.0f64.ln() * b / 2.0).sinh())) as f32
    }

    /// The fixed center frequencies of the layout in Hz.
    pub fn frequencies(&self) -> Vec<f32> {
        match self {
            Self::TenBand => vec![
                31.5, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
            ],
            Self::FifteenBand => vec![
                25.0, 40.0, 63.0, 100.0, 160.0, 250.0, 400.0, 630.0, 1000.0, 1600.0, 2500.0,
                4000.0, 6300.0, 10000.0, 16000.0,
            ],
            Self::ThirtyOneBand => vec![
                20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0,
                400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0,
                5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0, 20000.0,
            ],
            Self::ThirtyTwoBand => vec![
                16.0, 20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0,
                315.0, 400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0,
                4000.0, 5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0, 20000.0,
            ],
            Self::SixtyFourBand => (0..64)
                .map(|i| 20.0f64 * (2.0f64).powf(i as f64 / 6.0))
                .map(|f| f as f32)
                .collect(),
            Self::Custom(freqs) => {
                let mut f: Vec<f32> = freqs
                    .iter()
                    .copied()
                    .filter(|f| f.is_finite() && *f > 0.0)
                    .collect();
                f.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                f.dedup();
                f.truncate(64);
                f
            }
        }
    }

    /// Number of bands in this layout.
    pub fn num_bands(&self) -> usize {
        self.frequencies().len()
    }
}

/// Graphic EQ configuration (§9.1) — a slider layer compiled into the same
/// parametric biquad engine the PEQ uses.
///
/// Precedence rule: when `enabled` is true, the graphic EQ model is the
/// authoritative source for the pipeline's EQ bands and preamp (the
/// `EqConfig` bands are ignored until the graphic EQ is disabled). When
/// disabled, the plain `EqConfig` drives the EQ.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphicEqConfig {
    pub enabled: bool,
    pub layout: GraphicEqLayout,
    /// Per-band slider gains in dB, indexed by the layout's band order.
    /// Entries beyond the current layout's band count are ignored.
    #[serde(default)]
    pub gains_db: Vec<f32>,
    pub preamp_db: f32,
}

impl Default for GraphicEqConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            layout: GraphicEqLayout::TenBand,
            gains_db: vec![0.0; 10],
            preamp_db: 0.0,
        }
    }
}

impl Default for EqConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            preamp_db: 0.0,
            post_gain_db: 0.0,
            headroom_db: 0.0,
            auto_headroom: false,
            bands: vec![
                EqBandConfig {
                    frequency: 31.0,
                    ..Default::default()
                },
                EqBandConfig {
                    frequency: 62.0,
                    ..Default::default()
                },
                EqBandConfig {
                    frequency: 125.0,
                    ..Default::default()
                },
                EqBandConfig {
                    frequency: 250.0,
                    ..Default::default()
                },
                EqBandConfig {
                    frequency: 500.0,
                    ..Default::default()
                },
                EqBandConfig {
                    frequency: 1000.0,
                    ..Default::default()
                },
                EqBandConfig {
                    frequency: 2000.0,
                    ..Default::default()
                },
                EqBandConfig {
                    frequency: 4000.0,
                    ..Default::default()
                },
                EqBandConfig {
                    frequency: 8000.0,
                    ..Default::default()
                },
                EqBandConfig {
                    frequency: 16000.0,
                    ..Default::default()
                },
            ],
            presets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoudnessConfig {
    pub mode: LoudnessMode,
    pub target_lufs: f32,
    pub true_peak_guard: bool,
    pub true_peak_dbtp: f32,
    /// Maximum positive gain (boost) in dB the normalizer may apply;
    /// `None` = unlimited. Clamps the computed ReplayGain/R128 gain so a very
    /// quiet track cannot be boosted into clipping or fatigue (spec §21
    /// "max boost").
    #[serde(default)]
    pub max_boost_db: Option<f32>,
    /// Maximum negative gain (attenuation) in dB the normalizer may apply;
    /// `None` = unlimited. Bounds how far a very loud track is pulled down
    /// (spec §21 "max attenuation").
    #[serde(default)]
    pub max_attenuation_db: Option<f32>,
}

impl Default for LoudnessConfig {
    fn default() -> Self {
        Self {
            mode: LoudnessMode::Off,
            target_lufs: -14.0,
            true_peak_guard: true,
            true_peak_dbtp: -1.0,
            max_boost_db: None,
            max_attenuation_db: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossfeedConfig {
    pub enabled: bool,
    pub profile: CrossfeedProfile,
    pub custom_freq: f32,
    pub custom_q: f32,
    pub custom_delay_ms: f32,
}

impl Default for CrossfeedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: CrossfeedProfile::Bauer,
            custom_freq: 700.0,
            custom_q: 0.707,
            custom_delay_ms: 0.3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StereoEnhancerConfig {
    pub enabled: bool,
    pub width: f32,
}

impl Default for StereoEnhancerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            width: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimiterConfig {
    pub enabled: bool,
    pub lookahead_ms: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub ceiling_db: f32,
    pub soft_clip: bool,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lookahead_ms: 5.0,
            attack_ms: 0.5,
            release_ms: 100.0,
            ceiling_db: -0.3,
            soft_clip: false,
        }
    }
}

fn default_knee_db() -> f32 {
    6.0
}
fn default_stereo_link() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BandConfig {
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_gain_db: f32,
    /// Soft-knee width in dB (0 = hard knee).
    #[serde(default = "default_knee_db")]
    pub knee_db: f32,
    /// Envelope detector mode.
    #[serde(default)]
    pub detector: CompressorDetector,
    /// Whether the band's left/right channels share a single gain
    /// (linked detection keeps the stereo image intact).
    #[serde(default = "default_stereo_link")]
    pub stereo_link: bool,
}

impl Default for BandConfig {
    /// Neutral, transparent defaults: ratio 1:1 means the compressor applies
    /// **no** gain reduction when enabled with defaults. Sonic character
    /// presets are a UI concern, not baked into the DSP layer.
    fn default() -> Self {
        Self {
            threshold_db: -6.0,
            ratio: 1.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            makeup_gain_db: 0.0,
            knee_db: default_knee_db(),
            detector: CompressorDetector::Peak,
            stereo_link: default_stereo_link(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MultibandCompressorConfig {
    pub enabled: bool,
    pub low_band: BandConfig,
    pub mid_band: BandConfig,
    pub high_band: BandConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ConvolutionConfig {
    pub enabled: bool,
    pub wet_mix: f32,
    pub ir_path: Option<String>,
}

// ── Multichannel management (§5 incremental) ─────────────────────────────────

/// Per-channel trim: gain, delay, and polarity for one output channel.
///
/// Applied in the multichannel passthrough path *before* the pre-mix chain,
/// on every channel. Entries are matched by output channel index; channels
/// without an entry pass through untrimmed. Stereo playback uses the
/// pipeline's balance control instead — these entries are intentionally not
/// applied to the front L/R pair of a downmixed stereo stream.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ChannelTrimEntry {
    /// 0-based output channel index this entry applies to.
    pub channel: usize,
    /// Gain in dB (0.0 = unity).
    #[serde(default)]
    pub gain_db: f32,
    /// Delay in milliseconds (0.0 = none), clamped to `MAX_CHANNEL_DELAY_MS`.
    #[serde(default)]
    pub delay_ms: f32,
    /// Invert polarity (flip sign).
    #[serde(default)]
    pub invert: bool,
}

impl Default for ChannelTrimEntry {
    fn default() -> Self {
        Self {
            channel: 0,
            gain_db: 0.0,
            delay_ms: 0.0,
            invert: false,
        }
    }
}

/// Channel trim layer: per-channel gain / delay / polarity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChannelTrimConfig {
    pub enabled: bool,
    #[serde(default)]
    pub entries: Vec<ChannelTrimEntry>,
}

/// Source→destination routing matrix for the multichannel passthrough path.
///
/// `matrix[src][dst]` is the gain from input channel `src` to output channel
/// `dst`. The matrix must be square and its width must equal the active
/// channel count to be applied; any other shape is rejected (with a warning)
/// and routing is bypassed rather than guessing. All channels stay in place
/// when `enabled` is false.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChannelRoutingConfig {
    pub enabled: bool,
    #[serde(default)]
    pub matrix: Vec<Vec<f32>>,
}

/// LFE management: an extra gain applied to channels whose semantic role is
/// [`crate::decode::ChannelId::Lfe`] (derived from the active channel layout),
/// plus an optional LFE low-pass filter (bass management, spec §17/§34).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LfeConfig {
    pub enabled: bool,
    /// Gain in dB applied to LFE-role channels (0.0 = unity).
    #[serde(default)]
    pub gain_db: f32,
    /// LFE low-pass crossover frequency in Hz. When `Some`, a second-order
    /// minimum-phase low-pass is applied to LFE-role channels at this
    /// frequency (the standard LFE 120 Hz roll-off by default); `None` keeps
    /// the LFE channel full-band (gain only).
    #[serde(default)]
    pub crossover_hz: Option<f32>,
}

impl Default for LfeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gain_db: 0.0,
            crossover_hz: None,
        }
    }
}

/// One multichannel parametric-EQ entry. The entry owns a filter cascade for
/// one semantic/output channel; channels not listed remain unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelEqEntry {
    /// 0-based channel index in the active output layout.
    pub channel: usize,
    /// Biquad bands compiled by the same RBJ filter implementation as stereo EQ.
    #[serde(default)]
    pub bands: Vec<EqBandConfig>,
}

/// Per-channel EQ for intentional multichannel processing (§17).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChannelEqConfig {
    pub enabled: bool,
    #[serde(default)]
    pub entries: Vec<ChannelEqEntry>,
}

/// Bass-management crossover for the main speakers.
///
/// When enabled, every non-LFE channel receives the configured second-order
/// high-pass. The existing [`LfeConfig::crossover_hz`] independently controls
/// the LFE low-pass, so either half of a bass-management setup can be used
/// deliberately rather than silently inserting a filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BassManagementConfig {
    pub enabled: bool,
    /// Enable the mains high-pass section.
    #[serde(default = "default_true")]
    pub mains_highpass_enabled: bool,
    /// Crossover frequency shared by the mains high-pass and, when configured,
    /// the LFE low-pass. Validated/clamped by the DSP stage.
    #[serde(default = "default_bass_crossover_hz")]
    pub crossover_hz: f32,
    /// Filter Q; 1/sqrt(2) is the Butterworth default.
    #[serde(default = "default_bass_q")]
    pub q: f32,
}

fn default_true() -> bool {
    true
}

fn default_bass_crossover_hz() -> f32 {
    80.0
}

fn default_bass_q() -> f32 {
    std::f32::consts::FRAC_1_SQRT_2
}

impl Default for BassManagementConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mains_highpass_enabled: true,
            crossover_hz: default_bass_crossover_hz(),
            q: default_bass_q(),
        }
    }
}

/// Standard templates for converting between stereo and common desktop
/// speaker layouts. `Custom` uses `[source][destination]` gains and is
/// validated against the active channel counts before use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum ChannelMixTemplate {
    /// Semantic identity for equal-width layouts; ITU-R BS.775 downmix to
    /// stereo when the target is stereo.
    #[default]
    ItuBs775,
    /// Stereo → 5.1: center/surround fill at conservative, deterministic gain;
    /// LFE is silent because bass extraction is a separate DSP policy.
    StereoToFiveOne,
    /// Stereo → 7.1 with side and rear fill.
    StereoToSevenOne,
    /// Stereo → 7.1.4 with front/rear overhead fill.
    StereoToSevenPointOneFour,
    /// Explicit named downmix templates (LFE is excluded from the fold).
    FiveOneToStereo,
    SevenOneToStereo,
    SevenPointOneFourToStereo,
    /// Role/index-independent custom matrix, stored as `[source][destination]`.
    Custom(Vec<Vec<f32>>),
}

/// Configuration for explicit upmix/downmix at the decode/output boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelMixConfig {
    pub enabled: bool,
    pub template: ChannelMixTemplate,
}

// ── Room & headphone correction (Phase 7 S5) ────────────────────────────────

/// Phase-rendering mode for a correction IR (Phase 7 S5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CorrectionPhaseMode {
    /// Cepstral minimum phase: zero added latency, phase-dispersive.
    #[default]
    Minimum,
    /// Symmetric FIR: constant group delay `n/2`, phase-flat magnitude.
    Linear,
    /// Minimum phase below the crossover, linear phase above (rendered as
    /// the minimum-phase IR delayed by two crossover cycles).
    Hybrid,
}

/// Target response the correction is derived against (Phase 7 S5).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum CorrectionTarget {
    /// Flat magnitude (0 dB at every frequency).
    #[default]
    Flat,
    /// Linear tilt in log-frequency: `db_per_octave · log2(f / 1 kHz)`.
    Tilt {
        /// Slope in dB per octave.
        db_per_octave: f32,
    },
    /// Smooth shelf between two plateau gains around `corner_hz`, swept
    /// over `slope_octaves` with a raised cosine.
    Shelf {
        /// Center of the transition (Hz).
        corner_hz: f32,
        /// Plateau gain below the transition (dB).
        low_gain_db: f32,
        /// Plateau gain above the transition (dB).
        high_gain_db: f32,
        /// Total transition width (octaves).
        slope_octaves: f32,
    },
}

/// Room & headphone correction configuration (Phase 7 S5).
///
/// The node is a per-channel partitioned-convolution bank placed
/// post-aux / pre-EQ. `ir_paths` are the **measured** IR files (one path
/// per channel, or a single multichannel WAV); the full S2→S4 chain
/// (conditioning → smoothing → SNR-weighted regularized inverse → phase
/// render) runs on the control path at config-apply time, so a host that
/// configures correction at construction or config-load gets the derived
/// IRs wired without a separate load command. A missing/unreadable IR
/// leaves the node inactive — bit-exact passthrough.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorrectionConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Measured IR files used to derive the correction (see the struct
    /// docs). One path per channel; a single multichannel WAV also works
    /// (its channels are the per-channel IRs). Empty = no correction.
    #[serde(default)]
    pub ir_paths: Vec<String>,
    #[serde(default)]
    pub phase_mode: CorrectionPhaseMode,
    /// Hybrid crossover (Hz); only used when `phase_mode` is hybrid.
    #[serde(default = "default_hybrid_crossover_hz")]
    pub hybrid_crossover_hz: f32,
    #[serde(default)]
    pub target: CorrectionTarget,
    /// Hard clamp on any correction boost (dB); cuts are not clamped.
    #[serde(default = "default_max_boost_db")]
    pub max_boost_db: f32,
    /// Octave-fraction smoothing applied to the measured magnitude before
    /// inversion (power-domain, log-frequency).
    #[serde(default = "default_smoothing_octaves")]
    pub smoothing_octaves: f32,
    /// Wet/dry depth in [0, 1] (1.0 = fully corrected).
    #[serde(default = "default_correction_depth")]
    pub depth: f32,
}

fn default_correction_depth() -> f32 {
    1.0
}
fn default_hybrid_crossover_hz() -> f32 {
    1000.0
}
fn default_max_boost_db() -> f32 {
    6.0
}
fn default_smoothing_octaves() -> f32 {
    1.0 / 6.0
}

impl Default for CorrectionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ir_paths: Vec::new(),
            phase_mode: CorrectionPhaseMode::default(),
            hybrid_crossover_hz: default_hybrid_crossover_hz(),
            target: CorrectionTarget::default(),
            max_boost_db: default_max_boost_db(),
            smoothing_octaves: default_smoothing_octaves(),
            depth: 1.0,
        }
    }
}

impl Default for ChannelMixConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            template: ChannelMixTemplate::ItuBs775,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoeq_preset_parses_frequency_gain_and_q() {
        let text = "\
Preamp: -4.5 dB
Filter 1: ON PK Fc 100 Hz Gain 6.0 dB Q 0.5
Filter 2: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.70
Filter 3: ON HSC Fc 10000 Hz Gain -3.0 dB Q 0.71
";
        let preset = EqPreset::from_autoeq("test", text);

        assert_eq!(preset.bands.len(), 3);

        assert_eq!(preset.bands[0].filter_type, FilterType::Peaking);
        assert_eq!(preset.bands[0].frequency, 100.0);
        assert_eq!(preset.bands[0].gain_db, 6.0);
        assert_eq!(preset.bands[0].q, 0.5);

        assert_eq!(preset.bands[1].filter_type, FilterType::LowShelf);
        assert_eq!(preset.bands[1].frequency, 105.0);
        assert_eq!(preset.bands[1].gain_db, 5.5);
        assert_eq!(preset.bands[1].q, 0.70);

        assert_eq!(preset.bands[2].filter_type, FilterType::HighShelf);
        assert_eq!(preset.bands[2].frequency, 10_000.0);
        assert_eq!(preset.bands[2].gain_db, -3.0);
        assert_eq!(preset.bands[2].q, 0.71);
    }

    #[test]
    fn autoeq_preset_ignores_preamp_and_malformed_lines() {
        let text = "\
Preamp: -4.5 dB
Filter 1: ON PK Fc 100 Hz Gain 6.0 dB Q 0.5
Filter 2: OFF PK Fc 200 Hz Gain 1.0 dB Q 1.0
not a filter line
";
        let preset = EqPreset::from_autoeq("test", text);

        // Only the ON filter line is imported; the OFF line and junk are skipped.
        assert_eq!(preset.bands.len(), 1);
        assert_eq!(preset.bands[0].frequency, 100.0);
        assert_eq!(preset.bands[0].gain_db, 6.0);
    }
}
