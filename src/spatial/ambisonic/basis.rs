//! Spherical harmonic basis evaluation for Higher-Order Ambisonics (ACN / SN3D).
//!
//! Supports orders 0 through 9 (up to 100 channels). Orders 0–3 use exact closed-form
//! polynomials; orders 4–9 use the associated Legendre recurrence relation.

use crate::spatial::math::Vec3;

/// The default ambisonic order (1 = First-Order Ambisonics, FOA) — the
/// engine-wide bus width for fields and virtual rings.
pub const AMBISONIC_ORDER: u8 = 1;

/// Highest implemented order (9 = Ninth-Order Ambisonics, 100 channels).
pub const MAX_AMBISONIC_ORDER: u8 = 9;

/// Number of ambisonic channels for `order` (`(order+1)²`).
pub fn channel_count(order: u8) -> usize {
    let o = order as usize + 1;
    o * o
}

/// FOA channel count (order [`AMBISONIC_ORDER`]).
pub const AMBISONIC_CHANNELS: usize = 4;

/// Second-order (SOA) channel count.
pub const AMBISONIC_CHANNELS_ORDER_2: usize = 9;

/// Third-order (TOA) channel count.
pub const AMBISONIC_CHANNELS_ORDER_3: usize = 16;

/// Largest supported bus width (`channel_count(MAX_AMBISONIC_ORDER)` = 100
/// for order 9), used to size the allocation-free per-frame scratch.
pub const AMBISONIC_CHANNELS_MAX: usize = 100;

/// Real spherical-harmonic basis for a unit direction, ACN/SN3D, order 1:
/// `[W, Y, Z, X]` = `[1, √3·y, √3·z, √3·x]`.
#[inline]
pub fn sh_foa(dir: Vec3) -> [f32; 4] {
    let s3 = 3.0f32.sqrt();
    [1.0, s3 * dir.y, s3 * dir.z, s3 * dir.x]
}

/// Real spherical-harmonic basis, ACN/SN3D, order ≤ [`MAX_AMBISONIC_ORDER`].
/// Writes `channel_count(order)` values into `out` (panics if too short);
/// order 1 is exactly [`sh_foa`], order 2 appends ACN 4–8, order 3 appends
/// ACN 9–15. `dir` is normalised defensively; a zero direction reads as
/// `+Y`.
#[inline]
pub fn sh_n(order: u8, dir: Vec3, out: &mut [f32]) {
    let n = channel_count(order);
    assert!(out.len() >= n, "sh_n: out too short ({} < {n})", out.len());
    let d = dir.normalized().unwrap_or(Vec3::Y);
    let (x, y, z) = (d.x, d.y, d.z);
    out[0] = 1.0;
    if order >= 1 {
        let s3 = 3.0f32.sqrt();
        out[1] = s3 * y;
        out[2] = s3 * z;
        out[3] = s3 * x;
    }
    if order >= 2 {
        let s15 = 15.0f32.sqrt();
        out[4] = s15 * x * y;
        out[5] = s15 * y * z;
        out[6] = 5.0f32.sqrt() * 0.5 * (3.0 * z * z - 1.0);
        out[7] = s15 * x * z;
        out[8] = s15 * 0.5 * (x * x - y * y);
    }
    if order >= 3 {
        let s358 = (35.0f32 / 8.0).sqrt();
        let s105 = 105.0f32.sqrt();
        let s218 = (21.0f32 / 8.0).sqrt();
        let s7h = 7.0f32.sqrt() * 0.5;
        let s105h = 105.0f32.sqrt() * 0.5;
        out[9] = s358 * x * (x * x - 3.0 * y * y);
        out[10] = s105 * x * y * z;
        out[11] = s218 * y * (5.0 * z * z - 1.0);
        out[12] = s7h * z * (5.0 * z * z - 3.0);
        out[13] = s218 * x * (5.0 * z * z - 1.0);
        out[14] = s105h * z * (x * x - y * y);
        out[15] = s358 * y * (y * y - 3.0 * x * x);
    }
    if order >= 4 {
        sh_n_high_order(order, x, y, z, out);
    }
}

/// Compute real spherical harmonics for orders 4–9 using the Legendre
/// recurrence relation. Fills `out[acn(4,0)..acn(order,order)+1]`.
/// Requires that orders 0–3 are already populated in `out`.
#[allow(clippy::needless_range_loop)]
fn sh_n_high_order(order: u8, x: f32, y: f32, z: f32, out: &mut [f32]) {
    const MAXL: usize = 10;
    let maxm = (order as usize + 1).min(MAXL);
    let x64 = x as f64;
    let y64 = y as f64;
    let z64 = z as f64;

    let phi = y64.atan2(x64);
    let cos_phi = phi.cos();
    let sin_phi = phi.sin();

    let mut cos_mphi = [0.0f64; MAXL];
    let mut sin_mphi = [0.0f64; MAXL];
    cos_mphi[0] = 1.0;
    sin_mphi[0] = 0.0;
    if maxm > 1 {
        cos_mphi[1] = cos_phi;
        sin_mphi[1] = sin_phi;
    }
    for m in 2..maxm {
        cos_mphi[m] = 2.0 * cos_phi * cos_mphi[m - 1] - cos_mphi[m - 2];
        sin_mphi[m] = 2.0 * cos_phi * sin_mphi[m - 1] - sin_mphi[m - 2];
    }

    let mut plm = [[0.0f64; MAXL]; MAXL];
    plm[0][0] = 1.0;
    let sinth2 = 1.0 - z64 * z64;
    let sinth = sinth2.max(0.0).sqrt();

    for m in 1..maxm {
        let prev = plm[m - 1][m - 1];
        plm[m][m] = -((2 * m - 1) as f64) * sinth * prev;
        if m < maxm - 1 {
            plm[m + 1][m] = z64 * (2 * m + 1) as f64 * plm[m][m];
        }
    }
    for l in 2..maxm {
        for m in 0..l.saturating_sub(1) {
            if plm[l - 2][m] == 0.0 && plm[l - 1][m] == 0.0 {
                continue;
            }
            let lf = l as f64;
            let mf = m as f64;
            plm[l][m] = ((2.0 * lf - 1.0) * z64 * plm[l - 1][m] - (lf + mf - 1.0) * plm[l - 2][m])
                / (lf - mf);
        }
    }

    let factorial = |n: usize| -> f64 {
        let mut r = 1.0f64;
        for i in 2..=n {
            r *= i as f64;
        }
        r
    };

    for l in 4..=order as usize {
        let lf = l as f64;
        let two_l_1 = 2.0 * lf + 1.0;
        for signed_m in -(l as i32)..=(l as i32) {
            let m_abs = signed_m.unsigned_abs() as usize;
            let acn = l * l + (l as i32 + signed_m) as usize;
            if acn >= out.len() {
                continue;
            }
            let norm = ((2.0 - if m_abs == 0 { 1.0 } else { 0.0 })
                * (factorial(l - m_abs) / factorial(l + m_abs))
                * two_l_1)
                .sqrt();
            let p_val = plm[l][m_abs];
            let trig = if signed_m < 0 {
                sin_mphi[m_abs]
            } else {
                cos_mphi[m_abs]
            };
            out[acn] = (norm * p_val * trig) as f32;
        }
    }
}
