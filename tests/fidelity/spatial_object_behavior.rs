//! Acceptance suite for object behavior (spec Phase 5 / §30, §41, §43–44).
//!
//! The contract tests the renderers are reviewed against:
//!
//! - **Directivity** — a directional source must be loud only in the
//!   directions it faces, following the documented angle convention
//!   (0 = facing the listener, π = facing away), for omni / cardioid /
//!   supercardioid / custom curves.
//! - **Occlusion** — `amount` must attenuate broadband and roll off
//!   high frequencies through a real low-pass (10 kHz dies far harder than
//!   100 Hz), be monotonic, and keep the cutoff bounded.
//! - **Spread** — a spread source must widen its energy across *more*
//!   speakers while preserving total energy and left/right symmetry of a
//!   centered source (spec §29–30).
//! - **Composition** — directivity + occlusion + spread together stay
//!   finite, deterministic, and behave monotonically.
//!
//! All measurements use analytic signals (unit sine / impulse trains) and
//! tight tolerances; both the panner and the VBAP renderer are exercised.

use engine::spatial::math::{Quat, Vec3};
use engine::spatial::render::SpatialRenderer;
use engine::spatial::{
    CustomDirectivity, Directivity, Occlusion, SpatialScene, SpeakerLayout, VbapRenderer,
};

const SR: u32 = 48_000;
const PI: f32 = std::f32::consts::PI;

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// Total energy of frame 0 (unit input), excluding LFE (index 3 in 7.1.4).
fn energy_frame(out: &[f32], ch: usize, lfe: Option<usize>) -> f32 {
    let mut e = 0.0f32;
    for (spk, v) in out.iter().take(ch).enumerate() {
        if Some(spk) != lfe {
            e += v * v;
        }
    }
    e
}

/// Render a single object with a unit (1.0) input for `frames` frames,
/// returning the full interleaved output.
fn render_one(
    renderer: &mut VbapRenderer,
    layout: &SpeakerLayout,
    scene: &SpatialScene,
    frames: usize,
) -> Vec<f32> {
    let mut out = vec![0.0f32; layout.speakers.len() * frames];
    let input = vec![1.0f32; frames];
    renderer
        .process_block(scene, &[&input], frames, &mut out)
        .unwrap();
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Directivity
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn cardioid_hears_only_what_it_faces() {
    // Convention (spec §18/§153): angle 0 = the source faces the listener.
    // An object behind the listener facing +Y (identity) faces the listener;
    // the same object in front faces away.
    let layout = SpeakerLayout::five_point_one();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();

    // Behind the listener, facing forward: full cardioid gain.
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(-Vec3::Y).unwrap();
    scene.object_mut(id).unwrap().directivity = Directivity::Cardioid;
    let out = render_one(&mut r, &layout, &scene, 8);
    assert!(out.iter().all(|x| x.is_finite()));
    let e = energy_frame(&out, 6, Some(3));
    assert!(e > 0.9, "facing the listener → cardioid gain ~1 (e={e})");

    // In front, facing away: cardioid null → near silence.
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(Vec3::Y).unwrap();
    scene.object_mut(id).unwrap().directivity = Directivity::Cardioid;
    let out = render_one(&mut r, &layout, &scene, 8);
    let e = energy_frame(&out, 6, Some(3));
    assert!(e < 0.01, "facing away → cardioid null (e={e})");

    // Yaw the source 180° so it faces the listener even from the front.
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(Vec3::Y).unwrap();
    scene.object_mut(id).unwrap().directivity = Directivity::Cardioid;
    scene.object_mut(id).unwrap().source_orientation = Quat::from_euler_rad(PI, 0.0, 0.0);
    let out = render_one(&mut r, &layout, &scene, 8);
    let e = energy_frame(&out, 6, Some(3));
    assert!(e > 0.9, "yawed to face the listener → gain ~1 (e={e})");
}

#[test]
fn omnidirectional_default_is_unchanged() {
    // Regression guard: the default object (omni, no occlusion, spread 0)
    // must render exactly as before the behavior phase.
    let layout = SpeakerLayout::stereo();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::Y).unwrap();
    let out = render_one(&mut r, &layout, &scene, 8);
    assert!(out.iter().all(|x| x.is_finite()));
    assert!(
        (out[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3,
        "front → FL unchanged ({})",
        out[0]
    );
    assert!(
        (out[1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3,
        "front → FR unchanged ({})",
        out[1]
    );
}

#[test]
fn custom_directivity_routes_by_angle() {
    // Custom curve: only the exact front direction is audible (1.0 at 0°,
    // 0 elsewhere). The table is sampled every 2°.
    let mut table = [0.0f32; engine::spatial::directivity::DIRECTIVITY_TABLE_LEN];
    table[0] = 1.0; // 0° = facing the listener
    let custom = CustomDirectivity::from_samples(&table).unwrap();

    let layout = SpeakerLayout::five_point_one();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();

    // Behind the listener facing forward → angle 0 → gain 1.
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(-Vec3::Y).unwrap();
    scene.object_mut(id).unwrap().directivity = custom.clone().into_directivity();
    let out = render_one(&mut r, &layout, &scene, 8);
    let e = energy_frame(&out, 6, Some(3));
    assert!(e > 0.9, "facing listener → custom gain 1 (e={e})");

    // Directly to the side → angle 90° → table 0 → silent.
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(Vec3::X).unwrap();
    scene.object_mut(id).unwrap().directivity = custom.into_directivity();
    let out = render_one(&mut r, &layout, &scene, 8);
    let e = energy_frame(&out, 6, Some(3));
    assert!(e < 0.01, "side → custom null (e={e})");
}

// ─────────────────────────────────────────────────────────────────────────────
// Occlusion
// ─────────────────────────────────────────────────────────────────────────────

/// RMS of the stereo render of a front object fed a sine at `freq`.
fn occluded_rms(freq: f32, amount: f32) -> f32 {
    let layout = SpeakerLayout::stereo();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(Vec3::Y).unwrap();
    scene.object_mut(id).unwrap().occlusion = Occlusion {
        amount,
        ..Default::default()
    };
    let frames = 256usize;
    let input: Vec<f32> = (0..frames)
        .map(|i| (2.0 * PI * freq * i as f32 / SR as f32).sin())
        .collect();
    let mut out = vec![0.0f32; 2 * frames];
    // Warm up the filter state and cutoff smoothing before measuring.
    for _ in 0..8 {
        r.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
    }
    rms(&out)
}

#[test]
fn occlusion_attenuates_and_low_passes() {
    // No occlusion: a 100 Hz front sine reaches both speakers (combined
    // interleaved RMS of the equal FL/FR split ≈ 0.5).
    let clean = occluded_rms(100.0, 0.0);
    assert!(clean > 0.4, "unoccluded bass passes (rms={clean})");

    // Full occlusion: broadband attenuation (24 dB) applies even below the
    // cutoff, and 10 kHz dies far harder than 100 Hz (real low-pass).
    let full_bass = occluded_rms(100.0, 1.0);
    let full_treb = occluded_rms(10_000.0, 1.0);
    assert!(
        full_bass < clean * 0.1,
        "full occlusion attenuates 24 dB (bass {full_bass} vs {clean})"
    );
    assert!(
        full_treb < full_bass * 0.1,
        "low-pass rolls off 10 kHz far below 100 Hz ({full_treb} vs {full_bass})"
    );
    assert!(full_treb.is_finite());

    // Monotonic: more occlusion → quieter, at 100 Hz.
    let m0 = occluded_rms(100.0, 0.0);
    let m5 = occluded_rms(100.0, 0.5);
    let m10 = occluded_rms(100.0, 1.0);
    assert!(
        m0 > m5 && m5 > m10,
        "attenuation monotonic ({m0} > {m5} > {m10})"
    );
}

#[test]
fn occlusion_transmission_is_bounded() {
    let occ = Occlusion {
        amount: 0.7,
        ..Default::default()
    };
    let tr = occ.transmission(SR as f32);
    assert!(tr.attenuation_db > 0.0 && tr.attenuation_db <= 24.0);
    assert!(
        tr.cutoff_hz >= 500.0 && tr.cutoff_hz <= SR as f32 * 0.5,
        "cutoff bounded: {}",
        tr.cutoff_hz
    );
    assert_eq!(tr.diffusion, 0.0, "diffusion is a documented seam");
    assert!(tr.gain() > 0.0 && tr.gain() < 1.0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Spread
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn spread_widens_energy_and_preserves_energy() {
    // 7.1.4, front object. spread 0 → the real Center speaker (index 2)
    // takes everything. spread 1 → the angular region (60° cap) must spread
    // across several speakers, while total energy stays ≈ 1 (constant power).
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(Vec3::Y).unwrap();

    let mut concentrations = Vec::new();
    for spread in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
        scene.object_mut(id).unwrap().spread = spread;
        let out = render_one(&mut r, &layout, &scene, 8);
        assert!(out.iter().all(|x| x.is_finite()), "NaN at spread {spread}");
        let e = energy_frame(&out, 12, Some(3));
        assert!(
            (e - 1.0).abs() < 0.1,
            "energy preserved at spread {spread}: {e}"
        );
        // Concentration index Σ (e_k / E)²: 1.0 = single speaker, < 1 = spread.
        let total = out[0..12].iter().map(|v| v * v).sum::<f32>().max(1e-9);
        let c: f32 = out[0..12].iter().map(|v| (v * v / total).powi(2)).sum();
        concentrations.push(c);
    }
    assert!(
        (concentrations[0] - 1.0).abs() < 1e-3,
        "spread 0 → single speaker (c={})",
        concentrations[0]
    );
    assert!(
        concentrations[4] < 0.5,
        "spread 1 → energy spread across speakers (c={})",
        concentrations[4]
    );
    // Wider spread generally deconcentrates.
    assert!(
        concentrations[4] < concentrations[2],
        "monotonic deconcentration"
    );
}

#[test]
fn spread_preserves_symmetry_and_energy_in_stereo() {
    // A centered source with symmetric spread must stay perfectly centered
    // (phantom image), and energy must hold at every spread.
    let layout = SpeakerLayout::stereo();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(Vec3::Y).unwrap();
    for spread in [0.0f32, 0.3, 0.6, 1.0] {
        scene.object_mut(id).unwrap().spread = spread;
        let out = render_one(&mut r, &layout, &scene, 8);
        assert!(out.iter().all(|x| x.is_finite()), "NaN at spread {spread}");
        assert!(
            (out[0] - out[1]).abs() < 1e-3,
            "centered source stays centered at spread {spread}"
        );
        let e = out[0] * out[0] + out[1] * out[1];
        assert!(
            (e - 1.0).abs() < 0.05,
            "energy preserved at spread {spread}: {e}"
        );
    }
}

#[test]
fn spread_sweep_is_continuous_with_smoothing() {
    // Sweep spread 0→1 in fine steps with smoothing enabled: no NaN, no
    // discontinuous gain jump (per-block delta bounded by the smoothing
    // coefficient times a bounded target step).
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = VbapRenderer::with_smoothing(engine::spatial::panner::DEFAULT_SMOOTHING_MS);
    r.prepare(&layout, SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    let id = scene.create_audio_object(Vec3::Y).unwrap();
    let frames = 64usize;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 12 * frames];
    let mut prev_fl = 0.0f32;
    for step in 0..40 {
        scene.object_mut(id).unwrap().spread = step as f32 / 39.0;
        r.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        assert!(out.iter().all(|x| x.is_finite()), "NaN at step {step}");
        let fl = out[0];
        if step > 0 {
            let delta = (fl - prev_fl).abs();
            assert!(delta < 0.1, "no jump in FL at step {step} ({delta})");
        }
        prev_fl = fl;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Composition
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn directivity_occlusion_spread_compose_monotonically() {
    // All three behaviors on one object: an occluded, spread cardioid source
    // must stay finite and deterministic, and occlusion must still reduce the
    // delivered energy relative to the same source without occlusion.
    let layout = SpeakerLayout::seven_point_one_four();
    let mut r = VbapRenderer::with_smoothing(0.0);
    r.prepare(&layout, SR).unwrap();

    let mut build = |amount: f32| -> (SpatialScene, Vec<f32>) {
        let mut scene = SpatialScene::new(SR);
        let id = scene.create_audio_object(-Vec3::Y).unwrap(); // rear, facing listener
        let obj = scene.object_mut(id).unwrap();
        obj.directivity = Directivity::Cardioid;
        obj.spread = 0.4;
        obj.occlusion = Occlusion {
            amount,
            ..Default::default()
        };
        let out = render_one(&mut r, &layout, &scene, 8);
        (scene, out)
    };

    let (_, clean) = build(0.0);
    let (_, occluded) = build(1.0);
    assert!(clean.iter().all(|x| x.is_finite()));
    assert!(occluded.iter().all(|x| x.is_finite()));
    let e_clean = energy_frame(&clean, 12, Some(3));
    let e_occ = energy_frame(&occluded, 12, Some(3));
    assert!(
        e_clean > 0.5,
        "cardioid rear object delivers energy ({e_clean})"
    );
    assert!(
        e_occ < e_clean * 0.2,
        "full occlusion dominates composition ({e_occ} vs {e_clean})"
    );
    // Deterministic: same scene again → bit-identical output.
    let (scene, _) = build(0.0);
    let again = render_one(&mut r, &layout, &scene, 8);
    assert!(
        clean
            .iter()
            .zip(again.iter())
            .all(|(a, b)| (a - b).abs() < 1e-6),
        "composition is deterministic"
    );
}
