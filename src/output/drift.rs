//! Robust adaptive endpoint clock-drift correction / ASRC controller (Item 27).
//!
//! Independent audio output devices drift relative to the master audio clock
//! (crystal tolerances typically span 10–100 ppm). Without continuous drift
//! correction, endpoint ring buffers slowly fill or drain, eventually resulting
//! in audible buffer under-runs or drops.
//!
//! This module implements a robust, adaptive clock-error feedback controller:
//!
//! ```text
//!    Ring Fill
//!       │
//!       ▼
//!   Buffer Error  ──▶  2-Pole Jitter Filter  ──▶  PI Controller (Anti-Windup)
//!       │                                                  │
//!   Loss-of-Clock                                          ▼
//!     Detector    ─────────────────────────────▶   Slew-Rate Limiter (PPM)
//!                                                          │
//!                                                          ▼
//!                                                 Fractional Slip Ratio
//! ```
//!
//! ### Features
//! - **PPM-level resolution**: Sub-ppm fine ratio tuning.
//! - **Dual-pole jitter rejection**: Low-pass filters block chunk jitter and transport burstiness.
//! - **PI Loop with anti-windup**: Proportional tracking plus bounded integral correction to drive steady-state fill error to zero.
//! - **Slew-rate limiting**: Enforces maximum change in ratio per update ($\Delta \text{ratio}/\Delta t$), preventing audible pitch modulation or phase flutter.
//! - **Loss-of-clock detector**: Identifies stalled rings or disconnected endpoints and gently relaxes the ratio back to nominal 1.0.
//! - **Lock telemetry**: Reports instantaneous error, smoothed PPM offset, and lock acquisition status.

/// Maximum drift correction applied to the endpoint resampler ratio, as a
/// fraction of the nominal rate (500 ppm). Bounds the retune so a broken
/// device or a stuck ring can never drive the rate wildly off.
pub const MAX_DRIFT_RATIO: f64 = 0.0005;

/// Default proportional gain of the drift controller, per frame of low-passed ring
/// fill error.
pub const DEFAULT_DRIFT_P_GAIN: f64 = 1.5e-6;

/// Default integral gain of the drift controller for zero steady-state error.
pub const DEFAULT_DRIFT_I_GAIN: f64 = 5.0e-10;

/// Default maximum ratio slew allowed per update push (≈ 2 ppm per push).
pub const DEFAULT_MAX_SLEW_PER_PUSH: f64 = 2.0e-6;

/// Default loss-of-clock threshold (consecutive identical fill readings before stall flag).
pub const DEFAULT_STALL_COUNT_THRESHOLD: usize = 300;

/// Ring-fill feedback controller that steers a per-endpoint resampler/slip
/// to the device's actual clock instead of its nominal rate.
#[derive(Debug, Clone)]
pub struct DriftController {
    enabled: bool,
    /// Ring capacity in stereo frames.
    capacity: f64,
    /// 1st stage low-passed fill error (frames).
    error_lp1: f64,
    /// 2nd stage low-passed fill error (frames) for high-order jitter rejection.
    error_lp2: f64,
    /// Integrated error with anti-windup (frames * pushes).
    error_integral: f64,
    /// Maximum integral term contribution to ratio.
    max_integral_ratio: f64,
    /// Current applied slip ratio (≈ 1.0).
    ratio: f64,
    /// Proportional gain.
    p_gain: f64,
    /// Integral gain.
    i_gain: f64,
    /// Maximum ratio change per update push.
    slew_limit: f64,
    /// Last observed fill level for loss-of-clock detection.
    last_fill: usize,
    /// Count of consecutive identical fill levels.
    stalled_updates: usize,
    /// Loss-of-clock detection threshold (consecutive identical fill updates). None if disabled.
    loss_of_clock_threshold: Option<usize>,
    /// Loss-of-clock detected flag.
    loss_of_clock: bool,
    /// Number of total updates processed.
    update_count: u64,
}

impl DriftController {
    /// Create a new drift controller with default gains.
    pub fn new(enabled: bool, capacity_frames: usize) -> Self {
        let cap = capacity_frames.max(1) as f64;
        Self {
            enabled,
            capacity: cap,
            error_lp1: 0.0,
            error_lp2: 0.0,
            error_integral: 0.0,
            max_integral_ratio: 0.00015, // 150 ppm max integral authority
            ratio: 1.0,
            p_gain: DEFAULT_DRIFT_P_GAIN,
            i_gain: 0.0, // Default 0.0 for pure proportional compatibility unless configured
            slew_limit: DEFAULT_MAX_SLEW_PER_PUSH,
            last_fill: usize::MAX,
            stalled_updates: 0,
            loss_of_clock_threshold: None,
            loss_of_clock: false,
            update_count: 0,
        }
    }

    /// Create an adaptive PI drift controller with jitter rejection, integral trim, and slew limiting.
    pub fn new_adaptive(enabled: bool, capacity_frames: usize) -> Self {
        let mut ctrl = Self::new(enabled, capacity_frames);
        ctrl.i_gain = DEFAULT_DRIFT_I_GAIN;
        ctrl.loss_of_clock_threshold = Some(DEFAULT_STALL_COUNT_THRESHOLD);
        ctrl
    }

    /// Enable or disable loss-of-clock detection.
    pub fn set_loss_of_clock_detection(&mut self, threshold: Option<usize>) {
        self.loss_of_clock_threshold = threshold;
        if threshold.is_none() {
            self.loss_of_clock = false;
            self.stalled_updates = 0;
        }
    }

    /// Configure proportional and integral gains.
    pub fn set_pi_gains(&mut self, p_gain: f64, i_gain: f64) {
        self.p_gain = p_gain.max(0.0);
        self.i_gain = i_gain.max(0.0);
    }

    /// Set maximum slew rate per update block (in ratio units).
    pub fn set_slew_limit(&mut self, max_slew_per_push: f64) {
        self.slew_limit = max_slew_per_push.max(1e-9);
    }

    /// Sample the ring fill (stereo frames waiting for the device) and
    /// update the resampler ratio. Deterministic and allocation-free.
    pub fn update(&mut self, fill: usize) {
        if !self.enabled {
            return;
        }

        self.update_count = self.update_count.wrapping_add(1);

        // Loss-of-clock check if enabled: identical fill over many blocks indicates endpoint stall
        if let Some(threshold) = self.loss_of_clock_threshold {
            if fill == self.last_fill {
                self.stalled_updates = self.stalled_updates.saturating_add(1);
                if self.stalled_updates >= threshold {
                    self.loss_of_clock = true;
                }
            } else {
                self.stalled_updates = 0;
                self.loss_of_clock = false;
                self.last_fill = fill;
            }
        }

        // Target midpoint error (positive = ring fuller than midpoint)
        let raw_error = fill as f64 - self.capacity * 0.5;

        // Dual-stage cascaded lowpass filter for jitter attenuation
        // First stage: tracking alpha = 0.01 (matches original single pole convergence)
        self.error_lp1 += 0.01 * (raw_error - self.error_lp1);
        // Second stage: smoothing alpha = 0.05
        self.error_lp2 += 0.05 * (self.error_lp1 - self.error_lp2);

        let effective_error = if self.i_gain > 0.0 {
            self.error_lp2
        } else {
            self.error_lp1
        };

        if self.loss_of_clock {
            // Under loss-of-clock, gently decay integral and slew back towards nominal 1.0
            self.error_integral *= 0.98;
            let target = 1.0;
            let diff = target - self.ratio;
            self.ratio += diff.clamp(-self.slew_limit, self.slew_limit);
            return;
        }

        // Integrate error with anti-windup clamping
        if self.i_gain > 0.0 {
            self.error_integral += effective_error;
            let max_integral_frames = self.max_integral_ratio / self.i_gain;
            self.error_integral = self
                .error_integral
                .clamp(-max_integral_frames, max_integral_frames);
        }

        // PI control law: positive error -> slower resampler (ratio < 1)
        let p_term = self.p_gain * effective_error;
        let i_term = self.i_gain * self.error_integral;
        let target_correction = p_term + i_term;

        let target_ratio = 1.0 - target_correction;
        let clamped_target = target_ratio.clamp(1.0 - MAX_DRIFT_RATIO, 1.0 + MAX_DRIFT_RATIO);

        // Apply slew rate limiting
        let slew = clamped_target - self.ratio;
        self.ratio += slew.clamp(-self.slew_limit, self.slew_limit);
    }

    /// Current slip ratio to apply to the resampler.
    #[inline]
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Current drift offset in ppm (positive = device consumes faster than nominal).
    #[inline]
    pub fn offset_ppm(&self) -> i64 {
        ((self.ratio - 1.0) * 1e6).round() as i64
    }

    /// High-resolution smoothed drift offset in ppm.
    #[inline]
    pub fn smoothed_ppm(&self) -> f64 {
        (self.ratio - 1.0) * 1e6
    }

    /// Whether drift correction is enabled and active.
    #[inline]
    pub fn active(&self) -> bool {
        self.enabled
    }

    /// Returns `true` if the ring fill error has settled within tolerance (±2% of capacity).
    pub fn is_locked(&self) -> bool {
        if !self.enabled || self.loss_of_clock {
            return false;
        }
        let tol = self.capacity * 0.02;
        self.error_lp1.abs() < tol
    }

    /// Returns `true` if loss-of-clock / stall condition is detected.
    #[inline]
    pub fn is_loss_of_clock(&self) -> bool {
        self.loss_of_clock
    }

    /// Enable or disable drift correction. Disabling resets the ratio back to 1.0.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset();
        }
    }

    /// Reset internal filter and integrator states back to nominal 1.0.
    pub fn reset(&mut self) {
        self.ratio = 1.0;
        self.error_lp1 = 0.0;
        self.error_lp2 = 0.0;
        self.error_integral = 0.0;
        self.stalled_updates = 0;
        self.loss_of_clock = false;
        self.last_fill = usize::MAX;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_controller_disabled_never_moves() {
        let mut ctrl = DriftController::new(false, 8192);
        for _ in 0..100 {
            ctrl.update(7000);
        }
        assert_eq!(ctrl.ratio(), 1.0, "disabled: ratio stays nominal");
        assert_eq!(ctrl.offset_ppm(), 0);
        assert!(!ctrl.active());
    }

    #[test]
    fn drift_controller_slow_device_lowers_ratio() {
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(7000);
        }
        assert!(ctrl.ratio() < 1.0, "slow device → ratio below 1");
        assert!(ctrl.offset_ppm() < 0, "offset must be negative");
    }

    #[test]
    fn drift_controller_fast_device_raises_ratio() {
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(1000);
        }
        assert!(ctrl.ratio() > 1.0, "fast device → ratio above 1");
        assert!(ctrl.offset_ppm() > 0, "offset must be positive");
    }

    #[test]
    fn drift_controller_proportional_law() {
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(4200); // error 104 frames
        }
        let expect_ppm = -(DEFAULT_DRIFT_P_GAIN * 104.0 * 1e6).round() as i64;
        assert!(
            (ctrl.offset_ppm() - expect_ppm).abs() <= 2,
            "offset {} ≈ {expect_ppm} ppm (ratio {})",
            ctrl.offset_ppm(),
            ctrl.ratio()
        );

        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(4096);
        }
        assert_eq!(ctrl.ratio(), 1.0, "midpoint: ratio stays 1.0");
        assert_eq!(ctrl.offset_ppm(), 0);
    }

    #[test]
    fn drift_controller_clamps_at_max_drift() {
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(8192);
        }
        let max_ppm = (MAX_DRIFT_RATIO * 1e6).round() as i64;
        assert_eq!(ctrl.offset_ppm(), -max_ppm, "clamped at −500 ppm");

        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(0);
        }
        assert_eq!(ctrl.offset_ppm(), max_ppm, "clamped at +500 ppm");
    }

    #[test]
    fn drift_controller_enable_toggle_resets_state() {
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(7000);
        }
        assert_ne!(ctrl.offset_ppm(), 0);
        ctrl.set_enabled(false);
        assert_eq!(ctrl.ratio(), 1.0, "disable resets the ratio");
        assert_eq!(ctrl.offset_ppm(), 0, "disable resets the offset");
        ctrl.update(7000);
        assert_eq!(ctrl.ratio(), 1.0, "disabled: no movement");
        ctrl.set_enabled(true);
        for _ in 0..2000 {
            ctrl.update(7000);
        }
        assert_ne!(ctrl.offset_ppm(), 0);
    }

    #[test]
    fn drift_controller_slew_rate_limiting() {
        let mut ctrl = DriftController::new(true, 8192);
        ctrl.set_slew_limit(1.0e-6); // 1 ppm per update max
        ctrl.update(8192);
        // Single update cannot jump more than 1 ppm
        assert!(ctrl.smoothed_ppm().abs() <= 1.01);
    }

    #[test]
    fn drift_controller_loss_of_clock_detection() {
        let mut ctrl = DriftController::new_adaptive(true, 8192);
        for _ in 0..100 {
            ctrl.update(6000);
        }
        assert!(!ctrl.is_loss_of_clock());
        // Simulate endpoint stall: 350 identical fill readings
        for _ in 0..350 {
            ctrl.update(6000);
        }
        assert!(ctrl.is_loss_of_clock());
        // When active data returns with different fill
        ctrl.update(6001);
        assert!(!ctrl.is_loss_of_clock());
    }

    #[test]
    fn drift_controller_adaptive_pi_lock() {
        let mut ctrl = DriftController::new_adaptive(true, 8192);
        // Realistic audio operation: fill varies by ±1-2 frames around midpoint
        for i in 0..500 {
            let jitter = (i % 3) as usize;
            ctrl.update(4095 + jitter);
        }
        assert!(ctrl.is_locked());
    }
}
