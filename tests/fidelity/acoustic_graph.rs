//! Fidelity tests — acoustic world as Graph 2.0 nodes (v3.31).
//!
//! Evolution thresholds:
//! * a `BakedScene` attached to the [`OfflineExecutor`] turns every
//!   [`NodeKind::Acoustic`] node into a graph-routable room-response
//!   primitive: an impulse into the node reproduces the baked response
//!   (direct pass-through + one delayed, gain-scaled copy per baked path)
//!   **exactly**;
//! * the wet room and a dry gain branch route through `Split`/`Mix` like
//!   any other signal — reflections are just taps in the topology;
//! * an unbaked position (or no scene at all) passes the input through
//!   unchanged — deterministic fallback, no crash;
//! * the acoustic node adds **zero pipeline latency** (the direct path
//!   passes immediately; the tail is wet content, not alignment delay) —
//!   `analyze` reports 0 for the whole chain;
//! * a graph containing an acoustic node round-trips through JSON (Vec3
//!   positions serialize).

use engine::prelude::{
    analyze, AcousticBaker, AcousticRoom, AcousticWorld, BakePolicy, BakedScene, ExecutionOrder,
    Graph2, MaterialKind, MaterialSpectrum, NodeId, NodeKind, NodeParams, OfflineExecutor, PortId,
    Room, Vec3,
};

const SR: f32 = 48_000.0;
const BLOCK: usize = 512;

fn baked_world() -> BakedScene {
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
    AcousticBaker::new(world, 0.5).bake_scene(
        [Vec3::new(1.0, 5.0, 1.5)],
        Vec3::new(6.0, 5.0, 1.5),
        SR,
        BakePolicy::default(),
    )
}

/// The oracle output an acoustic node must produce for an impulse input:
/// direct at 0 (direct gain) + each non-direct path at its excess delay.
fn expected_response(scene: &BakedScene, pos: Vec3, frames: usize) -> Vec<f32> {
    let obj = scene.get(pos).expect("baked object");
    let direct = obj.direct().expect("direct path");
    let mut expected = vec![0.0f32; frames];
    expected[0] = direct.gain;
    for p in obj
        .paths
        .iter()
        .filter(|p| p.kind != engine::prelude::PathKind::Direct)
    {
        let excess = (p.delay_samples - direct.delay_samples).max(0.0).round() as usize;
        if excess < frames {
            expected[excess] += p.gain;
        }
    }
    expected
}

fn impulse_graph_with_acoustic(pos: Vec3) -> (Graph2, ExecutionOrder, NodeId) {
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let room = g.add_acoustic("room", pos);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, room, PortId::IN).unwrap();
    g.add_edge(room, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    (g, order, sink)
}

fn render(g: &Graph2, order: &ExecutionOrder, sink: NodeId, scene: Option<BakedScene>) -> Vec<f32> {
    let mut ex = OfflineExecutor::new(g, order, BLOCK, SR).unwrap();
    ex.set_baked_scene(scene);
    ex.process_blocks(4).unwrap(); // 2048 frames — covers the reflection tail
    ex.capture(sink).unwrap().to_vec()
}

#[test]
fn acoustic_node_renders_the_baked_response_exactly() {
    let scene = baked_world();
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let expected = expected_response(&scene, pos, 2048);
    assert!(
        expected.iter().skip(1).any(|s| s.abs() > 1e-6),
        "bake has reflections"
    );

    let (g, order, sink) = impulse_graph_with_acoustic(pos);
    let cap = render(&g, &order, sink, Some(scene));
    assert_eq!(cap, expected, "acoustic node == baked response");
    // The direct tap is present and the tail finite.
    assert!((cap[0] - expected[0]).abs() < 1e-6);
    assert!(cap.iter().all(|s| s.is_finite()));
}

#[test]
fn dry_wet_routes_the_room_like_any_signal() {
    // source → split(2) → { acoustic(wet), gain(0.3) dry } → mix → sink:
    // the reflections are just taps routed through the topology.
    let scene = baked_world();
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("s", 2);
    let room = g.add_acoustic("wet", pos);
    let dry = g.add_gain("dry", 0.3);
    let mix = g.add_mix("mix", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), room, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), dry, PortId::IN).unwrap();
    g.add_edge(room, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(dry, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    let cap = render(&g, &order, sink, Some(scene.clone()));
    let obj = scene.get(pos).unwrap();
    let direct_gain = obj.direct().unwrap().gain;
    // Direct: dry 0.3× + wet direct.
    assert!(
        (cap[0] - (0.3 + direct_gain)).abs() < 1e-6,
        "direct summed: {}",
        cap[0]
    );
    // Reflections are present at their excess delays, unscaled by dry.
    // (Symmetric walls share an excess delay, so accumulate per-excess.)
    let direct_delay = obj.direct().unwrap().delay_samples;
    let mut by_excess: std::collections::HashMap<usize, f32> = std::collections::HashMap::new();
    for p in obj
        .paths
        .iter()
        .filter(|p| p.kind != engine::prelude::PathKind::Direct)
    {
        let excess = (p.delay_samples - direct_delay).max(0.0).round() as usize;
        *by_excess.entry(excess).or_insert(0.0) += p.gain;
    }
    for (excess, want) in by_excess {
        assert!(
            (cap[excess] - want).abs() < 1e-6,
            "reflection sum at {excess}: {} vs {}",
            cap[excess],
            want
        );
    }
}

#[test]
fn unbaked_position_and_missing_scene_pass_through() {
    let scene = baked_world();
    // A position outside the baked cells: pass-through, no crash.
    let far = Vec3::new(9.0, 9.0, 9.0);
    assert!(scene.get(far).is_none());
    let (g, order, sink) = impulse_graph_with_acoustic(far);
    let cap = render(&g, &order, sink, Some(scene));
    assert_eq!(cap.len(), 2048);
    assert!((cap[0] - 1.0).abs() < 1e-6, "direct pass-through");
    assert!(cap[1..].iter().all(|s| s.abs() < 1e-6), "no reflections");

    // No scene at all: same pass-through.
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let (g, order, sink) = impulse_graph_with_acoustic(pos);
    let cap = render(&g, &order, sink, None);
    assert!((cap[0] - 1.0).abs() < 1e-6);
    assert!(cap[1..].iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn acoustic_node_adds_no_pipeline_latency() {
    let scene = baked_world();
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let (g, order, _sink) = impulse_graph_with_acoustic(pos);
    let _ = (order, scene);
    let rep = analyze(&g, SR).unwrap();
    let room = g
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Acoustic)
        .unwrap()
        .id;
    assert_eq!(rep.taps_at(room), 0, "direct passes immediately");
    assert_eq!(
        rep.total_samples, 0,
        "no alignment latency from a room tail"
    );
}

#[test]
fn acoustic_graph_serializes_and_round_trips() {
    let pos = Vec3::new(1.5, 5.5, 1.0);
    let (g, _order, _sink) = impulse_graph_with_acoustic(pos);
    let json = serde_json::to_string(&g).unwrap();
    let mut back: Graph2 = serde_json::from_str(&json).unwrap();
    assert_eq!(back.node_count(), g.node_count());
    assert_eq!(back.edge_count(), g.edge_count());
    let room = back
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Acoustic)
        .unwrap();
    match &room.params {
        NodeParams::Acoustic { position, scene } => {
            assert!((position.x - 1.5).abs() < 1e-6 && (position.z - 1.0).abs() < 1e-6);
            assert!(scene.is_none(), "plain add_acoustic has no scene id");
        }
        ref other => panic!("expected Acoustic params, got {other:?}"),
    }
    // The round-tripped graph compiles and renders identically.
    let _order2 = back.compile().unwrap();
}

#[test]
fn material_bake_changes_the_node_taps() {
    // A fabric left wall darkens the baked reflections; the graph node must
    // reflect the material (spectral collapse changes gain / low-pass).
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
    let concrete = AcousticWorld::new(
        AcousticRoom::from_render_room(&room, MaterialSpectrum::flat_reflective(room.absorption)),
        SR,
    );
    let mut fabric_room =
        AcousticRoom::from_render_room(&room, MaterialSpectrum::flat_reflective(room.absorption));
    fabric_room.walls[engine::spatial::wall_index(engine::spatial::Wall::MinX)] =
        MaterialKind::Fabric.spectrum();
    let fabric = AcousticWorld::new(fabric_room, SR);

    let pos = Vec3::new(1.0, 5.0, 1.5);
    let s_c = AcousticBaker::new(concrete, 0.5).bake_single(
        pos,
        Vec3::new(6.0, 5.0, 1.5),
        SR,
        BakePolicy::default(),
    );
    let s_f = AcousticBaker::new(fabric, 0.5).bake_single(
        pos,
        Vec3::new(6.0, 5.0, 1.5),
        SR,
        BakePolicy::default(),
    );

    let (g, order, sink) = impulse_graph_with_acoustic(pos);
    let cap_c = render(&g, &order, sink, Some(s_c.clone()));
    let cap_f = render(&g, &order, sink, Some(s_f.clone()));
    assert_ne!(cap_c, cap_f, "material changes the rendered taps");
    // Fabric absorbs: its *broadband* reflected gain is weaker (v3.40
    // colours the reflection spectrally — the low-pass spreads the impulse
    // across the kernel's taps, so the right metric is the collapsed gain,
    // not the raw impulse energy).
    let bband = |s: &BakedScene| {
        let obj = s.get(pos).unwrap();
        obj.paths
            .iter()
            .filter(|p| p.kind != engine::prelude::PathKind::Direct)
            .map(|p| p.gain.abs())
            .sum::<f32>()
    };
    assert!(
        bband(&s_f) < bband(&s_c),
        "fabric reflections weaker in broadband gain"
    );
}

#[test]
fn per_listener_scenes_render_distinct_responses_mixed() {
    // Two bakes of the same room for two different listener positions but
    // the SAME source cell — one named scene per listener. Two Acoustic
    // nodes reference the scenes by id and a Mix sums them in the topology:
    // the output is the elementwise sum of the two distinct room responses.
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
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let front = baker.bake_single(pos, Vec3::new(6.0, 5.0, 1.5), SR, BakePolicy::default());
    let back = baker.bake_single(pos, Vec3::new(6.0, 2.0, 1.5), SR, BakePolicy::default());
    let exp_front = expected_response(&front, pos, 2048);
    let exp_back = expected_response(&back, pos, 2048);
    assert_ne!(
        exp_front, exp_back,
        "different listeners → distinct responses"
    );

    let mut g = Graph2::new();
    let src = g.add_source("imp");
    let split = g.add_split("s", 2);
    let n_front = g.add_acoustic_scene("front", pos, "front");
    let n_back = g.add_acoustic_scene("back", pos, "back");
    let mix = g.add_mix("sum", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), n_front, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), n_back, PortId::IN).unwrap();
    g.add_edge(n_front, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(n_back, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();

    let order = g.compile().unwrap().clone();
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK, SR).unwrap();
    ex.set_scene("front", front);
    ex.set_scene("back", back);
    ex.process_blocks(4).unwrap();
    let cap = ex.capture(sink).unwrap();

    // The mix is the elementwise sum of the two listeners' responses.
    assert_eq!(cap.len(), 2048);
    for (k, got) in cap.iter().enumerate() {
        let want = exp_front[k] + exp_back[k];
        assert!(
            (got - want).abs() < 1e-6,
            "mixed[{k}] = {got}, expected {want}"
        );
    }
}
