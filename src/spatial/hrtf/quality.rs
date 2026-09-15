//! Multi-tap HRTF quality tiers and convolution strategies (spec §47–48, §62).

/// Maximum supported HRTF impulse-response tap length.
pub const MAX_HRTF_TAPS: usize = 2048;

/// Default HRTF impulse-response tap length (medium tier).
pub const DEFAULT_HRTF_TAPS: usize = 128;

/// Convolution strategy selected based on tap count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HrtfConvStrategy {
    /// Direct time-domain FIR convolution (low latency, optimal for <= 256 taps).
    #[default]
    DirectFir,
    /// Partitioned overlap-add frequency domain convolution (optimal for > 256 taps).
    PartitionedOa,
}

/// HRTF rendering quality tier controlling tap length and convolution strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HrtfQualityMode {
    /// 64 taps: lowest CPU overhead, suitable for low-power devices.
    Low64,
    /// 128 taps: standard audiophile baseline, full spectral pinna cues.
    #[default]
    Medium128,
    /// 512 taps: enhanced diffuse-field response and torso reflections.
    High512,
    /// 2048 taps: ultra-high fidelity, full ear-canal and room early-reflection detail.
    Ultra2048,
}

impl HrtfQualityMode {
    /// Number of FIR taps for this quality mode.
    #[inline]
    pub const fn taps(self) -> usize {
        match self {
            Self::Low64 => 64,
            Self::Medium128 => 128,
            Self::High512 => 512,
            Self::Ultra2048 => 2048,
        }
    }

    /// Convolution strategy recommended for this quality mode.
    #[inline]
    pub const fn conv_strategy(self) -> HrtfConvStrategy {
        if self.taps() <= 256 {
            HrtfConvStrategy::DirectFir
        } else {
            HrtfConvStrategy::PartitionedOa
        }
    }

    /// Select the closest quality tier for a requested tap count.
    pub fn from_taps(taps: usize) -> Self {
        if taps <= 64 {
            Self::Low64
        } else if taps <= 128 {
            Self::Medium128
        } else if taps <= 512 {
            Self::High512
        } else {
            Self::Ultra2048
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_mode_taps_and_strategies() {
        assert_eq!(HrtfQualityMode::Low64.taps(), 64);
        assert_eq!(
            HrtfQualityMode::Low64.conv_strategy(),
            HrtfConvStrategy::DirectFir
        );

        assert_eq!(HrtfQualityMode::Medium128.taps(), 128);
        assert_eq!(
            HrtfQualityMode::Medium128.conv_strategy(),
            HrtfConvStrategy::DirectFir
        );

        assert_eq!(HrtfQualityMode::High512.taps(), 512);
        assert_eq!(
            HrtfQualityMode::High512.conv_strategy(),
            HrtfConvStrategy::PartitionedOa
        );

        assert_eq!(HrtfQualityMode::Ultra2048.taps(), 2048);
        assert_eq!(
            HrtfQualityMode::Ultra2048.conv_strategy(),
            HrtfConvStrategy::PartitionedOa
        );
    }

    #[test]
    fn from_taps_mapping() {
        assert_eq!(HrtfQualityMode::from_taps(32), HrtfQualityMode::Low64);
        assert_eq!(HrtfQualityMode::from_taps(64), HrtfQualityMode::Low64);
        assert_eq!(HrtfQualityMode::from_taps(96), HrtfQualityMode::Medium128);
        assert_eq!(HrtfQualityMode::from_taps(128), HrtfQualityMode::Medium128);
        assert_eq!(HrtfQualityMode::from_taps(256), HrtfQualityMode::High512);
        assert_eq!(HrtfQualityMode::from_taps(512), HrtfQualityMode::High512);
        assert_eq!(HrtfQualityMode::from_taps(1024), HrtfQualityMode::Ultra2048);
        assert_eq!(HrtfQualityMode::from_taps(2048), HrtfQualityMode::Ultra2048);
    }
}
