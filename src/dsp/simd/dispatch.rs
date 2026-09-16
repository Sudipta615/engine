//! Dynamic runtime dispatch engine enforcing the Guide §8.2 priority hierarchy.
//!
//! Priority order:
//! `AVX-512` → `AVX2/FMA` → `SSE2` → `Scalar`, and `NEON` on ARM architectures.
//! Fallback to scalar is always available, guaranteed deterministic and bit-exact.

use super::levels::SimdLevel;

/// Dispatch `scale_slice` over `dst` using the highest available SIMD tier.
#[inline]
pub fn dispatch_scale_slice(dst: &mut [f32], g: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::scale_slice_avx512(dst, g, n);
            }
        }
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return super::x86::avx2::scale_slice_avx2(dst, g, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::scale_slice_sse2(dst, g, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::scale_slice_neon(dst, g, n);
        }
    }
    super::scalar::scale_slice(dst, g, n);
}

/// Dispatch `scale_slice_f64` over `dst` using the highest available SIMD tier.
#[inline]
pub fn dispatch_scale_slice_f64(dst: &mut [f64], g: f64, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::scale_slice_f64_avx512(dst, g, n);
            }
        }
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return super::x86::avx2::scale_slice_f64_avx2(dst, g, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::scale_slice_f64_sse2(dst, g, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::scale_slice_f64_neon(dst, g, n);
        }
    }
    super::scalar::scale_slice_f64(dst, g, n);
}

/// Dispatch `mix_slices` using the highest available SIMD tier.
#[inline]
pub fn dispatch_mix_slices(dst: &mut [f32], src: &[f32], n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::mix_slices_avx512(dst, src, n);
            }
        }
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return super::x86::avx2::mix_slices_avx2(dst, src, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::mix_slices_sse2(dst, src, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::mix_slices_neon(dst, src, n);
        }
    }
    super::scalar::mix_slices(dst, src, n);
}

/// Dispatch `accumulate_scaled` using the highest available SIMD tier.
#[inline]
pub fn dispatch_accumulate_scaled(dst: &mut [f32], src: &[f32], gain: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::accumulate_scaled_avx512(dst, src, gain, n);
            }
        }
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            unsafe {
                return super::x86::avx2::accumulate_scaled_avx2(dst, src, gain, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::accumulate_scaled_sse2(dst, src, gain, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::accumulate_scaled_neon(dst, src, gain, n);
        }
    }
    super::scalar::accumulate_scaled(dst, src, gain, n);
}

/// Dispatch `dot_product` using the highest available SIMD tier.
#[inline]
pub fn dispatch_dot_product(a: &[f32], b: &[f32], n: usize) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::dot_product_avx512(a, b, n);
            }
        }
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            unsafe {
                return super::x86::avx2::dot_product_avx2(a, b, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::dot_product_sse2(a, b, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::dot_product_neon(a, b, n);
        }
    }
    super::scalar::dot_product(a, b, n)
}

/// Explicit level-directed execution for testing, verification, and diagnostics.
pub fn execute_at_level(level: SimdLevel, dst: &mut [f32], g: f32, n: usize) {
    match level {
        SimdLevel::Scalar => super::scalar::scale_slice(dst, g, n),
        SimdLevel::Sse2 => {
            #[cfg(target_arch = "x86_64")]
            unsafe {
                super::x86::sse2::scale_slice_sse2(dst, g, n);
            }
            #[cfg(not(target_arch = "x86_64"))]
            super::scalar::scale_slice(dst, g, n);
        }
        SimdLevel::Neon => {
            #[cfg(target_arch = "aarch64")]
            unsafe {
                super::arm::neon::scale_slice_neon(dst, g, n);
            }
            #[cfg(not(target_arch = "aarch64"))]
            super::scalar::scale_slice(dst, g, n);
        }
        SimdLevel::Avx2 => {
            #[cfg(target_arch = "x86_64")]
            unsafe {
                super::x86::avx2::scale_slice_avx2(dst, g, n);
            }
            #[cfg(not(target_arch = "x86_64"))]
            super::scalar::scale_slice(dst, g, n);
        }
        SimdLevel::Avx512 => {
            #[cfg(target_arch = "x86_64")]
            unsafe {
                super::x86::avx512::scale_slice_avx512(dst, g, n);
            }
            #[cfg(not(target_arch = "x86_64"))]
            super::scalar::scale_slice(dst, g, n);
        }
    }
}
