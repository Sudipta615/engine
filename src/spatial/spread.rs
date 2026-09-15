//! Angular-region spread (spec §30).
//!
//! A point source and an extended source must not sound identical. Rather
//! than merely reducing localization, spread describes the source's angular
//! extent: `0 = point`, `small = focused`, `medium = broad`, `large ≈
//! diffuse`. This module implements the spec's recipe — *source direction →
//! angular region → sample/weight multiple directions → aggregate speaker
//! gains* — with a layout-proportional constant-spread algorithm:
//!
//! - one solve on the exact direction (weight `1 - s`), plus
//! - **N ring samples** at `constant_spread_half_angle(s, layout_span)` around
//!   it, equally spaced (weight `s/N` each), where N grows with spread mode,
//! - aggregated by speaker (duplicates summed) and **energy-normalised** so
//!   the perceived level stays constant while the image widens (§29).
//!
//! The half-angle is proportional to the speaker subtended angle (derived from
//! the layout span), so a given `spread` value represents the same perceived
//! image width regardless of speaker array geometry. For backward compatibility
//! the legacy `ring_directions` with a fixed `RING_SAMPLES` constant is kept.
//!
//! The sample count is fixed per mode (max 12 solves), so the render path
//! stays bounded and deterministic. True diffuse *fields* (rain, crowd) are
//! the domain of the field module; this is the extended-source model the spec
//! puts at `small→large` spread.

use super::math::Vec3;

/// Half-angle (radians) of the spread cap at `spread = 1` for the legacy
/// 3-sample ring path: 60°.
pub const SPREAD_MAX_HALF_ANGLE_RAD: f32 = std::f32::consts::FRAC_PI_3;

/// Number of ring samples for the legacy (3-sample) spread path.
pub const RING_SAMPLES: usize = 3;

/// Maximum number of `(speaker, gain)` entries a spread solve can emit:
/// 13 solves (1 base + 12 ring) × up to 3 speakers each, plus headroom.
pub const MAX_SPREAD_GAINS: usize = 40;

/// Perceptual spread mode — the source's angular extent as seen by the
/// listener. Each mode maps to a ring-sample count and a fraction of the
/// speaker layout span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpreadMode {
    /// No spread — exact point source.
    #[default]
    Point,
    /// Small image — slightly widened point (3 ring samples).
    Small,
    /// Medium image (5 ring samples).
    Medium,
    /// Wide image (8 ring samples).
    Wide,
    /// Diffuse image — maximally spread (12 ring samples).
    Diffuse,
}

/// Ring sample counts for each [`SpreadMode`]: `[Point, Small, Medium, Wide, Diffuse]`.
pub const SPREAD_RING_COUNTS: [usize; 5] = [0, 3, 5, 8, 12];

impl SpreadMode {
    /// Map a linear spread value `s ∈ [0.0, 1.0]` to the nearest `SpreadMode`.
    pub fn from_linear(spread: f32) -> SpreadMode {
        let s = spread.clamp(0.0, 1.0);
        if s < 0.10 {
            SpreadMode::Point
        } else if s < 0.30 {
            SpreadMode::Small
        } else if s < 0.55 {
            SpreadMode::Medium
        } else if s < 0.80 {
            SpreadMode::Wide
        } else {
            SpreadMode::Diffuse
        }
    }

    /// Number of ring samples for this mode.
    pub fn ring_count(self) -> usize {
        SPREAD_RING_COUNTS[self as usize]
    }

    /// Layout-fraction for this mode (`[0, 0.25, 0.45, 0.70, 1.0]` of the
    /// speaker layout span).
    pub fn layout_fraction(self) -> f32 {
        match self {
            SpreadMode::Point => 0.0,
            SpreadMode::Small => 0.25,
            SpreadMode::Medium => 0.45,
            SpreadMode::Wide => 0.70,
            SpreadMode::Diffuse => 1.0,
        }
    }
}

/// Compute the spread cap half-angle (radians) for `spread ∈ [0,1]` given the
/// speaker layout's angular span (radians). The half-angle scales with the
/// layout span so the same spread value represents the same perceived width
/// regardless of array geometry. Clamped to `[0, π/2]`.
pub fn constant_spread_half_angle(spread: f32, speaker_layout_span_rad: f32) -> f32 {
    let s = spread.clamp(0.0, 1.0);
    let span = speaker_layout_span_rad
        .abs()
        .clamp(0.0, std::f32::consts::PI);
    (s * span * 0.5).min(std::f32::consts::FRAC_PI_2)
}

/// Generate `n` ring directions around `dir` at cap `half_angle_rad`, equally
/// spaced in the perpendicular plane. Returns the number of directions written
/// (0 when `half_angle_rad` is negligible or `n == 0`). Each direction is unit
/// length. `out` must hold at least `n` entries.
pub fn ring_directions_n(dir: Vec3, half_angle_rad: f32, out: &mut [Vec3], n: usize) -> usize {
    if n == 0 || half_angle_rad <= 1e-4 || !half_angle_rad.is_finite() {
        return 0;
    }
    let n = n.min(out.len());
    let (u, v) = perpendicular_frame(dir);
    let c = half_angle_rad.cos();
    let s = half_angle_rad.sin();
    for (k, o) in out.iter_mut().enumerate().take(n) {
        let phi = k as f32 * std::f32::consts::TAU / n as f32;
        *o = dir * c + (u * phi.cos() + v * phi.sin()) * s;
    }
    n
}

/// Compute constant-power speaker gains for a spread source at `dir`.
///
/// - `spread ∈ [0,1]`: 0 = point, 1 = diffuse.
/// - `speaker_layout_span_rad`: full angular span of the speaker array.
/// - `gains`: output buffer (must hold ≥ [`MAX_SPREAD_GAINS`] entries).
/// - `speakers`: `(output_index, unit_direction)` for each pan-capable speaker.
///
/// Returns the number of `(speaker_index, gain)` pairs written into `gains`.
/// Gains are energy-normalized (constant power).
/// Allocation-free.
pub fn constant_power_spread_gains(
    dir: Vec3,
    spread: f32,
    speaker_layout_span_rad: f32,
    gains: &mut [(usize, f32)],
    speakers: &[(usize, Vec3)],
) -> usize {
    debug_assert!(gains.len() >= MAX_SPREAD_GAINS);
    let mode = SpreadMode::from_linear(spread);
    let n_ring = mode.ring_count();
    let half_angle = constant_spread_half_angle(spread, speaker_layout_span_rad);
    let s = spread.clamp(0.0, 1.0);

    // Nearest-speaker lookup (argmax dot product), allocation-free.
    let nearest = |d: Vec3| -> usize {
        let mut best = 0usize;
        let mut best_dot = f32::NEG_INFINITY;
        for &(idx, spk_dir) in speakers.iter() {
            let dot = d.dot(spk_dir);
            if dot > best_dot {
                best_dot = dot;
                best = idx;
            }
        }
        best
    };

    let base_weight = (1.0 - s).max(0.0);
    let ring_weight = if n_ring > 0 { s / n_ring as f32 } else { 0.0 };
    let mut n_gains = 0usize;

    // Base direction.
    n_gains = add_gain(gains, n_gains, nearest(dir), base_weight);

    // Ring samples.
    if n_ring > 0 {
        let mut ring = [Vec3::ZERO; 12];
        let written = ring_directions_n(dir, half_angle, &mut ring, n_ring);
        for &rd in ring[..written].iter() {
            n_gains = add_gain(gains, n_gains, nearest(rd), ring_weight);
        }
    }

    normalize_gains(&mut gains[..n_gains]);
    n_gains
}

/// Generate the `RING_SAMPLES` ring directions around `dir` at cap
/// `half_angle_rad`, equally spaced in the perpendicular plane. Returns the
/// number of directions written (0 when `half_angle_rad` is negligible —
/// spread ≈ 0). Each returned direction is unit length.
///
/// This is the legacy 3-sample API; prefer [`ring_directions_n`] for
/// mode-aware spread.
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

    #[test]
    fn spread_mode_from_linear_covers_all_modes() {
        assert_eq!(SpreadMode::from_linear(0.0), SpreadMode::Point);
        assert_eq!(SpreadMode::from_linear(0.05), SpreadMode::Point);
        assert_eq!(SpreadMode::from_linear(0.15), SpreadMode::Small);
        assert_eq!(SpreadMode::from_linear(0.40), SpreadMode::Medium);
        assert_eq!(SpreadMode::from_linear(0.65), SpreadMode::Wide);
        assert_eq!(SpreadMode::from_linear(0.90), SpreadMode::Diffuse);
        assert_eq!(SpreadMode::from_linear(1.0), SpreadMode::Diffuse);
    }

    #[test]
    fn spread_ring_counts_match_modes() {
        assert_eq!(SpreadMode::Point.ring_count(), 0);
        assert_eq!(SpreadMode::Small.ring_count(), 3);
        assert_eq!(SpreadMode::Medium.ring_count(), 5);
        assert_eq!(SpreadMode::Wide.ring_count(), 8);
        assert_eq!(SpreadMode::Diffuse.ring_count(), 12);
    }

    #[test]
    fn constant_spread_half_angle_scales_with_layout_span() {
        let span = std::f32::consts::PI;
        // At spread 0 → angle 0.
        assert_eq!(constant_spread_half_angle(0.0, span), 0.0);
        // At spread 1 → layout_span/2 = π/2.
        let h = constant_spread_half_angle(1.0, span);
        assert!((h - std::f32::consts::FRAC_PI_2).abs() < EPS);
        // Monotonic.
        let h05 = constant_spread_half_angle(0.5, span);
        let h10 = constant_spread_half_angle(1.0, span);
        assert!(h05 < h10 + EPS, "monotonic: {h05} < {h10}");
    }

    #[test]
    fn ring_directions_n_produces_distinct_unit_vectors() {
        for n in [3, 5, 8, 12] {
            let mut out = [Vec3::ZERO; 12];
            let written = ring_directions_n(Vec3::Y, 0.4, &mut out, n);
            assert_eq!(written, n, "written n={n}");
            for d in out[..n].iter() {
                assert!((d.length() - 1.0).abs() < EPS, "unit for n={n}");
                assert!(
                    (d.dot(Vec3::Y) - 0.4f32.cos()).abs() < EPS,
                    "cap angle n={n}"
                );
            }
            // All distinct.
            for i in 0..n {
                for j in i + 1..n {
                    assert!(
                        (out[i] - out[j]).length() > 0.05,
                        "distinct n={n} i={i} j={j}"
                    );
                }
            }
        }
    }

    #[test]
    fn constant_power_spread_gains_unit_energy() {
        // Simple 3-speaker layout at ±60° and front.
        let speakers = [
            (0usize, Vec3::Y),
            (1usize, Vec3::new(-0.866, 0.5, 0.0).normalized().unwrap()),
            (2usize, Vec3::new(0.866, 0.5, 0.0).normalized().unwrap()),
        ];
        let layout_span = std::f32::consts::PI * 2.0 / 3.0;
        let mut gains = [(0usize, 0.0f32); MAX_SPREAD_GAINS];

        for spread in [0.0f32, 0.3, 0.6, 1.0] {
            let n =
                constant_power_spread_gains(Vec3::Y, spread, layout_span, &mut gains, &speakers);
            if n > 0 {
                let energy: f32 = gains[..n].iter().map(|(_, g)| g * g).sum();
                assert!(
                    (energy - 1.0).abs() < 1e-4,
                    "unit energy at spread={spread}: {energy}"
                );
            }
        }
    }

    #[test]
    fn constant_power_spread_gains_point_returns_one_speaker() {
        let speakers = [(0usize, Vec3::Y), (1usize, Vec3::X), (2usize, Vec3::Z)];
        let mut gains = [(0usize, 0.0f32); MAX_SPREAD_GAINS];
        // Point spread (0.0) → base weight 1.0, no ring → one unique speaker.
        let n =
            constant_power_spread_gains(Vec3::Y, 0.0, std::f32::consts::PI, &mut gains, &speakers);
        assert_eq!(n, 1, "point spread → 1 speaker entry");
        assert_eq!(gains[0].0, 0, "nearest to front is speaker 0");
    }
}
