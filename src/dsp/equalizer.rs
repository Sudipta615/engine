//! Parametric equaliser — configurable multi-band EQ with smooth parameter transitions

use crate::{
    buffer::AudioFrame,
    dsp::biquad::{FilterType, SmoothedBiquad},
};

/// Maximum number of EQ bands (supports up to 64 bands for full AutoEQ and 1/6 octave curves)
pub const MAX_EQ_BANDS: usize = 64;

/// Number of default EQ bands
pub const NUM_EQ_BANDS: usize = 10;

/// EQ filter type for each band
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum EqFilterType {
    #[default]
    Peaking,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    Bandpass,
    Notch,
    AllPass,
}

impl EqFilterType {
    /// Map to the biquad FilterType
    pub fn to_filter_type(&self) -> FilterType {
        match self {
            Self::Peaking => FilterType::Peaking,
            Self::LowShelf => FilterType::Lowshelf,
            Self::HighShelf => FilterType::Highshelf,
            Self::LowPass => FilterType::Lowpass,
            Self::HighPass => FilterType::Highpass,
            Self::Bandpass => FilterType::Bandpass,
            Self::Notch => FilterType::Notch,
            Self::AllPass => FilterType::Allpass,
        }
    }
}

/// Parameters for a single EQ band
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqBandParams {
    /// Centre/cutoff frequency in Hz
    pub frequency: f32,
    /// Gain in dB (for shelving/peaking)
    pub gain_db: f32,
    /// Quality factor (bandwidth)
    pub q: f32,
    /// Filter type
    pub filter_type: EqFilterType,
    /// Whether this band is enabled
    pub enabled: bool,
}

impl Default for EqBandParams {
    fn default() -> Self {
        Self {
            frequency: 1000.0,
            gain_db: 0.0,
            q: 1.0,
            filter_type: EqFilterType::Peaking,
            enabled: false,
        }
    }
}

impl EqBandParams {
    /// Create a peaking EQ band
    pub fn peaking(frequency: f32, gain_db: f32, q: f32) -> Self {
        Self {
            frequency,
            gain_db,
            q,
            filter_type: EqFilterType::Peaking,
            enabled: true,
        }
    }

    /// Create a low-shelf band
    pub fn lowshelf(frequency: f32, gain_db: f32, q: f32) -> Self {
        Self {
            frequency,
            gain_db,
            q,
            filter_type: EqFilterType::LowShelf,
            enabled: true,
        }
    }

    /// Create a high-shelf band
    pub fn highshelf(frequency: f32, gain_db: f32, q: f32) -> Self {
        Self {
            frequency,
            gain_db,
            q,
            filter_type: EqFilterType::HighShelf,
            enabled: true,
        }
    }
}

/// A single EQ band using a smoothed biquad filter (stereo pair)
#[derive(Debug, Clone)]
struct EqBand {
    params: EqBandParams,
    filter_left: SmoothedBiquad<f64>,
    filter_right: SmoothedBiquad<f64>,
}

impl EqBand {
    fn new() -> Self {
        Self {
            params: EqBandParams::default(),
            filter_left: SmoothedBiquad::new(),
            filter_right: SmoothedBiquad::new(),
        }
    }

    fn update_coefficients(&mut self, sample_rate: f32) {
        self.filter_left.set_sample_rate(sample_rate);
        self.filter_right.set_sample_rate(sample_rate);
        if !self.params.enabled {
            self.filter_left.set_target_params(
                self.params.frequency,
                0.0,
                self.params.q,
                self.params.filter_type.to_filter_type(),
            );
            self.filter_right.set_target_params(
                self.params.frequency,
                0.0,
                self.params.q,
                self.params.filter_type.to_filter_type(),
            );
            return;
        }
        self.filter_left.set_target_params(
            self.params.frequency,
            self.params.gain_db,
            self.params.q,
            self.params.filter_type.to_filter_type(),
        );
        self.filter_right.set_target_params(
            self.params.frequency,
            self.params.gain_db,
            self.params.q,
            self.params.filter_type.to_filter_type(),
        );
    }

    /// Process a stereo sample pair in native f64 precision.
    ///
    /// This is the single band cascade step; `ParametricEq` routes both the
    /// f32 and f64 paths through here so a band never converts precision on
    /// its own (see the struct-level precision note).
    #[inline]
    fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if !self.params.enabled && self.filter_left.is_settled() && self.filter_right.is_settled() {
            return (left, right);
        }
        let out_l = self.filter_left.process_sample(0, left);
        let out_r = self.filter_right.process_sample(1, right);
        self.filter_left.advance_smoothing();
        self.filter_right.advance_smoothing();
        (out_l, out_r)
    }

    /// Process a block of stereo frames in place in native f64 precision.
    #[inline]
    fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if !self.params.enabled && self.filter_left.is_settled() && self.filter_right.is_settled() {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i], right[i]);
            left[i] = ol;
            right[i] = or_;
        }
    }

    fn reset(&mut self) {
        self.filter_left.reset();
        self.filter_right.reset();
    }
}

/// Parametric EQ processor — configurable multi-band equaliser
///
/// # Precision model
///
/// The filter cores are native `f64` biquads ([`SmoothedBiquad<f64>`]), so
/// the **entire cascade runs in f64** regardless of the pipeline precision
/// mode. The f32 (Performance) entry points promote the signal to f64 **once**
/// at the EQ boundary, run every band and the tone shelves in f64, and demote
/// **once** at the exit — there are no per-band f32↔f64 round-trips, which
/// both wastes cycles and loses the benefit of a high-precision cascade.
///
/// # Headroom model
///
/// Headroom is a **static pre-EQ attenuation** (default 0 dB = unity).  It
/// intentionally does *not* behave like a compressor: the gain is fixed at
/// configuration time and applied before the filter chain, so it cannot
/// react to transients and cannot "pump".  The intended use is to reserve
/// headroom for the EQ curve's own boost — e.g. combine `set_headroom_db`
/// with [`ParametricEq::combined_max_gain_db`] so a +6 dB EQ curve still
/// leaves the limiter headroom to work with.
///
/// (Historical note: a *dynamic* headroom stage with a peak detector and
/// attack/release once lived inside this struct.  It made the EQ a second
/// compressor in the chain — duplicating the downstream `LookaheadLimiter`
/// with a non-lookahead detector that reacted after transients had passed —
/// and it was removed.  Overshoot protection is the limiter's job; the EQ
/// only ever applies linear, time-invariant gain.)
#[derive(Debug, Clone)]
pub struct ParametricEq {
    bands: Vec<EqBand>,
    bass_band: EqBand,
    treble_band: EqBand,
    sample_rate: f32,
    enabled: bool,
    preamp_db: f32,
    preamp_linear: f32,
    post_gain_db: f32,
    /// Cached linear gain derived from `post_gain_db` — avoids a per-sample
    /// `powf` call in the hot path.  Updated by `set_post_gain_db()`.
    post_gain_linear: f32,
    /// Static pre-EQ headroom in dB (≤ 0). Applied once per sample as a
    /// fixed gain before the filter chain — see struct doc.
    headroom_db: f32,
    /// Cached linear equivalent of `headroom_db`.
    headroom_linear: f32,
    /// When true, the static pre-EQ headroom is recomputed automatically from
    /// the curve's peak combined boost whenever the curve changes.
    auto_headroom: bool,
    /// The last manually configured headroom, restored when auto headroom is
    /// disabled. Kept separate so automatic updates never overwrite it.
    manual_headroom_db: f32,
}

impl ParametricEq {
    /// Create a new 10-band parametric EQ with standard ISO frequencies
    pub fn default_10_band(sample_rate: f32) -> Self {
        let freqs = [
            31.25, 62.5, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
        ];
        let types = [
            EqFilterType::LowShelf,
            EqFilterType::Peaking,
            EqFilterType::Peaking,
            EqFilterType::Peaking,
            EqFilterType::Peaking,
            EqFilterType::Peaking,
            EqFilterType::Peaking,
            EqFilterType::Peaking,
            EqFilterType::Peaking,
            EqFilterType::HighShelf,
        ];

        let bands = freqs
            .iter()
            .zip(types.iter())
            .map(|(&freq, &ft)| EqBand {
                params: EqBandParams {
                    filter_type: ft,
                    frequency: freq,
                    gain_db: 0.0,
                    q: 1.4,
                    enabled: false,
                },
                filter_left: SmoothedBiquad::new(),
                filter_right: SmoothedBiquad::new(),
            })
            .collect();

        let bass_band = EqBand {
            params: EqBandParams::lowshelf(100.0, 0.0, 1.0),
            filter_left: SmoothedBiquad::new(),
            filter_right: SmoothedBiquad::new(),
        };
        let treble_band = EqBand {
            params: EqBandParams::highshelf(7500.0, 0.0, 0.70),
            filter_left: SmoothedBiquad::new(),
            filter_right: SmoothedBiquad::new(),
        };

        Self {
            bands,
            bass_band,
            treble_band,
            sample_rate,
            enabled: false,
            preamp_db: 0.0,
            preamp_linear: 1.0,
            post_gain_db: 0.0,
            post_gain_linear: 1.0,
            headroom_db: 0.0,
            headroom_linear: 1.0,
            auto_headroom: false,
            manual_headroom_db: 0.0,
        }
    }

    /// Create a standard 31-band graphic/parametric EQ (1/3 octave ISO frequencies from 20 Hz to 20 kHz).
    pub fn standard_31_band(sample_rate: f32) -> Self {
        let freqs = [
            20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0,
            400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0,
            5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0, 20000.0,
        ];
        let bands = freqs
            .iter()
            .enumerate()
            .map(|(i, &freq)| {
                let ft = if i == 0 {
                    EqFilterType::LowShelf
                } else if i == freqs.len() - 1 {
                    EqFilterType::HighShelf
                } else {
                    EqFilterType::Peaking
                };
                EqBand {
                    params: EqBandParams {
                        filter_type: ft,
                        frequency: freq,
                        gain_db: 0.0,
                        q: 4.31, // Standard 1/3 octave Q
                        enabled: false,
                    },
                    filter_left: SmoothedBiquad::new(),
                    filter_right: SmoothedBiquad::new(),
                }
            })
            .collect();

        let bass_band = EqBand {
            params: EqBandParams::lowshelf(100.0, 0.0, 1.0),
            filter_left: SmoothedBiquad::new(),
            filter_right: SmoothedBiquad::new(),
        };
        let treble_band = EqBand {
            params: EqBandParams::highshelf(7500.0, 0.0, 0.70),
            filter_left: SmoothedBiquad::new(),
            filter_right: SmoothedBiquad::new(),
        };

        Self {
            bands,
            bass_band,
            treble_band,
            sample_rate,
            enabled: false,
            preamp_db: 0.0,
            preamp_linear: 1.0,
            post_gain_db: 0.0,
            post_gain_linear: 1.0,
            headroom_db: 0.0,
            headroom_linear: 1.0,
            auto_headroom: false,
            manual_headroom_db: 0.0,
        }
    }

    /// Create a high-resolution 64-band parametric EQ with true 1/6-octave
    /// spacing.
    ///
    /// Band `i` sits at `f_i = 20 Hz × 2^(i/6)` — an exact 1/6-octave ladder
    /// starting at 20 Hz, not an arbitrary log-spacing that merely *looks*
    /// like one.  The last band (i = 63) therefore lands at
    /// 20 × 2^10.5 ≈ 28 963 Hz, which is usable at sample rates ≥ ~58 kHz
    /// and harmless (disabled, unity gain) below that.
    ///
    /// The default Q is **derived from the bandwidth**, not invented:
    /// for a peaking filter with bandwidth `B` octaves,
    /// `Q = 1 / (2·sinh(ln(2)·B/2))`; for B = 1/6 octave that is ≈ 8.65.
    /// Each band remains individually configurable through `EqBandParams`
    /// (frequency / Q / gain / filter type) — the generator only supplies
    /// the mathematically-consistent default.
    pub fn standard_64_band(sample_rate: f32) -> Self {
        const BANDS: usize = 64;
        const OCTAVES_PER_BAND: f64 = 1.0 / 6.0;
        let f_min = 20.0f64;
        let q = 1.0 / (2.0 * (2.0f64.ln() * OCTAVES_PER_BAND / 2.0).sinh());
        let mut bands = Vec::with_capacity(BANDS);
        for i in 0..BANDS {
            let freq = f_min * (2.0f64).powf(i as f64 * OCTAVES_PER_BAND);
            let ft = if i == 0 {
                EqFilterType::LowShelf
            } else if i == BANDS - 1 {
                EqFilterType::HighShelf
            } else {
                EqFilterType::Peaking
            };
            bands.push(EqBand {
                params: EqBandParams {
                    filter_type: ft,
                    frequency: freq as f32,
                    gain_db: 0.0,
                    q: q as f32,
                    enabled: false,
                },
                filter_left: SmoothedBiquad::new(),
                filter_right: SmoothedBiquad::new(),
            });
        }

        let bass_band = EqBand {
            params: EqBandParams::lowshelf(100.0, 0.0, 1.0),
            filter_left: SmoothedBiquad::new(),
            filter_right: SmoothedBiquad::new(),
        };
        let treble_band = EqBand {
            params: EqBandParams::highshelf(7500.0, 0.0, 0.70),
            filter_left: SmoothedBiquad::new(),
            filter_right: SmoothedBiquad::new(),
        };

        Self {
            bands,
            bass_band,
            treble_band,
            sample_rate,
            enabled: false,
            preamp_db: 0.0,
            preamp_linear: 1.0,
            post_gain_db: 0.0,
            post_gain_linear: 1.0,
            headroom_db: 0.0,
            headroom_linear: 1.0,
            auto_headroom: false,
            manual_headroom_db: 0.0,
        }
    }

    /// Build a ParametricEq from a configured [`config::EqPreset`].
    pub fn from_preset(sample_rate: f32, preset: &config::EqPreset) -> Self {
        let num_bands = preset.bands.len().max(10).min(MAX_EQ_BANDS);
        let mut eq = Self::new(num_bands, sample_rate);
        eq.set_preamp_db(preset.preamp_db);
        for (i, b) in preset.bands.iter().enumerate().take(MAX_EQ_BANDS) {
            let ft = match b.filter_type {
                config::FilterType::Peaking => EqFilterType::Peaking,
                config::FilterType::LowShelf => EqFilterType::LowShelf,
                config::FilterType::HighShelf => EqFilterType::HighShelf,
                config::FilterType::LowPass => EqFilterType::LowPass,
                config::FilterType::HighPass => EqFilterType::HighPass,
                config::FilterType::Bandpass => EqFilterType::Bandpass,
                config::FilterType::Notch => EqFilterType::Notch,
                config::FilterType::AllPass => EqFilterType::AllPass,
            };
            eq.set_band(
                i,
                EqBandParams {
                    frequency: b.frequency,
                    gain_db: b.gain_db,
                    q: b.q,
                    filter_type: ft,
                    enabled: b.enabled,
                },
            );
        }
        eq.set_enabled(true);
        eq
    }

    /// Create a new EQ with all bands disabled
    pub fn new(num_bands: usize, sample_rate: f32) -> Self {
        let clamped_bands = num_bands.min(MAX_EQ_BANDS);
        let bands = (0..clamped_bands).map(|_| EqBand::new()).collect();
        let mut bass_band = EqBand::new();
        bass_band.params = EqBandParams::lowshelf(100.0, 0.0, 1.0);
        let mut treble_band = EqBand::new();
        treble_band.params = EqBandParams::highshelf(7500.0, 0.0, 0.70);

        Self {
            bands,
            bass_band,
            treble_band,
            sample_rate,
            enabled: false,
            preamp_db: 0.0,
            preamp_linear: 1.0,
            post_gain_db: 0.0,
            post_gain_linear: 1.0,
            headroom_db: 0.0,
            headroom_linear: 1.0,
            auto_headroom: false,
            manual_headroom_db: 0.0,
        }
    }

    /// Enable or disable the EQ
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.refresh_auto_headroom();
    }

    /// Whether EQ is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Current preamp gain in dB.
    pub fn preamp_db(&self) -> f32 {
        self.preamp_db
    }

    /// Set preamp gain in dB (applied before EQ).
    pub fn set_preamp_db(&mut self, gain_db: f32) {
        if !gain_db.is_finite() {
            log::warn!(
                "ParametricEq::set_preamp_db: non-finite value {}; ignoring",
                gain_db
            );
            return;
        }
        let clamped = gain_db.clamp(-30.0, 30.0);
        self.preamp_db = clamped;
        self.preamp_linear = 10.0_f32.powf(clamped / 20.0);
    }

    /// Set post-EQ gain in dB
    pub fn set_post_gain_db(&mut self, gain_db: f32) {
        if !gain_db.is_finite() {
            log::warn!(
                "ParametricEq::set_post_gain_db: non-finite value {}; ignoring",
                gain_db
            );
            return;
        }
        let clamped = gain_db.clamp(-30.0, 30.0);
        self.post_gain_db = clamped;
        // Cache the linear equivalent so process() doesn't call powf() per sample.
        self.post_gain_linear = 10.0_f32.powf(clamped / 20.0);
    }

    /// Current static pre-EQ headroom in dB (≤ 0; 0 = unity).
    pub fn headroom_db(&self) -> f32 {
        self.headroom_db
    }

    /// Set the static pre-EQ headroom in dB (range [-60, 0]).
    ///
    /// This is a fixed attenuation applied *before* the filter chain — it is
    /// deliberately NOT a dynamic gain-reduction stage (see the struct doc).
    /// The default is 0 dB (unity).
    pub fn set_headroom_db(&mut self, headroom_db: f32) {
        if !headroom_db.is_finite() {
            log::warn!(
                "ParametricEq::set_headroom_db: non-finite value {}; ignoring",
                headroom_db
            );
            return;
        }
        let clamped = headroom_db.clamp(-60.0, 0.0);
        self.headroom_db = clamped;
        self.headroom_linear = 10.0_f32.powf(clamped / 20.0);
        // Track the manual value only when auto headroom is not the one
        // driving the knob; auto updates must not clobber it.
        if !self.auto_headroom {
            self.manual_headroom_db = clamped;
        }
    }

    /// Enable or disable automatic headroom management.
    ///
    /// When enabled, the static pre-EQ headroom is recomputed from the curve's
    /// own peak combined boost ([`Self::combined_max_gain_db`]) after every
    /// band, tone shelf, enable, or sample-rate change, so the downstream
    /// limiter never has to absorb the EQ's boost. Disabling restores the
    /// last manually configured headroom.
    pub fn set_auto_headroom(&mut self, enabled: bool) {
        if self.auto_headroom == enabled {
            return;
        }
        self.auto_headroom = enabled;
        if enabled {
            self.refresh_auto_headroom();
        } else {
            self.headroom_db = self.manual_headroom_db;
            self.headroom_linear = 10.0_f32.powf(self.manual_headroom_db / 20.0);
        }
    }

    /// Whether automatic headroom management is enabled.
    pub fn is_auto_headroom(&self) -> bool {
        self.auto_headroom
    }

    /// Recompute the static pre-EQ headroom from the current curve when
    /// automatic headroom is enabled; a no-op otherwise (preserving any
    /// manually configured headroom).
    fn refresh_auto_headroom(&mut self) {
        if !self.auto_headroom {
            return;
        }
        let boost = self.combined_max_gain_db(self.sample_rate);
        let headroom = if boost > 0.0 { -boost } else { 0.0 };
        self.set_headroom_db(headroom);
    }

    /// Process a stereo sample pair through the full EQ chain (f32).
    ///
    /// The signal is promoted to f64 once, cascaded through every band in
    /// f64, and demoted once — see the struct-level precision note. This is
    /// numerically identical to [`Self::process_f64`] up to the final cast.
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.enabled {
            return (left, right);
        }
        let (l, r) = self.process_f64(left as f64, right as f64);
        (l as f32, r as f32)
    }

    /// Process a stereo sample pair in native f64 precision (Quality mode).
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if !self.enabled {
            return (left, right);
        }

        let preamp_linear = self.preamp_linear as f64;
        let headroom_linear = self.headroom_linear as f64;
        let in_gain = preamp_linear * headroom_linear;
        let mut l = left * in_gain;
        let mut r = right * in_gain;

        for band in &mut self.bands {
            (l, r) = band.process_f64(l, r);
        }

        (l, r) = self.bass_band.process_f64(l, r);
        (l, r) = self.treble_band.process_f64(l, r);

        let post_linear = self.post_gain_linear as f64;
        (l * post_linear, r * post_linear)
    }

    /// Process a block of stereo frames through the full EQ chain in place.
    /// Hoists the whole-EQ enabled check; each frame is promoted to f64 once,
    /// cascaded in f64, and demoted once (see the struct-level precision note).
    #[inline]
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if !self.enabled {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i] as f64, right[i] as f64);
            left[i] = ol as f32;
            right[i] = or_ as f32;
        }
    }

    /// Process a block of stereo frames through the full EQ chain in native
    /// f64 precision. Hoists the whole-EQ enabled check out of the loop.
    #[inline]
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if !self.enabled {
            return;
        }
        let n = left.len().min(right.len());
        let preamp = self.preamp_linear as f64 * self.headroom_linear as f64;
        let post = self.post_gain_linear as f64;
        for i in 0..n {
            left[i] *= preamp;
            right[i] *= preamp;
        }
        for band in &mut self.bands {
            band.process_block_f64(left, right);
        }
        self.bass_band.process_block_f64(left, right);
        self.treble_band.process_block_f64(left, right);
        for i in 0..n {
            left[i] *= post;
            right[i] *= post;
        }
    }

    /// Collect the coefficients of every active band (parametric bands plus
    /// the bass/treble tone shelves) at the given sample rate.
    fn active_band_coeffs(&self, sr: f32) -> Vec<crate::dsp::biquad::BiquadCoeffs<f64>> {
        let mut coeffs: Vec<crate::dsp::biquad::BiquadCoeffs<f64>> = Vec::new();
        for band in &self.bands {
            if band.params.enabled && band.params.gain_db.abs() > 0.001 {
                coeffs.push(band.params.filter_type.to_filter_type().compute_coeffs(
                    sr,
                    band.params.frequency,
                    band.params.gain_db,
                    band.params.q,
                ));
            }
        }
        if self.bass_band.params.enabled && self.bass_band.params.gain_db.abs() > 0.001 {
            coeffs.push(
                self.bass_band
                    .params
                    .filter_type
                    .to_filter_type()
                    .compute_coeffs(
                        sr,
                        self.bass_band.params.frequency,
                        self.bass_band.params.gain_db,
                        self.bass_band.params.q,
                    ),
            );
        }
        if self.treble_band.params.enabled && self.treble_band.params.gain_db.abs() > 0.001 {
            coeffs.push(
                self.treble_band
                    .params
                    .filter_type
                    .to_filter_type()
                    .compute_coeffs(
                        sr,
                        self.treble_band.params.frequency,
                        self.treble_band.params.gain_db,
                        self.treble_band.params.q,
                    ),
            );
        }
        coeffs
    }

    /// Compute the maximum combined magnitude response (in dB) of all cascaded EQ bands
    /// across the audible spectrum (20 Hz to 20 kHz) at the given sample rate.
    ///
    /// The estimate is deliberately conservative: DC and Nyquist are evaluated
    /// analytically (where shelving filters realise their full boost), and the
    /// interior is covered by a dense logarithmic sweep with local
    /// golden-section refinement so narrow, high-Q peaks are not missed.
    pub fn combined_max_gain_db(&self, sample_rate: f32) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            self.sample_rate
        };
        let min_f = 20.0_f64;
        let max_f = (sr as f64 * 0.49).min(20000.0);
        if max_f <= min_f {
            return 0.0;
        }

        let active_coeffs = self.active_band_coeffs(sr);
        if active_coeffs.is_empty() {
            return 0.0;
        }

        // Shelves reach their extremum at the band edges, which the sweep
        // endpoints do not include. Compute those directly from the transfer
        // function: |H(1)| = |(b0+b1+b2)/(1+a1+a2)| at DC and
        // |H(-1)| = |(b0-b1+b2)/(1-a1+a2)| at Nyquist.
        let mut dc_mag = 1.0_f64;
        let mut nyq_mag = 1.0_f64;
        for coeff in &active_coeffs {
            let den_dc = 1.0 + coeff.a1 + coeff.a2;
            if den_dc.abs() > 1e-12 {
                dc_mag *= ((coeff.b0 + coeff.b1 + coeff.b2) / den_dc).abs();
            }
            let den_ny = 1.0 - coeff.a1 + coeff.a2;
            if den_ny.abs() > 1e-12 {
                nyq_mag *= ((coeff.b0 - coeff.b1 + coeff.b2) / den_ny).abs();
            }
        }
        let mut max_mag = dc_mag.max(nyq_mag).max(1.0_f64);

        // Dense logarithmic sweep: with Q clamped to <= 100 by the biquad, the
        // narrowest peak (at 20 Hz) is ~0.2 Hz wide, while 4096 points give a
        // ~0.034 Hz step there — comfortably inside the peak lobe. The sweep
        // only has to *locate* the lobe; golden-section refinement below
        // measures its apex exactly.
        const SWEEP_POINTS: usize = 4096;
        let log_min = min_f.ln();
        let log_max = max_f.ln();
        let log_step = (log_max - log_min) / SWEEP_POINTS as f64;

        let mut best_log_f = log_min;
        let mut best_index = 0usize;
        for i in 0..=SWEEP_POINTS {
            let log_f = log_min + i as f64 * log_step;
            let mag = eval_total_magnitude(log_f.exp(), &active_coeffs, sr);
            if mag > max_mag {
                max_mag = mag;
                best_log_f = log_f;
                best_index = i;
            }
        }

        // Refine around the best interior sweep point.
        let lo_log = if best_index > 0 {
            best_log_f - log_step
        } else {
            log_min
        };
        let hi_log = if best_index < SWEEP_POINTS {
            best_log_f + log_step
        } else {
            log_max
        };
        if hi_log > lo_log {
            let refined = refine_max_magnitude(lo_log.exp(), hi_log.exp(), |freq| {
                eval_total_magnitude(freq, &active_coeffs, sr)
            });
            max_mag = max_mag.max(refined);
        }

        if max_mag > 1.0 {
            (20.0 * max_mag.log10()) as f32
        } else {
            0.0
        }
    }

    /// Process an audio frame (alternative API)
    ///
    /// to avoid out-of-bounds access on frame.channels[1].
    pub fn process_frame(&mut self, frame: &mut AudioFrame) {
        if frame.num_channels <= 1 {
            // Mono: process the single channel through both L and R filters
            // to maintain consistent filter state, then copy result back.
            let (l, _r) = self.process(frame.channels[0], frame.channels[0]);
            frame.channels[0] = l;
        } else {
            let (l, r) = self.process(frame.channels[0], frame.channels[1]);
            frame.channels[0] = l;
            frame.channels[1] = r;
        }
    }

    /// Set a band's parameters and update its coefficients
    pub fn set_band(&mut self, index: usize, params: EqBandParams) {
        if let Some(band) = self.bands.get_mut(index) {
            band.params = params;
            band.update_coefficients(self.sample_rate);
            self.refresh_auto_headroom();
        }
    }

    /// Get number of bands
    pub fn num_bands(&self) -> usize {
        self.bands.len()
    }

    /// Get band parameters
    pub fn band_params(&self, index: usize) -> Option<&EqBandParams> {
        self.bands.get(index).map(|b| &b.params)
    }

    /// Set bass shelf gain
    pub fn set_bass_shelf(&mut self, gain_db: f32) {
        if !gain_db.is_finite() {
            log::warn!(
                "ParametricEq::set_bass_shelf: non-finite value {}; ignoring",
                gain_db
            );
            return;
        }
        let clamped = gain_db.clamp(-30.0, 30.0);
        self.bass_band.params.gain_db = clamped;
        self.bass_band.params.enabled = clamped.abs() > 0.001;
        self.bass_band.update_coefficients(self.sample_rate);
        self.refresh_auto_headroom();
    }

    /// Set treble shelf gain
    pub fn set_treble_shelf(&mut self, gain_db: f32) {
        if !gain_db.is_finite() {
            log::warn!(
                "ParametricEq::set_treble_shelf: non-finite value {}; ignoring",
                gain_db
            );
            return;
        }
        let clamped = gain_db.clamp(-30.0, 30.0);
        self.treble_band.params.gain_db = clamped;
        self.treble_band.params.enabled = clamped.abs() > 0.001;
        self.treble_band.update_coefficients(self.sample_rate);
        self.refresh_auto_headroom();
    }

    /// Update sample rate and recompute all coefficients
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        for band in &mut self.bands {
            band.update_coefficients(sample_rate);
        }
        self.bass_band.update_coefficients(sample_rate);
        self.treble_band.update_coefficients(sample_rate);
        self.refresh_auto_headroom();
    }

    /// Reset all bands
    pub fn reset(&mut self) {
        for band in &mut self.bands {
            band.reset();
        }
        self.bass_band.reset();
        self.treble_band.reset();
        // Note: headroom_db / headroom_linear are persistent settings, not
        // runtime state — no reset needed. The dynamic headroom fields that
        // used to live here (headroom_scale, peak_envelope, etc.) have been
        // removed; the downstream LookaheadLimiter now owns overshoot
        // protection.
    }
}

/// Product of the magnitude responses of `coeffs` at `freq_hz` (linear).
#[inline]
fn eval_total_magnitude(
    freq_hz: f64,
    coeffs: &[crate::dsp::biquad::BiquadCoeffs<f64>],
    sample_rate: f32,
) -> f64 {
    let mut total = 1.0_f64;
    for coeff in coeffs {
        total *= coeff.evaluate_magnitude(freq_hz as f32, sample_rate);
    }
    total
}

/// Maximise a unimodal magnitude function over `[a, b]` via golden-section
/// search, returning the largest magnitude encountered. Convergence is
/// ~1e-3 Hz in linear frequency — far finer than any practical EQ bandwidth.
fn refine_max_magnitude(mut a: f64, mut b: f64, f: impl Fn(f64) -> f64) -> f64 {
    const INV_PHI: f64 = 0.618_033_988_749_894_9;
    let mut c = b - INV_PHI * (b - a);
    let mut d = a + INV_PHI * (b - a);
    let mut fc = f(c);
    let mut fd = f(d);
    let mut best = fc.max(fd);
    while (b - a).abs() > 1e-3 {
        if fc < fd {
            a = c;
            c = d;
            fc = fd;
            d = a + INV_PHI * (b - a);
            fd = f(d);
        } else {
            b = d;
            d = c;
            fd = fc;
            c = b - INV_PHI * (b - a);
            fc = f(c);
        }
        best = best.max(fc).max(fd);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eq_passthrough_when_disabled() {
        let mut eq = ParametricEq::default_10_band(44100.0);
        let (l, r) = eq.process(0.5, 0.5);
        assert!((l - 0.5).abs() < 1e-5);
        assert!((r - 0.5).abs() < 1e-5);
    }

    #[test]
    fn test_eq_enabled_zero_gain() {
        let mut eq = ParametricEq::default_10_band(44100.0);
        eq.set_enabled(true);
        // After settling, zero-gain EQ should pass signal through
        for _ in 0..500 {
            eq.process(0.5, 0.5);
        }
        let (l, _r) = eq.process(0.5, 0.5);
        assert!(
            (l - 0.5).abs() < 0.05,
            "Zero-gain EQ should be near passthrough"
        );
    }

    #[test]
    fn test_eq_set_band() {
        let mut eq = ParametricEq::default_10_band(44100.0);
        eq.set_band(0, EqBandParams::peaking(100.0, 6.0, 1.4));
        let params = eq.band_params(0).unwrap();
        assert_eq!(params.frequency, 100.0);
        assert_eq!(params.gain_db, 6.0);
    }

    #[test]
    fn test_stereo_imaging_preserved() {
        let mut eq = ParametricEq::default_10_band(44100.0);
        eq.set_enabled(true);
        eq.set_band(0, EqBandParams::peaking(1000.0, 6.0, 1.4));
        // Process same signal on both channels
        for _ in 0..200 {
            eq.process(0.5, 0.5);
        }
        let (l, r) = eq.process(0.5, 0.5);
        assert!((l - r).abs() < 0.01, "Stereo imaging should be preserved");
    }

    #[test]
    fn test_eq_headroom_is_static_not_dynamic() {
        // Headroom is a STATIC pre-EQ attenuation: a fixed -3 dB gain applied
        // before the filters. It must NOT behave like a compressor — no
        // attack/release pumping, just a constant linear scale.
        let mut eq = ParametricEq::default_10_band(44100.0);
        eq.set_enabled(true);
        eq.set_headroom_db(-3.0);

        let expected = 2.0 * 10.0_f32.powf(-3.0 / 20.0); // ≈ 1.414

        // Feed a loud signal; after the filters' transient response settles,
        // the gain must be exactly the -3 dB headroom (plus unity band gain).
        for _ in 0..5000 {
            let _ = eq.process(2.0, 2.0);
        }
        let (l, r) = eq.process(2.0, 2.0);
        assert!(
            (l - expected).abs() < 0.02 && (r - expected).abs() < 0.02,
            "static headroom: expected ~{expected:.3}, got l={l:.3}, r={r:.3}"
        );

        // Constant gain regardless of input level => linear, not dynamic.
        let (l2, _) = eq.process(0.1, 0.1);
        let gain_ratio_hi = l / 2.0;
        let gain_ratio_lo = l2 / 0.1;
        assert!(
            (gain_ratio_hi - gain_ratio_lo).abs() < 0.01,
            "headroom must be level-independent (static), got {gain_ratio_hi} vs {gain_ratio_lo}"
        );

        // Default headroom is 0 dB (unity) — no unintended attenuation.
        let mut eq2 = ParametricEq::default_10_band(44100.0);
        eq2.set_enabled(true);
        for _ in 0..5000 {
            let _ = eq2.process(0.5, 0.5);
        }
        let (l3, _) = eq2.process(0.5, 0.5);
        assert!(
            (l3 - 0.5).abs() < 0.02,
            "default headroom must be unity, got {l3}"
        );
    }

    #[test]
    fn test_eq_reset_clears_filter_state() {
        // After reset, the EQ filters should be in a clean state (no
        // ringing from prior processing). This is the property the old
        // test_headroom_resets_to_unity was really verifying — that
        // reset() returns runtime state to a known-good baseline.
        let mut eq = ParametricEq::default_10_band(44100.0);
        eq.set_enabled(true);

        // Run a loud signal to populate filter state.
        for _ in 0..1000 {
            eq.process(2.0, 2.0);
        }
        // After reset, processing silence should produce near-silence
        // (filter state is cleared, no ringing).
        eq.reset();
        let mut max_out: f32 = 0.0;
        for _ in 0..100 {
            let (l, r) = eq.process(0.0, 0.0);
            max_out = max_out.max(l.abs()).max(r.abs());
        }
        assert!(
            max_out < 1e-4,
            "After reset, processing silence should produce near-silence; got max={}",
            max_out
        );
    }

    #[test]
    fn test_headroom_estimator_catches_high_q_peak() {
        let mut eq = ParametricEq::default_10_band(44100.0);
        eq.set_enabled(true);
        // A +12 dB, Q=50 peak is only ~20 Hz wide at 1 kHz. The previous
        // 151-point log sweep stepped ~47 Hz there and missed it entirely.
        eq.set_band(3, EqBandParams::peaking(1000.0, 12.0, 50.0));
        let boost = eq.combined_max_gain_db(44100.0);
        assert!(
            (boost - 12.0).abs() < 0.05,
            "Q=50 peak should measure ~12 dB, got {boost}"
        );
    }

    #[test]
    fn test_headroom_estimator_extreme_q_and_low_frequency() {
        let mut eq = ParametricEq::default_10_band(44100.0);
        eq.set_enabled(true);
        // Q=100 is the biquad's clamp limit; at 40 Hz its -3 dB bandwidth is
        // only 0.4 Hz — the hardest practical target for the estimator.
        eq.set_band(3, EqBandParams::peaking(40.0, 9.0, 100.0));
        let boost = eq.combined_max_gain_db(44100.0);
        assert!(
            (boost - 9.0).abs() < 0.05,
            "low-frequency Q=100 peak should measure ~9 dB, got {boost}"
        );
    }

    #[test]
    fn test_headroom_estimator_low_shelf_dc_gain() {
        // A Butterworth (Q=0.707) low shelf is monotonic, so its maximum is
        // exactly the +6 dB DC gain — which lives below the 20 Hz sweep floor
        // and must come from the analytic DC evaluation.
        let mut eq = ParametricEq::new(1, 44100.0);
        eq.set_enabled(true);
        eq.set_band(0, EqBandParams::lowshelf(100.0, 6.0, 0.707));
        let boost = eq.combined_max_gain_db(44100.0);
        assert!(
            (boost - 6.0).abs() < 0.05,
            "low-shelf boost should measure ~6 dB at DC, got {boost}"
        );
    }

    #[test]
    fn test_auto_headroom_recomputes_on_band_change() {
        let mut eq = ParametricEq::default_10_band(44100.0);
        eq.set_enabled(true);
        eq.set_auto_headroom(true);

        eq.set_band(3, EqBandParams::peaking(1000.0, 8.0, 1.0));
        assert!(
            (eq.headroom_db - (-8.0)).abs() < 0.05,
            "auto headroom should be -8 dB, got {}",
            eq.headroom_db
        );

        eq.set_band(3, EqBandParams::peaking(1000.0, 4.0, 1.0));
        assert!(
            (eq.headroom_db - (-4.0)).abs() < 0.05,
            "auto headroom should track the curve to -4 dB, got {}",
            eq.headroom_db
        );

        eq.set_band(3, EqBandParams::peaking(1000.0, -4.0, 1.0));
        assert!(
            eq.headroom_db.abs() < 0.05,
            "cut-only curve should need no headroom, got {}",
            eq.headroom_db
        );

        eq.set_auto_headroom(false);
        eq.set_headroom_db(-2.0);
        eq.set_band(3, EqBandParams::peaking(1000.0, 12.0, 1.0));
        assert!(
            (eq.headroom_db - (-2.0)).abs() < 1e-6,
            "manual headroom must be preserved when auto headroom is off"
        );
    }

    #[test]
    fn test_auto_headroom_disable_restores_manual() {
        let mut eq = ParametricEq::default_10_band(44100.0);
        eq.set_enabled(true);
        eq.set_headroom_db(-3.0);
        eq.set_auto_headroom(true);
        eq.set_band(3, EqBandParams::peaking(1000.0, 9.0, 1.0));
        assert!(
            (eq.headroom_db - (-9.0)).abs() < 0.05,
            "auto headroom should override the manual value, got {}",
            eq.headroom_db
        );

        eq.set_auto_headroom(false);
        assert!(
            (eq.headroom_db - (-3.0)).abs() < 1e-6,
            "disabling auto headroom should restore the manual -3 dB, got {}",
            eq.headroom_db
        );
    }
}
