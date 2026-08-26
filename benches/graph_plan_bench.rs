use config::EngineConfig;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use engine::dsp::graph::DspGraph;
use engine::dsp::pipeline::DspPipeline;
use std::hint::black_box;

fn full_config() -> EngineConfig {
    let mut c = EngineConfig::default();
    c.eq.enabled = true;
    if c.eq.bands.len() >= 5 {
        c.eq.bands[0].gain_db = 3.0;
        c.eq.bands[2].gain_db = -2.0;
        c.eq.bands[4].gain_db = 1.5;
    }
    c.limiter.enabled = true;
    c.limiter.lookahead_ms = 2.0;
    c.multiband_compressor.enabled = true;
    c.crossfeed.enabled = true;
    c.stereo_enhancer.enabled = true;
    c.stereo_enhancer.width = 1.3;
    c
}

fn bench_graph_plan(c: &mut Criterion) {
    let config = full_config();
    let mut graph = DspGraph::from_config(&config, 44100.0);

    let mut group = c.benchmark_group("graph_plan");

    // Block throughput (realistic audio callback size).
    const BLOCK: usize = 512;
    group.throughput(Throughput::Elements(BLOCK as u64));
    group.bench_function("full_chain/block_512_block_api", |b| {
        let mut left = vec![0.5f32; BLOCK];
        let mut right = vec![0.3f32; BLOCK];
        b.iter(|| {
            graph.process_block(&mut left, &mut right);
            black_box(&left);
        });
    });

    const BLOCK_4K: usize = 4096;
    group.throughput(Throughput::Elements(BLOCK_4K as u64));
    group.bench_function("full_chain/block_4096_block_api", |b| {
        let mut left = vec![0.5f32; BLOCK_4K];
        let mut right = vec![0.3f32; BLOCK_4K];
        b.iter(|| {
            graph.process_block(&mut left, &mut right);
            black_box(&left);
        });
    });

    // Quality (f64) mode: the enum-dispatch f64 plan path.
    let quality_config = EngineConfig {
        precision_mode: config::PrecisionMode::Quality,
        ..config.clone()
    };
    let mut quality_graph = DspGraph::from_config(&quality_config, 44100.0);
    group.throughput(Throughput::Elements(BLOCK_4K as u64));
    group.bench_function("full_chain/block_4096_block_api_quality", |b| {
        let mut left = vec![0.5f32; BLOCK_4K];
        let mut right = vec![0.3f32; BLOCK_4K];
        b.iter(|| {
            quality_graph.process_block(&mut left, &mut right);
            black_box(&left);
        });
    });

    // Multichannel (6ch, `NormalMc` plan: routing + chain + volume/fade).
    let layout = engine::decode::ChannelLayout::from_count(6);
    let mut mc_graph = DspGraph::from_config(&config, 44100.0);
    mc_graph.set_multichannel_layout(&layout);
    group.throughput(Throughput::Elements((BLOCK_4K * 6) as u64));
    group.bench_function("full_chain/block_4096_multichannel_6ch", |b| {
        let mut interleaved = vec![0.5f32; BLOCK_4K * 6];
        b.iter(|| {
            mc_graph.process_block_multichannel(&mut interleaved, 6);
            black_box(&interleaved);
        });
    });

    group.finish();
}

/// Head-to-head: the enum-dispatch plan executor vs the direct-call pipeline.
/// The Phase-1 contract is that the graph never regresses the hot path
/// dramatically; this group is the reporting side of that gate (the hard
/// CI assertion lives in the equivalence suite's fixed-iteration test).
fn bench_graph_vs_pipeline(c: &mut Criterion) {
    let config = full_config();
    let mut pipeline = DspPipeline::from_config(&config, 44100.0);
    let mut graph = DspGraph::from_config(&config, 44100.0);

    const BLOCK_4K: usize = 4096;
    let mut group = c.benchmark_group("graph_vs_pipeline");
    group.throughput(Throughput::Elements(BLOCK_4K as u64));

    let mut p_left = vec![0.5f32; BLOCK_4K];
    let mut p_right = vec![0.3f32; BLOCK_4K];
    group.bench_with_input(
        BenchmarkId::new("block_4096", "pipeline"),
        &BLOCK_4K,
        |b, _| {
            b.iter(|| {
                pipeline.process_block(&mut p_left, &mut p_right);
                black_box(&p_left);
            });
        },
    );

    let mut g_left = vec![0.5f32; BLOCK_4K];
    let mut g_right = vec![0.3f32; BLOCK_4K];
    group.bench_with_input(
        BenchmarkId::new("block_4096", "graph"),
        &BLOCK_4K,
        |b, _| {
            b.iter(|| {
                graph.process_block(&mut g_left, &mut g_right);
                black_box(&g_left);
            });
        },
    );

    group.finish();
}

criterion_group!(benches, bench_graph_plan, bench_graph_vs_pipeline);
criterion_main!(benches);
