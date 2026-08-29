//! Minimal linear-algebra primitives for the spatial engine.
//!
//! The engine is deliberately dependency-light (100% pure Rust, minimal
//! deps), so the spatial layer ships its own tiny, allocation-free `Vec3`
//! and `Quat` instead of pulling in `glam` / `nalgebra`. Every operation is
//! unit-tested; there are no hidden conventions.
//!
//! # Coordinate system (spec §17–18)
//!
//! One explicit, right-handed convention is used everywhere in the spatial
//! layer and enforced by tests (never silently assumed):
//!
//! ```text
//!      +Y = front               +Z = up
//!         ↑                        ↑
//!         |                        |
//!  -X ◄───┴──► +X  (right)        |          facing = +Y
//!         |                        |
//!         ↓                        ↓
//!      -Y = rear                -Z = down
//! ```
//!
//! - World-space and listener-space **position** are in metres.
//! - **Angles** are in radians internally; degrees appear only at API
//!   boundaries.
//! - **Quaternion** convention: `[x, y, z, w]`, unit-norm, right-handed,
//!   `w = cos(θ/2)` with the rotation axis `(x,y,z) = sin(θ/2)·a`.
//! - Handedness: a positive rotation about a unit axis follows the
//!   right-hand rule (a positive yaw about +Z turns +X toward +Y).

use serde::{Deserialize, Serialize};
use std::ops::{Add, Mul, Neg, Sub};

/// A vector in 3D world space (metres by convention).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3::new(0.0, 0.0, 0.0);
    pub const X: Vec3 = Vec3::new(1.0, 0.0, 0.0);
    /// Forward (front) unit vector.
    pub const Y: Vec3 = Vec3::new(0.0, 1.0, 0.0);
    /// Up unit vector.
    pub const Z: Vec3 = Vec3::new(0.0, 0.0, 1.0);

    #[inline]
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    #[inline]
    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    #[inline]
    pub fn cross(self, o: Self) -> Self {
        Self {
            x: self.y * o.z - self.z * o.y,
            y: self.z * o.x - self.x * o.z,
            z: self.x * o.y - self.y * o.x,
        }
    }

    #[inline]
    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    #[inline]
    pub fn length(self) -> f32 {
        self.length_squared().sqrt()
    }

    /// Normalise to unit length. Returns `None` for the (near-)zero vector
    /// so degeneracies surface as deterministic errors, never NaN.
    pub fn normalized(self) -> Option<Self> {
        let len = self.length();
        if len > f32::EPSILON {
            Some(Self {
                x: self.x / len,
                y: self.y / len,
                z: self.z / len,
            })
        } else {
            None
        }
    }

    /// Rotate this vector by a unit quaternion `q`.
    #[inline]
    pub fn rotate_by(self, q: Quat) -> Self {
        let qx = q.x;
        let qy = q.y;
        let qz = q.z;
        let qw = q.w;
        // t = 2·(qv × v)
        let tx = 2.0 * (qy * self.z - qz * self.y);
        let ty = 2.0 * (qz * self.x - qx * self.z);
        let tz = 2.0 * (qx * self.y - qy * self.x);
        // v + qw·t
        let px = self.x + qw * tx;
        let py = self.y + qw * ty;
        let pz = self.z + qw * tz;
        // + qv × t — the correct second order term (spec math § quaternion).
        Self {
            x: px + (qy * tz - qz * ty),
            y: py + (qz * tx - qx * tz),
            z: pz + (qx * ty - qy * tx),
        }
    }

    /// Azimuth angle in the horizontal (XY) plane, in radians.
    ///
    /// Measured from **front (+Y) toward right (+X)**, matching the diagram:
    /// `0 = front`, `+π/2 = right`, `±π = rear`, `-π/2 = left`.
    #[inline]
    pub fn azimuth_rad(self) -> f32 {
        self.x.atan2(self.y)
    }

    /// Elevation angle above the horizontal (XY) plane, in `[-π/2, π/2]`
    /// radians: `0` at the horizon, `+π/2` straight up, `-π/2` straight
    /// down.
    #[inline]
    pub fn elevation_rad(self) -> f32 {
        let horiz = (self.x * self.x + self.y * self.y).sqrt();
        self.z.atan2(horiz)
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    #[inline]
    fn add(self, o: Self) -> Self {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    #[inline]
    fn sub(self, o: Self) -> Self {
        Vec3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Vec3;
    #[inline]
    fn mul(self, s: f32) -> Self {
        Vec3::new(self.x * s, self.y * s, self.z * s)
    }
}

impl Neg for Vec3 {
    type Output = Vec3;
    #[inline]
    fn neg(self) -> Self {
        Vec3::new(-self.x, -self.y, -self.z)
    }
}

/// A unit quaternion `[x, y, z, w]` (right-handed, `w` = cos(θ/2)).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Quat {
    pub const IDENTITY: Quat = Quat::new(0.0, 0.0, 0.0, 1.0);

    #[inline]
    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    /// Build a unit quaternion from an axis and an angle in radians. The
    /// axis is normalised internally; a zero axis falls back to `+Y`.
    pub fn from_axis_angle(axis: Vec3, angle: f32) -> Self {
        let axis = axis.normalized().unwrap_or(Vec3::Y);
        let half = angle * 0.5;
        let s = half.sin();
        Self::new(axis.x * s, axis.y * s, axis.z * s, half.cos())
    }

    /// Build a rotation from Euler angles expressed as three axis-angle
    /// rotations applied in `yaw (about +Z) → pitch (about +X) → roll
    /// (about +Y)` order. Yaw rotates in the horizontal plane, pitch tilts
    /// the front vector up/down, roll banks about the forward axis.
    ///
    /// Angles are in radians. The composition order is fixed and tested.
    pub fn from_euler_rad(yaw: f32, pitch: f32, roll: f32) -> Self {
        // Yaw rotates about +Z but sign-flipped so that a **positive** yaw is
        // a right turn as seen from above: it drives the facing vector toward
        // +X, consistent with `azimuth_rad` (where +90° = right). Pitch (about
        // +X) and roll (about +Y) keep their positive CCW senses.
        let qyaw = Quat::from_axis_angle(Vec3::Z, -yaw);
        let qpitch = Quat::from_axis_angle(Vec3::X, pitch);
        let qroll = Quat::from_axis_angle(Vec3::Y, roll);
        // Apply yaw first, then pitch, then roll (right-most is applied
        // first by `compose`).
        qroll.compose(qpitch.compose(qyaw))
    }

    /// The conjugate. For a unit quaternion this is also the rotation
    /// inverse.
    #[inline]
    pub fn conjugate(self) -> Self {
        Self::new(-self.x, -self.y, -self.z, self.w)
    }

    /// Composition `self ∘ other`: rotate by `other` first, then by `self`.
    ///
    /// Named `compose` (not `mul`) so it does not shadow `std::ops::Mul` for
    /// the rotation quaternion type.
    pub fn compose(self, o: Self) -> Self {
        Self {
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
        }
    }

    /// Rotate a vector by this quaternion.
    #[inline]
    pub fn rotate_vec3(self, v: Vec3) -> Vec3 {
        v.rotate_by(self)
    }

    /// The rotation taking world-space vectors into listener space — the
    /// conjugate of the listener's orientation. Rotating the listener yaw
    /// clockwise turns world-fixed objects counter-clockwise in listener
    /// space (they appear to move as the head turns). This is only the
    /// rotation part; see
    /// [`crate::spatial::scene::ListenerTransform`] for the full
    /// rotate-then-translate transform.
    #[inline]
    pub fn inverse_rotation(self) -> Self {
        self.conjugate()
    }

    /// The quaternion of the same rotation with the opposite sign (the
    /// shortest-path double cover: `q` and `−q` rotate identically). Used by
    /// the interpolation routines to pick the shorter arc.
    #[inline]
    pub fn negated(self) -> Self {
        Self::new(-self.x, -self.y, -self.z, -self.w)
    }

    /// The dot product of the two quaternion 4-vectors.
    #[inline]
    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z + self.w * o.w
    }

    /// The 4-vector norm (1 for a unit quaternion).
    #[inline]
    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    /// Whether every component is finite.
    #[inline]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite() && self.w.is_finite()
    }

    /// Normalise to unit norm. Returns `None` for the (near-)zero
    /// quaternion.
    pub fn normalized(self) -> Option<Self> {
        let l = self.dot(self).sqrt();
        if l > f32::EPSILON {
            Some(Self::new(self.x / l, self.y / l, self.z / l, self.w / l))
        } else {
            None
        }
    }

    /// Shortest-path normalized linear interpolation (`nlerp`) toward
    /// `other` at `t ∈ [0, 1]`. `t = 0` returns `self`, `t = 1` returns
    /// `other`; the interpolant takes the *shorter* arc (flipping `other`'s
    /// sign when the dot product is negative — `q` and `−q` are the same
    /// rotation). Cheaper than slerp and visually indistinguishable for the
    /// small per-block steps head tracking produces.
    pub fn nlerp(self, other: Self, t: f32) -> Self {
        let o = if self.dot(other) < 0.0 {
            other.negated()
        } else {
            other
        };
        let q = self * (1.0 - t) + o * t;
        q.normalized().unwrap_or(self)
    }

    /// The rotation angle (radians) between two unit quaternions, in
    /// `[0, π]` — `2·acos(|dot|)`, the shortest-arc magnitude.
    #[inline]
    pub fn angle_to(self, other: Self) -> f32 {
        2.0 * self.dot(other).abs().clamp(-1.0, 1.0).acos()
    }
}

impl Add for Quat {
    type Output = Quat;
    #[inline]
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z, self.w + o.w)
    }
}

impl Mul<f32> for Quat {
    type Output = Quat;
    #[inline]
    fn mul(self, s: f32) -> Self {
        Self::new(self.x * s, self.y * s, self.z * s, self.w * s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-5;

    #[test]
    fn length_and_normalize() {
        let v = Vec3::new(3.0, 4.0, 0.0);
        assert!((v.length() - 5.0).abs() < EPS);
        let u = v.normalized().unwrap();
        assert!((u.length() - 1.0).abs() < EPS);
        assert!(Vec3::ZERO.normalized().is_none());
    }

    #[test]
    fn azimuth_convention_front_right_rear_left() {
        assert!(Vec3::Y.azimuth_rad().abs() < EPS); // front = 0
        assert!((Vec3::X.azimuth_rad() - std::f32::consts::FRAC_PI_2).abs() < EPS);
        assert!(((-Vec3::Y).azimuth_rad().abs() - std::f32::consts::PI).abs() < EPS);
        assert!(((-Vec3::X).azimuth_rad() + std::f32::consts::FRAC_PI_2).abs() < EPS);
    }

    #[test]
    fn elevation_up_down() {
        assert!((Vec3::Z.elevation_rad() - std::f32::consts::FRAC_PI_2).abs() < EPS);
        assert!(((-Vec3::Z).elevation_rad() + std::f32::consts::FRAC_PI_2).abs() < EPS);
        assert!(Vec3::X.elevation_rad().abs() < EPS);
    }

    #[test]
    fn dot_cross_and_ops() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, -5.0, 6.0);
        assert!((a.dot(b) - (4.0 - 10.0 + 18.0)).abs() < EPS);
        let c = Vec3::X.cross(Vec3::Y);
        assert!((c - Vec3::Z).length() < EPS);
        let d = a + b;
        assert!((d - Vec3::new(5.0, -3.0, 9.0)).length() < EPS);
        let e = a * 2.0;
        assert!((e - Vec3::new(2.0, 4.0, 6.0)).length() < EPS);
        let f = -a;
        assert!((f - Vec3::new(-1.0, -2.0, -3.0)).length() < EPS);
    }

    #[test]
    fn quat_identity_rotates_nothing() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert!((Quat::IDENTITY.rotate_vec3(v) - v).length() < EPS);
    }

    #[test]
    fn quat_axis_angle_90deg_about_z() {
        // Rotate +X (right) by +90° about +Z (right-hand rule): +Y (front).
        let q = Quat::from_axis_angle(Vec3::Z, std::f32::consts::FRAC_PI_2);
        assert!((q.rotate_vec3(Vec3::X) - Vec3::Y).length() < 1e-5);
    }

    #[test]
    fn quat_compose_then_inverse_is_identity() {
        let a = Quat::from_axis_angle(Vec3::Z, 0.4);
        let b = Quat::from_axis_angle(Vec3::X, -0.3);
        let q = a.compose(b);
        let qinv = q.inverse_rotation();
        // q ∘ qinv ≈ identity.
        let prod = q.compose(qinv);
        assert!(prod.x.abs() < 1e-6);
        assert!(prod.y.abs() < 1e-6);
        assert!(prod.z.abs() < 1e-6);
        assert!((prod.w - 1.0).abs() < 1e-6);
        // Applying q then its inverse round-trips any vector.
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert!((qinv.rotate_vec3(q.rotate_vec3(v)) - v).length() < 1e-5);
    }

    #[test]
    fn euler_yaw_only_rotates_front_to_right() {
        let q = Quat::from_euler_rad(std::f32::consts::FRAC_PI_2, 0.0, 0.0);
        assert!((q.rotate_vec3(Vec3::Y) - Vec3::X).length() < 1e-5);
    }

    #[test]
    fn euler_pitch_tilts_front_vector_up() {
        // Pitch +90° (about +X): +Y (front) tilts up to +Z.
        let q = Quat::from_euler_rad(0.0, std::f32::consts::FRAC_PI_2, 0.0);
        assert!((q.rotate_vec3(Vec3::Y) - Vec3::Z).length() < 1e-4);
    }

    #[test]
    fn nlerp_endpoints_and_yaw_midpoint() {
        // t=0 → self, t=1 → other; the midpoint of a 0°→90° yaw is a 45°
        // yaw (pinned via angle_to, which the tracker uses).
        let q0 = Quat::IDENTITY;
        let q90 = Quat::from_euler_rad(std::f32::consts::FRAC_PI_2, 0.0, 0.0);
        assert!(q0.nlerp(q90, 0.0).angle_to(q0) < 1e-6, "t=0 returns self");
        assert!(q0.nlerp(q90, 1.0).angle_to(q90) < 1e-6, "t=1 returns other");
        let mid = q0.nlerp(q90, 0.5);
        assert!((mid.length() - 1.0).abs() < 1e-6, "unit length");
        let deg = mid.angle_to(Quat::IDENTITY).to_degrees();
        assert!((deg - 45.0).abs() < 1e-3, "midpoint yaw {deg}°");
    }

    #[test]
    fn nlerp_takes_the_shortest_arc() {
        // 350° → 10° must rotate the *short* way through 0°, not the long
        // way through 180°. The midpoint quat is ±(0,0,0,1) ≈ identity, so
        // its angle to identity is ~0 (not ~180°).
        let q350 = Quat::from_euler_rad(350.0_f32.to_radians(), 0.0, 0.0);
        let q10 = Quat::from_euler_rad(10.0_f32.to_radians(), 0.0, 0.0);
        let mid = q350.nlerp(q10, 0.5);
        let deg = mid.angle_to(Quat::IDENTITY).to_degrees();
        assert!(deg < 1.0, "short way through 0° (mid at {deg}°)");
        assert!((mid.length() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn angle_to_and_negated() {
        let q90 = Quat::from_euler_rad(std::f32::consts::FRAC_PI_2, 0.0, 0.0);
        assert!((Quat::IDENTITY.angle_to(q90) - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!(Quat::IDENTITY.angle_to(Quat::IDENTITY.negated()).abs() < 1e-6);
        // q and −q rotate identically.
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert!((q90.rotate_vec3(v) - q90.negated().rotate_vec3(v)).length() < 1e-6);
        // angle_to is bounded to [0, π].
        let q180 = Quat::from_euler_rad(std::f32::consts::PI, 0.0, 0.0);
        assert!((Quat::IDENTITY.angle_to(q180) - std::f32::consts::PI).abs() < 1e-5);
    }

    #[test]
    fn quat_scalar_ops_and_normalize() {
        let q = Quat::new(1.0, 2.0, 3.0, 4.0);
        let s = q * 2.0;
        assert_eq!(s, Quat::new(2.0, 4.0, 6.0, 8.0));
        let sum = q + q;
        assert_eq!(sum, Quat::new(2.0, 4.0, 6.0, 8.0));
        let n = q.normalized().unwrap();
        assert!((n.length() - 1.0).abs() < 1e-6);
        assert!(Quat::new(0.0, 0.0, 0.0, 0.0).normalized().is_none());
    }

    #[test]
    fn listener_inverse_rotation_keeps_world_fixed() {
        // Listener yaws +90°. A world-fixed object at +X (to the world's
        // right) must appear at the listener's +Y (front).
        let listener_orient = Quat::from_euler_rad(std::f32::consts::FRAC_PI_2, 0.0, 0.0);
        let world_object = Vec3::X;
        let local = listener_orient.inverse_rotation().rotate_vec3(world_object);
        assert!((local - Vec3::Y).length() < 1e-5);
    }
}
