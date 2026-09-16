//! Output Profile Calibration (§10.5, Item 28).
//!
//! Provides acoustic calibration, speaker geometry alignment, delay compensation,
//! gain trimming, and polarity management for output monitoring profiles.
//!
//! # Standard Layout Presets
//! - Stereo (Nearfield / Midfield monitors)
//! - Headphones (Binaural reference)
//! - 5.1 Surround (ITU-R BS.775)
//! - 7.1 Surround (ITU-R BS.775)
//! - 7.1.4 Immersive (ITU-R BS.2051 System J)
//! - 9.1.6 Immersive (ITU-R BS.2051 System H)
//! - Custom speaker arrays
//! - Binaural headphone rendering

use crate::spatial::math::Vec3;
use serde::{Deserialize, Serialize};

/// Reproduction target layout classification (§10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetLayoutKind {
    /// 2-channel stereo loudspeaker pair.
    #[default]
    Stereo,
    /// Direct headphone presentation.
    Headphones,
    /// 5.1 ITU-R BS.775 surround sound.
    Surround5_1,
    /// 7.1 surround sound.
    Surround7_1,
    /// 7.1.4 3D immersive audio (4 height speakers).
    Immersive7_1_4,
    /// 9.1.6 3D immersive audio (6 height speakers).
    Immersive9_1_6,
    /// Dedicated binaural headphone monitoring.
    Binaural,
    /// Arbitrary custom loudspeaker installation.
    CustomArray,
}

/// Correction impulse response for room or headphone acoustic compensation.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CorrectionIr {
    /// Identifier or file path.
    pub name: String,
    /// Sampling rate of the stored impulse response.
    pub sample_rate: u32,
    /// Per-channel impulse response FIR taps.
    pub channels: Vec<Vec<f32>>,
}

/// Comprehensive per-output calibration bundle (§10.5).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OutputCalibration {
    /// Target layout classification.
    pub layout_kind: TargetLayoutKind,
    /// 3D speaker coordinates relative to listening sweet-spot (metres).
    pub speaker_positions: Vec<Vec3>,
    /// Per-channel acoustic delay compensation in seconds.
    pub delays: Vec<f32>,
    /// Per-channel level trim gain (linear, 1.0 = unity).
    pub gains: Vec<f32>,
    /// Per-channel polarity inversion (true = 180° inverted phase).
    pub polarity: Vec<bool>,
    /// Per-channel fine calibration offsets in dB.
    pub calibration_offsets: Vec<f32>,
    /// Calibrated target sample rate in Hz.
    pub sample_rate: Option<u32>,
    /// Measured hardware and pipeline output latency in seconds.
    pub latency: Option<f64>,
    /// Acoustic room or headphone correction filter IR.
    pub correction_ir: Option<CorrectionIr>,
}

impl OutputCalibration {
    /// Create calibrated stereo nearfield monitor preset.
    pub fn stereo(sample_rate: u32) -> Self {
        Self {
            layout_kind: TargetLayoutKind::Stereo,
            speaker_positions: vec![
                Vec3::new(-1.0, 1.732, 0.0), // Left (-30°)
                Vec3::new(1.0, 1.732, 0.0),  // Right (+30°)
            ],
            delays: vec![0.0; 2],
            gains: vec![1.0; 2],
            polarity: vec![false; 2],
            calibration_offsets: vec![0.0; 2],
            sample_rate: Some(sample_rate),
            latency: Some(0.005),
            correction_ir: None,
        }
    }

    /// Create calibrated headphone monitoring preset.
    pub fn headphones(sample_rate: u32) -> Self {
        Self {
            layout_kind: TargetLayoutKind::Headphones,
            speaker_positions: vec![
                Vec3::new(-0.1, 0.0, 0.0), // Left ear
                Vec3::new(0.1, 0.0, 0.0),  // Right ear
            ],
            delays: vec![0.0; 2],
            gains: vec![1.0; 2],
            polarity: vec![false; 2],
            calibration_offsets: vec![0.0; 2],
            sample_rate: Some(sample_rate),
            latency: Some(0.002),
            correction_ir: None,
        }
    }

    /// Create calibrated 5.1 surround monitoring preset (ITU-R BS.775).
    pub fn surround_5_1(sample_rate: u32) -> Self {
        Self {
            layout_kind: TargetLayoutKind::Surround5_1,
            speaker_positions: vec![
                Vec3::new(-1.0, 1.732, 0.0),  // L (-30°)
                Vec3::new(1.0, 1.732, 0.0),   // R (+30°)
                Vec3::new(0.0, 2.0, 0.0),     // C (0°)
                Vec3::new(0.0, 1.0, -0.5),    // LFE
                Vec3::new(-1.732, -1.0, 0.0), // Ls (-110°)
                Vec3::new(1.732, -1.0, 0.0),  // Rs (+110°)
            ],
            delays: vec![0.0; 6],
            gains: vec![1.0; 6],
            polarity: vec![false; 6],
            calibration_offsets: vec![0.0; 6],
            sample_rate: Some(sample_rate),
            latency: Some(0.008),
            correction_ir: None,
        }
    }

    /// Create calibrated 7.1 surround monitoring preset.
    pub fn surround_7_1(sample_rate: u32) -> Self {
        Self {
            layout_kind: TargetLayoutKind::Surround7_1,
            speaker_positions: vec![
                Vec3::new(-1.0, 1.732, 0.0),  // L
                Vec3::new(1.0, 1.732, 0.0),   // R
                Vec3::new(0.0, 2.0, 0.0),     // C
                Vec3::new(0.0, 1.0, -0.5),    // LFE
                Vec3::new(-2.0, 0.0, 0.0),    // Lss (-90°)
                Vec3::new(2.0, 0.0, 0.0),     // Rss (+90°)
                Vec3::new(-1.0, -1.732, 0.0), // Lsr (-150°)
                Vec3::new(1.0, -1.732, 0.0),  // Rsr (+150°)
            ],
            delays: vec![0.0; 8],
            gains: vec![1.0; 8],
            polarity: vec![false; 8],
            calibration_offsets: vec![0.0; 8],
            sample_rate: Some(sample_rate),
            latency: Some(0.008),
            correction_ir: None,
        }
    }

    /// Create calibrated 7.1.4 immersive monitoring preset (ITU-R BS.2051 System J).
    pub fn immersive_7_1_4(sample_rate: u32) -> Self {
        let mut base = Self::surround_7_1(sample_rate);
        base.layout_kind = TargetLayoutKind::Immersive7_1_4;
        base.speaker_positions.extend_from_slice(&[
            Vec3::new(-1.0, 1.0, 1.414),  // Top Front Left (TFL)
            Vec3::new(1.0, 1.0, 1.414),   // Top Front Right (TFR)
            Vec3::new(-1.0, -1.0, 1.414), // Top Rear Left (TRL)
            Vec3::new(1.0, -1.0, 1.414),  // Top Rear Right (TRR)
        ]);
        base.delays.resize(12, 0.0);
        base.gains.resize(12, 1.0);
        base.polarity.resize(12, false);
        base.calibration_offsets.resize(12, 0.0);
        base
    }

    /// Create calibrated 9.1.6 immersive monitoring preset (ITU-R BS.2051 System H).
    pub fn immersive_9_1_6(sample_rate: u32) -> Self {
        let mut base = Self::immersive_7_1_4(sample_rate);
        base.layout_kind = TargetLayoutKind::Immersive9_1_6;
        // Add Left/Right Wide and Top Middle Left/Right
        base.speaker_positions.extend_from_slice(&[
            Vec3::new(-1.732, 1.0, 0.0), // Left Wide (LW, -60°)
            Vec3::new(1.732, 1.0, 0.0),  // Right Wide (RW, +60°)
            Vec3::new(-1.0, 0.0, 1.414), // Top Middle Left (TML)
            Vec3::new(1.0, 0.0, 1.414),  // Top Middle Right (TMR)
        ]);
        base.delays.resize(16, 0.0);
        base.gains.resize(16, 1.0);
        base.polarity.resize(16, false);
        base.calibration_offsets.resize(16, 0.0);
        base
    }

    /// Create calibrated binaural headphone preset.
    pub fn binaural(sample_rate: u32) -> Self {
        let mut p = Self::headphones(sample_rate);
        p.layout_kind = TargetLayoutKind::Binaural;
        p
    }

    /// Apply calibration trims and polarity inversion in-place across planar block.
    pub fn apply_calibration_block(&self, planes: &mut [&mut [f32]]) {
        let channels = planes.len().min(self.gains.len());
        for (ch, plane) in planes.iter_mut().enumerate().take(channels) {
            let gain = self.gains[ch];
            let inverted = if ch < self.polarity.len() {
                self.polarity[ch]
            } else {
                false
            };
            let effective_gain = if inverted { -gain } else { gain };

            if (effective_gain - 1.0).abs() > 1e-6 {
                for sample in plane.iter_mut() {
                    *sample *= effective_gain;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_presets_geometry() {
        let stereo = OutputCalibration::stereo(48000);
        assert_eq!(stereo.speaker_positions.len(), 2);
        assert_eq!(stereo.layout_kind, TargetLayoutKind::Stereo);

        let surround51 = OutputCalibration::surround_5_1(48000);
        assert_eq!(surround51.speaker_positions.len(), 6);

        let imm714 = OutputCalibration::immersive_7_1_4(48000);
        assert_eq!(imm714.speaker_positions.len(), 12);

        let imm916 = OutputCalibration::immersive_9_1_6(48000);
        assert_eq!(imm916.speaker_positions.len(), 16);
    }

    #[test]
    fn calibration_apply_gain_and_polarity() {
        let mut cal = OutputCalibration::stereo(48000);
        cal.gains = vec![0.5, 0.8];
        cal.polarity = vec![true, false]; // Invert left channel

        let mut l = vec![1.0f32; 4];
        let mut r = vec![1.0f32; 4];
        let mut planes: [&mut [f32]; 2] = [&mut l, &mut r];

        cal.apply_calibration_block(&mut planes);

        assert_eq!(l, vec![-0.5; 4]);
        assert_eq!(r, vec![0.8; 4]);
    }
}
