//! DSP command handlers — stereo width, balance, dither, crossfeed,
//! compressor, limiter, bit-perfect, precision, crossfade, resampler quality.

use log::info;

use super::super::{AudioEngine, PlaybackStream};
use crate::dsp::pipeline::VolumePath;

impl AudioEngine {
    pub(super) fn handle_set_stereo_width(&mut self, width: f32) {
        self.pipeline.set_stereo_width(width);
    }

    pub(super) fn handle_set_balance(&mut self, balance: f32) {
        self.pipeline.set_balance(balance);
    }

    pub(super) fn handle_set_dither_enabled(&mut self, enabled: bool) {
        if let Some(ref output) = self.audio_output {
            output.set_dither_enabled(enabled && !self.dsd.dop_active);
        }
        self.config.dither_enabled = enabled;
    }

    pub(super) fn handle_set_crossfeed_enabled(&mut self, enabled: bool) {
        self.pipeline.set_crossfeed_enabled(enabled);
    }

    pub(super) fn handle_set_crossfeed_profile(&mut self, profile: config::CrossfeedProfile) {
        self.pipeline.set_crossfeed_profile(profile);
    }

    pub(super) fn handle_set_crossfeed_custom_params(
        &mut self,
        frequency_hz: f32,
        q: f32,
        delay_ms: f32,
    ) {
        self.pipeline
            .set_crossfeed_custom_params(frequency_hz, q, delay_ms);
    }

    pub(super) fn handle_set_compressor_enabled(&mut self, enabled: bool) {
        self.pipeline.set_compressor_enabled(enabled);
    }

    pub(super) fn handle_set_compressor_band_params(
        &mut self,
        band: usize,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    ) {
        self.pipeline.set_compressor_band_params(
            band,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_gain_db,
        );
    }

    pub(super) fn handle_set_precision_mode(&mut self, mode: crate::dsp::pipeline::PrecisionMode) {
        info!("DSP precision mode set to {:?}", mode);
        self.pipeline.set_precision_mode(mode);
    }

    pub(super) fn handle_set_bit_perfect(&mut self, enabled: bool) {
        info!("Bit-perfect mode: {}", if enabled { "on" } else { "off" });
        self.pipeline.set_bit_perfect(enabled);
        if enabled {
            self.pipeline.set_volume(1.0);
            self.pipeline.seek_fade.reset();
            let uses_hardware = self.volume_uses_hardware();
            self.write_playback_info(|pb| {
                pb.volume_path = if uses_hardware {
                    Some(VolumePath::Hardware)
                } else {
                    None
                };
                pb.volume_error = if uses_hardware {
                    None
                } else {
                    Some(
                        "Bit-Perfect mode: software volume disabled; hardware volume unavailable"
                            .to_string(),
                    )
                };
                pb.bit_perfect = true;
            });
        } else {
            self.write_playback_info(|pb| {
                pb.bit_perfect = false;
                pb.volume_path = None;
                pb.volume_error = None;
            });
        }
    }

    pub(super) fn handle_set_limiter_mode(&mut self, mode: crate::dsp::limiter::LimiterMode) {
        info!("Limiter mode set to {:?}", mode);
        self.pipeline.set_limiter_mode(mode);
    }

    pub(super) fn handle_set_limiter_true_peak(&mut self, enabled: bool) {
        info!(
            "Limiter true-peak FIR: {}",
            if enabled { "enabled" } else { "disabled" }
        );
        self.pipeline.set_limiter_true_peak(enabled);
    }

    pub(super) fn handle_set_resampler_quality(&mut self, quality: config::ResamplerQuality) {
        self.config.resampler_quality = quality;
        #[cfg(feature = "resample")]
        match &mut self.stream {
            Some(PlaybackStream::Single {
                resampler: Some(ref mut r),
                ..
            }) => {
                r.set_quality(quality);
            }
            Some(PlaybackStream::Transitioning {
                outgoing_resampler,
                incoming_resampler,
                ..
            }) => {
                if let Some(ref mut r) = outgoing_resampler {
                    r.set_quality(quality);
                }
                if let Some(ref mut r) = incoming_resampler {
                    r.set_quality(quality);
                }
            }
            _ => {}
        }
        info!("Resampler quality set to {:?}", quality);
    }

    pub(super) fn handle_set_crossfade_config(&mut self, cfg: config::CrossfadeConfig) {
        info!(
            "Crossfade config updated: enabled={}, duration={}ms, curve={:?}",
            cfg.enabled, cfg.duration_ms, cfg.curve
        );
        self.config.crossfade = cfg.clone();
        self.pipeline.mixer.set_curve(cfg.curve.into());
        self.pipeline
            .mixer
            .set_duration_ms(cfg.duration_ms, self.output_sample_rate as f32);
        self.pipeline.mixer.set_enabled(cfg.enabled);
    }

    pub(super) fn handle_set_crossfade_curve(&mut self, curve: config::CrossfadeCurve) {
        info!("Crossfade curve set to {:?}", curve);
        self.config.crossfade.curve = curve;
        self.pipeline.mixer.set_curve(curve.into());
    }

    pub(super) fn handle_set_transition_mode(&mut self, mode: config::TransitionMode) {
        info!("Transition mode set to {:?}", mode);
        self.config.transition_mode = mode;
    }

    pub(super) fn handle_set_speed_mode(&mut self, mode: config::SpeedMode) {
        info!("Speed mode set to {:?}", mode);
        self.config.speed_mode = mode;
        let current_speed = self.speed;
        self.handle_set_speed(current_speed);
    }
}
