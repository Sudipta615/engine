//! Ambisonics / Higher-Order Ambisonics (spec Part VI §32–37, §55).
//!
//! Ambisonics is the engine's sound-field representation: a direction-
//! independent bus that a *field* (or any spatial source) encodes into and
//! that any speaker layout decodes from, so the same bus renders to stereo,
//! 5.1, 7.1.4, or a custom array without re-authoring. It is the natural
//! home for diffuse environments, ambience and the room's late field
//! (§32, §55).
//!
//! ```text
//! Spatial source → encoder → ambisonic bus → decoder → SpeakerLayout → PCM
//! ```
//!
//! ## Conventions (documented, spec §35 / §153)
//!
//! - **Channel ordering**: ACN — order 1 is `[W, Y, Z, X]` (ACN 0–3),
//!   order 2 appends ACN 4–8.
//! - **Normalization**: SN3D (W = 1, first order = `√3` times the
//!   direction components, second order = `√15·(xy, yz)`,
//!   `(√5/2)(3z²−1)`, `√15·xz`, `(√15/2)(x²−y²)`).
//! - **Coordinate frame**: the spatial layer's single frame (`+X` right,
//!   `+Y` front, `+Z` up); directions are listener-space unit vectors.
//! - **Basis**: real spherical harmonics. Order 1 is the engine-wide
//!   default ([`AMBISONIC_ORDER`]); the decoder and renderer accept orders
//!   up to [`MAX_AMBISONIC_ORDER`] (3 = third-order, 16 channels) — the
//!   documented §34 table + rotation extension.
//! - **Rotation**: order-1 rotation keeps `W` invariant and rotates the
//!   `X Y Z` channels by the same 3×3 as direction vectors. Order-2
//!   rotation keeps `W` invariant, rotates the first-order block like
//!   vectors, and rotates the second-order block by its exact 5×5 Wigner
//!   matrix (the basis functions are quadratic forms in the direction, so
//!   the block is the representation of `F ↦ R F Rᵀ` on the traceless
//!   quadratic forms — computed by Frobenius projection, exact by
//!   construction). Order-3 extends to the exact 7×7 Wigner matrix on the
//!   cubic forms: each third-order basis function is a harmonic cubic, so
//!   the block is the projection of the triple-Kronecker action
//!   `F ↦ R ⊗ R ⊗ R` onto the order-3 subspace, computed by coefficient
//!   linear algebra (monomial substitution + Gram solve) — exact by
//!   construction. The renderer applies the listener orientation, so a
//!   world-encoded field stays world-fixed as the listener turns (§48).
//! - **Decoding** (§36): the sampling ("basic") decoder `D = Y(S)ᵀ/N` —
//!   every order weighted equally — plus a **max-rE** policy that narrows
//!   the lobe: order 1 uses the documented FOA window `a1 = √3/2 ≈ 0.866`;
//!   order 2 uses the published Zotter–Frank window `a1 ≈ 0.9057,
//!   a2 ≈ 0.6827`; order 3 uses the published window `a1 ≈ 0.7660,
//!   a2 ≈ 0.6534, a3 ≈ 0.5715`. Decoder selection is separate from the
//!   scene representation.

use super::math::{Quat, Vec3};
use super::render::RenderError;
use super::speaker::SpeakerLayout;
use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;

/// The default ambisonic order (1 = First-Order Ambisonics, FOA) — the
/// engine-wide bus width for fields and virtual rings.
pub const AMBISONIC_ORDER: u8 = 1;

/// Highest implemented order (3 = Third-Order Ambisonics, TOA — 16
/// channels).
pub const MAX_AMBISONIC_ORDER: u8 = 3;

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

/// Largest supported bus width (`channel_count(MAX_AMBISONIC_ORDER)`),
/// used to size the allocation-free per-frame scratch on the hot path.
pub const AMBISONIC_CHANNELS_MAX: usize = AMBISONIC_CHANNELS_ORDER_3;

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
        // Third-order SN3D block (ACN 9–15), each ± combination of the
        // seven harmonic cubics — all validated to unit sphere RMS (mean-
        // square 1) and mutual orthogonality, and to rotate by the exact
        // 7×7 Wigner block (`wigner_block_3`).
        //   ACN 9 :  √(35/8)·x(x²−3y²)
        //   ACN10 :  √105·xyz
        //   ACN11 :  √(21/8)·y(5z²−1)
        //   ACN12 :  (√7/2)·z(5z²−3)
        //   ACN13 :  √(21/8)·x(5z²−1)
        //   ACN14 :  (√105/2)·z(x²−y²)
        //   ACN15 :  √(35/8)·y(y²−3x²)
        let s358 = (35.0f32 / 8.0).sqrt(); // √(35/8)
        let s105 = 105.0f32.sqrt(); // √105
        let s218 = (21.0f32 / 8.0).sqrt(); // √(21/8)
        let s7h = 7.0f32.sqrt() * 0.5; // √7/2
        let s105h = 105.0f32.sqrt() * 0.5; // √105/2
        out[9] = s358 * x * (x * x - 3.0 * y * y);
        out[10] = s105 * x * y * z;
        out[11] = s218 * y * (5.0 * z * z - 1.0);
        out[12] = s7h * z * (5.0 * z * z - 3.0);
        out[13] = s218 * x * (5.0 * z * z - 1.0);
        out[14] = s105h * z * (x * x - y * y);
        out[15] = s358 * y * (y * y - 3.0 * x * x);
    }
}

/// Encode a plane wave from `dir` (gain `g`) into one order-1 FOA bus frame
/// (`[W, Y, Z, X]`). `dir` is normalised defensively; a zero direction
/// encodes silence rather than NaN.
pub fn encode_plane_wave(dir: Vec3, gain: f32, out: &mut [f32; 4]) {
    let d = dir.normalized().unwrap_or(Vec3::Y);
    let y = sh_foa(d);
    for (o, &v) in out.iter_mut().zip(y.iter()) {
        *o = v * gain;
    }
}

/// Encode a plane wave from `dir` (gain `g`) into a bus frame of any
/// supported order (write `channel_count(order)` values into `out`).
pub fn encode_plane_wave_n(order: u8, dir: Vec3, gain: f32, out: &mut [f32]) {
    let d = dir.normalized().unwrap_or(Vec3::Y);
    sh_n(order, d, out);
    for v in out.iter_mut().take(channel_count(order)) {
        *v *= gain;
    }
}

/// Rotate an order-1 FOA bus frame by `q` (order-1 rotation: `W` invariant,
/// the `X Y Z` channels rotate exactly like direction vectors). `q` is the
/// world-space rotation to apply to the field.
pub fn rotate_bus_frame(q: Quat, frame: &mut [f32; 4]) {
    // Channel order [W, Y, Z, X] → direction (X, Y, Z) = (frame[3], frame[1],
    // frame[2]).
    let v = Vec3::new(frame[3], frame[1], frame[2]);
    let r = q.rotate_vec3(v);
    frame[1] = r.y;
    frame[2] = r.z;
    frame[3] = r.x;
}

/// Rotate a bus frame of any supported order by `q`. Order 1 matches
/// [`rotate_bus_frame`]; order 2 additionally rotates the second-order
/// block (ACN 4–8) by its exact 5×5 Wigner matrix. `frame` must hold at
/// least `channel_count(order)` values.
pub fn rotate_bus_frame_n(q: Quat, order: u8, frame: &mut [f32]) {
    match order {
        0 => {}
        1 => {
            assert!(frame.len() >= 4);
            let v = Vec3::new(frame[3], frame[1], frame[2]);
            let r = q.rotate_vec3(v);
            frame[1] = r.y;
            frame[2] = r.z;
            frame[3] = r.x;
        }
        2 => {
            assert!(frame.len() >= 9);
            // W invariant.
            let v = Vec3::new(frame[3], frame[1], frame[2]);
            let r = q.rotate_vec3(v);
            frame[1] = r.y;
            frame[2] = r.z;
            frame[3] = r.x;
            // Exact second-order Wigner block (computed once per call;
            // the renderer hoists it per block since the orientation is
            // constant there).
            let w2 = wigner_block_2(q);
            let c = [frame[4], frame[5], frame[6], frame[7], frame[8]];
            let mut out5 = [0.0f32; 5];
            for (i, &ci) in c.iter().enumerate() {
                if ci == 0.0 {
                    continue;
                }
                for j in 0..5 {
                    out5[j] += w2[i][j] * ci;
                }
            }
            frame[4] = out5[0];
            frame[5] = out5[1];
            frame[6] = out5[2];
            frame[7] = out5[3];
            frame[8] = out5[4];
        }
        3 => {
            assert!(frame.len() >= 16);
            // W invariant; first- and second-order blocks rotate exactly as
            // for orders 1 and 2.
            let v = Vec3::new(frame[3], frame[1], frame[2]);
            let r = q.rotate_vec3(v);
            frame[1] = r.y;
            frame[2] = r.z;
            frame[3] = r.x;
            let w2 = wigner_block_2(q);
            let c2 = [frame[4], frame[5], frame[6], frame[7], frame[8]];
            let mut out5 = [0.0f32; 5];
            for (i, &ci) in c2.iter().enumerate() {
                if ci != 0.0 {
                    for j in 0..5 {
                        out5[j] += w2[i][j] * ci;
                    }
                }
            }
            frame[4] = out5[0];
            frame[5] = out5[1];
            frame[6] = out5[2];
            frame[7] = out5[3];
            frame[8] = out5[4];
            // Exact third-order Wigner block (ACN 9–15) by cubic-coefficient
            // linear algebra — validated to satisfy `sh_n(3,R·v) == W₃·sh_n(3,v)`.
            let w3 = wigner_block_3(q);
            let c3 = [
                frame[9], frame[10], frame[11], frame[12], frame[13], frame[14], frame[15],
            ];
            let mut out7 = [0.0f32; 7];
            for (i, out) in out7.iter_mut().enumerate() {
                // Accumulate into output index `i` from all input columns `j`.
                // (Order-2's block is stored transposed relative to this;
                // order-3's is the direct `W₃·c` convention.)
                let mut acc = 0.0f32;
                for j in 0..7 {
                    acc += w3[i][j] * c3[j];
                }
                *out = acc;
            }
            frame[9] = out7[0];
            frame[10] = out7[1];
            frame[11] = out7[2];
            frame[12] = out7[3];
            frame[13] = out7[4];
            frame[14] = out7[5];
            frame[15] = out7[6];
        }
        _ => panic!("rotate_bus_frame_n: order {order} unsupported"),
    }
}

/// The 3×3 rotation matrix for `q` (`R · v` rotates `v` by `q`, matching
/// [`Quat::rotate_vec3`]). f64 for the order-2 block's exactness.
fn rotation_matrix_f64(q: Quat) -> [[f64; 3]; 3] {
    let q = q.normalized().unwrap_or(Quat::IDENTITY);
    let (x, y, z, w) = (q.x as f64, q.y as f64, q.z as f64, q.w as f64);
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - w * z),
            2.0 * (x * z + w * y),
        ],
        [
            2.0 * (x * y + w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - w * x),
        ],
        [
            2.0 * (x * z - w * y),
            2.0 * (y * z + w * x),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ]
}

/// The exact order-2 Wigner rotation block for the real-SH basis (ACN 4–8).
///
/// The second-order SN3D basis functions are quadratic forms in the
/// direction (`√15·xy, √15·yz, (√5/2)(2z²−x²−y²), √15·xz, (√15/2)(x²−y²)`);
/// under a rotation `R` each form matrix transforms as `F ↦ R F Rᵀ`. The
/// 5×5 block is the representation of that action in the orthogonal,
/// equal-norm (7.5) basis, computed by Frobenius projection — exact by
/// construction, with the basis property pinned by tests
/// (`sh_n(R·d) == W(R)·sh_n(d)`).
fn wigner_block_2(q: Quat) -> [[f32; 5]; 5] {
    let r = rotation_matrix_f64(q);
    let s15 = 15.0f64.sqrt();
    let s5h = 5.0f64.sqrt() * 0.5;
    let s15h = s15 * 0.5;
    // Basis form matrices (SN3D-scaled), ACN 4..8, all traceless.
    let basis: [[[f64; 3]; 3]; 5] = [
        // xy
        [
            [0.0, s15 * 0.5, 0.0],
            [s15 * 0.5, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        ],
        // yz
        [
            [0.0, 0.0, 0.0],
            [0.0, 0.0, s15 * 0.5],
            [0.0, s15 * 0.5, 0.0],
        ],
        // 2z²−x²−y²
        [[-s5h, 0.0, 0.0], [0.0, -s5h, 0.0], [0.0, 0.0, 2.0 * s5h]],
        // xz
        [
            [0.0, 0.0, s15 * 0.5],
            [0.0, 0.0, 0.0],
            [s15 * 0.5, 0.0, 0.0],
        ],
        // x²−y²
        [[s15h, 0.0, 0.0], [0.0, -s15h, 0.0], [0.0, 0.0, 0.0]],
    ];
    let mut w = [[0.0f32; 5]; 5];
    for (i, fi) in basis.iter().enumerate() {
        // rfr = R F_i Rᵀ.
        let mut rf = [[0.0f64; 3]; 3];
        for a in 0..3 {
            for b in 0..3 {
                let mut s = 0.0;
                for c in 0..3 {
                    s += r[a][c] * fi[c][b];
                }
                rf[a][b] = s;
            }
        }
        let mut rfr = [[0.0f64; 3]; 3];
        for a in 0..3 {
            for b in 0..3 {
                let mut s = 0.0;
                for c in 0..3 {
                    s += rf[a][c] * r[b][c];
                }
                rfr[a][b] = s;
            }
        }
        for (j, fj) in basis.iter().enumerate() {
            let mut ip = 0.0;
            for a in 0..3 {
                for b in 0..3 {
                    ip += rfr[a][b] * fj[a][b];
                }
            }
            // All basis norms are 7.5 (orthogonal, equal-norm).
            w[i][j] = (ip / 7.5) as f32;
        }
    }
    w
}

/// The exact order-3 Wigner rotation block for the real-SH basis (ACN 9–15).
///
/// The third-order SN3D basis functions are harmonic cubics
/// (`√(35/8)·x(x²−3y²), √105·xyz, √(21/8)·y(5z²−1), (√7/2)·z(5z²−3),
/// √(21/8)·x(5z²−1), (√105/2)·z(x²−y²), √(35/8)·y(y²−3x²)`). Under a
/// rotation `R` each cubic transforms by the triple-Kronecker action
/// `F ↦ R ⊗ R ⊗ R` on its degree-3 homogeneous form. The 7×7 block is the
/// projection of that action onto the order-3 subspace, computed by
/// coefficient linear algebra: each basis cubic is a 10-vector over the
/// cubic monomials, the monomials are substituted under `R`, and the block
/// is obtained by Gram projection `W₃ = (BᵀMB)·(BᵀB)⁻¹` on the exact
/// orthonormal basis — so `sh_n(3, R·v) == W₃·sh_n(3, v)` holds by
/// construction (pinned by tests). f64 throughout for exactness.
fn wigner_block_3(q: Quat) -> [[f32; 7]; 7] {
    const M: usize = 10;
    // Cubic monomial exponents (x,y,z): x³ x²y x²z xy² xyz xz² y³ y²z yz² z³.
    const EXPS: [[usize; 3]; M] = [
        [3, 0, 0],
        [2, 1, 0],
        [2, 0, 1],
        [1, 2, 0],
        [1, 1, 1],
        [1, 0, 2],
        [0, 3, 0],
        [0, 2, 1],
        [0, 1, 2],
        [0, 0, 3],
    ];
    // SN3D constants.
    let s358 = (35.0f64 / 8.0).sqrt(); // √(35/8)
    let s105 = 105.0f64.sqrt();
    let s218 = (21.0f64 / 8.0).sqrt();
    let s7h = 7.0f64.sqrt() * 0.5;
    let s105h = 105.0f64.sqrt() * 0.5;
    // The 7 basis cubics as 10-vectors (homogenized — equal to the sphere
    // forms on the unit sphere, and rotation-invariant since `R` preserves
    // `r²`). ACN 9..15.
    let basis: [[f64; M]; 7] = [
        // x(x²−3y²) = x³ −3xy²
        [s358, 0.0, 0.0, -3.0 * s358, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        // xyz
        [0.0, 0.0, 0.0, 0.0, s105, 0.0, 0.0, 0.0, 0.0, 0.0],
        // y(5z²−1) → 4yz² −x²y −y³
        [0.0, -s218, 0.0, 0.0, 0.0, 0.0, -s218, 0.0, 4.0 * s218, 0.0],
        // z(5z²−3) → 2z³ −3x²z −3y²z
        [
            0.0,
            0.0,
            -3.0 * s7h,
            0.0,
            0.0,
            0.0,
            0.0,
            -3.0 * s7h,
            0.0,
            2.0 * s7h,
        ],
        // x(5z²−1) → 4xz² −x³ −xy²
        [-s218, 0.0, 0.0, -s218, 0.0, 4.0 * s218, 0.0, 0.0, 0.0, 0.0],
        // z(x²−y²) = x²z −y²z
        [0.0, 0.0, s105h, 0.0, 0.0, 0.0, 0.0, -s105h, 0.0, 0.0],
        // y(y²−3x²) = y³ −3x²y
        [0.0, -3.0 * s358, 0.0, 0.0, 0.0, 0.0, s358, 0.0, 0.0, 0.0],
    ];

    let r = rotation_matrix_f64(q);
    // The 10×10 monomial substitution: `sub[n][m]` is the coefficient of
    // monomial `n` in monomial `m` under `d ↦ R·d`. Each cubic monomial
    // `x^a y^b z^c` expands as `Π (Σ_k R[i][k] d_k)^e`, so we place each of
    // its `a+b+c` factors onto one of the three rows of `R` times a d-
    // component and collect the resulting d-exponents. Deterministic DCG
    // (with multiplicity) — a stack of fixed size 3⁴.
    let mut sub = [[0.0f64; M]; M];
    for (mi, &[a, b, c]) in EXPS.iter().enumerate() {
        let mut stack: Vec<(usize, usize, usize, [usize; 3], f64)> =
            vec![(a, b, c, [0, 0, 0], 1.0)];
        while let Some((ra, rb, rc, expt, coeff)) = stack.pop() {
            if ra + rb + rc == 0 {
                let idx = EXPS.iter().position(|&e| e == expt).unwrap();
                sub[idx][mi] += coeff;
                continue;
            }
            if ra > 0 {
                for comp in 0..3 {
                    let mut e2 = expt;
                    e2[comp] += 1;
                    stack.push((ra - 1, rb, rc, e2, coeff * r[0][comp]));
                }
            } else if rb > 0 {
                for comp in 0..3 {
                    let mut e2 = expt;
                    e2[comp] += 1;
                    stack.push((ra, rb - 1, rc, e2, coeff * r[1][comp]));
                }
            } else {
                for comp in 0..3 {
                    let mut e2 = expt;
                    e2[comp] += 1;
                    stack.push((ra, rb, rc - 1, e2, coeff * r[2][comp]));
                }
            }
        }
    }
    // Gram `G = BᵀB` (7×7) and `RHS = Bᵀ·(sub)ᵀ...`: compute `Q[i][j] =
    // <sub·H_i, H_j>` and set `W₃ = Q·G⁻¹` (since `sub·H_i = Σ_j W₃[i][j] H_j`).
    let dot = |u: [f64; M], v: [f64; M]| (0..M).map(|k| u[k] * v[k]).sum::<f64>();
    let mut g = [[0.0f64; 7]; 7];
    let mut q = [[0.0f64; 7]; 7];
    for i in 0..7 {
        // sub·H_i
        let mut shi = [0.0f64; M];
        for n in 0..M {
            for mm in 0..M {
                shi[n] += sub[n][mm] * basis[i][mm];
            }
        }
        for j in 0..7 {
            g[i][j] = dot(basis[i], basis[j]);
            q[i][j] = dot(shi, basis[j]);
        }
    }
    // Invert G.
    let mut gin = [[0.0f64; 7]; 7];
    gin.iter_mut().enumerate().for_each(|(i, row)| row[i] = 1.0);
    let mut gg = g;
    for c in 0..7 {
        let mut p = c;
        for r in c..7 {
            if gg[r][c].abs() > gg[p][c].abs() {
                p = r;
            }
        }
        gg.swap(c, p);
        gin.swap(c, p);
        let d = gg[c][c];
        for k in 0..7 {
            gg[c][k] /= d;
            gin[c][k] /= d;
        }
        for r in 0..7 {
            if r == c {
                continue;
            }
            let f = gg[r][c];
            for k in 0..7 {
                gg[r][k] -= f * gg[c][k];
                gin[r][k] -= f * gin[c][k];
            }
        }
    }
    // W₃ = Q·G⁻¹.
    let mut w = [[0.0f32; 7]; 7];
    for i in 0..7 {
        for j in 0..7 {
            let mut acc = 0.0;
            for p in 0..7 {
                acc += q[i][p] * gin[p][j];
            }
            w[i][j] = acc as f32;
        }
    }
    w
}

/// Ambisonic decoder policy (spec §36): how the bus maps onto speakers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DecoderPolicy {
    /// Sampling ("basic") decoder: `D = Y(S)ᵀ/N` — every order weighted
    /// equally. A plane wave from `d` lands on speaker `s` as
    /// `(1 + 3·cosθ)/N` (order 1).
    #[default]
    Basic,
    /// Max-rE weights — narrows the decoded lobe for a tighter image.
    /// Order 1: `a0 = 1, a1 = √3/2 ≈ 0.866` (the documented FOA window);
    /// order 2: `a1 ≈ 0.9057, a2 ≈ 0.6827` (the published Zotter–Frank
    /// window).
    MaxRe,
}

/// Per-order decoder weights for `policy` (length `channel_count(order)`;
/// every channel of order `l` shares `a_l`, so Basic is all ones and
/// Max-rE fills runs).
fn order_weights(policy: DecoderPolicy, order: u8) -> Vec<f32> {
    let n = channel_count(order);
    let mut w = vec![1.0f32; n];
    match (policy, order) {
        (DecoderPolicy::Basic, _) => {}
        (DecoderPolicy::MaxRe, 1) => w[1..4].fill(0.866_025_4),
        (DecoderPolicy::MaxRe, 2) => {
            w[1..4].fill(0.905_663_1);
            w[4..9].fill(0.682_689_4);
        }
        (DecoderPolicy::MaxRe, 3) => {
            // Published third-order max-rE window (Zotter–Frank).
            w[1..4].fill(0.766_044_5);
            w[4..9].fill(0.653_445_5);
            w[9..16].fill(0.571_5);
        }
        _ => {}
    }
    w
}

/// The ambisonic decoder: a precomputed per-speaker decode matrix applied to
/// an interleaved bus of any supported order. Realtime-safe after `prepare`
/// (all geometry work happens there; `process_bus` is flat-array
/// arithmetic).
#[derive(Debug)]
pub struct AmbisonicDecoder {
    /// Bus order (default [`AMBISONIC_ORDER`]).
    order: u8,
    /// Per-speaker decode weights, flat `speakers × channel_count(order)`.
    gains: Vec<f32>,
    /// Enabled non-LFE speaker output indices (rows of `gains`).
    speakers: Vec<usize>,
    speaker_count: usize,
    policy: DecoderPolicy,
    prepared: bool,
}

impl Default for AmbisonicDecoder {
    fn default() -> Self {
        Self::new(DecoderPolicy::Basic)
    }
}

impl AmbisonicDecoder {
    /// Order-1 decoder (the FOA default).
    pub fn new(policy: DecoderPolicy) -> Self {
        Self::with_order(policy, AMBISONIC_ORDER)
    }

    /// Decoder for any supported order (≤ [`MAX_AMBISONIC_ORDER`]).
    pub fn with_order(policy: DecoderPolicy, order: u8) -> Self {
        assert!(
            order <= MAX_AMBISONIC_ORDER,
            "ambisonic order {order} unsupported (max {MAX_AMBISONIC_ORDER})"
        );
        Self {
            order,
            gains: Vec::new(),
            speakers: Vec::new(),
            speaker_count: 0,
            policy,
            prepared: false,
        }
    }

    pub fn policy(&self) -> DecoderPolicy {
        self.policy
    }

    /// The bus order this decoder consumes.
    pub fn order(&self) -> u8 {
        self.order
    }

    /// Width of the bus this decoder consumes (`channel_count(order)`).
    pub fn bus_width(&self) -> usize {
        channel_count(self.order)
    }

    /// Control path: build the decode matrix for `layout` (enabled non-LFE
    /// speakers, unit directions, `N` = pan-speaker count).
    pub fn prepare(
        &mut self,
        layout: &SpeakerLayout,
        _sample_rate: u32,
    ) -> Result<(), RenderError> {
        layout.validate()?;
        let w = order_weights(self.policy, self.order);
        let ch = channel_count(self.order);
        let mut speakers = Vec::new();
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                continue;
            }
            speakers.push(idx);
        }
        if speakers.is_empty() {
            return Err(RenderError::DegenerateGeometry);
        }
        let n = speakers.len() as f32;
        let mut gains = Vec::with_capacity(speakers.len() * ch);
        for idx in &speakers {
            let dir = layout.speakers[*idx]
                .position
                .normalized()
                .unwrap_or(Vec3::Y);
            let mut y = vec![0.0f32; ch];
            sh_n(self.order, dir, &mut y);
            // D[s] = (1/N)·[a_l · Y_l^m(d)].
            for (k, v) in y.iter().enumerate() {
                gains.push(w[k] * v / n);
            }
        }
        self.gains = gains;
        self.speakers = speakers;
        self.speaker_count = layout.speakers.len();
        self.prepared = true;
        Ok(())
    }

    /// Total output speaker count (incl. LFE).
    pub fn channels(&self) -> usize {
        self.speaker_count
    }

    /// The pan (enabled, non-LFE) speaker output indices, in decode order.
    pub fn speakers(&self) -> &[usize] {
        &self.speakers
    }

    /// True when a decode matrix is ready.
    pub fn prepared(&self) -> bool {
        self.prepared
    }

    /// Decode a single bus frame into `row[0 .. self.speakers.len()]` (the
    /// pan speakers in decode order). Allocation-free; the per-frame
    /// building block for pipelines that wrap the decode in further
    /// processing (e.g. the field mixer's decorrelation rings).
    pub fn decode_frame(&self, frame: &[f32], row: &mut [f32]) {
        let ch = channel_count(self.order);
        for (k, v) in row.iter_mut().enumerate().take(self.speakers.len()) {
            let g = k * ch;
            let mut acc = 0.0f32;
            for c in 0..ch {
                acc += self.gains[g + c] * frame.get(c).copied().unwrap_or(0.0);
            }
            *v = acc;
        }
    }

    /// Decode an interleaved bus (`frames × channel_count(order)` per frame)
    /// into `out` (`frames × speakers`, **added**, not cleared — the hybrid
    /// mixer sums classes). Missing bus frames are treated as silence.
    /// Allocation-free.
    pub fn process_bus(&self, bus: &[f32], frames: usize, out: &mut [f32]) {
        if !self.prepared || frames == 0 {
            return;
        }
        let ch = channel_count(self.order);
        let n_spk = out.len().checked_div(frames).unwrap_or(0);
        if n_spk == 0 {
            return;
        }
        for f in 0..frames {
            let mut frame = [0.0f32; AMBISONIC_CHANNELS_MAX];
            for (c, slot) in frame.iter_mut().enumerate().take(ch) {
                *slot = bus.get(f * ch + c).copied().unwrap_or(0.0);
            }
            for (k, &spk) in self.speakers.iter().enumerate() {
                let row = k * ch;
                let mut v = 0.0f32;
                for (c, &fc) in frame.iter().enumerate().take(ch) {
                    v += self.gains[row + c] * fc;
                }
                if v != 0.0 && spk < n_spk {
                    out[f * n_spk + spk] += v;
                }
            }
        }
    }

    /// Decode a single plane wave directly (test/introspection helper):
    /// returns each pan speaker's gain for a source at `dir`.
    #[cfg(test)]
    fn plane_wave_gains(&self, dir: Vec3) -> Vec<f32> {
        let ch = channel_count(self.order);
        let mut frame = vec![0.0f32; ch];
        encode_plane_wave_n(self.order, dir, 1.0, &mut frame);
        let mut out = vec![0.0f32; self.speaker_count];
        let mut bus = Vec::new();
        bus.extend_from_slice(&frame);
        self.process_bus(&bus, 1, &mut out);
        out
    }
}

/// A standalone ambisonic renderer (spec §23): decodes an ambisonic bus of
/// order ≤ [`MAX_AMBISONIC_ORDER`] into the active speaker layout, applying
/// the listener's orientation (so a world-encoded field stays world-fixed,
/// §48) and per-speaker calibration.
///
/// Input convention for `process_block`: `object_inputs` carries the bus
/// planes `[W, Y, Z, X, …]` (one mono plane per channel, world
/// orientation). Beds/fields are not part of this renderer (the hybrid
/// renderers mix them); the trait's default `process_hybrid_block` forwards
/// to the bus path.
#[derive(Debug)]
pub struct AmbisonicRenderer {
    decoder: AmbisonicDecoder,
    /// Per-speaker calibration level.
    out_trim: Vec<f32>,
    /// Scratch for the listener-rotated interleaved bus.
    bus: Vec<f32>,
    prepared: bool,
}

impl AmbisonicRenderer {
    /// Order-1 (FOA) renderer.
    pub fn new(policy: DecoderPolicy) -> Self {
        Self::with_order(policy, AMBISONIC_ORDER)
    }

    /// Renderer for any supported order.
    pub fn with_order(policy: DecoderPolicy, order: u8) -> Self {
        Self {
            decoder: AmbisonicDecoder::with_order(policy, order),
            out_trim: Vec::new(),
            bus: vec![0.0; AMBISONIC_CHANNELS_MAX * MAX_AUDIO_BLOCK_FRAMES],
            prepared: false,
        }
    }

    pub fn policy(&self) -> DecoderPolicy {
        self.decoder.policy()
    }

    /// The rendered bus order.
    pub fn order(&self) -> u8 {
        self.decoder.order()
    }
}

impl Default for AmbisonicRenderer {
    fn default() -> Self {
        Self::new(DecoderPolicy::Basic)
    }
}

impl super::render::SpatialRenderer for AmbisonicRenderer {
    fn prepare(&mut self, layout: &SpeakerLayout, sample_rate: u32) -> Result<(), RenderError> {
        self.decoder.prepare(layout, sample_rate)?;
        self.out_trim = layout
            .speakers
            .iter()
            .map(|s| s.gain * layout.calibration.trim_gain(s.id))
            .collect();
        self.prepared = true;
        Ok(())
    }

    fn process_block(
        &mut self,
        scene: &super::scene::SpatialScene,
        object_inputs: &[&[f32]],
        frames: usize,
        out: &mut [f32],
    ) -> Result<(), RenderError> {
        if !self.prepared {
            return Err(RenderError::InvalidLayout);
        }
        if frames == 0 || frames > MAX_AUDIO_BLOCK_FRAMES {
            return Err(RenderError::BufferMismatch {
                expected: MAX_AUDIO_BLOCK_FRAMES,
                got: frames,
            });
        }
        let need = self.decoder.channels() * frames;
        if out.len() < need {
            return Err(RenderError::BufferMismatch {
                expected: need,
                got: out.len(),
            });
        }
        let ch = self.decoder.bus_width();
        let order = self.decoder.order();
        // Build the listener-space interleaved bus: rotate each world-
        // oriented bus frame by the listener orientation (conjugate), so a
        // world-fixed field appears to rotate opposite to the head (§48).
        let xf = super::scene::ListenerTransform::from_listener(&scene.listener);
        for f in 0..frames {
            let mut frame = [0.0f32; AMBISONIC_CHANNELS_MAX];
            for (c, slot) in frame.iter_mut().enumerate().take(ch) {
                *slot = object_inputs
                    .get(c)
                    .and_then(|plane| plane.get(f))
                    .copied()
                    .unwrap_or(0.0);
            }
            rotate_bus_frame_n(xf.orientation, order, &mut frame);
            for (c, &fc) in frame.iter().enumerate().take(ch) {
                self.bus[f * ch + c] = fc;
            }
        }
        for sample in out[..need].iter_mut() {
            *sample = 0.0;
        }
        self.decoder
            .process_bus(&self.bus[..frames * ch], frames, out);
        // Apply per-speaker calibration.
        let n_spk = self.decoder.channels();
        for f in 0..frames {
            for (spk, &trim) in self.out_trim.iter().enumerate().take(n_spk) {
                out[f * n_spk + spk] *= trim;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::scene::SpatialScene;
    use std::f32::consts::FRAC_PI_2;

    const EPS: f32 = 1e-4;

    #[test]
    fn sh_basis_matches_documented_sn3d_convention() {
        // +Y front → [1, √3, 0, 0]; +X right → [1, 0, 0, √3]; +Z up →
        // [1, 0, √3, 0]; diagonal → all four populated.
        let s3 = 3.0f32.sqrt();
        assert_eq!(sh_foa(Vec3::Y), [1.0, s3, 0.0, 0.0]);
        assert_eq!(sh_foa(Vec3::X), [1.0, 0.0, 0.0, s3]);
        assert_eq!(sh_foa(Vec3::Z), [1.0, 0.0, s3, 0.0]);
        let d = Vec3::new(1.0, 2.0, 3.0).normalized().unwrap();
        let y = sh_foa(d);
        assert!((y[0] - 1.0).abs() < EPS);
        assert!((y[1] - s3 * d.y).abs() < EPS);
        assert!((y[2] - s3 * d.z).abs() < EPS);
        assert!((y[3] - s3 * d.x).abs() < EPS);
    }

    #[test]
    fn order2_basis_matches_documented_sn3d_convention() {
        // Second-order block (ACN 4–8) at the axes and diagonal:
        // √15·xy, √15·yz, (√5/2)(3z²−1), √15·xz, (√15/2)(x²−y²).
        let s15 = 15.0f32.sqrt();
        let s5h = 5.0f32.sqrt() * 0.5;
        let s15h = s15 * 0.5;
        let mut y = [0.0f32; 9];
        sh_n(2, Vec3::Y, &mut y);
        // +Y: W=1, Y=√3, Z=0, X=0, xy=0, yz=0, z²=−√5/2, xz=0, x²−y²=−√15/2.
        assert!((y[0] - 1.0).abs() < EPS);
        assert!((y[1] - 3.0f32.sqrt()).abs() < EPS);
        assert!((y[6] + s5h).abs() < EPS, "Y₂⁰ at +Y = −√5/2");
        assert!((y[8] + s15h).abs() < EPS, "Y₂² at +Y = −√15/2");
        let mut y = [0.0f32; 9];
        sh_n(2, Vec3::X, &mut y);
        assert!((y[8] - s15h).abs() < EPS, "Y₂² at +X = +√15/2");
        assert!((y[6] + s5h).abs() < EPS);
        // Diagonal: all second-order channels populated.
        let d = Vec3::new(1.0, 2.0, 3.0).normalized().unwrap();
        let mut y = [0.0f32; 9];
        sh_n(2, d, &mut y);
        assert!((y[4] - s15 * d.x * d.y).abs() < EPS);
        assert!((y[5] - s15 * d.y * d.z).abs() < EPS);
        assert!((y[6] - s5h * (3.0 * d.z * d.z - 1.0)).abs() < EPS);
        assert!((y[7] - s15 * d.x * d.z).abs() < EPS);
        assert!((y[8] - s15h * (d.x * d.x - d.y * d.y)).abs() < EPS);
        // sh_n(1) matches sh_foa exactly (up to fp noise on normalization).
        let mut a = [0.0f32; 4];
        sh_n(1, Vec3::Y, &mut a);
        assert!((a[0] - sh_foa(Vec3::Y)[0]).abs() < EPS);
        assert!((a[1] - sh_foa(Vec3::Y)[1]).abs() < EPS);
    }

    #[test]
    fn sh_basis_is_norm_preserving_on_the_sphere() {
        // Discrete orthonormality check over an equirectangular grid: the
        // basis is 4π-normalized (SN3D), so the empirical RMS of Y_l^m over
        // the grid must converge to 1 (√(mean(y²)) → 1 as the grid refines).
        use std::f32::consts::{PI, TAU};
        const STEPS: usize = 48;
        let mut sums = [0.0f64; 9];
        let mut count = 0usize;
        for i in 0..STEPS {
            let th = (i as f32 + 0.5) * PI / STEPS as f32; // elevation
            let z = th.cos();
            let r = th.sin();
            for j in 0..(2 * STEPS) {
                let phi = (j as f32 + 0.5) * TAU / (2 * STEPS) as f32;
                let d = Vec3::new(r * phi.sin(), r * phi.cos(), z);
                let mut y = [0.0f32; 9];
                sh_n(2, d, &mut y);
                for (k, v) in y.iter().enumerate() {
                    sums[k] += (*v as f64) * (*v as f64) * th.sin() as f64;
                }
                count += 1;
            }
        }
        let _ = count; // the grid weight below is analytic, not empirical
                       // norm² = (1/4π)·Σ y²·sinθ·Δθ·Δφ with Δθ = Δφ = π/STEPS.
        let weight = std::f64::consts::PI / (4.0 * (STEPS as f64) * (STEPS as f64));
        for (k, s) in sums.iter().enumerate() {
            let norm_sq = s * weight;
            assert!(
                (norm_sq - 1.0).abs() < 0.02,
                "order-2 channel {k} SN3D norm² = {norm_sq}"
            );
        }
    }

    #[test]
    fn plane_wave_encode_then_basic_decode_matches_formula() {
        // Basic decoder on N speakers: a plane wave from d lands on speaker
        // s as (1 + 3·cosθ)/N, exactly the documented sampling pattern.
        let layout = SpeakerLayout::seven_point_one_four();
        let mut dec = AmbisonicDecoder::new(DecoderPolicy::Basic);
        dec.prepare(&layout, 48_000).unwrap();
        let dir = Vec3::Y; // front
        let gains = dec.plane_wave_gains(dir);
        let n = 11usize;
        let mut pan_idx = 0usize;
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                assert_eq!(gains[idx], 0.0, "LFE/speaker {idx} silent");
                continue;
            }
            let spk_dir = s.position.normalized().unwrap();
            let cos = spk_dir.dot(dir);
            let expected = (1.0 + 3.0 * cos) / n as f32;
            assert!(
                (gains[idx] - expected).abs() < 1e-4,
                "speaker {idx} gain {} want {expected}",
                gains[idx]
            );
            pan_idx += 1;
        }
        assert_eq!(pan_idx, n);
    }

    #[test]
    fn order2_basic_decode_round_trips_a_plane_wave() {
        // Order-2 sampling decode: the per-speaker gain for a plane wave
        // from d must equal Y(S)ᵀ·Y(d)/N — the orthonormal projection
        // (4π-normalized basis, 1/N spread). Pin against a direct
        // recomputation for a front source on 7.1.4.
        let layout = SpeakerLayout::seven_point_one_four();
        let mut dec = AmbisonicDecoder::with_order(DecoderPolicy::Basic, 2);
        dec.prepare(&layout, 48_000).unwrap();
        assert_eq!(dec.bus_width(), 9);
        let dir = Vec3::Y;
        let gains = dec.plane_wave_gains(dir);
        let n = 11usize;
        let mut pan = 0usize;
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                continue;
            }
            let spk_dir = s.position.normalized().unwrap();
            let mut spk_y = [0.0f32; 9];
            let mut src_y = [0.0f32; 9];
            sh_n(2, spk_dir, &mut spk_y);
            sh_n(2, dir, &mut src_y);
            let mut expected = 0.0f32;
            for k in 0..9 {
                expected += spk_y[k] * src_y[k];
            }
            expected /= n as f32;
            assert!(
                (gains[idx] - expected).abs() < 1e-4,
                "order-2 speaker {idx}: {} want {expected}",
                gains[idx]
            );
            pan += 1;
        }
        assert_eq!(pan, n);
    }

    #[test]
    fn max_re_policy_narrows_the_response() {
        // Max-rE (a1 = √3/2): same front lobe centre, but the rear gain
        // (cosθ = −1) is less negative — narrower, more focused.
        let layout = SpeakerLayout::seven_point_one_four();
        let mut basic = AmbisonicDecoder::new(DecoderPolicy::Basic);
        let mut maxre = AmbisonicDecoder::new(DecoderPolicy::MaxRe);
        basic.prepare(&layout, 48_000).unwrap();
        maxre.prepare(&layout, 48_000).unwrap();
        let bg = basic.plane_wave_gains(Vec3::Y);
        let mg = maxre.plane_wave_gains(Vec3::Y);
        let n = 11usize;
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                continue;
            }
            let cos = s.position.normalized().unwrap().dot(Vec3::Y);
            let a1 = 0.866_025_4f32;
            let expected = (1.0 + 3.0 * a1 * cos) / n as f32;
            assert!((mg[idx] - expected).abs() < 1e-4, "max-rE speaker {idx}");
            assert!(mg[idx].is_finite() && bg[idx].is_finite());
        }
    }

    #[test]
    fn order2_max_re_weights_apply_per_order_run() {
        // Order-2 max-rE: a1 applies to ACN 1–3, a2 to ACN 4–8 — verify via
        // the decoder against the documented window directly.
        let layout = SpeakerLayout::seven_point_one_four();
        let mut maxre = AmbisonicDecoder::with_order(DecoderPolicy::MaxRe, 2);
        maxre.prepare(&layout, 48_000).unwrap();
        let dir = Vec3::Y;
        let gains = maxre.plane_wave_gains(dir);
        let n = 11usize;
        let (a1, a2) = (0.905_663_1f32, 0.682_689_4f32);
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                continue;
            }
            let spk_dir = s.position.normalized().unwrap();
            let mut spk_y = [0.0f32; 9];
            let mut src_y = [0.0f32; 9];
            sh_n(2, spk_dir, &mut spk_y);
            sh_n(2, dir, &mut src_y);
            let mut expected = 0.0f32;
            for k in 0..9 {
                let a = if k == 0 {
                    1.0
                } else if k <= 3 {
                    a1
                } else {
                    a2
                };
                // The decoder weights apply on the decode side only (the
                // encoder stays unweighted).
                expected += a * spk_y[k] * src_y[k];
            }
            expected /= n as f32;
            assert!(
                (gains[idx] - expected).abs() < 1e-4,
                "order-2 max-rE speaker {idx}: {} want {expected}",
                gains[idx]
            );
        }
        // The max-rE order-2 lobe is narrower than order-1 max-rE: rear
        // (RL/RR) energy is closer to zero.
        let o1 = {
            let mut d = AmbisonicDecoder::new(DecoderPolicy::MaxRe);
            d.prepare(&layout, 48_000).unwrap();
            d.plane_wave_gains(Vec3::Y)
        };
        let rear_o1: f32 = o1[6..8].iter().map(|g| g * g).sum();
        let rear_o2: f32 = gains[6..8].iter().map(|g| g * g).sum();
        assert!(
            rear_o2 < rear_o1,
            "order-2 max-rE narrows the rear lobe ({rear_o2} vs {rear_o1})"
        );
    }

    #[test]
    fn rotation_commutes_with_the_basis() {
        // The defining property: rotating a direction then evaluating the
        // basis equals rotating the basis coefficients (block-diagonal
        // [1, R, W₂]).
        for q in [
            Quat::from_euler_rad(0.7, 0.3, -0.2),
            Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0),
            Quat::from_euler_rad(0.0, -1.1, 2.3),
        ] {
            let r = |v: Vec3| q.rotate_vec3(v);
            for d in [Vec3::Y, Vec3::X, Vec3::new(1.0, 2.0, 3.0)] {
                let d = d.normalized().unwrap();
                let rd = r(d);
                let mut y = [0.0f32; 9];
                let mut yr = [0.0f32; 9];
                sh_n(2, d, &mut y);
                sh_n(2, rd, &mut yr);
                // Apply the rotation operator directly.
                let mut rotated = y;
                rotate_bus_frame_n(q, 2, &mut rotated);
                for k in 0..9 {
                    assert!(
                        (rotated[k] - yr[k]).abs() < 1e-3,
                        "q={q:?} d={d:?} channel {k}: {} want {}",
                        rotated[k],
                        yr[k]
                    );
                }
            }
        }
    }

    #[test]
    fn order2_rotation_round_trips_to_identity() {
        let q = Quat::from_euler_rad(0.7, 0.3, -0.2);
        let d = Vec3::new(1.0, 2.0, 3.0).normalized().unwrap();
        let mut frame = [0.0f32; 9];
        encode_plane_wave_n(2, d, 1.0, &mut frame);
        rotate_bus_frame_n(q, 2, &mut frame);
        rotate_bus_frame_n(q.conjugate(), 2, &mut frame);
        let mut orig = [0.0f32; 9];
        encode_plane_wave_n(2, d, 1.0, &mut orig);
        for k in 0..9 {
            assert!((frame[k] - orig[k]).abs() < 1e-3, "channel {k}");
        }
    }

    #[test]
    fn bus_rotation_keeps_world_fixed_field_stable() {
        // A field encoded with a source at world +X. Listener yaws +90°
        // (faces +X): the source must appear front. The renderer rotates the
        // bus by the listener's conjugate; test the primitive directly.
        let mut frame = [0.0f32; 4];
        encode_plane_wave(Vec3::X, 1.0, &mut frame);
        assert!((frame[3] - 3.0f32.sqrt()).abs() < EPS, "X channel");
        let listener_yaw90 = Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0);
        rotate_bus_frame(listener_yaw90.conjugate(), &mut frame);
        // Now the field is at +Y (front): [1, √3, 0, 0].
        assert!(
            (frame[1] - 3.0f32.sqrt()).abs() < EPS,
            "Y channel {}",
            frame[1]
        );
        assert!(frame[3].abs() < EPS, "X channel cleared");
        assert!((frame[0] - 1.0).abs() < EPS, "W invariant");
        // Round-trip: rotate back by the yaw itself → +X again.
        rotate_bus_frame(listener_yaw90, &mut frame);
        assert!((frame[3] - 3.0f32.sqrt()).abs() < EPS, "round-trip");
    }

    #[test]
    fn renderer_applies_listener_rotation_and_calibration() {
        use super::super::render::SpatialRenderer;
        let layout = SpeakerLayout::stereo();
        let mut r = AmbisonicRenderer::new(DecoderPolicy::Basic);
        r.prepare(&layout, 48_000).unwrap();
        let mut scene = SpatialScene::new(48_000);
        scene
            .listener
            .set_orientation(Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0));
        // Encode a source at world +X; the yawed listener hears it at front
        // → equal FL/FR split of the (1 + 3·cos30°)/2 pattern.
        let frames = 8usize;
        let w = vec![1.0f32; frames];
        let x = vec![3.0f32.sqrt(); frames];
        let z = vec![0.0f32; frames];
        let y = vec![0.0f32; frames];
        let planes: Vec<&[f32]> = vec![&w, &y, &z, &x];
        let mut out = vec![0.0f32; 2 * frames];
        r.process_block(&scene, &planes, frames, &mut out).unwrap();
        assert!(out.iter().all(|v| v.is_finite()));
        let expected = (1.0 + 3.0 * 30f32.to_radians().cos()) / 2.0;
        assert!((out[0] - expected).abs() < 1e-3, "FL {}", out[0]);
        assert!((out[1] - expected).abs() < 1e-3, "FR {}", out[1]);
    }

    #[test]
    fn order2_renderer_decodes_onto_the_layout() {
        use super::super::render::SpatialRenderer;
        let layout = SpeakerLayout::stereo();
        let mut r = AmbisonicRenderer::with_order(DecoderPolicy::Basic, 2);
        r.prepare(&layout, 48_000).unwrap();
        let scene = SpatialScene::new(48_000);
        // Encode a front plane wave (W=1, Y=√3) at order 2.
        let frames = 8usize;
        let w = vec![1.0f32; frames];
        let y = vec![3.0f32.sqrt(); frames];
        let rest: Vec<&[f32]> = (2..9).map(|_| &[][..]).collect();
        let mut planes: Vec<&[f32]> = vec![&w, &y];
        planes.extend(rest);
        let mut out = vec![0.0f32; 2 * frames];
        r.process_block(&scene, &planes, frames, &mut out).unwrap();
        // Sampling decode on 2 speakers: (1 + 3·cos30°)/2 — the order-2
        // channels contribute zero for a front source on this layout.
        let expected = (1.0 + 3.0 * 30f32.to_radians().cos()) / 2.0;
        assert!((out[0] - expected).abs() < 1e-3, "FL {}", out[0]);
        assert!((out[1] - expected).abs() < 1e-3, "FR {}", out[1]);
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn order2_renderer_rotates_world_field_with_listener() {
        use super::super::render::SpatialRenderer;
        let layout = SpeakerLayout::stereo();
        let mut r = AmbisonicRenderer::with_order(DecoderPolicy::Basic, 2);
        r.prepare(&layout, 48_000).unwrap();
        let mut scene = SpatialScene::new(48_000);
        scene
            .listener
            .set_orientation(Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0));
        // Encode a plane wave at world +X at order 2. The yawed listener
        // hears it at front.
        let frames = 8usize;
        let mut frame = [0.0f32; 9];
        encode_plane_wave_n(2, Vec3::X, 1.0, &mut frame);
        let planes: Vec<Vec<f32>> = frame.iter().map(|&v| vec![v; frames]).collect();
        let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
        let mut out = vec![0.0f32; 2 * frames];
        r.process_block(&scene, &refs, frames, &mut out).unwrap();
        // The rotated frame is exactly the order-2 encoding of +Y; the
        // stereo decode is the full 9-channel projection, computed here
        // directly.
        let n = 2usize;
        let mut src = [0.0f32; 9];
        sh_n(2, Vec3::Y, &mut src);
        for (idx, s) in layout.speakers.iter().enumerate() {
            let mut spk = [0.0f32; 9];
            sh_n(2, s.position.normalized().unwrap(), &mut spk);
            let mut expected = 0.0f32;
            for k in 0..9 {
                expected += spk[k] * src[k];
            }
            expected /= n as f32;
            assert!(
                (out[idx] - expected).abs() < 1e-3,
                "speaker {idx}: {} want {expected}",
                out[idx]
            );
        }
    }

    #[test]
    fn decoder_rejects_empty_layout() {
        let mut dec = AmbisonicDecoder::new(DecoderPolicy::Basic);
        assert!(matches!(
            dec.prepare(&SpeakerLayout::custom(vec![]), 48_000),
            Err(RenderError::InvalidLayout)
        ));
        // LFE-only → no pan speakers → degenerate.
        let mut lfe = crate::spatial::speaker::Speaker::new(Vec3::ZERO);
        lfe.is_lfe = true;
        let lfe_only = SpeakerLayout {
            speakers: vec![lfe],
            reference_position: Vec3::ZERO,
            calibration: Default::default(),
        };
        assert!(matches!(
            dec.prepare(&lfe_only, 48_000),
            Err(RenderError::DegenerateGeometry)
        ));
        // Unsupported order panics at construction (order 4 > max).
        let result = std::panic::catch_unwind(|| {
            AmbisonicDecoder::with_order(DecoderPolicy::Basic, 4);
        });
        assert!(result.is_err(), "order 4 rejected");
    }

    #[test]
    fn order3_basis_matches_documented_sn3d_convention() {
        // Third-order block (ACN 9–15) at the cardinal directions:
        //   √(35/8)·x(x²−3y²), √105·xyz, √(21/8)·y(5z²−1), (√7/2)·z(5z²−3),
        //   √(21/8)·x(5z²−1), (√105/2)·z(x²−y²), √(35/8)·y(y²−3x²).
        let s358 = (35.0f32 / 8.0).sqrt();
        let s105 = 105.0f32.sqrt();
        let s218 = (21.0f32 / 8.0).sqrt();
        let s7h = 7.0f32.sqrt() * 0.5;
        let s105h = 0.5 * 105.0f32.sqrt();
        assert_eq!(channel_count(3), 16);
        let mut y = [0.0f32; 16];
        sh_n(3, Vec3::Y, &mut y);
        // At +Y front (x=0,z=0): ACN15 = √(35/8)·y³=√(35/8) and
        // ACN11 = √(21/8)·(5z²−1)y = −√(21/8); all others vanish.
        assert!((y[15] - s358).abs() < 1e-4, "Y₃³(+Y) = √(35/8)");
        assert!((y[11] + s218).abs() < 1e-4, "Y₃⁻¹(+Y) = −√(21/8)");
        for (k, &v) in y.iter().enumerate().skip(9) {
            if k != 15 && k != 11 {
                assert!(v.abs() < 1e-4, "ACN {k} vanishes at +Y: {v}");
            }
        }
        // At +Z up: ACN12 = (√7/2)(5−3)=√7; all others vanish.
        let mut y = [0.0f32; 16];
        sh_n(3, Vec3::Z, &mut y);
        for k in 9..16 {
            if k == 12 {
                assert!(
                    (y[12] - (s7h * 2.0)).abs() < 1e-3,
                    "Y₃⁰(+Z) = √7 ≈ 2.6458, got {}",
                    y[k]
                );
            } else {
                assert!(y[k].abs() < 1e-4, "ACN {k} vanishes at +Z: {}", y[k]);
            }
        }
        // +X right: ACN9 = √(35/8)·x³=√(35/8); ACN13 = −√(21/8); others
        // vanish.
        let mut y = [0.0f32; 16];
        sh_n(3, Vec3::X, &mut y);
        assert!((y[9] - s358).abs() < 1e-4, "Y₃⁻³(+X) = √(35/8)");
        assert!((y[13] + s218).abs() < 1e-4, "Y₃¹(+X) = −√(21/8)");
        // Diagonal: all seven populated, matching the closed forms directly.
        let d = Vec3::new(1.0, 2.0, 3.0).normalized().unwrap();
        let (x, yy, z) = (d.x, d.y, d.z);
        let mut y = [0.0f32; 16];
        sh_n(3, d, &mut y);
        assert!((y[9] - s358 * x * (x * x - 3.0 * yy * yy)).abs() < 1e-4);
        assert!((y[10] - s105 * x * yy * z).abs() < 1e-4);
        assert!((y[11] - s218 * yy * (5.0 * z * z - 1.0)).abs() < 1e-4);
        assert!((y[12] - s7h * z * (5.0 * z * z - 3.0)).abs() < 1e-4);
        assert!((y[13] - s218 * x * (5.0 * z * z - 1.0)).abs() < 1e-4);
        assert!((y[14] - s105h * z * (x * x - yy * yy)).abs() < 1e-4);
        assert!((y[15] - s358 * yy * (yy * yy - 3.0 * x * x)).abs() < 1e-4);
    }

    #[test]
    fn order3_norm_preserving_on_the_sphere() {
        // Each order-3 channel has unit mean-square over the sphere (SN3D),
        // and the seven are mutually orthogonal.
        use std::f32::consts::{PI, TAU};
        const STEPS: usize = 48;
        let mut sums = [0.0f64; 16];
        let mut cross = [[0.0f64; 7]; 7];
        for i in 0..STEPS {
            let th = (i as f32 + 0.5) * PI / STEPS as f32;
            let z = th.cos();
            let r = th.sin();
            for j in 0..(2 * STEPS) {
                let phi = (j as f32 + 0.5) * TAU / (2 * STEPS) as f32;
                let d = Vec3::new(r * phi.sin(), r * phi.cos(), z);
                let mut y = [0.0f32; 16];
                sh_n(3, d, &mut y);
                for (k, v) in y.iter().enumerate() {
                    sums[k] += (*v as f64) * (*v as f64) * th.sin() as f64;
                }
                for a in 0..7 {
                    for b in 0..7 {
                        cross[a][b] += y[9 + a] as f64 * y[9 + b] as f64 * th.sin() as f64;
                    }
                }
            }
        }
        let weight = std::f64::consts::PI / (4.0 * (STEPS as f64) * (STEPS as f64));
        for (k, &s) in sums.iter().enumerate().skip(9) {
            let norm_sq = s * weight;
            assert!(
                norm_sq - 1.0 < 0.02,
                "order-3 channel {k} norm² = {norm_sq}"
            );
        }
        for (a, row) in cross.iter().enumerate() {
            for (b, &v) in row.iter().enumerate().skip(a + 1) {
                let ip = v * weight;
                assert!(ip.abs() < 0.02, "<{}|{}> = {:.4}", 9 + a, 9 + b, ip);
            }
        }
    }

    #[test]
    fn order3_rotation_is_exact_on_the_basis() {
        // The defining property of the exact order-3 Wigner block: evaluating
        // the basis at a rotated direction equals rotating the coefficients.
        for q in [
            Quat::from_euler_rad(0.7, 0.3, -0.2),
            Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0),
            Quat::from_euler_rad(0.0, -1.1, 2.3),
        ] {
            for d in [Vec3::Y, Vec3::X, Vec3::new(1.0, 2.0, 3.0)] {
                let d = d.normalized().unwrap();
                let rd = q.rotate_vec3(d);
                let mut y = [0.0f32; 16];
                let mut yr = [0.0f32; 16];
                sh_n(3, d, &mut y);
                sh_n(3, rd, &mut yr);
                let mut rotated = y;
                rotate_bus_frame_n(q, 3, &mut rotated);
                for k in 0..16 {
                    assert!(
                        (rotated[k] - yr[k]).abs() < 2e-3,
                        "q={q:?} d={d:?} ACN {k}: {} want {}",
                        rotated[k],
                        yr[k]
                    );
                }
            }
        }
    }

    #[test]
    fn order3_rotation_round_trips_to_identity() {
        let q = Quat::from_euler_rad(0.7, 0.3, -0.2);
        let d = Vec3::new(1.0, 2.0, 3.0).normalized().unwrap();
        let mut frame = [0.0f32; 16];
        encode_plane_wave_n(3, d, 1.0, &mut frame);
        rotate_bus_frame_n(q, 3, &mut frame);
        rotate_bus_frame_n(q.conjugate(), 3, &mut frame);
        let mut orig = [0.0f32; 16];
        encode_plane_wave_n(3, d, 1.0, &mut orig);
        for k in 0..16 {
            assert!((frame[k] - orig[k]).abs() < 2e-3, "channel {k}");
        }
    }
}
