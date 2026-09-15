//! Formal Performance Budget Benchmarking Suite (spec §8.1).
//!
//! Measures cycles/frame, cycles/sample, throughput, and execution scalability
//! across block sizes: [16, 32, 64, 128, 256, 512, 1024]
//! and sample rates: [44.1 kHz, 48 kHz, 96 kHz, 192 kHz, 384 kHz]
//! for layouts: Stereo (2ch), 5.1 (6ch), 7.1 (8ch), 7.1.4 (12ch), 9.1.6 (16ch).

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

use engine::decode::ChannelLayout;
use engine::dsp::equalizer::ParametricEq;
use engine::dsp::limiter::LookaheadLimiter;
use engine::dsp::loudness::LoudnessMeter;

fn bench_block_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("performance_budget/block_sizes");
    let sr = 48000.0f32;

    for &block_size in &[16, 32, 64, 128, 256, 512, 1024] {
        group.throughput(Throughput::Elements(block_size as u64));

        // 10-band stereo EQ across various buffer quantum sizes
        let mut eq = ParametricEq::default_10_band(sr);
        eq.set_enabled(true);
        let mut frames = vec![(0.5f32, 0.3f32); block_size];

        group.bench_with_input(
            BenchmarkId::new("10_band_eq_stereo", block_size),
            &block_size,
            |b, _| {
                b.iter(|| {
                    for (l, r) in frames.iter_mut() {
                        let (out_l, out_r) = black_box(eq.process(black_box(*l), black_box(*r)));
                        *l = out_l;
                        *r = out_r;
                    }
                    black_box(&mut frames);
                });
            },
        );

        // ITU-R BS.1770-5 loudness meter block ingestion
        let mut meter = LoudnessMeter::new(sr, 2);
        let interleaved = vec![0.25f32; block_size * 2];

        group.bench_with_input(
            BenchmarkId::new("loudness_meter_stereo", block_size),
            &block_size,
            |b, _| {
                b.iter(|| {
                    meter.process_interleaved(black_box(&interleaved), 2);
                    black_box(&meter);
                });
            },
        );
    }
    group.finish();
}

fn bench_multichannel_scalability(c: &mut Criterion) {
    let mut group = c.benchmark_group("performance_budget/channel_layouts");
    let sr = 48000.0f32;
    const BLOCK: usize = 256;
    group.throughput(Throughput::Elements(BLOCK as u64));

    let layouts = [
        ("stereo_2ch", ChannelLayout::Stereo, 2),
        ("surround_5_1_6ch", ChannelLayout::FivePointOne, 6),
        ("surround_7_1_8ch", ChannelLayout::SevenPointOne, 8),
        ("immersive_7_1_4_12ch", ChannelLayout::SevenPointOneFour, 12),
        ("immersive_9_1_6_16ch", ChannelLayout::NinePointOneSix, 16),
    ];

    for (name, layout, channels) in layouts {
        let mut meter = LoudnessMeter::new(sr, channels);
        meter.set_channel_layout(&layout);
        let interleaved = vec![0.1f32; BLOCK * channels];

        group.bench_with_input(
            BenchmarkId::new("loudness_bs1770_5", name),
            &name,
            |b, _| {
                b.iter(|| {
                    meter.process_interleaved(black_box(&interleaved), channels);
                    black_box(&meter);
                });
            },
        );
    }
    group.finish();
}

fn bench_sample_rates(c: &mut Criterion) {
    let mut group = c.benchmark_group("performance_budget/sample_rates");
    const BLOCK: usize = 256;
    group.throughput(Throughput::Elements(BLOCK as u64));

    for &sr in &[44100.0f32, 48000.0, 96000.0, 192000.0, 384000.0] {
        let sr_khz = (sr / 1000.0).round() as u32;

        let mut limiter = LookaheadLimiter::new_with_params(sr, 5.0, 1.0, 50.0, -1.0, true);
        limiter.set_enabled(true);
        let mut frames = vec![(0.8f32, 0.8f32); BLOCK];

        group.bench_with_input(
            BenchmarkId::new("limiter_stereo", format!("{sr_khz}khz")),
            &sr,
            |b, _| {
                b.iter(|| {
                    for (l, r) in frames.iter_mut() {
                        let (out_l, out_r) =
                            black_box(limiter.process(black_box(*l), black_box(*r)));
                        *l = out_l;
                        *r = out_r;
                    }
                    black_box(&mut frames);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_block_sizes,
    bench_multichannel_scalability,
    bench_sample_rates
);
criterion_main!(benches);
