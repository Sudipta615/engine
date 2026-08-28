//! Source directivity (spec §41).
//!
//! A directional source outputs differently depending on the angle at which
//! the listener hears it, relative to the source's facing direction (+Y in
//! source-local space). The render pipeline is:
//!
//! ```text
//! source orientation → relative listener direction → angle → curve → gain
//! ```
//!
//! Supported responses: omnidirectional (uniform), cardioid-like, and
//! supercardioid-like, plus an arbitrary **custom curve** sampled over the
//! front hemisphere (0° = facing the listener … 180° = facing away).
//!
//! ## Convention (spec §18 / §153)
//!
//! - `angle = 0` means the listener is exactly in front of the source's
//!   facing; `angle = π` means the listener is directly behind it.
//! - The transform from listener space into source space is
//!   `q_source⁻¹ ∘ q_listener`; [`listener_angle_rad`] is the single shared
//!   implementation of that convention, so the two renderers can never
//!   disagree on it.
//! - Gains are linear and clamped to `[0, 1]`; the curve is evaluated at
//!   block rate (per object) and the per-path one-pole smoothing of the
//!   renderers ramps any change, so movement/rotation never clicks.

use super::math::{Quat, Vec3};

/// Number of samples in a [`CustomDirectivity`] table, covering 0°..=180°
/// in 2° steps (inclusive).
pub const DIRECTIVITY_TABLE_LEN: usize = 91;

/// Angle (radians) between the source's facing direction (+Y in source-local
/// space) and the direction from the source toward the listener, computed in
/// source-local space. `0 = facing the listener`, `π = facing away`.
///
/// `listener_space_position` is the source's position in listener space (the
/// renderer's `apply_to_point` result); `-position` points from the source
/// toward the listener. `source_orientation` / `listener_orientation` are the
/// world-space orientations.
pub fn listener_angle_rad(
    source_orientation: Quat,
    listener_orientation: Quat,
    listener_space_position: Vec3,
) -> f32 {
    // Direction from source → listener, in listener space, normalised.
    let to_listener = (-listener_space_position).normalized().unwrap_or(Vec3::Y);
    // Listener space → source space: source⁻¹ ∘ listener.
    let q = source_orientation
        .inverse_rotation()
        .compose(listener_orientation);
    let dir_src = q.rotate_vec3(to_listener);
    // Angle from the source-local facing (+Y). Clamp guards float drift.
    dir_src.y.clamp(-1.0, 1.0).acos()
}

/// Directional response curve of a source (spec §41).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Directivity {
    /// Uniform in every direction (default; no effect on the level chain).
    #[default]
    Omnidirectional,
    /// Cardioid-like: `(1 + cos θ)/2` — full at the front, null at the back.
    Cardioid,
    /// Supercardioid-like: `0.37 + 0.63·cos θ` (clamped ≥ 0) — tighter front
    /// lobe with side nulls, the classic vocal/spot pattern.
    Supercardioid,
    /// Arbitrary sampled curve (see [`CustomDirectivity`]). Boxed so the
    /// enum stays small; the 91-sample table lives on the heap once and is
    /// only read (never allocated) on the render path.
    Custom(Box<CustomDirectivity>),
}

impl Directivity {
    /// Gain at `angle_rad` (0 = facing the listener, π = facing away).
    /// Always finite and in `[0, 1]` — no undefined output for any angle.
    pub fn gain_at(&self, angle_rad: f32) -> f32 {
        match self {
            Directivity::Omnidirectional => 1.0,
            Directivity::Cardioid => 0.5 * (1.0 + angle_rad.cos()),
            Directivity::Supercardioid => (0.37 + 0.63 * angle_rad.cos()).clamp(0.0, 1.0),
            Directivity::Custom(c) => c.gain_at(angle_rad),
        }
    }
}

/// An arbitrary directivity curve sampled every 2° from 0° (facing the
/// listener) to 180° (facing away), evaluated by linear interpolation.
///
/// The table is a fixed-size `[f32; 91]` so the render path stays
/// allocation-free: `gain_at` is pure arithmetic on a stack-copied struct.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomDirectivity {
    /// Gain samples at 0°, 2°, 4°, …, 180° (index = degrees / 2).
    table: [f32; DIRECTIVITY_TABLE_LEN],
}

impl Default for CustomDirectivity {
    fn default() -> Self {
        Self::new()
    }
}

impl CustomDirectivity {
    /// An omnidirectional custom curve (all samples 1.0).
    pub fn new() -> Self {
        Self {
            table: [1.0; DIRECTIVITY_TABLE_LEN],
        }
    }

    /// Build from explicit samples ordered 0°..=180° in 2° steps
    /// (control path — the table is copied into the fixed array). Returns
    /// `None` unless the slice length is exactly [`DIRECTIVITY_TABLE_LEN`].
    /// Samples are clamped to `[0, 1]`.
    pub fn from_samples(samples: &[f32]) -> Option<Self> {
        if samples.len() != DIRECTIVITY_TABLE_LEN {
            return None;
        }
        let mut table = [0.0f32; DIRECTIVITY_TABLE_LEN];
        for (dst, &src) in table.iter_mut().zip(samples) {
            *dst = src.clamp(0.0, 1.0);
        }
        Some(Self { table })
    }
    /// Set the gain at an angle in degrees (writes the nearest sample).
    pub fn set(&mut self, angle_deg: f32, gain: f32) {
        let idx =
            ((angle_deg.clamp(0.0, 180.0) / 2.0).round() as usize).min(DIRECTIVITY_TABLE_LEN - 1);
        self.table[idx] = gain.clamp(0.0, 1.0);
    }

    /// Wrap this curve into a [`Directivity`] (boxes the table once).
    pub fn into_directivity(self) -> Directivity {
        Directivity::Custom(Box::new(self))
    }

    /// Gain at `angle_rad` by linear interpolation of the 2° table.
    pub fn gain_at(&self, angle_rad: f32) -> f32 {
        let deg = angle_rad.clamp(0.0, std::f32::consts::PI).to_degrees();
        let x = deg / 2.0;
        let i = (x.floor() as usize).min(DIRECTIVITY_TABLE_LEN - 1);
        let j = (i + 1).min(DIRECTIVITY_TABLE_LEN - 1);
        let frac = x - i as f32;
        let a = self.table[i];
        let b = self.table[j];
        (a + (b - a) * frac).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-5;

    #[test]
    fn cardioid_is_unit_front_null_back() {
        let c = Directivity::Cardioid;
        assert!((c.gain_at(0.0) - 1.0).abs() < EPS);
        assert!((c.gain_at(std::f32::consts::FRAC_PI_2) - 0.5).abs() < EPS);
        assert!(c.gain_at(std::f32::consts::PI).abs() < EPS);
    }

    #[test]
    fn supercardioid_stays_non_negative() {
        let s = Directivity::Supercardioid;
        assert!((s.gain_at(0.0) - 1.0).abs() < EPS);
        for angle in [
            0.0,
            0.3,
            std::f32::consts::FRAC_PI_2,
            2.0,
            std::f32::consts::PI,
        ] {
            let g = s.gain_at(angle);
            assert!(
                (0.0..=1.0).contains(&g),
                "supercardioid bounded at {angle}: {g}"
            );
        }
    }

    #[test]
    fn custom_curve_interpolates_and_clamps() {
        let mut c = CustomDirectivity::new();
        c.set(0.0, 1.0);
        c.set(90.0, 0.0);
        c.set(180.0, 0.0);
        // The table is sampled every 2°; 89° interpolates between the 88°
        // sample (default 1.0) and the 90° sample (0.0) → ~0.5.
        let g89 = c.gain_at(89.0f32.to_radians());
        assert!((g89 - 0.5).abs() < 2e-2, "89° interp: {g89}");
        // Exactly-on-sample and endpoint values.
        assert!((c.gain_at(0.0) - 1.0).abs() < EPS);
        assert!(
            c.gain_at(88.0f32.to_radians()) > 0.99,
            "88° sample still 1.0"
        );
        assert!(c.gain_at(std::f32::consts::FRAC_PI_2).abs() < 1e-3);
        assert!(c.gain_at(std::f32::consts::PI).abs() < EPS);
        // Out-of-range clamps: negative angle → front (1.0), > π → back (0.0).
        assert!((c.gain_at(-1.0) - 1.0).abs() < EPS);
        assert!(c.gain_at(4.0).abs() < EPS);
        // from_samples validates length and clamps.
        assert!(CustomDirectivity::from_samples(&[1.0; 10]).is_none());
        let mut samples = [0.0f32; DIRECTIVITY_TABLE_LEN];
        samples[0] = 5.0; // > 1 → clamped
        let cd = CustomDirectivity::from_samples(&samples).unwrap();
        assert!(cd.gain_at(0.0) <= 1.0);
    }

    #[test]
    fn listener_angle_convention() {
        // Object in front of the listener, source facing away (+Y identity):
        // the listener hears it from behind → π.
        let angle = listener_angle_rad(Quat::IDENTITY, Quat::IDENTITY, Vec3::Y);
        assert!(
            (angle - std::f32::consts::PI).abs() < 1e-5,
            "behind: {angle}"
        );
        // Object behind the listener, source facing +Y (toward the listener):
        // heard from the front → 0.
        let angle = listener_angle_rad(Quat::IDENTITY, Quat::IDENTITY, -Vec3::Y);
        assert!(angle.abs() < 1e-5, "facing: {angle}");
        // Source yawed 180° (facing the listener) with the object in front:
        // 0.
        let facing_listener = Quat::from_euler_rad(std::f32::consts::PI, 0.0, 0.0);
        let angle = listener_angle_rad(facing_listener, Quat::IDENTITY, Vec3::Y);
        assert!(angle.abs() < 1e-5, "yawed to face listener: {angle}");
        // Object directly to the right → side (π/2).
        let angle = listener_angle_rad(Quat::IDENTITY, Quat::IDENTITY, Vec3::X);
        assert!((angle - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    }
}
