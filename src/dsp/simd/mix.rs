//! SIMD-accelerated channel mixing and crossfade summing kernels.

/// Element-wise in-place addition: `dst[i] += src[i]` over `n` elements (f32).
#[inline]
pub fn mix_slices(dst: &mut [f32], src: &[f32], n: usize) {
    let n = n.min(dst.len()).min(src.len());
    let mut i = 0usize;

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{_mm_add_ps, _mm_loadu_ps, _mm_storeu_ps};
        while i + 4 <= n {
            unsafe {
                let a = _mm_loadu_ps(dst.as_ptr().add(i));
                let b = _mm_loadu_ps(src.as_ptr().add(i));
                let s = _mm_add_ps(a, b);
                _mm_storeu_ps(dst.as_mut_ptr().add(i), s);
            }
            i += 4;
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vaddq_f32, vld1q_f32, vst1q_f32};
        while i + 4 <= n {
            unsafe {
                let a = vld1q_f32(dst.as_ptr().add(i));
                let b = vld1q_f32(src.as_ptr().add(i));
                let s = vaddq_f32(a, b);
                vst1q_f32(dst.as_mut_ptr().add(i), s);
            }
            i += 4;
        }
    }

    while i < n {
        dst[i] += src[i];
        i += 1;
    }
}

/// f64 twin of [`mix_slices`]: `dst[i] += src[i]` over `n` elements.
#[inline]
pub fn mix_slices_f64(dst: &mut [f64], src: &[f64], n: usize) {
    let n = n.min(dst.len()).min(src.len());
    let mut i = 0usize;

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{_mm_add_pd, _mm_loadu_pd, _mm_storeu_pd};
        while i + 2 <= n {
            unsafe {
                let a = _mm_loadu_pd(dst.as_ptr().add(i));
                let b = _mm_loadu_pd(src.as_ptr().add(i));
                let s = _mm_add_pd(a, b);
                _mm_storeu_pd(dst.as_mut_ptr().add(i), s);
            }
            i += 2;
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vaddq_f64, vld1q_f64, vst1q_f64};
        while i + 2 <= n {
            unsafe {
                let a = vld1q_f64(dst.as_ptr().add(i));
                let b = vld1q_f64(src.as_ptr().add(i));
                let s = vaddq_f64(a, b);
                vst1q_f64(dst.as_mut_ptr().add(i), s);
            }
            i += 2;
        }
    }

    while i < n {
        dst[i] += src[i];
        i += 1;
    }
}

/// Dual-source crossfade mixing: `dst[i] = src_curr[i] * g_curr + src_next[i] * g_next`.
#[inline]
pub fn mix_crossfade(
    dst: &mut [f32],
    src_curr: &[f32],
    g_curr: f32,
    src_next: &[f32],
    g_next: f32,
    n: usize,
) {
    let n = n.min(dst.len()).min(src_curr.len()).min(src_next.len());
    let mut i = 0usize;

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{
            _mm_add_ps, _mm_loadu_ps, _mm_mul_ps, _mm_set1_ps, _mm_storeu_ps,
        };
        let gc = unsafe { _mm_set1_ps(g_curr) };
        let gn = unsafe { _mm_set1_ps(g_next) };
        while i + 4 <= n {
            unsafe {
                let c = _mm_loadu_ps(src_curr.as_ptr().add(i));
                let next = _mm_loadu_ps(src_next.as_ptr().add(i));
                let mc = _mm_mul_ps(c, gc);
                let mn = _mm_mul_ps(next, gn);
                let s = _mm_add_ps(mc, mn);
                _mm_storeu_ps(dst.as_mut_ptr().add(i), s);
            }
            i += 4;
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vaddq_f32, vld1q_f32, vmulq_n_f32, vst1q_f32};
        while i + 4 <= n {
            unsafe {
                let c = vld1q_f32(src_curr.as_ptr().add(i));
                let next = vld1q_f32(src_next.as_ptr().add(i));
                let mc = vmulq_n_f32(c, g_curr);
                let mn = vmulq_n_f32(next, g_next);
                let s = vaddq_f32(mc, mn);
                vst1q_f32(dst.as_mut_ptr().add(i), s);
            }
            i += 4;
        }
    }

    while i < n {
        dst[i] = src_curr[i] * g_curr + src_next[i] * g_next;
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_slices_matches_scalar() {
        let mut dst = vec![1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];
        let src = vec![0.5f32, -0.5, 1.5, -2.0, 0.25, 0.75, -1.0];
        let mut expected = dst.clone();
        for (d, s) in expected.iter_mut().zip(src.iter()) {
            *d += *s;
        }
        mix_slices(&mut dst, &src, 7);
        for (a, b) in dst.iter().zip(expected.iter()) {
            assert!((a - b).abs() <= f32::EPSILON);
        }
    }

    #[test]
    fn mix_crossfade_matches_scalar() {
        let mut dst = vec![0.0f32; 8];
        let c = vec![1.0f32, 2.0, -1.0, -2.0, 0.5, 1.5, -0.5, 0.0];
        let n = vec![0.5f32, -0.5, 2.0, 1.0, -1.0, 0.25, 0.75, 3.0];
        let gc = 0.707f32;
        let gn = 0.707f32;
        let mut expected = [0.0f32; 8];
        for i in 0..8 {
            expected[i] = c[i] * gc + n[i] * gn;
        }
        mix_crossfade(&mut dst, &c, gc, &n, gn, 8);
        for (a, b) in dst.iter().zip(expected.iter()) {
            assert!((a - b).abs() <= 1e-6);
        }
    }
}
