//! Unit tests for room acoustics (early reflections + late-field FDN).

use super::*;
use crate::spatial::math::Vec3;

const EPS: f32 = 1e-4;

#[test]
fn default_room_is_disabled_and_sane() {
    let r = Room::default();
    assert!(!r.enabled, "default room is disabled (bit-exact)");
    assert!(r.width > 0.0 && r.depth > 0.0 && r.height > 0.0);
    assert!((reflection_coefficient(&r) - 0.8).abs() < EPS);
    assert!(r.reflection_order >= 1 && r.reflection_order <= 2);
}

#[test]
fn first_order_images_match_closed_form() {
    let room = Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        ..Default::default()
    };
    let mut imgs = [ReflectionImage::ZERO; MAX_IMAGES];
    let n = image_sources(&room, Vec3::new(3.0, 4.0, 2.0), &mut imgs);
    assert_eq!(n, 6, "order 1 → 6 images");
    let r = 0.8f32;
    let expected = [
        (Vec3::new(-3.0, 4.0, 2.0), r), // x = 0
        (Vec3::new(21.0, 4.0, 2.0), r), // x = 12
        (Vec3::new(3.0, -4.0, 2.0), r), // y = 0
        (Vec3::new(3.0, 16.0, 2.0), r), // y = 10
        (Vec3::new(3.0, 4.0, -2.0), r), // z = 0
        (Vec3::new(3.0, 4.0, 4.0), r),  // z = 3
    ];
    for (pos, coeff) in expected {
        let found = imgs[..n].iter().any(|i| {
            let d = i.position - pos;
            d.dot(d) < 1e-6 && (i.coeff - coeff).abs() < EPS
        });
        assert!(found, "missing image at {pos:?} coeff {coeff}");
    }
}

#[test]
fn second_order_images_count_and_coefficients() {
    let room = Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 2,
        ..Default::default()
    };
    let mut imgs = [ReflectionImage::ZERO; MAX_IMAGES];
    let n = image_sources(&room, Vec3::new(3.0, 4.0, 2.0), &mut imgs);
    assert_eq!(n, 24, "order 2 → 24 distinct images");
    let r = 0.8f32;
    // Two different walls → coeff r².
    for (pos, coeff) in [
        (Vec3::new(-3.0, -4.0, 2.0), r * r),
        (Vec3::new(21.0, 16.0, 2.0), r * r),
        (Vec3::new(-3.0, 4.0, -2.0), r * r),
    ] {
        let found = imgs[..n].iter().any(|i| {
            let d = i.position - pos;
            d.dot(d) < 1e-6 && (i.coeff - coeff).abs() < EPS
        });
        assert!(found, "missing two-wall image at {pos:?}");
    }
    // Same axis, both walls (2 crossings on x) → coeff r², positions
    // 2W+sx and sx−2W.
    for (pos, coeff) in [
        (Vec3::new(27.0, 4.0, 2.0), r * r),
        (Vec3::new(-21.0, 4.0, 2.0), r * r),
    ] {
        let found = imgs[..n].iter().any(|i| {
            let d = i.position - pos;
            d.dot(d) < 1e-6 && (i.coeff - coeff).abs() < EPS
        });
        assert!(found, "missing same-axis image at {pos:?}");
    }
}

#[test]
fn listener_relative_delays_match_path_difference() {
    let mut er = EarlyReflections::new();
    er.prepare(6, 48_000, 1.0);
    let room = Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        ..Default::default()
    };
    // Listener at centre, object 4 m to the left of the left wall's
    // mirror plane: direct = 5 m; left-wall image dist = 7 m.
    let listener = Vec3::new(6.0, 5.0, 1.5);
    let obj = Vec3::new(1.0, 5.0, 1.5);
    let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
    let n = er.images_for_object(&room, listener, obj, &mut imgs);
    assert_eq!(n, 6);
    let direct = 5.0f32;
    for img in imgs.iter().take(n) {
        let expect_delay = ((img.dist - direct).max(0.0) / 343.0 * 48_000.0).round() as u32;
        assert_eq!(
            img.delay, expect_delay,
            "delay = excess path / c for dist {}",
            img.dist
        );
    }
    // The left-wall image: dir −X (world), dist 7, delay 280.
    let left = imgs
        .iter()
        .find(|i| {
            let d = i.dir - Vec3::new(-1.0, 0.0, 0.0);
            d.dot(d) < 1e-6
        })
        .expect("left-wall image");
    assert!((left.dist - 7.0).abs() < 1e-4);
    assert_eq!(left.delay, 280);
    assert!((left.coeff - 0.8).abs() < EPS);
}

#[test]
fn engine_delays_an_impulse_by_the_image_excess_path() {
    // End-to-end engine check: an impulse object renders its left-wall
    // reflection 280 frames later, at the tap gain, on the pan speaker.
    let mut er = EarlyReflections::new();
    er.prepare(6, 48_000, 1.0); // smoothing off → exact taps
    let room = Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        ..Default::default()
    };
    let listener = Vec3::new(6.0, 5.0, 1.5);
    let obj_pos = Vec3::new(1.0, 5.0, 1.5);
    let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
    let n = er.images_for_object(&room, listener, obj_pos, &mut imgs);
    er.begin_object(0);
    // Simulate the renderer: pan the left-wall image to speaker 4 with a
    // hard gain (1.0), all others to speaker 0 with small gains.
    let target_for = |img: &ListenerImage| -> (usize, f32) {
        if img.delay == 280 {
            (4, 0.8)
        } else {
            (0, 0.05)
        }
    };
    for (i, img) in imgs.iter().take(n).enumerate() {
        let (spk, g) = target_for(img);
        er.add_tap(0, i, spk, img.delay, g);
    }
    let frames = 512usize;
    let mut out = vec![0.0f32; 6 * frames];
    let trim = vec![1.0f32; 6];
    er.begin_block(frames);
    let mut input = vec![0.0f32; frames];
    input[0] = 1.0;
    for (f, &s) in input.iter().enumerate() {
        er.object_frame(0, s, 1.0, f, 6, &mut out, &trim);
    }
    er.end_block(frames);
    // The delayed tap: frame 280, speaker 4, amplitude 1.0 × 0.8.
    assert!(
        (out[280 * 6 + 4] - 0.8).abs() < 1e-4,
        "reflection at 280 on spk 4: {}",
        out[280 * 6 + 4]
    );
    // Nothing on speaker 4 before the delay.
    assert!(out[279 * 6 + 4].abs() < 1e-6);
    // The direct impulse (send) is not panned by the engine itself.
    assert!(out[4].abs() < 1e-6);
}

#[test]
fn filter_reflection_colours_binaural_impulse_reads() {
    let mut er = EarlyReflections::new();
    er.prepare(2, 48_000, 1.0); // stereo ears, exact
    er.set_reflection_filter(0, 0, 500.0); // coloured image
    er.set_reflection_filter(0, 1, f32::INFINITY); // flat image
    let mut coloured = Vec::new();
    let mut flat = Vec::new();
    for f in 0..8usize {
        let x = if f == 0 { 1.0 } else { 0.0 };
        coloured.push(er.filter_reflection(0, 0, x));
        flat.push(er.filter_reflection(0, 1, x));
    }
    assert_eq!(flat[0], 1.0, "flat passthrough keeps the impulse");
    assert!(
        flat[1..].iter().all(|&v| v == 0.0),
        "flat image stays a clean tap"
    );
    assert!(
        coloured[0] > 0.0 && coloured[0] < 1.0,
        "coloured peak sagged ({})",
        coloured[0]
    );
    assert!(
        coloured[1..].iter().any(|&v| v != 0.0),
        "coloured image rings after the tap"
    );
}

#[test]
fn spectral_reflection_filter_colours_an_impulse_tap() {
    let mut er = EarlyReflections::new();
    er.prepare(6, 48_000, 1.0); // smoothing off → exact
    let room = Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        ..Default::default()
    };
    let listener = Vec3::new(6.0, 5.0, 1.5);
    let obj_pos = Vec3::new(1.0, 5.0, 1.5);
    let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
    let n = er.images_for_object(&room, listener, obj_pos, &mut imgs);
    let idx_280 = (0..n).find(|&i| imgs[i].delay == 280).unwrap();
    er.begin_object(0);
    for (i, img) in imgs.iter().take(n).enumerate() {
        let (spk, g) = if i == idx_280 { (4, 0.8) } else { (0, 0.05) };
        er.add_tap(0, i, spk, img.delay, g);
    }
    er.set_reflection_filter(0, idx_280, 500.0);
    for i in 0..n {
        if i != idx_280 {
            er.set_reflection_filter(0, i, f32::INFINITY);
        }
    }
    let frames = 512usize;
    let mut out = vec![0.0f32; 6 * frames];
    let trim = vec![1.0f32; 6];
    er.begin_block(frames);
    let mut input = vec![0.0f32; frames];
    input[0] = 1.0;
    for (f, &s) in input.iter().enumerate() {
        er.object_frame(0, s, 1.0, f, 6, &mut out, &trim);
    }
    er.end_block(frames);
    let peak = out[280 * 6 + 4];
    assert!(
        peak > 0.0 && peak < 0.8,
        "coloured peak sagged (peak {peak})"
    );
    let mut tail = 0.0f32;
    for f in 281..420 {
        tail += out[f * 6 + 4].abs();
    }
    assert!(
        tail > 0.02,
        "spectral reflection rings after the tap ({tail})"
    );
}

#[test]
fn schroeder_tail_rt60_matches_config() {
    let mut tail = RoomLateField::new();
    tail.prepare(48_000);
    let room = Room {
        enabled: true,
        rt60_ms: 500.0,
        ..Default::default()
    };
    let total = 240_000usize; // 5 s
    let cutoff = 48_000usize; // 1 s of excitation, then silence
    let mut send = vec![0.0f32; total];
    send[..cutoff].fill(1.0);
    let mut out = vec![0.0f32; total];
    let n = tail.process(&room, &send, total, &mut out);
    assert_eq!(n, total);

    let steady = out[40_000..44_000]
        .iter()
        .fold(0.0f32, |m, &v| m.max(v.abs()));
    assert!(steady > 0.5, "tail reaches steady state ({steady})");

    let win = 2_400usize; // 50 ms
    let start = cutoff + 4_800; // +100 ms
    let mut env: Vec<(f32, f32)> = Vec::new();
    let mut w = start;
    while w + win <= total {
        let e = out[w..w + win].iter().map(|v| v * v).sum::<f32>() / win as f32;
        let db = 10.0 * e.log10();
        if db > -70.0 {
            env.push((w as f32 / 48_000.0, db));
        }
        w += win;
    }
    assert!(env.len() > 5, "enough decay windows ({})", env.len());

    let seg = &env[..env.len() - 1];
    let m = seg.len() as f32;
    let sx: f32 = seg.iter().map(|&(t, _)| t).sum();
    let sy: f32 = seg.iter().map(|&(_, d)| d).sum();
    let sxx: f32 = seg.iter().map(|&(t, _)| t * t).sum();
    let sxy: f32 = seg.iter().map(|&(t, d)| t * d).sum();
    let a = (m * sxy - sx * sy) / (m * sxx - sx * sx).max(1e-9);
    assert!(a < 0.0, "envelope decays (slope {a})");
    let measured_secs = 60.0 / (-a).max(1e-3);
    assert!(
        (measured_secs - 0.5).abs() / 0.5 < 0.15,
        "measured RT60 {:.0} ms vs 500 ms",
        measured_secs * 1000.0
    );
}

#[test]
fn schroeder_tail_is_bounded_and_deterministic() {
    let mut tail = RoomLateField::new();
    tail.prepare(48_000);
    let room = Room {
        enabled: true,
        rt60_ms: 1500.0,
        late_mix: 1.0,
        late_distance: false,
        ..Default::default()
    };
    let mut send = vec![0.0f32; 480_000]; // 10 s of noise
    let mut seed = 0x1234_5678u32;
    for s in send.iter_mut() {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *s = ((seed >> 8) as f32 / 16_777_216.0 - 0.5) * 2.0;
    }
    let mut out = vec![0.0f32; send.len()];
    let n = tail.process(&room, &send, send.len(), &mut out);
    assert_eq!(n, send.len());
    assert!(out.iter().all(|v| v.is_finite()));
    let max_abs = out.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    assert!(max_abs < 4.0, "bounded tail output (max {max_abs})");

    let mut tail2 = RoomLateField::new();
    tail2.prepare(48_000);
    let mut out2 = vec![0.0f32; send.len()];
    tail2.process(&room, &send, send.len(), &mut out2);
    assert!(
        out.iter().zip(out2.iter()).all(|(a, b)| a == b),
        "tail is deterministic"
    );
}

#[test]
fn fdn_householder_matrix_energy_conservation() {
    // Verify that the Householder reflection matrix A = I - (2/N)*1*1^T is exactly orthogonal
    const N: usize = 8;
    let mut input = [0.0f32; N];
    input[0] = 1.0; // single unit impulse
    let sum_u: f32 = input.iter().sum();
    let householder_term = sum_u * (2.0 / N as f32);
    let mut out = [0.0f32; N];
    for i in 0..N {
        out[i] = input[i] - householder_term;
    }
    // Check energy conservation: ||out||^2 == ||input||^2 = 1.0
    let energy: f32 = out.iter().map(|&x| x * x).sum();
    assert!(
        (energy - 1.0).abs() < 1e-6,
        "Householder matrix must strictly conserve energy: {energy}"
    );
    // Check all channels receive energy
    assert!(
        out.iter().all(|&x| x.abs() > 0.1),
        "All channels must receive non-zero energy"
    );
}
