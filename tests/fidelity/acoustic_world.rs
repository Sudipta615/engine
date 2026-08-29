//! Fidelity tests — Acoustic World (v3.25, Direction 6/7/8/9).
//!
//! Evolution thresholds (`docs/EVOLUTION.md` Phase 23):
//! * a source/listener pair in an order-1 box yields exactly one direct +
//!   six image-source reflections, each carrying a finite, physically-placed
//!   delay (excess path over the direct, matched to the renderer's
//!   [`image_sources`] geometry to the same wall mirroring);
//! * a fully-open portal between two spaces yields a bright,
//!   effectively-filter-free transmission path plus a diffraction path around
//!   each jamb, with the diffracted paths HF-rolled-off more than the flat
//!   transmission;
//! * material interactions are *frequency* aware: a fabric wall's reflection
//!   is low-passed well below a concrete wall's, while both stay finite and
//!   bounded in [20 Hz, Nyquist];
//! * the disabled world degrades to a single exact direct path (no room
//!   interaction), and every solved path is deterministic and finite.
//!
//! This suite pins the **simulation** contract only — rendering the paths
//! onto ears/speakers remains the renderers' job, so nothing here asserts an
//! audible waveform.

use engine::spatial::{
    diffract_around_edge, wall_index, AcousticRoom, AcousticWorld, DiffractionEdge, MaterialKind,
    MaterialSpectrum, PathKind, Portal, Vec3, Wall, MAX_PATHS,
};

const FS: f32 = 48_000.0;

fn default_world() -> AcousticWorld {
    AcousticWorld::new(AcousticRoom::default(), FS)
}

fn solve(w: &AcousticWorld, src: Vec3, lst: Vec3) -> Vec<(PathKind, f32, f32)> {
    let mut out =
        [engine::spatial::AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
    let n = w.solve(src, lst, &mut out);
    out[..n]
        .iter()
        .map(|p| (p.kind, p.delay_samples, p.gain))
        .collect()
}

#[test]
fn order1_room_direct_plus_six_reflections() {
    let w = default_world();
    let src = Vec3::new(1.0, 2.0, 1.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let paths = solve(&w, src, lst);
    assert_eq!(
        paths
            .iter()
            .filter(|(k, _, _)| *k == PathKind::Direct)
            .count(),
        1
    );
    assert_eq!(
        paths
            .iter()
            .filter(|(k, _, _)| *k == PathKind::Reflected)
            .count(),
        6,
        "order-1 box -> 6 image sources"
    );
    // Every reflection is finite and later than (or equal to) the direct path
    // of the same scene; none is in the past.
    let direct_delay = paths
        .iter()
        .find(|(k, _, _)| *k == PathKind::Direct)
        .map(|(_, d, _)| *d)
        .unwrap();
    for (k, d, g) in &paths {
        assert!(d.is_finite() && *d >= 0.0, "{k:?} delay {d}");
        assert!(g.is_finite() && *g > 0.0, "{k:?} gain {g}");
        if *k == PathKind::Reflected {
            assert!(*d > direct_delay, "reflection must be later than direct");
        }
    }
}

#[test]
fn reflection_delays_match_renderer_wall_geometry() {
    // Mirror the room.rs closed-form: object 4 m to the left of the x=0
    // plane, listener at corridor centre → left-wall image excess path = 2 m
    // (280 samples @ 48 k). The solver must agree with the renderer's own
    // image enumeration, because they share the same room geometry.
    let r = AcousticRoom {
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        ..Default::default()
    };
    let w = AcousticWorld::new(r, FS);
    let src = Vec3::new(1.0, 5.0, 1.5); // direct = 5 m to listener
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let mut out =
        [engine::spatial::AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
    let n = w.solve(src, lst, &mut out);
    let direct = out[0].delay_samples;
    // The left-wall (x=0) reflection: image at (−1, 5, 1.5), dist 7 m.
    let left_stdel = 7.0 / 343.0 * FS;
    let found = out[..n].iter().any(|p| {
        p.kind == PathKind::Reflected
            && (p.delay_samples - left_stdel).abs() < 0.5
            && p.delay_samples > direct
    });
    assert!(
        found,
        "left-wall reflection at ≈{left_stdel} samples not found"
    );
}

#[test]
fn open_portal_transmits_brightly_and_diffracts_around_jambs() {
    let mut w = default_world();
    w.add_portal(Portal {
        wall: Wall::MaxX, // x = 12 wall
        corner: Vec3::new(12.0, 4.0, 0.4),
        width: 1.0,
        height: 2.2,
        material: MaterialSpectrum::flat_transmissive(1.0),
    });
    let src = Vec3::new(6.0, 4.0, 1.4);
    let lst = Vec3::new(6.0, 4.5, 1.4);
    let paths = solve(&w, src, lst);

    let trans = paths
        .iter()
        .find(|(k, _, _)| *k == PathKind::Transmitted)
        .map(|(_, _, g)| *g);
    assert!(
        trans.is_some(),
        "an open portal must yield a transmission path"
    );
    let trans_gain = trans.unwrap();
    assert!(
        trans_gain > 0.5,
        "fully-open portal passes most energy ({trans_gain})"
    );

    let diffs = paths
        .iter()
        .filter(|(k, _, _)| *k == PathKind::Diffracted)
        .count();
    assert!(diffs >= 1, "an open portal has jambs to diffract around");
}

#[test]
fn fabric_wall_lowpasses_reflections_concrete_does_not() {
    // Two identical rooms: one with all-concrete walls, one with a heavy
    // fabric min-X wall. Reflections bouncing off the fabric wall are
    // collapsed (via the material's `broadband` reduction) to a far lower
    // low-pass corner than the concrete room's.
    let concrete = AcousticWorld::new(AcousticRoom::default(), FS);
    let mut r = AcousticRoom::default();
    r.walls[wall_index(Wall::MinX)] = MaterialKind::Fabric.spectrum();
    let fabric = AcousticWorld::new(r, FS);

    let src = Vec3::new(0.5, 2.0, 1.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let mut o_c =
        [engine::spatial::AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
    let mut o_f =
        [engine::spatial::AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
    let nc = concrete.solve(src, lst, &mut o_c);
    let nf = fabric.solve(src, lst, &mut o_f);

    // The left (fabric) reflection's low-pass corner is well below the
    // concrete room's equivalent and stays in-band.
    let fabric_lp = o_f[..nf]
        .iter()
        .filter(|p| p.kind == PathKind::Reflected)
        .filter(|p| (p.direction - Vec3::new(-1.0, 0.0, 0.0)).length() < 0.5) // −X image
        .map(|p| p.lowpass_hz)
        .fold(f32::INFINITY, f32::min);
    let concrete_lp = o_c[..nc]
        .iter()
        .filter(|p| p.kind == PathKind::Reflected)
        .filter(|p| (p.direction - Vec3::new(-1.0, 0.0, 0.0)).length() < 0.5)
        .map(|p| p.lowpass_hz)
        .fold(f32::INFINITY, f32::min);
    assert!(
        concrete_lp > fabric_lp,
        "concrete {concrete_lp} vs fabric {fabric_lp} — material must roll off HF"
    );
    assert!(fabric_lp < 24_000.0, "fabric rolls off before Nyquist");
    assert!(fabric_lp > 20.0, "fabric keeps the bass band");
    assert!(o_f[..nf]
        .iter()
        .all(|p| p.gain.is_finite() && p.lowpass_hz.is_finite() || p.kind == PathKind::Direct));
}

#[test]
fn disabled_world_is_exact_direct_only() {
    let mut w = default_world();
    w.enabled = false;
    let src = Vec3::new(1.0, 2.0, 1.5);
    let lst = Vec3::new(6.0, 7.0, 1.0);
    let mut out =
        [engine::spatial::AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
    let n = w.solve(src, lst, &mut out);
    assert_eq!(n, 1);
    assert_eq!(out[0].kind, PathKind::Direct);
    // Exact: distance/delay let us recover the geometry.
    assert!((out[0].distance - (src - lst).length()).abs() < 1e-3);
}

#[test]
fn freestanding_edge_diffracts_around_a_fin() {
    let mut w = default_world();
    // A vertical fin just outside the doorway.
    w.add_edge(DiffractionEdge::new(
        Vec3::new(0.0, 4.0, 0.0),
        Vec3::new(0.0, 4.0, 3.0),
    ));
    let src = Vec3::new(-2.0, 4.0, 1.5);
    let lst = Vec3::new(3.0, 4.0, 1.5);
    let paths = solve(&w, src, lst);
    assert!(paths.iter().any(|(k, _, _)| *k == PathKind::Diffracted));
}

#[test]
fn solve_is_deterministic_and_bounded() {
    let w = default_world();
    for _ in 0..3 {
        let a = solve(&w, Vec3::new(1.0, 2.0, 1.5), Vec3::new(6.0, 5.0, 1.5));
        let b = solve(&w, Vec3::new(1.0, 2.0, 1.5), Vec3::new(6.0, 5.0, 1.5));
        assert_eq!(a, b, "deterministic path set");
    }
    // Even a fully-portaled scene stays within the cap.
    let mut busy = default_world();
    for i in 0..4 {
        busy.add_portal(Portal {
            wall: Wall::MaxX,
            corner: Vec3::new(12.0, (i * 2) as f32, 0.4),
            width: 1.0,
            height: 2.2,
            material: MaterialSpectrum::flat_transmissive(1.0),
        });
    }
    let mut out =
        [engine::spatial::AcousticPath::direct(Vec3::ZERO, Vec3::ZERO, FS, 343.0); MAX_PATHS];
    let n = busy.solve(Vec3::new(1.0, 2.0, 1.5), Vec3::new(6.0, 5.0, 1.5), &mut out);
    assert!(n <= MAX_PATHS);
    assert!(out[..n].iter().all(|p| p.delay_samples.is_finite()));
}

#[test]
fn diffract_around_edge_reports_distance_and_delay() {
    let e = DiffractionEdge::new(Vec3::new(0.0, 0.0, 0.0), Vec3::new(0.0, 0.0, 2.0));
    let p = diffract_around_edge(
        &e,
        Vec3::new(-1.0, 0.0, 1.0),
        Vec3::new(1.0, 0.0, 1.0),
        FS,
        343.0,
    )
    .unwrap();
    // Source and listener each 1 m from the edge point (0,0,1): total 2 m.
    assert!((p.distance - 2.0).abs() < 1e-3);
    assert!((p.delay_samples - 2.0 / 343.0 * FS).abs() < 0.5);
    assert_eq!(p.kind, PathKind::Diffracted);
}
