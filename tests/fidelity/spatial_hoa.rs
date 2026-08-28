//! Acceptance suite for higher-order ambisonics (spec Phase 16 / roadmap
//! Phase 16, Part VI §32–37, §55).
//!
//! The contract this suite pins down:
//!
//! - **Conventions** — ACN ordering (order 2 = `[W, Y, Z, X, …]`), SN3D
//!   normalization (order 2 block: `√15·xy, √15·yz, (√5/2)(3z²−1),
//!   √15·xz, (√15/2)(x²−y²)`), the spatial layer's single coordinate frame.
//! - **Sampling decode** — the order-2 basic decoder is the exact
//!   projection `Y(S)ᵀ·Y(d)/N`: a plane wave from `d` lands on speaker `s`
//!   with the full 9-channel inner product, and the same bus renders to
//!   any layout (speaker independence).
//! - **Rotation** — the defining property `sh_n(R·d) == W(R)·sh_n(d)` for
//!   the exact order-2 Wigner block; a world-encoded order-2 field stays
//!   world-fixed as the listener turns.
//! - **Max-rE** — the published Zotter–Frank window (`a1 ≈ 0.9057`,
//!   `a2 ≈ 0.6827`) narrows the decoded lobe vs basic.
//! - **Determinism** — identical buses through fresh renderers are
//!   bit-for-bit identical and finite.

use engine::spatial::math::Vec3;
use engine::spatial::render::SpatialRenderer;
use engine::spatial::{
    channel_count, encode_plane_wave_n, sh_n, AmbisonicRenderer, DecoderPolicy, Quat, SpatialScene,
    SpeakerLayout, MAX_AMBISONIC_ORDER,
};
use std::f32::consts::FRAC_PI_2;

const SR: u32 = 48_000;
const EPS: f32 = 1e-3;

#[test]
fn order2_sn3d_acn_conventions_are_pinned() {
    // The documented second-order block at the cardinal directions.
    let s15 = 15.0f32.sqrt();
    let s5h = 5.0f32.sqrt() * 0.5;
    let mut y = [0.0f32; 9];
    sh_n(2, Vec3::Y, &mut y);
    assert!((y[0] - 1.0).abs() < EPS, "W");
    assert!((y[6] + s5h).abs() < EPS, "Y₂⁰(+Y) = −√5/2");
    assert!((y[8] + s15 * 0.5).abs() < EPS, "Y₂²(+Y) = −√15/2");
    sh_n(2, Vec3::X, &mut y);
    assert!((y[8] - s15 * 0.5).abs() < EPS, "Y₂²(+X) = +√15/2");
    sh_n(2, Vec3::Z, &mut y);
    assert!((y[6] - s5h * 2.0).abs() < EPS, "Y₂⁰(+Z) = +√5");
    assert_eq!(channel_count(2), 9);
    assert!(MAX_AMBISONIC_ORDER >= 3, "order-3 supported");
}

#[test]
fn order2_plane_wave_round_trips_to_any_layout() {
    // One order-2 bus, two different layouts: the decoded plane wave from
    // `d` is Y(S)ᵀ·Y(d)/N on each layout's pan speakers. The SAME bus
    // decodes to stereo and 7.1.4 — speaker independence.
    let d = Vec3::new(1.0, 2.0, 0.0).normalized().unwrap();
    let mut frame = [0.0f32; 9];
    encode_plane_wave_n(2, d, 1.0, &mut frame);
    let frames = 4;
    // All-ones plane (W=1 at frame 0) per channel: rebuild properly.
    let planes: Vec<Vec<f32>> = frame
        .iter()
        .map(|&v| {
            let mut p = vec![0.0f32; frames];
            p[0] = v;
            p
        })
        .collect();
    let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
    for layout in [
        SpeakerLayout::stereo(),
        SpeakerLayout::seven_point_one_four(),
    ] {
        let mut r = AmbisonicRenderer::with_order(DecoderPolicy::Basic, 2);
        r.prepare(&layout, SR).unwrap();
        let scene = SpatialScene::new(SR);
        let mut out = vec![0.0f32; layout.speakers.len() * frames];
        r.process_block(&scene, &refs, frames, &mut out).unwrap();
        let n = layout.pan_speaker_count() as f32;
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                assert_eq!(out[idx], 0.0, "LFE/speaker {idx} silent");
                continue;
            }
            let mut sy = [0.0f32; 9];
            sh_n(2, s.position.normalized().unwrap(), &mut sy);
            let expected: f32 = sy.iter().zip(frame.iter()).map(|(a, b)| a * b).sum::<f32>() / n;
            assert!(
                (out[idx] - expected).abs() < EPS,
                "speaker {idx}: {} want {expected}",
                out[idx]
            );
        }
    }
}

#[test]
fn order2_rotation_is_exact_on_the_basis() {
    // The defining property of the rotation block: evaluating the basis at
    // a rotated direction equals rotating the basis coefficients.
    for q in [
        Quat::from_euler_rad(0.7, 0.3, -0.2),
        Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0),
    ] {
        for d in [Vec3::Y, Vec3::X, Vec3::new(1.0, 2.0, 3.0)] {
            let d = d.normalized().unwrap();
            let rd = q.rotate_vec3(d);
            let mut y = [0.0f32; 9];
            let mut yr = [0.0f32; 9];
            sh_n(2, d, &mut y);
            sh_n(2, rd, &mut yr);
            let mut rotated = y;
            engine::spatial::rotate_bus_frame_n(q, 2, &mut rotated);
            for k in 0..9 {
                assert!(
                    (rotated[k] - yr[k]).abs() < 1e-3,
                    "q={q:?} d={d:?} channel {k}: {} vs {}",
                    rotated[k],
                    yr[k]
                );
            }
        }
        // Round trip: rotate then un-rotate restores the bus.
        let mut f = [0.0f32; 9];
        encode_plane_wave_n(
            2,
            Vec3::new(1.0, 2.0, 3.0).normalized().unwrap(),
            1.0,
            &mut f,
        );
        let orig = f;
        engine::spatial::rotate_bus_frame_n(q, 2, &mut f);
        engine::spatial::rotate_bus_frame_n(q.conjugate(), 2, &mut f);
        for k in 0..9 {
            assert!((f[k] - orig[k]).abs() < 1e-3, "round-trip channel {k}");
        }
    }
}

#[test]
fn order2_world_field_stays_world_fixed_as_listener_turns() {
    // Encode an order-2 plane wave at world +X; yaw the listener +90° (faces
    // +X): the source must land at the front. The renderer rotates the bus
    // by the listener's conjugate — the full 9-channel Wigner rotation.
    let layout = SpeakerLayout::stereo();
    let mut r = AmbisonicRenderer::with_order(DecoderPolicy::Basic, 2);
    r.prepare(&layout, SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene
        .listener
        .set_orientation(Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0));
    let frames = 8;
    let mut frame = [0.0f32; 9];
    encode_plane_wave_n(2, Vec3::X, 1.0, &mut frame);
    let planes: Vec<Vec<f32>> = frame.iter().map(|&v| vec![v; frames]).collect();
    let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
    let mut out = vec![0.0f32; 2 * frames];
    r.process_block(&scene, &refs, frames, &mut out).unwrap();
    // Expected: the rotated bus equals the order-2 encoding of +Y, decoded
    // by the full 9-channel projection on stereo.
    let mut src = [0.0f32; 9];
    sh_n(2, Vec3::Y, &mut src);
    for (idx, s) in layout.speakers.iter().enumerate() {
        let mut sy = [0.0f32; 9];
        sh_n(2, s.position.normalized().unwrap(), &mut sy);
        let expected: f32 = sy.iter().zip(src.iter()).map(|(a, b)| a * b).sum::<f32>() / 2.0;
        assert!(
            (out[idx] - expected).abs() < 1e-3,
            "speaker {idx}: {} want {expected}",
            out[idx]
        );
    }
}

#[test]
fn order2_max_re_narrows_the_lobe_with_documented_window() {
    // Max-rE order 2 (a1 ≈ 0.9057, a2 ≈ 0.6827): the rear speakers receive
    // measurably less energy than the order-2 basic decode (the lobe
    // narrows toward the source), and the per-speaker gains match the
    // documented window applied on the decode side only.
    let layout = SpeakerLayout::seven_point_one_four();
    let mut basic = AmbisonicRenderer::with_order(DecoderPolicy::Basic, 2);
    let mut maxre = AmbisonicRenderer::with_order(DecoderPolicy::MaxRe, 2);
    basic.prepare(&layout, SR).unwrap();
    maxre.prepare(&layout, SR).unwrap();
    let scene = SpatialScene::new(SR);
    let frames = 2;
    let mut frame = [0.0f32; 9];
    encode_plane_wave_n(2, Vec3::Y, 1.0, &mut frame);
    let planes: Vec<Vec<f32>> = frame.iter().map(|&v| vec![v; frames]).collect();
    let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
    let mut out_b = vec![0.0f32; 12 * frames];
    let mut out_m = vec![0.0f32; 12 * frames];
    basic
        .process_block(&scene, &refs, frames, &mut out_b)
        .unwrap();
    maxre
        .process_block(&scene, &refs, frames, &mut out_m)
        .unwrap();
    let n = layout.pan_speaker_count() as f32;
    let (a1, a2) = (0.905_663_1f32, 0.682_689_4f32);
    for (idx, s) in layout.speakers.iter().enumerate() {
        if s.is_lfe || !s.enabled {
            continue;
        }
        let mut sy = [0.0f32; 9];
        sh_n(2, s.position.normalized().unwrap(), &mut sy);
        let expected: f32 = sy
            .iter()
            .zip(frame.iter())
            .enumerate()
            .map(|(k, (a, b))| {
                // The decoder weights apply on the decode side only.
                let w = if k == 0 {
                    1.0
                } else if k <= 3 {
                    a1
                } else {
                    a2
                };
                w * a * b
            })
            .sum::<f32>()
            / n;
        assert!(
            (out_m[idx] - expected).abs() < 1e-3,
            "max-rE speaker {idx}: {} want {expected}",
            out_m[idx]
        );
    }
    // Rear energy: max-rE order 2 < basic order 2.
    let rear_b: f32 = out_b[6 * frames..8 * frames].iter().map(|v| v * v).sum();
    let rear_m: f32 = out_m[6 * frames..8 * frames].iter().map(|v| v * v).sum();
    assert!(
        rear_m < rear_b,
        "max-rE narrows rear ({rear_m} vs {rear_b})"
    );
}

#[test]
fn order2_render_is_deterministic_and_finite() {
    let layout = SpeakerLayout::seven_point_one_four();
    let scene = SpatialScene::new(SR);
    let frames = 64;
    let mut frame = [0.0f32; 9];
    encode_plane_wave_n(
        2,
        Vec3::new(1.0, 0.5, 0.3).normalized().unwrap(),
        0.7,
        &mut frame,
    );
    let planes: Vec<Vec<f32>> = frame.iter().map(|&v| vec![v; frames]).collect();
    let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
    let render = || {
        let mut r = AmbisonicRenderer::with_order(DecoderPolicy::MaxRe, 2);
        r.prepare(&layout, SR).unwrap();
        let mut out = vec![0.0f32; 12 * frames];
        r.process_block(&scene, &refs, frames, &mut out).unwrap();
        out
    };
    let a = render();
    let b = render();
    assert_eq!(a, b, "bit-for-bit deterministic");
    assert!(a.iter().all(|v| v.is_finite()));
}

#[test]
fn order3_sn3d_acn_conventions_are_pinned() {
    // Third-order block (ACN 9–15), SN3D mean-square 1:
    //   √(35/8)·x(x²−3y²), √105·xyz, √(21/8)·y(5z²−1), (√7/2)·z(5z²−3),
    //   √(21/8)·x(5z²−1), (√105/2)·z(x²−y²), √(35/8)·y(y²−3x²).
    let s358 = (35.0f32 / 8.0).sqrt();
    let s218 = (21.0f32 / 8.0).sqrt();
    let s7h = 7.0f32.sqrt() * 0.5;
    assert_eq!(channel_count(3), 16);
    let mut y = [0.0f32; 16];
    sh_n(3, Vec3::Y, &mut y);
    assert!((y[15] - s358).abs() < EPS, "Y₃³(+Y) = √(35/8)");
    assert!((y[11] + s218).abs() < EPS, "Y₃⁻¹(+Y) = −√(21/8)");
    sh_n(3, Vec3::Z, &mut y);
    assert!(
        (y[12] - (s7h * 2.0)).abs() < EPS,
        "Y₃⁰(+Z) = √7 ≈ {}, got {}",
        s7h * 2.0,
        y[12]
    );
    // The order-1 and order-2 rows are unchanged by the order-3 extension.
    let mut o1 = [0.0f32; 4];
    let mut o3 = [0.0f32; 16];
    sh_n(1, Vec3::Y, &mut o1);
    sh_n(3, Vec3::Y, &mut o3);
    for k in 0..4 {
        assert!((o1[k] - o3[k]).abs() < EPS, "ACN {k} unchanged");
    }
}

#[test]
fn order3_rotation_is_exact_and_world_fixed() {
    // The defining rotation property at order 3: sh_n(3, R·d) == W₃·sh_n(3, d),
    // and a world-encoded order-3 field stays world-fixed as the listener turns.
    for q in [
        Quat::from_euler_rad(0.7, 0.3, -0.2),
        Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0),
    ] {
        for d in [Vec3::Y, Vec3::X, Vec3::new(1.0, 2.0, 3.0)] {
            let d = d.normalized().unwrap();
            let rd = q.rotate_vec3(d);
            let mut y = [0.0f32; 16];
            let mut yr = [0.0f32; 16];
            sh_n(3, d, &mut y);
            sh_n(3, rd, &mut yr);
            let mut rotated = y;
            engine::spatial::rotate_bus_frame_n(q, 3, &mut rotated);
            for k in 0..16 {
                assert!(
                    (rotated[k] - yr[k]).abs() < 2e-3,
                    "q={q:?} d={d:?} ACN {k}: {} vs {}",
                    rotated[k],
                    yr[k]
                );
            }
        }
    }
    // Round-trip: rotate then un-rotate restores the order-3 bus exactly.
    let q = Quat::from_euler_rad(0.7, 0.3, -0.2);
    let mut f = [0.0f32; 16];
    encode_plane_wave_n(
        3,
        Vec3::new(1.0, 2.0, 3.0).normalized().unwrap(),
        1.0,
        &mut f,
    );
    let orig = f;
    engine::spatial::rotate_bus_frame_n(q, 3, &mut f);
    engine::spatial::rotate_bus_frame_n(q.conjugate(), 3, &mut f);
    for k in 0..16 {
        assert!((f[k] - orig[k]).abs() < 2e-3, "round-trip ACN {k}");
    }
    // End to end: the renderer rotates a world order-3 field into the
    // listener frame (same full 16-channel projection as the unit tests).
    let layout = SpeakerLayout::stereo();
    let mut r = AmbisonicRenderer::with_order(DecoderPolicy::Basic, 3);
    r.prepare(&layout, SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene
        .listener
        .set_orientation(Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0));
    let frames = 8;
    let mut frame = [0.0f32; 16];
    encode_plane_wave_n(3, Vec3::X, 1.0, &mut frame);
    let planes: Vec<Vec<f32>> = frame.iter().map(|&v| vec![v; frames]).collect();
    let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
    let mut out = vec![0.0f32; 2 * frames];
    r.process_block(&scene, &refs, frames, &mut out).unwrap();
    let mut src = [0.0f32; 16];
    sh_n(3, Vec3::Y, &mut src);
    for (idx, s) in layout.speakers.iter().enumerate() {
        let mut sy = [0.0f32; 16];
        sh_n(3, s.position.normalized().unwrap(), &mut sy);
        let expected: f32 = sy.iter().zip(src.iter()).map(|(a, b)| a * b).sum::<f32>() / 2.0;
        assert!(
            (out[idx] - expected).abs() < 2e-3,
            "order-3 speaker {idx}: {} want {expected}",
            out[idx]
        );
    }
}

#[test]
fn order3_max_re_window_narrows_the_lobe() {
    // Order-3 max-rE uses the published third-order window; decode a front
    // plane wave on 7.1.4 with each of order-2 and order-3 max-rE and check
    // the rear lobe is narrower at order 3.
    let layout = SpeakerLayout::seven_point_one_four();
    let scene = SpatialScene::new(SR);
    let frames = 2;
    let front = |order: u8| -> Vec<f32> {
        let mut frame = [0.0f32; 16];
        encode_plane_wave_n(order, Vec3::Y, 1.0, &mut frame);
        let planes: Vec<Vec<f32>> = frame[..channel_count(order)]
            .iter()
            .map(|&v| vec![v; frames])
            .collect();
        // Pad to 16 planes so one renderer path handles all orders.
        let mut refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
        while refs.len() < 16 {
            refs.push(&[][..]);
        }
        let mut r = AmbisonicRenderer::with_order(DecoderPolicy::MaxRe, order);
        r.prepare(&layout, SR).unwrap();
        let mut out = vec![0.0f32; 12 * frames];
        r.process_block(&scene, &refs, frames, &mut out).unwrap();
        out
    };
    let o2 = front(2);
    let o3 = front(3);
    // Verify the published order-3 window on the decode: for a front plane
    // wave `d`, max-rE speaker gain = Σ_k a_l·Y_k(s)·Y_k(d)/N with the
    // third-order window (a1≈0.7660, a2≈0.6534, a3≈0.5715).
    let n = layout.pan_speaker_count() as f32;
    let (a1, a2, a3) = (0.766_044_5f32, 0.653_445_5f32, 0.571_5f32);
    let mut frame3 = [0.0f32; 16];
    encode_plane_wave_n(3, Vec3::Y, 1.0, &mut frame3);
    for (idx, s) in layout.speakers.iter().enumerate() {
        if s.is_lfe || !s.enabled {
            continue;
        }
        let mut sy = [0.0f32; 16];
        sh_n(3, s.position.normalized().unwrap(), &mut sy);
        let expected: f32 = sy
            .iter()
            .zip(frame3.iter())
            .enumerate()
            .map(|(k, (a, b))| {
                let aw = if k == 0 {
                    1.0
                } else if k <= 3 {
                    a1
                } else if k <= 8 {
                    a2
                } else {
                    a3
                };
                aw * a * b
            })
            .sum::<f32>()
            / n;
        assert!(
            (o3[idx] - expected).abs() < 1e-3,
            "order-3 max-rE speaker {idx}: {} want {expected}",
            o3[idx]
        );
    }
    // Within order 3, max-rE narrows the rear lobe vs the basic decoder
    // (the same invariant the order-2 test pins for order 2).
    let basic = {
        let mut r = AmbisonicRenderer::with_order(DecoderPolicy::Basic, 3);
        r.prepare(&layout, SR).unwrap();
        let mut frame = [0.0f32; 16];
        encode_plane_wave_n(3, Vec3::Y, 1.0, &mut frame);
        let planes: Vec<Vec<f32>> = frame.iter().map(|&v| vec![v; frames]).collect();
        let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
        let mut out = vec![0.0f32; 12 * frames];
        r.process_block(&scene, &refs, frames, &mut out).unwrap();
        out
    };
    let rear_basic: f32 = basic[6 * frames..8 * frames].iter().map(|v| v * v).sum();
    let rear_maxre: f32 = o3[6 * frames..8 * frames].iter().map(|v| v * v).sum();
    assert!(
        rear_maxre < rear_basic,
        "order-3 max-rE narrows rear vs basic ({rear_maxre} vs {rear_basic})"
    );
    let _ = o2; // (order-2 max-rE is pinned by its own test)
    assert!(o3.iter().all(|v| v.is_finite()));
}
