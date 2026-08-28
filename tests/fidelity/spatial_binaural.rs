//! Acceptance suite for the binaural renderer (spec Phase 9 / roadmap
//! Phase 14, Part VII §47–48, §62, §136).
//!
//! The contract this suite pins down:
//!
//! - **ITD** — the Woodworth formula: a source at the ear axis (+90°)
//!   delays the contralateral ear by `(a/c)(π/2 + 1)` ≈ 31.5 samples at
//!   48 kHz; front/rear sources have zero ITD.
//! - **Head shadow** — the Duda-Martens shelf: the ipsilateral ear *boosts*
//!   (α→2) while the contralateral ear is shadowed (α→0.1), so a hard-right
//!   source is louder at the right ear.
//! - **Symmetry** — mirroring a source across the median plane swaps the
//!   ears exactly (the invariant of the head model, not constant power).
//! - **Beds** — semantic-role fold: FL routes to the left-ear cues, the LFE
//!   role folds at `1/√2` into both ears.
//! - **Fields & late field** — diffuse content decodes onto the virtual
//!   8-speaker ring: both ears receive decorrelated energy, equal totals
//!   (the ring is symmetric), never a phantom.
//! - **Room** — image-source reflections are binauralized: each ear hears
//!   the reflection at `excess_path + ITD(ear)`.
//! - **Listener rotation** — a world-fixed object moves with the head
//!   (world +X sounds front when the listener faces +X).
//! - **Spread** — widening reduces the effective interaural delay (the
//!   image's lateral pull weakens).
//! - **Determinism** — identical scenes through fresh renderers are
//!   bit-for-bit identical and finite.

use engine::decode::ChannelLayout;
use engine::spatial::math::Vec3;
use engine::spatial::render::{HybridBlockInputs, SpatialRenderer};
use engine::spatial::{BinauralRenderer, Ear, Room, SpatialScene, SpeakerLayout};
use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};

const SR: u32 = 48_000;

fn impulse_plane(frames: usize) -> Vec<f32> {
    let mut p = vec![0.0f32; frames];
    p[0] = 1.0;
    p
}

/// Argmax frame and peak amplitude of one ear (interleaved 2-ch output).
fn ear_argmax(out: &[f32], frames: usize, ear: usize) -> (usize, f32) {
    let mut best = (0usize, 0.0f32);
    for f in 0..frames {
        let v = out[f * 2 + ear].abs();
        if v > best.1 {
            best = (f, v);
        }
    }
    best
}

fn render_impulse(r: &mut BinauralRenderer, scene: &SpatialScene, frames: usize) -> Vec<f32> {
    let input = impulse_plane(frames);
    let mut out = vec![0.0f32; 2 * frames];
    r.process_block(scene, &[&input], frames, &mut out).unwrap();
    out
}

#[test]
fn front_center_renders_balanced_and_unity() {
    // A front object (distance 1 m) reaches both ears at unity: the shelf's
    // DC gain is exactly 1 for any α and the ITD is zero. The shelf pole
    // (|a1| ≈ 0.92) makes DC convergence geometric, so assert after a warm
    // up.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::Y).unwrap();
    let frames = 256usize;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 2 * frames];
    r.process_block(&scene, &[&input], frames, &mut out)
        .unwrap();
    for f in 200..frames {
        assert!(
            (out[f * 2] - 1.0).abs() < 1e-3,
            "left unity at {f}: {}",
            out[f * 2]
        );
        assert!(
            (out[f * 2 + 1] - 1.0).abs() < 1e-3,
            "right unity at {f}: {}",
            out[f * 2 + 1]
        );
        assert!(
            (out[f * 2] - out[f * 2 + 1]).abs() < 1e-6,
            "front is balanced"
        );
    }
}

#[test]
fn hard_right_produces_woodworth_itd_and_head_shadow() {
    // A source at +90° (ear axis): the left (contralateral) ear hears it
    // ~31.5 samples later and through the shadow (α→0.1); the right ear
    // hears it immediately with the diffraction boost (α→2).
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::X).unwrap();
    let frames = 256usize;
    let out = render_impulse(&mut r, &scene, frames);

    let (l_peak, l_amp) = ear_argmax(&out, frames, 0);
    let (r_peak, r_amp) = ear_argmax(&out, frames, 1);
    let expect = r.itd_samples(FRAC_PI_2, Ear::Left);
    assert!(
        (l_peak as f32 - r_peak as f32 - expect).abs() < 2.0,
        "contralateral delay = Woodworth ITD: L@{l_peak} R@{r_peak} expect {expect:.1}"
    );
    // The ear at the source is louder (boost vs shadow, measured at the
    // impulse peaks).
    assert!(
        r_amp > 3.0 * l_amp,
        "ipsilateral boost vs contralateral shadow: R {r_amp} vs L {l_amp}"
    );
    assert!(out.iter().all(|v| v.is_finite()));
}

#[test]
fn mirrored_sources_swap_ears_exactly() {
    // +45° and −45° are mirror images: fresh renderers must swap L/R
    // bit-for-bit (the head model's exact symmetry invariant).
    let render = |dir: Vec3| -> Vec<f32> {
        let mut r = BinauralRenderer::new(0.0);
        r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
        let mut scene = SpatialScene::new(SR);
        scene.create_audio_object(dir).unwrap();
        render_impulse(&mut r, &scene, 256)
    };
    let a = render(Vec3::new(1.0, 1.0, 0.0).normalized().unwrap());
    let b = render(Vec3::new(-1.0, 1.0, 0.0).normalized().unwrap());
    for f in 0..256 {
        assert!(
            (a[f * 2] - b[f * 2 + 1]).abs() < 1e-6,
            "L(+45) vs R(−45) at {f}"
        );
        assert!(
            (a[f * 2 + 1] - b[f * 2]).abs() < 1e-6,
            "R(+45) vs L(−45) at {f}"
        );
    }
}

#[test]
fn stereo_bed_folds_by_semantic_role() {
    // A stereo bed's FL plane must arrive at the left-ear cues (earlier +
    // louder at HF); the LFE channel folds at exactly 1/√2 into both ears.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_bed(ChannelLayout::Stereo).unwrap();
    let frames = 256usize;
    let fl = impulse_plane(frames);
    let fr = vec![0.0f32; frames];
    let refs: Vec<&[f32]> = vec![fl.as_slice(), fr.as_slice()];
    let inputs = HybridBlockInputs {
        objects: &[],
        beds: &refs,
        fields: &[],
    };
    let mut out = vec![0.0f32; 2 * frames];
    r.process_hybrid_block(&scene, &inputs, frames, &mut out)
        .unwrap();

    let (l_peak, l_amp) = ear_argmax(&out, frames, 0);
    let (r_peak, r_amp) = ear_argmax(&out, frames, 1);
    assert!(
        l_peak < r_peak,
        "FL arrives at the left ear first (L@{l_peak} R@{r_peak})"
    );
    assert!(
        l_amp > 1.5 * r_amp,
        "FL is louder at the left ear (L {l_amp} vs R {r_amp})"
    );

    // LFE role: a two-point-one bed's LFE plane folds at 1/√2 to both ears.
    // Fresh renderer — persistent bed-ring state must not leak across
    // scenes.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_bed(ChannelLayout::TwoPointOne).unwrap();
    let lfe = impulse_plane(frames);
    let z = vec![0.0f32; frames];
    let refs: Vec<&[f32]> = vec![z.as_slice(), z.as_slice(), lfe.as_slice()];
    let inputs = HybridBlockInputs {
        objects: &[],
        beds: &refs,
        fields: &[],
    };
    let mut out = vec![0.0f32; 2 * frames];
    r.process_hybrid_block(&scene, &inputs, frames, &mut out)
        .unwrap();
    let fold = std::f32::consts::FRAC_1_SQRT_2;
    for f in 0..frames {
        assert!(
            (out[f * 2] - out[f * 2 + 1]).abs() < 1e-6,
            "LFE fold is equal on both ears at {f}"
        );
        if out[f * 2] != 0.0 {
            assert!(
                (out[f * 2] - fold).abs() < 1e-4,
                "LFE folds at 1/√2: {}",
                out[f * 2]
            );
        }
    }
}

#[test]
fn field_is_diffuse_with_equal_ear_energy() {
    // An impulse field decodes onto the virtual 8-speaker ring and is
    // head-modeled: both ears receive decorrelated energy with *equal
    // totals* (the ring is symmetric), spread over time — not a phantom.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_field().unwrap();
    let frames = 1024usize;
    let input = impulse_plane(frames);
    let refs: Vec<&[f32]> = vec![input.as_slice()];
    let inputs = HybridBlockInputs {
        objects: &[],
        beds: &[],
        fields: &refs,
    };
    let mut out = vec![0.0f32; 2 * frames];
    r.process_hybrid_block(&scene, &inputs, frames, &mut out)
        .unwrap();
    assert!(out.iter().all(|v| v.is_finite()));
    let energy = |ear: usize| -> f32 {
        (0..frames)
            .map(|f| out[f * 2 + ear] * out[f * 2 + ear])
            .sum()
    };
    let e_l = energy(0);
    let e_r = energy(1);
    assert!(e_l > 1e-3, "left ear receives field energy ({e_l})");
    assert!(e_r > 1e-3, "right ear receives field energy ({e_r})");
    // The fractional ITD interpolation weights the two ears' spectra very
    // slightly differently, so equality holds within a small tolerance.
    assert!(
        (e_l - e_r).abs() / (e_l + e_r).max(1e-9) < 0.02,
        "diffuse field has equal ear energy (L {e_l} vs R {e_r})"
    );
    // Decorrelated: the two ear signals are not identical sample by sample.
    assert!(
        (0..frames).any(|f| (out[f * 2] - out[f * 2 + 1]).abs() > 1e-3),
        "the field is decorrelated between the ears"
    );
}

#[test]
fn room_reflection_delays_per_ear_by_woodworth() {
    // The classic room scene (listener at the room centre, object 4 m from
    // the left wall): the left-wall image arrives 280 samples after the
    // direct sound on the ipsilateral (left) ear and 280 + ITD on the
    // contralateral (right) ear.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.listener.set_position(Vec3::new(6.0, 5.0, 1.5));
    let id = scene.create_audio_object(Vec3::new(1.0, 5.0, 1.5)).unwrap();
    scene.object_mut(id).unwrap().room_send = 1.0;
    scene.room = Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        rt60_ms: 800.0,
        late_mix: 0.0, // keep the late field out of the timing window
        speed_of_sound: 343.0,
    };
    let frames = 512usize;
    let out = render_impulse(&mut r, &scene, frames);

    // Reflection window: search for the left-wall image's tap around the
    // predicted 280 samples. (Other images land at 116 and 865; the window
    // keeps them out of the measurement.)
    let win_peak = |ear: usize, lo: usize, hi: usize| -> (usize, f32) {
        let mut best = (lo, 0.0f32);
        for f in lo..hi {
            let v = out[f * 2 + ear].abs();
            if v > best.1 {
                best = (f, v);
            }
        }
        best
    };
    let (l_ref, l_amp) = win_peak(0, 250, 330);
    let (r_ref, r_amp) = win_peak(1, 250, 360);
    let itd = r.itd_samples(FRAC_PI_2, Ear::Left);
    assert!(
        (l_ref as f32 - 280.0).abs() < 2.0,
        "left ear hears the left-wall reflection at the excess path (L@{l_ref})"
    );
    assert!(
        (r_ref as f32 - l_ref as f32 - itd).abs() < 2.0,
        "right ear hears it ITD later (L@{l_ref} R@{r_ref} expect +{itd:.1})"
    );
    // The reflection is real: it beats the direct-sound tail in the window.
    assert!(l_amp > 1e-3, "reflection present (L {l_amp})");
    assert!(r_amp > 1e-4, "reflection present (R {r_amp})");
    assert!(out.iter().all(|v| v.is_finite()));
}

#[test]
fn listener_rotation_moves_the_image() {
    // World +X with the listener facing +X (yaw +90°) must sound front
    // (balanced, no ITD); with no rotation the same object sounds right
    // (R leads L by the Woodworth ITD).
    let render = |yaw: f32| -> (usize, usize, f32) {
        let mut r = BinauralRenderer::new(0.0);
        r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
        let mut scene = SpatialScene::new(SR);
        scene
            .listener
            .set_orientation(engine::spatial::Quat::from_euler_rad(yaw, 0.0, 0.0));
        scene.create_audio_object(Vec3::X).unwrap();
        let frames = 256usize;
        let out = render_impulse(&mut r, &scene, frames);
        let (l, la) = ear_argmax(&out, frames, 0);
        let (r, _) = ear_argmax(&out, frames, 1);
        (l, r, la)
    };
    let (l_front, r_front, amp) = render(FRAC_PI_2);
    assert!(
        (l_front as i64 - r_front as i64).abs() <= 1,
        "yawed listener hears a front image (L@{l_front} R@{r_front})"
    );
    assert!(amp > 1e-3, "front image present");
    let (l_right, r_right, _) = render(0.0);
    assert!(
        r_right < l_right,
        "un-rotated listener hears +X on the right (R@{r_right} < L@{l_right})"
    );
}

#[test]
fn spread_reduces_effective_interaural_delay() {
    // A +45° source at spread 0 places the ears' energy at their own ITDs;
    // at spread 1 the ring directions (each with its own ITD) blur the cue,
    // so the temporal-energy-centroid *difference* between the ears shrinks.
    let centroid = |ear: usize, out: &[f32], frames: usize| -> f32 {
        let mut num = 0.0f32;
        let mut den = 0.0f32;
        for f in 0..frames {
            let v = out[f * 2 + ear];
            num += f as f32 * v * v;
            den += v * v;
        }
        num / den.max(1e-9)
    };
    let run = |spread: f32| -> Vec<f32> {
        let mut r = BinauralRenderer::new(0.0);
        r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
        let mut scene = SpatialScene::new(SR);
        let id = scene
            .create_audio_object(Vec3::new(1.0, 1.0, 0.0).normalized().unwrap())
            .unwrap();
        scene.object_mut(id).unwrap().spread = spread;
        render_impulse(&mut r, &scene, 512)
    };
    let out0 = run(0.0);
    let out1 = run(1.0);
    let frames = 512usize;
    let diff0 = (centroid(0, &out0, frames) - centroid(1, &out0, frames)).abs();
    let diff1 = (centroid(0, &out1, frames) - centroid(1, &out1, frames)).abs();
    assert!(
        diff0 > diff1 + 3.0,
        "spread shrinks the effective ITD ({diff0:.1} → {diff1:.1})"
    );
    assert!(out0.iter().all(|v| v.is_finite()));
    assert!(out1.iter().all(|v| v.is_finite()));
}

#[test]
fn full_hybrid_scene_is_deterministic_and_finite() {
    // Objects (one occluded + spread), a bed, a field, and the room through
    // two fresh renderers: bit-for-bit identical and finite.
    let mut scene = SpatialScene::new(SR);
    scene.listener.set_position(Vec3::new(6.0, 5.0, 1.5));
    let obj = scene.create_audio_object(Vec3::new(1.0, 5.0, 1.5)).unwrap();
    {
        let o = scene.object_mut(obj).unwrap();
        o.room_send = 1.0;
        o.spread = 0.4;
        o.occlusion = engine::spatial::Occlusion {
            amount: 0.5,
            ..Default::default()
        };
    }
    scene.room = Room {
        enabled: true,
        late_mix: 0.4,
        ..Default::default()
    };
    scene.create_bed(ChannelLayout::Stereo).unwrap();
    scene.create_field().unwrap();

    let frames = 1024usize;
    let obj_plane = impulse_plane(frames);
    let fl = vec![0.25f32; frames];
    let fr = vec![0.25f32; frames];
    let field_plane = vec![0.5f32; frames];
    let bed_refs: Vec<&[f32]> = vec![fl.as_slice(), fr.as_slice()];
    let field_refs: Vec<&[f32]> = vec![field_plane.as_slice()];
    let inputs = HybridBlockInputs {
        objects: &[obj_plane.as_slice()],
        beds: &bed_refs,
        fields: &field_refs,
    };

    let run = |r: &mut BinauralRenderer| -> Vec<f32> {
        let mut out = vec![0.0f32; 2 * frames];
        r.process_hybrid_block(&scene, &inputs, frames, &mut out)
            .unwrap();
        out
    };
    let mut r1 = BinauralRenderer::new(0.0);
    let mut r2 = BinauralRenderer::new(0.0);
    r1.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    r2.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let a = run(&mut r1);
    let b = run(&mut r2);
    assert!(a.iter().all(|v| v.is_finite()));
    assert!(
        a.iter().zip(b.iter()).all(|(x, y)| x == y),
        "binaural render is deterministic"
    );
}

#[test]
fn prepare_requires_a_stereo_layout() {
    // The binaural renderer *is* the head: exactly two enabled non-LFE
    // speakers (stereo/headphone layouts) are accepted.
    let mut r = BinauralRenderer::new(0.0);
    assert!(matches!(
        r.prepare(&SpeakerLayout::five_point_one(), SR),
        Err(engine::spatial::RenderError::InvalidLayout)
    ));
    assert!(r.prepare(&SpeakerLayout::stereo(), SR).is_ok());
}

#[test]
fn woodworth_values_pin_closed_form_at_public_api() {
    // The public head-model math pins the documented closed forms: the
    // ear-axis ITD is exactly (a/c)(π/2 + 1) and α at the ear is 2.0 /
    // 0.1 (ipsi / contra).
    let max = engine::spatial::woodworth_itd_sec(FRAC_PI_2, 0.0875, 343.0);
    assert!((max - 0.0875 / 343.0 * (FRAC_PI_2 + 1.0)).abs() < 1e-6);
    assert!(engine::spatial::woodworth_itd_sec(0.0, 0.0875, 343.0).abs() < 1e-9);
    assert!(engine::spatial::woodworth_itd_sec(FRAC_PI_4, 0.0875, 343.0) < max);
    assert!((engine::spatial::head_shadow_alpha(FRAC_PI_2, Ear::Right) - 2.0).abs() < 1e-6);
    assert!((engine::spatial::head_shadow_alpha(FRAC_PI_2, Ear::Left) - 0.1).abs() < 1e-6);
}
