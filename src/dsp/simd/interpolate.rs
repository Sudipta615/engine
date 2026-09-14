//! SIMD-accelerated interpolation kernels (HRTF FIR tap accumulation, bilinear grid lookup).

/// In-place linear interpolation between slices: `out[k] = a[k] + factor * (b[k] - a[k])`.
#[inline]
pub fn vector_lerp(out: &mut [f32], a: &[f32], b: &[f32], factor: f32, n: usize) {
    let n = n.min(out.len()).min(a.len()).min(b.len());
    let mut i = 0usize;

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{
            _mm_add_ps, _mm_loadu_ps, _mm_mul_ps, _mm_set1_ps, _mm_storeu_ps, _mm_sub_ps,
        };
        let fv = unsafe { _mm_set1_ps(factor) };
        while i + 4 <= n {
            unsafe {
                let va = _mm_loadu_ps(a.as_ptr().add(i));
                let vb = _mm_loadu_ps(b.as_ptr().add(i));
                let diff = _mm_sub_ps(vb, va);
                let scaled = _mm_mul_ps(diff, fv);
                let res = _mm_add_ps(va, scaled);
                _mm_storeu_ps(out.as_mut_ptr().add(i), res);
            }
            i += 4;
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vaddq_f32, vld1q_f32, vmulq_n_f32, vst1q_f32, vsubq_f32};
        while i + 4 <= n {
            unsafe {
                let va = vld1q_f32(a.as_ptr().add(i));
                let vb = vld1q_f32(b.as_ptr().add(i));
                let diff = vsubq_f32(vb, va);
                let scaled = vmulq_n_f32(diff, factor);
                let res = vaddq_f32(va, scaled);
                vst1q_f32(out.as_mut_ptr().add(i), res);
            }
            i += 4;
        }
    }

    while i < n {
        out[i] = a[i] + factor * (b[i] - a[i]);
        i += 1;
    }
}

/// Bilinear interpolation across 4 corner slices:
/// `lo = a00 + fa * (a10 - a00)`
/// `hi = a01 + fa * (a11 - a01)`
/// `out = lo + fe * (hi - lo)`
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn vector_bilinear(
    out: &mut [f32],
    a00: &[f32],
    a10: &[f32],
    a01: &[f32],
    a11: &[f32],
    fa: f32,
    fe: f32,
    n: usize,
) {
    let n = n
        .min(out.len())
        .min(a00.len())
        .min(a10.len())
        .min(a01.len())
        .min(a11.len());
    let mut i = 0usize;

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{
            _mm_add_ps, _mm_loadu_ps, _mm_mul_ps, _mm_set1_ps, _mm_storeu_ps, _mm_sub_ps,
        };
        let fav = unsafe { _mm_set1_ps(fa) };
        let fev = unsafe { _mm_set1_ps(fe) };
        while i + 4 <= n {
            unsafe {
                let v00 = _mm_loadu_ps(a00.as_ptr().add(i));
                let v10 = _mm_loadu_ps(a10.as_ptr().add(i));
                let v01 = _mm_loadu_ps(a01.as_ptr().add(i));
                let v11 = _mm_loadu_ps(a11.as_ptr().add(i));

                let lo = _mm_add_ps(v00, _mm_mul_ps(_mm_sub_ps(v10, v00), fav));
                let hi = _mm_add_ps(v01, _mm_mul_ps(_mm_sub_ps(v11, v01), fav));
                let res = _mm_add_ps(lo, _mm_mul_ps(_mm_sub_ps(hi, lo), fev));
                _mm_storeu_ps(out.as_mut_ptr().add(i), res);
            }
            i += 4;
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vaddq_f32, vld1q_f32, vmulq_n_f32, vst1q_f32, vsubq_f32};
        while i + 4 <= n {
            unsafe {
                let v00 = vld1q_f32(a00.as_ptr().add(i));
                let v10 = vld1q_f32(a10.as_ptr().add(i));
                let v01 = vld1q_f32(a01.as_ptr().add(i));
                let v11 = vld1q_f32(a11.as_ptr().add(i));

                let lo = vaddq_f32(v00, vmulq_n_f32(vsubq_f32(v10, v00), fa));
                let hi = vaddq_f32(v01, vmulq_n_f32(vsubq_f32(v11, v01), fa));
                let res = vaddq_f32(lo, vmulq_n_f32(vsubq_f32(hi, lo), fe));
                vst1q_f32(out.as_mut_ptr().add(i), res);
            }
            i += 4;
        }
    }

    while i < n {
        let lo = a00[i] + fa * (a10[i] - a00[i]);
        let hi = a01[i] + fa * (a11[i] - a01[i]);
        out[i] = lo + fe * (hi - lo);
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bilinear_matches_scalar() {
        let mut out = vec![0.0f32; 8];
        let a00 = vec![1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let a10 = vec![2.0f32, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0];
        let a01 = vec![3.0f32, 6.0, 9.0, 12.0, 15.0, 18.0, 21.0, 24.0];
        let a11 = vec![4.0f32, 8.0, 12.0, 16.0, 20.0, 24.0, 28.0, 32.0];
        let fa = 0.3f32;
        let fe = 0.7f32;

        let mut expected = [0.0f32; 8];
        for k in 0..8 {
            let lo = a00[k] + fa * (a10[k] - a00[k]);
            let hi = a01[k] + fa * (a11[k] - a01[k]);
            expected[k] = lo + fe * (hi - lo);
        }

        vector_bilinear(&mut out, &a00, &a10, &a01, &a11, fa, fe, 8);

        for (a, b) in out.iter().zip(expected.iter()) {
            assert!((a - b).abs() <= 1e-6);
        }
    }
}
