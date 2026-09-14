//! SIMD-accelerated gain and ramping kernels.
//!
//! Provides vector scaling and linear ramps with exact scalar fallbacks.

/// Element-wise in-place multiplication: `dst[i] *= g` over `n` elements (f32).
#[inline]
pub fn scale_slice(dst: &mut [f32], g: f32, n: usize) {
    let n = n.min(dst.len());
    let mut i = 0usize;

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{_mm_loadu_ps, _mm_mul_ps, _mm_set1_ps, _mm_storeu_ps};
        let gv = unsafe { _mm_set1_ps(g) };
        while i + 4 <= n {
            unsafe {
                let a = _mm_loadu_ps(dst.as_ptr().add(i));
                let m = _mm_mul_ps(a, gv);
                _mm_storeu_ps(dst.as_mut_ptr().add(i), m);
            }
            i += 4;
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vld1q_f32, vmulq_n_f32, vst1q_f32};
        while i + 4 <= n {
            unsafe {
                let a = vld1q_f32(dst.as_ptr().add(i));
                let m = vmulq_n_f32(a, g);
                vst1q_f32(dst.as_mut_ptr().add(i), m);
            }
            i += 4;
        }
    }

    while i < n {
        dst[i] *= g;
        i += 1;
    }
}

/// f64 twin of [`scale_slice`]: `dst[i] *= g` over `n` elements.
#[inline]
pub fn scale_slice_f64(dst: &mut [f64], g: f64, n: usize) {
    let n = n.min(dst.len());
    let mut i = 0usize;

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{_mm_loadu_pd, _mm_mul_pd, _mm_set1_pd, _mm_storeu_pd};
        let gv = unsafe { _mm_set1_pd(g) };
        while i + 2 <= n {
            unsafe {
                let a = _mm_loadu_pd(dst.as_ptr().add(i));
                let m = _mm_mul_pd(a, gv);
                _mm_storeu_pd(dst.as_mut_ptr().add(i), m);
            }
            i += 2;
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vld1q_f64, vmulq_n_f64, vst1q_f64};
        while i + 2 <= n {
            unsafe {
                let a = vld1q_f64(dst.as_ptr().add(i));
                let m = vmulq_n_f64(a, g);
                vst1q_f64(dst.as_mut_ptr().add(i), m);
            }
            i += 2;
        }
    }

    while i < n {
        dst[i] *= g;
        i += 1;
    }
}

/// Linear gain ramp applied in-place: `dst[i] *= (start_g + i as f32 * step_g)`.
#[inline]
pub fn ramp_slice(dst: &mut [f32], start_g: f32, step_g: f32, n: usize) {
    let n = n.min(dst.len());
    let mut cur_g = start_g;
    for x in dst.iter_mut().take(n) {
        *x *= cur_g;
        cur_g += step_g;
    }
}

/// f64 twin of [`ramp_slice`].
#[inline]
pub fn ramp_slice_f64(dst: &mut [f64], start_g: f64, step_g: f64, n: usize) {
    let n = n.min(dst.len());
    let mut cur_g = start_g;
    for x in dst.iter_mut().take(n) {
        *x *= cur_g;
        cur_g += step_g;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_slice_matches_scalar() {
        let mut data = vec![1.0f32, -0.5, 0.25, 2.0, -1.5, 0.75, 0.0, -2.5, 3.125];
        let mut expected = data.clone();
        let g = 0.8f32;
        let n = data.len();

        for x in &mut expected {
            *x *= g;
        }
        scale_slice(&mut data, g, n);

        for (a, b) in data.iter().zip(expected.iter()) {
            assert!((a - b).abs() <= f32::EPSILON);
        }
    }

    #[test]
    fn scale_slice_f64_matches_scalar() {
        let mut data = vec![1.0f64, -0.5, 0.25, 2.0, -1.5, 0.75, 0.0, -2.5, 3.125];
        let mut expected = data.clone();
        let g = 0.8f64;
        let n = data.len();

        for x in &mut expected {
            *x *= g;
        }
        scale_slice_f64(&mut data, g, n);

        for (a, b) in data.iter().zip(expected.iter()) {
            assert!((a - b).abs() <= f64::EPSILON);
        }
    }
}
