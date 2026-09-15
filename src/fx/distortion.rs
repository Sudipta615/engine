//! Nonlinear Distortion & Saturation Processors (Item 32).

use serde::{Deserialize, Serialize};

/// Type of nonlinear waveshaping / saturation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum DistortionType {
    /// Hyperbolic tangent soft saturation.
    #[default]
    SoftClipTanh,
    /// Arctangent smooth progressive saturation.
    SoftClipAtan,
    /// Cubic polynomial soft saturation ($x - x^3 / 3$).
    SoftClipCubic,
    /// Hard clip at ceiling.
    HardClip,
    /// Tape saturation with mild magnetic hysteresis and progressive compression.
    TapeSaturation,
    /// Asymmetric tube saturation with even-harmonic bias.
    TubeSaturation,
    /// Wavefolding (reflects signals exceeding threshold back on themselves).
    Wavefolder,
}

/// Creative saturator and nonlinear waveshaper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saturator {
    pub distortion_type: DistortionType,
    pub drive: f32,
    pub bias: f32,
    pub ceiling: f32,
    pub wet: f32,
    pub dry: f32,
}

impl Default for Saturator {
    fn default() -> Self {
        Self {
            distortion_type: DistortionType::SoftClipTanh,
            drive: 1.0,
            bias: 0.0,
            ceiling: 1.0,
            wet: 1.0,
            dry: 0.0,
        }
    }
}

impl Saturator {
    pub fn new(distortion_type: DistortionType, drive: f32) -> Self {
        Self {
            distortion_type,
            drive: drive.max(0.1),
            bias: 0.0,
            ceiling: 1.0,
            wet: 1.0,
            dry: 0.0,
        }
    }

    /// Process in-place on an audio plane. Guaranteed zero allocation.
    pub fn process_plane(&mut self, plane: &mut [f32]) {
        for sample in plane.iter_mut() {
            let x = *sample;
            let driven = (x * self.drive) + self.bias;

            let shaped = match self.distortion_type {
                DistortionType::SoftClipTanh => driven.tanh(),
                DistortionType::SoftClipAtan => (driven * std::f32::consts::FRAC_PI_2).atan(),
                DistortionType::SoftClipCubic => {
                    let clamped = driven.clamp(-1.5, 1.5);
                    clamped - (clamped * clamped * clamped) / 3.0
                }
                DistortionType::HardClip => driven.clamp(-self.ceiling, self.ceiling),
                DistortionType::TapeSaturation => {
                    // Symmetrical soft compression with gentle shoulder
                    let s = driven / (1.0 + driven.abs());
                    s * 1.2
                }
                DistortionType::TubeSaturation => {
                    // Asymmetrical saturation generating 2nd harmonic warmth
                    if driven >= 0.0 {
                        1.0 - (-driven).exp()
                    } else {
                        -(-driven).tanh()
                    }
                }
                DistortionType::Wavefolder => {
                    // Foldback distortion
                    let mut w = driven;
                    while w.abs() > 1.0 {
                        w = if w > 1.0 { 2.0 - w } else { -2.0 - w };
                    }
                    w
                }
            };

            // Remove DC bias from output and apply ceiling
            let out_unclamped = shaped - self.bias * 0.5;
            let out_shaped = out_unclamped.clamp(-self.ceiling, self.ceiling);

            *sample = x * self.dry + out_shaped * self.wet;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_soft_clip_bounds() {
        let mut sat = Saturator::new(DistortionType::SoftClipTanh, 10.0);
        let mut plane = [-5.0f32, -2.0, 0.0, 2.0, 5.0];

        sat.process_plane(&mut plane);
        for &s in &plane {
            assert!((-1.0001..=1.0001).contains(&s));
        }
    }

    #[test]
    fn test_tube_saturation_asymmetry() {
        let mut tube = Saturator::new(DistortionType::TubeSaturation, 2.0);
        let mut pos = [1.0f32];
        let mut neg = [-1.0f32];

        tube.process_plane(&mut pos);
        tube.process_plane(&mut neg);

        // Asymmetric transfer characteristic
        assert_ne!(pos[0].abs(), neg[0].abs());
    }

    #[test]
    fn test_wavefolder_reflection() {
        let mut wf = Saturator::new(DistortionType::Wavefolder, 1.0);
        let mut plane = [1.5f32]; // exceeds 1.0 -> folds to 2.0 - 1.5 = 0.5

        wf.process_plane(&mut plane);
        assert!((plane[0] - 0.5).abs() < 1e-4);
    }
}
