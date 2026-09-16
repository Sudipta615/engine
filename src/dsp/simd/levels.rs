//! Formalized SIMD feature levels and runtime hardware detection (§8.2, Item 29).

use serde::{Deserialize, Serialize};

/// Supported SIMD execution tiers across x86_64, aarch64, and scalar architectures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimdLevel {
    /// Pure Rust fallback; guaranteed on 100% of platforms and CPU models.
    Scalar,
    /// 128-bit SSE2 vector extensions on x86_64 (4 x f32, 2 x f64).
    Sse2,
    /// 128-bit NEON vector extensions on aarch64 (4 x f32, 2 x f64).
    Neon,
    /// 256-bit AVX2 / FMA3 vector extensions on x86_64 (8 x f32, 4 x f64).
    Avx2,
    /// 512-bit AVX-512 Foundation vector extensions on x86_64 (16 x f32, 8 x f64).
    Avx512,
}

impl SimdLevel {
    /// Detect the highest SIMD execution level supported by the running CPU.
    ///
    /// The dispatch hierarchy follows Guide §8.2:
    /// `AVX-512` → `AVX2/FMA` → `SSE2` → `Scalar`, with `NEON` on ARM architectures.
    pub fn detect() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            if is_x86_feature_detected!("avx512f") {
                return Self::Avx512;
            }
            if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
                return Self::Avx2;
            }
            if is_x86_feature_detected!("sse2") {
                return Self::Sse2;
            }
            Self::Scalar
        }
        #[cfg(target_arch = "aarch64")]
        {
            #[cfg(target_feature = "neon")]
            {
                Self::Neon
            }
            #[cfg(not(target_feature = "neon"))]
            {
                Self::Scalar
            }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Self::Scalar
        }
    }

    /// User-facing display name of this SIMD execution tier.
    #[inline]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Scalar => "Scalar (Portable Fallback)",
            Self::Sse2 => "x86_64 SSE2 (128-bit)",
            Self::Neon => "ARM NEON (128-bit)",
            Self::Avx2 => "x86_64 AVX2+FMA (256-bit)",
            Self::Avx512 => "x86_64 AVX-512F (512-bit)",
        }
    }

    /// Native vector lane count for single-precision floating point (`f32`).
    #[inline]
    pub const fn lanes_f32(&self) -> usize {
        match self {
            Self::Scalar => 1,
            Self::Sse2 | Self::Neon => 4,
            Self::Avx2 => 8,
            Self::Avx512 => 16,
        }
    }

    /// Native vector lane count for double-precision floating point (`f64`).
    #[inline]
    pub const fn lanes_f64(&self) -> usize {
        match self {
            Self::Scalar => 1,
            Self::Sse2 | Self::Neon => 2,
            Self::Avx2 => 4,
            Self::Avx512 => 8,
        }
    }

    /// Whether this level utilizes hardware SIMD registers.
    #[inline]
    pub const fn is_hardware_accelerated(&self) -> bool {
        !matches!(self, Self::Scalar)
    }
}

impl Default for SimdLevel {
    fn default() -> Self {
        Self::detect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd_level_detection_and_properties() {
        let level = SimdLevel::detect();
        assert!(level.lanes_f32() >= 1);
        assert!(level.lanes_f64() >= 1);
        assert!(!level.name().is_empty());

        let scalar = SimdLevel::Scalar;
        assert_eq!(scalar.lanes_f32(), 1);
        assert_eq!(scalar.lanes_f64(), 1);
        assert!(!scalar.is_hardware_accelerated());

        let avx2 = SimdLevel::Avx2;
        assert_eq!(avx2.lanes_f32(), 8);
        assert_eq!(avx2.lanes_f64(), 4);
        assert!(avx2.is_hardware_accelerated());

        let avx512 = SimdLevel::Avx512;
        assert_eq!(avx512.lanes_f32(), 16);
        assert_eq!(avx512.lanes_f64(), 8);
        assert!(avx512.is_hardware_accelerated());
    }
}
