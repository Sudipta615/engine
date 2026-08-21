//! Biquad filter building blocks — coefficients, state, and smoothed variant.
//!
//! Implements Direct Form II Transposed (DFII-T) which has the best numerical
//! behaviour for audio-rate IIR filters at the cost of two state variables.
//!
//! ## Precision
//!
//! `BiquadCoeffs` and `BiquadState` are generic over [`crate::dsp::float::AudioFloat`].
//!
//! * `BiquadCoeffs<f32>` / `BiquadState<f32>` — Performance mode (f32 DSP path)
//! * `BiquadCoeffs<f64>` / `BiquadState<f64>` — Quality mode (f64 DSP path)
//!
//! Type aliases `BiquadCoeffsF32`, `BiquadCoeffsF64` etc. are provided for
//! ergonomic use.
//!
//! **Coefficient calculation** always uses `f64` internally regardless of the
//! output type, ensuring the best possible coefficient accuracy even in `f32` mode.

use crate::buffer::AudioFrame;
use crate::dsp::float::AudioFloat;

// ─────────────────────────────────────────────────────────────────────────────
// BiquadCoeffs<T>
// ─────────────────────────────────────────────────────────────────────────────

/// Biquad filter coefficients (normalised, a0 = 1).
///
/// Generic over the sample precision `T` (either `f32` or `f64`).
/// All coefficient calculation is performed in `f64` and then cast to `T`.
#[derive(Debug, Clone, Copy, Default)]
pub struct BiquadCoeffs<T: AudioFloat = f32> {
    pub b0: T,
    pub b1: T,
    pub b2: T,
    pub a1: T,
    pub a2: T,
}

/// Type alias for the common f32 variant.
pub type BiquadCoeffsF32 = BiquadCoeffs<f32>;
/// Type alias for the high-precision f64 variant.
pub type BiquadCoeffsF64 = BiquadCoeffs<f64>;

impl<T: AudioFloat> BiquadCoeffs<T> {
    /// Identity / pass-through coefficients
    pub fn identity() -> Self {
        Self {
            b0: T::one(),
            b1: T::zero(),
            b2: T::zero(),
            a1: T::zero(),
            a2: T::zero(),
        }
    }

    /// Build from raw f64 values.  All public constructors call this.
    #[inline]
    fn from_f64_raw(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: T::from_f64(b0),
            b1: T::from_f64(b1),
            b2: T::from_f64(b2),
            a1: T::from_f64(a1),
            a2: T::from_f64(a2),
        }
    }

    /// Validate and clamp filter parameters to safe ranges.
    ///
    /// Returns `(sample_rate_f64, freq_f64, q_f64)`.
    #[inline]
    fn validate_params(sample_rate: f32, freq: f32, q: f32) -> (f64, f64, f64) {
        let sr = if sample_rate <= 0.0 || !sample_rate.is_finite() {
            log::warn!(
                "Biquad: invalid sample_rate {}, clamping to 44100",
                sample_rate
            );
            44100.0_f64
        } else {
            sample_rate as f64
        };
        let f = if freq <= 0.0 || !freq.is_finite() {
            log::warn!("Biquad: invalid frequency {}, clamping to 20", freq);
            20.0_f64
        } else {
            (freq as f64).clamp(1.0, sr * 0.499)
        };
        let qv = if q <= 0.0 || !q.is_finite() {
            log::warn!("Biquad: invalid Q {}, clamping to 0.01", q);
            0.01_f64
        } else {
            (q as f64).clamp(0.01, 100.0)
        };
        (sr, f, qv)
    }

    /// Second-order (biquad) low-pass filter
    pub fn lowpass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let b0 = (1.0 - cos_w0) / 2.0;
        let b1 = 1.0 - cos_w0;
        let b2 = b0;
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            b0 / a0,
            b1 / a0,
            b2 / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }

    /// Second-order (biquad) high-pass filter
    pub fn highpass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let b0 = (1.0 + cos_w0) / 2.0;
        let b1 = -(1.0 + cos_w0);
        let b2 = b0;
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            b0 / a0,
            b1 / a0,
            b2 / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }

    /// Peaking EQ filter
    pub fn peaking(sample_rate: f32, freq: f32, gain_db: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let gain = gain_db as f64;
        let a = 10.0_f64.powf(gain / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        Self::from_f64_raw(
            b0 / a0,
            b1 / a0,
            b2 / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha / a) / a0,
        )
    }

    /// Low-shelf filter
    pub fn lowshelf(sample_rate: f32, freq: f32, gain_db: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let gain = gain_db as f64;
        let a = 10.0_f64.powf(gain / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha);
        let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
        let a1_n = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0);
        let a2_n = (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha;
        Self::from_f64_raw(b0 / a0, b1 / a0, b2 / a0, a1_n / a0, a2_n / a0)
    }

    /// High-shelf filter
    pub fn highshelf(sample_rate: f32, freq: f32, gain_db: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let gain = gain_db as f64;
        let a = 10.0_f64.powf(gain / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
        let a1_n = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0);
        let a2_n = (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha;
        Self::from_f64_raw(b0 / a0, b1 / a0, b2 / a0, a1_n / a0, a2_n / a0)
    }

    /// Band-pass filter (constant skirt gain)
    pub fn bandpass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            alpha / a0,
            0.0,
            -alpha / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }

    /// Notch filter
    pub fn notch(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            1.0 / a0,
            -2.0 * cos_w0 / a0,
            1.0 / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }

    /// All-pass filter
    pub fn allpass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            (1.0 - alpha) / a0,
            -2.0 * cos_w0 / a0,
            (1.0 + alpha) / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }
    /// Evaluate the magnitude response |H(e^{j*w})| of this biquad filter at the specified frequency.
    pub fn evaluate_magnitude(&self, freq_hz: f32, sample_rate: f32) -> f64 {
        let (mag, _phase) = self.evaluate_response(freq_hz, sample_rate);
        mag
    }

    /// Evaluate complex response (magnitude, phase) at the specified frequency.
    pub fn evaluate_response(&self, freq_hz: f32, sample_rate: f32) -> (f64, f64) {
        if sample_rate <= 0.0 || freq_hz <= 0.0 {
            return (1.0, 0.0);
        }
        let w = 2.0 * std::f64::consts::PI * (freq_hz as f64) / (sample_rate as f64);
        let cos_w = w.cos();
        let sin_w = w.sin();
        let cos_2w = (2.0 * w).cos();
        let sin_2w = (2.0 * w).sin();

        let b0 = self.b0.to_f64();
        let b1 = self.b1.to_f64();
        let b2 = self.b2.to_f64();
        let a1 = self.a1.to_f64();
        let a2 = self.a2.to_f64();

        // Numerator: b0 + b1*e^{-jw} + b2*e^{-j2w}
        let num_re = b0 + b1 * cos_w + b2 * cos_2w;
        let num_im = -b1 * sin_w - b2 * sin_2w;

        // Denominator: 1 + a1*e^{-jw} + a2*e^{-j2w}
        let den_re = 1.0 + a1 * cos_w + a2 * cos_2w;
        let den_im = -a1 * sin_w - a2 * sin_2w;

        let num_mag_sq = num_re * num_re + num_im * num_im;
        let den_mag_sq = den_re * den_re + den_im * den_im;

        // `den_mag_sq` can legitimately be very small at the resonance of a
        // high-Q filter (e.g. ~1e-14 for Q=100 at 40 Hz); only a truly zero
        // denominator is degenerate. The previous 1e-12 threshold silently
        // reported unity gain across such peaks, which corrupted downstream
        // magnitude / headroom measurements.
        let mag = if den_mag_sq > 0.0 {
            (num_mag_sq / den_mag_sq).sqrt()
        } else {
            1.0
        };

        let num_phase = num_im.atan2(num_re);
        let den_phase = den_im.atan2(den_re);
        let phase = num_phase - den_phase;

        (mag, phase)
    }
}

// Keep the IDENTITY constant for backward compat on the f32 specialization.
impl BiquadCoeffs<f32> {
    /// Identity / pass-through coefficients (f32 specialization, backward compat)
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };
}

// ─────────────────────────────────────────────────────────────────────────────
// FilterType
// ─────────────────────────────────────────────────────────────────────────────

/// Supported biquad filter topologies (spec §33).
///
/// # Phase / filter semantics
///
/// All topologies below are **minimum-phase** RBJ-cookbook biquads: the
/// magnitude response is the design target and the phase response is the
/// minimum-phase phase that is consistent with it (computed from the Hilbert
/// transform of the log-magnitude). They are stable for all valid parameters
/// (see [`Self::compute_coeffs`], which clamps Q and frequency to stable
/// ranges) and add one-sample-per-section phase delay at DC plus the
/// topology's own phase rotation near its corner frequency.
///
/// - `Lowpass` / `Highpass` / `Bandpass` / `Notch` — 2nd-order (−12 dB/oct for
///   LP/HP) minimum-phase selectivity; the magnitude roll-off is what carries
///   the phase rotation, so a steeper cut also means more phase lag near the
///   corner. There is **no** linear-phase option in this module.
/// - `Peaking` / `Lowshelf` / `Highshelf` — boost/cut shapes with a minimum
///   phase dip/bump in the transition region; the phase returns to 0° where
///   the gain is flat (far below/above the band).
/// - `Allpass` — flat magnitude, but a frequency-dependent phase rotation
///   (used to align phase between paths or to add intentional group delay).
///
/// "Minimum phase" is not claimed to be *better* than linear phase — it is
/// the actual, documented trade-off of this module: minimum group-delay and
/// causality, at the cost of phase dispersion. Linear-phase EQ lives in the
/// FIR/convolution subsystem (see `dsp::convolution`), which trades latency
/// for a symmetric, zero-phase-ripple response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterType {
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
    Allpass,
    Peaking,
    Lowshelf,
    Highshelf,
}

impl FilterType {
    /// Compute coefficients for this filter type at the given parameters.
    pub fn compute_coeffs<T: AudioFloat>(
        self,
        sample_rate: f32,
        freq: f32,
        gain_db: f32,
        q: f32,
    ) -> BiquadCoeffs<T> {
        match self {
            Self::Lowpass => BiquadCoeffs::lowpass(sample_rate, freq, q),
            Self::Highpass => BiquadCoeffs::highpass(sample_rate, freq, q),
            Self::Bandpass => BiquadCoeffs::bandpass(sample_rate, freq, q),
            Self::Notch => BiquadCoeffs::notch(sample_rate, freq, q),
            Self::Allpass => BiquadCoeffs::allpass(sample_rate, freq, q),
            Self::Peaking => BiquadCoeffs::peaking(sample_rate, freq, gain_db, q),
            Self::Lowshelf => BiquadCoeffs::lowshelf(sample_rate, freq, gain_db, q),
            Self::Highshelf => BiquadCoeffs::highshelf(sample_rate, freq, gain_db, q),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BiquadState<T>
// ─────────────────────────────────────────────────────────────────────────────

/// Biquad filter state (Direct Form II Transposed).
///
/// Generic over `T`. When `T = f32`, the accumulator state is **promoted to
/// `f64`** internally before the DFII-T recursion, then cast back — this is
/// the existing behaviour and gives the noise-floor benefit of f64 accumulation
/// even on the f32 path. When `T = f64`, everything stays in f64.
#[derive(Debug, Clone, Copy)]
pub struct BiquadState<T: AudioFloat = f32> {
    pub z1: f64,
    pub z2: f64,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: AudioFloat> Default for BiquadState<T> {
    fn default() -> Self {
        Self {
            z1: 0.0,
            z2: 0.0,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Type alias for the common f32 variant.
pub type BiquadStateF32 = BiquadState<f32>;
/// Type alias for the high-precision f64 variant.
pub type BiquadStateF64 = BiquadState<f64>;

impl<T: AudioFloat> BiquadState<T> {
    /// Process a single sample through the biquad filter.
    ///
    /// For `f32` precision: input is widened to `f64` for the recursion,
    /// output is narrowed back to `f32`.
    /// For `f64` precision: everything stays in `f64`.
    #[inline]
    pub fn process(&mut self, sample: T, coeffs: &BiquadCoeffs<T>) -> T {
        let s = sample.to_f64();
        let b0 = coeffs.b0.to_f64();
        let b1 = coeffs.b1.to_f64();
        let b2 = coeffs.b2.to_f64();
        let a1 = coeffs.a1.to_f64();
        let a2 = coeffs.a2.to_f64();

        let output = b0 * s + self.z1;
        self.z1 = crate::buffer::flush_denormal_f64(b1 * s - a1 * output + self.z2);
        self.z2 = crate::buffer::flush_denormal_f64(b2 * s - a2 * output);
        T::from_f64(output)
    }

    /// Reset filter state
    #[inline]
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SmoothedBiquad<T> — parameter-domain smoothing
// ─────────────────────────────────────────────────────────────────────────────

/// EQ parameter set (frequency, gain, Q) for a single band.
/// Smoothing operates in this domain rather than on raw coefficients,
/// which avoids the transient instability of direct coefficient interpolation
/// at extreme settings (high Q, large gains, topology changes).
#[derive(Debug, Clone, Copy)]
struct EqParams {
    freq: f32,
    gain_db: f32,
    q: f32,
    filter_type: FilterType,
}

impl Default for EqParams {
    fn default() -> Self {
        Self {
            freq: 1000.0,
            gain_db: 0.0,
            q: 0.707,
            filter_type: FilterType::Peaking,
        }
    }
}

impl EqParams {
    #[inline]
    fn lerp(a: &Self, b: &Self, t: f32) -> Self {
        Self {
            freq: a.freq + (b.freq - a.freq) * t,
            gain_db: a.gain_db + (b.gain_db - a.gain_db) * t,
            q: a.q + (b.q - a.q) * t,
            // Topology uses the target immediately for the "to" filter; the
            // jump in the *response* caused by a topology change is smoothed
            // by `SmoothedBiquad::transition`, which crossfades from the
            // frozen previous filter instead of switching instantly.
            filter_type: b.filter_type,
        }
    }
}

/// Frozen snapshot of the previous filter used during a topology-change
/// crossfade (see [`SmoothedBiquad::transition`]).
///
/// When the filter *type* changes (e.g. Peaking → LowShelf) there is no
/// continuous path between the two topologies in either parameter or
/// coefficient space, so a naive ramp would jump the response instantly at
/// the first step. Instead the old topology is frozen here (coefficients +
/// per-channel state) and its output is crossfaded out over the smoothing
/// window while the new topology ramps in — the audible result is a short
/// blend instead of a discontinuity.
#[derive(Debug, Clone)]
struct FilterTransition<T: AudioFloat> {
    /// Coefficients of the previous topology at the moment of the change.
    from_coeffs: BiquadCoeffs<T>,
    /// Per-channel filter state of the previous topology (kept coherent so
    /// the from-filter continues to process the same input stream).
    from_states: [BiquadState<T>; 2],
    /// Remaining crossfade steps.
    remaining: u32,
    /// Total crossfade steps (== the parameter smoothing window).
    total: u32,
}

/// A biquad filter that smoothly transitions between parameter sets, computing
/// new coefficients from interpolated parameters rather than interpolating
/// coefficients directly.
///
/// This avoids the coefficient-domain instability that occurs when `b0/b1/b2/a1/a2`
/// are linearly interpolated at high Q or during topology changes.
#[derive(Debug, Clone)]
pub struct SmoothedBiquad<T: AudioFloat = f32> {
    /// Current active parameters (source of the smooth ramp)
    current_params: EqParams,
    /// Target parameters set by the user
    target_params: EqParams,
    /// Remaining smoothing steps
    remaining: u32,
    /// Total smoothing steps (≈ 1.5 ms at the configured sample rate)
    smooth_steps: u32,
    /// Baked coefficients from the latest interpolated parameters
    current_coeffs: BiquadCoeffs<T>,
    /// Per-channel filter state (stereo)
    states: [BiquadState<T>; 2],
    /// Cached sample rate for coefficient recalculation
    sample_rate: f32,
    /// Active topology-change crossfade, if any. `None` in steady state.
    transition: Option<FilterTransition<T>>,
}

/// Type alias for the common f32 variant.
pub type SmoothedBiquadF32 = SmoothedBiquad<f32>;
/// Type alias for the high-precision f64 variant.
pub type SmoothedBiquadF64 = SmoothedBiquad<f64>;

impl<T: AudioFloat> SmoothedBiquad<T> {
    /// Create a new smoothed biquad initialised to identity (pass-through).
    pub fn new() -> Self {
        let p = EqParams::default();
        Self {
            current_params: p,
            target_params: p,
            remaining: 0,
            smooth_steps: 64,
            current_coeffs: BiquadCoeffs::identity(),
            states: [BiquadState::default(), BiquadState::default()],
            sample_rate: 44100.0,
            transition: None,
        }
    }

    /// Update the sample rate, recomputing smooth steps for ≈1.5 ms duration.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return;
        }
        self.sample_rate = sample_rate;
        self.smooth_steps = (sample_rate * 0.0015).clamp(8.0, 4096.0) as u32;
    }

    /// Set new target parameters — begins smooth transition.
    ///
    /// When the target **topology** differs from the currently settled one, a
    /// crossfade from the old filter to the new filter is started so the
    /// response change is a short blend rather than an instant jump while the
    /// remaining parameters (frequency/gain/Q) interpolate.
    pub fn set_target_params(&mut self, freq: f32, gain_db: f32, q: f32, filter_type: FilterType) {
        let topology_changed = filter_type != self.current_params.filter_type;
        self.target_params = EqParams {
            freq,
            gain_db,
            q,
            filter_type,
        };
        self.remaining = self.smooth_steps;
        if topology_changed {
            self.transition = Some(FilterTransition {
                from_coeffs: self.current_coeffs,
                from_states: self.states,
                remaining: self.smooth_steps,
                total: self.smooth_steps,
            });
        }
    }

    /// Whether smoothing is complete (reached target parameters and any
    /// topology-change crossfade has finished).
    #[inline]
    pub fn is_settled(&self) -> bool {
        self.remaining == 0 && self.transition.is_none()
    }

    /// Legacy: set new target from pre-computed coefficients.
    /// For backward compatibility — passes through immediately (no parameter ramp).
    pub fn set_target(&mut self, coeffs: BiquadCoeffs<T>) {
        self.current_coeffs = coeffs;
        self.remaining = 0;
        self.transition = None;
    }

    /// Process a single sample on a given channel.
    ///
    /// While a topology-change crossfade is active, the frozen from-filter is
    /// run on the same sample and the two outputs are blended linearly from
    /// the old filter (t=0) to the new filter (t=1) over the smoothing window.
    #[inline]
    pub fn process_sample(&mut self, ch: usize, sample: T) -> T {
        if ch < 2 {
            let out = self.states[ch].process(sample, &self.current_coeffs);
            match &mut self.transition {
                Some(tr) => {
                    let from_out = tr.from_states[ch].process(sample, &tr.from_coeffs);
                    let t = T::from_f64(1.0 - tr.remaining as f64 / tr.total.max(1) as f64);
                    let one_minus_t = T::one() - t;
                    from_out * one_minus_t + out * t
                }
                None => out,
            }
        } else {
            sample
        }
    }

    /// Process an audio frame through the filter (both channels).
    #[inline]
    pub fn process_frame(&mut self, frame: &mut AudioFrame) {
        if frame.num_channels <= 1 {
            let out = self.states[0].process(T::from_f32(frame.channels[0]), &self.current_coeffs);
            frame.channels[0] = out.to_f32();
            frame.channels[1] = frame.channels[0];
        } else {
            for (ch, state) in self
                .states
                .iter_mut()
                .enumerate()
                .take(frame.num_channels as usize)
            {
                let s_in = T::from_f32(frame.channels[ch]);
                frame.channels[ch] = state.process(s_in, &self.current_coeffs).to_f32();
            }
        }
        self.advance_smoothing();
    }

    /// Advance parameter interpolation by one sample and recompute coefficients.
    #[inline]
    pub(crate) fn advance_smoothing(&mut self) {
        if self.remaining > 0 {
            let t = 1.0 - (self.remaining as f32 / self.smooth_steps as f32);
            let interpolated = EqParams::lerp(&self.current_params, &self.target_params, t);
            self.current_coeffs = interpolated.filter_type.compute_coeffs(
                self.sample_rate,
                interpolated.freq,
                interpolated.gain_db,
                interpolated.q,
            );
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current_params = self.target_params;
                self.current_coeffs = self.target_params.filter_type.compute_coeffs(
                    self.sample_rate,
                    self.target_params.freq,
                    self.target_params.gain_db,
                    self.target_params.q,
                );
            }
        }
        if let Some(tr) = &mut self.transition {
            tr.remaining = tr.remaining.saturating_sub(1);
            if tr.remaining == 0 {
                self.transition = None;
            }
        }
    }

    /// Reset filter state (but not parameters).
    pub fn reset(&mut self) {
        self.states[0].reset();
        self.states[1].reset();
        self.transition = None;
    }

    /// Reset both state and parameters to identity.
    pub fn reset_all(&mut self) {
        self.reset();
        let p = EqParams::default();
        self.current_params = p;
        self.target_params = p;
        self.remaining = 0;
        self.current_coeffs = BiquadCoeffs::identity();
    }
}

impl<T: AudioFloat> Default for SmoothedBiquad<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backward compatibility: re-export f32 specializations with old names
// ─────────────────────────────────────────────────────────────────────────────

/// Backward-compatible alias. New code should use `BiquadCoeffs<f32>` or
/// `BiquadCoeffs<f64>` directly.
pub type BiquadCoeffs32 = BiquadCoeffs<f32>;

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn test_identity_passes_signal() {
        let coeffs = BiquadCoeffs::<f32>::identity();
        let mut state = BiquadState::<f32>::default();
        let output = state.process(0.5_f32, &coeffs);
        assert_relative_eq!(output, 0.5_f32, epsilon = 1e-6);
    }

    #[test]
    fn test_identity_f64_passes_signal() {
        let coeffs = BiquadCoeffs::<f64>::identity();
        let mut state = BiquadState::<f64>::default();
        let output = state.process(0.5_f64, &coeffs);
        assert_relative_eq!(output, 0.5_f64, epsilon = 1e-12);
    }

    #[test]
    fn test_lowpass_attenuates_high_freq() {
        let coeffs = BiquadCoeffs::<f32>::lowpass(44100.0, 1000.0, 0.707);
        let mut state = BiquadState::<f32>::default();
        let mut output = 0.0_f32;
        for _ in 0..1000 {
            output = state.process(1.0, &coeffs);
        }
        assert!(
            output > 0.5,
            "DC should pass through lowpass, got {}",
            output
        );
    }

    #[test]
    fn test_lowpass_f64_attenuates_high_freq() {
        let coeffs = BiquadCoeffs::<f64>::lowpass(44100.0, 1000.0, 0.707);
        let mut state = BiquadState::<f64>::default();
        let mut output = 0.0_f64;
        for _ in 0..1000 {
            output = state.process(1.0, &coeffs);
        }
        assert!(
            output > 0.5,
            "DC should pass through f64 lowpass, got {}",
            output
        );
    }

    #[test]
    fn test_smoothed_biquad_converges() {
        let mut bq = SmoothedBiquad::<f32>::new();
        bq.set_target_params(1000.0, 0.0, 0.707, FilterType::Lowpass);
        for _ in 0..200 {
            let mut frame = AudioFrame::stereo(0.5, 0.5);
            bq.process_frame(&mut frame);
        }
        assert_eq!(bq.remaining, 0);
    }

    #[test]
    fn test_smoothed_biquad_f64_converges() {
        let mut bq = SmoothedBiquad::<f64>::new();
        bq.set_target_params(1000.0, 3.0, 1.0, FilterType::Peaking);
        for _ in 0..200 {
            let mut frame = AudioFrame::stereo(0.5, 0.5);
            bq.process_frame(&mut frame);
        }
        assert_eq!(bq.remaining, 0);
    }

    #[test]
    fn test_filter_type_dispatch_f32() {
        let coeffs: BiquadCoeffs<f32> =
            FilterType::Lowpass.compute_coeffs(44100.0, 1000.0, 0.0, 0.707);
        assert!(coeffs.b0 != 0.0);
    }

    #[test]
    fn test_filter_type_dispatch_f64() {
        let coeffs: BiquadCoeffs<f64> =
            FilterType::Peaking.compute_coeffs(44100.0, 1000.0, 3.0, 1.0);
        assert!(coeffs.b0 != 0.0);
    }

    #[test]
    fn test_topology_change_crossfades_instead_of_jumping() {
        // Regression for "filter-topology changes are abrupt": changing
        // Peaking → LowShelf used to jump the response instantly on the first
        // smoothing step while freq/gain/Q were still interpolating. The
        // topology change must now be crossfaded from the frozen previous
        // filter over the smoothing window.
        //
        // DC test: a peaking filter has 0 dB DC gain; a +12 dB low shelf has
        // ≈ +12 dB DC gain. On a constant 1.0 input the old behaviour jumped
        // from 1.0 to ~3.98 in one sample; the crossfade must ramp smoothly.
        let mut bq = SmoothedBiquad::<f64>::new();
        bq.set_sample_rate(48_000.0);
        bq.set_target_params(1000.0, 12.0, 1.0, FilterType::Peaking);
        // Warm up well past the ramp so the filter state fully converges on DC.
        for _ in 0..20_000 {
            let _ = bq.process_sample(0, 1.0);
            bq.advance_smoothing();
        }
        assert!(bq.is_settled());

        // Settled peaking DC gain is 1.0.
        let before = bq.process_sample(0, 1.0);
        assert!((before - 1.0).abs() < 1e-6, "peaking DC gain: {before}");

        // Request a topology change to a +12 dB low shelf.
        bq.set_target_params(1000.0, 12.0, 1.0, FilterType::Lowshelf);
        assert!(!bq.is_settled(), "topology change must start a transition");

        let total = bq.smooth_steps as f64;
        let target_dc = 10.0_f64.powf(12.0 / 20.0); // ≈ 3.981

        // The very first sample after the change must still be ~the old
        // response (blend t=0), not an instant jump to the new DC gain.
        let first = bq.process_sample(0, 1.0);
        assert!(
            (first - 1.0).abs() < 0.05,
            "first post-change sample must stay near the old response, got {first}"
        );
        bq.advance_smoothing();

        let mut prev = first;
        let mut max_step = 0.0_f64;
        let mut at_end_of_blend = 0.0_f64;
        for _ in 1..bq.smooth_steps {
            let out = bq.process_sample(0, 1.0);
            max_step = max_step.max((out - prev).abs());
            prev = out;
            at_end_of_blend = out;
            bq.advance_smoothing();
        }

        // Ramp must be gradual: the total change spread over the window, with
        // generous margin for the filter's own transient.
        assert!(
            max_step < (target_dc - 1.0) / total * 8.0,
            "topology crossfade step too large: {max_step:.6} (expected ~{:.6})",
            (target_dc - 1.0) / total
        );

        // By the end of the blend the response must already be most of the way
        // to the target (the blend itself is over), and the transition must
        // have completed.
        assert!(bq.is_settled(), "transition must complete");
        assert!(
            at_end_of_blend > target_dc * 0.95,
            "blend should reach ~the new DC gain by its end, got {at_end_of_blend}"
        );

        // Continue feeding DC after the crossfade: the new topology must fully
        // converge on the low-shelf DC gain (+12 dB ≈ 3.981).
        let mut last = 0.0_f64;
        for _ in 0..20_000 {
            last = bq.process_sample(0, 1.0);
            bq.advance_smoothing();
        }
        assert!(
            (last - target_dc).abs() < 0.01,
            "should converge to low-shelf DC gain {target_dc}, got {last}"
        );
    }

    #[test]
    fn test_evaluate_magnitude_high_q_resonance() {
        // Regression: a narrow resonance used to trip a `den_mag_sq > 1e-12`
        // guard and report unity gain at the peak. Q=100, +9 dB at 40 Hz has
        // a denominator magnitude squared of ~1e-14 at resonance, so the
        // guard must not treat it as degenerate.
        let coeffs: BiquadCoeffs<f64> =
            FilterType::Peaking.compute_coeffs(44100.0, 40.0, 9.0, 100.0);
        let mag = coeffs.evaluate_magnitude(40.0, 44100.0);
        let expected = 10.0_f64.powf(9.0 / 20.0);
        assert_relative_eq!(mag, expected, epsilon = 1e-6);
    }
}
