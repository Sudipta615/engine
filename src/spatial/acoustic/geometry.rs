//! Acoustic geometry — walls, portals, and diffraction edges (v3.25).
//!
//! This is the **static** side of the acoustic world: the shapes sound
//! interacts with. Path solving (`solver.rs`) consumes these; the renderers
//! never see them directly.
//!
//! The room is an axis-aligned box (origin at a corner), matching
//! [`super::super::room::Room`]; each of its six walls carries its own
//! frequency-dependent [`MaterialSpectrum`](super::material::MaterialSpectrum)
//! (the per-wall seam the older [`Room::absorption`](super::super::room::Room::absorption))
//! documented). A **portal** is an opening in a wall that couples two
//! spaces; a **diffraction edge** is an open boundary sound bends around
//! (a door jamb, a window sill, a freestanding mullion).

use crate::spatial::acoustic::material::MaterialSpectrum;
use crate::spatial::math::Vec3;

/// The index of one of the six walls of the box in the solver's convention.
///
/// Order matches the reflection enumeration in
/// [`image_sources`](super::super::room::image_sources): `x=0, x=w, y=0,
/// y=d, z=0, z=h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Wall {
    /// The `x = 0` wall.
    MinX,
    /// The `x = width` wall.
    MaxX,
    /// The `y = 0` wall.
    MinY,
    /// The `y = depth` wall.
    MaxY,
    /// The `z = 0` wall.
    MinZ,
    /// The `z = height` wall.
    MaxZ,
}

/// All six walls, in the canonical enumeration order.
pub const ALL_WALLS: [Wall; 6] = [
    Wall::MinX,
    Wall::MaxX,
    Wall::MinY,
    Wall::MaxY,
    Wall::MinZ,
    Wall::MaxZ,
];

impl Wall {
    /// The wall's outward unit normal in world space.
    pub fn normal(self) -> Vec3 {
        match self {
            Wall::MinX => Vec3::new(-1.0, 0.0, 0.0),
            Wall::MaxX => Vec3::new(1.0, 0.0, 0.0),
            Wall::MinY => Vec3::new(0.0, -1.0, 0.0),
            Wall::MaxY => Vec3::new(0.0, 1.0, 0.0),
            Wall::MinZ => Vec3::new(0.0, 0.0, -1.0),
            Wall::MaxZ => Vec3::new(0.0, 0.0, 1.0),
        }
    }

    /// The coordinate (0, width, depth or height) at which this wall sits.
    pub fn plane_coord(self, width: f32, depth: f32, height: f32) -> f32 {
        match self {
            Wall::MinX | Wall::MinY | Wall::MinZ => 0.0,
            Wall::MaxX => width,
            Wall::MaxY => depth,
            Wall::MaxZ => height,
        }
    }
}

/// One wall of the room, with its own frequency-dependent material.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WallSurface {
    pub wall: Wall,
    pub material: MaterialSpectrum,
}

/// An axis-aligned box room with **per-wall** materials — the replacement
/// for the single-scalar [`Room`](super::super::room::Room) in the
/// simulation layer. Kept deliberately small and pure (control path);
/// rendering consumes solved paths, not this.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticRoom {
    pub width: f32,
    pub depth: f32,
    pub height: f32,
    /// Per-wall spectra in [`ALL_WALLS`] order.
    pub walls: [MaterialSpectrum; 6],
    /// Speed of sound (m/s).
    pub speed_of_sound: f32,
}

impl Default for AcousticRoom {
    /// A 12×10×3 m box with medium reflective default walls (the same
    /// geometry as the renderer's [`Room`] default).
    fn default() -> Self {
        Self {
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            walls: [MaterialSpectrum::flat_reflective(0.2); 6],
            speed_of_sound: 343.0,
        }
    }
}

impl AcousticRoom {
    /// Build an acoustic room from the renderer's [`Room`] geometry and a
    /// single material applied to every wall (the legacy scalar
    /// coefficient becomes the material's broadband absorption).
    pub fn from_render_room(
        room: &crate::spatial::room::Room,
        wall_material: MaterialSpectrum,
    ) -> Self {
        Self {
            width: room.width,
            depth: room.depth,
            height: room.height,
            walls: [wall_material; 6],
            speed_of_sound: room.speed_of_sound.max(1.0),
        }
    }

    /// Whether a world-space point is strictly inside the box (used to test
    /// line-of-sight against a portal).
    pub fn contains(&self, p: Vec3) -> bool {
        (0.0..self.width).contains(&p.x)
            && (0.0..self.depth).contains(&p.y)
            && (0.0..self.height).contains(&p.z)
    }
}

/// A rectangular opening in a wall coupling two spaces — a doorway, a
/// window, an arch. Portals have a geometric opening (so the direct /
/// peered paths can pass) and a material (so the sounds travelling through
/// them can be filtered and attenuated, e.g. a closed door). `MaterialKind`
/// `OpenMesh` yields a fully open portal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Portal {
    /// Which wall the opening is cut in.
    pub wall: Wall,
    /// Lower corner of the opening rectangle (world space, on the wall).
    pub corner: Vec3,
    /// Width (`x` extent on a `y`-oriented opening, else the sensible axis)
    /// and height in metres.
    pub width: f32,
    pub height: f32,
    pub material: MaterialSpectrum,
}

impl Portal {
    /// The centre of the opening in world space.
    pub fn center(&self) -> Vec3 {
        self.corner
            + match self.wall {
                Wall::MinX | Wall::MaxX => Vec3::new(0.0, self.width * 0.5, self.height * 0.5),
                Wall::MinY | Wall::MaxY => Vec3::new(self.width * 0.5, 0.0, self.height * 0.5),
                Wall::MinZ | Wall::MaxZ => Vec3::new(self.width * 0.5, self.height * 0.5, 0.0),
            }
    }

    /// The two vertical diffraction edges of a doorway-style opening (the
    /// jambs sound bends around), as an axis-aligned pair. Returns an empty
    /// iterator when the portal material fully transmits (no edge to diffract
    /// around).
    pub fn jamb_edges(&self) -> [Vec3; 2] {
        match self.wall {
            Wall::MinX | Wall::MaxX => [
                Vec3::new(self.corner.x, self.corner.y, self.corner.z + self.height),
                Vec3::new(
                    self.corner.x,
                    self.corner.y + self.width,
                    self.corner.z + self.height,
                ),
            ],
            Wall::MinY | Wall::MaxY => [
                Vec3::new(self.corner.x, self.corner.y, self.corner.z + self.height),
                Vec3::new(
                    self.corner.x + self.width,
                    self.corner.y,
                    self.corner.z + self.height,
                ),
            ],
            Wall::MinZ | Wall::MaxZ => [
                Vec3::new(self.corner.x, self.corner.y, self.corner.z),
                Vec3::new(
                    self.corner.x + self.width,
                    self.corner.y + self.height,
                    self.corner.z,
                ),
            ],
        }
    }
}

/// A straight edge sound can diffract around (a freestanding fin, a mullion,
/// the edge of a partition). Defined as a segment `a→b` in world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiffractionEdge {
    pub a: Vec3,
    pub b: Vec3,
}

impl DiffractionEdge {
    pub const fn new(a: Vec3, b: Vec3) -> Self {
        Self { a, b }
    }

    /// The edge's direction unit vector.
    pub fn dir(&self) -> Vec3 {
        (self.b - self.a)
            .normalized()
            .unwrap_or(crate::spatial::math::Vec3::Z)
    }
}

/// Build the two jamb edges as [`DiffractionEdge`]s for a portal.
pub fn portal_diffraction_edges(p: &Portal) -> [DiffractionEdge; 2] {
    p.jamb_edges()
        .map(|top| DiffractionEdge::new(p.center(), top))
}

/// A query point projected onto (and clamped to) a segment.
pub(crate) fn closest_point_on_segment(seg: &DiffractionEdge, p: Vec3) -> Vec3 {
    let ab = seg.b - seg.a;
    let t = (p - seg.a).dot(ab) / ab.length_squared().max(1e-12);
    seg.a + ab * t.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_plane_coords_match_the_box() {
        let r = AcousticRoom::default();
        for (i, w) in ALL_WALLS.iter().enumerate() {
            let c = w.plane_coord(r.width, r.depth, r.height);
            assert!(
                c == 0.0 || c == r.width || c == r.depth || c == r.height,
                "{i}"
            );
        }
    }

    #[test]
    fn room_contains_inside_only() {
        let r = AcousticRoom::default();
        assert!(r.contains(Vec3::new(6.0, 5.0, 1.5)));
        assert!(!r.contains(Vec3::new(-0.5, 5.0, 1.5)));
        assert!(!r.contains(Vec3::new(6.0, 5.0, 4.0)));
    }

    #[test]
    fn doorway_center_and_jambs() {
        // A door in the x=9 wall, centred vertically, 1 m wide × 2.2 m high.
        let door = Portal {
            wall: Wall::MaxX,
            corner: Vec3::new(9.0, 4.0, 0.4),
            width: 1.0,
            height: 2.2,
            material: MaterialSpectrum::flat_transmissive(1.0),
        };
        assert!((door.center() - Vec3::new(9.0, 4.5, 1.5)).length() < 1e-5);
        for e in portal_diffraction_edges(&door) {
            assert!((e.b - e.a).length() > 0.0);
        }
    }
}
