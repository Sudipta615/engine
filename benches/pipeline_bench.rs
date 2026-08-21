use config::EngineConfig;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use engine::dsp::pipeline::DspPipeline;
use std::hint::black_box;

fn bench_pipeline(c: &mut Criterion) {
    let config = EngineConfig::default();
    let mut pipeline = DspPipeline::from_config(&config, 44100.0);

    let mut group = c.benchmark_group("pipeline");

    // Single frame throughput
    group.bench_function("full_chain/single_frame", |b| {
        b.iter(|| black_box(pipeline.process(black_box(0.5_f32), black_box(0.3_f32))))
    });

    // Block throughput (realistic audio callback size)
    const BLOCK: usize = 512;
    group.throughput(Throughput::Elements(BLOCK as u64));
    group.bench_function("full_chain/block_512", |b| {
        let mut frames: [(f32, f32); BLOCK] = [(0.5, 0.3); BLOCK];
        b.iter(|| {
            for frame in frames.iter_mut() {
                *frame = pipeline.process(frame.0, frame.1);
            }
            black_box(&frames);
        });
    });

    // Larger block simulating high-latency scenario
    const BLOCK_4K: usize = 4096;
    group.throughput(Throughput::Elements(BLOCK_4K as u64));
    group.bench_function("full_chain/block_4096", |b| {
        let mut frames: Vec<(f32, f32)> = vec![(0.5, 0.3); BLOCK_4K];
        b.iter(|| {
            for frame in frames.iter_mut() {
                *frame = pipeline.process(frame.0, frame.1);
            }
            black_box(&frames);
        });
    });

    // Block API: the new process_block() path (Performance mode).
    group.throughput(Throughput::Elements(BLOCK as u64));
    group.bench_function("full_chain/block_512_block_api", |b| {
        let mut left = vec![0.5f32; BLOCK];
        let mut right = vec![0.3f32; BLOCK];
        b.iter(|| {
            pipeline.process_block(&mut left, &mut right);
            black_box(&left);
        });
    });

    group.throughput(Throughput::Elements(BLOCK_4K as u64));
    group.bench_function("full_chain/block_4096_block_api", |b| {
        let mut left = vec![0.5f32; BLOCK_4K];
        let mut right = vec![0.3f32; BLOCK_4K];
        b.iter(|| {
            pipeline.process_block(&mut left, &mut right);
            black_box(&left);
        });
    });

    // Block API in Quality (f64) mode.
    let mut quality_config = EngineConfig::default();
    quality_config.precision_mode = config::PrecisionMode::Quality;
    let mut quality_pipeline = DspPipeline::from_config(&quality_config, 44100.0);
    group.throughput(Throughput::Elements(BLOCK_4K as u64));
    group.bench_function("full_chain/block_4096_block_api_quality", |b| {
        let mut left = vec![0.5f32; BLOCK_4K];
        let mut right = vec![0.3f32; BLOCK_4K];
        b.iter(|| {
            quality_pipeline.process_block(&mut left, &mut right);
            black_box(&left);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_pipeline);
criterion_main!(benches);
