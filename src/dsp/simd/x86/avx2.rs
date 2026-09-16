//! 256-bit AVX2 and FMA3 vector kernels for x86_64 architectures (8 x f32, 4 x f64).
#![allow(clippy::missing_safety_doc)]

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

/// Element-wise scale: `dst[i] *= g` using 256-bit AVX2 vector registers.
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn scale_slice_avx2(dst: &mut [f32], g: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len());
        let gv = _mm256_set1_ps(g);
        let mut i = 0usize;

        while i + 8 <= n {
            let a = _mm256_loadu_ps(dst.as_ptr().add(i));
            let m = _mm256_mul_ps(a, gv);
            _mm256_storeu_ps(dst.as_mut_ptr().add(i), m);
            i += 8;
        }

        while i < n {
            *dst.get_unchecked_mut(i) *= g;
            i += 1;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        crate::dsp::simd::scalar::scale_slice(dst, g, n);
    }
}

/// Double-precision scale using 256-bit AVX2 registers (4 x f64): `dst[i] *= g`.
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn scale_slice_f64_avx2(dst: &mut [f64], g: f64, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len());
        let gv = _mm256_set1_pd(g);
        let mut i = 0usize;

        while i + 4 <= n {
            let a = _mm256_loadu_pd(dst.as_ptr().add(i));
            let m = _mm256_mul_pd(a, gv);
            _mm256_storeu_pd(dst.as_mut_ptr().add(i), m);
            i += 4;
        }

        while i < n {
            *dst.get_unchecked_mut(i) *= g;
            i += 1;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        crate::dsp::simd::scalar::scale_slice_f64(dst, g, n);
    }
}

/// Mix slices: `dst[i] += src[i]` using AVX2.
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn mix_slices_avx2(dst: &mut [f32], src: &[f32], n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len()).min(src.len());
        let mut i = 0usize;

        while i + 8 <= n {
            let d = _mm256_loadu_ps(dst.as_ptr().add(i));
            let s = _mm256_loadu_ps(src.as_ptr().add(i));
            let r = _mm256_add_ps(d, s);
            _mm256_storeu_ps(dst.as_mut_ptr().add(i), r);
            i += 8;
        }

        while i < n {
            *dst.get_unchecked_mut(i) += *src.get_unchecked(i);
            i += 1;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        crate::dsp::simd::scalar::mix_slices(dst, src, n);
    }
}

/// Scaled accumulate using FMA: `dst[i] = dst[i] + src[i] * gain`.
#[inline]
#[target_feature(enable = "avx2", enable = "fma")]
pub unsafe fn accumulate_scaled_avx2(dst: &mut [f32], src: &[f32], gain: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len()).min(src.len());
        let gv = _mm256_set1_ps(gain);
        let mut i = 0usize;

        while i + 8 <= n {
            let d = _mm256_loadu_ps(dst.as_ptr().add(i));
            let s = _mm256_loadu_ps(src.as_ptr().add(i));
            // Fused multiply-add: s * gv + d
            let r = _mm256_fmadd_ps(s, gv, d);
            _mm256_storeu_ps(dst.as_mut_ptr().add(i), r);
            i += 8;
        }

        while i < n {
            *dst.get_unchecked_mut(i) += *src.get_unchecked(i) * gain;
            i += 1;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        crate::dsp::simd::scalar::accumulate_scaled(dst, src, gain, n);
    }
}

/// Vector dot product using AVX2 + FMA.
#[inline]
#[target_feature(enable = "avx2", enable = "fma")]
pub unsafe fn dot_product_avx2(a: &[f32], b: &[f32], n: usize) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(a.len()).min(b.len());
        let mut acc = _mm256_setzero_ps();
        let mut i = 0usize;

        while i + 8 <= n {
            let va = _mm256_loadu_ps(a.as_ptr().add(i));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i));
            acc = _mm256_fmadd_ps(va, vb, acc);
            i += 8;
        }

        let mut temp = [0.0f32; 8];
        _mm256_storeu_ps(temp.as_mut_ptr(), acc);
        let mut sum = temp[0] + temp[1] + temp[2] + temp[3] + temp[4] + temp[5] + temp[6] + temp[7];

        while i < n {
            sum += *a.get_unchecked(i) * *b.get_unchecked(i);
            i += 1;
        }

        sum
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        crate::dsp::simd::scalar::dot_product(a, b, n)
    }
}
