//! Spatial metering and loudness (spec §70).
//!
//! The spatial mix reports per-speaker peak/RMS, the spatial bus level, and
//! the LFE level. Meters are **channel-aware** — never a stereo assumption —
//! and updated incrementally by the renderers at block rate (allocation-free,
//! lock-free). A host reads a [`SpatialMeters`] snapshot via
//! [`SpatialMeterState::snapshot`] on the control thread.

/// Meter view available to a host (control path).
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialMeters {
    /// Per-speaker peak (linear), 0.0 for speakers the renderer never touches.
    pub speaker_peak: Vec<f32>,
    /// Per-speaker RMS (linear, block-integrated).
    pub speaker_rms: Vec<f32>,
    /// Whole-bus peak (across all speakers).
    pub bus_peak: f32,
    /// Whole-bus RMS (across all speakers).
    pub bus_rms: f32,
    /// LFE peak (the LFE slot if present, else 0.0).
    pub lfe_peak: f32,
    /// LFE RMS.
    pub lfe_rms: f32,
    /// Active voice count last block.
    pub active_voices: usize,
    /// Normalized inter-channel correlation of the first two output channels
    /// (the stereo ears/speakers) over the metering window, `[-1, 1]`;
    /// `1.0` = perfectly phase-coherent, `-1.0` = anti-phase. `0.0` means
    /// unknown (no samples, or a channel silent). Mono output reports `1.0`.
    /// Derived from the cross-energy accumulator on the render path
    /// (allocation-free, opt-in via the meter's `enabled` flag).
    pub stereo_correlation: f32,
}

/// Renderer-owned meter accumulator (realtime-safe).
#[derive(Debug, Clone)]
pub struct SpatialMeterState {
    channels: usize,
    lfe_index: Option<usize>,
    peak: Vec<f32>,
    energy: Vec<f64>,
    /// Sum of `L×R` over the window (first two channels) — the numerator of
    /// the normalized inter-channel correlation. Allocation-free and opt-in
    /// (only accumulated while `enabled`).
    cross_energy: f64,
    samples: u64,
    enabled: bool,
}

impl Default for SpatialMeterState {
    fn default() -> Self {
        Self::new(0)
    }
}

impl SpatialMeterState {
    pub fn new(channels: usize) -> Self {
        Self {
            channels,
            lfe_index: None,
            peak: vec![0.0; channels],
            energy: vec![0.0; channels],
            cross_energy: 0.0,
            samples: 0,
            enabled: true,
        }
    }

    /// Set the (optional) LFE output slot so it is reported separately.
    pub fn set_lfe_index(&mut self, idx: Option<usize>) {
        self.lfe_index = idx;
    }

    /// Enable/disable metering (disabled = zero cost).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Feed one interleaved frame. Allocation-free.
    #[inline]
    pub fn feed_frame(&mut self, frame: &[f32]) {
        if !self.enabled {
            return;
        }
        for (ch, &v) in frame.iter().enumerate().take(self.channels) {
            let a = v.abs();
            if a > self.peak[ch] {
                self.peak[ch] = a;
            }
            self.energy[ch] += (v as f64) * (v as f64);
        }
        if self.channels >= 2 {
            let l = frame.first().copied().unwrap_or(0.0) as f64;
            let r = frame.get(1).copied().unwrap_or(0.0) as f64;
            self.cross_energy += l * r;
        }
        self.samples += 1;
    }

    /// Feed an interleaved block (`frames × channels`). Allocation-free.
    pub fn feed_block(&mut self, interleaved: &[f32], frames: usize) {
        if !self.enabled || self.channels == 0 {
            return;
        }
        let f = frames.min(interleaved.len().checked_div(self.channels).unwrap_or(0));
        for i in 0..f {
            let base = i * self.channels;
            for ch in 0..self.channels {
                let v = interleaved[base + ch].abs();
                if v > self.peak[ch] {
                    self.peak[ch] = v;
                }
                let s = interleaved[base + ch] as f64;
                self.energy[ch] += s * s;
            }
            if self.channels >= 2 {
                let l = interleaved[base] as f64;
                let r = interleaved[base + 1] as f64;
                self.cross_energy += l * r;
            }
        }
        self.samples += f as u64;
    }

    /// Reset the accumulator (start of a metering window).
    pub fn clear(&mut self) {
        self.peak.fill(0.0);
        self.energy.fill(0.0);
        self.cross_energy = 0.0;
        self.samples = 0;
    }

    /// Current snapshot (control path read).
    pub fn snapshot(&self) -> SpatialMeters {
        let n = self.channels;
        let rms_of = |ch: usize| -> f32 {
            if self.samples == 0 {
                0.0
            } else {
                (self.energy[ch] / self.samples as f64).sqrt() as f32
            }
        };
        let bus_peak = self.peak.iter().cloned().fold(0.0f32, f32::max);
        let bus_energy = self.energy.iter().sum::<f64>();
        let bus_rms = if self.samples == 0 {
            0.0
        } else {
            (bus_energy / (self.samples as f64 * n.max(1) as f64)).sqrt() as f32
        };
        let (lfe_peak, lfe_rms) = match self.lfe_index {
            Some(i) if i < n => (self.peak[i], rms_of(i)),
            _ => (0.0, 0.0),
        };
        let stereo_correlation = if self.channels < 2 {
            1.0
        } else if self.samples == 0 {
            0.0
        } else {
            let e0 = self.energy[0];
            let e1 = self.energy[1];
            if e0 <= 0.0 || e1 <= 0.0 {
                0.0
            } else {
                (self.cross_energy / (e0 * e1).sqrt()).clamp(-1.0, 1.0) as f32
            }
        };
        SpatialMeters {
            speaker_peak: self.peak.clone(),
            speaker_rms: (0..n).map(rms_of).collect(),
            bus_peak,
            bus_rms,
            lfe_peak,
            lfe_rms,
            active_voices: 0,
            stereo_correlation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_per_speaker_and_lfe() {
        let mut m = SpatialMeterState::new(6);
        m.set_lfe_index(Some(3));
        // 5.1: FL=1.0, FR=0.0, C=0.5, LFE=0.9, SL=0.0, SR=0.25 over 2 frames.
        let frame0 = [1.0, 0.0, 0.5, 0.9, 0.0, 0.25];
        let frame1 = [0.5, 0.5, 0.5, 0.9, 0.0, 0.25];
        let block: Vec<f32> = frame0.iter().chain(frame1.iter()).cloned().collect();
        m.feed_block(&block, 2);
        let s = m.snapshot();
        assert!((s.speaker_peak[0] - 1.0).abs() < 1e-6);
        assert!((s.speaker_peak[1] - 0.5).abs() < 1e-6);
        assert!((s.bus_peak - 1.0).abs() < 1e-6);
        assert!((s.lfe_peak - 0.9).abs() < 1e-6);
        // LFE RMS of 0.9, 0.9 → 0.9.
        assert!((s.lfe_rms - 0.9).abs() < 1e-6);
        // FL RMS over (1.0, 0.5) → sqrt((1 + .25)/2) ≈ 0.79.
        assert!((s.speaker_rms[0] - 0.790_57).abs() < 1e-3);
    }

    #[test]
    fn disabled_is_zero_cost_and_zero_report() {
        let mut m = SpatialMeterState::new(2);
        m.set_enabled(false);
        m.feed_block(&[1.0, 1.0], 1);
        let s = m.snapshot();
        assert_eq!(s.bus_peak, 0.0);
    }

    #[test]
    fn clear_resets() {
        let mut m = SpatialMeterState::new(1);
        m.feed_block(&[0.8, 0.9], 2);
        m.clear();
        let s = m.snapshot();
        assert_eq!(s.speaker_peak[0], 0.0);
        assert_eq!(s.bus_rms, 0.0);
    }

    #[test]
    fn correlation_tracks_inter_channel_phase() {
        // Perfectly correlated mono-ish stereo: L == R → ρ = 1.
        let mut m = SpatialMeterState::new(2);
        m.feed_block(&[0.5, 0.5, 1.0, 1.0], 2);
        let s = m.snapshot();
        assert!((s.stereo_correlation - 1.0).abs() < 1e-6);

        // Anti-phase: L = +x, R = −x → ρ = −1.
        let mut m = SpatialMeterState::new(2);
        m.feed_block(&[0.5, -0.5, 1.0, -1.0], 2);
        let s = m.snapshot();
        assert!((s.stereo_correlation + 1.0).abs() < 1e-6);

        // Uncorrelated: L = sin-ish, R = cos-ish over a window → ρ ≈ 0.
        let mut m = SpatialMeterState::new(2);
        for i in 0..256 {
            let t = i as f32 * std::f32::consts::TAU / 256.0;
            let frame = [t.sin() * 0.5, t.cos() * 0.5];
            m.feed_frame(&frame);
        }
        let s = m.snapshot();
        assert!(
            s.stereo_correlation.abs() < 0.05,
            "ρ = {}",
            s.stereo_correlation
        );

        // One silent channel → unknown (0.0).
        let mut m = SpatialMeterState::new(2);
        m.feed_block(&[0.5, 0.0], 1);
        assert_eq!(m.snapshot().stereo_correlation, 0.0);

        // Mono meter reports perfect correlation (no phase risk).
        let mut m = SpatialMeterState::new(1);
        m.feed_block(&[0.9, 0.4], 2);
        assert_eq!(m.snapshot().stereo_correlation, 1.0);
    }
}
