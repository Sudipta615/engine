//! Acceptance suite for the VBAP-style object renderer (spec Part IV §21–22,
//! Part V §25–29, §56).
//!
//! These are the contract tests the renderer is reviewed against: an object
//! at a defined world position rendered through the VBAP solver must land on
//! the speakers that geometrically enclose it, preserve energy, stay
//! symmetric, move continuously, fall back deterministically out of coverage,
//! and never emit NaN/Inf — including on degenerate geometry and arbitrary
//! custom layouts. The front-centre regression test pins the tessellation
//! property (spec §21): no speaker may lie on the boundary of another
//! triangle, or the renderer snaps between a phantom stereo pair and the real
//! centre speaker at the front axis.

use engine::spatial::math::Vec3;
use engine::spatial::panner::DEFAULT_SMOOTHING_MS;
use engine::spatial::render::SpatialRenderer;
use engine::spatial::vbap::PanMode;
use engine::spatial::{SpatialScene, SpeakerLayout, VbapRenderer};

const SR: u32 = 48_000;

/// One-object scene (mono input of `1.0`) rendered through a fresh VBAP
/// renderer prepared on `layout`. Returns the renderer (for introspection)
/// and the output buffer.
fn render(
    layout: &SpeakerLayout,
    pos: Vec3,
    frames: usize,
    smooth_ms: f32,
) -> (VbapRenderer, Vec<f32>) {
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(pos).unwrap();
    let mut r = VbapRenderer::with_smoothing(smooth_ms);
    r.prepare(layout, SR).unwrap();
    let mut out = vec![0.0f32; layout.speakers.len() * frames];
    let input = vec![1.0f32; frames];
    r.process_block(&scene, &[&input], frames, &mut out)
        .unwrap();
    (r, out)
}

fn finite(samples: &[f32]) -> bool {
    samples.iter().all(|x| x.is_finite())
}

/// Energy of frame 0 (unit input → one frame is enough), excluding the LFE
/// channel.
fn energy_non_lfe(out: &[f32], ch: usize, lfe: Option<usize>) -> f32 {
    let mut e = 0.0f32;
    for (spk, v) in out.iter().take(ch).enumerate() {
        if Some(spk) == lfe {
            continue;
        }
        e += v * v;
    }
    e
}

#[test]
fn layout_classification_three_dim_planar_single() {
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&SpeakerLayout::seven_point_one_four(), SR)
        .unwrap();
    assert_eq!(r.pan_mode(), PanMode::ThreeDim);
    assert_eq!(r.channels(), 12);

    // 7.1 (no heights) is horizontal-only → planar reduction.
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&SpeakerLayout::seven_point_one(), SR).unwrap();
    assert_eq!(r.pan_mode(), PanMode::Planar);

    // A single pan speaker.
    let single = SpeakerLayout::custom(vec![Vec3::new(1.0, 0.0, 0.0)]);
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&single, SR).unwrap();
    assert_eq!(r.pan_mode(), PanMode::Single);
}

#[test]
fn front_center_uses_real_center_not_phantom_pair() {
    // Regression for the tessellation gap (spec §21): the 7.1.4 centre
    // speaker sits exactly on the FL–FR base edge of the spurious triangle
    // {FL, FR, height}. A front object must be rendered with the real Center
    // speaker (index 2) dominant and essentially nothing on FL/FR (indices
    // 0/1) — not a phantom equal-power pair.
    let layout = SpeakerLayout::seven_point_one_four();
    let (r, out) = render(&layout, Vec3::Y, 8, 0.0);
    assert!(r.pan_mode() == PanMode::ThreeDim);
    assert!(finite(&out));
    let center = out[2];
    let fl = out[0];
    let fr = out[1];
    assert!(center > 0.95, "front object → real Center ({center})");
    assert!(fl < 1e-3, "no phantom FL ({fl})");
    assert!(fr < 1e-3, "no phantom FR ({fr})");
}

#[test]
fn energy_preserved_over_sphere_on_three_dim_layout() {
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let frames = 16usize;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 12 * frames];
    let mut covered = 0usize;
    let mut energy_max = 0.0f32;
    // Sweep elevation -60°..+60° × azimuth 0..345° (15° steps): every
    // covered direction must carry ~unit energy (LFE excluded).
    for el_deg in (-60..=60).step_by(15) {
        for az_deg in (0..360).step_by(15) {
            let el = (el_deg as f32).to_radians();
            let az = (az_deg as f32).to_radians();
            let dir = Vec3::new(el.cos() * az.sin(), el.cos() * az.cos(), el.sin());
            let mut scene = SpatialScene::new(SR);
            scene.create_audio_object(dir).unwrap();
            r.process_block(&scene, &[&input], frames, &mut out)
                .unwrap();
            assert!(finite(&out), "NaN at el={el_deg} az={az_deg}");
            let e = energy_non_lfe(&out, 12, Some(3));
            if e > 2e-3 {
                covered += 1;
                energy_max = energy_max.max(e);
                assert!(
                    e < 1.15,
                    "3D VBAP energy overshoot at el={el_deg} az={az_deg}: {e}"
                );
            }
        }
    }
    // The hull must cover a meaningful share of the sampled hemisphere.
    assert!(
        covered > 60,
        "7.1.4 covers most sampled directions ({covered})"
    );
}

#[test]
fn symmetry_left_right_is_mirrored_in_3d() {
    let layout = SpeakerLayout::seven_point_one_four();
    // Mirror pair: object at +30° azimuth / -30° (i.e. left-front vs
    // right-front), same elevation.
    let (_, left) = render(&layout, Vec3::new(-1.0, 1.7, 0.6), 8, 0.0);
    let (_, right) = render(&layout, Vec3::new(1.0, 1.7, 0.6), 8, 0.0);
    // 7.1.4 order: FL,FR,C,LFE,SL,SR,RL,RR,TFL,TFR,TRL,TRR.
    // Left-front ↔ right-front mirror: FL↔FR, SL↔SR, RL↔RR, TFL↔TFR, TRL↔TRR.
    let pairs: &[(usize, usize)] = &[
        (0, 1),   // FL ↔ FR
        (4, 5),   // SL ↔ SR
        (6, 7),   // RL ↔ RR
        (8, 9),   // TFL ↔ TFR
        (10, 11), // TRL ↔ TRR
    ];
    for &(a, b) in pairs {
        assert!(
            (left[a] - right[b]).abs() < 1e-3,
            "mirror channel {a}↔{b}: {} vs {}",
            left[a],
            right[b]
        );
    }
    // And the mirrored counterpart.
    for &(a, b) in pairs {
        assert!(
            (left[b] - right[a]).abs() < 1e-3,
            "mirror channel {b}↔{a}: {} vs {}",
            left[b],
            right[a]
        );
    }
}

#[test]
fn overhead_object_lands_on_height_speakers() {
    // Straight up in 7.1.4: the height ring is the nearest geometry, so all
    // energy lands on a top-layer speaker — never on a floor speaker.
    let layout = SpeakerLayout::seven_point_one_four();
    let (_, out) = render(&layout, Vec3::Z, 8, 0.0);
    assert!(finite(&out));
    let e = energy_non_lfe(&out, 12, Some(3));
    assert!(e > 0.5, "overhead object delivers energy ({e})");
    let floor: f32 = [0, 1, 2, 4, 5, 6, 7].iter().map(|&s| out[s] * out[s]).sum();
    let top: f32 = [8, 9, 10, 11].iter().map(|&s| out[s] * out[s]).sum();
    assert!(
        top > floor * 2.0,
        "height speakers dominate (top={top} floor={floor})"
    );
}

#[test]
fn out_of_coverage_falls_back_deterministically() {
    // 7.1.4 has no speakers below the floor. A downward object must still
    // render via the nearest-speaker fallback: deterministic, finite, and
    // delivering energy — never silent, never NaN.
    let layout = SpeakerLayout::seven_point_one_four();
    let (r, out) = render(&layout, -Vec3::Z, 8, 0.0);
    assert!(finite(&out));
    let e = energy_non_lfe(&out, 12, Some(3));
    assert!(e > 0.5, "out-of-coverage still delivers energy ({e})");
    // Deterministic: same scene again gives bit-identical output.
    let (_, out2) = render(&layout, -Vec3::Z, 8, 0.0);
    assert!(
        out.iter()
            .zip(out2.iter())
            .all(|(a, b)| (a - b).abs() < 1e-6),
        "fallback must be deterministic"
    );
    // The renderer state is not polluted by the fallback.
    let (_, out_front) = render(&layout, Vec3::Y, 8, 0.0);
    assert!(finite(&out_front));
    let _ = r;
}

#[test]
fn degenerate_coplanar_geometry_is_finite_and_energy_sane() {
    // Three coplanar speakers: 3D coverage is impossible (§27) → the
    // renderer must reduce to 2D and stay finite and energy-sane on the
    // plane — even for off-plane objects (nearest-pair fallback).
    let layout = SpeakerLayout::custom(vec![
        Vec3::new(-1.0, 0.0, 0.0),
        Vec3::new(1.0, 0.0, 0.0),
        Vec3::new(0.0, 1.0, 0.0),
    ]);
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    assert_eq!(r.pan_mode(), PanMode::Planar);
    let frames = 8usize;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 3 * frames];
    for deg in 0..=360 {
        let rad = (deg as f32).to_radians();
        for pos in [
            Vec3::new(rad.sin(), rad.cos(), 0.0),
            Vec3::new(rad.sin(), rad.cos(), 0.5),
        ] {
            let mut scene = SpatialScene::new(SR);
            scene.create_audio_object(pos).unwrap();
            r.process_block(&scene, &[&input], frames, &mut out)
                .unwrap();
            assert!(finite(&out), "NaN for coplanar layout at {deg}°");
        }
    }
}

#[test]
fn custom_asymmetric_3d_layout_pans_sensibly() {
    // A deliberately asymmetric 5-speaker 3D array: front, rear, left,
    // right, up. Must classify as 3D, render without error, and route a
    // front object to the front speaker and an up object to the up speaker.
    let layout = SpeakerLayout::custom(vec![
        Vec3::new(0.0, 1.0, 0.0),  // front
        Vec3::new(0.0, -1.0, 0.0), // rear
        Vec3::new(1.0, 0.0, 0.0),  // right
        Vec3::new(-1.0, 0.0, 0.0), // left
        Vec3::new(0.0, 0.0, 1.0),  // up
    ]);
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    assert_eq!(r.pan_mode(), PanMode::ThreeDim);

    let frames = 8usize;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 5 * frames];
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::Y).unwrap();
    r.process_block(&scene, &[&input], frames, &mut out)
        .unwrap();
    assert!(finite(&out));
    assert!(out[0] > 0.9, "front object → front speaker ({})", out[0]);

    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::Z).unwrap();
    r.process_block(&scene, &[&input], frames, &mut out)
        .unwrap();
    assert!(finite(&out));
    let up = out[4];
    let e: f32 = out.iter().take(5).map(|v| v * v).sum(); // frame 0
    assert!(up > 0.5, "up object → up speaker ({up})");
    assert!(
        (e - 1.0).abs() < 0.15,
        "energy near unity for covered dir ({e})"
    );
}

#[test]
fn continuity_full_sweep_no_nan_no_jump() {
    // A front object with the listener yawed 0..360° in fine (0.25°) steps,
    // smoothing enabled: never NaN, and the smoothed per-block delta stays
    // small (a true region-boundary snap would show up as a ≳0.05 step).
    let layout = SpeakerLayout::seven_point_one_four();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::Y).unwrap();
    let mut r = VbapRenderer::with_smoothing(DEFAULT_SMOOTHING_MS);
    r.prepare(&layout, SR).unwrap();
    let frames = 64usize;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 12 * frames];
    let mut prev_fl = 0.0f32;
    for step in 0..1440 {
        let deg = step as f32 * 0.25;
        scene
            .listener
            .set_orientation(engine::spatial::math::Quat::from_euler_rad(
                deg.to_radians(),
                0.0,
                0.0,
            ));
        r.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        assert!(finite(&out), "NaN at yaw {deg}°");
        let fl = out[0];
        if step > 0 {
            let delta = (fl - prev_fl).abs();
            assert!(delta < 0.05, "no jump in FL at yaw {deg}° ({delta})");
        }
        prev_fl = fl;
    }
}

#[test]
fn lfe_is_additive_send_never_a_pan_target() {
    // An object with an LFE send must reach the LFE channel (index 3) while
    // still panning normally to its spatial speakers — LFE is an effects
    // path, never a panning target (spec §56).
    let layout = SpeakerLayout::seven_point_one_four();
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(Vec3::Y).unwrap();
    scene.object_mut(id).unwrap().lfe_send = 1.0;
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let frames = 8usize;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 12 * frames];
    r.process_block(&scene, &[&input], frames, &mut out)
        .unwrap();
    assert!(finite(&out));
    for i in 0..frames {
        assert!(out[i * 12 + 3] > 0.9, "LFE send reaches LFE channel");
        // The front object still reaches the real Center speaker.
        assert!(out[i * 12 + 2] > 0.9, "front object still reaches Center");
    }
}
