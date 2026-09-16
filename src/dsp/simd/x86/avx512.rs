//! 512-bit AVX-512 vector kernels for x86_64 architectures (16 x f32, 8 x f64).
#![allow(clippy::missing_safety_doc)]

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

/// Element-wise scale: `dst[i] *= g` using 512-bit AVX-512 vector registers.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn scale_slice_avx512(dst: &mut [f32], g: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len());
        let gv = _mm512_set1_ps(g);
        let mut i = 0usize;

        while i + 16 <= n {
            let a = _mm512_loadu_ps(dst.as_ptr().add(i));
            let m = _mm512_mul_ps(a, gv);
            _mm512_storeu_ps(dst.as_mut_ptr().add(i), m);
            i += 16;
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

/// Double-precision scale using 512-bit AVX-512 registers (8 x f64): `dst[i] *= g`.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn scale_slice_f64_avx512(dst: &mut [f64], g: f64, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len());
        let gv = _mm512_set1_pd(g);
        let mut i = 0usize;

        while i + 8 <= n {
            let a = _mm512_loadu_pd(dst.as_ptr().add(i));
            let m = _mm512_mul_pd(a, gv);
            _mm512_storeu_pd(dst.as_mut_ptr().add(i), m);
            i += 8;
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

/// Mix slices: `dst[i] += src[i]` using AVX-512.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn mix_slices_avx512(dst: &mut [f32], src: &[f32], n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len()).min(src.len());
        let mut i = 0usize;

        while i + 16 <= n {
            let d = _mm512_loadu_ps(dst.as_ptr().add(i));
            let s = _mm512_loadu_ps(src.as_ptr().add(i));
            let r = _mm512_add_ps(d, s);
            _mm512_storeu_ps(dst.as_mut_ptr().add(i), r);
            i += 16;
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

/// Scaled accumulate using AVX-512 FMA: `dst[i] = dst[i] + src[i] * gain`.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn accumulate_scaled_avx512(dst: &mut [f32], src: &[f32], gain: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len()).min(src.len());
        let gv = _mm512_set1_ps(gain);
        let mut i = 0usize;

        while i + 16 <= n {
            let d = _mm512_loadu_ps(dst.as_ptr().add(i));
            let s = _mm512_loadu_ps(src.as_ptr().add(i));
            let r = _mm512_fmadd_ps(s, gv, d);
            _mm512_storeu_ps(dst.as_mut_ptr().add(i), r);
            i += 16;
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

/// Vector dot product using AVX-512.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn dot_product_avx512(a: &[f32], b: &[f32], n: usize) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(a.len()).min(b.len());
        let mut acc = _mm512_setzero_ps();
        let mut i = 0usize;

        while i + 16 <= n {
            let va = _mm512_loadu_ps(a.as_ptr().add(i));
            let vb = _mm512_loadu_ps(b.as_ptr().add(i));
            acc = _mm512_fmadd_ps(va, vb, acc);
            i += 16;
        }

        let mut sum = _mm512_reduce_add_ps(acc);

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
