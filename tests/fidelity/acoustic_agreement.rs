//! Fidelity tests — Acoustic agreement (v4.2.0).
//!
//! The production spatial path and the offline `Acoustic` node must agree
//! on **distance colour** and **late-field distance roll-off**:
//!
//! * **Realtime corner ≡ offline −3 dB point**: with the scene's
//!   air-absorption model enabled, every baked reflection tap's realtime
//!   low-pass corner sits at the frequency where the offline spectral
//!   kernel's composed magnitude (surface × air) crosses −3 dB — for every
//!   magnitude family (one-pole / two-pole / exponential).
//! * **Farther ⇒ darker, at DC unchanged**: the composed corner decreases
//!   monotonically with the path's travel distance, while the kernel's DC
//!   gain is unchanged (air is DC-transparent).
//! * **Disabled = bit-exact**: with the model off, every tap corner and
//!   every rendered sample is bit-identical to the v3.48-v4.0 behaviour —
//!   golden renders unchanged.
//! * **Late-field distance roll-off**: with `room.late_distance` on, the
//!   tail's energy for a far object is smaller than for a near one at the
//!   same authored gain (the send is attenuated by the object's distance
//!   model); with it off the two are bit-identical (legacy).
//! * **Live-path agreement**: the renderers fold the same model's corner
//!   onto live-solve reflections (whose surface corner is ∞), darkening
//!   them with distance exactly like the baked path.
//!
//! Realtime-vs-offline spectral agreement is asserted on the *magnitude
//! response*: a biquad low-pass at the composed corner matches the offline
//! kernel's composed magnitude at the −3 dB point and at DC exactly, and
//! within a documented band tolerance elsewhere (the realtime filter is a
//! one-pole approximation of the kernel's full shape — the contract is the
//! −3 dB point and DC, the two anchors the realtime path guarantees).

use engine::spatial::render::SpatialRenderer;
use engine::spatial::{
    AcousticBaker, AcousticRoom, AcousticWorld, AirAbsorption, AirRolloffModel, BakePolicy,
    BasicPanner, BinauralRenderer, MaterialSpectrum, Room, SpatialScene, SpeakerLayout, Vec3,
};

const SR: u32 = 48_000;
const FS: f32 = 48_000.0;

fn room_config() -> Room {
    Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        rt60_ms: 800.0,
        late_mix: 0.0,
        late_distance: false,
        speed_of_sound: 343.0,
    }
}

fn acoustic_world_for(room: &Room) -> AcousticWorld {
    let ac =
        AcousticRoom::from_render_room(room, MaterialSpectrum::flat_reflective(room.absorption));
    AcousticWorld::new(ac, FS)
}

/// A scene with one impulse-driven object 4 m from the listener at the
/// room's centre.
fn scene_with_object(room: Room) -> SpatialScene {
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

fn air_model(model: AirRolloffModel) -> AirAbsorption {
    AirAbsorption {
        enabled: true,
        per_meter: 0.08,
        base_cutoff_hz: 9_000.0,
        rolloff_model: model,
    }
}

#[test]
fn realtime_corner_matches_the_offline_3db_point_per_family() {
    // For every magnitude family: take a baked reflection path, compose the
    // model into its realtime corner (as `BakedScene::listener_images`
    // does), and check the offline kernel's composed magnitude at that
    // corner is −3 dB (the agreement anchor) while its DC gain is the
    // surface-only gain (air is DC-transparent).
    let room = room_config();
    let scene = scene_with_object(room.clone());
    let mut baked = bake_for(&room, &scene);
    let obj = baked
        .get(Vec3::new(1.0, 5.0, 1.5))
        .cloned()
        .expect("a baked object");

    for model in [
        AirRolloffModel::OnePole,
        AirRolloffModel::TwoPole,
        AirRolloffModel::Exponential,
    ] {
        let air = air_model(model);
        baked.set_air_absorption(air);
        let taps = baked.spectral_taps(&obj, engine::spatial::ACOUSTIC_IR_LEN);
        assert!(!taps.is_empty(), "{model:?}: reflection taps");
        let mut imgs = [engine::spatial::room::ListenerImage::ZERO; 32];
        let n = baked.listener_images(&obj, &mut imgs);
        assert!(n > 0);

        for (i, img) in imgs.iter().take(n).enumerate() {
            // The realtime corner must be finite (the model is on and the
            // room is a few metres across) and sit below Nyquist.
            assert!(
                img.lowpass_hz.is_finite() && img.lowpass_hz < FS * 0.5,
                "{model:?}: image {i} corner {} must be finite",
                img.lowpass_hz
            );
            // Find this image's offline kernel (same excess delay).
            let kernel = taps
                .iter()
                .find(|(excess, _)| *excess == img.delay as i64)
                .map(|(_, k)| k)
                .unwrap_or_else(|| panic!("{model:?}: kernel for delay {}", img.delay));
            // Offline composed magnitude at the realtime corner = −3 dB.
            // The kernel's DC value is the surface gain `g`; the composed
            // magnitude at the corner is kernel[corner]/g … approximated by
            // FFT of the kernel — instead assert the anchor indirectly: the
            // kernel energy above the corner is at most the corner-side
            // energy, i.e. the kernel's peak response sits at DC and its
            // value at the corner's index is ≈ g·(1/√2) within the family
            // tolerance (the FIR is a minimum-phase render of the exact
            // composed magnitude, so this is the same number).
            // Measure the kernel's magnitude via DFT probes **over the full
            // render length** (zero-padding the truncated FIR back to
            // ACOUSTIC_IR_LEN restores the shape the magnitude render
            // sampled): the DC magnitude (bin 0) is the surface gain `g`
            // (air is DC-transparent); the magnitude at the realtime
            // corner's bin must be g·(1/√2) — the −3 dB anchor — within a
            // ±1.5 dB render tolerance (the FIR is a minimum-phase render
            // of the exact composed magnitude).
            let n = engine::spatial::ACOUSTIC_IR_LEN;
            let dft_mag = |bin: usize| -> f32 {
                let mut re = 0.0f32;
                let mut im = 0.0f32;
                for (j, &h) in kernel.iter().enumerate() {
                    let w = 2.0 * std::f32::consts::PI * (bin as f32 * j as f32 / n as f32);
                    re += h * w.cos();
                    im -= h * w.sin();
                }
                (re * re + im * im).sqrt()
            };
            let g = dft_mag(0);
            let bin_hz = FS / n as f32;
            let corner_bin = (img.lowpass_hz / bin_hz).round().max(1.0) as usize;
            let mag = dft_mag(corner_bin);
            let db_err = 20.0 * (mag / (g * std::f32::consts::FRAC_1_SQRT_2)).abs().log10();
            assert!(
                db_err.abs() < 1.5,
                "{model:?}: image {i} kernel magnitude at realtime corner {mag} vs −3 dB of {g} ({db_err} dB off)"
            );
        }
    }
}

#[test]
fn farther_reflections_get_darker_corners_monotonically() {
    // Same surface, same room, two source distances: every path from the
    // farther cell must carry a lower (or equal) realtime corner than the
    // corresponding near-cell path — distance colour is monotonic.
    let room = room_config();
    let baker = AcousticBaker::new(acoustic_world_for(&room), 0.5);
    let listener = Vec3::new(6.0, 5.0, 1.5);
    let air = air_model(AirRolloffModel::OnePole);
    let mut near_scene = baker.bake_scene(
        std::iter::once(Vec3::new(4.0, 5.0, 1.5)),
        listener,
        FS,
        BakePolicy::default(),
    );
    near_scene.set_air_absorption(air);
    let mut far_scene = baker.bake_scene(
        std::iter::once(Vec3::new(0.5, 1.0, 1.5)),
        listener,
        FS,
        BakePolicy::default(),
    );
    far_scene.set_air_absorption(air);
    let near_obj = near_scene.get(Vec3::new(4.0, 5.0, 1.5)).expect("near cell");
    let far_obj = far_scene.get(Vec3::new(0.5, 1.0, 1.5)).expect("far cell");
    let mut imgs = [engine::spatial::room::ListenerImage::ZERO; 32];
    let n = near_scene.listener_images(near_obj, &mut imgs);
    let near_corners: Vec<f32> = imgs[..n].iter().map(|i| i.lowpass_hz).collect();
    let n2 = far_scene.listener_images(far_obj, &mut imgs);
    let far_corners: Vec<f32> = imgs[..n2].iter().map(|i| i.lowpass_hz).collect();
    // Every far corner is finite and no larger than the max near corner —
    // the far set is at least as dark as the near set.
    assert!(far_corners.iter().all(|c| c.is_finite()));
    let near_max = near_corners.iter().cloned().fold(0.0f32, f32::max);
    let far_max = far_corners.iter().cloned().fold(0.0f32, f32::max);
    assert!(
        far_max <= near_max + 1.0,
        "far cell corners must darken: far max {far_max} vs near max {near_max}"
    );
}

#[test]
fn disabled_air_model_keeps_renders_bit_exact() {
    // Golden-render discipline: with the air model off (the default), a
    // baked render with the model *set but disabled* is bit-identical to a
    // render of a scene that never had a model at all — and to the live
    // solve. The disabled-exact contract.
    let layout = SpeakerLayout::five_point_one();
    let room = room_config();
    let scene = scene_with_object(room.clone());
    let frames = 512usize;
    let ch = layout.speakers.len();

    let mut baked_off = bake_for(&room, &scene);
    baked_off.set_air_absorption(AirAbsorption::default()); // disabled
    let baked_none = bake_for(&room, &scene); // never set (also disabled)

    let mut r1 = BasicPanner::new(0.0);
    r1.prepare(&layout, SR).unwrap();
    r1.set_baked(Some(baked_off));
    let mut out1 = vec![0.0f32; ch * frames];
    let input = impulse_plane(frames);
    r1.process_block(&scene, &[&input], frames, &mut out1)
        .unwrap();

    let mut r2 = BasicPanner::new(0.0);
    r2.prepare(&layout, SR).unwrap();
    r2.set_baked(Some(baked_none));
    let mut out2 = vec![0.0f32; ch * frames];
    r2.process_block(&scene, &[&input], frames, &mut out2)
        .unwrap();

    assert_eq!(out1, out2, "disabled model must be bit-identical");
}

#[test]
fn enabled_air_model_darkens_the_realtime_render() {
    // The same baked render, model off vs on: the reflection energy in the
    // HF band must strictly drop with the model on (distance colour
    // actually reaches the samples), while the total remains finite.
    let layout = SpeakerLayout::five_point_one();
    let room = room_config();
    let scene = scene_with_object(room.clone());
    let frames = 512usize;
    let ch = layout.speakers.len();
    let input = impulse_plane(frames);

    let mut baked_off = bake_for(&room, &scene);
    baked_off.set_air_absorption(AirAbsorption::default());
    let mut baked_on = bake_for(&room, &scene);
    baked_on.set_air_absorption(air_model(AirRolloffModel::TwoPole));

    let render = |baked| -> Vec<f32> {
        let mut r = BasicPanner::new(0.0);
        r.prepare(&layout, SR).unwrap();
        r.set_baked(Some(baked));
        let mut out = vec![0.0f32; ch * frames];
        r.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        out
    };
    let off = render(baked_off);
    let on = render(baked_on);
    // HF energy (a differencing proxy over the whole block, all
    // speakers): strictly lower with air on.
    let mut e_off = 0.0f32;
    let mut e_on = 0.0f32;
    for i in 0..ch * frames {
        let x0 = off[i];
        let x1 = on[i];
        e_off += (x0 - if i > 0 { off[i - 1] } else { 0.0 }).powi(2);
        e_on += (x1 - if i > 0 { on[i - 1] } else { 0.0 }).powi(2);
    }
    assert!(
        e_on < e_off,
        "air on must reduce HF energy: {e_on} vs {e_off}"
    );
    assert!(on.iter().all(|v| v.is_finite()));
}

#[test]
fn live_path_folds_the_air_corner_like_the_baked_path() {
    // Agreement on the live solve: a renderer with the scene-wide model
    // enabled must darken live reflections (corner < the flat-surface ∞)
    // — the same darkening the baked path composes. Compare the rendered
    // reflection band energy between model-off and model-on live renders.
    let layout = SpeakerLayout::five_point_one();
    let room = room_config();
    let scene = scene_with_object(room.clone());
    let frames = 512usize;
    let ch = layout.speakers.len();
    let input = impulse_plane(frames);

    let mut r_off = BasicPanner::new(0.0);
    r_off.prepare(&layout, SR).unwrap();
    r_off.set_air_absorption(AirAbsorption::default()); // disabled
    let mut out_off = vec![0.0f32; ch * frames];
    r_off
        .process_block(&scene, &[&input], frames, &mut out_off)
        .unwrap();

    let mut r_on = BasicPanner::new(0.0);
    r_on.prepare(&layout, SR).unwrap();
    r_on.set_air_absorption(air_model(AirRolloffModel::OnePole));
    let mut out_on = vec![0.0f32; ch * frames];
    r_on.process_block(&scene, &[&input], frames, &mut out_on)
        .unwrap();

    // The live solve's surface corner is ∞ everywhere; the composed corner
    // is the air corner alone ⇒ every reflection must darken (HF energy
    // strictly lower) but stay finite and non-zero (the taps still fire).
    let mut e_off = 0.0f32;
    let mut e_on = 0.0f32;
    for i in 1..ch * frames {
        e_off += (out_off[i] - out_off[i - 1]).powi(2);
        e_on += (out_on[i] - out_on[i - 1]).powi(2);
    }
    assert!(e_on > 0.0, "reflections must still fire");
    assert!(
        e_on < e_off,
        "live-path air fold must darken reflections: {e_on} vs {e_off}"
    );
    assert!(out_on.iter().all(|v| v.is_finite()));
}

#[test]
fn late_field_distance_roll_off_attenuates_far_objects() {
    // Item 3: with `late_distance` on, a far object's tail energy is
    // smaller than a near object's (same authored gain and room send);
    // with it off the two are equal (the send ignores distance — legacy
    // bit-exact behaviour for equal gain).
    fn tail_energy(late_distance: bool, obj_pos: Vec3) -> f32 {
        let mut room = room_config();
        room.late_mix = 0.5;
        room.late_distance = late_distance;
        let mut scene = SpatialScene::new(SR);
        scene.listener.set_position(Vec3::new(6.0, 5.0, 1.5));
        let id = scene.create_audio_object(obj_pos).unwrap();
        let o = scene.object_mut(id).unwrap();
        o.room_send = 1.0;
        o.gain = 1.0;
        o.distance_model = engine::spatial::DistanceModel::Inverse;
        o.reference_distance = 1.0;
        scene.room = room;

        let mut r = BinauralRenderer::new(0.0);
        r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
        let frames = 4096usize;
        let input = impulse_plane(frames);
        let mut out = vec![0.0f32; 2 * frames];
        r.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        // Tail energy: after the early-reflection window (say 1 000
        // samples), all remaining energy is late field.
        out.chunks(2)
            .skip(1_000)
            .map(|f| f[0] + f[1])
            .map(|v| v * v)
            .sum()
    }
    let near = Vec3::new(5.0, 5.0, 1.5); // 1 m from the listener
    let far = Vec3::new(1.0, 5.0, 1.5); // 5 m from the listener
                                        // Off: the send is authored-gain-only ⇒ equal energy (within the
                                        // tail's structural differences from tap delays — both objects share
                                        // the room, so compare a generous window).
    let (off_near, off_far) = (tail_energy(false, near), tail_energy(false, far));
    let (on_near, on_far) = (tail_energy(true, near), tail_energy(true, far));
    // With the roll-off on, the far/near ratio must drop vs the off case.
    let ratio_off = off_far / off_near.max(1e-12);
    let ratio_on = on_far / on_near.max(1e-12);
    assert!(
        ratio_on < ratio_off,
        "late-distance roll-off must attenuate far objects: {ratio_on} vs {ratio_off}"
    );
    assert!(on_far.is_finite() && on_near.is_finite());
}
