//! SIMD-accelerated peak and envelope calculation kernels for limiters/compressors.

/// Find maximum absolute sample value in `src` over `n` samples.
#[inline]
pub fn vector_abs_max(src: &[f32], n: usize) -> f32 {
    let n = n.min(src.len());
    if n == 0 {
        return 0.0;
    }
    let mut i = 0usize;
    let mut max_val = 0.0f32;

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{_mm_and_ps, _mm_loadu_ps, _mm_max_ps, _mm_set1_ps, _mm_store_ss};
        // Mask out sign bit: 0x7FFF_FFFF
        let mask = unsafe { _mm_set1_ps(f32::from_bits(0x7FFF_FFFF)) };
        let mut vmax = unsafe { _mm_set1_ps(0.0) };

        while i + 4 <= n {
            unsafe {
                let v = _mm_loadu_ps(src.as_ptr().add(i));
                let vabs = _mm_and_ps(v, mask);
                vmax = _mm_max_ps(vmax, vabs);
            }
            i += 4;
        }

        // Horizontal max of 4-lane vector
        unsafe {
            use core::arch::x86_64::_mm_shuffle_ps;
            let shuf1 = _mm_shuffle_ps(vmax, vmax, 0b01_00_11_10);
            let m1 = _mm_max_ps(vmax, shuf1);
            let shuf2 = _mm_shuffle_ps(m1, m1, 0b00_00_00_01);
            let m2 = _mm_max_ps(m1, shuf2);
            let mut res = 0.0f32;
            _mm_store_ss(&mut res, m2);
            max_val = max_val.max(res);
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vabsq_f32, vld1q_f32, vmaxq_f32, vmaxvq_f32};
        let mut vmax = unsafe { vld1q_f32([0.0, 0.0, 0.0, 0.0].as_ptr()) };

        while i + 4 <= n {
            unsafe {
                let v = vld1q_f32(src.as_ptr().add(i));
                let vabs = vabsq_f32(v);
                vmax = vmaxq_f32(vmax, vabs);
            }
            i += 4;
        }

        unsafe {
            let res = vmaxvq_f32(vmax);
            max_val = max_val.max(res);
        }
    }

    while i < n {
        max_val = max_val.max(src[i].abs());
        i += 1;
    }

    max_val
}

/// f64 twin of [`vector_abs_max`].
#[inline]
pub fn vector_abs_max_f64(src: &[f64], n: usize) -> f64 {
    let n = n.min(src.len());
    let mut max_val = 0.0f64;
    for x in src.iter().take(n) {
        max_val = max_val.max(x.abs());
    }
    max_val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_max_finds_peak() {
        let data = vec![0.1f32, -0.9, 0.4, -0.2, 0.85, -0.3, 0.0];
        let peak = vector_abs_max(&data, data.len());
        assert!((peak - 0.9).abs() < 1e-6);
    }
}
