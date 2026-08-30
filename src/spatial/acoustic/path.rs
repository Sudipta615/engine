//! Acoustic propagation paths — the interface between the simulation and
//! any renderer (v3.25, Direction 6).
//!
//! The acoustic *world* (`solver.rs`) produces a set of [`AcousticPath`]s
//! from a source/listener pair. A path is everything a renderer needs to
//! place one delayed, filtered, attenuated copy of the source: how far,
//! which direction, how many samples late, and how the material
//! interactions shaped the spectrum. This is the contract the guide's
//! "separate acoustic simulation from acoustic rendering" hinges on — the
//! renderer consumes paths and never re-derives propagation itself.

use crate::spatial::acoustic::geometry::Wall;
use crate::spatial::math::Vec3;

/// The kind of interaction a path represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PathKind {
    /// Line of sight, no interaction.
    Direct,
    /// One or more specular wall reflections (image source).
    Reflected,
    /// Bending around an edge (diffraction).
    Diffracted,
    /// Passing through an opening / transparent material (transmission).
    Transmitted,
    /// A diffuse (non-specular) contribution — a declared seam for
    /// scattering / late energy.
    Diffuse,
}

/// Bit-flag metadata carried on a path that influences how a renderer handles
/// it (surface material filtering, occlusion culls, elevation weighting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathFlags(pub u8);

impl PathFlags {
    pub const NONE: PathFlags = PathFlags(0);
    /// The path's spectrum has been collapsed to a broadband gain; the
    /// renderer should apply `lowpass_hz` from the path (otherwise the path
    /// carries the full material spectrum separately).
    pub const SPECTRAL_COLLAPSED: u8 = 1 << 0;
    /// The path passes through or bends around the room's boundary (not a
    /// purely interior reflection).
    pub const CROSSES_BOUNDARY: u8 = 1 << 1;

    #[inline]
    pub fn set(&mut self, flag: u8) {
        self.0 |= flag;
    }

    #[inline]
    pub fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}

/// One resolved propagation path from a source to the listener.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticPath {
    pub kind: PathKind,
    /// Unit direction *from* the listener toward the virtual source /
    /// diffracted point (world space), matching the renderers' convention.
    pub direction: Vec3,
    /// Total propagation distance (m).
    pub distance: f32,
    /// Total propagation delay in samples.
    pub delay_samples: f32,
    /// Broadband gain (linear) after distance attenuation and any surface
    /// interaction.
    pub gain: f32,
    /// Low-pass corner (Hz) imparted by the path; `f32::INFINITY` = none.
    pub lowpass_hz: f32,
    /// Path metadata flags.
    pub flags: PathFlags,
    /// The wall/edge the path interacts with (if any); `None` for Direct.
    pub interacting: Option<Wall>,
}

impl AcousticPath {
    /// Assemble a path from its raw fields. Eight arguments is deliberate:
    /// every field of a path is meaningful and named, and there is nothing to
    /// omit.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        kind: PathKind,
        direction: Vec3,
        distance: f32,
        delay_samples: f32,
        gain: f32,
        lowpass_hz: f32,
        flags: PathFlags,
        interacting: Option<Wall>,
    ) -> Self {
        Self {
            kind,
            direction,
            distance,
            delay_samples,
            gain,
            lowpass_hz,
            flags,
            interacting,
        }
    }

    /// Build a direct path between two positions with a given sample rate
    /// and speed of sound.
    pub fn direct(source: Vec3, listener: Vec3, sample_rate: f32, speed_of_sound: f32) -> Self {
        let d = source - listener;
        let dist = d.length();
        let dir = d.normalized().unwrap_or(Vec3::Y);
        Self::new(
            PathKind::Direct,
            dir,
            dist,
            dist / speed_of_sound.max(1.0) * sample_rate,
            1.0,
            f32::INFINITY,
            PathFlags::NONE,
            None,
        )
    }

    /// Whether the path carries any low-pass filtering (a real corner, not
    /// the "transparent" infinity/edge-flag sentinel).
    pub fn is_filtered(&self) -> bool {
        self.lowpass_hz.is_finite() && self.lowpass_hz > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 48_000.0;

    #[test]
    fn direct_path_delay_is_distance_over_sound() {
        // 3 m away, straight back.
        let path = AcousticPath::direct(Vec3::new(0.0, -3.0, 0.0), Vec3::ZERO, FS, 343.0);
        assert_eq!(path.kind, PathKind::Direct);
        assert!((path.distance - 3.0).abs() < 1e-5);
        // 3/343 s × 48 kHz
        let expect = 3.0 / 343.0 * FS;
        assert!((path.delay_samples - expect).abs() < 1e-3);
        assert!((path.gain - 1.0).abs() < 1e-6);
        assert!(!path.is_filtered());
        assert!(path.flags == PathFlags::NONE);
        assert_eq!(path.interacting, None);
    }

    #[test]
    fn flags_sets_and_tests() {
        let mut f = PathFlags::NONE;
        assert!(!f.has(PathFlags::SPECTRAL_COLLAPSED));
        f.set(PathFlags::SPECTRAL_COLLAPSED);
        assert!(f.has(PathFlags::SPECTRAL_COLLAPSED));
        assert!(!f.has(PathFlags::CROSSES_BOUNDARY));
        f.set(PathFlags::CROSSES_BOUNDARY);
        assert!(f.has(0b11));
    }
}
