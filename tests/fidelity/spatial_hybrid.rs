//! Acceptance suite for hybrid beds & fields (spec Phase 6 / §13, §37).
//!
//! The contract tests the spatial mixer is reviewed against:
//!
//! - **Beds** — channel-based content must route by *semantic role* onto the
//!   matching output speakers (never by numeric position), including the LFE
//!   channel, with gain + calibration trim; unmatched channels drop cleanly.
//! - **Fields** — a diffuse source must spread with equal power across every
//!   pan speaker, be decorrelated per speaker (impulses arrive at distinct,
//!   deterministic delays), never touch the LFE, and preserve total energy.
//! - **Hybrid mix** — objects + beds + fields rendered together through one
//!   `process_hybrid_block` must sum deterministically, stay finite, and let
//!   each content class land where it belongs.

use engine::decode::ChannelLayout;
use engine::spatial::math::Vec3;
use engine::spatial::render::{HybridBlockInputs, SpatialRenderer};
use engine::spatial::{SpatialScene, SpeakerLayout, VbapRenderer};

const SR: u32 = 48_000;

/// Frame-0 speaker outputs after a hybrid render with unit planes.
fn hybrid_frame(
    renderer: &mut VbapRenderer,
    scene: &SpatialScene,
    object_planes: &[&[f32]],
    bed_planes: &[&[f32]],
    field_planes: &[&[f32]],
    frames: usize,
) -> Vec<f32> {
    let ch = renderer.channels();
    let mut out = vec![0.0f32; ch * frames];
    let inputs = HybridBlockInputs {
        objects: object_planes,
        beds: bed_planes,
        fields: field_planes,
    };
    renderer
        .process_hybrid_block(scene, &inputs, frames, &mut out)
        .unwrap();
    out[..ch].to_vec() // frame 0, all channels
}

#[test]
fn bed_routes_5_1_by_semantic_role_onto_5_1_output() {
    // A 5.1 bed on a 5.1 layout: every authored channel must land on exactly
    // the matching speaker, regardless of any numeric coincidence.
    let layout = SpeakerLayout::five_point_one();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();

    let mut scene = SpatialScene::new(SR);
    scene.create_bed(ChannelLayout::FivePointOne).unwrap();
    // FL=1, FR=2, C=3, LFE=4, SL=5, SR=6 (unit impulses, one frame).
    let planes: Vec<Vec<f32>> = (0..6).map(|v| vec![v as f32 + 1.0]).collect();
    let refs: Vec<&[f32]> = planes.iter().map(|v| v.as_slice()).collect();
    let frame = hybrid_frame(&mut r, &scene, &[], &refs, &[], 1);
    assert!(frame.iter().all(|x| x.is_finite()));
    for (spk, expected) in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0].iter().enumerate() {
        assert!(
            (frame[spk] - expected).abs() < 1e-4,
            "speaker {spk} = {} want {expected}",
            frame[spk]
        );
    }
}

#[test]
fn bed_drops_channels_with_no_matching_speaker() {
    // A 7.1 bed on a 5.1 output: RL/RR (planes 6/7) have no 5.1 speaker and
    // must be dropped; the five matching channels + LFE still route.
    let layout = SpeakerLayout::five_point_one();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();

    let mut scene = SpatialScene::new(SR);
    scene.create_bed(ChannelLayout::SevenPointOne).unwrap();
    let planes: Vec<Vec<f32>> = (0..8).map(|v| vec![v as f32 + 1.0]).collect();
    let refs: Vec<&[f32]> = planes.iter().map(|v| v.as_slice()).collect();
    let frame = hybrid_frame(&mut r, &scene, &[], &refs, &[], 1);
    for (spk, expected) in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0].iter().enumerate() {
        assert!(
            (frame[spk] - expected).abs() < 1e-4,
            "matching speaker {spk} = {} want {expected}",
            frame[spk]
        );
    }
    // No stray energy from the dropped RL/RR planes.
    assert!(
        (frame[1] - 2.0).abs() < 1e-4,
        "FR unaffected by dropped planes"
    );
}

#[test]
fn field_spreads_equal_power_with_distinct_decorrelation_delays() {
    // An impulse field on 7.1.4: every pan speaker gets 1/√11, each at its
    // own deterministic delay; the LFE stays silent; total energy ≈ 1.
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();

    let mut scene = SpatialScene::new(SR);
    scene.create_field().unwrap();
    let frames = 700usize; // > max decorrelation delay (~10.25 ms ≈ 492 samples)
    let plane = vec![1.0f32];
    let refs: Vec<&[f32]> = vec![plane.as_slice()];

    let ch = r.channels();
    let mut out = vec![0.0f32; ch * frames];
    let inputs = HybridBlockInputs {
        objects: &[],
        beds: &[],
        fields: &refs,
    };
    r.process_hybrid_block(&scene, &inputs, frames, &mut out)
        .unwrap();
    assert!(out.iter().all(|x| x.is_finite()));

    // Per-speaker first-arrival: exactly 1/√11 at distinct frames.
    let n = 11usize;
    let per = 1.0 / (n as f32).sqrt();
    let mut arrivals: Vec<usize> = Vec::new();
    for spk in 0..ch {
        if spk == 3 {
            continue; // LFE
        }
        let first = (0..frames)
            .find(|&f| out[f * ch + spk].abs() > 1e-3)
            .expect("every pan speaker receives the field");
        assert!(
            (out[first * ch + spk] - per).abs() < 1e-3,
            "speaker {spk} arrival gain {} want {per}",
            out[first * ch + spk]
        );
        arrivals.push(first);
    }
    // All arrival delays distinct → the field is decorrelated across speakers.
    assert_eq!(arrivals.len(), n);
    let mut seen = std::collections::HashSet::new();
    for &a in &arrivals {
        assert!(a > 0, "no zero-delay speaker");
        assert!(seen.insert(a), "distinct decorrelation delays");
    }
    // LFE silent throughout.
    for f in 0..frames {
        assert!(out[f * ch + 3].abs() < 1e-6, "LFE silent at frame {f}");
    }
    // Total energy ≈ 1.
    let e: f32 = (0..ch)
        .filter(|&spk| spk != 3)
        .map(|spk| (0..frames).map(|f| out[f * ch + spk]).sum::<f32>())
        .map(|s| s * s)
        .sum();
    assert!((e - 1.0).abs() < 1e-3, "field energy {e}");
}

#[test]
fn hybrid_mix_is_deterministic_and_finite() {
    // Objects + bed + field in one scene: each class lands where it belongs
    // and the sum is reproducible.
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();

    let mut scene = SpatialScene::new(SR);
    // Object at front (renders on the real Center, index 2).
    let obj = scene.create_audio_object(Vec3::Y).unwrap();
    scene.object_mut(obj).unwrap().gain = 0.5;
    // 5.1 bed with its own level.
    let bed = scene.create_bed(ChannelLayout::FivePointOne).unwrap();
    scene.bed_mut(bed).unwrap().gain = 0.25;
    // Field with modest gain.
    scene.create_field().unwrap();

    let frames = 8usize;
    let obj_planes: Vec<Vec<f32>> = vec![vec![1.0; frames]];
    let obj_refs: Vec<&[f32]> = obj_planes.iter().map(|v| v.as_slice()).collect();
    // Bed channels FL..SR as constant 1.0.
    let bed_planes: Vec<Vec<f32>> = (0..6).map(|_| vec![1.0; frames]).collect();
    let bed_refs: Vec<&[f32]> = bed_planes.iter().map(|v| v.as_slice()).collect();
    // Field: single impulse plane.
    let mut field_plane = vec![0.0f32; frames];
    field_plane[0] = 1.0;
    let field_refs: Vec<&[f32]> = vec![field_plane.as_slice()];

    let run = |r: &mut VbapRenderer| -> Vec<f32> {
        let ch = r.channels();
        let mut out = vec![0.0f32; ch * frames];
        let inputs = HybridBlockInputs {
            objects: &obj_refs,
            beds: &bed_refs,
            fields: &field_refs,
        };
        r.process_hybrid_block(&scene, &inputs, frames, &mut out)
            .unwrap();
        out
    };
    let a = run(&mut r);
    let b = run(&mut r);
    assert!(a.iter().all(|x| x.is_finite()));
    assert!(
        a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-7),
        "hybrid mix is deterministic"
    );

    // Object contribution on the front-center speaker (index 2): 0.5 gain.
    // Bed Center channel also feeds C: +0.25. Field contributes ~0 everywhere
    // (impulse is in the delay lines during warm-up), so C ≈ 0.75.
    assert!(
        (a[2] - 0.75).abs() < 0.05,
        "Center = object + bed C (got {})",
        a[2]
    );
    // Bed FL/FR/SL/SR receive their 0.25 bed levels.
    assert!((a[0] - 0.25).abs() < 0.05, "FL = bed FL ({})", a[0]);
    assert!((a[4] - 0.25).abs() < 0.05, "SL = bed SL ({})", a[4]);
    // LFE receives the bed's LFE channel (0.25) and nothing else.
    assert!((a[3] - 0.25).abs() < 0.05, "LFE = bed LFE ({})", a[3]);
}

#[test]
fn hybrid_accepts_missing_planes_without_error() {
    // Beds/fields with no planes are tolerated (write silence), and the
    // object path still renders.
    let layout = SpeakerLayout::stereo();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::Y).unwrap();
    scene.create_bed(ChannelLayout::FivePointOne).unwrap();
    scene.create_field().unwrap();

    let frames = 8usize;
    let obj_planes: Vec<Vec<f32>> = vec![vec![1.0; frames]];
    let obj_refs: Vec<&[f32]> = obj_planes.iter().map(|v| v.as_slice()).collect();
    let ch = r.channels();
    let mut out = vec![0.0f32; ch * frames];
    let inputs = HybridBlockInputs {
        objects: &obj_refs,
        beds: &[],
        fields: &[],
    };
    r.process_hybrid_block(&scene, &inputs, frames, &mut out)
        .unwrap();
    assert!(out.iter().all(|x| x.is_finite()));
    // Front object still lands as an equal split.
    assert!(
        (out[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3,
        "object FL {}",
        out[0]
    );
    assert!(
        (out[1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3,
        "object FR {}",
        out[1]
    );
}

#[test]
fn panner_hybrid_mixes_beds_and_fields() {
    // The equal-power panner's hybrid path must mix beds (semantic routing)
    // and fields (diffuse spread) alongside objects, not just objects.
    use engine::spatial::BasicPanner;
    let layout = SpeakerLayout::five_point_one();
    let mut p = BasicPanner::new(0.0);
    p.prepare(&layout, SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::Y).unwrap();
    scene.create_bed(ChannelLayout::FivePointOne).unwrap();
    scene.create_field().unwrap();

    let frames = 512usize;
    let obj_planes: Vec<Vec<f32>> = vec![vec![1.0; frames]];
    let obj_refs: Vec<&[f32]> = obj_planes.iter().map(|v| v.as_slice()).collect();
    // Bed: FL=1, FR=2, C=3, LFE=4, SL=5, SR=6 as constants.
    let bed_planes: Vec<Vec<f32>> = (0..6).map(|v| vec![v as f32 + 1.0; frames]).collect();
    let bed_refs: Vec<&[f32]> = bed_planes.iter().map(|v| v.as_slice()).collect();
    // Field: impulse at frame 0.
    let mut field_plane = vec![0.0f32; frames];
    field_plane[0] = 1.0;
    let field_refs: Vec<&[f32]> = vec![field_plane.as_slice()];

    let ch = 6usize;
    let mut out = vec![0.0f32; ch * frames];
    let inputs = HybridBlockInputs {
        objects: &obj_refs,
        beds: &bed_refs,
        fields: &field_refs,
    };
    p.process_hybrid_block(&scene, &inputs, frames, &mut out)
        .unwrap();
    assert!(out.iter().all(|x| x.is_finite()));
    // Steady frame: object (front → C ≈ 1.0) + bed C (3.0) → C ≈ 4.0.
    let steady = frames / 2;
    assert!(
        (out[steady * ch + 2] - 4.0).abs() < 0.05,
        "C = object + bed C ({})",
        out[steady * ch + 2]
    );
    // Bed side channels arrive at their own levels.
    assert!(
        (out[steady * ch + 4] - 5.0).abs() < 0.05,
        "SL = bed SL ({})",
        out[steady * ch + 4]
    );
    assert!(
        (out[steady * ch + 5] - 6.0).abs() < 0.05,
        "SR = bed SR ({})",
        out[steady * ch + 5]
    );
    // The field impulse reaches every pan speaker (decorrelated arrivals).
    for spk in [0, 1, 2, 4, 5] {
        let peak = (0..frames)
            .map(|f| out[f * ch + spk].abs())
            .fold(0.0f32, f32::max);
        assert!(peak > 0.1, "field reaches speaker {spk} (peak {peak})");
    }
}
