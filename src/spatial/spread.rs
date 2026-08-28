//! Angular-region spread (spec §30).
//!
//! A point source and an extended source must not sound identical. Rather
//! than merely reducing localization, spread describes the source's angular
//! extent: `0 = point`, `small = focused`, `medium = broad`, `large ≈
//! diffuse`. This module implements the spec's recipe — *source direction →
//! angular region → sample/weight multiple directions → aggregate speaker
//! gains* — with a fixed, allocation-free sample pattern:
//!
//! - one solve on the exact direction (weight `1 - s`), plus
//! - **3 ring samples** at `s × 60°` around it, equally spaced (weight
//!   `s/3` each),
//! - aggregated by speaker (duplicates summed) and **energy-normalised** so
//!   the perceived level stays constant while the image widens (§29).
//!
//! The sample count is fixed (4 solves max, 12 speaker entries max), so the
//! render path stays bounded and deterministic. True diffuse *fields*
//! (rain, crowd) are the Phase 6 domain; this is the extended-source model
//! the spec puts at `small→large` spread.

use super::math::Vec3;

/// Half-angle (radians) of the spread cap at `spread = 1`: 60°.
pub const SPREAD_MAX_HALF_ANGLE_RAD: f32 = std::f32::consts::FRAC_PI_3;

/// Number of ring samples around the source direction.
pub const RING_SAMPLES: usize = 3;

/// Maximum number of `(speaker, gain)` entries a spread solve can emit:
/// 4 solves (1 base + 3 ring) × up to 3 speakers each, plus headroom for
/// the nearest-speaker fallback entry.
pub const MAX_SPREAD_GAINS: usize = 12;

/// Generate the `RING_SAMPLES` ring directions around `dir` at cap
/// `half_angle_rad`, equally spaced in the perpendicular plane. Returns the
/// number of directions written (0 when `half_angle_rad` is negligible —
/// spread ≈ 0). Each returned direction is unit length.
pub fn ring_directions(dir: Vec3, half_angle_rad: f32, out: &mut [Vec3; RING_SAMPLES]) -> usize {
    if half_angle_rad <= 1e-4 || !half_angle_rad.is_finite() {
        return 0;
    }
    let (u, v) = perpendicular_frame(dir);
    let c = half_angle_rad.cos();
    let s = half_angle_rad.sin();
    for (k, o) in out.iter_mut().enumerate().take(RING_SAMPLES) {
        let phi = k as f32 * std::f32::consts::TAU / RING_SAMPLES as f32;
        *o = dir * c + (u * phi.cos() + v * phi.sin()) * s;
    }
    RING_SAMPLES
}

/// Deterministic orthonormal basis `(u, v)` perpendicular to `dir`, built
/// by crossing with the world axis least aligned with `dir`.
fn perpendicular_frame(dir: Vec3) -> (Vec3, Vec3) {
    let ax = if dir.x.abs() <= dir.y.abs() && dir.x.abs() <= dir.z.abs() {
        Vec3::X
    } else if dir.y.abs() <= dir.z.abs() {
        Vec3::Y
    } else {
        Vec3::Z
    };
    let u = ax.cross(dir).normalized().unwrap_or(Vec3::X);
    let v = dir.cross(u).normalized().unwrap_or(Vec3::Y);
    (u, v)
}

/// Compact `(speaker, gain)` accumulator: add `gain` to the entry for `spk`
/// (summing duplicates) or append a new entry. `len` is the current number
/// of live entries in `gains[..]`; returns the new length (bounded by
/// `gains.len()`).
#[inline]
pub fn add_gain(gains: &mut [(usize, f32)], len: usize, spk: usize, gain: f32) -> usize {
    if gain == 0.0 {
        return len;
    }
    for g in gains[..len].iter_mut() {
        if g.0 == spk {
            g.1 += gain;
            return len;
        }
    }
    if len < gains.len() {
        gains[len] = (spk, gain);
        len + 1
    } else {
        len
    }
}

/// Energy-normalise a compacted `(speaker, gain)` list to unit energy
/// (constant power across movement, spec §29). Returns the pre-normalisation
/// energy. Leaves the list untouched when the energy is negligible.
pub fn normalize_gains(gains: &mut [(usize, f32)]) -> f32 {
    let mut e = 0.0f32;
    for g in gains.iter() {
        e += g.1 * g.1;
    }
    if e > 1e-12 {
        let inv = 1.0 / e.sqrt();
        for g in gains.iter_mut() {
            g.1 *= inv;
        }
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-4;

    #[test]
    fn ring_directions_are_unit_and_around_dir() {
        let mut out = [Vec3::ZERO; RING_SAMPLES];
        let dir = Vec3::new(0.0, 1.0, 0.0);
        let n = ring_directions(dir, std::f32::consts::FRAC_PI_3, &mut out);
        assert_eq!(n, RING_SAMPLES);
        for d in out.iter() {
            assert!((d.length() - 1.0).abs() < EPS, "unit ring sample");
            // Same angular distance from the axis for every sample.
            let dot = d.dot(dir);
            assert!(
                (dot - std::f32::consts::FRAC_PI_3.cos()).abs() < 1e-4,
                "ring at cap angle: {dot}"
            );
        }
        // Distinct directions.
        assert!((out[0] - out[1]).length() > 0.1);
        assert!((out[0] - out[2]).length() > 0.1);
    }

    #[test]
    fn ring_directions_zero_for_no_spread() {
        let mut out = [Vec3::ZERO; RING_SAMPLES];
        assert_eq!(ring_directions(Vec3::Y, 0.0, &mut out), 0);
        assert_eq!(ring_directions(Vec3::Y, -1.0, &mut out), 0);
    }

    #[test]
    fn ring_samples_stay_at_cap_angle_for_odd_directions() {
        // Ring samples sit at the cap half-angle around the axis (dot = cos),
        // for every axis orientation — including diagonal ones.
        for dir in [
            Vec3::Y,
            Vec3::X,
            Vec3::Z,
            Vec3::new(1.0, 2.0, 3.0).normalized().unwrap(),
        ] {
            let mut out = [Vec3::ZERO; RING_SAMPLES];
            let n = ring_directions(dir, 0.5, &mut out);
            assert_eq!(n, RING_SAMPLES);
            for d in out.iter() {
                assert!(
                    (d.dot(dir) - 0.5f32.cos()).abs() < 1e-4,
                    "at cap angle for {dir:?}"
                );
                assert!((d.length() - 1.0).abs() < 1e-4, "unit");
            }
        }
    }

    #[test]
    fn add_gain_sums_duplicates_and_bounds() {
        let mut g = [(0usize, 0.0f32); 4];
        let mut len = add_gain(&mut g, 0, 3, 0.5);
        len = add_gain(&mut g, len, 1, 0.25);
        len = add_gain(&mut g, len, 3, 0.5); // duplicate → summed
        assert_eq!(len, 2);
        assert_eq!(g[0], (3, 1.0));
        assert_eq!(g[1], (1, 0.25));
        // Zero gain is ignored.
        len = add_gain(&mut g, len, 7, 0.0);
        assert_eq!(len, 2);
        // Bounded: extra entries are dropped.
        len = add_gain(&mut g, len, 5, 0.1);
        len = add_gain(&mut g, len, 6, 0.1);
        assert_eq!(len, 4);
        assert_eq!(add_gain(&mut g, len, 9, 0.1), 4);
    }

    #[test]
    fn normalize_gains_yields_unit_energy() {
        let mut g = [(0usize, 0.0f32); 3];
        g[0] = (0, 0.5);
        g[1] = (1, 0.5);
        g[2] = (2, 0.5);
        let e = normalize_gains(&mut g);
        assert!((e - 0.75).abs() < EPS, "energy before: {e}");
        let after: f32 = g.iter().map(|(_, v)| v * v).sum();
        assert!((after - 1.0).abs() < EPS, "normalized: {after}");
        // Negligible energy → untouched.
        let mut z = [(0usize, 1e-30f32); 1];
        z[0] = (0, 1e-30);
        assert_eq!(normalize_gains(&mut z), 1e-60);
        assert_eq!(z[0].1, 1e-30);
    }
}
