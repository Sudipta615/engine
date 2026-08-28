//! Acceptance suite for head tracking (spec Phase 10 / roadmap Phase 15,
//! §48, §136).
//!
//! Head tracking is a control-side seam: a [`HeadTracker`] turns a stream
//! of timestamped orientation samples into a smooth listener orientation
//! that the host applies before each render block — the renderers never
//! change. The contract this suite pins down:
//!
//! - **Woodworth consistency** — with exact tracking, a world-fixed source
//!   renders at the ears with the *closed-form* ITD for the head's current
//!   orientation, block by block: `lag = itd(az, Left) − itd(az, Right)`
//!   where `az` is the source's listener-local azimuth at that instant.
//! - **The scene follows the head** — the same sweep without applying the
//!   tracker leaves the image frozen (the contrast that makes tracking
//!   meaningful); with a speaker renderer, a head turn moves the image
//!   across the layout exactly as the physics demands.
//! - **No zipper** — smoothing turns a jump into bounded, convergent
//!   per-block steps (the tracker's one-pole + optional rate limit).
//! - **Determinism** — identical sample streams produce bit-identical
//!   orientations.

use engine::spatial::math::{Quat, Vec3};
use engine::spatial::render::SpatialRenderer;
use engine::spatial::{
    BasicPanner, BinauralRenderer, Ear, HeadSample, HeadTracker, SpatialScene, SpeakerLayout,
    TrackingConfig,
};
use std::f32::consts::FRAC_PI_2;

const SR: u32 = 48_000;

fn yaw(deg: f32) -> Quat {
    Quat::from_euler_rad(deg.to_radians(), 0.0, 0.0)
}

/// Render one impulse block through the binaural renderer and measure each
/// ear's argmax frame.
fn binaural_ear_peaks(
    r: &mut BinauralRenderer,
    scene: &SpatialScene,
    frames: usize,
) -> (usize, usize) {
    let mut input = vec![0.0f32; frames];
    input[0] = 1.0;
    let mut out = vec![0.0f32; 2 * frames];
    r.process_block(scene, &[&input], frames, &mut out).unwrap();
    let argmax = |ear: usize| -> usize {
        let mut best = 0usize;
        let mut best_v = 0.0f32;
        for f in 0..frames {
            let v = out[f * 2 + ear].abs();
            if v > best_v {
                best_v = v;
                best = f;
            }
        }
        best
    };
    (argmax(0), argmax(1))
}

#[test]
fn tracked_yaw_sweep_matches_woodworth_at_the_ears() {
    // A world-fixed object at +X; the head sweeps yaw 0 → 137° in 24
    // blocks. With exact tracking the rendered ear lag must equal the
    // closed form `itd(az, L) − itd(az, R)` at *every* block — the cues
    // track the head's actual orientation, not a frozen one.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::X).unwrap();

    let mut tracker = HeadTracker::new(TrackingConfig {
        smoothing_ms: 0.0, // exact — isolate the tracking, not the smoothing
        max_angular_rate_deg_s: 0.0,
    });
    let frames = 512usize;
    let n = 24usize;
    for k in 0..=n {
        let deg = 137.0 * k as f32 / n as f32;
        tracker.push(HeadSample::new(0.01 * k as f64, yaw(deg)));
        tracker.apply_to(&mut scene.listener, 0.01 * k as f64);
        let (l, rp) = binaural_ear_peaks(&mut r, &scene, frames);
        let lag = l as i64 - rp as i64;
        // Listener-local azimuth of world +X at head yaw θ: atan2(cosθ, sinθ).
        let az = (deg.to_radians().cos()).atan2(deg.to_radians().sin());
        let expect = (r.itd_samples(az, Ear::Left) - r.itd_samples(az, Ear::Right)) as i64;
        assert!(
            (lag - expect).abs() <= 3,
            "block {k}: lag {lag} vs Woodworth {expect} (az {:.1}°)",
            az.to_degrees()
        );
    }
}

#[test]
fn untracked_sweep_leaves_the_image_frozen() {
    // The contrast: sweep the *tracker* exactly as above but never apply it
    // to the listener. The head has turned 90°+, yet the renderer still
    // hears the +X source at +90° (lag stays 31.5 samples) instead of
    // tracking it toward the front (lag ≈ 0). This is what head tracking
    // fixes.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::X).unwrap();

    let frames = 512usize;
    // The tracker advances (the head really turns 90° to face the +X
    // source)…
    let mut tracker = HeadTracker::new(TrackingConfig {
        smoothing_ms: 0.0,
        max_angular_rate_deg_s: 0.0,
    });
    for k in 0..=16 {
        tracker.push(HeadSample::new(
            0.01 * k as f64,
            yaw(90.0 * k as f32 / 16.0),
        ));
    }
    // …but the listener is never updated (the bug head tracking fixes).
    let (l, rp) = binaural_ear_peaks(&mut r, &scene, frames);
    let frozen_lag = l as i64 - rp as i64;
    assert!(
        (frozen_lag - 31).abs() <= 3,
        "untracked image stays at the right ear (lag {frozen_lag})"
    );

    // The tracked counterpart: by the end of the same sweep the source is
    // straight ahead of the turned head — lag ≈ 0.
    let mut tracker2 = HeadTracker::new(TrackingConfig {
        smoothing_ms: 0.0,
        max_angular_rate_deg_s: 0.0,
    });
    for k in 0..=16 {
        tracker2.push(HeadSample::new(
            0.01 * k as f64,
            yaw(90.0 * k as f32 / 16.0),
        ));
        tracker2.apply_to(&mut scene.listener, 0.01 * k as f64);
    }
    let (l2, r2) = binaural_ear_peaks(&mut r, &scene, frames);
    let tracked_lag = l2 as i64 - r2 as i64;
    assert!(
        tracked_lag.abs() <= 3,
        "tracked image reaches the front (lag {tracked_lag})"
    );
    assert!(
        (frozen_lag - tracked_lag).abs() > 20,
        "tracked and untracked diverge"
    );
}

#[test]
fn smoothing_glides_the_image_without_zipper() {
    // A 90° head jump with τ = 40 ms sampled at 100 Hz: the rendered ear
    // lag moves in bounded per-block steps (no sample jumps 30+ samples)
    // and converges to the front (lag ≈ 0) — the listener's orientation
    // follows the one-pole, and the renderer is only ever fed smooth
    // orientations.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::X).unwrap();

    let mut tracker = HeadTracker::new(TrackingConfig {
        smoothing_ms: 40.0,
        max_angular_rate_deg_s: 0.0,
    });
    tracker.push(HeadSample::new(0.0, yaw(0.0)));
    tracker.push(HeadSample::new(0.01, yaw(90.0)));
    let frames = 512usize;
    let mut prev_lag = 31.0f64; // start: source at +90°
    let mut max_step = 0.0f64;
    let mut last_lag = prev_lag;
    for k in 1..=60 {
        tracker.apply_to(&mut scene.listener, 0.01 * k as f64);
        let (l, rp) = binaural_ear_peaks(&mut r, &scene, frames);
        let lag = l as f64 - rp as f64;
        let step = (lag - prev_lag).abs();
        max_step = max_step.max(step);
        prev_lag = lag;
        last_lag = lag;
    }
    assert!(
        max_step < 10.0,
        "no zipper: max per-block lag step {max_step:.1} samples"
    );
    assert!(
        last_lag.abs() < 3.0,
        "converged to the front (final lag {last_lag:.1})"
    );
}

#[test]
fn panner_tracked_sweep_moves_the_image_across_the_layout() {
    // With a speaker renderer the same physics: a world-fixed +X object
    // starts at the side (SL-dominant in 5.1) and moves to the front pair
    // (FL/FR) as the head turns 90° to face it.
    let mut p = BasicPanner::new(0.0);
    p.prepare(&SpeakerLayout::five_point_one(), SR).unwrap();
    let mut scene = SpatialScene::new(SR);
    scene.create_audio_object(Vec3::X).unwrap();

    let mut tracker = HeadTracker::new(TrackingConfig {
        smoothing_ms: 0.0,
        max_angular_rate_deg_s: 0.0,
    });
    let frames = 256usize;
    let input = vec![1.0f32; frames];
    let mut out = vec![0.0f32; 6 * frames];
    // "Front" energy includes the center channel — a straight-ahead object
    // lands on C in 5.1 (azimuth 0°), not on the FL/FR pair.
    let energies = |out: &[f32]| -> (f32, f32) {
        let front: f32 = (0..frames)
            .map(|f| {
                out[f * 6] * out[f * 6]
                    + out[f * 6 + 1] * out[f * 6 + 1]
                    + out[f * 6 + 2] * out[f * 6 + 2]
            })
            .sum();
        let side: f32 = (0..frames)
            .map(|f| out[f * 6 + 4] * out[f * 6 + 4] + out[f * 6 + 5] * out[f * 6 + 5])
            .sum();
        (front, side)
    };
    let mut front_first = 0.0f32;
    let mut front_last = 0.0f32;
    let mut side_first = 0.0f32;
    let mut side_last = 0.0f32;
    for k in 0..=16 {
        let deg = 90.0 * k as f32 / 16.0;
        tracker.push(HeadSample::new(0.01 * k as f64, yaw(deg)));
        tracker.apply_to(&mut scene.listener, 0.01 * k as f64);
        p.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        let (front, side) = energies(&out);
        if k == 0 {
            front_first = front;
            side_first = side;
        }
        if k == 16 {
            front_last = front;
            side_last = side;
        }
    }
    assert!(
        front_last > front_first + 0.2,
        "image moves to the front pair ({front_first:.3} → {front_last:.3})"
    );
    assert!(
        side_last < side_first - 0.2,
        "and away from the sides ({side_first:.3} → {side_last:.3})"
    );
    assert!(out.iter().all(|v| v.is_finite()));
}

#[test]
fn tracker_is_deterministic_and_rate_limited_end_to_end() {
    // Two trackers fed the identical (jittery, including a violent 90°
    // glitch) sample stream produce bit-identical orientations; with a
    // 200°/s cap the per-block angular step never exceeds 2° at 100 Hz.
    let run = |config: TrackingConfig| -> Vec<Quat> {
        let mut t = HeadTracker::new(config);
        let mut out = Vec::new();
        t.push(HeadSample::new(0.0, yaw(0.0)));
        // A glitch: the head snaps 90° in one sample.
        t.push(HeadSample::new(0.01, yaw(90.0)));
        for k in 1..=50 {
            let q = t.sample(0.01 * k as f64);
            out.push(q);
        }
        out
    };
    let a = run(TrackingConfig {
        smoothing_ms: 5.0,
        max_angular_rate_deg_s: 0.0,
    });
    let b = run(TrackingConfig {
        smoothing_ms: 5.0,
        max_angular_rate_deg_s: 0.0,
    });
    assert!(
        a.iter().zip(b.iter()).all(|(x, y)| x == y),
        "tracker is deterministic"
    );

    // Rate limit: 200°/s at 100 Hz → ≤ 2° per sample, so the glitch cannot
    // fling the soundfield.
    let limited = run(TrackingConfig {
        smoothing_ms: 0.0,
        max_angular_rate_deg_s: 200.0,
    });
    let mut max_step = 0.0f32;
    let mut prev = limited[0];
    for &q in &limited[1..] {
        max_step = max_step.max(prev.angle_to(q).to_degrees());
        prev = q;
    }
    assert!(max_step <= 2.001, "rate limit caps the step ({max_step}°)");
    assert!(limited.iter().all(|q| q.is_finite()));
}

#[test]
fn apply_to_updates_the_listener_orientation() {
    // The host seam: applying the tracker writes exactly its current
    // orientation onto the listener, and the scene renders with it.
    let mut t = HeadTracker::new(TrackingConfig {
        smoothing_ms: 0.0,
        max_angular_rate_deg_s: 0.0,
    });
    t.push(HeadSample::new(0.0, yaw(0.0)));
    t.push(HeadSample::new(0.1, yaw(90.0)));
    let mut scene = SpatialScene::new(SR);
    let q = t.apply_to(&mut scene.listener, 0.1);
    assert_eq!(scene.listener.orientation, q);
    assert!(scene.listener.orientation.angle_to(yaw(90.0)) < 1e-5);
    // And the renderer uses it: an object at world +Y (front of the
    // un-rotated world) is now at the listener's left (−90°), so the right
    // ear is delayed by the full Woodworth ITD.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    scene.create_audio_object(Vec3::Y).unwrap();
    let frames = 512usize;
    let (l, rp) = binaural_ear_peaks(&mut r, &scene, frames);
    let expect = r.itd_samples(-FRAC_PI_2, Ear::Right);
    assert!(
        (rp as f32 - l as f32 - expect).abs() < 2.0,
        "right ear delayed by the ITD (L@{l} R@{rp} expect {expect:.1})"
    );
}
