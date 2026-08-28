//! Acceptance suite for room acoustics (spec Phase 8 / roadmap Phase 13,
//! §49, §43–44, §55).
//!
//! The contract this suite pins down:
//!
//! - **Early reflections** — the image-source method: a source near a wall
//!   produces a distinct, delayed virtual-source tap at the excess-path
//!   delay `(dist_image − dist_direct)/c`, panned by the same renderer as
//!   the direct path, scaled by the wall reflection coefficient and the
//!   image distance, never on the LFE.
//! - **Participation** — the room is opt-in: `Room::default()` is disabled
//!   (render bit-identical to no room), and an object contributes only via
//!   its `room_send` (a dry object is bit-exact whether the room is on or
//!   off).
//! - **Late field** — the Schroeder tail driven by `room_send` encodes into
//!   the ambisonic bus and decodes as a diffuse source: energy reaches every
//!   pan speaker, the LFE stays silent, and the `late_mix` knob is monotonic
//!   (zero removes the late field entirely).
//! - **Geometry** — higher absorption ⇒ quieter reflections; order 2 adds
//!   energy (24 images vs 6); the whole render is deterministic and finite.

use engine::spatial::math::Vec3;
use engine::spatial::render::SpatialRenderer;
use engine::spatial::{BasicPanner, Room, SpatialScene, SpeakerLayout, VbapRenderer};

const SR: u32 = 48_000;

/// A 12×10×3 m room; the listener sits at its centre and the object 4 m in
/// front of the left wall (image distance 7 m vs 5 m direct).
fn room_config() -> Room {
    Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        rt60_ms: 800.0,
        late_mix: 0.3,
        speed_of_sound: 343.0,
    }
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

/// Render one block; returns the interleaved output.
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

#[test]
fn early_reflection_arrives_at_predicted_delay_on_vbap() {
    // Object 5 m from the listener, 4 m from the left wall. The left-wall
    // image is 7 m away → the reflection arrives 280 samples after the
    // direct sound, on the hard-left pair (SL dominant in 5.1), at
    // room_send × coeff(0.8) × (1/dist 7) × pan(0.924).
    let layout = SpeakerLayout::five_point_one();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let scene = scene_with_impulse_object(Room {
        late_mix: 0.0, // keep the late field out of this timing test
        ..room_config()
    });
    let frames = 512usize;
    let out = render_block(&mut r, &scene, frames, 6);

    assert!(out.iter().all(|v| v.is_finite()));
    // Direct sound at frame 0 (dist 5 → 1/5 gain).
    let sl_direct = out[4];
    let fl_direct = out[0];
    assert!(
        (sl_direct - 0.2 * 0.924).abs() < 0.02,
        "direct SL {}",
        sl_direct
    );
    assert!(
        (fl_direct - 0.2 * 0.383).abs() < 0.02,
        "direct FL {}",
        fl_direct
    );
    // The reflection: 280 samples later, SL ≈ 0.8/7 × 0.924.
    let sl = out[280 * 6 + 4];
    let fl = out[280 * 6];
    assert!(
        (sl - 0.8 / 7.0 * 0.924).abs() < 0.02,
        "reflection SL at 280 = {sl}"
    );
    assert!(
        (fl - 0.8 / 7.0 * 0.383).abs() < 0.015,
        "reflection FL at 280 = {fl}"
    );
    // Nothing on SL before the delay.
    assert!(out[279 * 6 + 4].abs() < 1e-6, "no tap before the delay");
    // The LFE never receives direct or reflected energy.
    for f in 0..frames {
        assert!(out[f * 6 + 3].abs() < 1e-6, "LFE silent at frame {f}");
    }
}

#[test]
fn panner_renders_early_reflections() {
    // The equal-power panner applies the same image-source machinery: the
    // left-wall tap lands 280 samples after the direct sound on the left
    // pair.
    let layout = SpeakerLayout::five_point_one();
    let mut p = BasicPanner::new(0.0);
    p.prepare(&layout, SR).unwrap();
    let scene = scene_with_impulse_object(Room {
        late_mix: 0.0,
        ..room_config()
    });
    let out = render_block(&mut p, &scene, 512, 6);
    let sl = out[280 * 6 + 4];
    let fl = out[280 * 6];
    assert!(sl > 0.05, "panner reflection SL at 280 = {sl}");
    assert!(fl > 0.01, "panner reflection FL at 280 = {fl}");
    assert!(out[279 * 6 + 4].abs() < 1e-6);
    assert!(out.iter().all(|v| v.is_finite()));
}

#[test]
fn room_disabled_restores_bit_exact_output() {
    // Rendering with the room on must not pollute the renderer: after
    // disabling it again, the output is bit-identical to the original
    // no-room render.
    let layout = SpeakerLayout::five_point_one();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let mut scene = scene_with_impulse_object(Room::default()); // disabled
    let a = render_block(&mut r, &scene, 512, 6);

    scene.room = room_config(); // enabled
    let b = render_block(&mut r, &scene, 512, 6);
    assert!(
        a.iter().zip(b.iter()).any(|(x, y)| x != y),
        "enabling the room must change the output"
    );

    scene.room = Room::default(); // disabled again
    let c = render_block(&mut r, &scene, 512, 6);
    assert!(
        a.iter().zip(c.iter()).all(|(x, y)| x == y),
        "disabling the room restores the bit-exact no-room render"
    );
}

#[test]
fn absorption_reduces_reflection_level() {
    let layout = SpeakerLayout::five_point_one();
    let mut low = VbapRenderer::with_smoothing(0.0);
    let mut high = VbapRenderer::with_smoothing(0.0);
    low.prepare(&layout, SR).unwrap();
    high.prepare(&layout, SR).unwrap();
    // Absorption 0.1 → coeff 0.9; absorption 0.9 → coeff 0.1.
    let scene_low = scene_with_impulse_object(Room {
        late_mix: 0.0,
        absorption: 0.1,
        ..room_config()
    });
    let scene_high = scene_with_impulse_object(Room {
        late_mix: 0.0,
        absorption: 0.9,
        ..room_config()
    });
    let out_low = render_block(&mut low, &scene_low, 512, 6);
    let out_high = render_block(&mut high, &scene_high, 512, 6);
    let tap_low = out_low[280 * 6 + 4];
    let tap_high = out_high[280 * 6 + 4];
    // Reflection coefficient 1 − absorption: 0.9 vs 0.1 → ~9× apart.
    assert!(
        tap_low > tap_high * 4.0,
        "higher absorption ⇒ quieter reflection ({tap_low} vs {tap_high})"
    );
    assert!((tap_low - 0.9 / 7.0 * 0.924).abs() < 0.02);
    assert!((tap_high - 0.1 / 7.0 * 0.924).abs() < 0.01);
}

#[test]
fn second_order_adds_more_reflection_energy() {
    let layout = SpeakerLayout::five_point_one();
    let mut r1 = VbapRenderer::with_smoothing(0.0);
    let mut r2 = VbapRenderer::with_smoothing(0.0);
    r1.prepare(&layout, SR).unwrap();
    r2.prepare(&layout, SR).unwrap();
    let frames = 4096usize;
    let scene_1 = scene_with_impulse_object(Room {
        late_mix: 0.0,
        reflection_order: 1,
        ..room_config()
    });
    let scene_2 = scene_with_impulse_object(Room {
        late_mix: 0.0,
        reflection_order: 2,
        ..room_config()
    });
    let out_1 = render_block(&mut r1, &scene_1, frames, 6);
    let out_2 = render_block(&mut r2, &scene_2, frames, 6);
    // Early-reflection energy = everything after the direct impulse at
    // frame 0, summed over the block.
    let energy = |out: &[f32]| -> f32 {
        out.chunks_exact(6)
            .skip(1)
            .map(|f| f.iter().map(|v| v * v).sum::<f32>())
            .sum()
    };
    let e1 = energy(&out_1);
    let e2 = energy(&out_2);
    assert!(
        e2 > e1 * 1.2,
        "order 2 adds reflection energy ({e1} → {e2})"
    );
}

#[test]
fn objects_without_room_send_stay_dry() {
    // A dry object (room_send = 0) is bit-exact whether the room is enabled
    // or not — the per-object participation seam (§15).
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let mut scene_off = scene_with_impulse_object(Room::default());
    let obj = scene_off.objects.active_indices()[0];
    scene_off
        .object_mut(engine::spatial::ObjectId(obj))
        .unwrap()
        .room_send = 0.0;
    let a = render_block(&mut r, &scene_off, 512, 12);

    let mut scene_on = scene_off.clone();
    scene_on.room = room_config();
    let b = render_block(&mut r, &scene_on, 512, 12);
    assert!(
        a.iter().zip(b.iter()).all(|(x, y)| x == y),
        "room_send 0 stays dry even with the room enabled"
    );
}

#[test]
fn late_field_is_diffuse_and_lfe_free() {
    // With late_mix = 1, the Schroeder tail encodes into the ambisonic bus
    // and decodes as a diffuse source: every pan speaker receives energy at
    // its own decorrelation delay, the LFE stays silent, and the output is
    // bounded and finite.
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let scene = scene_with_impulse_object(Room {
        late_mix: 1.0,
        absorption: 0.5,
        ..room_config()
    });
    let ch = 12usize;
    let block = 4096usize;
    let mut all: Vec<f32> = Vec::new();
    for b in 0..12 {
        let mut input = vec![0.0f32; block];
        if b == 0 {
            input[0] = 1.0; // the single impulse of the whole run
        }
        let mut out = vec![0.0f32; ch * block];
        r.process_block(&scene, &[&input], block, &mut out).unwrap();
        all.extend(out);
    }
    assert!(all.iter().all(|v| v.is_finite()));
    // Every pan speaker sees late-field energy (impulse → tail → decode →
    // per-speaker decorrelation).
    for spk in 0..ch {
        if spk == 3 {
            continue; // LFE
        }
        let peak = all
            .chunks_exact(ch)
            .map(|f| f[spk].abs())
            .fold(0.0f32, f32::max);
        assert!(
            peak > 1e-4,
            "late field reaches speaker {spk} (peak {peak})"
        );
    }
    // LFE never receives field energy.
    assert!(
        all.chunks_exact(ch).all(|f| f[3].abs() < 1e-6),
        "LFE stays silent"
    );
    let max_abs = all.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    assert!(max_abs < 1.0, "bounded output (max {max_abs})");
}

#[test]
fn late_mix_zero_removes_late_field() {
    let layout = SpeakerLayout::five_point_one();
    let mut dry = VbapRenderer::with_smoothing(0.0);
    let mut wet = VbapRenderer::with_smoothing(0.0);
    dry.prepare(&layout, SR).unwrap();
    wet.prepare(&layout, SR).unwrap();
    let frames = 4096usize;
    let scene_dry = scene_with_impulse_object(Room {
        late_mix: 0.0,
        ..room_config()
    });
    let scene_wet = scene_with_impulse_object(Room {
        late_mix: 0.5,
        ..room_config()
    });
    let out_dry = render_block(&mut dry, &scene_dry, frames, 6);
    let out_wet = render_block(&mut wet, &scene_wet, frames, 6);

    // Order-1 reflections all land before ~2 000 samples; beyond that the
    // dry render is silent while the wet render carries the decaying tail.
    let late_frame = 3_000usize;
    let late_dry: f32 = out_dry
        .chunks_exact(6)
        .skip(late_frame)
        .map(|f| f.iter().map(|v| v.abs()).sum::<f32>())
        .take(500)
        .sum();
    let late_wet: f32 = out_wet
        .chunks_exact(6)
        .skip(late_frame)
        .map(|f| f.iter().map(|v| v.abs()).sum::<f32>())
        .take(500)
        .sum();
    assert_eq!(late_dry, 0.0, "no late field at late_mix 0");
    assert!(
        late_wet > 1e-3,
        "late field present at late_mix 0.5 ({late_wet})"
    );
}

#[test]
fn room_render_is_deterministic_and_finite() {
    // The full hybrid (object + bed + field + room) rendered through two
    // identically-prepared renderers is bit-for-bit identical and finite.
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r1 = VbapRenderer::with_smoothing(0.0);
    let mut r2 = VbapRenderer::with_smoothing(0.0);
    r1.prepare(&layout, SR).unwrap();
    r2.prepare(&layout, SR).unwrap();
    let mut scene = scene_with_impulse_object(room_config());
    scene
        .create_bed(engine::decode::ChannelLayout::FivePointOne)
        .unwrap();
    scene.create_field().unwrap();

    let ch = 12usize;
    let frames = 4096usize;
    let input = impulse_plane(frames);
    let bed_planes: Vec<Vec<f32>> = (0..6).map(|_| vec![0.25f32; frames]).collect();
    let bed_refs: Vec<&[f32]> = bed_planes.iter().map(|v| v.as_slice()).collect();
    let field_plane = impulse_plane(frames);
    let field_refs: Vec<&[f32]> = vec![field_plane.as_slice()];
    let inputs = engine::spatial::render::HybridBlockInputs {
        objects: &[&input],
        beds: &bed_refs,
        fields: &field_refs,
    };
    let run = |r: &mut VbapRenderer| -> Vec<f32> {
        let mut out = vec![0.0f32; ch * frames];
        r.process_hybrid_block(&scene, &inputs, frames, &mut out)
            .unwrap();
        out
    };
    let a = run(&mut r1);
    let b = run(&mut r2);
    assert!(a.iter().all(|v| v.is_finite()));
    assert!(
        a.iter().zip(b.iter()).all(|(x, y)| x == y),
        "room render is deterministic"
    );
}
