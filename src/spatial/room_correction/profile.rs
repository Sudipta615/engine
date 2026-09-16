//! Decoupled correction profile model (§11.2, Item 35).
//!
//! Stores room / output calibration data independently of runtime graph topology,
//! supporting serialization and direct export to `OutputCalibration`.

use serde::{Deserialize, Serialize};

use super::target_curve::TargetCurve;
use crate::output::calibration::{CorrectionIr, OutputCalibration, TargetLayoutKind};
use crate::spatial::math::Vec3;

/// Channel-level correction validation metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelCorrectionMetrics {
    /// Initial RMS error against target curve (dB).
    pub pre_correction_rms_db: f64,
    /// Predicted post-correction RMS error against target curve (dB).
    pub post_correction_rms_db: f64,
    /// Maximum acoustic peak reduction achieved (dB).
    pub peak_cut_db: f64,
    /// Maximum acoustic null boost applied (clamped) (dB).
    pub peak_boost_db: f64,
}

/// Decoupled, topology-independent room/speaker correction profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorrectionProfile {
    pub name: String,
    pub description: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub target_curve: TargetCurve,
    pub channel_fir_filters: Vec<Vec<f32>>,
    pub channel_gain_trims_db: Vec<f32>,
    pub channel_delay_ms: Vec<f32>,
    pub channel_polarity_invert: Vec<bool>,
    pub validation_metrics: Vec<ChannelCorrectionMetrics>,
}

impl CorrectionProfile {
    /// Converts this decoupled correction profile into an active `OutputCalibration` bundle.
    pub fn to_output_calibration(&self, layout_kind: TargetLayoutKind) -> OutputCalibration {
        let n = self.channels;
        let correction_ir = if !self.channel_fir_filters.is_empty() {
            Some(CorrectionIr {
                name: self.name.clone(),
                sample_rate: self.sample_rate,
                channels: self.channel_fir_filters.clone(),
            })
        } else {
            None
        };

        let delays = self
            .channel_delay_ms
            .iter()
            .map(|&ms| ms / 1000.0)
            .collect::<Vec<f32>>();

        let gains = self
            .channel_gain_trims_db
            .iter()
            .map(|&db| 10.0f32.powf(db / 20.0))
            .collect::<Vec<f32>>();

        let speaker_positions = match layout_kind {
            TargetLayoutKind::Stereo => {
                vec![Vec3::new(-1.0, 1.732, 0.0), Vec3::new(1.0, 1.732, 0.0)]
            }
            _ => vec![Vec3::default(); n],
        };

        OutputCalibration {
            layout_kind,
            speaker_positions,
            delays,
            gains,
            polarity: self.channel_polarity_invert.clone(),
            calibration_offsets: self.channel_gain_trims_db.clone(),
            sample_rate: Some(self.sample_rate),
            latency: Some(0.005),
            correction_ir,
        }
    }
}
