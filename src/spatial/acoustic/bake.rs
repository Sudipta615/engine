//! Acoustic baking — position-aware propagation caches (v3.26).
//!
//! The primary objective of this phase, per the guide: **turn expensive
//! acoustic computation into reusable render data.** [`AcousticWorld`]'s
//! solver enumerates every path between a source and a listener — direct,
//! image-source reflections, wedge diffraction, portal transmission. For a
//! *static* scene that enumeration is identical block after block, yet the
//! renderers would otherwise re-run it every frame.
//!
//! A [`BakedScene`] precomputes that work for a set of static source
//! positions against a fixed listener, keyed by position (a **position-
//! dependent response cache**). Each [`BakedObject`] stores the resolved
//! [`BakedPath`]s — direction, distance, delay, gain, low-pass corner, path
//! kind and the full per-band material spectrum where a wall interacted —
//! so the renderers look up a light flat response instead of re-solving
//! geometry. [`BakedScene::listener_images`] converts cached reflection
//! paths back into the `ListenerImage` taps the existing
//! `EarlyReflections`/renderer machinery already places.
//!
//! ```text
//! AcousticWorld.solve(source, listener)   ── expensive, run-once
//!         │ AcousticBaker::bake (control path)
//!         v
//! BakedScene { cell → BakedObject }       ── reusable render data
//!         │ listener_images() → [ListenerImage; N]
//!         v
//!   renderers (EarlyReflections tap placement)
//! ```

use crate::spatial::acoustic::geometry::Wall;
use crate::spatial::acoustic::material::MaterialSpectrum;
use crate::spatial::acoustic::path::{AcousticPath, PathKind};
use crate::spatial::acoustic::solver::{AcousticWorld, MAX_PATHS};
use crate::spatial::math::Vec3;
use crate::spatial::room::{ListenerImage, MAX_IMAGES};

/// How finely source positions are cached. Two sources inside the same
/// world-space cube of this side length share one BakedObject (the cache key
/// is the corner of the containing cube). Larger = coarser cache, more reuse;
/// smaller = finer spatial response.
pub const DEFAULT_BAKE_CELL_M: f32 = 0.5;

/// A single cached propagation path for one (static) source position —
/// everything a renderer needs to place one delayed, filtered, attenuated
/// copy of the source. Light and `Copy`; the hot path reads these without
/// touching the solver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BakedPath {
    pub kind: PathKind,
    /// Unit direction from the listener toward the virtual source / bend
    /// point (world space — the renderer's `ListenerTransform` rotates it).
    pub direction: Vec3,
    /// Propagation distance (m) — feeds the renderer's distance model.
    pub distance: f32,
    /// Total source→listener delay via this path (samples).
    pub delay_samples: f32,
    /// Broadband gain (linear) after surface interactions (no distance term —
    /// the renderer's distance model applies that, as for `images_for_object`).
    pub gain: f32,
    /// Low-pass corner (Hz) imparted by the path; `f32::INFINITY` = none.
    pub lowpass_hz: f32,
    /// The wall/edge the path interacts with (if any).
    pub interacting: Option<Wall>,
    /// Full per-band spectrum of the surface cascade for this path (`None`
    /// for paths with no surface interaction). Kept so offline/reference
    /// renderers can do true frequency-domain processing instead of the
    /// collapsed low-pass.
    pub spectrum: Option<MaterialSpectrum>,
}

impl BakedPath {
    fn from_acoustic(p: &AcousticPath, spectrum: Option<MaterialSpectrum>) -> Self {
        Self {
            kind: p.kind,
            direction: p.direction,
            distance: p.distance,
            delay_samples: p.delay_samples,
            gain: p.gain,
            lowpass_hz: p.lowpass_hz,
            interacting: p.interacting,
            spectrum,
        }
    }
}

/// The resolved, render-ready response for one static source position.
#[derive(Debug, Clone, PartialEq)]
pub struct BakedObject {
    /// The cache cell key this object was stored under (a cube corner).
    pub key: (i32, i32, i32),
    /// The source position that produced these paths.
    pub source: Vec3,
    /// The listener position used.
    pub listener: Vec3,
    /// The resolved paths (direct + reflections + diffraction + transmission),
    /// pre-sorted direct-first.
    pub paths: Vec<BakedPath>,
    /// The sample rate baked into this response.
    pub sample_rate: f32,
}

impl BakedObject {
    /// The direct path (kind == Direct) if present.
    pub fn direct(&self) -> Option<&BakedPath> {
        self.paths.iter().find(|p| p.kind == PathKind::Direct)
    }
}

/// A bake policy controlling which path kinds are retained (a host may bake
/// only what it renders — e.g. reflections for a panner, transmission for a
/// networked room).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BakePolicy {
    pub reflections: bool,
    pub diffraction: bool,
    pub transmission: bool,
}

impl Default for BakePolicy {
    fn default() -> Self {
        Self {
            reflections: true,
            diffraction: true,
            transmission: true,
        }
    }
}

/// Position-dependent response cache built from an [`AcousticWorld`] for a
/// set of static source positions against one listener. The render-time API
/// (`listener_images`) is read-only and allocation-free; baking is
/// control/offline-path and heap-happy by design.
#[derive(Debug, Clone, PartialEq)]
pub struct BakedScene {
    /// Listener position used for all bakes.
    pub listener: Vec3,
    /// Cache cell size (metres).
    pub cell: f32,
    /// The position → response map.
    cache: std::collections::HashMap<(i32, i32, i32), BakedObject>,
    /// Solver state snapshot so lookups can rebuild missing cells from the
    /// same world later. Public so hosts can seed a scene from an
    /// [`AcousticBaker`]'s world and accumulate cells incrementally.
    pub world: Option<AcousticWorld>,
    sample_rate: f32,
    /// The bake policy retained for rebuilds.
    policy: BakePolicy,
}

impl Default for BakedScene {
    fn default() -> Self {
        Self::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M)
    }
}

impl BakedScene {
    pub fn new(listener: Vec3, cell: f32) -> Self {
        Self {
            listener,
            cell: cell.max(0.05),
            cache: std::collections::HashMap::new(),
            world: None,
            sample_rate: 48_000.0,
            policy: BakePolicy::default(),
        }
    }

    fn cell_key(&self, p: Vec3) -> (i32, i32, i32) {
        (
            cell_index(p.x, self.cell),
            cell_index(p.y, self.cell),
            cell_index(p.z, self.cell),
        )
    }

    /// Look up a cached response for a static source position (quantised to
    /// the cache cell). Returns `None` if that cell has not been baked.
    pub fn get(&self, source: Vec3) -> Option<&BakedObject> {
        let key = self.cell_key(source);
        self.cache.get(&key)
    }

    /// The listener position used by this bake.
    pub fn listener(&self) -> Vec3 {
        self.listener
    }

    /// Precompute a response for `source` through the current world (control
    /// path), storing it under the source's cell. Idempotent per cell — a
    /// repeated bake of the same cell overwrites the response.
    pub fn bake(&mut self, source: Vec3, sample_rate: f32, policy: BakePolicy) -> &BakedObject {
        self.sample_rate = sample_rate;
        self.policy = policy;
        // Solve once (control path — the expensive part we're caching).
        let mut out = [AcousticPath::direct(source, self.listener, sample_rate, 343.0); MAX_PATHS];
        let n = {
            if self.world.is_none() {
                self.world = Some(AcousticWorld::default());
            }
            let w = self.world.as_mut().expect("just set");
            w.sample_rate = sample_rate;
            w.solve(source, self.listener, &mut out)
        }
        .min(MAX_PATHS);

        // Reflection spectra in image order, for the frequency-domain data.
        let reflection_spectra = self
            .world
            .as_ref()
            .map(|w| w.probe_reflection_spectra(source))
            .unwrap_or_default();

        let mut spectra_iter = reflection_spectra.iter();
        let paths: Vec<BakedPath> = out[..n]
            .iter()
            .filter(|p| match p.kind {
                PathKind::Direct => true,
                PathKind::Reflected => policy.reflections,
                PathKind::Diffracted => policy.diffraction,
                PathKind::Transmitted => policy.transmission,
                PathKind::Diffuse => true,
            })
            .map(|p| {
                let spectrum = if p.kind == PathKind::Reflected {
                    spectra_iter.next().copied()
                } else {
                    None
                };
                BakedPath::from_acoustic(p, spectrum)
            })
            .collect();

        let key = self.cell_key(source);
        let obj = BakedObject {
            key,
            source,
            listener: self.listener,
            paths,
            sample_rate,
        };
        self.cache.insert(key, obj);
        self.cache.get(&key).expect("just inserted")
    }

    /// Convert one cached response into the `ListenerImage` taps the
    /// renderer machinery already understands (`images_for_object`'s output
    /// format). Reflection paths map to speaker-image taps with the
    /// renderer's **excess-delay** convention (`delay = path − direct`), so
    /// the existing panner/binaural tap placement stays bit-equivalent to
    /// the live `images_for_object` path for the same geometry. The direct
    /// path is excluded (the renderer renders the direct path through its
    /// normal pair solve). Writes into `out`, returning the count.
    pub fn listener_images(
        &self,
        obj: &BakedObject,
        out: &mut [ListenerImage; MAX_IMAGES],
    ) -> usize {
        let direct_delay = obj.direct().map(|d| d.delay_samples).unwrap_or(0.0);
        let mut count = 0usize;
        for p in obj.paths.iter().filter(|p| p.kind == PathKind::Reflected) {
            if count >= out.len() {
                break;
            }
            out[count] = ListenerImage {
                dir: p.direction,
                dist: p.distance,
                coeff: p.gain,
                delay: (p.delay_samples - direct_delay).max(0.0).round() as u32,
            };
            count += 1;
        }
        count
    }

    /// Number of cached source cells.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the scene has no baked cells.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Remove all baked responses.
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

fn cell_index(v: f32, cell: f32) -> i32 {
    (v / cell).floor() as i32
}

/// Control-path builder that owns an [`AcousticWorld`] and produces
/// [`BakedScene`]s from it. The baker keeps the world session so a scene can
/// be baked across many source positions, then handed to a renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct AcousticBaker {
    pub world: AcousticWorld,
    pub cell: f32,
}

impl AcousticBaker {
    pub fn new(world: AcousticWorld, cell: f32) -> Self {
        Self {
            world,
            cell: cell.max(0.05),
        }
    }

    /// Bake a whole scene's object positions against `listener` into a fresh
    /// [`BakedScene`]. `positions` are the static object source positions.
    pub fn bake_scene(
        &self,
        positions: impl IntoIterator<Item = Vec3>,
        listener: Vec3,
        sample_rate: f32,
        policy: BakePolicy,
    ) -> BakedScene {
        let mut scene = BakedScene::new(listener, self.cell);
        scene.world = Some(self.world);
        scene.sample_rate = sample_rate;
        scene.policy = policy;
        for p in positions {
            scene.bake(p, sample_rate, policy);
        }
        scene
    }

    /// Bake a single source position into a fresh scene (useful when the
    /// room moves but a host only cares about one response).
    pub fn bake_single(
        &self,
        source: Vec3,
        listener: Vec3,
        sample_rate: f32,
        policy: BakePolicy,
    ) -> BakedScene {
        self.bake_scene(std::iter::once(source), listener, sample_rate, policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::acoustic::geometry::AcousticRoom;
    use crate::spatial::acoustic::material::MaterialKind;
    use crate::spatial::acoustic::solver::wall_index;

    const FS: f32 = 48_000.0;

    fn world() -> AcousticWorld {
        AcousticWorld::new(AcousticRoom::default(), FS)
    }

    #[test]
    fn bake_is_positioned_and_cached_by_cell() {
        let mut scene = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        scene.world = Some(world());
        // Two bakes inside the same 0.5 m cell reuse the single entry.
        scene.bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default());
        scene.bake(Vec3::new(1.1, 2.0, 1.5), FS, BakePolicy::default());
        assert_eq!(scene.len(), 1, "same cell reuses one entry");
        // A well-separated source lands in its own cell.
        scene.bake(Vec3::new(4.0, 5.0, 2.5), FS, BakePolicy::default());
        assert_eq!(scene.len(), 2);
        // An unbaked cell is absent.
        assert!(scene.get(Vec3::new(9.0, 9.0, 9.0)).is_none());
        // The cached response is retrievable by the original position.
        assert!(scene.get(Vec3::new(1.0, 2.0, 1.5)).is_some());
    }

    #[test]
    fn bake_is_deterministic_and_has_direct() {
        // Two identical bakes (fresh scenes) must agree path-for-path.
        let a = {
            let mut s = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
            s.world = Some(world());
            s.bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default())
                .clone()
        };
        let b = {
            let mut s = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
            s.world = Some(world());
            s.bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default())
                .clone()
        };
        assert_eq!(a.paths.len(), 7, "direct + 6 reflections");
        assert!(a.direct().is_some(), "direct path present");
        assert_eq!(a.direct().unwrap().kind, PathKind::Direct);
        for (x, y) in a.paths.iter().zip(b.paths.iter()) {
            assert!(x.delay_samples.is_finite() && x.gain.is_finite());
            assert_eq!(x.kind, y.kind);
            assert!((x.delay_samples - y.delay_samples).abs() < 1e-4);
            assert!(x.direction.length() > 0.9);
            assert!(x.distance > 0.0);
        }
    }

    #[test]
    fn policy_filters_path_kinds() {
        let mut scene = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        scene.world = Some(world());
        let only_reflect = scene
            .bake(
                Vec3::new(1.0, 2.0, 1.5),
                FS,
                BakePolicy {
                    diffraction: false,
                    transmission: false,
                    ..Default::default()
                },
            )
            .clone();
        assert!(only_reflect
            .paths
            .iter()
            .all(|p| p.kind == PathKind::Direct || p.kind == PathKind::Reflected));
    }

    #[test]
    fn listener_images_map_baked_objects_to_taps() {
        let mut scene = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        scene.world = Some(world());
        let obj = scene
            .bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default())
            .clone();
        let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
        let n = scene.listener_images(&obj, &mut imgs);
        assert!(n >= 1, "at least one reflection tap");
        for img in imgs[..n].iter() {
            assert!(img.coeff.is_finite());
            assert!(img.dist > 0.0);
            assert!(img.dir.length() > 0.9);
        }
        // The gain flows from the solved path's collapsed spectrum (0.8 for
        // the default 0.2-absorption material).
        assert!(imgs[0].coeff > 0.5 && imgs[0].coeff <= 1.0);
    }

    #[test]
    fn listener_images_use_excess_delay() {
        let mut scene = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        scene.world = Some(world());
        let obj = scene
            .bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default())
            .clone();
        let direct = obj.direct().unwrap().delay_samples;
        let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
        let n = scene.listener_images(&obj, &mut imgs);
        for img in imgs[..n].iter() {
            let total = img.delay as f32 + direct;
            // The excess tap plus the direct delay recovers the absolute path
            // delay of the corresponding reflection (within a sample).
            let approx = obj
                .paths
                .iter()
                .filter(|p| p.kind == PathKind::Reflected)
                .any(|p| (p.delay_samples - total).abs() < 1.0);
            assert!(approx, "tap delay {total} matches a baked reflection");
        }
    }

    #[test]
    fn bake_retains_per_band_spectra_for_reflections() {
        let mut scene = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        scene.world = Some(world());
        let obj = scene
            .bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default())
            .clone();
        let refl = obj
            .paths
            .iter()
            .find(|p| p.kind == PathKind::Reflected)
            .expect("a reflection");
        assert!(refl.spectrum.is_some(), "reflection carries per-band data");
        assert!(refl.lowpass_hz.is_finite() || refl.lowpass_hz.is_infinite());
    }

    #[test]
    fn bake_respects_material_frequency_content() {
        let baker = AcousticBaker::new(world(), DEFAULT_BAKE_CELL_M);
        let mut f_world = world();
        let mut room = AcousticRoom::default();
        room.walls[wall_index(Wall::MinX)] = MaterialKind::Fabric.spectrum();
        f_world.room = room;
        let baker_f = AcousticBaker::new(f_world, DEFAULT_BAKE_CELL_M);
        let scene = baker.bake_single(
            Vec3::new(1.0, 2.0, 1.5),
            Vec3::ZERO,
            FS,
            BakePolicy::default(),
        );
        let scene_f = baker_f.bake_single(
            Vec3::new(1.0, 2.0, 1.5),
            Vec3::ZERO,
            FS,
            BakePolicy::default(),
        );
        let concrete = scene.get(Vec3::new(1.0, 2.0, 1.5)).unwrap();
        let fabric = scene_f.get(Vec3::new(1.0, 2.0, 1.5)).unwrap();
        // Compare the *darkest* reflection (lowest low-pass): concrete's
        // floor is bright, fabric's floor is a damped wall.
        let cp = concrete
            .paths
            .iter()
            .filter(|p| p.kind == PathKind::Reflected)
            .fold(f32::INFINITY, |m, p| m.min(p.lowpass_hz));
        let fp = fabric
            .paths
            .iter()
            .filter(|p| p.kind == PathKind::Reflected)
            .fold(f32::INFINITY, |m, p| m.min(p.lowpass_hz));
        assert!(
            fp < cp,
            "fabric darkens at least one reflection ({fp} < {cp} expected)"
        );
    }
}
