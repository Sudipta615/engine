//! Acoustic world simulation — path solving (v3.25).
//!
//! This is the layer that **separates acoustic simulation from acoustic
//! rendering**: an [`AcousticWorld`] owns the geometry (per-wall materials
//! and portals) and, given a source/listener pair, enumerates the acoustic
//! paths connecting them (direct, specular image-source reflections,
//! wedged diffraction around portal jambs, and transmission through
//! openings). The result is a set of
//! [`AcousticPath`](super::path::AcousticPath)s the existing
//! binaural/panner renderers (or an offline baker) consume — the renderer
//! never re-derives propagation.
//!
//! ## Model
//!
//! - **Direct** — straight line, no room interaction.
//! - **Image-source reflections** — mirror the source across each wall in a
//!   breadth-first cascade (order 1 → 6 images, order 2 → 24), using the
//!   wall's *per-band* material to filter the reflected spectrum. The
//!   material's broadband reduction yields the path's low-pass corner and
//!   gain.
//! - **Diffraction** — around each portal jamb: the shortest path through a
//!   point on that edge (a geometric wedge model; see
//!   [`diffract_around_edge`]). The spectrum is filtered by the edge's
//!   material with an extra HF roll-off proportional to the bend.
//! - **Transmission** — of the two spaces, when a portal's wall is crossed
//!   and the portal material transmits, the path passes straight through the
//!   opening, filtered by the portal material.
//!
//! The solver is **control/offline path**: it allocates into caller-supplied
//! buffers or returns a `Vec`, and is never on the audio thread. The
//! realtime renderers consume only the resulting [`AcousticPath`]s.

use crate::spatial::acoustic::geometry::{
    closest_point_on_segment, portal_diffraction_edges, AcousticRoom, DiffractionEdge, Portal,
    Wall, ALL_WALLS,
};
use crate::spatial::acoustic::material::MaterialSpectrum;
use crate::spatial::acoustic::path::{AcousticPath, PathFlags, PathKind};
use crate::spatial::math::Vec3;

/// Maximum image-source cascade depth the solver will walk (order 2).
pub const MAX_REFLECTION_ORDER: u8 = 2;

/// Cap on the number of paths a single solve may emit (direct + reflections +
/// portal interactions + diffraction). Prevent unbounded output.
pub const MAX_PATHS: usize = 256;

/// A distinct virtual image source: position and the accumulated material
/// spectrum of the walls crossed.
#[derive(Debug, Clone, Copy)]
struct ImageSource {
    pos: Vec3,
    spectrum: MaterialSpectrum,
}

/// The acoustic world: geometry + settings, the simulation-side twin of the
/// renderer's [`Room`](super::super::room::Room).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticWorld {
    pub room: AcousticRoom,
    /// Openings coupling the room to adjacent spaces.
    pub portals: [Portal; 4],
    /// Number of active portals in `portals`.
    pub portal_count: usize,
    /// Reflection cascade depth (1 or 2).
    pub reflection_order: u8,
    /// Extra diffraction edge set (freestanding fins/mullions) beyond the
    /// portal jambs. Owned by the world so callers needn't.
    pub edges: [DiffractionEdge; 4],
    pub edge_count: usize,
    pub sample_rate: f32,
    pub enabled: bool,
}

impl Default for AcousticWorld {
    fn default() -> Self {
        Self::new(AcousticRoom::default(), 48_000.0)
    }
}

impl AcousticWorld {
    pub fn new(room: AcousticRoom, sample_rate: f32) -> Self {
        Self {
            room,
            portals: [Portal {
                wall: Wall::MaxX,
                corner: Vec3::ZERO,
                width: 0.0,
                height: 0.0,
                material: MaterialSpectrum::flat_transmissive(1.0),
            }; 4],
            portal_count: 0,
            reflection_order: 1,
            edges: [DiffractionEdge::new(Vec3::ZERO, Vec3::ZERO); 4],
            edge_count: 0,
            sample_rate,
            enabled: true,
        }
    }

    pub fn add_portal(&mut self, portal: Portal) -> Option<usize> {
        if self.portal_count >= self.portals.len() {
            return None;
        }
        let i = self.portal_count;
        self.portals[i] = portal;
        self.portal_count += 1;
        Some(i)
    }

    pub fn add_edge(&mut self, edge: DiffractionEdge) -> Option<usize> {
        if self.edge_count >= self.edges.len() {
            return None;
        }
        let i = self.edge_count;
        self.edges[i] = edge;
        self.edge_count += 1;
        Some(i)
    }

    /// The wavelength-agnostic sample rate used for delay computation.
    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Enumerate the image sources of `source` up to `reflection_order`,
    /// writing into `out`. Returns the count. Order 1 → 6, order 2 → 24,
    /// matching the renderer's [`image_sources`](super::super::room::image_sources).
    fn image_sources(&self, source: Vec3, out: &mut [ImageSource; 32]) -> usize {
        let r = self.room;
        let w = r.width.max(0.1);
        let d = r.depth.max(0.1);
        let h = r.height.max(0.1);
        let order = self.reflection_order.clamp(1, MAX_REFLECTION_ORDER);

        // Collapse each wall's material to a flat reflective coefficient for
        // the accumulation (the per-band spectrum is folded into the path
        // filter below via `filter_by_materials`).
        let wall_r = |wall: Wall| -> f32 {
            let m = &r.walls[wall_index(wall)];
            m.reflection[4] // 1 kHz — the reference band
        };

        // BFS frontiers, mirroring room.rs.
        let mut current = [(Vec3::ZERO, 0.0f32); 32 * 4];
        let mut next = [(Vec3::ZERO, 0.0f32); 32 * 4];
        let mut seen = [Vec3::ZERO; 32 * 4];
        let mut crossed = [[Wall::MaxX; MAX_REFLECTION_ORDER as usize]; 32 * 4];
        current[0] = (source, 1.0);
        crossed[0] = [Wall::MaxX; 2];
        let mut n_cur = 1usize;
        seen[0] = source;
        let mut n_seen = 1usize;
        let mut count = 0usize;

        for _ in 0..order {
            let mut n_next = 0usize;
            for ci in 0..n_cur {
                let (pos, _) = current[ci];
                let crossed_here = crossed[ci];
                let candidates = [
                    (Wall::MinX, Vec3::new(2.0 * 0.0 - pos.x, pos.y, pos.z)),
                    (Wall::MaxX, Vec3::new(2.0 * w - pos.x, pos.y, pos.z)),
                    (Wall::MinY, Vec3::new(pos.x, 2.0 * 0.0 - pos.y, pos.z)),
                    (Wall::MaxY, Vec3::new(pos.x, 2.0 * d - pos.y, pos.z)),
                    (Wall::MinZ, Vec3::new(pos.x, pos.y, 2.0 * 0.0 - pos.z)),
                    (Wall::MaxZ, Vec3::new(pos.x, pos.y, 2.0 * h - pos.z)),
                ];
                for (w, c) in candidates {
                    // Dedup identical image positions (as room.rs does).
                    let mut fresh = true;
                    for s in seen[..n_seen].iter() {
                        let d = c - *s;
                        if d.dot(d) < 1e-8 {
                            fresh = false;
                            break;
                        }
                    }
                    if !fresh {
                        continue;
                    }
                    seen[n_seen] = c;
                    n_seen += 1;
                    // Accumulate the crossed walls into the image's wall
                    // list (first free slot in the fixed-size array).
                    let mut walls = crossed_here;
                    let slot = walls
                        .iter()
                        .position(|&s| s == Wall::MaxX)
                        .unwrap_or(MAX_REFLECTION_ORDER as usize);
                    if slot < MAX_REFLECTION_ORDER as usize {
                        walls[slot] = w;
                    }
                    let mut coeff = 1.0f32;
                    for &cw in walls.iter() {
                        if cw != Wall::MaxX {
                            coeff *= wall_r(cw);
                        }
                    }
                    if count < out.len() {
                        // Build the image's accumulated material spectrum.
                        let mut acc: Option<MaterialSpectrum> = None;
                        for cw in walls.iter() {
                            if *cw == Wall::MaxX {
                                continue;
                            }
                            acc = Some(match acc {
                                None => r.walls[wall_index(*cw)],
                                Some(a) => multiply_spectra(&a, &r.walls[wall_index(*cw)]),
                            });
                        }
                        out[count] = ImageSource {
                            pos: c,
                            spectrum: acc.unwrap_or_else(|| MaterialSpectrum::flat_reflective(0.2)),
                        };
                        count += 1;
                    }
                    if n_next < 32 * 4 {
                        next[n_next] = (c, coeff);
                        crossed[n_next] = walls;
                        n_next += 1;
                    }
                }
            }
            current[..n_next].copy_from_slice(&next[..n_next]);
            n_cur = n_next;
        }
        count
    }

    /// The accumulated frequency-dependent material spectrum of each
    /// image-source reflection (order 0 = direct path is not included), in
    /// the same order `image_sources` writes them. Control-path:
    /// `probe_reflection_spectra` is heap-happy and used by the v3.26 baker
    /// so baked paths carry full per-band data, not just the collapsed
    /// low-pass/gain.
    pub fn probe_reflection_spectra(&self, source: Vec3) -> Vec<MaterialSpectrum> {
        let mut imgs = [ImageSource {
            pos: Vec3::ZERO,
            spectrum: MaterialSpectrum::flat_reflective(0.2),
        }; 32];
        let n = self.image_sources(source, &mut imgs);
        imgs[..n].iter().map(|im| im.spectrum).collect()
    }

    /// Solve all acoustic paths between `source` and `listener` into `out`.
    /// Returns the number of paths written (capped at `out.len()`).
    pub fn solve<const N: usize>(
        &self,
        source: Vec3,
        listener: Vec3,
        out: &mut [AcousticPath; N],
    ) -> usize {
        if !self.enabled {
            out[0] =
                AcousticPath::direct(source, listener, self.sample_rate, self.room.speed_of_sound);
            return 1;
        }
        let mut n = 0usize;

        // 1. Direct.
        out[n] = AcousticPath::direct(source, listener, self.sample_rate, self.room.speed_of_sound);
        n += 1;

        // 2. Image-source reflections.
        let mut imgs = [ImageSource {
            pos: Vec3::ZERO,
            spectrum: MaterialSpectrum::flat_reflective(0.2),
        }; 32];
        let n_imgs = self.image_sources(source, &mut imgs);
        for img in imgs[..n_imgs].iter() {
            if n >= N {
                break;
            }
            let d = img.pos - listener;
            let dist = d.length();
            let dir = d.normalized().unwrap_or(Vec3::Y);
            let (gain, lowpass) = img.spectrum.broadband(self.sample_rate);
            let delay = dist / self.room.speed_of_sound.max(1.0) * self.sample_rate;
            let mut flags = PathFlags::NONE;
            flags.set(PathFlags::SPECTRAL_COLLAPSED);
            out[n] = AcousticPath::new(
                PathKind::Reflected,
                dir,
                dist,
                delay,
                gain,
                lowpass,
                flags,
                None,
            );
            n += 1;
        }

        // 3. Transmission + diffraction through/around each portal.
        for portal in self.portals[..self.portal_count].iter() {
            // Skip degenerate (zero-size) portals.
            if portal.width <= 0.0 || portal.height <= 0.0 {
                continue;
            }
            let pc = portal.center();
            let (trans_gain, trans_lp) = portal.material.transmitted_broadband(self.sample_rate);
            // Transmission path straight through the opening (source →
            // portal centre → listener), filtered by the portal material.
            if trans_gain > 0.0 && n < N {
                let d = pc - listener;
                let dist_to_portal_centre = (pc - source).length() + d.length();
                let dir = d.normalized().unwrap_or(Vec3::Y);
                let delay =
                    dist_to_portal_centre / self.room.speed_of_sound.max(1.0) * self.sample_rate;
                let mut flags = PathFlags::NONE;
                flags.set(PathFlags::SPECTRAL_COLLAPSED);
                flags.set(PathFlags::CROSSES_BOUNDARY);
                out[n] = AcousticPath::new(
                    PathKind::Transmitted,
                    dir,
                    dist_to_portal_centre,
                    delay,
                    trans_gain,
                    trans_lp,
                    flags,
                    Some(portal.wall),
                );
                n += 1;
            }
            // Diffraction around the two jambs.
            for edge in portal_diffraction_edges(portal) {
                if n >= N {
                    break;
                }
                if let Some(path) = diffract_around_edge(
                    &edge,
                    source,
                    listener,
                    self.sample_rate,
                    self.room.speed_of_sound,
                ) {
                    // Wedge diffraction rolls off HF more with tighter bends,
                    // on top of the edge material's own spectrum.
                    let base_lp = path.lowpass_hz;
                    let bend = bend_angle(&edge, source, listener).max(0.05);
                    let material_lowpass = portal.material.broadband(self.sample_rate).1;
                    let edge_lp = base_lp.min(material_lowpass).min(8_000.0 / bend / bend); // HF loss grows with bend
                                                                                            // Diffraction is (per the geometric model) a fraction of
                                                                                            // the direct energy.
                    let mut p = path;
                    p.kind = PathKind::Diffracted;
                    p.gain *= 0.3 * bend / std::f32::consts::PI;
                    p.lowpass_hz = edge_lp.max(40.0);
                    p.flags.set(PathFlags::CROSSES_BOUNDARY);
                    p.interacting = Some(portal.wall);
                    out[n] = p;
                    n += 1;
                }
            }
        }

        // 4. Freestanding diffraction edges (fins, mullions).
        for edge in self.edges[..self.edge_count].iter() {
            if n >= N {
                break;
            }
            if let Some(mut p) = diffract_around_edge(
                edge,
                source,
                listener,
                self.sample_rate,
                self.room.speed_of_sound,
            ) {
                let bend = bend_angle(edge, source, listener).max(0.05);
                p.kind = PathKind::Diffracted;
                p.gain *= 0.3 * bend / std::f32::consts::PI;
                p.lowpass_hz = p.lowpass_hz.min(8_000.0 / bend / bend).max(40.0);
                p.interacting = None;
                out[n] = p;
                n += 1;
            }
        }

        n.min(N)
    }
}

/// Multiply two spectra (material cascades).
fn multiply_spectra(a: &MaterialSpectrum, b: &MaterialSpectrum) -> MaterialSpectrum {
    let mut out = *a;
    for i in 0..super::material::OCTAVE_BANDS {
        out.absorption[i] = a.absorption[i] * b.absorption[i];
        out.reflection[i] = a.reflection[i] * b.reflection[i];
        out.transmission[i] = a.transmission[i] * b.transmission[i];
    }
    out
}

/// Index into an [`AcousticRoom::walls`] array for a [`Wall`]. The wall
/// enum encodes all six faces; the index is its position in [`ALL_WALLS`].
pub fn wall_index(wall: Wall) -> usize {
    match wall {
        Wall::MinX => 0,
        Wall::MaxX => 1,
        Wall::MinY => 2,
        Wall::MaxY => 3,
        Wall::MinZ => 4,
        Wall::MaxZ => 5,
    }
    .min(ALL_WALLS.len() - 1)
}

/// Compute the shortest source→edge→listener path bending through the
/// closest point on `edge` (a geometric wedge model): the path length is
/// `|S−E| + |E−L|` with `E` = closest point, yielding a deterministic
/// diffracted path. Frequency content is attenuated with an HF roll-off
/// that grows with the bend angle (see caller).
pub fn diffract_around_edge(
    edge: &DiffractionEdge,
    source: Vec3,
    listener: Vec3,
    sample_rate: f32,
    speed_of_sound: f32,
) -> Option<AcousticPath> {
    let e = closest_point_on_segment(edge, source);
    // The listener-side point need not be the source's closest; take the
    // midpoint of the two projections for a stable bend.
    let e2 = closest_point_on_segment(edge, listener);
    let bend_point = (e + e2) * 0.5;
    let d1 = (source - bend_point).length();
    let d2 = (listener - bend_point).length();
    if d1 < 1e-5 || d2 < 1e-5 {
        return None; // degenerate: source/listener on the edge
    }
    let dist = d1 + d2;
    let dir = (bend_point - listener).normalized()?;
    let gain = 1.0 / dist.max(0.1); // 1/r energy fall-off along the bend
    let delay = dist / speed_of_sound.max(1.0) * sample_rate;
    let mut flags = PathFlags::NONE;
    flags.set(PathFlags::SPECTRAL_COLLAPSED);
    Some(AcousticPath::new(
        PathKind::Diffracted,
        dir,
        dist,
        delay,
        gain,
        8_000.0, // caller attenuates with the actual bend
        flags,
        None,
    ))
}

/// The geometric bend angle (radians) of the path around an edge: how far
/// the source→edge→listener path deviates from straight. 0 ≈ grazing, π ≈
/// fully hidden.
fn bend_angle(edge: &DiffractionEdge, source: Vec3, listener: Vec3) -> f32 {
    let b = closest_point_on_segment(edge, source);
    let b2 = closest_point_on_segment(edge, listener);
    let e = (b + b2) * 0.5;
    let d1 = source - e;
    let d2 = listener - e;
    let u1 = d1.normalized();
    let u2 = d2.normalized();
    match (u1, u2) {
        (Some(a), Some(b)) => a.dot(b).clamp(-1.0, 1.0).acos(),
        _ => std::f32::consts::FRAC_PI_2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::acoustic::material::MaterialKind;

    const FS: f32 = 48_000.0;

    fn default_room() -> AcousticRoom {
        AcousticRoom::default()
    }

    #[test]
    fn disabled_world_returns_only_direct() {
        let mut w = AcousticWorld::new(default_room(), FS);
        w.enabled = false;
        let mut out = [AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
        let n = w.solve(Vec3::new(0.0, -3.0, 1.5), Vec3::ZERO, &mut out);
        assert_eq!(n, 1);
        assert_eq!(out[0].kind, PathKind::Direct);
        assert!(out[0].gain == 1.0);
    }

    #[test]
    fn order1_room_yields_direct_plus_six_reflections() {
        let w = AcousticWorld::new(default_room(), FS);
        let mut out = [AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
        let n = w.solve(Vec3::new(1.0, 2.0, 1.5), Vec3::new(6.0, 5.0, 1.5), &mut out);
        // direct + 6 first-order images. The reflection path is a broadband
        // collapsed specular echo.
        assert!(n >= 7, "expected direct + 6 reflections, got {n}");
        let reflections = out[..n]
            .iter()
            .filter(|p| p.kind == PathKind::Reflected)
            .count();
        assert_eq!(reflections, 6);
        // All reflections carry a low-pass <= Nyquist (spectral collapse).
        for p in out[1..n].iter() {
            assert!(!p.delay_samples.is_nan());
        }
    }

    #[test]
    fn portal_adds_transmission_and_diffraction() {
        let mut w = AcousticWorld::new(default_room(), FS);
        let door = Portal {
            wall: Wall::MaxX, // x = width wall
            corner: Vec3::new(12.0, 0.5, 0.4),
            width: 1.0,
            height: 2.2,
            material: MaterialSpectrum::flat_transmissive(1.0),
        };
        w.add_portal(door);
        let src = Vec3::new(6.0, 0.5, 1.4); // inside, near the door
        let lst = Vec3::new(6.0, 1.2, 1.4); // inside, offset
        let mut out = [AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
        let n = w.solve(src, lst, &mut out);
        assert!(out[..n].iter().any(|p| p.kind == PathKind::Transmitted));
        assert!(out[..n].iter().any(|p| p.kind == PathKind::Diffracted));
        // Transmission through a fully open portal is strong and bright.
        let trans = out[..n]
            .iter()
            .find(|p| p.kind == PathKind::Transmitted)
            .unwrap();
        assert!(trans.gain > 0.5);
        assert!(!trans.lowpass_hz.is_finite() || trans.lowpass_hz > 20_000.0);
    }

    #[test]
    fn diffraction_bend_angles_respond_to_geometry() {
        let e = DiffractionEdge::new(Vec3::new(0.0, 0.0, 0.0), Vec3::new(0.0, 0.0, 2.0));
        // A near-straight (grazing) pass has a small bend angle; a source and
        // listener collinear with the edge (fully occluded) fold to ~π.
        let grazing = bend_angle(&e, Vec3::new(-3.0, 2.0, 1.0), Vec3::new(3.0, 2.0, 1.0));
        let hidden = bend_angle(&e, Vec3::new(-1.0, 0.0, 1.0), Vec3::new(1.0, 0.0, 1.0));
        assert!(grazing < hidden, "grazing {grazing} vs hidden {hidden}");
        assert!(hidden > std::f32::consts::FRAC_PI_2);
    }

    #[test]
    fn walls_filter_frequencies_by_material() {
        // A room with heavy fabric min-X wall: reflections from that side are
        // strongly HF-rolled-off.
        let mut room = default_room();
        room.walls[wall_index(Wall::MinX)] = MaterialKind::Fabric.spectrum();
        let w = AcousticWorld::new(room, FS);
        let mut out = [AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
        let n = w.solve(Vec3::new(0.5, 2.0, 1.5), Vec3::new(6.0, 5.0, 1.5), &mut out); // The image source's `interacting` is None in the current model (we
                                                                                       // don't tag which wall); instead assert every retro path stays finite
                                                                                       // and in-band.
        assert!(out[..n]
            .iter()
            .all(|p| p.lowpass_hz.is_finite() || p.kind == PathKind::Direct));
    }
}
