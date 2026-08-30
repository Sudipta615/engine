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
use crate::spatial::acoustic::material::{surface_lowpass_hz, MaterialSpectrum};
use crate::spatial::acoustic::path::{AcousticPath, PathKind};
use crate::spatial::acoustic::solver::{AcousticWorld, MAX_PATHS};
use crate::spatial::level::AirAbsorption;
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
/// touching the solver. Serde: baked scenes are embedded verbatim in aelog
/// scene-swap logs (v3.37), so an animated world's snapshots replay
/// exactly.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// Serde: JSON cannot carry non-finite floats (serde_json emits
    /// `null`), so the `f32::INFINITY` sentinel maps to `-1.0` and back.
    #[serde(with = "hz_serde")]
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

/// Kernel length (FFT point count) used to synthesize a per-path spectral
/// filter. A power of two ≥ 16 (the correction renderer's constraint);
/// smooth octave-band material spectra need only a modest IR to shape.
pub const ACOUSTIC_IR_LEN: usize = 1024;

impl BakedScene {
    /// Render one cached response into **per-path spectral taps** — the
    /// production form the `Acoustic` node consumes (v3.40). Replaces the
    /// collapsed broadband tap (`gain` at `excess`) with a real filter per
    /// non-direct path:
    ///
    /// * a **reflection** with a full per-band [`MaterialSpectrum`] applies
    ///   that spectrum directly (sampled per FFT bin via
    ///   `reflectivity_at_hz`) — a curtain that eats the treble genuinely
    ///   darkens a reflection instead of just scaling it;
    /// * a **diffraction / transmission** path that was collapsed to a
    ///   corner (a finite `lowpass_hz`, no spectrum — the diffusion or
    ///   `SPECTRAL_COLLAPSED` case) applies a one-pole low-pass at that
    ///   corner, scaled by the path's broadband gain;
    /// * a **flat** path (no spectrum, no finite corner) reduces to a
    ///   single-tap delta scaled by its gain — reproducing the classic
    ///   broadband behavior exactly and for free.
    ///
    /// Each kernel is **minimum-phase** (zero algorithmic latency), so a
    /// path's time remains purely its physical `excess` delay; the filter
    /// merely colours it. Returns `(excess_delay, kernel)` per non-direct
    /// path, direct-first order preserved. Both `run_acoustic` and the
    /// golden oracles share this function, so a rendered room and its
    /// expected curve always agree.
    pub fn spectral_taps(&self, obj: &BakedObject, ir_len: usize) -> Vec<(i64, Vec<f32>)> {
        spectral_taps_with(obj, ir_len, self.air_absorption)
    }
}

/// Free-function form of [`BakedScene::spectral_taps`], usable with a bare
/// [`BakedObject`] (the executor, tests) without holding a scene. Uses the
/// **disabled** air-absorption model, so the result is bit-identical to the
/// classic per-path kernels until a scene opt-in via
/// [`BakedScene::set_air_absorption`].
pub fn spectral_taps(obj: &BakedObject, ir_len: usize) -> Vec<(i64, Vec<f32>)> {
    spectral_taps_with(obj, ir_len, AirAbsorption::default())
}

/// Per-path spectral taps threaded with the scene's air-absorption model
/// (v3.48): each non-direct kernel is additionally shaped by the
/// distance-dependent HF roll-off `1 / √(1 + (f/f_air)²)` where `f_air =
/// [`AirAbsorption::cutoff_hz`]`(path.distance)` — so a farther reflection
/// darkens (loses highs) exactly as the model intends, while staying
/// equal at DC. With a disabled model this is the classic kernel verbatim.
pub fn spectral_taps_with(
    obj: &BakedObject,
    ir_len: usize,
    air: AirAbsorption,
) -> Vec<(i64, Vec<f32>)> {
    let direct_delay = obj.direct().map(|d| d.delay_samples).unwrap_or(0.0);
    obj.paths
        .iter()
        .filter(|p| p.kind != PathKind::Direct)
        .map(|p| {
            let excess = (p.delay_samples - direct_delay).max(0.0).round() as i64;
            (
                excess,
                path_filter_kernel_with(p, obj.sample_rate, ir_len, air),
            )
        })
        .collect()
}

/// Build the minimum-phase spectral filter kernel for one non-direct path
/// (see [`BakedScene::spectral_taps`]), using the **disabled** air-absorption
/// model (bit-exact classic kernels). A flat path yields a one-tap gain
/// delta.
pub fn path_filter_kernel(p: &BakedPath, sample_rate: f32, ir_len: usize) -> Vec<f32> {
    path_filter_kernel_with(p, sample_rate, ir_len, AirAbsorption::default())
}

/// [`path_filter_kernel`] threaded with the scene's air-absorption model
/// (v3.48): `air` composes a per-path distance-dependent HF roll-off onto the
/// kernel's magnitude before the minimum-phase render, so a farther
/// reflection darkens. A disabled model reproduces `path_filter_kernel`
/// exactly.
pub fn path_filter_kernel_with(
    p: &BakedPath,
    sample_rate: f32,
    ir_len: usize,
    air: AirAbsorption,
) -> Vec<f32> {
    let nyq = (sample_rate * 0.5).max(1.0);
    // Air absorption as a per-path HF roll-off from the path's travelled
    // distance. Disabled ⇒ factor 1 (excludes air entirely, bit-exact); a
    // corner at/above Nyquist is also skipped so nothing spurious is added.
    let f_air = if air.enabled {
        air.cutoff_hz(p.distance, sample_rate)
    } else {
        nyq
    };
    let air_on = air.enabled && f_air < nyq * 0.999;
    let air_shape = move |f: f32| -> f32 {
        if air_on {
            1.0 / (1.0 + (f / f_air).powi(2)).sqrt()
        } else {
            1.0
        }
    };
    // A reflection carrying the full per-band material spectrum shapes
    // directly (its collapsed `gain` was derived from the same spectrum —
    // no double counting), air-composed.
    if p.kind == PathKind::Reflected {
        if let Some(sp) = &p.spectrum {
            return render_magnitude_shape(
                &|f| sp.reflectivity_at_hz(f) * air_shape(f),
                ir_len,
                sample_rate,
            );
        }
    }
    // A path collapsed to a corner only (diffraction / transmission / a
    // `SPECTRAL_COLLAPSED` flag): one-pole low-pass at the corner, scaled
    // by the path broadband gain, air-composed.
    if p.lowpass_hz.is_finite() && p.lowpass_hz > 1.0 && p.lowpass_hz < nyq * 0.999 {
        let fc = p.lowpass_hz;
        let g = p.gain;
        return render_magnitude_shape(
            &|f| (g / (1.0 + (f / fc).powi(2)).sqrt()) * air_shape(f),
            ir_len,
            sample_rate,
        );
    }
    // Truly flat: a pure gain tap at DC — with air enabled this becomes a
    // distance-darkened low-pass; otherwise the exact one-tap delta.
    if air_on {
        let g = p.gain;
        render_magnitude_shape(&|f| g * air_shape(f), ir_len, sample_rate)
    } else {
        vec![p.gain]
    }
}

/// Synthesize the minimum-phase FIR whose magnitude follows `shape`
/// (linear) across the Nyquist half-spectrum, truncated to drop its
/// decaying zero tail. A constant shape collapses to a near-impulse.
fn render_magnitude_shape(shape: &dyn Fn(f32) -> f32, ir_len: usize, sample_rate: f32) -> Vec<f32> {
    let n = ir_len.max(16).next_power_of_two();
    let half = n / 2 + 1;
    let nyq = sample_rate * 0.5;
    let mut mag_db = Vec::with_capacity(half);
    for j in 0..half {
        let f = j as f32 / (half - 1) as f32 * nyq;
        let m = shape(f).max(0.0);
        let db = if m <= 1e-9 { -120.0 } else { 20.0 * m.log10() };
        mag_db.push(db as f64);
    }
    let rendered = crate::dsp::correction::phase::render_from_magnitude_db(
        &mag_db,
        &crate::dsp::correction::phase::RenderParams {
            sample_rate: sample_rate as f64,
            ir_len_samples: n,
            phase_mode: crate::dsp::correction::phase::PhaseMode::Minimum,
            ..Default::default()
        },
    )
    .expect("path magnitude shape renders");
    let s = &rendered.samples;
    let max_abs = s.iter().fold(0.0f64, |m, &v| m.max(v.abs()));
    let mut last = 0usize;
    for (i, &v) in s.iter().enumerate() {
        if v.abs() > 1e-5 * max_abs.max(1e-12) {
            last = i;
        }
    }
    s[..=last.min(s.len() - 1)]
        .iter()
        .map(|&v| v as f32)
        .collect()
}

/// A bake policy controlling which path kinds are retained (a host may bake
/// only what it renders — e.g. reflections for a panner, transmission for a
/// networked room).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
///
/// Serde (v3.37): the scene serializes **deterministically** — the response
/// map is a [`BTreeMap`] so iteration order is stable, and the solver world
/// is skipped (`#[serde(skip)]` — rebuilds need it, rendering never does),
/// keeping a logged scene to the flat response data it renders from. This
/// is what lets aelog embed an animated world's scene snapshots and hash
/// them like any other command.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BakedScene {
    /// Listener position used for all bakes.
    pub listener: Vec3,
    /// Cache cell size (metres).
    pub cell: f32,
    /// The position → response map. A `BTreeMap` so serialization (and
    /// therefore aelog hashing) is order-stable; the serde adapter emits it
    /// as an ordered entry list (JSON object keys must be strings, and a
    /// cell tuple is not one).
    #[serde(with = "baked_cache_serde")]
    cache: std::collections::BTreeMap<(i32, i32, i32), BakedObject>,
    /// Solver state snapshot so lookups can rebuild missing cells from the
    /// same world later. Public so hosts can seed a scene from an
    /// [`AcousticBaker`]'s world and accumulate cells incrementally. Not
    /// serialized (solver internals are out of scope for a render log).
    #[serde(skip)]
    pub world: Option<AcousticWorld>,
    sample_rate: f32,
    /// The bake policy retained for rebuilds.
    policy: BakePolicy,
    /// Scene-scoped air-absorption model (v3.48). When enabled, `spectral_taps`
    /// folds a per-path distance-dependent HF roll-off onto every non-direct
    /// kernel, darkening reflections with travel distance. `#[serde(default)]`:
    /// older baked-scene logs without the field load with the model disabled
    /// (bit-exact).
    #[serde(default)]
    air_absorption: AirAbsorption,
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
            cache: std::collections::BTreeMap::new(),
            world: None,
            sample_rate: 48_000.0,
            policy: BakePolicy::default(),
            air_absorption: AirAbsorption::default(),
        }
    }

    /// Attach the scene-scoped air-absorption model (v3.48). Reflections are
    /// then rendered (by the `Acoustic` node and any `spectral_taps` consumer)
    /// with a per-path distance-dependent HF roll-off — farther paths darken.
    /// The default (disabled) model keeps every kernel bit-identical.
    pub fn set_air_absorption(&mut self, air: AirAbsorption) -> &mut Self {
        self.air_absorption = air;
        self
    }

    /// The scene's air-absorption model.
    pub fn air_absorption(&self) -> AirAbsorption {
        self.air_absorption
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
    ///
    /// **Spectral fork (v3.47):** each tap also carries the surface's
    /// low-pass corner — from its full per-band material spectrum (via
    /// `surface_lowpass_hz`) when one is present, else its collapsed
    /// diffraction/transmission low-pass corner, else ∞ (flat). The realtime
    /// renderers realise that corner as a one-pole per-image low-pass, the
    /// same spectral model the offline `Acoustic` node applies exactly.
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
            // A surface whose derived corner sits at (or above) Nyquist is
            // spectrally flat — collapse it to ∞ so the realtime filter stays
            // a strict passthrough (bit-identical to the pre-v3.47 path);
            // only genuinely damped surfaces get a real corner.
            let nyq = obj.sample_rate * 0.5;
            let corner_for = |hz: f32| {
                if hz.is_finite() && hz > 1.0 && hz < nyq * 0.999 {
                    hz
                } else {
                    f32::INFINITY
                }
            };
            let lowpass_hz = if let Some(sp) = &p.spectrum {
                corner_for(surface_lowpass_hz(sp, obj.sample_rate))
            } else {
                corner_for(p.lowpass_hz)
            };
            out[count] = ListenerImage {
                dir: p.direction,
                dist: p.distance,
                coeff: p.gain,
                delay: (p.delay_samples - direct_delay).max(0.0).round() as u32,
                lowpass_hz,
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

/// Serde adapter for a low-pass corner in Hz. `f32::INFINITY` means "no
/// filter" and is the default on every direct/unfiltered path, but JSON
/// cannot represent non-finite floats — so it round-trips as the sentinel
/// `-1.0` (a corner can never legitimately be negative).
mod hz_serde {
    use serde::{Deserialize, Serialize};

    const INF_SENTINEL: f32 = -1.0;

    pub fn serialize<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
        let out = if v.is_finite() { *v } else { INF_SENTINEL };
        out.serialize(s)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
        let v = f32::deserialize(d)?;
        Ok(if v == INF_SENTINEL { f32::INFINITY } else { v })
    }
}

/// Serde adapter for the cell cache: a `BTreeMap<(i32, i32, i32), …>`
/// cannot be a JSON object (non-string keys), so it serializes as an
/// **ordered entry list** — `BTreeMap` iteration keeps it deterministic,
/// which is what aelog hashing relies on.
mod baked_cache_serde {
    use super::BakedObject;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    /// The cell → response map. Named so the adapter signatures stay
    /// readable (clippy's type-complexity bar).
    type CacheMap = BTreeMap<(i32, i32, i32), BakedObject>;

    pub fn serialize<S: Serializer>(m: &CacheMap, s: S) -> Result<S::Ok, S::Error> {
        let entries: Vec<((i32, i32, i32), BakedObject)> =
            m.iter().map(|(k, o)| (*k, o.clone())).collect();
        entries.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<CacheMap, D::Error> {
        let entries = Vec::<((i32, i32, i32), BakedObject)>::deserialize(d)?;
        Ok(entries.into_iter().collect())
    }
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
    fn listener_images_carry_the_surface_lowpass_corner() {
        // v3.47: the default flat reflective room yields spectrally flat
        // taps (∞); a Fabric MinX wall yields at least one reflection with a
        // finite low-pass corner below Nyquist — the data the realtime
        // renderers realise as a per-image one-pole.
        let mut scene = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        scene.world = Some(world());
        let flat = scene
            .bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default())
            .clone();
        let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
        let n = scene.listener_images(&flat, &mut imgs);
        assert!(n >= 1);
        for img in imgs[..n].iter() {
            assert!(
                img.lowpass_hz.is_infinite(),
                "flat room must yield a flat corner"
            );
        }

        let mut froom = AcousticRoom::default();
        froom.walls[wall_index(crate::spatial::Wall::MinX)] = MaterialKind::Fabric.spectrum();
        let mut fscene = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        fscene.world = Some(AcousticWorld::new(froom, FS));
        let fab = fscene
            .bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default())
            .clone();
        let n2 = fscene.listener_images(&fab, &mut imgs);
        let has_corner = imgs[..n2]
            .iter()
            .any(|i| i.lowpass_hz.is_finite() && i.lowpass_hz < FS * 0.5);
        assert!(has_corner, "fabric wall colours at least one reflection");
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

    #[test]
    fn air_absorption_darkens_far_paths_more_than_near() {
        // v3.48: with the scene's air-absorption model enabled, an otherwise
        // identical reflection travels farther → a darker (stronger HF
        // roll-off) spectral kernel, while DC gain is unchanged (air is
        // transparent at DC).
        use crate::spatial::level::AirAbsorption;
        let path = |distance: f32| BakedPath {
            kind: PathKind::Reflected,
            direction: Vec3::Y,
            distance,
            delay_samples: distance / 343.0 * FS,
            gain: 0.8,
            lowpass_hz: f32::INFINITY,
            interacting: None,
            spectrum: None,
        };
        let air = AirAbsorption {
            enabled: true,
            ..Default::default()
        };
        let near = path_filter_kernel_with(&path(3.0), FS, ACOUSTIC_IR_LEN, air);
        let far = path_filter_kernel_with(&path(12.0), FS, ACOUSTIC_IR_LEN, air);
        // Both are real low-pass kernels now (air turns even a flat path
        // into a distance-darkened filter).
        assert!(near.len() > 1 && far.len() > 1);
        let hf_ratio = |k: &[f32]| -> f32 {
            let dc: f32 = k.iter().sum();
            let nyq: f32 = k
                .iter()
                .enumerate()
                .map(|(i, &v)| v * if i % 2 == 0 { 1.0 } else { -1.0 })
                .sum();
            (nyq.abs() / dc.abs().max(1e-12)).max(0.0)
        };
        assert!(
            hf_ratio(&far) < hf_ratio(&near) * 0.85,
            "far path darker (near {} far {})",
            hf_ratio(&near),
            hf_ratio(&far)
        );
        // Disabled model: the same flat path is the exact one-tap gain delta
        // (bit-exact classic behavior; free `spectral_taps` is unchanged).
        let off = path_filter_kernel(&path(12.0), FS, ACOUSTIC_IR_LEN);
        assert_eq!(off, vec![0.8], "disabled air keeps flat path exact");
    }

    #[test]
    fn serialized_scene_round_trips_its_air_model() {
        use crate::spatial::level::AirAbsorption;
        let air = AirAbsorption {
            enabled: true,
            per_meter: 0.1,
            base_cutoff_hz: 16_000.0,
        };
        let mut scene = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        scene.set_air_absorption(air);
        // A baked response so the serialize is non-trivial (paths incl.).
        scene.world = Some(world());
        scene.bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default());
        let json = serde_json::to_string(&scene).expect("serialize");
        let back: BakedScene = serde_json::from_str(&json).expect("deserialize");
        let a = back.air_absorption();
        assert!(a.enabled && (a.per_meter - 0.1).abs() < 1e-9);
        assert!((a.base_cutoff_hz - 16_000.0).abs() < 1e-3);
        assert_eq!(back.len(), 1, "cells round-trip");
        // Legacy logs lacking the field load with the model disabled.
        let legacy = serde_json::from_str::<BakedScene>(
            &serde_json::to_string(&BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M)).unwrap(),
        )
        .unwrap();
        assert!(!legacy.air_absorption().enabled, "old scene stays flat");
    }

    #[test]
    fn spectral_taps_render_per_path_filters() {
        use crate::spatial::acoustic::geometry::AcousticRoom;
        use crate::spatial::acoustic::material::MaterialKind;
        use crate::spatial::acoustic::solver::wall_index;

        // A flat room: every reflection's kernel must collapse to a single
        // gain tap (the classic broadband behavior, reproduced exactly).
        let mut room = AcousticRoom::default();
        let world = AcousticWorld::new(room, FS);
        let mut s_flat = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        s_flat.world = Some(world);
        let obj_flat = s_flat
            .bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default())
            .clone();
        for (excess, kern) in spectral_taps(&obj_flat, ACOUSTIC_IR_LEN) {
            assert_eq!(
                kern.len(),
                1,
                "flat path ({excess}) must be a single gain tap"
            );
        }

        // Fabric MinX wall: the MinX reflection is spectrally coloured — a
        // multi-tap minimum-phase low-pass kernel (its magnitude rolls off
        // toward Nyquist), rather than a collapsed broadband gain.
        room.walls[wall_index(crate::spatial::Wall::MinX)] = MaterialKind::Fabric.spectrum();
        let mut s_fab = BakedScene::new(Vec3::ZERO, DEFAULT_BAKE_CELL_M);
        s_fab.world = Some(AcousticWorld::new(room, FS));
        let obj_fab = s_fab
            .bake(Vec3::new(1.0, 2.0, 1.5), FS, BakePolicy::default())
            .clone();
        let taps_fab = spectral_taps(&obj_fab, ACOUSTIC_IR_LEN);
        assert!(
            taps_fab.iter().any(|(_, k)| k.len() > 1),
            "at least one fabric reflection is a real filter"
        );
        // The filter really is a low-pass: H(π) = Σ(-1)^k·h[k] is far below
        // H(0) = Σ h[k] for at least one coloured path (fabric's reflectivity
        // falls toward Nyquist).
        let mut saw_lowpass = false;
        for (_, kern) in taps_fab.iter().filter(|(_, k)| k.len() > 1) {
            let dc: f32 = kern.iter().sum();
            let nyq: f32 = kern
                .iter()
                .enumerate()
                .map(|(i, &v)| v * if i % 2 == 0 { 1.0 } else { -1.0 })
                .sum();
            if nyq.abs() < dc.abs() * 0.9 {
                saw_lowpass = true;
            }
        }
        assert!(saw_lowpass, "a fabric reflection is spectrally low-pass");
    }
}
