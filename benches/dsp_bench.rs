use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

fn bench_equalizer(c: &mut Criterion) {
    use engine::dsp::equalizer::ParametricEq;
    let mut eq = ParametricEq::default_10_band(44100.0);
    eq.set_enabled(true);
    c.bench_function("equalizer/10_band_stereo_frame", |b| {
        b.iter(|| black_box(eq.process(black_box(0.5_f32), black_box(0.3_f32))));
    });
    c.bench_function("equalizer/10_band_stereo_block_256", |b| {
        let mut frames: Vec<(f32, f32)> = vec![(0.5, 0.3); 256];
        b.iter(|| {
            for (l, r) in frames.iter_mut() {
                let (out_l, out_r) = black_box(eq.process(black_box(*l), black_box(*r)));
                *l = out_l;
                *r = out_r;
            }
            black_box(&mut frames);
        });
    });
}

fn bench_limiter(c: &mut Criterion) {
    use engine::dsp::limiter::LookaheadLimiter;
    let mut limiter = LookaheadLimiter::new_with_params(44100.0, 5.0, 1.0, 100.0, -1.0, true);
    limiter.set_enabled(true);
    c.bench_function("limiter/lookahead_stereo_frame", |b| {
        b.iter(|| black_box(limiter.process(black_box(0.9_f32), black_box(0.9_f32))))
    });
}

fn bench_loudness(c: &mut Criterion) {
    use engine::dsp::loudness::{LoudnessMode, LoudnessNormalizer};
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::EbuR128);
    c.bench_function("loudness/ebu_r128_stereo_frame", |b| {
        b.iter(|| black_box(norm.process(black_box(0.5_f32), black_box(0.5_f32))))
    });
}

/// Time-stretch / pitch-shift throughput per quality tier (spec §22/§30).
///
/// Each tier is a real WSOLA parameter change (window/hop/search), so the
/// cost must scale visibly with the tier: `Low` is the cheapest, `High` the
/// most expensive. Benchmarks run the block API at 2.0× speed (the
/// processing-heavy direction) on a 512-frame block.
fn bench_timestretch(c: &mut Criterion) {
    use config::TimeStretchQuality;
    use engine::dsp::timestretch::TimeStretcher;

    const BLOCK: usize = 512;
    let mut group = c.benchmark_group("timestretch");
    group.throughput(criterion::Throughput::Elements(BLOCK as u64));

    for tier in [
        TimeStretchQuality::Low,
        TimeStretchQuality::Balanced,
        TimeStretchQuality::High,
    ] {
        let mut stretcher = TimeStretcher::new(44_100.0);
        stretcher.set_quality(tier);
        stretcher.set_speed(2.0);
        let mut left = vec![0.5f32; BLOCK];
        let mut right = vec![0.3f32; BLOCK];
        group.bench_function(format!("speed_2_0/{tier:?}"), |b| {
            b.iter(|| {
                stretcher.process_block(&mut left, &mut right);
                black_box(&left);
            });
        });
    }

    // Pitch-shift (resampler path) at Balanced — the interpolation hot path.
    let mut stretcher = TimeStretcher::new(44_100.0);
    stretcher.set_quality(TimeStretchQuality::Balanced);
    stretcher.set_speed(1.0);
    stretcher.set_pitch_semitones(-12.0);
    let mut left = vec![0.5f32; BLOCK];
    let mut right = vec![0.3f32; BLOCK];
    group.bench_function("pitch_minus_12st/balanced", |b| {
        b.iter(|| {
            stretcher.process_block(&mut left, &mut right);
            black_box(&left);
        });
    });

    group.finish();
}

/// 64-band parametric EQ (spec §9.2): the full band count with a mix of
/// peaking/shelf filters active.
fn bench_eq64(c: &mut Criterion) {
    use engine::dsp::equalizer::ParametricEq;

    let mut eq = ParametricEq::new(64, 44_100.0);
    eq.set_enabled(true);
    for i in 0..64 {
        eq.set_band(
            i,
            engine::dsp::equalizer::EqBandParams {
                enabled: true,
                filter_type: engine::dsp::equalizer::EqFilterType::Peaking,
                frequency: 20.0 * (i as f32 / 63.0 * 10.0).exp2(),
                q: 1.0,
                gain_db: (i % 5) as f32 - 2.0,
            },
        );
    }
    let mut group = c.benchmark_group("equalizer_64band");
    group.throughput(criterion::Throughput::Elements(256));
    group.bench_function("stereo_block_256", |b| {
        let mut frames: Vec<(f32, f32)> = vec![(0.5, 0.3); 256];
        b.iter(|| {
            for (l, r) in frames.iter_mut() {
                let (out_l, out_r) = black_box(eq.process(black_box(*l), black_box(*r)));
                *l = out_l;
                *r = out_r;
            }
            black_box(&mut frames);
        });
    });
    group.finish();
}

/// Partitioned convolution (spec §18): a 8192-sample IR through the uniform
/// partitioned engine.
fn bench_convolution(c: &mut Criterion) {
    use engine::dsp::convolution::ConvolutionEngine;

    // Deterministic decaying-noise stereo IR (8192 samples).
    let mut ir: Vec<(f32, f32)> = Vec::with_capacity(8192);
    let mut state: u64 = 0x243F_6A88_85A3_08D3;
    for i in 0..8192usize {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let v = ((state >> 33) as f32 / (1u64 << 31) as f32 - 1.0) * (1.0 - i as f32 / 8192.0);
        ir.push((v, v * 0.9));
    }
    let mut conv = ConvolutionEngine::new(44_100.0, 8192);
    conv.load_ir_from_samples(&ir).expect("IR load");
    conv.set_enabled(true);

    let mut group = c.benchmark_group("convolution");
    group.throughput(criterion::Throughput::Elements(512));
    group.bench_function("partitioned_ir_8192_block_512", |b| {
        let mut left = vec![0.5f32; 512];
        let mut right = vec![0.3f32; 512];
        b.iter(|| {
            conv.process_block(&mut left, &mut right);
            black_box(&left);
        });
    });
    group.finish();
}

/// Multiband compressor (spec §6/§17): three-band split with all bands
/// active.
fn bench_multiband(c: &mut Criterion) {
    use engine::dsp::multiband_compressor::MultibandCompressor;

    let mut mb = MultibandCompressor::new(44_100.0);
    mb.set_enabled(true);
    let mut group = c.benchmark_group("multiband");
    group.throughput(criterion::Throughput::Elements(512));
    group.bench_function("stereo_block_512", |b| {
        let mut frames: Vec<(f32, f32)> = vec![(0.5, 0.3); 512];
        b.iter(|| {
            for (l, r) in frames.iter_mut() {
                let (out_l, out_r) = black_box(mb.process(black_box(*l), black_box(*r)));
                *l = out_l;
                *r = out_r;
            }
            black_box(&mut frames);
        });
    });
    group.finish();
}

/// Resampler quality tiers (spec §14/§30): 44.1 kHz → 48 kHz at every tier.
/// `Ultra` uses a ~2× longer filter than `Balanced`, so the CPU cost must
/// scale visibly.
fn bench_resampler_tiers(c: &mut Criterion) {
    use config::ResamplerQuality;
    use engine::dsp::resampler::AudioResampler;

    let mut group = c.benchmark_group("resampler");
    group.throughput(criterion::Throughput::Elements(512));
    for quality in [
        ResamplerQuality::Fast,
        ResamplerQuality::Balanced,
        ResamplerQuality::HighQuality,
        ResamplerQuality::Ultra,
    ] {
        let mut rs = AudioResampler::<f32>::new(quality, 44_100.0, 48_000.0).expect("resampler");
        group.bench_function(format!("44k_to_48k/{quality:?}"), |b| {
            b.iter(|| {
                for i in 0..512 {
                    let _ = rs.feed(0.5, 0.3);
                    let _ = rs.read();
                    black_box(i);
                }
            });
        });
    }
    group.finish();
}

/// Multichannel DSP path (spec §17/§30): 8-channel trim + routing planes.
fn bench_multichannel(c: &mut Criterion) {
    use config::ChannelTrimConfig;
    use engine::dsp::channel_trim::ChannelTrimmer;

    let mut config = ChannelTrimConfig::default();
    config.enabled = true;
    let mut trimmer = ChannelTrimmer::new(48_000.0);
    trimmer.set_config(&config, 48_000.0);

    const CHANNELS: usize = 8;
    const FRAMES: usize = 512;
    let mut planes: Vec<Vec<f32>> = (0..CHANNELS).map(|_| vec![0.5; FRAMES]).collect();
    let mut group = c.benchmark_group("multichannel");
    group.throughput(criterion::Throughput::Elements((CHANNELS * FRAMES) as u64));
    group.bench_function("channel_trim_8ch_block_512", |b| {
        b.iter(|| {
            trimmer.process_planes(&mut planes, CHANNELS, FRAMES);
            black_box(&planes);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_equalizer,
    bench_eq64,
    bench_limiter,
    bench_loudness,
    bench_multiband,
    bench_convolution,
    bench_resampler_tiers,
    bench_timestretch,
    bench_multichannel
);
criterion_main!(benches);
