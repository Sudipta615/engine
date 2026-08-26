/// Loudness normalisation mode
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LoudnessMode {
    #[default]
    Off,
    TrackReplayGain,
    AlbumReplayGain,
    EbuR128,
}

/// Loudness metadata for a track (pre-computed during scanning)
#[derive(Debug, Clone, Copy, Default)]
pub struct LoudnessMetadata {
    /// ReplayGain track gain in dB
    pub replaygain_track_db: Option<f32>,
    /// ReplayGain album gain in dB
    pub replaygain_album_db: Option<f32>,
    /// ReplayGain track peak (linear)
    pub replaygain_track_peak: Option<f32>,
    /// ReplayGain album peak (linear)
    pub replaygain_album_peak: Option<f32>,
    /// EBU R128 integrated loudness in LUFS
    pub ebu_r128_loudness: Option<f32>,
    /// EBU R128 true peak in dBTP
    pub ebu_r128_peak: Option<f32>,
}

/// Absolute gate threshold per EBU R128: −70 LUFS.
pub(crate) const ABSOLUTE_GATE_LUFS: f32 = -70.0;

/// Relative gate offset: −10 LU below ungated mean.
pub(crate) const RELATIVE_GATE_OFFSET_LU: f32 = -10.0;

/// Momentary block duration: 400 ms.
pub(crate) const MOMENTARY_BLOCK_SECS: f32 = 0.400;

/// Momentary block hop: 75% overlap = 100 ms interval.
pub(crate) const MOMENTARY_HOP_SECS: f32 = 0.100;

/// Short-term window duration: 3 s.
pub(crate) const SHORT_TERM_WINDOW_SECS: f32 = 3.0;

/// Output of a single `LoudnessMeter::snapshot()` call.
#[derive(Debug, Clone, Default)]
pub struct LoudnessMeasurement {
    /// Momentary LUFS (400 ms block ending now).
    pub momentary_lufs: f32,
    /// Short-term LUFS (3 s window ending now).
    pub short_term_lufs: f32,
    /// Integrated LUFS since last `reset()` (gated per BS.1770-4).
    pub integrated_lufs: f32,
    /// Loudness Range in LU (10th–95th percentile of gated short-term blocks).
    ///
    /// **Check `lra_valid` before using this value.** When `lra_valid` is `false`,
    /// `lra_lu` is 0.0 (undefined) because not enough short-term blocks have
    /// accumulated yet (typically requires ~6 s of above-gate audio).
    pub lra_lu: f32,
    /// Whether `lra_lu` was computed from genuine multi-window short-term data.
    ///
    /// `false` when:
    /// - The track is shorter than ~6 s, OR
    /// - The gated short-term history has < 2 blocks, OR
    /// - The signal has been below the absolute gate (−70 LUFS) throughout.
    ///
    /// The EBU R128 standard defines LRA only for programme material with
    /// sufficient duration; returning a fabricated value for short tracks
    /// would be misleading.
    pub lra_valid: bool,
    /// Instantaneous true-peak estimate (linear, not in dBTP yet).
    pub true_peak_linear: f32,
}

impl LoudnessMeasurement {
    /// True peak in dBTP.
    pub fn true_peak_dbtp(&self) -> f32 {
        if self.true_peak_linear > 0.0 {
            20.0 * self.true_peak_linear.log10()
        } else {
            f32::NEG_INFINITY
        }
    }
}
