//! SIMD-accelerated biquad filter processing.
//!
//! Includes parallel stereo biquad execution where Left and Right channels
//! are updated simultaneously with SIMD vectors, plus unrolled single-channel kernels.

use crate::dsp_utils::flush_denormal;

/// Parallel stereo Direct-Form I biquad processing over `frames`.
///
/// Both channels share the same coefficients `(b0, b1, b2, a1, a2)`.
/// State is updated in-place.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn process_biquad_stereo(
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    left: &mut [f32],
    right: &mut [f32],
    x1_l: &mut f32,
    x2_l: &mut f32,
    y1_l: &mut f32,
    y2_l: &mut f32,
    x1_r: &mut f32,
    x2_r: &mut f32,
    y1_r: &mut f32,
    y2_r: &mut f32,
    frames: usize,
) {
    let n = frames.min(left.len()).min(right.len());

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{
            _mm_add_ps, _mm_mul_ps, _mm_set_ps, _mm_setr_ps, _mm_store_ss, _mm_sub_ps,
        };

        // Pack coefficients into 2-lane / 4-lane vectors: [L, R, 0, 0]
        let vb0 = unsafe { _mm_setr_ps(b0, b0, 0.0, 0.0) };
        let vb1 = unsafe { _mm_setr_ps(b1, b1, 0.0, 0.0) };
        let vb2 = unsafe { _mm_setr_ps(b2, b2, 0.0, 0.0) };
        let va1 = unsafe { _mm_setr_ps(a1, a1, 0.0, 0.0) };
        let va2 = unsafe { _mm_setr_ps(a2, a2, 0.0, 0.0) };

        let mut vx1 = unsafe { _mm_setr_ps(*x1_l, *x1_r, 0.0, 0.0) };
        let mut vx2 = unsafe { _mm_setr_ps(*x2_l, *x2_r, 0.0, 0.0) };
        let mut vy1 = unsafe { _mm_setr_ps(*y1_l, *y1_r, 0.0, 0.0) };
        let mut vy2 = unsafe { _mm_setr_ps(*y2_l, *y2_r, 0.0, 0.0) };

        for i in 0..n {
            unsafe {
                let vx = _mm_setr_ps(left[i], right[i], 0.0, 0.0);

                // y = b0*x + b1*x1 + b2*x2 - a1*y1 - a2*y2
                let p0 = _mm_mul_ps(vb0, vx);
                let p1 = _mm_mul_ps(vb1, vx1);
                let p2 = _mm_mul_ps(vb2, vx2);
                let q1 = _mm_mul_ps(va1, vy1);
                let q2 = _mm_mul_ps(va2, vy2);

                let sum_b = _mm_add_ps(_mm_add_ps(p0, p1), p2);
                let sum_a = _mm_add_ps(q1, q2);
                let vy = _mm_sub_ps(sum_b, sum_a);

                // Extract outputs
                let mut out_l = 0.0f32;
                let mut out_r = 0.0f32;
                _mm_store_ss(&mut out_l, vy);
                let shuf = _mm_set_ps(0.0, 0.0, 0.0, 0.0);
                // Move lane 1 to lane 0
                use core::arch::x86_64::_mm_shuffle_ps;
                let vy_r = _mm_shuffle_ps(vy, shuf, 0b00_00_00_01);
                _mm_store_ss(&mut out_r, vy_r);

                left[i] = flush_denormal(out_l);
                right[i] = flush_denormal(out_r);

                vx2 = vx1;
                vx1 = vx;
                vy2 = vy1;
                vy1 = _mm_setr_ps(left[i], right[i], 0.0, 0.0);
            }
        }

        unsafe {
            let mut l = 0.0f32;
            let mut r = 0.0f32;
            _mm_store_ss(&mut l, vx1);
            use core::arch::x86_64::_mm_shuffle_ps;
            let shuf = _mm_set_ps(0.0, 0.0, 0.0, 0.0);
            _mm_store_ss(&mut r, _mm_shuffle_ps(vx1, shuf, 0b00_00_00_01));
            *x1_l = l;
            *x1_r = r;

            _mm_store_ss(&mut l, vx2);
            _mm_store_ss(&mut r, _mm_shuffle_ps(vx2, shuf, 0b00_00_00_01));
            *x2_l = l;
            *x2_r = r;

            _mm_store_ss(&mut l, vy1);
            _mm_store_ss(&mut r, _mm_shuffle_ps(vy1, shuf, 0b00_00_00_01));
            *y1_l = l;
            *y1_r = r;

            _mm_store_ss(&mut l, vy2);
            _mm_store_ss(&mut r, _mm_shuffle_ps(vy2, shuf, 0b00_00_00_01));
            *y2_l = l;
            *y2_r = r;
        }
        return;
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vadd_f32, vld1_f32, vmul_n_f32, vst1_f32, vsub_f32};
        let mut vx1 = unsafe { vld1_f32([*x1_l, *x1_r].as_ptr()) };
        let mut vx2 = unsafe { vld1_f32([*x2_l, *x2_r].as_ptr()) };
        let mut vy1 = unsafe { vld1_f32([*y1_l, *y1_r].as_ptr()) };
        let mut vy2 = unsafe { vld1_f32([*y2_l, *y2_r].as_ptr()) };

        for i in 0..n {
            unsafe {
                let vx = vld1_f32([left[i], right[i]].as_ptr());
                let p0 = vmul_n_f32(vx, b0);
                let p1 = vmul_n_f32(vx1, b1);
                let p2 = vmul_n_f32(vx2, b2);
                let q1 = vmul_n_f32(vy1, a1);
                let q2 = vmul_n_f32(vy2, a2);

                let sum_b = vadd_f32(vadd_f32(p0, p1), p2);
                let sum_a = vadd_f32(q1, q2);
                let vy = vsub_f32(sum_b, sum_a);

                let mut out = [0.0f32; 2];
                vst1_f32(out.as_mut_ptr(), vy);
                left[i] = flush_denormal(out[0]);
                right[i] = flush_denormal(out[1]);

                vx2 = vx1;
                vx1 = vx;
                vy2 = vy1;
                vy1 = vld1_f32([left[i], right[i]].as_ptr());
            }
        }

        let mut st_x1 = [0.0f32; 2];
        let mut st_x2 = [0.0f32; 2];
        let mut st_y1 = [0.0f32; 2];
        let mut st_y2 = [0.0f32; 2];
        unsafe {
            vst1_f32(st_x1.as_mut_ptr(), vx1);
            vst1_f32(st_x2.as_mut_ptr(), vx2);
            vst1_f32(st_y1.as_mut_ptr(), vy1);
            vst1_f32(st_y2.as_mut_ptr(), vy2);
        }
        *x1_l = st_x1[0];
        *x1_r = st_x1[1];
        *x2_l = st_x2[0];
        *x2_r = st_x2[1];
        *y1_l = st_y1[0];
        *y1_r = st_y1[1];
        *y2_l = st_y2[0];
        *y2_r = st_y2[1];
        return;
    }

    // Scalar fallback
    #[allow(unreachable_code)]
    for i in 0..n {
        let xl = left[i];
        let yl = b0 * xl + b1 * *x1_l + b2 * *x2_l - a1 * *y1_l - a2 * *y2_l;
        *x2_l = *x1_l;
        *x1_l = xl;
        *y2_l = *y1_l;
        *y1_l = flush_denormal(yl);
        left[i] = *y1_l;

        let xr = right[i];
        let yr = b0 * xr + b1 * *x1_r + b2 * *x2_r - a1 * *y1_r - a2 * *y2_r;
        *x2_r = *x1_r;
        *x1_r = xr;
        *y2_r = *y1_r;
        *y1_r = flush_denormal(yr);
        right[i] = *y1_r;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biquad_stereo_runs_deterministically() {
        let mut l = vec![1.0f32, 0.0, 0.0, 0.0, 0.0];
        let mut r = vec![0.5f32, 0.0, 0.0, 0.0, 0.0];
        let mut x1_l = 0.0;
        let mut x2_l = 0.0;
        let mut y1_l = 0.0;
        let mut y2_l = 0.0;
        let mut x1_r = 0.0;
        let mut x2_r = 0.0;
        let mut y1_r = 0.0;
        let mut y2_r = 0.0;

        process_biquad_stereo(
            0.5, 0.2, 0.1, -0.3, 0.1, &mut l, &mut r, &mut x1_l, &mut x2_l, &mut y1_l, &mut y2_l,
            &mut x1_r, &mut x2_r, &mut y1_r, &mut y2_r, 5,
        );

        assert!((l[0] - 0.5).abs() < 1e-6);
        assert!((r[0] - 0.25).abs() < 1e-6);
    }
}
