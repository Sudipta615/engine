//! Spatial renderer benchmarks (Phase 22 — Optimization).
//!
//! Pins the binaural hot paths (FIR-with-dataset, analytic, room) and the
//! VBAP array renderer at realistic block sizes, plus the production graph
//! with the SpatialNode enabled. The binaural numbers are the regression
//! guard for the Phase-22 arithmetic reduction (per-block trig hoisting,
//! modulo-free FIR ring reads); the graph number exercises the same paths
//! end-to-end through the node.
//!
//! Run: `cargo bench --bench spatial_bench`

use config::EngineConfig;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use engine::dsp::graph::DspGraph;
use engine::spatial::math::Vec3;
use engine::spatial::render::{HybridBlockInputs, SpatialRenderer};
use engine::spatial::{
    BinauralRenderer, DistanceModel, HrtfDataset, SpatialScene, SpeakerLayout, VbapRenderer,
};
use std::hint::black_box;
use std::sync::Arc;

const SR: u32 = 48_000;
const BLOCK: usize = 1024;

/// A scene with `n` objects spread across the horizontal plane plus one
/// raised object, and (optionally) the room enabled.
fn scene_with(n: usize, room: bool) -> SpatialScene {
    let mut scene = SpatialScene::new(SR);
    for i in 0..n {
        let az = (i as f32 / n.max(1) as f32) * std::f32::consts::TAU;
        let pos = Vec3::new(
            az.sin() * 2.0,
            az.cos() * 2.0,
            if i % 3 == 0 { 1.5 } else { 0.0 },
        );
        let id = scene.create_audio_object(pos).unwrap();
        scene.object_mut(id).unwrap().distance_model = DistanceModel::Linear;
        scene.object_mut(id).unwrap().spread = if i % 4 == 0 { 0.35 } else { 0.0 };
    }
    if room {
        scene.room.enabled = true;
        scene.room.reflection_order = 1;
        scene.room.width = 10.0;
        scene.room.depth = 8.0;
        scene.room.height = 3.0;
    }
    scene
}

/// Leaked input planes: one 1024-frame block per object. Leaking is fine
/// for a benchmark process; the renderer borrows the slices read-only.
fn block_inputs(n: usize) -> HybridBlockInputs<'static> {
    let mut refs = Vec::new();
    for i in 0..n {
        let mut p = vec![0.0f32; BLOCK];
        for (k, v) in p.iter_mut().enumerate() {
            // Deterministic non-trivial content (a gentle ramp + low sine)
            // so the filter/FIR paths do real work.
            *v = (k as f32 * 0.001) + (i as f32 * 0.1).sin() * 0.25;
        }
        refs.push(Box::leak(p.into_boxed_slice()) as &'static [f32]);
    }
    HybridBlockInputs {
        objects: refs.leak(),
        beds: &[],
        fields: &[],
    }
}

fn bench_binaural(c: &mut Criterion) {
    let mut group = c.benchmark_group("spatial/binaural");
    group.throughput(Throughput::Elements(BLOCK as u64));
    const N: usize = 4;

    // FIR path: measured-style dataset (64 taps, 15° grid) loaded.
    let ds = Arc::new(HrtfDataset::synthetic(SR, 64, 15.0, 15.0));
    group.bench_function("fir_64taps_4obj", |b| {
        let mut r = BinauralRenderer::new(0.0);
        r.use_dataset(Some(Arc::clone(&ds)));
        r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
        let scene = scene_with(N, false);
        let inputs = block_inputs(N);
        let mut out = vec![0.0f32; BLOCK * 2];
        b.iter(|| {
            r.process_hybrid_block(&scene, &inputs, BLOCK, &mut out)
                .unwrap();
            black_box(&out[0]);
        });
    });

    // Analytic path: no dataset (Woodworth ITD + shelf + pinna notch).
    group.bench_function("analytic_4obj", |b| {
        let mut r = BinauralRenderer::new(0.0);
        r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
        let scene = scene_with(N, false);
        let inputs = block_inputs(N);
        let mut out = vec![0.0f32; BLOCK * 2];
        b.iter(|| {
            r.process_hybrid_block(&scene, &inputs, BLOCK, &mut out)
                .unwrap();
            black_box(&out[0]);
        });
    });

    // Room on (image-source early reflections) over the analytic path.
    group.bench_function("analytic_4obj_room", |b| {
        let mut r = BinauralRenderer::new(0.0);
        r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
        let scene = scene_with(N, true);
        let inputs = block_inputs(N);
        let mut out = vec![0.0f32; BLOCK * 2];
        b.iter(|| {
            r.process_hybrid_block(&scene, &inputs, BLOCK, &mut out)
                .unwrap();
            black_box(&out[0]);
        });
    });

    group.finish();
}

fn bench_vbap(c: &mut Criterion) {
    let mut group = c.benchmark_group("spatial/vbap");
    group.throughput(Throughput::Elements(BLOCK as u64));
    const N: usize = 4;

    group.bench_function("5_1_4obj", |b| {
        let mut r = VbapRenderer::new();
        r.prepare(&SpeakerLayout::five_point_one(), SR).unwrap();
        let scene = scene_with(N, false);
        let inputs = block_inputs(N);
        let mut out = vec![0.0f32; BLOCK * 6];
        b.iter(|| {
            r.process_hybrid_block(&scene, &inputs, BLOCK, &mut out)
                .unwrap();
            black_box(&out[0]);
        });
    });

    group.finish();
}

/// Production path: the graph with the SpatialNode enabled renders the
/// mixed stereo master through the binaural head model.
fn bench_graph_spatial(c: &mut Criterion) {
    let mut group = c.benchmark_group("spatial/graph");
    group.throughput(Throughput::Elements(BLOCK as u64));

    let config = EngineConfig::default();
    let mut graph = DspGraph::from_config(&config, SR as f32);
    graph.control_handle().set_spatial_enabled(true);
    graph.drain_queued_control();

    group.bench_function("spatial_node_512_block", |b| {
        const B: usize = 512;
        let mut left = vec![0.5f32; B];
        let mut right = vec![0.3f32; B];
        b.iter(|| {
            graph.process_block(&mut left, &mut right);
            black_box(&left);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_binaural, bench_vbap, bench_graph_spatial);
criterion_main!(benches);
