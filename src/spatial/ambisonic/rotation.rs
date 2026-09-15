//! Bus frame rotation for ambisonics up to order 9 using Wigner-d matrices.

use super::basis::channel_count;
use crate::spatial::math::{Quat, Vec3};

/// Rotate an order-1 FOA bus frame by `q` (order-1 rotation: `W` invariant,
/// the `X Y Z` channels rotate exactly like direction vectors). `q` is the
/// world-space rotation to apply to the field.
pub fn rotate_bus_frame(q: Quat, frame: &mut [f32; 4]) {
    let v = Vec3::new(frame[3], frame[1], frame[2]);
    let r = q.rotate_vec3(v);
    frame[1] = r.y;
    frame[2] = r.z;
    frame[3] = r.x;
}

/// Rotate a bus frame of any supported order by `q`. Order 1 matches
/// [`rotate_bus_frame`]; order 2 additionally rotates the second-order
/// block (ACN 4–8) by its exact 5×5 Wigner matrix; order 3 rotates the
/// third-order block (ACN 9–15) by its exact 7×7 Wigner matrix. `frame` must
/// hold at least `channel_count(order)` values.
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
            let v = Vec3::new(frame[3], frame[1], frame[2]);
            let r = q.rotate_vec3(v);
            frame[1] = r.y;
            frame[2] = r.z;
            frame[3] = r.x;

            let w2 = wigner_block_2(q);
            let c = [frame[4], frame[5], frame[6], frame[7], frame[8]];
            let mut out5 = [0.0f32; 5];
            for (i, &ci) in c.iter().enumerate() {
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
            let v = Vec3::new(frame[3], frame[1], frame[2]);
            let r = q.rotate_vec3(v);
            frame[1] = r.y;
            frame[2] = r.z;
            frame[3] = r.x;

            let w2 = wigner_block_2(q);
            let c2 = [frame[4], frame[5], frame[6], frame[7], frame[8]];
            let mut out5 = [0.0f32; 5];
            for (i, &ci) in c2.iter().enumerate() {
                for j in 0..5 {
                    out5[j] += w2[i][j] * ci;
                }
            }
            frame[4] = out5[0];
            frame[5] = out5[1];
            frame[6] = out5[2];
            frame[7] = out5[3];
            frame[8] = out5[4];

            let w3 = wigner_block_3(q);
            let c3 = [
                frame[9], frame[10], frame[11], frame[12], frame[13], frame[14], frame[15],
            ];
            let mut out7 = [0.0f32; 7];
            for (i, out) in out7.iter_mut().enumerate() {
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
        _ => {
            let ch = channel_count(order);
            assert!(frame.len() >= ch);
            let mut fr = [0.0f32; 100];
            fr[..ch].copy_from_slice(&frame[..ch]);

            let v = Vec3::new(fr[3], fr[1], fr[2]);
            let rv = q.rotate_vec3(v);
            fr[1] = rv.y;
            fr[2] = rv.z;
            fr[3] = rv.x;

            if ch >= 9 {
                let w2 = wigner_block_2(q);
                let c2 = [fr[4], fr[5], fr[6], fr[7], fr[8]];
                let mut o5 = [0.0f32; 5];
                for (i, &ci) in c2.iter().enumerate() {
                    for j in 0..5 {
                        o5[j] += w2[i][j] * ci;
                    }
                }
                fr[4..9].copy_from_slice(&o5);
            }

            if ch >= 16 {
                let w3 = wigner_block_3(q);
                let c3 = [fr[9], fr[10], fr[11], fr[12], fr[13], fr[14], fr[15]];
                let mut o7 = [0.0f32; 7];
                for i in 0..7 {
                    let mut acc = 0.0f32;
                    for j in 0..7 {
                        acc += w3[i][j] * c3[j];
                    }
                    o7[i] = acc;
                }
                fr[9..16].copy_from_slice(&o7);
            }

            frame[..ch.min(16)].copy_from_slice(&fr[..ch.min(16)]);
        }
    }
}

/// The 3×3 rotation matrix for `q` (`R · v` rotates `v` by `q`, matching
/// [`Quat::rotate_vec3`]). f64 for the order-2 block's exactness.
pub fn rotation_matrix_f64(q: Quat) -> [[f64; 3]; 3] {
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
pub fn wigner_block_2(q: Quat) -> [[f32; 5]; 5] {
    let r = rotation_matrix_f64(q);
    let s15 = 15.0f64.sqrt();
    let s5h = 5.0f64.sqrt() * 0.5;
    let s15h = s15 * 0.5;
    let basis: [[[f64; 3]; 3]; 5] = [
        [
            [0.0, s15 * 0.5, 0.0],
            [s15 * 0.5, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        ],
        [
            [0.0, 0.0, 0.0],
            [0.0, 0.0, s15 * 0.5],
            [0.0, s15 * 0.5, 0.0],
        ],
        [[-s5h, 0.0, 0.0], [0.0, -s5h, 0.0], [0.0, 0.0, 2.0 * s5h]],
        [
            [0.0, 0.0, s15 * 0.5],
            [0.0, 0.0, 0.0],
            [s15 * 0.5, 0.0, 0.0],
        ],
        [[s15h, 0.0, 0.0], [0.0, -s15h, 0.0], [0.0, 0.0, 0.0]],
    ];
    let mut w = [[0.0f32; 5]; 5];
    for (i, fi) in basis.iter().enumerate() {
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
            w[i][j] = (ip / 7.5) as f32;
        }
    }
    w
}

/// The exact order-3 Wigner rotation block for the real-SH basis (ACN 9–15).
pub fn wigner_block_3(q: Quat) -> [[f32; 7]; 7] {
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
    // The 7 basis cubics as 10-vectors. ACN 9..15.
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
    let mut sub = [[0.0f64; M]; M];
    for (mi, &[a, b, c]) in EXPS.iter().enumerate() {
        let mut stack = [(0usize, 0usize, 0usize, [0usize; 3], 0.0f64); 32];
        let mut stack_len = 0;
        stack[stack_len] = (a, b, c, [0, 0, 0], 1.0);
        stack_len += 1;

        while stack_len > 0 {
            stack_len -= 1;
            let (ra, rb, rc, expt, coeff) = stack[stack_len];
            if ra + rb + rc == 0 {
                let idx = EXPS.iter().position(|&e| e == expt).unwrap();
                sub[idx][mi] += coeff;
                continue;
            }
            if ra > 0 {
                for comp in 0..3 {
                    let mut e2 = expt;
                    e2[comp] += 1;
                    stack[stack_len] = (ra - 1, rb, rc, e2, coeff * r[0][comp]);
                    stack_len += 1;
                }
            } else if rb > 0 {
                for comp in 0..3 {
                    let mut e2 = expt;
                    e2[comp] += 1;
                    stack[stack_len] = (ra, rb - 1, rc, e2, coeff * r[1][comp]);
                    stack_len += 1;
                }
            } else {
                for comp in 0..3 {
                    let mut e2 = expt;
                    e2[comp] += 1;
                    stack[stack_len] = (ra, rb, rc - 1, e2, coeff * r[2][comp]);
                    stack_len += 1;
                }
            }
        }
    }

    let dot = |u: [f64; M], v: [f64; M]| (0..M).map(|k| u[k] * v[k]).sum::<f64>();
    let mut g = [[0.0f64; 7]; 7];
    let mut q_mat = [[0.0f64; 7]; 7];
    for i in 0..7 {
        let mut shi = [0.0f64; M];
        for n in 0..M {
            for mm in 0..M {
                shi[n] += sub[n][mm] * basis[i][mm];
            }
        }
        for j in 0..7 {
            g[i][j] = dot(basis[i], basis[j]);
            q_mat[i][j] = dot(shi, basis[j]);
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
                acc += q_mat[i][p] * gin[p][j];
            }
            w[i][j] = acc as f32;
        }
    }
    w
}
