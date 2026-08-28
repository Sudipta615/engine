//! Acceptance suite for the scene-file format (spec Phase 19 / roadmap
//! Phase 19): Serde-serializable scenes that are **content only** —
//! listener, objects, beds, fields, room — independent of the output
//! speaker layout and the renderer, which stay host choices.
//!
//! The contract this suite pins down:
//!
//! - **Save/load round-trip is lossless** — every scene parameter survives
//!   the JSON file, and a scene loaded back renders *bit-for-bit* the same
//!   as the original (the file is the scene, not an approximation).
//! - **Forward compatibility** — optional fields default via `#[serde(
//!   default)]`, so an older host reading a newer file keeps working.
//! - **Validation** — unknown bed role names, out-of-range gains, non-finite
//!   positions, and class counts over the engine caps are rejected with
//!   typed errors before anything reaches the audio thread.
//! - **Renderer-independence** — the same file renders through both the
//!   binaural head model and a VBAP speaker array: the scene never picks a
//!   renderer.
//! - **Listener orientation is lossless** — stored as the canonical
//!   quaternion; no Euler round-trip drift.

use engine::decode::ChannelLayout;
use engine::spatial::math::{Quat, Vec3};
use engine::spatial::render::{HybridBlockInputs, SpatialRenderer, VbapRenderer};
use engine::spatial::speaker::SpeakerLayout;
use engine::spatial::{
    load_scene_json, save_scene_json, BinauralRenderer, DistanceModel, RenderError, SpatialScene,
};

const SR: u32 = 48_000;

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("freebuff_scene_{tag}_{}.json", std::process::id()));
    p
}

/// A deliberately rich scene: translated + yawed listener, objects with
/// every knob, a 5.1 bed by role names, a field, and a fully-specified room.
fn rich_scene() -> SpatialScene {
    let mut sc = SpatialScene::new(SR);
    sc.listener.position = Vec3::new(1.0, -0.5, 1.7);
    sc.listener.set_orientation(Quat::from_euler_rad(
        137f32.to_radians(),
        12f32.to_radians(),
        0.0,
    ));
    let id = sc.create_audio_object(Vec3::new(2.0, 3.0, 0.0)).unwrap();
    let obj = sc.object_mut(id).unwrap();
    obj.gain = 0.8;
    obj.spread = 0.4;
    obj.room_send = 0.35;
    obj.lfe_send = 0.2;
    obj.enabled = true;
    let id2 = sc.create_audio_object(Vec3::new(-3.0, 1.0, 1.0)).unwrap();
    sc.object_mut(id2).unwrap().gain = 0.5;
    let bed = sc.create_bed(ChannelLayout::FivePointOne).unwrap();
    sc.bed_mut(bed).unwrap().gain = 0.9;
    let f = sc.create_field().unwrap();
    sc.field_mut(f).unwrap().gain = 0.7;
    let room = &mut sc.room;
    room.enabled = true;
    room.width = 10.0;
    room.depth = 8.0;
    room.height = 3.2;
    room.absorption = 0.3;
    room.reflection_order = 2;
    room.rt60_ms = 480.0;
    room.late_mix = 0.4;
    room.speed_of_sound = 344.0;
    sc
}

/// Render one impulse block of a single-object scene through a binaural
/// renderer (the direct path only — no room) for bit-comparison.
fn render_impulse(scene: &SpatialScene, frames: usize) -> Vec<f32> {
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut input = vec![0.0f32; frames];
    input[0] = 1.0;
    let refs = [input.as_slice()];
    let inputs = HybridBlockInputs {
        objects: &refs,
        beds: &[],
        fields: &[],
    };
    let mut out = vec![0.0f32; 2 * frames];
    r.process_hybrid_block(scene, &inputs, frames, &mut out)
        .unwrap();
    out
}

#[test]
fn save_load_round_trip_is_lossless_and_renders_bit_identical() {
    let sc = rich_scene();
    let path = temp_path("roundtrip");
    save_scene_json(&path, &sc).unwrap();
    let back = load_scene_json(&path).unwrap();

    // The config model itself round-trips exactly (listener quaternion
    // included — no Euler drift; see listener_orientation_survives for the
    // dedicated pin).
    assert_eq!(sc.to_config(), back.to_config());

    // Content-level equality.
    assert_eq!(back.sample_rate, SR);
    assert_eq!(back.objects.len(), 2);
    assert_eq!(back.beds.len(), 1);
    assert_eq!(back.fields.len(), 1);
    assert!(back.room.enabled);
    assert_eq!(back.room.width, 10.0);
    assert_eq!(back.room.speed_of_sound, 344.0);

    // And the audio path is bit-identical: load renders exactly what the
    // original would. (One object at a time — a single impulse per scene.)
    for idx in 0..2 {
        let mut a = rich_scene();
        let mut b = back.clone();
        // Keep only object `idx` in both, to avoid summing two objects.
        for i in 0..2 {
            if i != idx {
                a.object_mut(engine::spatial::ObjectId(i)).unwrap().enabled = false;
                b.object_mut(engine::spatial::ObjectId(i)).unwrap().enabled = false;
            }
        }
        let ra = render_impulse(&a, 256);
        let rb = render_impulse(&b, 256);
        assert_eq!(ra, rb, "object {idx} renders identically after round-trip");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn minimal_json_uses_defaults_forward_compatible() {
    // An older host writes a minimal file: every optional field defaults.
    let json = r#"{
        "sample_rate": 48000,
        "listener": { "position": [0.0, 0.0, 0.0] },
        "objects": [ { "position": [1.0, 2.0, 0.0] } ]
    }"#;
    let cfg: config::SpatialSceneConfig = serde_json::from_str(json).unwrap();
    cfg.validate().unwrap();
    let scene = SpatialScene::from_config(&cfg).unwrap();
    let obj = scene.object(engine::spatial::ObjectId(0)).unwrap();
    assert_eq!(obj.gain, 1.0, "default gain");
    assert!(obj.enabled, "default enabled");
    // The listener orientation quaternion defaults to identity.
    let q = scene.listener.orientation;
    assert_eq!([q.x, q.y, q.z, q.w], [0.0, 0.0, 0.0, 1.0]);
    // Saving it back emits the defaults explicitly — the file stays
    // self-describing.
    let path = temp_path("minimal");
    save_scene_json(&path, &scene).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        text.contains("\"gain\": 1.0"),
        "defaults materialized on save"
    );
}

#[test]
fn unknown_bed_role_name_is_rejected() {
    let json = r#"{
        "beds": [ { "channels": ["FL", "QX"] } ]
    }"#;
    let cfg: config::SpatialSceneConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err(), "validate rejects unknown role");
    match SpatialScene::from_config(&cfg) {
        Err(RenderError::InvalidScene) => {}
        other => panic!("expected InvalidScene, got {other:?}"),
    }
}

#[test]
fn capacity_and_range_validation_rejects_bad_scenes() {
    // Too many objects (cap 64).
    let cfg = config::SpatialSceneConfig {
        objects: (0..65)
            .map(|i| config::SpatialObjectConfig {
                position: [i as f32, 0.0, 0.0],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    assert!(cfg.validate().unwrap_err().contains("too many objects"));

    // Out-of-range gain.
    let cfg = config::SpatialSceneConfig {
        objects: vec![config::SpatialObjectConfig {
            gain: 5.0,
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(cfg.validate().unwrap_err().contains("invalid gain"));

    // Non-finite position.
    let cfg = config::SpatialSceneConfig {
        objects: vec![config::SpatialObjectConfig {
            position: [f32::NAN, 0.0, 0.0],
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(cfg.validate().unwrap_err().contains("non-finite"));

    // Bad sample rate.
    let cfg = config::SpatialSceneConfig {
        sample_rate: 100,
        ..Default::default()
    };
    assert!(cfg.validate().unwrap_err().contains("sample_rate"));

    // A corrupt JSON file surfaces as a typed error, not a panic.
    let path = temp_path("corrupt");
    std::fs::write(&path, "{ not json").unwrap();
    let err = load_scene_json(&path).unwrap_err();
    let _ = std::fs::remove_file(&path);
    assert!(err.to_string().contains("json"));
}

#[test]
fn same_file_renders_through_binaural_and_vbap() {
    // The scene file is renderer-independent: content only. Build one scene,
    // save it, and render the loaded copy through both the head model and a
    // 5.1 speaker array.
    let mut sc = SpatialScene::new(SR);
    let id = sc.create_audio_object(Vec3::new(0.0, 2.0, 0.0)).unwrap();
    sc.object_mut(id).unwrap().distance_model = DistanceModel::Linear;
    let path = temp_path("both");
    save_scene_json(&path, &sc).unwrap();
    let loaded = load_scene_json(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    let frames = 256;
    let mut input = vec![0.0f32; frames];
    input[0] = 1.0;
    let refs = [input.as_slice()];
    let inputs = HybridBlockInputs {
        objects: &refs,
        beds: &[],
        fields: &[],
    };

    let mut bin = BinauralRenderer::new(0.0);
    bin.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut out_bin = vec![0.0f32; 2 * frames];
    bin.process_hybrid_block(&loaded, &inputs, frames, &mut out_bin)
        .unwrap();
    assert!(out_bin[1] != 0.0, "binaural rendered the impulse");

    let mut vbap = VbapRenderer::new();
    vbap.prepare(&SpeakerLayout::five_point_one(), SR).unwrap();
    let mut out_vbap = vec![0.0f32; 6 * frames];
    vbap.process_hybrid_block(&loaded, &inputs, frames, &mut out_vbap)
        .unwrap();
    // A front object in 5.1 lands on the center channel.
    assert!(out_vbap[0] != 0.0, "vbap rendered the impulse");
}

#[test]
fn listener_orientation_survives_as_quaternion_not_euler() {
    // A 137° yaw is stored as the canonical quaternion and comes back
    // exactly — the round trip never decomposes into Euler angles.
    let mut sc = SpatialScene::new(SR);
    sc.listener
        .set_orientation(Quat::from_euler_rad(137f32.to_radians(), 0.0, 0.0));
    let cfg = sc.to_config();
    let q = cfg.listener.orientation;
    assert!(
        (Quat::new(q[0], q[1], q[2], q[3]).angle_to(Quat::IDENTITY) - 137f32.to_radians()).abs()
            < 1e-5
    );
    let back = SpatialScene::from_config(&cfg).unwrap();
    let bq = back.listener.orientation;
    assert!(
        Quat::new(bq.x, bq.y, bq.z, bq.w)
            .angle_to(sc.listener.orientation)
            .abs()
            < 1e-6,
        "quaternion is lossless through the file"
    );
}

#[test]
fn empty_scene_and_defaults_round_trip() {
    // The degenerate file — nothing but defaults — loads and saves.
    let json = r#"{}"#;
    let cfg: config::SpatialSceneConfig = serde_json::from_str(json).unwrap();
    cfg.validate().unwrap();
    let scene = SpatialScene::from_config(&cfg).unwrap();
    assert_eq!(scene.objects.len(), 0);
    assert_eq!(scene.beds.len(), 0);
    assert_eq!(scene.fields.len(), 0);
    assert_eq!(scene.sample_rate, 48_000, "default scene rate");
    let path = temp_path("empty");
    save_scene_json(&path, &scene).unwrap();
    let back = load_scene_json(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(back.to_config(), scene.to_config());
}
