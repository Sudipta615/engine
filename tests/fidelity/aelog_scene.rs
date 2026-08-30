//! Fidelity tests — animated acoustic worlds in aelog (v3.37).
//!
//! A `BakedScene` swap is now a recorded command
//! (`SetBakedScene { at, scene }`, stamped with the master sample at record
//! time), so an animated acoustic world replays its geometry timeline
//! deterministically:
//! * the scene embeds in the aelog JSON verbatim (order-stable serde —
//!   `BTreeMap` cache, `f32::INFINITY` low-pass sentinel mapped to −1.0,
//!   solver world skipped) and identical sessions hash identically;
//! * `replay_render` re-attaches each swap at its sample without resetting
//!   the `Acoustic` nodes' tapped delay lines — the response switches
//!   taps exactly at the swap while the room keeps ringing;
//! * the golden render is reproducible: a second replay and the
//!   aelog-hash cache return byte-identical captures.

use engine::prelude::{
    graph_fingerprint, log_hash, render_cached, replay_events, replay_render, spectral_taps,
    AcousticBaker, AcousticRoom, AcousticWorld, AelogCache, AelogRecorder, BakePolicy, BakedScene,
    ExecutionOrder, Graph2, MaterialKind, MaterialSpectrum, NodeId, PortId, Room, TestSignal,
    TransportState, Vec3, ACOUSTIC_IR_LEN,
};

const SR: f32 = 48_000.0;
const BLOCK: u64 = 256;

/// Two bakes of the same room, one with a fabric left wall — scene A is
/// reflective, scene B darkens the reflections that hit MinX.
fn two_scenes() -> (BakedScene, BakedScene) {
    let room = Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        rt60_ms: 800.0,
        late_mix: 0.0,
        speed_of_sound: 343.0,
    };
    let base = MaterialSpectrum::flat_reflective(room.absorption);
    let a = AcousticWorld::new(AcousticRoom::from_render_room(&room, base), SR);
    let mut room_b = AcousticRoom::from_render_room(&room, base);
    room_b.walls[engine::spatial::wall_index(engine::spatial::Wall::MinX)] =
        MaterialKind::Fabric.spectrum();
    let b = AcousticWorld::new(room_b, SR);
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let scene_a = AcousticBaker::new(a, 0.5).bake_single(pos, lst, SR, BakePolicy::default());
    let scene_b = AcousticBaker::new(b, 0.5).bake_single(pos, lst, SR, BakePolicy::default());
    (scene_a, scene_b)
}

const POS: Vec3 = Vec3::new(1.0, 5.0, 1.5);

/// sine(160 Hz) → acoustic node → sink, compiled.
fn sine_room_graph() -> (Graph2, ExecutionOrder, NodeId) {
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        engine::prelude::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let room = g.add_acoustic("room", POS);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, room, PortId::IN).unwrap();
    g.add_edge(room, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    (g, order, sink)
}

/// The raw sine the source injects (also the acoustic node's unbaked
/// pass-through, which forwards the input scaler-free).
fn passthrough(frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|k| (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin())
        .collect()
}

/// The per-sample oracle the acoustic node must produce against the
/// continuous sine input: the direct path times the input plus each
/// non-direct path's **spectral filter** convolved and delayed by its
/// excess — the v3.40 per-path filtering (a flat scene collapses to
/// `/gain·x[k−excess]`, the classic form). The scene only selects the taps
/// — the input history is the same before and after a position/scene
/// change (the delay line keeps ringing), so the oracle takes no change
/// point. Shares [`spectral_taps`] with `run_acoustic`, so a rendered room
/// and its expected curve always agree.
fn oracle(scene: &BakedScene, frames: usize) -> Vec<f32> {
    oracle_at(scene, POS, frames)
}

/// [`oracle`] for an explicit source position `p`.
fn oracle_at(scene: &BakedScene, p: Vec3, frames: usize) -> Vec<f32> {
    let obj = scene.get(p).expect("baked object");
    let direct = obj.direct().map(|d| d.gain).unwrap_or(1.0);
    let taps: Vec<(i64, Vec<f32>)> = spectral_taps(obj, ACOUSTIC_IR_LEN);
    let x = |k: isize| {
        if k < 0 {
            0.0
        } else {
            (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin()
        }
    };
    let mut out = vec![0.0f32; frames];
    for (k, o) in out.iter_mut().enumerate() {
        let kk = k as isize;
        *o = direct * x(kk);
        for (excess, kern) in &taps {
            for (j, &hj) in kern.iter().enumerate() {
                *o += hj * x(kk - *excess as isize - j as isize);
            }
        }
    }
    out
}

/// Assert `capture` matches the piecewise oracle: scene A's taps before
/// `swap_at`, scene B's from `swap_at` (tolerance — the sine source
/// accumulates phase, the oracle evaluates the absolute angle).
fn assert_piecewise(capture: &[f32], a: &BakedScene, b: &BakedScene, swap_at: usize) {
    let oracle_a = oracle(a, capture.len());
    let oracle_b = oracle(b, capture.len());
    for (k, got) in capture.iter().enumerate() {
        let want = if k < swap_at {
            oracle_a[k]
        } else {
            oracle_b[k]
        };
        assert!(
            (got - want).abs() < 1e-4,
            "sample {k}: got {got}, oracle {want}"
        );
    }
}

/// The animated session: scene A from master 0, swap to scene B at
/// `swap_blocks` blocks in, run `total_blocks` total.
fn animated_session(a: &BakedScene, b: &BakedScene, swap_blocks: u64) -> engine::prelude::Aelog {
    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    rec.record_baked_scene(a); // initial world at master 0
    for i in 0..swap_blocks {
        if i == swap_blocks / 2 {
            // A second swap back to A mid-way proves arbitrary swap timing.
            rec.record_baked_scene(a);
        }
        rec.advance_block(BLOCK);
    }
    rec.record_baked_scene(b); // door opens / wall turns to fabric
    for _ in swap_blocks..20 {
        rec.advance_block(BLOCK);
    }
    rec.finish()
}

#[test]
fn scene_swaps_replay_deterministically_and_match_the_oracle() {
    let (a, b) = two_scenes();
    let (g, order, sink) = sine_room_graph();
    let swap_at = (10 * BLOCK) as usize; // 2560 samples in

    let log = animated_session(&a, &b, 10);
    let out = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.captured.len(), (20 * BLOCK) as usize);

    // The render is the piecewise oracle: A's taps before the swap, B's
    // from the swap sample onward (sample-exact, live tail).
    assert_piecewise(&out.captured, &a, &b, swap_at);

    // Golden: a second replay of the same log is byte-identical.
    let again = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.captured, again.captured, "golden render reproducible");

    // The trajectory is exposed: initial attach at 0, the mid-session
    // return to A, then the swap to B at the exact sample.
    let events = replay_events(&log).unwrap();
    assert_eq!(events.scene_swaps.len(), 3);
    assert_eq!(events.scene_swaps[0].0, 0);
    assert_eq!(events.scene_swaps[0].1, a, "initial scene A");
    assert_eq!(
        events.scene_swaps[1].0,
        5 * BLOCK,
        "mid-session return to A"
    );
    assert_eq!(
        events.scene_swaps[2].0,
        10 * BLOCK,
        "swap to B at the stamp"
    );
    assert_eq!(events.scene_swaps[2].1, b);
}

#[test]
fn the_swap_visibly_changes_the_render() {
    // The animated render must differ from both the A-only and B-only
    // renders — the swap is not a no-op and not a reset.
    let (a, b) = two_scenes();
    let (g, order, sink) = sine_room_graph();
    let swap_at = (10 * BLOCK) as usize;

    let log = animated_session(&a, &b, 10);
    let animated = replay_render(&log, &g, &order, sink).unwrap().captured;

    let a_only = oracle(&a, animated.len());
    let b_only = oracle(&b, animated.len());
    let diff = |v: &[f32], w: &[f32]| {
        v.iter()
            .zip(w.iter())
            .map(|(x, y)| (x - y).abs())
            .sum::<f32>()
    };
    let da = diff(&animated, &a_only);
    let db = diff(&animated, &b_only);
    assert!(da > 1e-3, "animated differs from scene-A-only (diff {da})");
    assert!(db > 1e-3, "animated differs from scene-B-only (diff {db})");

    // And the boundary is sample-exact: the last sample before the swap is
    // still A's, the first after it is already B's.
    assert!(
        (animated[swap_at - 1] - a_only[swap_at - 1]).abs() < 1e-4,
        "sample swap_at-1 still scene A"
    );
    assert!(
        (animated[swap_at] - b_only[swap_at]).abs() < 1e-4,
        "sample swap_at is scene B"
    );
}

#[test]
fn scenes_in_the_log_hash_deterministically_and_use_the_cache() {
    let (a, b) = two_scenes();
    let (g, order, sink) = sine_room_graph();

    // Identical sessions (same scene snapshots) hash identically; the
    // scene is part of the key, so swapping a different scene in changes
    // both the hash and the render.
    let log1 = animated_session(&a, &b, 10);
    let log2 = animated_session(&a, &b, 10);
    assert_eq!(log_hash(&log1), log_hash(&log2), "identical sessions");
    let log_diff = animated_session(&b, &a, 10); // swapped roles
    assert_ne!(
        log_hash(&log1),
        log_hash(&log_diff),
        "scene changes the hash"
    );
    assert_ne!(
        graph_fingerprint(&g),
        0,
        "graph fingerprint still part of the key"
    );

    // The golden cache: the second render_cached reuses the stored capture
    // and it is byte-identical to a fresh replay_render.
    let root = std::env::temp_dir().join(format!("aelog-scene-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cache = AelogCache::new(root.clone());
    let cold = cache.render_cached(&log1, &g, &order, sink).unwrap();
    let warm = cache.render_cached(&log1, &g, &order, sink).unwrap();
    let direct = replay_render(&log1, &g, &order, sink).unwrap();
    assert_eq!(cold.captured, warm.captured, "cache hit == cold render");
    assert_eq!(warm.captured, direct.captured, "cached == fresh golden");
    assert_eq!(
        cache.lookup(&log1, &g, sink),
        Some(cold.captured.clone()),
        "entry stored under the log+graph+sink key"
    );
    // A different scene session is a different entry.
    assert_eq!(cache.lookup(&log_diff, &g, sink), None);

    // The no-constructor convenience path agrees. It uses the persistent
    // default cache root, so clear it first — otherwise a stale golden from
    // an earlier build (same key, older render) would be served instead of
    // a fresh render.
    if let Some(dflt) = AelogCache::default_root() {
        let _ = std::fs::remove_dir_all(&dflt);
    }
    let via_fn = render_cached(&log1, &g, &order, sink).unwrap();
    assert_eq!(via_fn.captured, direct.captured);
    if let Some(dflt) = AelogCache::default_root() {
        let _ = std::fs::remove_dir_all(&dflt);
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn listener_trajectory_drives_the_acoustic_node_golden() {
    // The full spatial baked-room path, exercised dynamically: baked cells
    // at L0 and L1 (two distinct positions), unbaked beyond. A replayed
    // listener trajectory drives the Acoustic node's lookup, so the render
    // walks different baked cells (and a pass-through) over time, and is
    // byte-reproducible as a golden.
    let room = Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        rt60_ms: 800.0,
        late_mix: 0.0,
        speed_of_sound: 343.0,
    };
    let world = AcousticWorld::new(
        AcousticRoom::from_render_room(&room, MaterialSpectrum::flat_reflective(room.absorption)),
        SR,
    );
    let baker = AcousticBaker::new(world, 0.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let l0 = Vec3::new(1.0, 5.0, 1.5);
    let l1 = Vec3::new(1.0, 6.5, 1.5);
    let lx = Vec3::new(1.0, 9.0, 1.5); // unbaked cell
    let scene = baker.bake_scene([l0, l1], lst, SR, BakePolicy::default());
    assert_ne!(
        scene.get(l0).unwrap().key,
        scene.get(l1).unwrap().key,
        "two cells"
    );
    assert!(scene.get(lx).is_none(), "lx is genuinely unbaked");

    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        engine::prelude::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let room_node = g.add_acoustic("room", l0);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, room_node, PortId::IN).unwrap();
    g.add_edge(room_node, PortId::OUT, sink, PortId::IN)
        .unwrap();
    let order = g.compile().unwrap().clone();

    // The session: scene A from 0, listener walks L0 → L1 → Lx at blocks
    // 0 / 5 / 10, rendering 15 blocks total.
    let mut rec = AelogRecorder::new(SR, BLOCK);
    rec.set_state(TransportState::Playing, 0);
    rec.record_baked_scene(&scene);
    rec.record_listener_position(l0); // at master 0
    for b in 0..15u64 {
        if b == 5 {
            rec.record_listener_position(l1);
        }
        if b == 10 {
            rec.record_listener_position(lx);
        }
        rec.advance_block(BLOCK);
    }
    let log = rec.finish();

    let frames = (15 * BLOCK) as usize;
    let m1 = (5 * BLOCK) as usize;
    let m2 = (10 * BLOCK) as usize;
    // Full-length oracles (each against the continuous sine from sample 0);
    // the drive selects which response applies at each sample.
    let seg0 = oracle_at(&scene, l0, frames); // L0's cell
    let seg1 = oracle_at(&scene, l1, frames); // L1's cell
    let seg2 = passthrough(frames); // unbaked → pure sine

    let out = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.captured.len(), frames);
    for (k, got) in out.captured.iter().enumerate() {
        let want = if k < m1 {
            seg0[k]
        } else if k < m2 {
            seg1[k]
        } else {
            seg2[k]
        };
        assert!(
            (got - want).abs() < 1e-4,
            "sample {k}: got {got}, oracle {want}"
        );
    }

    // Golden: a second replay is byte-identical, and the hash cache reuses
    // the stored capture.
    let again = replay_render(&log, &g, &order, sink).unwrap();
    assert_eq!(out.captured, again.captured, "golden render reproducible");
    let root = std::env::temp_dir().join(format!("aelog-listen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cache = AelogCache::new(root.clone());
    let cold = cache.render_cached(&log, &g, &order, sink).unwrap();
    let warm = cache.render_cached(&log, &g, &order, sink).unwrap();
    assert_eq!(cold.captured, warm.captured, "cache hit == cold render");
    assert_eq!(warm.captured, out.captured, "cached == fresh golden");
    let _ = std::fs::remove_dir_all(&root);

    // The full input timeline is exposed: scene swap + listener trajectory.
    let events = replay_events(&log).unwrap();
    assert_eq!(events.scene_swaps.len(), 1);
    assert_eq!(events.scene_swaps[0].0, 0);
    assert_eq!(
        events.listener_motion,
        vec![(0, l0), (5 * BLOCK, l1), (10 * BLOCK, lx)],
        "listener trajectory stamped per move"
    );
}
