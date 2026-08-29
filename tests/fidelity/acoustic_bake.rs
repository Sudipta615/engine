//! Fidelity tests — Acoustic baking (v3.26, roadmap v3.26).
//!
//! Evolution thresholds (`docs/EVOLUTION.md` Phase 24):
//! * a `BakedScene` built from an `AcousticWorld` whose room mirrors the
//!   renderer's `Room` reproduces the **live** room-reflection solve — the
//!   baked panner, VBAP and binaural outputs match the no-bake renders of
//!   the same scene within tight tolerance (identical geometry, the bake is
//!   a cache, not a new model);
//! * the bake is a **position-dependent response cache**: distinct cells
//!   are distinct entries, same-cell re-bakes reuse, and a static source
//!   renders the same taps every block;
//! * objects whose position is *not* baked fall back to the live solve
//!   (no crash, deterministic output);
//! * **frequency-dependent data survives**: a fabric wall bake darkens the
//!   reflected low-pass corner vs a concrete bake, while both stay finite
//!   and in-band;
//! * the whole bake + render path is deterministic and finite.
//!
//! The equivalence is asserted with a small relative tolerance rather than
//! bit-exactness: the baked path converts solver directions via
//! `Vec3::normalized` (divide per component) while the live image path
//! multiplies by a reciprocal — an arithmetic-order difference of ≤ 1 ulp,
//! not a model difference.

use engine::spatial::render::SpatialRenderer;
use engine::spatial::{
    AcousticBaker, AcousticRoom, AcousticWorld, BakePolicy, BasicPanner, BinauralRenderer,
    MaterialKind, MaterialSpectrum, Room, SpatialScene, SpeakerLayout, VbapRenderer, Vec3,
};

const SR: u32 = 48_000;
const FS: f32 = 48_000.0;

/// A 12×10×3 m room matching `Room::default()` geometry; the listener sits
/// at its centre and the object 4 m in front of the left wall.
fn room_config() -> Room {
    Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        rt60_ms: 800.0,
        late_mix: 0.0, // keep the late field out of these reflection checks
        speed_of_sound: 343.0,
    }
}

/// The acoustic-world twin of `room_config()`: same geometry, a flat
/// reflective material whose broadband coefficient equals `1 − absorption`.
fn acoustic_world_for(room: &Room) -> AcousticWorld {
    let ac =
        AcousticRoom::from_render_room(room, MaterialSpectrum::flat_reflective(room.absorption));
    AcousticWorld::new(ac, FS)
}

fn scene_with_impulse_object(room: Room) -> SpatialScene {
    let mut scene = SpatialScene::new(SR);
    scene.listener.set_position(Vec3::new(6.0, 5.0, 1.5));
    let id = scene.create_audio_object(Vec3::new(1.0, 5.0, 1.5)).unwrap();
    scene.object_mut(id).unwrap().room_send = 1.0;
    scene.room = room;
    scene
}

fn impulse_plane(frames: usize) -> Vec<f32> {
    let mut p = vec![0.0f32; frames];
    p[0] = 1.0;
    p
}

fn bake_for(room: &Room, scene: &SpatialScene) -> engine::spatial::BakedScene {
    let baker = AcousticBaker::new(acoustic_world_for(room), 0.5);
    baker.bake_scene(
        std::iter::once(scene.objects.iter_enabled().next().unwrap().1.position),
        scene.listener.position,
        FS,
        BakePolicy::default(),
    )
}

fn render_block<R: SpatialRenderer>(
    r: &mut R,
    scene: &SpatialScene,
    frames: usize,
    ch: usize,
) -> Vec<f32> {
    let mut out = vec![0.0f32; ch * frames];
    let input = impulse_plane(frames);
    r.process_block(scene, &[&input], frames, &mut out).unwrap();
    out
}

/// Relative tolerance for baked-vs-live equivalence (see module docs).
fn assert_close(a: &[f32], b: &[f32], label: &str) {
    assert_eq!(a.len(), b.len(), "{label}: length");
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        let scale = x.abs().max(y.abs()).max(1e-9);
        let rel = (x - y).abs() / scale;
        assert!(rel < 1e-3, "{label}: frame {i} {x} vs {y} (rel {rel})");
    }
}

#[test]
fn baked_panner_matches_live_panner() {
    let layout = SpeakerLayout::five_point_one();
    let scene = scene_with_impulse_object(room_config());
    let frames = 512usize;

    let mut live = BasicPanner::new(0.0);
    live.prepare(&layout, SR).unwrap();
    let live_out = render_block(&mut live, &scene, frames, 6);

    let mut baked = BasicPanner::new(0.0);
    baked.prepare(&layout, SR).unwrap();
    let bs = bake_for(&scene.room, &scene);
    baked.set_baked(Some(bs));
    let baked_out = render_block(&mut baked, &scene, frames, 6);

    assert_close(&baked_out, &live_out, "baked panner vs live panner");
    // The direct impulse and its reflection are both present and finite.
    assert!(baked_out.iter().all(|v| v.is_finite()));
    assert!(
        baked_out[280 * 6 + 4].abs() > 0.05,
        "reflection on SL at 280"
    );
}

#[test]
fn baked_vbap_matches_live_vbap() {
    let layout = SpeakerLayout::five_point_one();
    let scene = scene_with_impulse_object(room_config());
    let frames = 512usize;

    let mut live = VbapRenderer::with_smoothing(0.0);
    live.prepare(&layout, SR).unwrap();
    let live_out = render_block(&mut live, &scene, frames, 6);

    let mut baked = VbapRenderer::with_smoothing(0.0);
    baked.prepare(&layout, SR).unwrap();
    let bs = bake_for(&scene.room, &scene);
    baked.set_baked(Some(bs));
    let baked_out = render_block(&mut baked, &scene, frames, 6);

    assert_close(&baked_out, &live_out, "baked vbap vs live vbap");
}

#[test]
fn baked_binaural_matches_live_binaural() {
    let layout = SpeakerLayout::stereo();
    let scene = scene_with_impulse_object(room_config());
    let frames = 512usize;

    let mut live = BinauralRenderer::new(0.0);
    live.prepare(&layout, SR).unwrap();
    let live_out = render_block(&mut live, &scene, frames, 2);

    let mut baked = BinauralRenderer::new(0.0);
    baked.prepare(&layout, SR).unwrap();
    let bs = bake_for(&scene.room, &scene);
    baked.set_baked(Some(bs));
    let baked_out = render_block(&mut baked, &scene, frames, 2);

    assert_close(&baked_out, &live_out, "baked binaural vs live binaural");
}

#[test]
fn bake_cache_is_position_keyed() {
    let room = room_config();
    let baker = AcousticBaker::new(acoustic_world_for(&room), 0.5);
    // Two distinct cells.
    let mut scene = TestScene::new();
    scene.bake_at(&baker, Vec3::new(1.0, 5.0, 1.5), FS, BakePolicy::default());
    scene.bake_at(&baker, Vec3::new(4.0, 5.0, 1.5), FS, BakePolicy::default());
    assert_eq!(scene.len(), 2, "distinct cells");
    // Same-cell re-bake reuses the entry (still 2).
    scene.bake_at(&baker, Vec3::new(1.1, 5.0, 1.5), FS, BakePolicy::default());
    assert_eq!(scene.len(), 2, "same cell reuses");
    // A source outside the baked cells is absent.
    assert!(scene.get(Vec3::new(9.0, 9.0, 9.0)).is_none());
}

#[test]
fn unbaked_object_falls_back_to_live_solve() {
    // A renderer with a bake that does NOT cover the object's position must
    // fall back to the live image-source solve and still render.
    let layout = SpeakerLayout::five_point_one();
    let room = room_config();
    let mut scene = SpatialScene::new(SR);
    scene.listener.set_position(Vec3::new(6.0, 5.0, 1.5));
    let id = scene.create_audio_object(Vec3::new(1.0, 5.0, 1.5)).unwrap(); // 5 m away — not baked
    scene.object_mut(id).unwrap().room_send = 1.0;
    scene.room = room.clone();

    // Bake for a *different* cell (a far corner).
    let baker = AcousticBaker::new(acoustic_world_for(&room), 0.5);
    let bs = baker.bake_scene(
        std::iter::once(Vec3::new(11.0, 9.0, 2.5)),
        scene.listener.position,
        FS,
        BakePolicy::default(),
    );

    let mut p = BasicPanner::new(0.0);
    p.prepare(&layout, SR).unwrap();
    p.set_baked(Some(bs));
    let frames = 512usize;
    let out = render_block(&mut p, &scene, frames, 6);
    assert!(out.iter().all(|v| v.is_finite()));
    // The direct sound (1/5 distance ≈ 0.2) reached some speaker.
    assert!(out.iter().any(|v| v.abs() > 0.05));
}

#[test]
fn fabric_bake_darkens_reflection_lowpass() {
    let room = room_config();
    // Concrete world (default flat 0.2) vs a fabric left-wall world.
    // The AcousticWorld walls carry the material below.
    let concrete_world = acoustic_world_for(&room);
    let mut fabric_ac =
        AcousticRoom::from_render_room(&room, MaterialSpectrum::flat_reflective(room.absorption));
    fabric_ac.walls[engine::spatial::wall_index(engine::spatial::Wall::MinX)] =
        MaterialKind::Fabric.spectrum();
    let fabric_world = AcousticWorld::new(fabric_ac, FS);

    let src = Vec3::new(1.0, 5.0, 1.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let b_c =
        AcousticBaker::new(concrete_world, 0.5).bake_single(src, lst, FS, BakePolicy::default());
    let b_f =
        AcousticBaker::new(fabric_world, 0.5).bake_single(src, lst, FS, BakePolicy::default());
    let obj_c = b_c.get(src).unwrap();
    let obj_f = b_f.get(src).unwrap();

    let floor_lp = |obj: &engine::spatial::BakedObject| {
        obj.paths
            .iter()
            .filter(|p| p.kind == engine::spatial::PathKind::Reflected)
            .fold(f32::INFINITY, |m, p| m.min(p.lowpass_hz))
    };
    let cp = floor_lp(obj_c);
    let fp = floor_lp(obj_f);
    assert!(
        fp < cp,
        "fabric darkens at least one reflection ({fp} < {cp} expected)"
    );
    assert!(fp > 20.0 && fp < 24_000.0, "in-band corner {fp}");
    // Frequency-domain data survives on the baked reflections.
    assert!(obj_f
        .paths
        .iter()
        .find(|p| p.kind == engine::spatial::PathKind::Reflected)
        .unwrap()
        .spectrum
        .is_some());
}

#[test]
fn bake_and_render_is_deterministic() {
    let room = room_config();
    let scene = scene_with_impulse_object(room.clone());
    let layout = SpeakerLayout::five_point_one();
    let frames = 256usize;
    let render = |baker: &AcousticBaker| -> Vec<f32> {
        let bs = baker.bake_single(
            scene.objects.iter_enabled().next().unwrap().1.position,
            scene.listener.position,
            FS,
            BakePolicy::default(),
        );
        let mut p = BasicPanner::new(0.0);
        p.prepare(&layout, SR).unwrap();
        p.set_baked(Some(bs));
        render_block(&mut p, &scene, frames, 6)
    };
    let a = render(&AcousticBaker::new(acoustic_world_for(&room), 0.5));
    let b = render(&AcousticBaker::new(acoustic_world_for(&room), 0.5));
    assert_eq!(a, b, "deterministic baked render");
}

// ── test-only helpers ────────────────────────────────────────────────────────

struct TestScene(engine::spatial::BakedScene);

impl TestScene {
    fn new() -> Self {
        TestScene(engine::spatial::BakedScene::new(Vec3::ZERO, 0.5))
    }

    fn bake_at(
        &mut self,
        baker: &AcousticBaker,
        source: Vec3,
        sample_rate: f32,
        policy: BakePolicy,
    ) {
        // Adopt the baker's world, then accumulate a new cell (keeps
        // previously baked cells intact — this is the cache-under-test).
        self.0.world = Some(baker.world);
        self.0.bake(source, sample_rate, policy);
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn get(&self, source: Vec3) -> Option<&engine::spatial::BakedObject> {
        self.0.get(source)
    }
}
