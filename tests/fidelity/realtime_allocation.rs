//! Real-time safety stress validation: the full worst-case DSP graph must
//! perform ZERO heap allocations during steady-state processing.
//!
//! Unlike a unit smoke test, this enables every processor in the graph at
//! once — EQ with boosted bands, multiband compressor with real gain
//! reduction, convolution with a loaded IR, crossfeed, stereo width,
//! loudness normalization, time-stretch at 2×, and the final safety limiter —
//! in BOTH precision modes, and asserts the audio thread never allocates.
//! It also covers a genuine (non-passthrough) resampler conversion.
//!
//! Any `Vec::push`/`resize`/`collect` that slips into a hot path fails here.
//!
//! # Why the counter is thread-local
//!
//! The libtest harness spawns its own helper machinery. In particular, once
//! ANY test in this binary exceeds libtest's 60-second default timeout, the
//! harness thread busy-loops in `get_timed_out_tests()` — `recv_timeout(0)`
//! followed by a `Vec<TestDesc>::push` per iteration — until the long test
//! finishes. A process-global counter would count that flood (and other
//! tests' construction allocations) inside a concurrently-measuring test's
//! window, producing intermittent spurious failures. Counting per-thread
//! restricts the assertion to allocations made by the thread that actually
//! runs the DSP loop, which is exactly the property the test must guarantee.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use engine::dsp::graph::DspGraph;
use engine::dsp::loudness::LoudnessMetadata;
use engine::dsp::pipeline::DspPipeline;
use engine::spatial::{
    BasicPanner, SpatialRenderer, SpatialScene, SpeakerLayout, VbapRenderer, Vec3,
};

/// Write a short 16-bit stereo WAV whose left channel is a single impulse
/// (sample 0 = 1.0, rest silence); right channel silent. Used as the aux
/// insert's IR file in [`run_graph_plan_no_alloc`].
fn write_impulse_wav(path: &std::path::Path, sample_rate: u32, n_frames: usize) {
    let mut data = Vec::with_capacity(n_frames * 4);
    for i in 0..n_frames {
        let v = if i == 0 { 32767i16 } else { 0i16 };
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(path, &wav).unwrap();
}

thread_local! {
    /// Heap allocations performed on THIS thread while the measurement
    /// window is armed.
    static THREAD_ALLOCS: Cell<usize> = const { Cell::new(0) };
}

/// Set while the audio loop is being measured; the allocator only records
/// allocations during steady-state processing, not pipeline construction or
/// warm-up.
static ARMED: AtomicBool = AtomicBool::new(false);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// A config that turns on every DSP stage with real (non-trivial) parameters.
// Deliberately sets many config fields one at a time for readability.
#[allow(clippy::field_reassign_with_default)]
fn full_chain_config() -> config::EngineConfig {
    let mut c = config::EngineConfig::default();
    c.precision_mode = config::PrecisionMode::Performance;

    // EQ with several boosted/cut bands.
    c.eq.enabled = true;
    if c.eq.bands.len() >= 5 {
        c.eq.bands[0].gain_db = 6.0;
        c.eq.bands[2].gain_db = -3.0;
        c.eq.bands[4].gain_db = 4.0;
    }

    // Multiband compressor with real gain reduction.
    c.multiband_compressor.enabled = true;
    for band in [
        &mut c.multiband_compressor.low_band,
        &mut c.multiband_compressor.mid_band,
        &mut c.multiband_compressor.high_band,
    ] {
        band.threshold_db = -18.0;
        band.ratio = 4.0;
        band.makeup_gain_db = 0.0;
    }

    // Crossfeed + stereo width.
    c.crossfeed.enabled = true;
    c.stereo_enhancer.enabled = true;
    c.stereo_enhancer.width = 1.3;

    // Loudness normalization with a realistic target and guard.
    c.loudness.mode = config::LoudnessMode::EbuR128;
    c.loudness.target_lufs = -14.0;
    c.loudness.true_peak_guard = true;

    // Final safety limiter active.
    c.limiter.enabled = true;
    c.limiter.lookahead_ms = 5.0;

    c.dither_enabled = true;
    c
}

fn run_full_chain_no_alloc(mode: config::PrecisionMode) {
    let mut cfg = full_chain_config();
    cfg.precision_mode = mode;

    let mut pipeline = DspPipeline::from_config(&cfg, 48_000.0);

    // Convolution with a real IR (short synthetic room impulse response).
    let ir: Vec<(f32, f32)> = (0..2048)
        .map(|i| {
            let e = (-i as f32 / 512.0).exp() * 0.5;
            (e, e * 0.9)
        })
        .collect();
    pipeline.convolution.set_enabled(true);
    pipeline
        .convolution
        .load_ir_from_samples(&ir)
        .expect("synthetic IR must load");
    pipeline.convolution.set_wet_mix(0.3);

    // Loudness metadata so the normalizer has a target to gain toward.
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-20.0),
        ..Default::default()
    };
    pipeline.apply_loudness_metadata_outgoing(Some(meta));

    // Time-stretch processor active in a self-balancing configuration.
    // Pitch-shift (pitch +1 octave, tempo constant) is used rather than
    // playback speed: a speed change intentionally produces more output
    // frames than input frames, which in the real engine is balanced by the
    // decoder delivering proportionally fewer source frames — a block-level
    // test cannot reproduce that steady state. Pitch-shift keeps the
    // WSOLA/resampler FIFOs balanced at the block rate, so it exercises the
    // documented f32-core processor allocation-free.
    pipeline.timestretcher_mut().set_pitch_semitones(12.0);

    // Volume + balance for full-path coverage.
    pipeline.set_volume(0.8);
    pipeline.set_balance(-0.2);

    let mut left = [0.0f32; 128];
    let mut right = [0.0f32; 128];

    // Warm up all stateful stages (envelope followers, crossover filters,
    // convolution partitions, WSOLA rings, limiter lookahead) before the
    // measurement window.
    pipeline.process_block(&mut left, &mut right);
    pipeline.process_final_limiter_block(&mut left, &mut right);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    // 10k blocks × 128 samples = 1.28M samples ≈ 27 s of audio at 48 kHz per
    // mode — long enough to catch lazy one-off allocations (FIFO growth,
    // partition re-layout, lookahead deque edges) without ballooning CI time.
    for block in 0..10_000 {
        let value = (block as f32 * 0.01).sin() * 0.3;
        left.fill(value);
        right.fill(-value * 0.8);
        pipeline.process_block(&mut left, &mut right);
        pipeline.process_final_limiter_block(&mut left, &mut right);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state full-chain processing ({mode:?}) allocated on the audio path"
    );
}

#[test]
fn realtime_full_chain_performance_mode_does_not_allocate() {
    run_full_chain_no_alloc(config::PrecisionMode::Performance);
}

#[test]
fn realtime_full_chain_quality_mode_does_not_allocate() {
    run_full_chain_no_alloc(config::PrecisionMode::Quality);
}

/// The Phase-1 plan executor (`DspGraph`) must uphold the same zero-allocation
/// contract as the pipeline it mirrors: the enum-dispatch `run_plan` hot path,
/// the f64 quality-mode promotion, and the multichannel `NormalMc` plan
/// (de-interleave → routing → chain → re-interleave) all run entirely on
/// preallocated scratch.
fn run_graph_plan_no_alloc(mode: config::PrecisionMode) {
    let mut cfg = full_chain_config();
    cfg.precision_mode = mode;

    // Phase 5/6: the aux bus with per-slot sends and the global convolution
    // insert must also be allocation-free on the audio path (aux taps in the
    // sum, the SIMD `accumulate_scaled` return, and the insert's in-place
    // convolution all run on preallocated planes). The IR is loaded from a
    // file exactly like a host would configure it (control path — before the
    // measurement window).
    let ir_path = std::env::temp_dir().join(format!(
        "rt_aux_ir_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_impulse_wav(&ir_path, 48_000, 2048);
    cfg.aux.enabled = true;
    cfg.aux.return_gain = 0.5;
    cfg.aux.insert_enabled = true;
    cfg.aux.insert_wet_mix = 0.3;
    cfg.aux.insert_ir_path = Some(ir_path.display().to_string());

    let mut graph = DspGraph::from_config(&cfg, 48_000.0);

    // Same synthetic IR as the pipeline path.
    let ir: Vec<(f32, f32)> = (0..2048)
        .map(|i| {
            let e = (-i as f32 / 512.0).exp() * 0.5;
            (e, e * 0.9)
        })
        .collect();
    graph.convolution_mut().engine.set_enabled(true);
    graph
        .convolution_mut()
        .engine
        .load_ir_from_samples(&ir)
        .expect("synthetic IR must load");
    graph.convolution_mut().engine.set_wet_mix(0.3);

    // Per-slot sends into the aux bus (post-fader taps in the sum loops).
    graph.set_slot_send(0, 1.0, 0.5);
    graph.set_slot_send(1, 1.0, 0.5);
    graph.drain_queued_control();

    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-20.0),
        ..Default::default()
    };
    graph.apply_loudness_metadata_outgoing(Some(meta));

    // Self-balancing pitch shift (same rationale as the pipeline path).
    graph.timestretch_mut().stretcher.set_pitch_semitones(12.0);

    graph.set_volume(0.8);
    graph.set_balance(-0.2);

    let mut left = [0.0f32; 128];
    let mut right = [0.0f32; 128];

    // Warm up all stateful stages before the measurement window.
    graph.process_block(&mut left, &mut right);
    graph.process_final_limiter_block(&mut left, &mut right);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for block in 0..10_000 {
        let value = (block as f32 * 0.01).sin() * 0.3;
        left.fill(value);
        right.fill(-value * 0.8);
        graph.process_block(&mut left, &mut right);
        graph.process_final_limiter_block(&mut left, &mut right);
    }

    ARMED.store(false, Ordering::Relaxed);
    let _ = std::fs::remove_file(&ir_path);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state graph plan execution ({mode:?}) allocated on the audio path"
    );
}

#[test]
fn realtime_graph_plan_performance_mode_does_not_allocate() {
    run_graph_plan_no_alloc(config::PrecisionMode::Performance);
}

#[test]
fn realtime_graph_plan_quality_mode_does_not_allocate() {
    run_graph_plan_no_alloc(config::PrecisionMode::Quality);
}

/// The multichannel entry point (`NormalMc` plan: routing on every channel,
/// stereo filters on the front pair, volume/seek-fade on every channel) must
/// also be allocation-free, including the per-block plane-view construction
/// and channel de-interleave/re-interleave. Exercises the >2-channel path
/// with channel trim configured and a mid-ramp volume, the scenario that
/// surfaced the scratch-length bug in Phase 1.
#[test]
fn realtime_graph_plan_multichannel_does_not_allocate() {
    let mut cfg = full_chain_config();
    cfg.precision_mode = config::PrecisionMode::Performance;
    cfg.channel_trim.enabled = true;
    cfg.channel_trim.entries = vec![config::ChannelTrimEntry {
        channel: 0,
        gain_db: -3.0,
        ..Default::default()
    }];

    let mut graph = DspGraph::from_config(&cfg, 48_000.0);
    let layout = engine::decode::ChannelLayout::from_count(6);
    graph.set_multichannel_layout(&layout);
    graph.set_volume(0.8);

    let mut interleaved = vec![0.0f32; 128 * 6];

    graph.process_block_multichannel(&mut interleaved, 6);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for block in 0..10_000 {
        let value = (block as f32 * 0.01).sin() * 0.3;
        for (i, s) in interleaved.iter_mut().enumerate() {
            *s = if i % 6 == 0 { value } else { -value * 0.5 };
        }
        graph.process_block_multichannel(&mut interleaved, 6);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state multichannel graph plan execution allocated on the audio path"
    );
}

/// Phase 2: a generation swap executed by the audio thread at a block
/// boundary must itself be allocation-free. The swap path is exactly
/// `Box::from_raw` / `mem::replace` / `Box::into_raw` plus the bounded queue
/// drains — no allocation, no locks. The generations are built and published
/// on a separate CONTROL thread whose allocations are legal (and unmeasured
/// via the thread-local counter); the measured audio thread only executes
/// the swap.
#[test]
fn realtime_graph_swap_does_not_allocate_on_audio_thread() {
    let mut cfg = full_chain_config();
    cfg.precision_mode = config::PrecisionMode::Performance;

    let mut graph = DspGraph::from_config(&cfg, 48_000.0);
    let handle = graph.control_handle();

    // Same synthetic IR as the other graph tests.
    let ir: Vec<(f32, f32)> = (0..2048)
        .map(|i| {
            let e = (-i as f32 / 512.0).exp() * 0.5;
            (e, e * 0.9)
        })
        .collect();
    graph.convolution_mut().engine.set_enabled(true);
    graph
        .convolution_mut()
        .engine
        .load_ir_from_samples(&ir)
        .expect("synthetic IR must load");
    graph.convolution_mut().engine.set_wet_mix(0.3);
    graph.set_volume(0.8);

    let mut left = [0.0f32; 128];
    let mut right = [0.0f32; 128];

    // Warm up before the measurement window (drains the queued volume cmd).
    graph.process_block(&mut left, &mut right);
    graph.process_final_limiter_block(&mut left, &mut right);

    // Pre-build + pre-send the swap batch during warm-up, when this thread's
    // allocations are unmeasured. The control thread then publishes them
    // during the measured window (its allocations are unmeasured too).
    const N_SWAPS: usize = 40;
    let (tx, rx) = std::sync::mpsc::channel::<Box<engine::dsp::graph::GraphGeneration>>();
    for i in 0..N_SWAPS {
        let mut c2 = full_chain_config();
        c2.eq.bands[2].gain_db = i as f32 * 0.25;
        tx.send(engine::dsp::graph::GraphGeneration::from_config(
            &c2,
            48_000.0,
            &graph.multichannel_layout,
        ))
        .expect("pre-warm channel send");
    }
    drop(tx);

    let ctl_handle = handle.clone();
    let ctl = std::thread::spawn(move || {
        while let Ok(gen) = rx.recv() {
            ctl_handle.publish_generation(gen);
        }
    });

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for block in 0..10_000 {
        let value = (block as f32 * 0.01).sin() * 0.3;
        left.fill(value);
        right.fill(-value * 0.8);
        graph.process_block(&mut left, &mut right);
        graph.process_final_limiter_block(&mut left, &mut right);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());
    ctl.join().expect("control thread");

    assert!(handle.generation() >= 1, "swaps must have occurred");
    assert_eq!(
        allocations, 0,
        "generation swap on the audio thread allocated"
    );
}

/// A genuine 44.1 → 48 kHz conversion (not the passthrough rate) must also
/// be allocation-free in steady state.
#[cfg(feature = "resample")]
#[test]
fn realtime_resampler_non_passthrough_does_not_allocate() {
    use engine::dsp::resampler::AudioResampler;
    use engine::ResamplerQuality;

    let mut resampler =
        AudioResampler::<f32>::new(ResamplerQuality::HighQuality, 44_100.0, 48_000.0)
            .expect("44.1 -> 48 kHz resampler");

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for i in 0..40_000 {
        let sample = (i as f32 * 0.01).sin();
        resampler.feed(sample, -sample);
        while resampler.read().is_some() {}
    }
    resampler.flush();
    while resampler.read().is_some() {}

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "non-passthrough resampler processing allocated on the audio path"
    );
}

/// The spatial `BasicPanner` (Phase A, spec Part V) must uphold the same
/// zero-allocation contract as the rest of the engine's hot path: after
/// `prepare`, `process_block` writes into a caller buffer using only
/// preallocated per-object state and stack arrays — no `Vec` growth, no
/// data-structure rebuild.
///
/// Scene construction, the layout, and `prepare` are all control-path (run
/// before the measurement window). The measured loop only calls
/// `process_block` against a fixed scene and input planes.
#[test]
fn realtime_spatial_panner_does_not_allocate() {
    let mut scene = SpatialScene::new(48_000);
    // Several objects spread around the ring to exercise multiple pan paths.
    for (x, y) in [
        (0.0, 1.0),
        (-1.0, 0.0),
        (1.0, 0.0),
        (0.0, -1.0),
        (0.5, 0.5),
        (-0.5, 0.5),
    ] {
        scene
            .create_audio_object(Vec3::new(x, y, 0.0))
            .expect("add object");
    }

    let layout = SpeakerLayout::five_point_one();
    let mut panner = BasicPanner::new(engine::spatial::panner::DEFAULT_SMOOTHING_MS);
    panner.prepare(&layout, 48_000).unwrap();

    // Preallocate input planes and the interleaved output buffer. The input
    // values stay fixed for the whole measured loop (this test measures
    // allocation, not sample correctness).
    const FRAMES: usize = 128;
    let inputs: Vec<Vec<f32>> = (0..6).map(|_| vec![0.3f32; FRAMES]).collect();
    let input_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
    let mut out = vec![0.0f32; 6 * FRAMES];

    // Warm up the smoothing state (control-rate one-pole + per-object paths)
    // before arming the allocator.
    panner
        .process_block(&scene, &input_refs, FRAMES, &mut out)
        .unwrap();
    out.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..10_000 {
        panner
            .process_block(&scene, &input_refs, FRAMES, &mut out)
            .unwrap();
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state spatial panner processing allocated on the audio path"
    );
}

/// The VBAP renderer (Phase 4, spec Part V §25–29) must uphold the same
/// zero-allocation contract: `process_block` solves against the precomputed
/// triangle table, reuses its per-(object,speaker) smoothing state, and
/// never allocates — including the out-of-coverage nearest-speaker fallback
/// and the LFE send path.
///
/// Scene construction, the 7.1.4 layout, and `prepare` (triangulation +
/// Delaunay region filter) are all control-path, run before the measurement
/// window.
#[test]
fn realtime_spatial_vbap_does_not_allocate() {
    let mut scene = SpatialScene::new(48_000);
    // Spread objects across the sphere: covered directions, an overhead
    // object, and one below the floor (out-of-coverage fallback).
    for pos in [
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(1.0, 0.0, 0.5),
        Vec3::new(-1.0, 0.0, 0.0),
        Vec3::new(0.0, -1.0, 0.0),
        Vec3::new(0.0, 0.0, 1.0),
        Vec3::new(0.0, 0.0, -1.0),
    ] {
        scene.create_audio_object(pos).expect("add object");
    }
    // One object with an LFE send (exercises the additive LFE path).
    let lfe_obj = scene
        .create_audio_object(Vec3::new(0.0, 1.0, 0.0))
        .expect("add LFE-send object");
    scene.object_mut(lfe_obj).unwrap().lfe_send = 0.5;

    let layout = SpeakerLayout::seven_point_one_four();
    let mut vbap = VbapRenderer::new();
    vbap.prepare(&layout, 48_000).unwrap();

    const FRAMES: usize = 128;
    let inputs: Vec<Vec<f32>> = (0..7).map(|_| vec![0.3f32; FRAMES]).collect();
    let input_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
    let mut out = vec![0.0f32; 12 * FRAMES];

    // Warm up the smoothing state before arming the allocator.
    vbap.process_block(&scene, &input_refs, FRAMES, &mut out)
        .unwrap();
    out.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..10_000 {
        vbap.process_block(&scene, &input_refs, FRAMES, &mut out)
            .unwrap();
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state VBAP processing allocated on the audio path"
    );
}

/// Object behavior (Phase 5, spec §30/§41/§43–44) must uphold the same
/// zero-allocation contract: directivity curve evaluation (stack-copied
/// table), the per-object occlusion biquad (preallocated state, block-rate
/// coefficients), and the angular-region spread solve (fixed ring samples)
/// all run inside `process_block` with no allocation.
#[test]
fn realtime_spatial_object_behavior_does_not_allocate() {
    let mut scene = SpatialScene::new(48_000);
    // Objects exercising every behavior: cardioid directivity (one yawed to
    // face the listener), heavy occlusion, wide spread, and combinations.
    let mut samples = [1.0f32; engine::spatial::directivity::DIRECTIVITY_TABLE_LEN];
    samples[90] = 0.0; // side null on a custom curve
    let custom = engine::spatial::CustomDirectivity::from_samples(&samples).unwrap();
    for (i, pos) in [
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, -1.0, 0.0),
        Vec3::new(1.0, 0.0, 0.5),
        Vec3::new(0.0, 0.0, -1.0),
    ]
    .iter()
    .enumerate()
    {
        let id = scene.create_audio_object(*pos).expect("add object");
        let obj = scene.object_mut(id).unwrap();
        obj.directivity = match i {
            0 => engine::spatial::Directivity::Cardioid,
            1 => custom.clone().into_directivity(),
            _ => engine::spatial::Directivity::Supercardioid,
        };
        obj.spread = 0.3 + 0.2 * i as f32;
        obj.occlusion = engine::spatial::Occlusion {
            amount: 0.2 + 0.2 * i as f32,
            ..Default::default()
        };
        if i == 0 {
            obj.source_orientation =
                engine::spatial::Quat::from_euler_rad(std::f32::consts::PI, 0.0, 0.0);
        }
    }

    let layout = SpeakerLayout::seven_point_one_four();
    let mut vbap = VbapRenderer::new();
    vbap.prepare(&layout, 48_000).unwrap();

    const FRAMES: usize = 128;
    let inputs: Vec<Vec<f32>> = (0..4).map(|_| vec![0.3f32; FRAMES]).collect();
    let input_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
    let mut out = vec![0.0f32; 12 * FRAMES];

    // Warm up the smoothing state (incl. occlusion cutoff + filter state).
    vbap.process_block(&scene, &input_refs, FRAMES, &mut out)
        .unwrap();
    out.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..10_000 {
        vbap.process_block(&scene, &input_refs, FRAMES, &mut out)
            .unwrap();
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state object behavior processing allocated on the audio path"
    );
}
