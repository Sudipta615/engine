//! Offline-executor test battery (v3.50 Phase 45 — moved from `exec.rs`
//! verbatim; imports adapted to the `exec/` split).
//!
//! The oracles here are the **bit-exact expectations** the realtime
//! executor must reproduce: `tests/fidelity/graph2_rt_offline_equivalence.rs`
//! asserts RT == offline across these same topologies.

use super::*;
use crate::spatial::acoustic::bake::{spectral_taps, ACOUSTIC_IR_LEN};
use crate::spatial::acoustic::path::PathKind;
use crate::spatial::hrtf::HrtfDataset;

/// The renderable **broadband** taps of a baked response: `(excess_delay,
/// gain)` per non-direct path (the classic collapsed form). Retained for
/// the flat-material oracles and as the reduction a wholly flat path
/// equals; `run_acoustic` now applies true per-path spectral filters via
/// [`spectral_taps`], which collapses to these exact taps when every path
/// is flat.
pub(crate) fn acoustic_taps(obj: &crate::spatial::acoustic::bake::BakedObject) -> Vec<(i64, f32)> {
    let direct_delay = obj.direct().map(|d| d.delay_samples).unwrap_or(0.0);
    obj.paths
        .iter()
        .filter(|p| p.kind != crate::spatial::acoustic::path::PathKind::Direct)
        .map(|p| {
            let excess = (p.delay_samples - direct_delay).max(0.0).round() as i64;
            (excess, p.gain)
        })
        .collect()
}

const SR: f32 = 48_000.0;

fn dry_wet_graph() -> (Graph2, ExecutionOrder, NodeId, NodeId) {
    // source → split(2) → {gain(0.5), delay(100)} → mix → sink
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("drywet", 2);
    let gain = g.add_gain("dry", 0.5);
    let delay = g.add_delay("wet", 100);
    let mix = g.add_mix("sum", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), gain, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), delay, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(delay, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    (g, order, sink, src)
}

#[test]
fn dry_wet_split_and_mix_render() {
    let (g, order, sink, _src) = dry_wet_graph();
    let mut ex = OfflineExecutor::new(&g, &order, 128, SR).unwrap();
    ex.process_blocks(2).unwrap();
    let cap = ex.capture(sink).unwrap();
    assert_eq!(cap.len(), 256);
    // Dry impulse at frame 0 scaled by 0.5.
    assert!((cap[0] - 0.5).abs() < 1e-6, "dry: {}", cap[0]);
    // Wet impulse at frame 100, unscaled.
    assert!((cap[100] - 1.0).abs() < 1e-6, "wet: {}", cap[100]);
    // Nothing else.
    assert!(cap[1..100].iter().all(|s| s.abs() < 1e-6));
    assert!(cap[101..].iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn delay_across_block_boundary() {
    // Delay(300) with 128-frame blocks: the impulse written in block 0
    // must arrive at absolute sample 300.
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let d = g.add_delay("d", 300);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, d, PortId::IN).unwrap();
    g.add_edge(d, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 128, SR).unwrap();
    ex.process_blocks(3).unwrap();
    let cap = ex.capture(sink).unwrap();
    assert_eq!(cap.len(), 384);
    assert!(cap[..300].iter().all(|s| s.abs() < 1e-6));
    assert!((cap[300] - 1.0).abs() < 1e-6);
    assert!(cap[301..].iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn sine_source_is_continuous_across_blocks() {
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "sine",
        crate::dsp::graph2::node::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 440.0,
        },
    );
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 128, SR).unwrap();
    ex.process_blocks(2).unwrap();
    let cap = ex.capture(sink).unwrap();
    assert_eq!(cap.len(), 256);
    // First sample starts at phase 0; sample 128 continues the sine —
    // the value at 128 must equal sin(2π·440·128/48000), i.e. the
    // continuous wave, not a restart.
    let expect = (2.0 * std::f32::consts::PI * 440.0 * 128.0 / SR).sin();
    assert!((cap[128] - expect).abs() < 1e-3, "{} vs {expect}", cap[128]);
    // And the wave is bounded.
    assert!(cap.iter().all(|s| s.abs() <= 1.0 + 1e-6));
}

#[test]
fn gain_step_is_sample_accurate() {
    // sine → gain(0.0) → sink. A step to 2.0 at local frame 40 must leave
    // frames [0,40) silent and start scaling with 2.0 exactly at 40.
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "sine",
        crate::dsp::graph2::node::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 1000.0,
        },
    );
    let gain = g.add_gain("vol", 0.0);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let mut ex = OfflineExecutor::new(&g, &order, 128, SR).unwrap();
    ex.set_gain_step(gain, 2.0, 40).unwrap();
    ex.process_block().unwrap();
    let cap = ex.capture(sink).unwrap();
    assert_eq!(cap.len(), 128);
    // Frames before the step are silent (base gain 0).
    assert!(cap[..40].iter().all(|s| s.abs() < 1e-6));
    // Frame 40 onward is 2× the raw sine.
    let raw = |i: usize| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR).sin();
    assert!((cap[40] - 2.0 * raw(40)).abs() < 1e-3, "step frame 40");
    assert!((cap[100] - 2.0 * raw(100)).abs() < 1e-3, "after step");
    // The stepped gain persists to the next block (block-quantized set).
    // Block 1's frame 40 is absolute sample 128 + 40 = 168.
    ex.process_block().unwrap();
    let cap2 = ex.capture(sink).unwrap();
    let abs = 128 + 40;
    assert!(
        (cap2[abs] - 2.0 * raw(abs)).abs() < 1e-3,
        "persists into next block: {} vs {}",
        cap2[abs],
        2.0 * raw(abs)
    );
}

#[test]
fn tempo_mapped_gain_automation_ramps_across_blocks() {
    // DC(1) → gain(0) → sink. A tempo-mapped curve (beat 0 → 0.0,
    // beat 2 → 1.0) at 120 BPM sweeps the gain linearly 0 → 1 over two
    // beats (48000 samples). The captured output equals the curve's
    // value at each sample — the gain is the signal.
    use crate::dsp::timeline::automation::CurveBeats;
    use crate::dsp::timeline::tempo::TempoMap;

    let block = 1200usize; // 120 BPM → 1 beat = 24000 samples = 20 blocks
    let mut g = Graph2::new();
    let src = g.add_buffer("dc", vec![1.0], true);
    let gain = g.add_gain("vol", 0.0);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let mut map = TempoMap::new();
    map.push(0.0, 120.0);
    let curve = CurveBeats::from_points(&[(0.0, 0.0), (2.0, 1.0)]).unwrap();

    let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
    ex.set_tempo_map(Some(map));
    ex.set_gain_automation(gain, Some(curve));
    ex.process_blocks(40).unwrap(); // 48000 samples = 2 beats
    let cap = ex.capture(sink).unwrap();
    assert_eq!(cap.len(), 48_000);

    // Sample s → beat = s / 24000 → gain = s / 48000 (linear 0→1).
    let expect = |s: usize| s as f32 / 48_000.0;
    for s in [0, 4_799, 12_000, 23_999, 24_001, 36_000, 47_999] {
        assert!(
            (cap[s] - expect(s)).abs() < 2e-4,
            "sample {s}: got {}, want {}",
            cap[s],
            expect(s)
        );
    }
    assert!((cap[0] - 0.0).abs() < 1e-6);
    assert!((cap[47_999] - expect(47_999)).abs() < 2e-4, "reaches ~1.0");
    // The ramp is strictly non-decreasing (monotone DC → volume rises).
    assert!(cap.windows(100).all(|w| w[0] <= w[99] + 1e-5));

    // Detaching the curve (or clearing the tempo map) restores the
    // static gain 0.
    let mut ex2 = OfflineExecutor::new(&g, &order, block, SR).unwrap();
    ex2.set_tempo_map(Some(TempoMap::new()));
    ex2.set_gain_automation(
        gain,
        Some(CurveBeats::from_points(&[(0.0, 0.4), (4.0, 0.4)]).unwrap()),
    );
    ex2.set_gain_automation(gain, None);
    ex2.process_blocks(2).unwrap();
    assert!(ex2.capture(sink).unwrap().iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn a_tempo_change_shifts_where_automation_landmarks_land() {
    // Same curve, but the tempo doubles at beat 1: the value at a *beat*
    // is identical, yet the sample where the gain reaches 0.75 moves —
    // beat 1.5 = beat 1 (at 24000 samples @120) + 0.5 beat @240 (6000)
    // = sample 30000.
    use crate::dsp::timeline::automation::CurveBeats;
    use crate::dsp::timeline::tempo::TempoMap;

    let block = 100usize;
    let mut g = Graph2::new();
    let src = g.add_buffer("dc", vec![1.0], true);
    let gain = g.add_gain("vol", 0.0);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let mut map = TempoMap::new();
    map.push(0.0, 120.0);
    map.push(1.0, 240.0);
    let curve = CurveBeats::from_points(&[(0.0, 0.0), (2.0, 1.0)]).unwrap();

    let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
    ex.set_tempo_map(Some(map.clone()));
    ex.set_gain_automation(gain, Some(curve));
    // Run 30100 samples = 301 blocks (1.5 beats under the changing map,
    // plus one block so sample 30000 exists in the capture).
    ex.process_blocks(301).unwrap();
    let cap = ex.capture(sink).unwrap();
    // The curve value is piecewise-linear in *beats*: 0 at beat 0, 1.0 at
    // beat 2. So value at beat 1.5 = 0.75, and beat 1.5 sits at sample
    // 30000 after the tempo-up.
    assert!(
        (cap[30_000] - 0.75).abs() < 5e-4,
        "gain 0.75 at beat 1.5 = sample 30000: got {}",
        cap[30_000]
    );
    // Before the tempo-up, at 120 BPM the midpoint (beat 1 = sample 24000)
    // is 0.5.
    assert!(
        (map.beat_at_sample(24_000.0, SR) - 1.0).abs() < 1e-9 && (cap[24_000] - 0.5).abs() < 5e-4,
        "beat 1 (sample 24000) still 0.5"
    );
}

#[test]
fn acoustic_node_reproduces_baked_room_response() {
    use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
    use crate::spatial::acoustic::geometry::AcousticRoom;
    use crate::spatial::acoustic::material::MaterialSpectrum;
    use crate::spatial::acoustic::solver::AcousticWorld;
    use crate::spatial::math::Vec3;
    use crate::spatial::room::Room;

    let room = Room {
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
    };
    let world = AcousticWorld::new(
        AcousticRoom::from_render_room(&room, MaterialSpectrum::flat_reflective(room.absorption)),
        SR,
    );
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let scene = AcousticBaker::new(world, 0.5).bake_single(pos, lst, SR, BakePolicy::default());

    // Oracle first (releases the scene borrow): direct at 0 (direct
    // gain), plus each non-direct path at its excess delay with its
    // gain — the same arithmetic the node runs.
    let obj = scene.get(pos).expect("baked object");
    let direct_delay = obj.direct().unwrap().delay_samples;
    let mut expected = vec![0.0f32; 2048];
    expected[0] = obj.direct().unwrap().gain;
    for p in obj.paths.iter().filter(|p| p.kind != PathKind::Direct) {
        let excess = (p.delay_samples - direct_delay).max(0.0).round() as usize;
        if excess < 2048 {
            expected[excess] += p.gain;
        }
    }
    assert!(
        expected.iter().skip(1).any(|s| s.abs() > 1e-6),
        "bake has reflections"
    );

    // Now the graph side: an impulse into the Acoustic node with the
    // baked scene attached must reproduce the oracle exactly.
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let room_node = g.add_acoustic("room", pos);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, room_node, PortId::IN).unwrap();
    g.add_edge(room_node, PortId::OUT, sink, PortId::IN)
        .unwrap();
    let order = g.compile().unwrap().clone();

    let mut ex = OfflineExecutor::new(&g, &order, 512, SR).unwrap();
    ex.set_baked_scene(Some(scene));
    ex.process_blocks(4).unwrap(); // 2048 frames — covers the reflection tail
    let cap = ex.capture(sink).unwrap();
    assert_eq!(cap, expected, "graph acoustic node == baked response");
}

#[test]
fn acoustic_node_honours_the_scene_air_absorption_model() {
    // v3.48: the same baked room, rendered through the Acoustic node
    // twice — once with the scene's air-absorption model enabled and once
    // without. Enabling it must darken the (flat-wall) reflections into
    // distance-dependent low-pass tails, spreading the reflection energy
    // over more output frames than the classic single-tap path.
    use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
    use crate::spatial::acoustic::geometry::AcousticRoom;
    use crate::spatial::acoustic::material::MaterialSpectrum;
    use crate::spatial::acoustic::solver::AcousticWorld;
    use crate::spatial::level::AirAbsorption;
    use crate::spatial::math::Vec3;
    use crate::spatial::room::Room;

    let room = Room {
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
    };
    let world = AcousticWorld::new(
        AcousticRoom::from_render_room(&room, MaterialSpectrum::flat_reflective(room.absorption)),
        SR,
    );
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let base = AcousticBaker::new(world, 0.5).bake_single(pos, lst, SR, BakePolicy::default());
    let mut scene = base.clone();
    scene.set_air_absorption(AirAbsorption {
        enabled: true,
        ..Default::default()
    });

    let render = |scene: crate::spatial::acoustic::bake::BakedScene| -> Vec<f32> {
        let mut g = Graph2::new();
        let src = g.add_source("imp");
        let room_node = g.add_acoustic("room", pos);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, room_node, PortId::IN).unwrap();
        g.add_edge(room_node, PortId::OUT, sink, PortId::IN)
            .unwrap();
        let order = g.compile().unwrap().clone();
        let mut ex = OfflineExecutor::new(&g, &order, 512, SR).unwrap();
        ex.set_baked_scene(Some(scene));
        ex.process_blocks(4).unwrap();
        ex.capture(sink).unwrap().to_vec()
    };
    let off = render(base);
    let on = render(scene);
    // Direct (frame 0) is identical; only reflections diverge.
    assert!((off[0] - on[0]).abs() < 1e-6, "direct unaffected by air");
    let spread = |c: &[f32]| {
        c.iter()
            .enumerate()
            .skip(1)
            .filter(|&(_, &v)| v.abs() > 1e-4)
            .count()
    };
    assert!(
        spread(&on) > spread(&off),
        "air darkens reflections into tails (off {} vs on {})",
        spread(&off),
        spread(&on)
    );
    assert!(on.iter().all(|v| v.is_finite()));
}

#[test]
fn swap_baked_scene_switches_taps_without_cutting_the_tail() {
    // A sine through the Acoustic node with scene A (concrete), then a
    // mid-session swap to scene B (fabric MinX wall — weaker
    // reflections). The swap must take effect from the next block while
    // the tapped delay line keeps ringing: output[k] = direct·x[k] +
    // Σ gain_e·x[k−e] with A's taps before the swap and B's after, all
    // from the same continuous input history.
    use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
    use crate::spatial::acoustic::geometry::AcousticRoom;
    use crate::spatial::acoustic::material::{MaterialKind, MaterialSpectrum};
    use crate::spatial::acoustic::solver::wall_index;
    use crate::spatial::acoustic::solver::AcousticWorld;
    use crate::spatial::math::Vec3;
    use crate::spatial::room::Room;

    let room = Room {
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
    };
    let base = MaterialSpectrum::flat_reflective(room.absorption);
    let world_a = AcousticWorld::new(AcousticRoom::from_render_room(&room, base), SR);
    let mut room_b = AcousticRoom::from_render_room(&room, base);
    room_b.walls[wall_index(crate::spatial::Wall::MinX)] = MaterialKind::Fabric.spectrum();
    let world_b = AcousticWorld::new(room_b, SR);
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let scene_a = AcousticBaker::new(world_a, 0.5).bake_single(pos, lst, SR, BakePolicy::default());
    let scene_b = AcousticBaker::new(world_b, 0.5).bake_single(pos, lst, SR, BakePolicy::default());

    // Oracle taps per scene: per-path spectral filter kernels (excess,
    // kernel). Scene A is flat (single-tap gain kernels, matching the
    // classic broadband form); scene B's fabric wall spectrally colours
    // its reflections. Read before the scenes move into the executor.
    let taps = |scene: &crate::spatial::acoustic::bake::BakedScene| {
        spectral_taps(scene.get(pos).unwrap(), ACOUSTIC_IR_LEN)
    };
    let taps_a = taps(&scene_a);
    let taps_b = taps(&scene_b);
    let dg_a = scene_a
        .get(pos)
        .unwrap()
        .direct()
        .map(|d| d.gain)
        .unwrap_or(1.0);
    let dg_b = scene_b
        .get(pos)
        .unwrap()
        .direct()
        .map(|d| d.gain)
        .unwrap_or(1.0);
    // Fabric dampens the *broadband* reflected energy (its low-pass
    // colours rather than merely scales — the spectral point of v3.40).
    let bband = |scene: &crate::spatial::acoustic::bake::BakedScene| {
        let obj = scene.get(pos).unwrap();
        obj.paths
            .iter()
            .filter(|p| p.kind != crate::spatial::acoustic::path::PathKind::Direct)
            .map(|p| p.gain.abs())
            .sum::<f32>()
    };
    assert!(
        bband(&scene_a) > bband(&scene_b),
        "fabric weakens the reflections"
    );

    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        crate::dsp::graph2::node::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let room_node = g.add_acoustic("room", pos);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, room_node, PortId::IN).unwrap();
    g.add_edge(room_node, PortId::OUT, sink, PortId::IN)
        .unwrap();
    let order = g.compile().unwrap().clone();

    let block = 256usize;
    let swap_at = 4 * block; // after four blocks, mid-session
    let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
    ex.set_baked_scene(Some(scene_a));
    ex.process_blocks(4).unwrap();
    ex.swap_baked_scene(scene_b);
    ex.process_blocks(4).unwrap();
    let cap = ex.capture(sink).unwrap();

    let x = |k: isize| {
        if k < 0 {
            0.0
        } else {
            (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin()
        }
    };
    let oracle = |k: isize, t: &[(i64, Vec<f32>)], dg: f32| {
        let mut v = dg * x(k);
        for (excess, kern) in t {
            for (j, &hj) in kern.iter().enumerate() {
                v += hj * x(k - *excess as isize - j as isize);
            }
        }
        v
    };
    let mut expected = vec![0.0f32; cap.len()];
    for (k, e) in expected.iter_mut().enumerate() {
        let (t, dg) = if (k as isize) < swap_at as isize {
            (&taps_a, dg_a)
        } else {
            (&taps_b, dg_b)
        };
        *e = oracle(k as isize, t, dg);
    }
    assert!(
        cap[swap_at + 1..].iter().any(|s| s.abs() > 0.5),
        "ringing continues"
    );
    // Tolerance, not bit-exact: the sine source accumulates phase
    // incrementally while the oracle evaluates the absolute angle, so
    // the two differ in float low bits (the aelog golden checks assert
    // byte-exactness between two runs of the *same* code path).
    for (k, (got, want)) in cap.iter().zip(expected.iter()).enumerate() {
        assert!(
            (got - want).abs() < 1e-4,
            "sample {k}: got {got}, oracle {want}"
        );
    }
    // The swap visibly changes the response (fabric weakens the taps).
    let a_only = |k: isize| oracle(k, &taps_a, dg_a);
    let b_only = |k: isize| oracle(k, &taps_b, dg_b);
    let mid = swap_at + 600;
    assert!(
        (a_only(mid as isize) - b_only(mid as isize)).abs() > 1e-3,
        "the swap changes the rendered response"
    );
}

#[test]
fn listener_position_drives_the_acoustic_lookup() {
    // Bake two source cells P0 and P1 of the same room. A listener
    // drive retargets the node's lookup from P0 to P1 (and back),
    // rendering each cell's own response against the continuous sine
    // history — the moving-listener path.
    use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
    use crate::spatial::acoustic::geometry::AcousticRoom;
    use crate::spatial::acoustic::material::MaterialSpectrum;
    use crate::spatial::acoustic::solver::AcousticWorld;
    use crate::spatial::math::Vec3;
    use crate::spatial::room::Room;

    let room = Room {
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
    };
    let world = AcousticWorld::new(
        AcousticRoom::from_render_room(&room, MaterialSpectrum::flat_reflective(room.absorption)),
        SR,
    );
    let baker = AcousticBaker::new(world, 0.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let p0 = Vec3::new(1.0, 5.0, 1.5);
    let p1 = Vec3::new(1.0, 6.5, 1.5); // a different cell (y + 1.5)
    let scene = baker.bake_scene([p0, p1], lst, SR, BakePolicy::default());
    assert_ne!(
        scene.get(p0).unwrap().key,
        scene.get(p1).unwrap().key,
        "two distinct baked cells"
    );

    // Owned per-cell handles, computed before the scene moves into the
    // executor (no closure keeps a borrow on `scene`).
    let taps0 = acoustic_taps(scene.get(p0).unwrap());
    let taps1 = acoustic_taps(scene.get(p1).unwrap());
    let dg0 = scene
        .get(p0)
        .unwrap()
        .direct()
        .map(|d| d.gain)
        .unwrap_or(1.0);
    let dg1 = scene
        .get(p1)
        .unwrap()
        .direct()
        .map(|d| d.gain)
        .unwrap_or(1.0);

    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        crate::dsp::graph2::node::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let room_node = g.add_acoustic("room", p0);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, room_node, PortId::IN).unwrap();
    g.add_edge(room_node, PortId::OUT, sink, PortId::IN)
        .unwrap();
    let order = g.compile().unwrap().clone();

    let block = 256usize;
    let move_at = 5 * block;
    let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
    ex.set_baked_scene(Some(scene));
    ex.process_blocks(5).unwrap(); // renders p0 (the node's baked position)
    ex.set_listener_position(Some(p1)); // listener walks to p1
    ex.process_blocks(3).unwrap();
    ex.set_listener_position(Some(p0)); // and back
    ex.process_blocks(2).unwrap();
    let cap = ex.capture(sink).unwrap();

    let x = |k: isize| {
        if k < 0 {
            0.0
        } else {
            (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin()
        }
    };
    let oracle = |k: isize, t: &[(i64, f32)], d: f32| {
        let mut v = d * x(k);
        for &(excess, gain) in t {
            v += gain * x(k - excess as isize);
        }
        v
    };
    let seg_for = |k: usize| {
        if k < move_at {
            (taps0.as_slice(), dg0)
        } else if k < move_at + 3 * block {
            (taps1.as_slice(), dg1)
        } else {
            (taps0.as_slice(), dg0)
        }
    };
    for (k, got) in cap.iter().enumerate() {
        let (t, d) = seg_for(k);
        assert!(
            (got - oracle(k as isize, t, d)).abs() < 1e-4,
            "sample {k}: got {got}"
        );
    }
    // The motion changed the render: mid-segment differs from the
    // p0-only oracle.
    let mid = move_at + block + 100;
    assert!(
        (cap[mid] - oracle(mid as isize, &taps0, dg0)).abs() > 1e-3,
        "moving the listener visibly changed the response"
    );
    // Clearing the drive restores the node's own position (the p0
    // oracle at the very first sample).
    let mut ex2 = OfflineExecutor::new(&g, &order, block, SR).unwrap();
    ex2.set_baked_scene(ex.baked.clone());
    ex2.set_listener_position(Some(p1));
    ex2.set_listener_position(None);
    ex2.process_blocks(1).unwrap();
    let first = ex2.capture(sink).unwrap();
    assert!(
        (first[0] - oracle(0, &taps0, dg0)).abs() < 1e-4,
        "None restores the node's baked position"
    );
}

#[test]
fn named_scenes_render_per_listener_responses() {
    // Two bakes of the same room, for two different listener positions
    // but the SAME source cell: one named scene per listener. Two
    // Acoustic nodes (same source, different scene ids) must each
    // render their own listener's response; an unregistered id falls
    // back to pass-through; replacing a named scene changes that node.
    use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
    use crate::spatial::acoustic::geometry::AcousticRoom;
    use crate::spatial::acoustic::material::MaterialSpectrum;
    use crate::spatial::acoustic::solver::AcousticWorld;
    use crate::spatial::math::Vec3;
    use crate::spatial::room::Room;

    let room = Room {
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
    };
    let world = AcousticWorld::new(
        AcousticRoom::from_render_room(&room, MaterialSpectrum::flat_reflective(room.absorption)),
        SR,
    );
    let baker = AcousticBaker::new(world, 0.5);
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let front = baker.bake_single(pos, Vec3::new(6.0, 5.0, 1.5), SR, BakePolicy::default());
    let back = baker.bake_single(pos, Vec3::new(6.0, 2.0, 1.5), SR, BakePolicy::default());
    assert_ne!(front.get(pos).unwrap().paths, back.get(pos).unwrap().paths);

    // Oracle: the node's impulse response for one scene at `pos`.
    let response = |scene: &crate::spatial::acoustic::bake::BakedScene| {
        let obj = scene.get(pos).unwrap();
        let direct_delay = obj.direct().unwrap().delay_samples;
        let mut e = vec![0.0f32; 2048];
        e[0] = obj.direct().unwrap().gain;
        for p in obj.paths.iter().filter(|p| p.kind != PathKind::Direct) {
            let excess = (p.delay_samples - direct_delay).max(0.0).round() as usize;
            if excess < e.len() {
                e[excess] += p.gain;
            }
        }
        e
    };
    let exp_front = response(&front);
    let exp_back = response(&back);
    assert_ne!(
        exp_front, exp_back,
        "different listeners → different responses"
    );

    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("s", 2);
    let n_front = g.add_acoustic_scene("front", pos, "front");
    let n_back = g.add_acoustic_scene("back", pos, "back");
    let sink_f = g.add_sink("f");
    let sink_b = g.add_sink("b");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), n_front, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), n_back, PortId::IN).unwrap();
    g.add_edge(n_front, PortId::OUT, sink_f, PortId::IN)
        .unwrap();
    g.add_edge(n_back, PortId::OUT, sink_b, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let mut ex = OfflineExecutor::new(&g, &order, 512, SR).unwrap();
    ex.set_scene("front", front);
    ex.set_scene("back", back);
    ex.process_blocks(4).unwrap();
    assert_eq!(ex.capture(sink_f).unwrap(), exp_front, "front listener");
    assert_eq!(ex.capture(sink_b).unwrap(), exp_back, "back listener");

    // An unregistered scene id passes the input through unchanged.
    let mut g2 = Graph2::new();
    let s2 = g2.add_source("imp");
    let unknown = g2.add_acoustic_scene("u", pos, "missing");
    let k2 = g2.add_sink("k");
    g2.add_edge(s2, PortId::OUT, unknown, PortId::IN).unwrap();
    g2.add_edge(unknown, PortId::OUT, k2, PortId::IN).unwrap();
    let order2 = g2.compile().unwrap().clone();
    let mut ex2 = OfflineExecutor::new(&g2, &order2, 512, SR).unwrap();
    ex2.process_blocks(1).unwrap();
    let cap = ex2.capture(k2).unwrap();
    assert_eq!(cap[0], 1.0, "impulse passes through untouched");
    assert!(cap[1..].iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn buffer_plays_embedded_clip_one_shot_and_loops() {
    // One-shot: [1,2,3] then silence.
    let mut g = Graph2::new();
    let b = g.add_buffer("in", vec![1.0, 2.0, 3.0], false);
    let sink = g.add_sink("out");
    g.add_edge(b, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
    ex.process_blocks(2).unwrap();
    let cap = ex.capture(sink).unwrap();
    assert_eq!(cap, [1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0]);

    // Looping: [1,2,3] repeats.
    let mut g2 = Graph2::new();
    let b2 = g2.add_buffer("in", vec![1.0, 2.0, 3.0], true);
    let sink2 = g2.add_sink("out");
    g2.add_edge(b2, PortId::OUT, sink2, PortId::IN).unwrap();
    let order2 = g2.compile().unwrap().clone();
    let mut ex2 = OfflineExecutor::new(&g2, &order2, 4, SR).unwrap();
    ex2.process_blocks(2).unwrap();
    let cap2 = ex2.capture(sink2).unwrap();
    assert_eq!(cap2, [1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 1.0, 2.0]);
}

#[test]
fn external_input_overrides_buffer_clip() {
    let mut g = Graph2::new();
    let b = g.add_buffer("in", vec![], false); // empty embedded clip
    let sink = g.add_sink("out");
    g.add_edge(b, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 3, SR).unwrap();
    ex.set_external_input(Some(vec![vec![9.0, 8.0, 7.0, 6.0]]));
    ex.process_blocks(2).unwrap();
    let cap = ex.capture(sink).unwrap();
    assert_eq!(
        cap,
        [9.0, 8.0, 7.0, 6.0, 0.0, 0.0],
        "one-shot external track"
    );
}

#[test]
fn clip_addressed_external_track_feeds_only_matching_nodes() {
    // Two buffers: "mic" (clip "mic") and "aux" (unaddressed). A
    // per-clip track registered for "mic" plays only through the mic
    // node; the global external track drives only the unaddressed
    // node — the multi-input routing contract.
    let mut g = Graph2::new();
    let mic = g.add_buffer_clip("mic", "mic", vec![], false);
    let aux = g.add_buffer("aux", vec![], false);
    let sm = g.add_sink("mic-out");
    let sa = g.add_sink("aux-out");
    g.add_edge(mic, PortId::OUT, sm, PortId::IN).unwrap();
    g.add_edge(aux, PortId::OUT, sa, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let mut ex = OfflineExecutor::new(&g, &order, 3, SR).unwrap();
    ex.set_external_clip("mic", Some(vec![vec![1.0, 2.0, 3.0, 4.0]]));
    ex.set_external_input(Some(vec![vec![9.0, 8.0, 7.0, 6.0]]));
    ex.process_blocks(2).unwrap();
    assert_eq!(
        ex.capture(sm).unwrap(),
        [1.0, 2.0, 3.0, 4.0, 0.0, 0.0],
        "clip track feeds the addressed node only"
    );
    assert_eq!(
        ex.capture(sa).unwrap(),
        [9.0, 8.0, 7.0, 6.0, 0.0, 0.0],
        "unaddressed node keeps the global track"
    );

    // A clip-named node with no registered track plays its embedded
    // clip — no silent cross-feeding from another clip's track.
    let mut g2 = Graph2::new();
    let b = g2.add_buffer_clip("synth", "synth", vec![5.0, 5.0], false);
    let s2 = g2.add_sink("out");
    g2.add_edge(b, PortId::OUT, s2, PortId::IN).unwrap();
    let order2 = g2.compile().unwrap().clone();
    let mut ex2 = OfflineExecutor::new(&g2, &order2, 3, SR).unwrap();
    ex2.set_external_clip("mic", Some(vec![vec![1.0, 2.0, 3.0]])); // not "synth"
    ex2.process_blocks(1).unwrap();
    assert_eq!(
        ex2.capture(s2).unwrap(),
        [5.0, 5.0, 0.0],
        "embedded clip when no track for the address"
    );
}

#[test]
fn stereo_buffer_plays_each_channel_on_its_own_port() {
    // A two-channel clip: ports 0 (L) and 1 (R) each carry their own
    // plane in lockstep, looping together — the graph side of a stereo
    // aelog track.
    let mut g = Graph2::new();
    let b = g.add_buffer_channels(
        "stereo",
        vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]],
        true,
    );
    let sl = g.add_sink("l");
    let sr = g.add_sink("r");
    g.add_edge(b, PortId(0), sl, PortId::IN).unwrap();
    g.add_edge(b, PortId(1), sr, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
    ex.process_blocks(2).unwrap();
    assert_eq!(
        ex.capture(sl).unwrap(),
        [1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 1.0, 2.0],
        "left plane loops on port 0"
    );
    assert_eq!(
        ex.capture(sr).unwrap(),
        [4.0, 5.0, 6.0, 4.0, 5.0, 6.0, 4.0, 5.0],
        "right plane loops on port 1"
    );

    // A mono external track on a stereo node: channel 1 reads silence
    // (no upmix — the track is what it is); the looping cursor repeats
    // the single plane in lockstep.
    let mut ex2 = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
    ex2.set_external_input(Some(vec![vec![7.0, 8.0]]));
    ex2.process_blocks(1).unwrap();
    assert_eq!(ex2.capture(sl).unwrap(), [7.0, 8.0, 7.0, 8.0]);
    assert_eq!(ex2.capture(sr).unwrap(), [0.0, 0.0, 0.0, 0.0]);
}

#[test]
fn convolution_renders_kernel_at_its_pipeline_delay() {
    // Impulse → conv(kernel [1,2,3]) → sink, block 4. The node emits
    // `output[k] = (x * h)[k - N]` with N = kernel length, so the
    // impulse response appears at offset 3 — matching node_latency.
    let kernel = vec![1.0, 2.0, 3.0];
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let conv = g.add_convolution("c", kernel.clone());
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
    ex.process_blocks(2).unwrap();
    let cap = ex.capture(sink).unwrap();
    assert_eq!(
        cap,
        [0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 0.0, 0.0],
        "IR at offset kernel.len(), tail carries across blocks"
    );
}

#[test]
fn convolution_pipeline_does_not_drift_over_many_blocks() {
    // Sine → conv(identity kernel, 300 taps) → sink, block 256. The
    // output must stay exactly x[k-300] after 94 blocks — a per-block
    // overlap mishandling would grow the pipeline queue and drift (or
    // silence) the response over time.
    let mut h = vec![0.0; 300];
    h[0] = 1.0; // identity kernel: convolution = delay by 300
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        crate::dsp::graph2::node::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let conv = g.add_convolution("c", h);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 256, SR).unwrap();
    ex.process_blocks(94).unwrap(); // 24 064 samples, past the gate point
    let cap = ex.capture(sink).unwrap();
    let x = |k: usize| (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin();
    assert!((cap[301] - x(1)).abs() < 1e-3, "early: {}", cap[301]);
    assert!(
        (cap[10_000] - x(9_700)).abs() < 1e-3,
        "mid-run: {} vs {}",
        cap[10_000],
        x(9_700)
    );
    assert!(
        (cap[23_990] - x(23_690)).abs() < 1e-3,
        "late (no drift): {} vs {}",
        cap[23_990],
        x(23_690)
    );
}

#[test]
fn convolution_kernel_longer_than_block_is_continuous() {
    // A 10-tap kernel with block 4: the response must span blocks
    // without a seam — out[k] = h[k-10] for k >= 10.
    let h: Vec<f32> = (1..=10).map(|i| i as f32).collect();
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let conv = g.add_convolution("c", h.clone());
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 4, SR).unwrap();
    ex.process_blocks(5).unwrap(); // 20 frames ≥ 10 + 10
    let cap = ex.capture(sink).unwrap();
    for k in 0..cap.len() {
        let want = if k >= 10 { h[k - 10] } else { 0.0 };
        assert_eq!(cap[k], want, "frame {k}");
    }
}

#[test]
fn long_kernel_routes_through_partitioned_fft_engine() {
    // A 600-tap identity kernel: convolution = delay by 600, rendered via
    // the partitioned-FFT engine with half the executor's block size as
    // the engine partition (executor 256, engine 512). The extra `N - B`
    // front delay keeps the node's `kernel.len()` offset and nothing
    // drifts over many blocks.
    let mut h = vec![0.0f32; 600];
    h[0] = 1.0;
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        crate::dsp::graph2::node::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let conv = g.add_convolution("c", h);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 256, SR).unwrap();
    ex.process_blocks(200).unwrap(); // 51 200 samples
    let cap = ex.capture(sink).unwrap();
    let x = |k: usize| (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin();
    assert!((cap[1500] - x(900)).abs() < 1e-2, "early");
    assert!((cap[20_010] - x(19_410)).abs() < 1e-2, "mid-run");
    assert!((cap[50_990] - x(50_390)).abs() < 1e-2, "late, no drift");
    // The long kernel must have selected the engine path, not the exact
    // direct one.
    assert!(
        ex.fft_convolutions.contains_key(&conv),
        "long kernel uses the partitioned-FFT engine"
    );
    assert!(
        !ex.convolutions.contains_key(&conv),
        "long kernel is not on the direct path"
    );
}

#[test]
fn long_kernel_matches_direct_linear_convolution() {
    // A non-identity 600-tap kernel against a single impulse: the engine
    // must reproduce the exact IR contour at offset `kernel.len()` (its
    // reported taps) with only FFT float-level error, and the tail must
    // carry seamlessly across the many engine partitions.
    let n = 600u32;
    let h: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            (t * std::f32::consts::PI * 8.0).sin() * (-3.0 * t).exp()
        })
        .collect();
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let conv = g.add_convolution("c", h.clone());
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 256, SR).unwrap();
    // 6 blocks = 1536 samples ≥ N + h.len − 1 = 1199 plus the warmup.
    ex.process_blocks(6).unwrap();
    let cap = ex.capture(sink).unwrap();
    for k in 0..cap.len() {
        let want = if k >= n as usize && (k - n as usize) < h.len() {
            h[k - n as usize]
        } else {
            0.0
        };
        assert!(
            (cap[k] - want).abs() < 1e-3,
            "frame {k}: {} vs {}",
            cap[k],
            want
        );
    }
}

#[test]
fn very_long_kernel_renders_correctly_across_many_partitions() {
    // An 8000-tap identity IR = 16 engine partitions, with the executor's
    // 256-frame block smaller than the engine's 512 partition — the
    // misalignment that would break a naive per-block driver. The node
    // must keep the exact `kernel.len()` offset with no drift.
    let mut h = vec![0.0f32; 8000];
    h[0] = 1.0;
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        crate::dsp::graph2::node::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let conv = g.add_convolution("c", h);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 256, SR).unwrap();
    ex.process_blocks(230).unwrap(); // 58 880 samples > 8000 + margin
    let cap = ex.capture(sink).unwrap();
    let x = |k: usize| (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin();
    assert!((cap[8_300] - x(300)).abs() < 1e-2, "rises after N");
    assert!((cap[40_010] - x(32_010)).abs() < 1e-2, "mid, no drift");
    assert!((cap[58_750] - x(50_750)).abs() < 1e-2, "late, no drift");
}

#[test]
fn resampler_ratio_one_reproduces_input_delayed_by_its_reported_taps() {
    // A ratio-1 resampler is near-identity: out[k] ≈ x[k - quality], where
    // `quality` is exactly the taps the latency pass reports (so reported
    // == actual delay, like Delay/Convolution). This exercises the
    // windowed-sinc interpolation both *and* the reported-delay pipe.
    let (quality, ratio) = (32u32, 1.0f32);
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        crate::dsp::graph2::node::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let rs = g.add_resampler_with_quality("rs", ratio, quality);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, rs, PortId::IN).unwrap();
    g.add_edge(rs, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 256, SR).unwrap();
    ex.process_blocks(100).unwrap();
    let cap = ex.capture(sink).unwrap();
    let q = quality as usize;
    assert!(
        cap[..q].iter().all(|s| s.abs() < 1e-3),
        "first {q} outputs are the reported taps' leading zeros"
    );
    assert_eq!(
        crate::dsp::graph2::latency::node_latency(g.node(rs).unwrap()),
        quality as u64,
        "reports quality taps"
    );
    assert!(ex.resamplers.contains_key(&rs), "resampler state built");
    let x = |k: usize| (2.0 * std::f32::consts::PI * 160.0 * k as f32 / SR).sin();
    for &k in &[1_000usize, 11_000, 23_500] {
        // out[k] = resampled(x)[k - quality] = x[k - quality] (ratio 1).
        assert!(
            (cap[k] - x(k - q)).abs() < 1e-2,
            "frame {k}: {} vs {}",
            cap[k],
            x(k - q)
        );
    }
}

#[test]
fn resampler_ratio_two_samples_source_at_half_rate() {
    // ratio 2: each output frame reads the source at m/2 (subpos advances
    // 1/2 per output), so out[k] ≈ x(k - quality / 2) — concrete proof the
    // fixed-grid resampler is sampling at the requested ratio.
    let (quality, ratio) = (48u32, 2.0f32);
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        crate::dsp::graph2::node::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 160.0,
        },
    );
    let rs = g.add_resampler_with_quality("rs", ratio, quality);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, rs, PortId::IN).unwrap();
    g.add_edge(rs, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 256, SR).unwrap();
    ex.process_blocks(100).unwrap();
    let cap = ex.capture(sink).unwrap();
    let x = |s: f32| (2.0 * std::f32::consts::PI * 160.0 * s / SR).sin();
    for &k in &[2_000usize, 12_000, 23_000] {
        // out[k] = x[(k - quality)/ratio] (the pipe delays the resampled
        // stream by `quality` source samples).
        let want = x((k as f32 - quality as f32) / ratio);
        assert!(
            (cap[k] - want).abs() < 1e-2,
            "frame {k}: {} vs {}",
            cap[k],
            want
        );
    }
    // And the reported taps are independent of ratio (quality only).
    assert_eq!(
        crate::dsp::graph2::latency::node_latency(g.node(rs).unwrap()),
        quality as u64
    );
}

#[test]
fn hrtf_renders_stereo_pair_aligned_to_longer_ir() {
    // Left IR 5 taps, right IR 2 taps: both ears are delayed by 5 (the
    // node's reported taps), so the pair stays mutually aligned.
    let left = vec![1.0, 2.0, 3.0, 4.0, 5.0];
    let right = vec![10.0, 20.0];
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let hrtf = g.add_hrtf("bin", left.clone(), right.clone());
    let sl = g.add_sink("l");
    let sr = g.add_sink("r");
    g.add_edge(src, PortId::OUT, hrtf, PortId::IN).unwrap();
    g.add_edge(hrtf, PortId(0), sl, PortId::IN).unwrap();
    g.add_edge(hrtf, PortId(1), sr, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 8, SR).unwrap();
    ex.process_blocks(2).unwrap();

    let mut exp_l = vec![0.0; 5];
    exp_l.extend(left.iter().copied());
    exp_l.resize(16, 0.0);
    let mut exp_r = vec![0.0; 5];
    exp_r.extend(right.iter().copied());
    exp_r.resize(16, 0.0);
    assert_eq!(ex.capture(sl).unwrap(), exp_l, "left ear at offset 5");
    assert_eq!(ex.capture(sr).unwrap(), exp_r, "right ear aligned too");
}

#[test]
fn hrtf_dataset_renders_measured_per_ear_irs() {
    // A dataset-sourced binaural node reads the *real* measured head-
    // related responses from the executor's HrtfDataset, not hand-authored
    // tabs. The synthetic dataset at az 90° puts the source on the right:
    // the right (ipsilateral) ear's IR peaks near tap 0 while the left
    // (contralateral) ear is delayed by the ~31-sample Woodworth ITD —
    // proof the rendered pair carries the measured interaural timing.
    let ds = HrtfDataset::synthetic(48_000, 64, 15.0, 15.0);
    let taps = ds.taps() as u32;
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let node = g.add_hrtf_dataset_with_taps("bin", 90.0, 0.0, taps);
    let sl = g.add_sink("l");
    let sr = g.add_sink("r");
    g.add_edge(src, PortId::OUT, node, PortId::IN).unwrap();
    g.add_edge(node, PortId(0), sl, PortId::IN).unwrap();
    g.add_edge(node, PortId(1), sr, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, 256, 48_000.0).unwrap();
    ex.set_hrtf_dataset(Some(ds));
    assert_eq!(
        crate::dsp::graph2::latency::node_latency(g.node(node).unwrap()),
        taps as u64,
        "dataset node reports its taps"
    );
    ex.process_blocks(40).unwrap();
    let cap_l = ex.capture(sl).unwrap(); // left ear, port 0
    let cap_r = ex.capture(sr).unwrap(); // right ear, port 1
    let argmax = |c: &[f32]| -> usize {
        c.iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap()
    };
    let pl = argmax(cap_l);
    let pr = argmax(cap_r);
    // Both ears ride at the reported tap offset (the pipeline delay).
    assert!(
        (pr as i64 - taps as i64).abs() <= 2,
        "ipsilateral right ear near tap {taps}, got {pr}"
    );
    let itd = crate::spatial::hrtf::woodworth_itd_sec(
        std::f32::consts::FRAC_PI_2,
        crate::spatial::hrtf::DEFAULT_HEAD_RADIUS,
        crate::spatial::hrtf::DEFAULT_SPEED_OF_SOUND,
    ) * 48_000.0;
    assert!(
        (pl as f32 - (taps as f32 + itd)).abs() <= 4.0,
        "contralateral left ear delayed by the measured ITD: {pl} vs {}+{}",
        taps,
        itd
    );
    assert!(ex.hrtf_dataset().is_some(), "dataset attached");
}
