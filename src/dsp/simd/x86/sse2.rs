//! 128-bit SSE2 vector kernels for x86_64 architectures (4 x f32, 2 x f64).
#![allow(clippy::missing_safety_doc)]

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

/// Element-wise scale: `dst[i] *= g` using 128-bit SSE2 vector registers.
#[inline]
#[target_feature(enable = "sse2")]
pub unsafe fn scale_slice_sse2(dst: &mut [f32], g: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len());
        let gv = _mm_set1_ps(g);
        let mut i = 0usize;

        while i + 4 <= n {
            let a = _mm_loadu_ps(dst.as_ptr().add(i));
            let m = _mm_mul_ps(a, gv);
            _mm_storeu_ps(dst.as_mut_ptr().add(i), m);
            i += 4;
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

/// Double-precision scale using SSE2: `dst[i] *= g`.
#[inline]
#[target_feature(enable = "sse2")]
pub unsafe fn scale_slice_f64_sse2(dst: &mut [f64], g: f64, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len());
        let gv = _mm_set1_pd(g);
        let mut i = 0usize;

        while i + 2 <= n {
            let a = _mm_loadu_pd(dst.as_ptr().add(i));
            let m = _mm_mul_pd(a, gv);
            _mm_storeu_pd(dst.as_mut_ptr().add(i), m);
            i += 2;
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

/// Mix slices: `dst[i] += src[i]` using SSE2.
#[inline]
#[target_feature(enable = "sse2")]
pub unsafe fn mix_slices_sse2(dst: &mut [f32], src: &[f32], n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len()).min(src.len());
        let mut i = 0usize;

        while i + 4 <= n {
            let d = _mm_loadu_ps(dst.as_ptr().add(i));
            let s = _mm_loadu_ps(src.as_ptr().add(i));
            let r = _mm_add_ps(d, s);
            _mm_storeu_ps(dst.as_mut_ptr().add(i), r);
            i += 4;
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

/// Accumulate scaled: `dst[i] += src[i] * gain` using SSE2.
#[inline]
#[target_feature(enable = "sse2")]
pub unsafe fn accumulate_scaled_sse2(dst: &mut [f32], src: &[f32], gain: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(dst.len()).min(src.len());
        let gv = _mm_set1_ps(gain);
        let mut i = 0usize;

        while i + 4 <= n {
            let d = _mm_loadu_ps(dst.as_ptr().add(i));
            let s = _mm_loadu_ps(src.as_ptr().add(i));
            let scaled = _mm_mul_ps(s, gv);
            let r = _mm_add_ps(d, scaled);
            _mm_storeu_ps(dst.as_mut_ptr().add(i), r);
            i += 4;
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

/// Vector dot product using SSE2.
#[inline]
#[target_feature(enable = "sse2")]
pub unsafe fn dot_product_sse2(a: &[f32], b: &[f32], n: usize) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        let n = n.min(a.len()).min(b.len());
        let mut acc = _mm_setzero_ps();
        let mut i = 0usize;

        while i + 4 <= n {
            let va = _mm_loadu_ps(a.as_ptr().add(i));
            let vb = _mm_loadu_ps(b.as_ptr().add(i));
            let prod = _mm_mul_ps(va, vb);
            acc = _mm_add_ps(acc, prod);
            i += 4;
        }

        let mut temp = [0.0f32; 4];
        _mm_storeu_ps(temp.as_mut_ptr(), acc);
        let mut sum = temp[0] + temp[1] + temp[2] + temp[3];

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
