//! Parametric equaliser — configurable multi-band EQ with smooth parameter transitions

use crate::dsp::biquad::{FilterType, SmoothedBiquad};

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
pub(crate) struct EqBand {
    pub(crate) params: EqBandParams,
    pub(crate) filter_left: SmoothedBiquad<f64>,
    pub(crate) filter_right: SmoothedBiquad<f64>,
}

impl EqBand {
    pub(crate) fn new() -> Self {
        Self {
            params: EqBandParams::default(),
            filter_left: SmoothedBiquad::new(),
            filter_right: SmoothedBiquad::new(),
        }
    }

    pub(crate) fn update_coefficients(&mut self, sample_rate: f32) {
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
    pub(crate) fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
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
    pub(crate) fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
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

    pub(crate) fn reset(&mut self) {
        self.filter_left.reset();
        self.filter_right.reset();
    }
}
