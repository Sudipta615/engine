//! Three-band multiband compressor with playback-oriented detector features.
//!
//! ## Structure vs character
//!
//! The DSP layer is deliberately **neutral**: the crossover structure
//! (Linkwitz-Riley 4th order at 250 Hz / 4 kHz) is pure signal processing, and
//! the default band parameters are transparent (ratio 1:1 → no gain
//! reduction). Sonic "character" presets (thump catching, peak taming, …)
//! belong in the UI layer as parameter presets — they are not baked into the
//! processor.
//!
//! ## Detector features
//!
//! - **Stereo-linked detection** (default on): the band computes independent
//!   L/R envelopes but derives a single gain from the louder channel and
//!   applies it to both. This keeps a centered bass or transient from
//!   shifting the stereo image.
//! - **Optional RMS detection**: a windowed (leaky-integrator) RMS envelope
//!   instead of the instantaneous peak follower. RMS reacts to program level
//!   rather than transients.
//! - **Soft knee**: a quadratic transition region around the threshold so
//!   compression fades in instead of switching abruptly.
//! - Peak detection with attack/release smoothing as the default, matching
//!   classic single-band behaviour.

use super::biquad::{BiquadCoeffs, BiquadState};
use config::CompressorDetector;

/// One band of the multiband compressor.
struct BandCompressor {
    threshold_db_cached: f32,
    ratio: f32,
    attack_coeff: f32,
    release_coeff: f32,
    makeup_gain: f32, // linear
    /// Soft-knee width in dB (0 = hard knee).
    knee_db: f32,
    /// Detection mode (Peak | Rms).
    detector: CompressorDetector,
    /// When true, L/R share one gain derived from the louder envelope.
    stereo_link: bool,
    /// Leaky-integrated mean-square values (RMS detector, per channel).
    rms_mean_sq_l: f32,
    rms_mean_sq_r: f32,
    /// RMS window coefficient (≈ 10 ms at typical rates).
    rms_coeff: f32,
    /// Smoothed envelopes (per channel).
    envelope_l: f32,
    envelope_r: f32,
}

impl BandCompressor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        sample_rate: f32,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_db: f32,
        knee_db: f32,
        detector: CompressorDetector,
        stereo_link: bool,
    ) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let attack_ms = attack_ms.max(0.0001);
        let release_ms = release_ms.max(0.0001);
        let ratio = ratio.clamp(1.0, 100.0);
        // RMS window ≈ 10 ms, independent of the attack/release times.
        let rms_window_secs = 0.010;
        Self {
            threshold_db_cached: threshold_db.max(-100.0),
            ratio,
            attack_coeff: (-1.0 / (attack_ms * 0.001 * sample_rate)).exp(),
            release_coeff: (-1.0 / (release_ms * 0.001 * sample_rate)).exp(),
            makeup_gain: 10.0_f32.powf(makeup_db / 20.0),
            knee_db: knee_db.max(0.0),
            detector,
            stereo_link,
            rms_mean_sq_l: 0.0,
            rms_mean_sq_r: 0.0,
            rms_coeff: 1.0 - (-1.0 / (rms_window_secs * sample_rate)).exp(),
            envelope_l: 0.0,
            envelope_r: 0.0,
        }
    }

    /// Advance one channel's envelope detector and return the smoothed
    /// envelope (linear). The absolute input sample is `abs_sample`.
    #[inline]
    fn update_envelope(&mut self, ch: usize, abs_sample: f64) -> f64 {
        let (envelope, mean_sq) = if ch == 0 {
            (&mut self.envelope_l, &mut self.rms_mean_sq_l)
        } else {
            (&mut self.envelope_r, &mut self.rms_mean_sq_r)
        };
        let env = *envelope as f64;
        let new_env = match self.detector {
            CompressorDetector::Peak => {
                let coeff = if abs_sample > env {
                    self.attack_coeff as f64
                } else {
                    self.release_coeff as f64
                };
                abs_sample + coeff * (env - abs_sample)
            }
            CompressorDetector::Rms => {
                // Leaky-integrated mean square, then sqrt → RMS level, then
                // attack/release smoothing on top (so attack/release still
                // govern how fast the gain moves).
                let ms = *mean_sq as f64;
                let new_ms = ms + self.rms_coeff as f64 * (abs_sample * abs_sample - ms);
                *mean_sq = new_ms as f32;
                let rms = new_ms.sqrt();
                let coeff = if rms > env {
                    self.attack_coeff as f64
                } else {
                    self.release_coeff as f64
                };
                rms + coeff * (env - rms)
            }
        };
        let new_env = if new_env < 1e-6 { 0.0 } else { new_env };
        *envelope = new_env as f32;
        new_env
    }

    /// Compute the linear gain multiplier for a given linear envelope level,
    /// applying the (soft-knee) threshold law and ratio.
    #[inline]
    fn gain_for_envelope(&self, env_linear: f64) -> f64 {
        if env_linear <= 0.0 {
            return 1.0;
        }
        let env_db = 20.0 * env_linear.log10().max(-100.0);
        let threshold_db = self.threshold_db_cached as f64;
        let knee = self.knee_db as f64;
        // slope = 1 - 1/R; 0 when ratio == 1 (transparent default).
        let slope = 1.0 - 1.0 / (self.ratio as f64).max(1.0);
        if slope <= 0.0 {
            return 1.0;
        }

        let t = env_db - threshold_db;
        let reduction_db = if 2.0 * t < -knee {
            // Below the knee: no compression.
            0.0
        } else if knee > 0.0 && 2.0 * t < knee {
            // Quadratic soft-knee region, C1-continuous with both neighbours.
            let x = t + knee / 2.0;
            (x * x) / (2.0 * knee) * slope
        } else {
            // Above the knee: full ratio.
            t * slope
        };

        let gain = 10.0_f64.powf(-reduction_db / 20.0);
        if gain.is_finite() {
            gain
        } else {
            1.0
        }
    }

    /// Process a stereo pair with per-band envelopes.
    #[inline]
    fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        let env_l = self.update_envelope(0, left.abs());
        let env_r = self.update_envelope(1, right.abs());

        if self.stereo_link {
            // One gain from the louder channel, applied to both — preserves
            // the stereo image for centered bass/transients.
            let gain = self.gain_for_envelope(env_l.max(env_r));
            let g = gain * self.makeup_gain as f64;
            (left * g, right * g)
        } else {
            let gl = self.gain_for_envelope(env_l) * self.makeup_gain as f64;
            let gr = self.gain_for_envelope(env_r) * self.makeup_gain as f64;
            (left * gl, right * gr)
        }
    }

    fn reset(&mut self) {
        self.envelope_l = 0.0;
        self.envelope_r = 0.0;
        self.rms_mean_sq_l = 0.0;
        self.rms_mean_sq_r = 0.0;
    }
}

/// Linkwitz-Riley 4th order crossover (cascaded 2nd order Butterworth)
struct CrossoverFilter {
    lp1: BiquadState<f64>,
    lp2: BiquadState<f64>,
    hp1: BiquadState<f64>,
    hp2: BiquadState<f64>,
    lp_coeffs: BiquadCoeffs<f64>,
    hp_coeffs: BiquadCoeffs<f64>,
}

impl CrossoverFilter {
    fn new(sample_rate: f32, freq: f32) -> Self {
        Self {
            lp1: BiquadState::default(),
            lp2: BiquadState::default(),
            hp1: BiquadState::default(),
            hp2: BiquadState::default(),
            lp_coeffs: BiquadCoeffs::lowpass(sample_rate, freq, 0.707),
            hp_coeffs: BiquadCoeffs::highpass(sample_rate, freq, 0.707),
        }
    }

    #[inline]
    fn process_f64(&mut self, sample: f64) -> (f64, f64) {
        let mut low = self.lp1.process(sample, &self.lp_coeffs);
        low = self.lp2.process(low, &self.lp_coeffs);

        let mut high = self.hp1.process(sample, &self.hp_coeffs);
        high = self.hp2.process(high, &self.hp_coeffs);

        (low, high)
    }

    fn reset(&mut self) {
        self.lp1.reset();
        self.lp2.reset();
        self.hp1.reset();
        self.hp2.reset();
    }
}

pub struct MultibandCompressor {
    enabled: bool,
    #[allow(dead_code)]
    sample_rate: f32,

    // Crossovers
    xover_low_mid_l: CrossoverFilter,
    xover_low_mid_r: CrossoverFilter,
    xover_mid_high_l: CrossoverFilter,
    xover_mid_high_r: CrossoverFilter,

    // Compressors (stereo-linked by default: one shared gain per band)
    comp_low_l: BandCompressor,
    comp_low_r: BandCompressor,
    comp_mid_l: BandCompressor,
    comp_mid_r: BandCompressor,
    comp_high_l: BandCompressor,
    comp_high_r: BandCompressor,
}

/// Neutral band parameters — transparent by default (ratio 1:1, no makeup).
/// Sonic character is a UI-level preset concern, not a DSP default.
fn neutral_band(sample_rate: f32) -> (BandCompressor, BandCompressor) {
    (
        BandCompressor::new(
            sample_rate,
            -6.0,
            1.0,
            5.0,
            100.0,
            0.0,
            6.0,
            CompressorDetector::Peak,
            true,
        ),
        BandCompressor::new(
            sample_rate,
            -6.0,
            1.0,
            5.0,
            100.0,
            0.0,
            6.0,
            CompressorDetector::Peak,
            true,
        ),
    )
}

impl MultibandCompressor {
    pub fn new(sample_rate: f32) -> Self {
        let freq_low_mid = 250.0;
        let freq_mid_high = 4000.0;

        let (comp_low_l, comp_low_r) = neutral_band(sample_rate);
        let (comp_mid_l, comp_mid_r) = neutral_band(sample_rate);
        let (comp_high_l, comp_high_r) = neutral_band(sample_rate);

        Self {
            enabled: false,
            sample_rate,

            xover_low_mid_l: CrossoverFilter::new(sample_rate, freq_low_mid),
            xover_low_mid_r: CrossoverFilter::new(sample_rate, freq_low_mid),
            xover_mid_high_l: CrossoverFilter::new(sample_rate, freq_mid_high),
            xover_mid_high_r: CrossoverFilter::new(sample_rate, freq_mid_high),

            comp_low_l,
            comp_low_r,
            comp_mid_l,
            comp_mid_r,
            comp_high_l,
            comp_high_r,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            if !enabled {
                self.reset();
            }
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        // Preserve user-configured band parameters across sample rate changes.

        let enabled = self.enabled;
        let old_rate = self.sample_rate;

        // Snapshot the current band parameters so we can restore them after
        // rebuilding the crossover filters (whose coefficients depend on the
        // new sample rate). Feature settings (knee/detector/link) round-trip
        // directly; attack/release coefficients are inverted back to ms.
        let snapshot = |comp: &BandCompressor,
                        sr: f32|
         -> (f32, f32, f32, f32, f32, f32, CompressorDetector, bool) {
            (
                comp.threshold_db_cached,
                comp.ratio,
                if comp.attack_coeff > 0.0 && comp.attack_coeff < 1.0 && sr > 0.0 {
                    (-1.0 / (sr * comp.attack_coeff.ln())) * 1000.0
                } else {
                    0.0001
                },
                if comp.release_coeff > 0.0 && comp.release_coeff < 1.0 && sr > 0.0 {
                    (-1.0 / (sr * comp.release_coeff.ln())) * 1000.0
                } else {
                    0.001
                },
                20.0 * comp.makeup_gain.log10(),
                comp.knee_db,
                comp.detector,
                comp.stereo_link,
            )
        };

        let low_params = snapshot(&self.comp_low_l, old_rate);
        let mid_params = snapshot(&self.comp_mid_l, old_rate);
        let high_params = snapshot(&self.comp_high_l, old_rate);

        // Rebuild at the new sample rate with default band params, then
        // re-apply the snapshotted user params.
        *self = Self::new(sample_rate);
        self.enabled = enabled;
        self.set_band_params(
            0,
            low_params.0,
            low_params.1,
            low_params.2,
            low_params.3,
            low_params.4,
        );
        self.set_band_features(0, low_params.5, low_params.6, low_params.7);
        self.set_band_params(
            1,
            mid_params.0,
            mid_params.1,
            mid_params.2,
            mid_params.3,
            mid_params.4,
        );
        self.set_band_features(1, mid_params.5, mid_params.6, mid_params.7);
        self.set_band_params(
            2,
            high_params.0,
            high_params.1,
            high_params.2,
            high_params.3,
            high_params.4,
        );
        self.set_band_features(2, high_params.5, high_params.6, high_params.7);
    }

    /// Set the core band parameters (threshold, ratio, times, makeup).
    ///
    /// Feature settings (soft knee, detector mode, stereo link) are preserved
    /// from the band's current state, so existing callers keep their behaviour.
    pub fn set_band_params(
        &mut self,
        band: usize,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    ) {
        let (comp_l, comp_r) = match band {
            0 => (&mut self.comp_low_l, &mut self.comp_low_r),
            1 => (&mut self.comp_mid_l, &mut self.comp_mid_r),
            2 => (&mut self.comp_high_l, &mut self.comp_high_r),
            _ => return,
        };

        // Preserve the band's detector features across a param update.
        let knee = comp_l.knee_db;
        let detector = comp_l.detector;
        let link = comp_l.stereo_link;

        let mut comp_new_l = BandCompressor::new(
            self.sample_rate,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_gain_db,
            knee,
            detector,
            link,
        );
        let mut comp_new_r = BandCompressor::new(
            self.sample_rate,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_gain_db,
            knee,
            detector,
            link,
        );

        comp_new_l.envelope_l = comp_l.envelope_l;
        comp_new_l.envelope_r = comp_l.envelope_r;
        comp_new_l.rms_mean_sq_l = comp_l.rms_mean_sq_l;
        comp_new_l.rms_mean_sq_r = comp_l.rms_mean_sq_r;
        comp_new_r.envelope_l = comp_r.envelope_l;
        comp_new_r.envelope_r = comp_r.envelope_r;
        comp_new_r.rms_mean_sq_l = comp_r.rms_mean_sq_l;
        comp_new_r.rms_mean_sq_r = comp_r.rms_mean_sq_r;

        *comp_l = comp_new_l;
        *comp_r = comp_new_r;
    }

    /// Set detector features for a band: soft-knee width, detector mode and
    /// stereo linking.
    pub fn set_band_features(
        &mut self,
        band: usize,
        knee_db: f32,
        detector: CompressorDetector,
        stereo_link: bool,
    ) {
        let (comp_l, comp_r) = match band {
            0 => (&mut self.comp_low_l, &mut self.comp_low_r),
            1 => (&mut self.comp_mid_l, &mut self.comp_mid_r),
            2 => (&mut self.comp_high_l, &mut self.comp_high_r),
            _ => return,
        };
        comp_l.knee_db = knee_db.max(0.0);
        comp_l.detector = detector;
        comp_l.stereo_link = stereo_link;
        comp_r.knee_db = knee_db.max(0.0);
        comp_r.detector = detector;
        comp_r.stereo_link = stereo_link;
    }

    /// Whether the multiband compressor is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.enabled {
            return (left, right);
        }

        let (ol, or_) = self.process_f64(left as f64, right as f64);
        (ol as f32, or_ as f32)
    }

    /// Process a stereo sample pair in native f64 precision.
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if !self.enabled {
            return (left, right);
        }

        // Split Left
        let (l_low, l_mid_high) = self.xover_low_mid_l.process_f64(left);
        let (l_mid, l_high) = self.xover_mid_high_l.process_f64(l_mid_high);

        // Split Right
        let (r_low, r_mid_high) = self.xover_low_mid_r.process_f64(right);
        let (r_mid, r_high) = self.xover_mid_high_r.process_f64(r_mid_high);

        // Compress (stereo-linked per band by default)
        let (l_low_c, r_low_c) = self.comp_low_l.process_f64(l_low, r_low);
        let (l_mid_c, r_mid_c) = self.comp_mid_l.process_f64(l_mid, r_mid);
        let (l_high_c, r_high_c) = self.comp_high_l.process_f64(l_high, r_high);

        // Sum back
        (l_low_c + l_mid_c + l_high_c, r_low_c + r_mid_c + r_high_c)
    }

    /// Process a block of stereo frames in place. Hoists the enabled check
    /// out of the per-frame loop.
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

    /// Process a block of stereo frames in native f64 precision. Hoists the
    /// enabled check out of the per-frame loop.
    #[inline]
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if !self.enabled {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i], right[i]);
            left[i] = ol;
            right[i] = or_;
        }
    }

    pub fn reset(&mut self) {
        self.xover_low_mid_l.reset();
        self.xover_low_mid_r.reset();
        self.xover_mid_high_l.reset();
        self.xover_mid_high_r.reset();

        self.comp_low_l.reset();
        self.comp_low_r.reset();
        self.comp_mid_l.reset();
        self.comp_mid_r.reset();
        self.comp_high_l.reset();
        self.comp_high_r.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_passthrough() {
        let mut mb = MultibandCompressor::new(48_000.0);
        let (l, r) = mb.process(0.5, 0.5);
        assert_eq!((l, r), (0.5, 0.5));
    }

    #[test]
    fn neutral_defaults_are_transparent() {
        // The DSP defaults must be transparent: ratio 1:1 → no gain reduction
        // regardless of input level, so enabling the compressor changes nothing.
        let mut mb = MultibandCompressor::new(48_000.0);
        mb.set_enabled(true);
        let mut l = vec![1.0f32; 4800];
        let mut r = vec![1.0f32; 4800];
        mb.process_block(&mut l, &mut r);
        // Skip the crossover startup transient (first ~25%); the steady-state
        // reconstruction of DC must be unity (LP+HP sum = 1 at DC).
        for i in 3600..4800 {
            assert!(
                (l[i] - 1.0).abs() < 1e-3 && (r[i] - 1.0).abs() < 1e-3,
                "neutral default must pass 0 dBFS through unchanged at {i}: l={} r={}",
                l[i],
                r[i]
            );
        }
    }

    #[test]
    fn stereo_link_keeps_image_for_centered_bass() {
        // With stereo linking, a loud signal only on L must reduce R by the
        // same amount — no channel imbalance is introduced by the detector.
        let mut linked = MultibandCompressor::new(48_000.0);
        linked.set_enabled(true);
        linked.set_band_params(0, -20.0, 4.0, 5.0, 100.0, 0.0);

        // Loud signal on L, quiet on R (both inside the low band).
        for _ in 0..2000 {
            let _ = linked.process(0.5, 0.01);
        }
        // Keep driving L loud so the linked envelope stays above threshold.
        let mut max_l = 0.0f32;
        let mut max_r = 0.0f32;
        for _ in 0..2000 {
            let (ol, or_) = linked.process(0.5, 0.01);
            max_l = max_l.max(ol.abs());
            max_r = max_r.max(or_.abs());
        }
        assert!(max_l < 0.4, "linked low band should reduce L: {max_l}");
        // R shares L's envelope via the link, so its gain is reduced as well
        // (well below its 0.01 input) — the detector never introduces an
        // L/R imbalance of its own.
        assert!(
            max_r < 0.01 * 0.8,
            "linked R should share L's gain reduction, got {max_r}"
        );
        assert!(max_r > 0.0, "linked R must not be zeroed");
    }

    #[test]
    fn hard_knee_vs_soft_knee_continuity() {
        // Soft knee must compress gently below threshold and reach the same
        // full-ratio behaviour well above it; hard knee (knee=0) must be
        // exact ratio above threshold.
        let mut hard = BandCompressor::new(
            48_000.0,
            -12.0,
            4.0,
            1.0,
            100.0,
            0.0,
            0.0,
            CompressorDetector::Peak,
            true,
        );
        let mut soft = BandCompressor::new(
            48_000.0,
            -12.0,
            4.0,
            1.0,
            100.0,
            0.0,
            6.0,
            CompressorDetector::Peak,
            true,
        );

        // Feed a constant loud level (well above threshold) to steady state.
        for _ in 0..20000 {
            let _ = hard.process_f64(0.5, 0.5);
            let _ = soft.process_f64(0.5, 0.5);
        }
        let (hl, _) = hard.process_f64(0.5, 0.5);
        let (sl, _) = soft.process_f64(0.5, 0.5);

        // -6 dBFS input, -12 dB threshold, 4:1: reduction = 6·(1-1/4) = 4.5 dB
        // → output ≈ -10.5 dBFS ≈ 0.2985.
        let expected = 10.0_f64.powf((-6.0 - 4.5) / 20.0);
        assert!(
            (hl - expected).abs() < 1e-3,
            "hard knee full-ratio: {hl} vs {expected}"
        );
        // Soft knee asymptotes to the same law far above the knee.
        assert!(
            (sl - expected).abs() < 0.02,
            "soft knee full-ratio: {sl} vs {expected}"
        );

        // Just below the threshold, the soft knee must already be applying a
        // little reduction while the hard knee applies none.
        for _ in 0..20000 {
            let _ = hard.process_f64(0.24, 0.24);
            let _ = soft.process_f64(0.24, 0.24);
        }
        let (hl2, _) = hard.process_f64(0.24, 0.24);
        let (sl2, _) = soft.process_f64(0.24, 0.24);
        // 0.24 ≈ -12.4 dBFS — just below the -12 dB threshold.
        assert!(
            (hl2 - 0.24).abs() < 1e-3,
            "hard knee below threshold: {hl2}"
        );
        assert!(
            sl2 < 0.24,
            "soft knee should start reducing below threshold: {sl2}"
        );
    }

    #[test]
    fn rms_detector_reacts_to_level_not_transients() {
        // Instant attack so the peak detector catches the transient sample;
        // the RMS detector (10 ms window) must not.
        let mut peak = BandCompressor::new(
            48_000.0,
            -20.0,
            4.0,
            0.0001,
            100.0,
            0.0,
            0.0,
            CompressorDetector::Peak,
            true,
        );
        let mut rms = BandCompressor::new(
            48_000.0,
            -20.0,
            4.0,
            0.0001,
            100.0,
            0.0,
            0.0,
            CompressorDetector::Rms,
            true,
        );

        // Warm both to silence.
        for _ in 0..1000 {
            let _ = peak.process_f64(0.0, 0.0);
            let _ = rms.process_f64(0.0, 0.0);
        }

        // A single loud sample (~-0.9 dBFS). The peak detector computes its
        // gain from the instantaneous level and attenuates the sample itself;
        // the RMS detector's window still reads ~silence and passes it.
        let (pl, _) = peak.process_f64(0.9, 0.9);
        let (rl, _) = rms.process_f64(0.9, 0.9);
        assert!(
            pl < 0.9 * 0.5,
            "peak detector must reduce on a transient: {pl}"
        );
        assert!(
            rl > 0.9 * 0.9,
            "RMS detector should barely react to a single transient: {rl}"
        );
    }
}
