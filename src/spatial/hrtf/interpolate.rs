//! Spherical triangulation and barycentric interpolation on the unit sphere (spec §47–48, §62).

use crate::spatial::math::Vec3;

/// A spherical triangle defined by three vertex indices on the unit sphere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SphericalTriangle {
    pub a: usize,
    pub b: usize,
    pub c: usize,
}

impl SphericalTriangle {
    pub const fn new(a: usize, b: usize, c: usize) -> Self {
        Self { a, b, c }
    }
}

/// Compute spherical barycentric coordinates `[w0, w1, w2]` for `query` with
/// respect to the spherical triangle `(v0, v1, v2)`.
///
/// Returns `Some([w0, w1, w2])` if `query` projects within or on the spherical triangle
/// (with sum of weights normalized to 1.0), or `None` if `query` is outside.
pub fn barycentric_sphere(query: Vec3, v0: Vec3, v1: Vec3, v2: Vec3) -> Option<[f32; 3]> {
    // Solve [v0 v1 v2] * [c0, c1, c2]^T = query
    // Using Cramer's rule: det = v0 . (v1 x v2)
    let det = v0.dot(v1.cross(v2));
    if det.abs() < 1e-6 {
        return None;
    }

    let c0 = query.dot(v1.cross(v2)) / det;
    let c1 = v0.dot(query.cross(v2)) / det;
    let c2 = v0.dot(v1.cross(query)) / det;

    const EPS: f32 = -1e-4;
    if c0 >= EPS && c1 >= EPS && c2 >= EPS {
        let sum = (c0 + c1 + c2).max(1e-12);
        let w0 = (c0 / sum).max(0.0);
        let w1 = (c1 / sum).max(0.0);
        let w2 = (c2 / sum).max(0.0);
        let norm = (w0 + w1 + w2).max(1e-12);
        Some([w0 / norm, w1 / norm, w2 / norm])
    } else {
        None
    }
}

/// Compute 3D convex hull / spherical Delaunay triangulation for points on the unit sphere.
/// For points on S^2, the convex hull facets form the exact spherical Delaunay triangulation.
pub fn triangulate_sphere(points: &[Vec3]) -> Vec<SphericalTriangle> {
    let n = points.len();
    if n < 4 {
        return Vec::new();
    }

    let mut triangles = Vec::new();

    // Direct Gift-Wrapping / Facet search for spherical points:
    // A facet (i, j, k) belongs to the convex hull of points on S^2 iff
    // the normal n = (p_j - p_i) x (p_k - p_i) points outwards and all points lie
    // on or behind the plane.
    for i in 0..n {
        for j in (i + 1)..n {
            for k in (j + 1)..n {
                let pi = points[i];
                let pj = points[j];
                let pk = points[k];

                let norm = (pj - pi).cross(pk - pi);
                let norm_len = norm.length();
                if norm_len < 1e-6 {
                    continue;
                }

                // Orient normal so that origin is strictly inside (plane d > 0)
                let d = norm.dot(pi);
                let (norm, d, a, b, c) = if d < 0.0 {
                    (-norm, -d, i, k, j)
                } else {
                    (norm, d, i, j, k)
                };

                let mut valid = true;
                for (m, &pm) in points.iter().enumerate() {
                    if m == i || m == j || m == k {
                        continue;
                    }
                    if norm.dot(pm) > d + 1e-4 {
                        valid = false;
                        break;
                    }
                }

                if valid {
                    triangles.push(SphericalTriangle::new(a, b, c));
                }
            }
        }
    }

    triangles
}

/// Spherical HRTF interpolator for irregular direction sets.
#[derive(Debug, Clone)]
pub struct SphericalHrtfInterpolator {
    points: Vec<Vec3>,
    triangles: Vec<SphericalTriangle>,
}

impl SphericalHrtfInterpolator {
    /// Build an interpolator from a set of unit measurement directions.
    pub fn new(directions: &[Vec3]) -> Self {
        let points = directions
            .iter()
            .map(|&d| d.normalized().unwrap_or(Vec3::Y))
            .collect::<Vec<_>>();
        let triangles = triangulate_sphere(&points);
        Self { points, triangles }
    }

    /// Number of measurement directions.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// True if there are no measurement directions.
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Find enclosing triangle and barycentric weights for a query direction.
    /// Falls back to 3 nearest neighbors if no spherical triangle contains the query directly.
    pub fn find_weights(&self, query: Vec3) -> (SphericalTriangle, [f32; 3]) {
        let q = query.normalized().unwrap_or(Vec3::Y);

        // First attempt: check spherical triangles
        for &tri in &self.triangles {
            let v0 = self.points[tri.a];
            let v1 = self.points[tri.b];
            let v2 = self.points[tri.c];
            if let Some(w) = barycentric_sphere(q, v0, v1, v2) {
                return (tri, w);
            }
        }

        // Fallback: 3 nearest neighbors on the sphere by cosine distance (dot product)
        let mut dists = self
            .points
            .iter()
            .enumerate()
            .map(|(idx, &p)| (idx, 1.0 - q.dot(p)))
            .collect::<Vec<_>>();
        dists.sort_by(|a, b| a.1.total_cmp(&b.1));

        let i0 = dists[0].0;
        let i1 = dists.get(1).map(|x| x.0).unwrap_or(i0);
        let i2 = dists.get(2).map(|x| x.0).unwrap_or(i1);

        let d0 = dists[0].1.max(1e-6);
        let d1 = dists.get(1).map(|x| x.1.max(1e-6)).unwrap_or(d0);
        let d2 = dists.get(2).map(|x| x.1.max(1e-6)).unwrap_or(d1);

        let inv0 = 1.0 / d0;
        let inv1 = 1.0 / d1;
        let inv2 = 1.0 / d2;
        let sum = inv0 + inv1 + inv2;

        (
            SphericalTriangle::new(i0, i1, i2),
            [inv0 / sum, inv1 / sum, inv2 / sum],
        )
    }

    /// Interpolate HRTF impulse responses for `query` into `out`.
    pub fn interpolate(
        &self,
        query: Vec3,
        irs: &[f32],
        taps: usize,
        stride: usize,
        ear_offset: usize,
        out: &mut [f32],
    ) {
        debug_assert!(out.len() >= taps);
        let (tri, w) = self.find_weights(query);

        let base_a = tri.a * stride + ear_offset;
        let base_b = tri.b * stride + ear_offset;
        let base_c = tri.c * stride + ear_offset;

        let ir_a = &irs[base_a..base_a + taps];
        let ir_b = &irs[base_b..base_b + taps];
        let ir_c = &irs[base_c..base_c + taps];

        for k in 0..taps {
            out[k] = w[0] * ir_a[k] + w[1] * ir_b[k] + w[2] * ir_c[k];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barycentric_sphere_exact_at_vertices() {
        let v0 = Vec3::X;
        let v1 = Vec3::Y;
        let v2 = Vec3::Z;

        let w0 = barycentric_sphere(v0, v0, v1, v2).expect("v0 inside");
        assert!((w0[0] - 1.0).abs() < 1e-4);
        assert!(w0[1].abs() < 1e-4);
        assert!(w0[2].abs() < 1e-4);

        let w1 = barycentric_sphere(v1, v0, v1, v2).expect("v1 inside");
        assert!(w1[0].abs() < 1e-4);
        assert!((w1[1] - 1.0).abs() < 1e-4);
        assert!(w1[2].abs() < 1e-4);
    }

    #[test]
    fn barycentric_sphere_centroid() {
        let v0 = Vec3::X;
        let v1 = Vec3::Y;
        let v2 = Vec3::Z;
        let centroid = (v0 + v1 + v2).normalized().unwrap();

        let w = barycentric_sphere(centroid, v0, v1, v2).expect("centroid inside");
        assert!((w[0] - 1.0 / 3.0).abs() < 1e-4);
        assert!((w[1] - 1.0 / 3.0).abs() < 1e-4);
        assert!((w[2] - 1.0 / 3.0).abs() < 1e-4);
    }

    #[test]
    fn octahedron_triangulation() {
        let pts = vec![Vec3::X, -Vec3::X, Vec3::Y, -Vec3::Y, Vec3::Z, -Vec3::Z];
        let interp = SphericalHrtfInterpolator::new(&pts);
        assert_eq!(interp.len(), 6);
        assert_eq!(interp.triangles.len(), 8); // 8 octants

        let query = Vec3::new(1.0, 1.0, 1.0).normalized().unwrap();
        let (_tri, weights) = interp.find_weights(query);
        assert!((weights.iter().sum::<f32>() - 1.0).abs() < 1e-4);
    }
}
