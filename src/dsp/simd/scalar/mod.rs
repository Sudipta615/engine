//! Pure Rust portable scalar reference implementations for DSP vector kernels.
//!
//! Every SIMD optimization in `x86/` and `arm/` must produce results that are
//! either bit-identical or numerically equivalent within strict IEEE-754 tolerances
//! of the functions defined here.

/// Element-wise in-place multiplication: `dst[i] *= g` over `n` elements.
#[inline]
pub fn scale_slice(dst: &mut [f32], g: f32, n: usize) {
    let n = n.min(dst.len());
    for sample in dst.iter_mut().take(n) {
        *sample *= g;
    }
}

/// Double-precision in-place multiplication: `dst[i] *= g` over `n` elements.
#[inline]
pub fn scale_slice_f64(dst: &mut [f64], g: f64, n: usize) {
    let n = n.min(dst.len());
    for sample in dst.iter_mut().take(n) {
        *sample *= g;
    }
}

/// Linear ramp gain application: `dst[i] *= (start + i * step)` over `n` elements.
#[inline]
pub fn ramp_slice(dst: &mut [f32], mut current: f32, step: f32, n: usize) {
    let n = n.min(dst.len());
    for sample in dst.iter_mut().take(n) {
        *sample *= current;
        current += step;
    }
}

/// Double-precision linear ramp: `dst[i] *= (start + i * step)`.
#[inline]
pub fn ramp_slice_f64(dst: &mut [f64], mut current: f64, step: f64, n: usize) {
    let n = n.min(dst.len());
    for sample in dst.iter_mut().take(n) {
        *sample *= current;
        current += step;
    }
}

/// Element-wise sum in-place: `dst[i] += src[i]` over `n` elements.
#[inline]
pub fn mix_slices(dst: &mut [f32], src: &[f32], n: usize) {
    let n = n.min(dst.len()).min(src.len());
    for (d, s) in dst.iter_mut().zip(src.iter()).take(n) {
        *d += *s;
    }
}

/// Double-precision element-wise sum: `dst[i] += src[i]`.
#[inline]
pub fn mix_slices_f64(dst: &mut [f64], src: &[f64], n: usize) {
    let n = n.min(dst.len()).min(src.len());
    for (d, s) in dst.iter_mut().zip(src.iter()).take(n) {
        *d += *s;
    }
}

/// Scaled accumulate: `dst[i] += src[i] * gain` over `n` elements.
#[inline]
pub fn accumulate_scaled(dst: &mut [f32], src: &[f32], gain: f32, n: usize) {
    let n = n.min(dst.len()).min(src.len());
    for (d, s) in dst.iter_mut().zip(src.iter()).take(n) {
        *d += *s * gain;
    }
}

/// Crossfade blend: `dst[i] = a[i] * g_a + b[i] * g_b`.
#[inline]
pub fn mix_crossfade(dst: &mut [f32], a: &[f32], b: &[f32], g_a: f32, g_b: f32, n: usize) {
    let n = n.min(dst.len()).min(a.len()).min(b.len());
    for i in 0..n {
        dst[i] = a[i] * g_a + b[i] * g_b;
    }
}

/// Find peak absolute magnitude in slice: `max(|src[i]|)`.
#[inline]
pub fn vector_abs_max(src: &[f32], n: usize) -> f32 {
    let n = n.min(src.len());
    let mut peak = 0.0f32;
    for &s in src.iter().take(n) {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
    }
    peak
}

/// Double-precision peak absolute magnitude.
#[inline]
pub fn vector_abs_max_f64(src: &[f64], n: usize) -> f64 {
    let n = n.min(src.len());
    let mut peak = 0.0f64;
    for &s in src.iter().take(n) {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
    }
    peak
}

/// Linear interpolation between vectors: `out[i] = a[i] + t * (b[i] - a[i])`.
#[inline]
pub fn vector_lerp(out: &mut [f32], a: &[f32], b: &[f32], t: f32, n: usize) {
    let n = n.min(out.len()).min(a.len()).min(b.len());
    for i in 0..n {
        out[i] = a[i] + t * (b[i] - a[i]);
    }
}

/// Bilinear interpolation across 4 grid corner vectors.
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

    let w00 = (1.0 - fa) * (1.0 - fe);
    let w10 = fa * (1.0 - fe);
    let w01 = (1.0 - fa) * fe;
    let w11 = fa * fe;

    for i in 0..n {
        out[i] = a00[i] * w00 + a10[i] * w10 + a01[i] * w01 + a11[i] * w11;
    }
}

/// Vector dot product: `sum(a[i] * b[i])`.
#[inline]
pub fn dot_product(a: &[f32], b: &[f32], n: usize) -> f32 {
    let n = n.min(a.len()).min(b.len());
    let mut sum = 0.0f32;
    for i in 0..n {
        sum += a[i] * b[i];
    }
    sum
}

/// Double-precision vector dot product.
#[inline]
pub fn dot_product_f64(a: &[f64], b: &[f64], n: usize) -> f64 {
    let n = n.min(a.len()).min(b.len());
    let mut sum = 0.0f64;
    for i in 0..n {
        sum += a[i] * b[i];
    }
    sum
}
