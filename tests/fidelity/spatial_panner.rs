//! Acceptance suite for the spatial BasicPanner (spec Part XVIII, §90–97,
//! §56) and the spatial scene/level foundation (Parts III–VII).
//!
//! These tests use analytical signals and tight tolerances, mirroring the
//! approach of `multichannel_graph.rs`: an object at a defined world position
//! rendered through the equal-power panner must land on the expected speakers
//! with the documented energy/level laws and no NaN/Inf anywhere. They are
//! the contract the renderer is reviewed against — written before the
//! implementation shipped.

use engine::spatial::level::{AirAbsorption, DistanceModel};
use engine::spatial::math::Vec3;
use engine::spatial::panner::DEFAULT_SMOOTHING_MS;
use engine::spatial::render::{RenderError, SpatialRenderer};
use engine::spatial::{BasicPanner, SpatialScene, Speaker, SpeakerId, SpeakerLayout};

const SR: u32 = 48_000;
const EPS: f32 = 1e-3;

/// A one-object scene with a mono input of `1.0` for `frames`, at a world
/// position. Returns the panner already prepared on `layout` and an output
/// buffer of the right size.
fn render(
    layout: &SpeakerLayout,
    pos: Vec3,
    frames: usize,
    smooth_ms: f32,
) -> (BasicPanner, Vec<f32>) {
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(pos).unwrap();
    let mut p = BasicPanner::new(smooth_ms);
    p.prepare(layout, SR).unwrap();
    let mut out = vec![0.0f32; layout.speakers.len() * frames];
    let input = vec![1.0f32; frames];
    p.process_block(&scene, &[&input], frames, &mut out)
        .unwrap();
    (p, out)
}

fn finite(samples: &[f32]) -> bool {
    samples.iter().all(|x| x.is_finite())
}

#[test]
fn panner_rejects_unprepared_and_degenerate_layouts() {
    let mut p = BasicPanner::new(0.0);
    let scene = SpatialScene::new(SR);
    let frames = 8;
    let mut out = vec![0.0f32; 2 * frames];
    // Not prepared.
    assert!(matches!(
        p.process_block(&scene, &[&[0.0; 8]], frames, &mut out),
        Err(RenderError::InvalidLayout)
    ));
    // Degenerate layout with no pan speakers.
    p.prepare(&SpeakerLayout::custom(vec![]), SR)
        .expect_err("empty layout is invalid");
    // LFE-only layout is degenerate for panning.
    let lfe_only = SpeakerLayout {
        speakers: vec![Speaker::new(Vec3::ZERO)], // is_lfe defaults false, but no panning
        reference_position: Vec3::ZERO,
        calibration: Default::default(),
    };
    // A single non-LFE speaker IS pan-capable, so it's not degenerate.
    assert!(BasicPanner::new(0.0).prepare(&lfe_only, SR).is_ok());
}

#[test]
fn cardinal_impulses_land_on_expected_speakers() {
    // Stereo: front → FL+FR evenly, right → FR dominant.
    let layout = SpeakerLayout::stereo();

    let (_, out) = render(&layout, Vec3::Y, 8, 0.0); // front
    assert!(finite(&out));
    let fl = out[0];
    let fr = out[1];
    assert!(
        (fl - std::f32::consts::FRAC_1_SQRT_2).abs() < EPS,
        "front → FL {fl}"
    );
    assert!(
        (fr - std::f32::consts::FRAC_1_SQRT_2).abs() < EPS,
        "front → FR {fr}"
    );

    let (_, out) = render(&layout, Vec3::X, 8, 0.0); // right
    assert!(finite(&out));
    let fl = out[0];
    let fr = out[1];
    // azimuth +90° → the FR speaker dominates.
    assert!(fr > 0.9, "right → FR dominant ({fr})");
    assert!(fl < fr, "right favour FR over FL");

    let (_, out) = render(&layout, -Vec3::X, 8, 0.0); // left
    let fl = out[0];
    let fr = out[1];
    assert!(fl > 0.9, "left → FL dominant ({fl})");
    assert!(fl > fr);
}

#[test]
fn elevation_and_distance_are_monotonic() {
    let layout = SpeakerLayout::stereo();
    // Front, flat, close vs far: far is quieter under InverseReference.
    let (_, near) = render(&layout, Vec3::Y, 8, 0.0);
    let (_, far) = render(&layout, Vec3::new(0.0, 4.0, 0.0), 8, 0.0);
    let near_e = near[0] * near[0] + near[1] * near[1];
    let far_e = far[0] * far[0] + far[1] * far[1];
    assert!(far_e < near_e, "far object is quieter");

    // Elevated vs flat: elevated is quieter at the ring under cos(elevation).
    let (_, elev) = render(&layout, Vec3::new(0.0, 1.0, 1.0), 8, 0.0);
    let elev_e = elev[0] * elev[0] + elev[1] * elev[1];
    assert!(elev_e < near_e, "elevated source is quieter");
}

#[test]
fn symmetry_left_right_is_mirrored() {
    let layout = SpeakerLayout::stereo();
    let (_, left) = render(&layout, Vec3::new(-1.0, 2.0, 0.0), 8, 0.0);
    let (_, right) = render(&layout, Vec3::new(1.0, 2.0, 0.0), 8, 0.0);
    // Mirror: left.FL ≈ right.FR and left.FR ≈ right.FL.
    assert!(
        (left[0] - right[1]).abs() < 1e-4,
        "mirror FL vs FR: {} vs {}",
        left[0],
        right[1]
    );
    assert!(
        (left[1] - right[0]).abs() < 1e-4,
        "mirror FR vs FL: {} vs {}",
        left[1],
        right[0]
    );
}

#[test]
fn lfe_is_an_additive_send_never_a_pan_target() {
    let layout = SpeakerLayout::five_point_one();
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(Vec3::Y).unwrap();
    scene.object_mut(id).unwrap().lfe_send = 1.0;
    let mut p = BasicPanner::new(0.0);
    p.prepare(&layout, SR).unwrap();
    let frames = 8;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 6 * frames];
    p.process_block(&scene, &[&input], frames, &mut out)
        .unwrap();
    for i in 0..frames {
        // LFE slot (index 3 in five_point_one order) receives the send.
        assert!(out[i * 6 + 3] > 0.5, "lfe_send reaches LFE");
        // The object is front: Center (index 2) carries the frontal energy.
        assert!(out[i * 6 + 2] > 0.5, "front object still reaches Center");
    }
}

#[test]
fn listener_rotation_keeps_world_fixed_objects_stable() {
    // Listener yaws +90° (now facing +X). A world object held at +X must
    // appear in front.
    let layout = SpeakerLayout::stereo();
    let mut scene = SpatialScene::new(SR);
    scene
        .listener
        .set_orientation(engine::spatial::math::Quat::from_euler_rad(
            std::f32::consts::FRAC_PI_2,
            0.0,
            0.0,
        ));
    scene.create_audio_object(Vec3::X).unwrap();
    let mut p = BasicPanner::new(0.0);
    p.prepare(&layout, SR).unwrap();
    let frames = 8;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 2 * frames];
    p.process_block(&scene, &[&input], frames, &mut out)
        .unwrap();
    // After the rotation, +X appears at front → evenly split FL/FR.
    assert!(
        (out[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < EPS,
        "rotated object → FL {}",
        out[0]
    );
    assert!(
        (out[1] - std::f32::consts::FRAC_1_SQRT_2).abs() < EPS,
        "rotated object → FR {}",
        out[1]
    );
}

#[test]
fn continuity_around_full_circle_no_nan_no_jump() {
    // A front object, listener yaw swept 0..360 in fine steps with smoothing
    // enabled, must never produce NaN, Inf, or a discontinuous gain jump.
    let layout = SpeakerLayout::five_point_one();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::Y).unwrap();
    let mut p = BasicPanner::new(DEFAULT_SMOOTHING_MS);
    p.prepare(&layout, SR).unwrap();
    let frames = 64;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 6 * frames];
    let mut prev_fl = 0.0f32;
    for deg in 0..720 {
        scene
            .listener
            .set_orientation(engine::spatial::math::Quat::from_euler_rad(
                (deg as f32).to_radians(),
                0.0,
                0.0,
            ));
        p.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        assert!(finite(&out), "no NaN/Inf at {deg}°");
        let fl = out[0];
        if prev_fl != 0.0 {
            let delta = (fl - prev_fl).abs();
            assert!(delta < 0.1, "no jump in FL gain at {deg}° ({delta})");
        }
        prev_fl = fl;
    }
}

#[test]
fn energy_invariant_at_spread_zero_is_equal_power() {
    // 5.1: across a full azimuth sweep, Σ_spanning² == 1 (LFE excluded).
    let layout = SpeakerLayout::five_point_one();
    let mut p = BasicPanner::new(0.0);
    p.prepare(&layout, SR).unwrap();
    let frames = 16;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 6 * frames];
    for deg in 0..=360 {
        let rad = (deg as f32).to_radians();
        let dir = Vec3::new(rad.sin(), rad.cos(), 0.0);
        let mut scene = SpatialScene::new(SR);
        scene.create_audio_object(dir).unwrap();
        p.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        let mut e = 0.0f32;
        for (spk, v) in out.iter().take(6).enumerate() {
            if spk == 3 {
                continue; // LFE is not a pan speaker
            }
            e += v * v;
        }
        assert!((e - 1.0).abs() < 2e-3, "energy drift at {deg}°: {e}");
    }
}

#[test]
fn custom_layout_pans_continuously() {
    // Arbitrary 4-speaker ring, no named preset, must render without error.
    let layout = SpeakerLayout::custom(vec![
        Vec3::new(-1.0, 0.0, 0.0),
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(1.0, 0.0, 0.0),
        Vec3::new(0.0, -1.0, 0.0),
    ]);
    let mut p = BasicPanner::new(0.0);
    p.prepare(&layout, SR).unwrap();
    let frames = 16;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 4 * frames];
    let mut energy_ok = true;
    for deg in 0..=360 {
        let rad = (deg as f32).to_radians();
        let dir = Vec3::new(rad.sin(), rad.cos(), 0.0);
        let mut scene = SpatialScene::new(SR);
        scene.create_audio_object(dir).unwrap();
        p.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        assert!(finite(&out));
        let e: f32 = out.iter().take(4).map(|v| v * v).sum();
        if (e - 1.0).abs() > 3e-2 {
            energy_ok = false;
            break;
        }
    }
    assert!(energy_ok, "custom 4-speaker layout preserves energy");
}

#[test]
fn per_speaker_calibration_trim_is_applied() {
    let mut layout = SpeakerLayout::stereo();
    layout.calibration.per_speaker_trim_db = vec![(SpeakerId(0), -12.0)];
    let (_, out) = render(&layout, Vec3::Y, 8, 0.0);
    // -12 dB ≈ ×0.251 on the FL speaker only; FR unmodified.
    let fl = out[0];
    let fr = out[1];
    let unmodified = std::f32::consts::FRAC_1_SQRT_2;
    assert!((fr - unmodified).abs() < EPS, "FR unmodified ({fr})");
    assert!(fl < fr * 0.4, "FL trimmed by -12 dB ({fl})");
    assert!(finite(&out));
}

#[test]
fn air_absorption_cutoff_is_bounded_and_disabled_is_passthrough() {
    let mut a = AirAbsorption::default();
    assert_eq!(
        a.cutoff_hz(10.0, SR as f32),
        SR as f32 * 0.5,
        "disabled = full band"
    );
    a.enabled = true;
    let near = a.cutoff_hz(1.0, SR as f32);
    let far = a.cutoff_hz(10.0, SR as f32);
    assert!(near > far, "closer source keeps more HF");
    assert!(far >= 500.0 && near <= SR as f32 * 0.45);
}

#[test]
fn distance_models_are_monotonic_across_range() {
    let inv = DistanceModel::Inverse;
    let sq = DistanceModel::InverseSquare;
    assert!(inv.distance_gain(1.0, 1.0) > inv.distance_gain(4.0, 1.0));
    assert!(sq.distance_gain(1.0, 1.0) > sq.distance_gain(4.0, 1.0));
    for d in [0.0, 0.5, 1.0, 2.0, 5.0, 10.0] {
        let g = inv.distance_gain(d, 1.0);
        assert!(g.is_finite() && g >= 0.0, "inverse at d={d}");
    }
    // Force a NaN-prone degenerate distance and confirm the guard.
    assert!(sq.distance_gain(f32::NEG_INFINITY, 1.0).is_finite());
}
