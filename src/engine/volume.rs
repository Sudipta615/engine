//! Volume management — linear gain, dB conversion, hardware/software routing.

use super::AudioEngine;

impl AudioEngine {
    /// Whether the active volume path is the endpoint's hardware control.
    pub(crate) fn volume_uses_hardware(&self) -> bool {
        match self.config.volume_mode {
            config::VolumeMode::HardwarePreferred => self
                .audio_output
                .as_ref()
                .is_some_and(|o| o.supports_hardware_volume()),
            config::VolumeMode::HardwareOnly => true,
            _ => false,
        }
    }

    pub fn set_volume(&mut self, vol: f32) {
        let clamped = if vol.is_finite() {
            vol.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if self.graph.is_bit_perfect() {
            self.graph.set_volume(1.0);
            self.write_playback_info(|pb| {
                pb.volume_error = Some(
                    "Bit-Perfect mode: software volume is disabled; use hardware volume or disable Bit-Perfect mode"
                        .to_string(),
                );
                pb.volume_path = None;
            });
            return;
        }
        self.graph.set_volume(clamped);
        self.write_playback_info(|pb| pb.volume = clamped);
    }

    /// Set the volume in dB. Range: [-60.0, 0.0].
    pub fn set_volume_db(&mut self, db: f32) {
        if !db.is_finite() {
            log::warn!(
                "AudioEngine::set_volume_db: non-finite value {}; ignoring",
                db
            );
            return;
        }
        if self.graph.is_bit_perfect() {
            self.graph.set_volume(1.0);
            self.write_playback_info(|pb| {
                pb.volume_error = Some(
                    "Bit-Perfect mode: software volume is disabled; use hardware volume or disable Bit-Perfect mode"
                        .to_string(),
                );
                pb.volume_path = None;
            });
            return;
        }
        self.graph.set_volume_db(db);
        let linear = crate::dsp::pipeline::DspPipeline::volume_db_to_linear(db);
        self.write_playback_info(|pb| pb.volume = linear);
    }

    /// Read the current volume in dB.
    pub fn volume_db(&self) -> f32 {
        self.graph.volume_db()
    }

    /// Convert a UI percentage (0.0–100.0) to a dB value suitable for
    /// `set_volume_db`.
    pub fn volume_percent_to_db(percent: f32) -> f32 {
        crate::dsp::pipeline::DspPipeline::volume_percent_to_db(percent)
    }

    /// Reset the clip and NaN counters in PlaybackInfo.
    pub fn reset_dsp_diagnostics(&mut self) {
        self.write_playback_info(|pb| {
            pb.clip_count = 0;
            pb.nan_count = 0;
        });
    }

    /// Enable or disable true-peak detection on the limiter.
    pub fn set_limiter_true_peak(&mut self, enabled: bool) {
        self.graph.set_limiter_true_peak(enabled);
    }

    /// Whether true-peak detection is currently active on the limiter.
    pub fn limiter_true_peak_enabled(&self) -> bool {
        self.graph.limiter_true_peak_enabled()
    }
}
