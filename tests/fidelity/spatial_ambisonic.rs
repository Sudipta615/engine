//! Acceptance suite for Ambisonics / First-Order Ambisonics (spec Phase 7,
//! Part VI §32–37, §55).
//!
//! The contract this suite pins down:
//!
//! - **Conventions** — ACN ordering `[W, Y, Z, X]`, SN3D normalization
//!   (`W = 1`, first order = `√3·(x, y, z)`), in the spatial layer's single
//!   coordinate frame.
//! - **Plane-wave round-trip** — encode a direction, decode with the sampling
//!   ("basic") decoder: a source at `d` lands on speaker `s` as
//!   `(1 + 3·cosθ)/N`, on *any* speaker layout (stereo, 5.1, 7.1.4) from the
//!   *same* bus — the speaker-independence property (§32, §55).
//! - **Policies** — the max-rE decoder narrows the lobe (less-negative rear,
//!   same front centre) while staying bounded and finite.
//! - **Listener rotation** — a world-encoded field stays world-fixed as the
//!   listener turns; the rendered gains move smoothly (continuity sweep).
//! - **Diffuse (W-only) decode** — the sampling pattern delivers `1/N` to
//!   every pan speaker and nothing to the LFE (the field mixer's `√N`
//!   compensation, which restores unit energy, is covered in the hybrid
//!   suite's field tests).
//! - **Determinism** — the same bus + scene renders bit-identically.

use engine::spatial::ambisonic::{
    encode_plane_wave, AmbisonicDecoder, AmbisonicRenderer, DecoderPolicy,
};
use engine::spatial::math::{Quat, Vec3};
use engine::spatial::render::SpatialRenderer;
use engine::spatial::SpeakerLayout;

const SR: u32 = 48_000;
const EPS: f32 = 1e-3;

/// Interleaved bus planes `[W, Y, Z, X]` for a constant plane wave at
/// world-space `dir` (unit gain).
fn bus_planes(dir: Vec3, frames: usize) -> Vec<Vec<f32>> {
    let mut frame = [0.0f32; 4];
    encode_plane_wave(dir, 1.0, &mut frame);
    frame.iter().map(|&c| vec![c; frames]).collect::<Vec<_>>()
}

fn refs(planes: &[Vec<f32>]) -> Vec<&[f32]> {
    planes.iter().map(|p| p.as_slice()).collect()
}

/// Basic-decode one frame of the bus onto `layout`; returns per-speaker
/// gains (frame 0).
fn decode_frame_gains(layout: &SpeakerLayout, policy: DecoderPolicy, dir: Vec3) -> Vec<f32> {
    let mut dec = AmbisonicDecoder::new(policy);
    dec.prepare(layout, SR).unwrap();
    let mut frame = [0.0f32; 4];
    encode_plane_wave(dir, 1.0, &mut frame);
    let mut bus = Vec::new();
    bus.extend_from_slice(&frame);
    let ch = layout.speakers.len();
    let mut out = vec![0.0f32; ch];
    dec.process_bus(&bus, 1, &mut out);
    out
}

#[test]
fn sh_basis_and_encoder_match_documented_sn3d_acn_convention() {
    // Axis directions: [W, Y, Z, X] = [1, √3·y, √3·z, √3·x].
    let s3 = 3.0f32.sqrt();
    let mut f = [0.0f32; 4];
    encode_plane_wave(Vec3::Y, 1.0, &mut f);
    assert!(
        (f[0] - 1.0).abs() < EPS && (f[1] - s3).abs() < EPS,
        "front {:?}",
        f
    );
    encode_plane_wave(Vec3::X, 1.0, &mut f);
    assert!((f[3] - s3).abs() < EPS && f[1].abs() < EPS, "right {:?}", f);
    encode_plane_wave(Vec3::Z, 1.0, &mut f);
    assert!((f[2] - s3).abs() < EPS && f[3].abs() < EPS, "up {:?}", f);
    // A zero direction encodes silence, not NaN.
    encode_plane_wave(Vec3::ZERO, 1.0, &mut f);
    assert!(f.iter().all(|v| v.is_finite()));
}

#[test]
fn plane_wave_round_trips_to_every_layout_from_one_bus() {
    // The same encoded bus decodes onto stereo, 5.1 and 7.1.4 with the
    // sampling pattern (1 + 3·cosθ)/N — speaker independence (§32, §55).
    for (layout, n) in [
        (SpeakerLayout::stereo(), 2usize),
        (SpeakerLayout::five_point_one(), 5usize),
        (SpeakerLayout::seven_point_one_four(), 11usize),
    ] {
        let dir = Vec3::Y; // front
        let gains = decode_frame_gains(&layout, DecoderPolicy::Basic, dir);
        assert!(gains.iter().all(|g| g.is_finite()));
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                assert!(gains[idx].abs() < EPS, "LFE/disbled {idx} silent");
                continue;
            }
            let cos = s.position.normalized().unwrap().dot(dir);
            let expected = (1.0 + 3.0 * cos) / n as f32;
            assert!(
                (gains[idx] - expected).abs() < EPS,
                "speaker {idx}: {} want {expected}",
                gains[idx]
            );
        }
    }
}

#[test]
fn max_re_policy_narrows_the_lobe() {
    // Max-rE (a1 = √3/2): the same front-centre peak as Basic, but the
    // rear/opposite lobe is less negative — a narrower, more focused image.
    let layout = SpeakerLayout::seven_point_one_four();
    let basic = decode_frame_gains(&layout, DecoderPolicy::Basic, Vec3::Y);
    let maxre = decode_frame_gains(&layout, DecoderPolicy::MaxRe, Vec3::Y);
    let n = 11usize;
    let a1 = 0.866_025_4f32;
    for (idx, s) in layout.speakers.iter().enumerate() {
        if s.is_lfe || !s.enabled {
            continue;
        }
        let cos = s.position.normalized().unwrap().dot(Vec3::Y);
        let expected = (1.0 + 3.0 * a1 * cos) / n as f32;
        assert!((maxre[idx] - expected).abs() < EPS, "max-rE speaker {idx}");
        // Rear speakers: max-rE is less negative (a1 < 1 lifts cosθ = −1).
        if cos < -0.5 {
            assert!(
                maxre[idx] > basic[idx],
                "rear {idx} less negative under max-rE"
            );
        }
        assert!(maxre[idx].is_finite());
    }
}

#[test]
fn listener_rotation_keeps_world_field_world_fixed() {
    // A field encoded at world +X. Listener yaws +90° (now faces +X): the
    // field must appear dead ahead → equal FL/FR split on stereo. At yaw 0°
    // it is hard right → FR dominates.
    let layout = SpeakerLayout::stereo();
    let mut r = AmbisonicRenderer::new(DecoderPolicy::Basic);
    r.prepare(&layout, SR).unwrap();
    let frames = 8usize;
    let planes = bus_planes(Vec3::X, frames);
    let rrefs = refs(&planes);

    let mut scene = engine::spatial::SpatialScene::new(SR);
    // Yaw +90°: source at world right → local front.
    scene
        .listener
        .set_orientation(Quat::from_euler_rad(std::f32::consts::FRAC_PI_2, 0.0, 0.0));
    let mut out = vec![0.0f32; 2 * frames];
    r.process_block(&scene, &rrefs, frames, &mut out).unwrap();
    let expected = (1.0 + 3.0 * 30f32.to_radians().cos()) / 2.0;
    assert!(
        (out[0] - expected).abs() < EPS && (out[1] - expected).abs() < EPS,
        "front: FL {} FR {} want {expected}",
        out[0],
        out[1]
    );

    // Yaw 0°: the same world source is at the listener's right → FR strongest.
    scene
        .listener
        .set_orientation(Quat::from_euler_rad(0.0, 0.0, 0.0));
    let mut out2 = vec![0.0f32; 2 * frames];
    r.process_block(&scene, &rrefs, frames, &mut out2).unwrap();
    assert!(
        out2[1] > out2[0] && out2[1] > 1.0,
        "hard right: FL {} FR {}",
        out2[0],
        out2[1]
    );
}

#[test]
fn w_only_bus_decodes_at_equal_power_per_speaker() {
    // A perfectly diffuse field encodes to W only. The sampling decoder then
    // delivers W/N = 1/N to every pan speaker (unit W), nothing to the LFE,
    // total energy 1/N. (The field mixer's √N compensation — which restores
    // unit energy for diffuse content — is asserted by the hybrid suite.)
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = AmbisonicRenderer::new(DecoderPolicy::Basic);
    r.prepare(&layout, SR).unwrap();
    let frames = 8usize;
    let w = vec![1.0f32; frames];
    let zero = vec![0.0f32; frames];
    let planes: Vec<Vec<f32>> = vec![w, zero.clone(), zero.clone(), zero];
    let rrefs = refs(&planes);
    let ch = layout.speakers.len();
    let mut out = vec![0.0f32; ch * frames];
    let scene = engine::spatial::SpatialScene::new(SR);
    r.process_block(&scene, &rrefs, frames, &mut out).unwrap();

    let n = 11usize;
    let per = 1.0 / n as f32;
    for (idx, s) in layout.speakers.iter().enumerate() {
        if s.is_lfe {
            assert!(out[idx].abs() < EPS, "LFE silent");
            continue;
        }
        assert!(
            (out[idx] - per).abs() < EPS,
            "speaker {idx} = {} want {per}",
            out[idx]
        );
    }
    // Total energy = N·(1/N)² = 1/N.
    let e: f32 = out[..ch].iter().map(|v| v * v).sum();
    assert!((e - 1.0 / n as f32).abs() < EPS, "W-only energy {e}");
}

#[test]
fn listener_rotation_continuity_sweep() {
    // Rotate the listener through 360° in 0.5° steps with a constant
    // world-front source. The decoded gains are sinusoids of the angle, so
    // per-step deltas must stay tiny — no jumps, no clicks (§48).
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = AmbisonicRenderer::new(DecoderPolicy::Basic);
    r.prepare(&layout, SR).unwrap();
    let ch = layout.speakers.len();
    let frames = 1usize;
    let planes = bus_planes(Vec3::Y, frames);
    let rrefs = refs(&planes);
    let mut scene = engine::spatial::SpatialScene::new(SR);

    let mut prev: Option<Vec<f32>> = None;
    let step = 0.5f32.to_radians();
    for i in 0..720 {
        let yaw = i as f32 * step;
        scene
            .listener
            .set_orientation(Quat::from_euler_rad(yaw, 0.0, 0.0));
        let mut out = vec![0.0f32; ch];
        r.process_block(&scene, &rrefs, frames, &mut out).unwrap();
        assert!(out.iter().all(|v| v.is_finite()));
        if let Some(prev) = &prev {
            let max_delta = out
                .iter()
                .zip(prev.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                max_delta < 0.01,
                "continuity: yaw {}° jumped {max_delta}",
                i
            );
        }
        prev = Some(out);
    }
}

#[test]
fn renderer_is_deterministic_and_rejects_unprepared_use() {
    let layout = SpeakerLayout::five_point_one();
    let mut r = AmbisonicRenderer::new(DecoderPolicy::MaxRe);
    r.prepare(&layout, SR).unwrap();
    let frames = 32usize;
    let planes = bus_planes(Vec3::new(1.0, 2.0, 3.0).normalized().unwrap(), frames);
    let rrefs = refs(&planes);
    let scene = engine::spatial::SpatialScene::new(SR);

    let ch = 6usize;
    let mut a = vec![0.0f32; ch * frames];
    let mut b = vec![0.0f32; ch * frames];
    r.process_block(&scene, &rrefs, frames, &mut a).unwrap();
    r.process_block(&scene, &rrefs, frames, &mut b).unwrap();
    assert!(a.iter().all(|v| v.is_finite()));
    assert!(
        a.iter().zip(b.iter()).all(|(x, y)| x == y),
        "deterministic decode"
    );

    // Unprepared renderer is rejected, not silently wrong.
    let mut raw = AmbisonicRenderer::new(DecoderPolicy::Basic);
    let mut out = vec![0.0f32; ch * frames];
    assert!(raw.process_block(&scene, &rrefs, frames, &mut out).is_err());
}
