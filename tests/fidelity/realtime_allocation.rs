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

use engine::dsp::graph2::prod::DspGraph;
use engine::dsp::graph2::prod::Graph2Engine;
use engine::dsp::loudness::LoudnessMetadata;
use engine::dsp::pipeline::DspPipeline;
use engine::spatial::{
    BasicPanner, DecoderPolicy, Quat, SpatialRenderer, SpatialScene, SpeakerLayout, VbapRenderer,
    Vec3,
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

/// The plan executor (`DspGraph`) must uphold the same zero-allocation
/// contract as the pipeline it mirrors: the enum-dispatch `run_plan` hot path,
/// the f64 quality-mode promotion, and the multichannel `NormalMc` plan
/// (de-interleave → routing → chain → re-interleave) all run entirely on
/// preallocated scratch.
fn run_graph_plan_no_alloc(mode: config::PrecisionMode) {
    let mut cfg = full_chain_config();
    cfg.precision_mode = mode;

    // The aux bus with per-slot sends and the global convolution
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
/// Surfaced the scratch-length bug in.
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

/// A generation swap executed by the audio thread at a block
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
    let (tx, rx) = std::sync::mpsc::channel::<Box<engine::dsp::graph2::prod::GraphGeneration>>();
    for i in 0..N_SWAPS {
        let mut c2 = full_chain_config();
        c2.eq.bands[2].gain_db = i as f32 * 0.25;
        tx.send(engine::dsp::graph2::prod::GraphGeneration::from_config(
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

/// The VBAP renderer (spec Part V §25–29) must uphold the same
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

/// Object behavior (spec §30/§41/§43–44) must uphold the same
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

/// Hybrid beds & fields (spec §13/§37) must uphold the same
/// zero-allocation contract inside `process_hybrid_block`: bed routing is a
/// role-table scan, and the diffuse field mixer reads/writes preallocated
/// per-speaker delay rings with a fixed stack-array plane list — no `Vec`
/// growth anywhere on the hot path.
#[test]
fn realtime_spatial_hybrid_does_not_allocate() {
    let mut scene = SpatialScene::new(48_000);
    // Objects with behaviors.
    let obj = scene.create_audio_object(Vec3::new(0.0, 1.0, 0.0)).unwrap();
    scene.object_mut(obj).unwrap().directivity = engine::spatial::Directivity::Cardioid;
    scene.object_mut(obj).unwrap().spread = 0.5;
    // Two beds (5.1 + stereo) and two fields.
    scene
        .create_bed(engine::decode::ChannelLayout::FivePointOne)
        .unwrap();
    scene
        .create_bed(engine::decode::ChannelLayout::Stereo)
        .unwrap();
    scene.create_field().unwrap();
    scene.create_field().unwrap();

    let layout = SpeakerLayout::seven_point_one_four();
    let mut vbap = VbapRenderer::new();
    vbap.prepare(&layout, 48_000).unwrap();

    const FRAMES: usize = 128;
    let object_planes: Vec<Vec<f32>> = vec![vec![0.3f32; FRAMES]];
    let object_refs: Vec<&[f32]> = object_planes.iter().map(|v| v.as_slice()).collect();
    let bed_planes: Vec<Vec<f32>> = (0..8).map(|_| vec![0.2f32; FRAMES]).collect();
    let bed_refs: Vec<&[f32]> = bed_planes.iter().map(|v| v.as_slice()).collect();
    let field_planes: Vec<Vec<f32>> = (0..2).map(|_| vec![0.1f32; FRAMES]).collect();
    let field_refs: Vec<&[f32]> = field_planes.iter().map(|v| v.as_slice()).collect();
    let inputs = engine::spatial::render::HybridBlockInputs {
        objects: &object_refs,
        beds: &bed_refs,
        fields: &field_refs,
    };
    let mut out = vec![0.0f32; 12 * FRAMES];

    // Warm up smoothing + field delay rings before arming the allocator.
    vbap.process_hybrid_block(&scene, &inputs, FRAMES, &mut out)
        .unwrap();
    out.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..10_000 {
        vbap.process_hybrid_block(&scene, &inputs, FRAMES, &mut out)
            .unwrap();
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state hybrid (objects + beds + fields) processing allocated on the audio path"
    );
}

/// The ambisonic renderer (spec Part VI §32–37) must uphold the
/// same zero-allocation contract: the per-frame listener rotation (stack
/// frame + `rotate_bus_frame`) and the decode matrix multiplication all run
/// on preallocated scratch — no `Vec` growth on the hot path. The field
/// mixer's bus path (encode → decode → decorrelation rings) is exercised by
/// `realtime_spatial_hybrid_does_not_allocate` above, which now rides the
/// same ambisonic pipeline.
#[test]
fn realtime_ambisonic_renderer_does_not_allocate() {
    let layout = SpeakerLayout::seven_point_one_four();
    let mut renderer = engine::spatial::AmbisonicRenderer::new(DecoderPolicy::MaxRe);
    renderer.prepare(&layout, 48_000).unwrap();

    const FRAMES: usize = 128;
    // A world-encoded bus: constant front plane wave [W, Y, Z, X].
    let mut frame = [0.0f32; 4];
    engine::spatial::ambisonic::encode_plane_wave(Vec3::Y, 1.0, &mut frame);
    let planes: Vec<Vec<f32>> = frame.iter().map(|&c| vec![c; FRAMES]).collect();
    let input_refs: Vec<&[f32]> = planes.iter().map(|v| v.as_slice()).collect();
    let mut out = vec![0.0f32; 12 * FRAMES];
    let mut scene = SpatialScene::new(48_000);

    // Warm up (incl. the bus scratch) before arming the allocator.
    scene
        .listener
        .set_orientation(Quat::from_euler_rad(0.0, 0.0, 0.0));
    renderer
        .process_block(&scene, &input_refs, FRAMES, &mut out)
        .unwrap();
    out.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    // Sweep the listener yaw every block to exercise the per-frame bus
    // rotation on the hot path.
    for block in 0..10_000 {
        let yaw = block as f32 * 0.001;
        scene
            .listener
            .set_orientation(Quat::from_euler_rad(yaw, 0.0, 0.0));
        renderer
            .process_block(&scene, &input_refs, FRAMES, &mut out)
            .unwrap();
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state ambisonic renderer processing allocated on the audio path"
    );
}

/// Room acoustics (spec §49/§55) must uphold the same
/// zero-allocation contract inside `process_hybrid_block`: the image-source
/// enumeration is pure arithmetic into fixed stack arrays, the reflection
/// rings/tap matrix are preallocated, and the Schroeder tail writes into a
/// preallocated scratch — worst case is order-2 (24 images per object) with
/// the late field active.
#[test]
fn realtime_spatial_room_does_not_allocate() {
    let mut scene = SpatialScene::new(48_000);
    scene.listener.set_position(Vec3::new(6.0, 5.0, 1.5));
    scene.room = engine::spatial::Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.3,
        reflection_order: 2, // worst case: 24 image sources per object
        rt60_ms: 800.0,
        late_mix: 0.7,
        late_distance: false,
        speed_of_sound: 343.0,
    };
    // Several participating objects (one occluded, one directional).
    for (i, pos) in [
        Vec3::new(1.0, 5.0, 1.5),
        Vec3::new(11.0, 5.0, 1.5),
        Vec3::new(6.0, 1.0, 1.5),
        Vec3::new(6.0, 9.0, 1.5),
    ]
    .iter()
    .enumerate()
    {
        let id = scene.create_audio_object(*pos).expect("add object");
        let obj = scene.object_mut(id).unwrap();
        obj.room_send = 1.0;
        if i == 0 {
            obj.occlusion = engine::spatial::Occlusion {
                amount: 0.5,
                ..Default::default()
            };
        }
        if i == 1 {
            obj.directivity = engine::spatial::Directivity::Cardioid;
        }
    }

    let layout = SpeakerLayout::seven_point_one_four();
    let mut vbap = VbapRenderer::new();
    vbap.prepare(&layout, 48_000).unwrap();

    const FRAMES: usize = 128;
    let object_planes: Vec<Vec<f32>> = (0..4).map(|_| vec![0.3f32; FRAMES]).collect();
    let object_refs: Vec<&[f32]> = object_planes.iter().map(|v| v.as_slice()).collect();
    let inputs = engine::spatial::render::HybridBlockInputs {
        objects: &object_refs,
        beds: &[],
        fields: &[],
    };
    let mut out = vec![0.0f32; 12 * FRAMES];

    // Warm up: fill the reflection rings, converge the tap smoothing, and
    // ring the tail before arming the allocator.
    vbap.process_hybrid_block(&scene, &inputs, FRAMES, &mut out)
        .unwrap();
    out.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..10_000 {
        vbap.process_hybrid_block(&scene, &inputs, FRAMES, &mut out)
            .unwrap();
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state room (reflections + late field) processing allocated on the audio path"
    );
}

/// The binaural renderer (spec Part VII §47–48) must uphold the
/// same zero-allocation contract inside `process_hybrid_block`: the per-ear
/// ITD rings, the head-shadow shelves, the room's reflection taps, and the
/// virtual-ring diffuse path are all preallocated flat at `prepare` — the
/// measured loop runs the *worst case* (order-2 room, occluded, spread,
/// directional, a bed, a field, and the late field) with no `Vec` growth.
#[test]
fn realtime_spatial_binaural_does_not_allocate() {
    let mut scene = SpatialScene::new(48_000);
    scene.listener.set_position(Vec3::new(6.0, 5.0, 1.5));
    scene.room = engine::spatial::Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.3,
        reflection_order: 2, // worst case: 24 image sources per object
        rt60_ms: 800.0,
        late_mix: 0.7,
        late_distance: false,
        speed_of_sound: 343.0,
    };
    // Participating objects exercising every head-model path: cardioid
    // directivity, occlusion, spread, LFE send, room send.
    let mut samples = [1.0f32; engine::spatial::directivity::DIRECTIVITY_TABLE_LEN];
    samples[45] = 0.0;
    let custom = engine::spatial::CustomDirectivity::from_samples(&samples).unwrap();
    for (i, pos) in [
        Vec3::new(1.0, 5.0, 1.5),
        Vec3::new(11.0, 5.0, 1.5),
        Vec3::new(6.0, 1.0, 1.5),
        Vec3::new(6.0, 9.0, 1.5),
    ]
    .iter()
    .enumerate()
    {
        let id = scene.create_audio_object(*pos).expect("add object");
        let obj = scene.object_mut(id).unwrap();
        obj.room_send = 1.0;
        obj.spread = 0.4 + 0.1 * i as f32;
        obj.lfe_send = 0.3;
        obj.occlusion = engine::spatial::Occlusion {
            amount: 0.2 + 0.2 * i as f32,
            ..Default::default()
        };
        obj.directivity = if i == 0 {
            engine::spatial::Directivity::Cardioid
        } else {
            custom.clone().into_directivity()
        };
    }
    scene
        .create_bed(engine::decode::ChannelLayout::Stereo)
        .unwrap();
    scene.create_field().unwrap();

    let layout = SpeakerLayout::stereo();
    let mut renderer = engine::spatial::BinauralRenderer::new(0.0);
    renderer.prepare(&layout, 48_000).unwrap();

    const FRAMES: usize = 128;
    let object_planes: Vec<Vec<f32>> = (0..4).map(|_| vec![0.3f32; FRAMES]).collect();
    let object_refs: Vec<&[f32]> = object_planes.iter().map(|v| v.as_slice()).collect();
    let bed_planes: Vec<Vec<f32>> = (0..2).map(|_| vec![0.2f32; FRAMES]).collect();
    let bed_refs: Vec<&[f32]> = bed_planes.iter().map(|v| v.as_slice()).collect();
    let field_planes: Vec<Vec<f32>> = vec![vec![0.1f32; FRAMES]];
    let field_refs: Vec<&[f32]> = field_planes.iter().map(|v| v.as_slice()).collect();
    let inputs = engine::spatial::render::HybridBlockInputs {
        objects: &object_refs,
        beds: &bed_refs,
        fields: &field_refs,
    };
    let mut out = vec![0.0f32; 2 * FRAMES];

    // Warm up: fill the ITD rings, converge shelf smoothing + reflection
    // taps, ring the tail, and fill the virtual-ring delay lines before
    // arming the allocator.
    renderer
        .process_hybrid_block(&scene, &inputs, FRAMES, &mut out)
        .unwrap();
    out.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    // Sweep the listener yaw every block to exercise the per-frame head
    // cue updates (ITD glides through the fractional delay lines).
    for block in 0..10_000 {
        let yaw = block as f32 * 0.001;
        scene
            .listener
            .set_orientation(Quat::from_euler_rad(yaw, 0.0, 0.0));
        renderer
            .process_hybrid_block(&scene, &inputs, FRAMES, &mut out)
            .unwrap();
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state binaural processing allocated on the audio path"
    );
}

/// Head tracking (spec §48/§136) must also be allocation-free:
/// the tracker is host-side (the renderers never touch it), but a host may
/// run it on the audio thread's caller — `push` and `sample` are pure
/// fixed-size state (interpolation + one-pole + optional rate limit), no
/// `Vec` growth, no locks.
#[test]
fn realtime_head_tracker_does_not_allocate() {
    use engine::spatial::{HeadSample, HeadTracker, Quat, TrackingConfig};

    let mut tracker = HeadTracker::new(TrackingConfig {
        smoothing_ms: 8.0,
        max_angular_rate_deg_s: 540.0,
    });
    tracker.push(HeadSample::new(0.0, Quat::IDENTITY));

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    // A 10k-sample jittery stream (the IMU callback rate is independent of
    // the block rate) with the host sampling at block rate.
    for i in 1..=10_000 {
        let t = 0.001 * i as f64;
        let yaw = (i as f32 * 0.05).sin() * 2.0 + i as f32 * 1e-4;
        tracker.push(HeadSample::new(t, Quat::from_euler_rad(yaw, 0.0, 0.0)));
        let q = tracker.sample(t);
        assert!(q.is_finite());
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state head tracking allocated on the audio path"
    );
}

/// Higher-order ambisonics (roadmap) must uphold the
/// zero-allocation contract at order 2: the 9-channel SH basis, the exact
/// order-2 bus rotation (WXYZ→WXYZ+UV), and the per-order max-rE decoder
/// weights all run on preallocated flat buffers.
#[test]
fn realtime_hoa_renderer_does_not_allocate() {
    use engine::spatial::{AmbisonicRenderer, MAX_AMBISONIC_ORDER};

    const _: () = assert!(MAX_AMBISONIC_ORDER >= 2);
    let layout = SpeakerLayout::seven_point_one_four();
    let mut renderer = AmbisonicRenderer::with_order(DecoderPolicy::MaxRe, 2);
    renderer.prepare(&layout, 48_000).unwrap();
    assert_eq!(renderer.order(), 2);

    const FRAMES: usize = 128;
    const CH: usize = 9; // order-2 channel count
                         // A world-encoded order-2 bus: constant front plane wave.
    let mut frame = [0.0f32; CH];
    engine::spatial::encode_plane_wave_n(2, Vec3::Y, 1.0, &mut frame);
    let planes: Vec<Vec<f32>> = frame.iter().map(|&c| vec![c; FRAMES]).collect();
    let input_refs: Vec<&[f32]> = planes.iter().map(|v| v.as_slice()).collect();
    let mut out = vec![0.0f32; 12 * FRAMES];
    let mut scene = SpatialScene::new(48_000);

    // Warm up (incl. the 9-channel bus scratch) before arming the allocator.
    renderer
        .process_block(&scene, &input_refs, FRAMES, &mut out)
        .unwrap();
    out.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    // Sweep the listener yaw every block: the order-2 rotation matrices are
    // recomputed per frame on the hot path.
    for block in 0..10_000 {
        let yaw = block as f32 * 0.001;
        scene
            .listener
            .set_orientation(Quat::from_euler_rad(yaw, 0.0, 0.0));
        renderer
            .process_block(&scene, &input_refs, FRAMES, &mut out)
            .unwrap();
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state order-2 ambisonic processing allocated on the audio path"
    );
}

/// The SpatialNode (roadmap) is a real plan step in the
/// production graph, so its steady-state path — binaural head model with the
/// room's image sources + late field on the master's front pair — must be
/// allocation-free like every other graph node. The node preallocates its
/// renderer flat at construction/prepare; the measured loop only runs the
/// compiled plan.
#[test]
fn realtime_spatial_node_does_not_allocate() {
    let mut graph = DspGraph::from_config(&config::EngineConfig::default(), 48_000.0);
    graph.set_spatial_enabled(true);
    graph.set_spatial_screen(0.0, 30.0, 0.0, 1.0);
    graph.set_spatial_room(true, 12.0, 10.0, 3.0, 0.3, 2, 800.0, 0.5, false, 0.5);
    graph.set_spatial_listener(0.0, 0.0, 0.0);
    graph.drain_queued_control();
    assert!(graph.spatial().enabled());

    let mut left = [0.0f32; 128];
    let mut right = [0.0f32; 128];

    // Warm up: fill the ITD rings and the room's reflection/tail state.
    left[0] = 1.0;
    right[0] = 0.5;
    graph.process_block(&mut left, &mut right);
    left.fill(0.0);
    right.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    // A block-rate listener sweep exercises the per-frame cue updates and
    // the per-block room tap recomputation inside the plan step.
    for block in 0..10_000 {
        let yaw = block as f32 * 0.001;
        graph.set_spatial_listener(yaw, 0.0, 0.0);
        graph.drain_queued_control();
        left.fill(0.2);
        right.fill(0.15);
        graph.process_block(&mut left, &mut right);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state SpatialNode processing allocated on the audio path"
    );
}

/// A runtime-moving listener — pose
/// targets re-set every block through the queued control surface and the
/// node gliding (nlerp + one-pole + rate limit) inside the plan step —
/// must stay allocation-free on the audio path. This exercises the exact
/// production surface: `set_spatial_listener_pose` enqueues, the drain
/// applies, `glide_listener` runs inside `render_block`.
#[test]
fn realtime_spatial_listener_motion_does_not_allocate() {
    let mut graph = DspGraph::from_config(&config::EngineConfig::default(), 48_000.0);
    graph.set_spatial_enabled(true);
    graph.set_spatial_screen(0.0, 30.0, 0.0, 1.0);
    graph.set_spatial_room(true, 12.0, 10.0, 3.0, 0.3, 2, 800.0, 0.5, false, 0.5);
    graph.set_spatial_listener_pose(Quat::IDENTITY, Vec3::ZERO);
    graph.drain_queued_control();
    assert!(graph.spatial().enabled());

    let mut left = [0.0f32; 128];
    let mut right = [0.0f32; 128];

    // Warm up: fill the ITD rings and the room state before arming.
    left[0] = 1.0;
    right[0] = 0.5;
    graph.process_block(&mut left, &mut right);
    left.fill(0.0);
    right.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    // A per-block pose target sweep drives the glide every block: the
    // target changes (never converges), so the nlerp / one-pole / rate
    // limit path runs fully each block alongside the room + binaural
    // render.
    let mut yaw = 0.0f32;
    let mut x = 0.0f32;
    for block in 0..10_000 {
        yaw = (yaw + 0.05) % 360.0;
        x = (x + 0.01) % 2.0;
        let q = engine::spatial::math::Quat::from_euler_rad(
            yaw.to_radians(),
            (block as f32 * 0.001).sin(),
            0.0,
        );
        graph.set_spatial_listener_pose(q, Vec3::new(x, 0.5, 0.0));
        graph.drain_queued_control();
        left.fill(0.2);
        right.fill(0.15);
        graph.process_block(&mut left, &mut right);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "listener-motion spatial processing allocated on the audio path"
    );
}

/// The measured-HRTF dataset path (roadmap) must be
/// allocation-free even in the worst case: bilinear IR interpolation into
/// preallocated scratch, FIR convolution on preallocated rings, and the
/// analytic fallback shelf/notch chain all coexist in one block.
#[test]
fn realtime_hrtf_dataset_path_does_not_allocate() {
    use engine::spatial::{BinauralRenderer, HrtfDataset, Occlusion};

    let mut scene = SpatialScene::new(48_000);
    scene.listener.set_position(Vec3::new(6.0, 5.0, 1.5));
    scene.room = engine::spatial::Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.3,
        reflection_order: 2, // worst case: 24 image sources per object
        rt60_ms: 800.0,
        late_mix: 0.7,
        late_distance: false,
        speed_of_sound: 343.0,
    };
    for pos in [
        Vec3::new(1.0, 5.0, 1.5),
        Vec3::new(11.0, 5.0, 1.5),
        Vec3::new(6.0, 1.0, 1.5),
        Vec3::new(6.0, 9.0, 1.5),
    ] {
        let id = scene.create_audio_object(pos).expect("add object");
        let obj = scene.object_mut(id).unwrap();
        obj.room_send = 1.0;
        obj.spread = 0.3;
        obj.lfe_send = 0.3;
        obj.occlusion = Occlusion {
            amount: 0.3,
            ..Default::default()
        };
    }
    scene
        .create_bed(engine::decode::ChannelLayout::Stereo)
        .unwrap();
    scene.create_field().unwrap();

    let ds = HrtfDataset::synthetic(48_000, 64, 15.0, 15.0);
    let mut renderer = BinauralRenderer::new(0.0);
    renderer.use_dataset(Some(std::sync::Arc::new(ds)));
    renderer.prepare(&SpeakerLayout::stereo(), 48_000).unwrap();

    const FRAMES: usize = 128;
    let object_planes: Vec<Vec<f32>> = (0..4).map(|_| vec![0.3f32; FRAMES]).collect();
    let object_refs: Vec<&[f32]> = object_planes.iter().map(|v| v.as_slice()).collect();
    let bed_planes: Vec<Vec<f32>> = (0..2).map(|_| vec![0.2f32; FRAMES]).collect();
    let bed_refs: Vec<&[f32]> = bed_planes.iter().map(|v| v.as_slice()).collect();
    let field_planes: Vec<Vec<f32>> = vec![vec![0.1f32; FRAMES]];
    let field_refs: Vec<&[f32]> = field_planes.iter().map(|v| v.as_slice()).collect();
    let inputs = engine::spatial::render::HybridBlockInputs {
        objects: &object_refs,
        beds: &bed_refs,
        fields: &field_refs,
    };
    let mut out = vec![0.0f32; 2 * FRAMES];

    // Warm up: fill the FIR rings and reflection state before arming.
    renderer
        .process_hybrid_block(&scene, &inputs, FRAMES, &mut out)
        .unwrap();
    out.fill(0.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for block in 0..10_000 {
        let yaw = block as f32 * 0.001;
        scene
            .listener
            .set_orientation(Quat::from_euler_rad(yaw, 0.0, 0.0));
        renderer
            .process_hybrid_block(&scene, &inputs, FRAMES, &mut out)
            .unwrap();
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state HRTF dataset rendering allocated on the audio path"
    );
}

/// The Graph 2.0 **realtime executor** must render a compiled
/// topology with zero allocation on the audio path — plan build and publish
/// are control-side; the measured loop only adopts and renders.
///
/// The topology exercises every RT op family: a sine Source (phase state),
/// a Split fan-out, a Gain branch, a Delay ring across block boundaries, a
/// Convolution overlap-add pipeline, an HRTF per-ear pair, a ratio-1
/// Resampler window, and a Mix fan-in before the Sink — so any
/// `Vec::push`/`resize`/`collect` that slips into an RT op fails here.
/// A mid-loop `publish` also exercises the block-boundary adopt (the
/// retired-plan handoff itself must not allocate audio-side).
#[test]
fn graph2_rt_executor_does_not_allocate() {
    use engine::dsp::graph2::rt::{RtPlan, RtScenes};
    use engine::prelude::{Graph2, NodeParams, PortId, RtExecutor, SourceParams, TestSignal};

    const BLOCK: usize = 256;
    let mut g = Graph2::new();
    let src = g.add_source("tone");
    g.set_params(
        src,
        NodeParams::Source(SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 997.0,
        }),
    );
    let split = g.add_split("sw", 2);
    let gain = g.add_gain("dry", 0.5);
    let conv = g.add_convolution("ir", vec![0.4, -0.2, 0.1, 0.05, 0.3, -0.15, 0.07, 0.02]);
    let delay = g.add_delay("wet", 300);
    let hrtf = g.add_hrtf("bin", vec![0.9, 0.3, -0.1], vec![0.8, 0.25]);
    let rsmp = g.add_resampler("rs", 1.0);
    let mix = g.add_mix("sum", 4);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), gain, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), delay, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, rsmp, PortId::IN).unwrap();
    g.add_edge(delay, PortId::OUT, conv, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, hrtf, PortId::IN).unwrap();
    g.add_edge(rsmp, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(hrtf, PortId(0), mix, PortId(1)).unwrap();
    g.add_edge(hrtf, PortId(1), mix, PortId(2)).unwrap();
    g.add_edge(delay, PortId::OUT, mix, PortId(3)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();

    // Control side: build (allocating is fine) + warm-up render.
    let scenes = RtScenes::new();
    let mut ex =
        RtExecutor::new(RtPlan::build(&g, &order, BLOCK, 48_000.0, Some(&scenes), None).unwrap());
    let mut out = vec![0.0f32; BLOCK];
    for _ in 0..4 {
        ex.render_block(&mut out);
    }

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    // Steady state, with a mid-loop plan publish to exercise the adopt.
    for block in 0..10_000 {
        if block == 5_000 {
            // Publishing is control-side work; the adopt at the block
            // boundary must be allocation-free audio-side.
            ARMED.store(false, Ordering::Relaxed);
            ex.publish(RtPlan::build(&g, &order, BLOCK, 48_000.0, Some(&scenes), None).unwrap());
            ARMED.store(true, Ordering::Relaxed);
        }
        ex.render_block(&mut out);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state Graph2 RT rendering allocated on the audio path"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The Graph2 production engine (`Graph2Engine`) — the lowered
// plans drive the same arena, so the zero-allocation contract must hold
// identically. Shadow mode is *diagnostic by design* (it allocates on
// purpose); these cases measure the flag-off production path.
// ─────────────────────────────────────────────────────────────────────────────

/// The Graph2 production engine's stereo hot path (lowered plans → arena
/// plan execution) must be allocation-free in steady state — identical
/// machinery to `DspGraph`, identical contract.
#[test]
fn realtime_graph2_prod_stereo_does_not_allocate() {
    let mut cfg = full_chain_config();
    cfg.precision_mode = config::PrecisionMode::Performance;

    let ir_path = std::env::temp_dir().join(format!(
        "rt_g2_ir_{}_{}.wav",
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

    let mut g2 = Graph2Engine::from_config(&cfg, 48_000.0);

    let ir: Vec<(f32, f32)> = (0..2048)
        .map(|i| {
            let e = (-i as f32 / 512.0).exp() * 0.5;
            (e, e * 0.9)
        })
        .collect();
    g2.with_graph(|g| {
        g.convolution_mut().engine.set_enabled(true);
        g.convolution_mut()
            .engine
            .load_ir_from_samples(&ir)
            .expect("synthetic IR must load");
        g.convolution_mut().engine.set_wet_mix(0.3);
    });
    g2.set_slot_send(0, 1.0, 0.5);
    g2.set_slot_send(1, 1.0, 0.5);
    g2.set_volume(0.8);
    g2.set_balance(-0.2);
    g2.drain_queued_control();

    let mut left = [0.0f32; 128];
    let mut right = [0.0f32; 128];

    // Warm up all stateful stages before the measurement window.
    g2.process_block(&mut left, &mut right);
    g2.process_final_limiter_block(&mut left, &mut right);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for block in 0..10_000 {
        let value = (block as f32 * 0.01).sin() * 0.3;
        left.fill(value);
        right.fill(-value * 0.8);
        g2.process_block(&mut left, &mut right);
        g2.process_final_limiter_block(&mut left, &mut right);
    }

    ARMED.store(false, Ordering::Relaxed);
    let _ = std::fs::remove_file(&ir_path);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state Graph2 production stereo execution allocated on the audio path"
    );
}

/// The Graph2 production engine's multichannel hot path (the lowered
/// `NormalMc` plan) must be allocation-free in steady state.
#[test]
fn realtime_graph2_prod_multichannel_does_not_allocate() {
    let mut cfg = full_chain_config();
    cfg.precision_mode = config::PrecisionMode::Performance;
    cfg.channel_trim.enabled = true;
    cfg.channel_trim.entries = vec![config::ChannelTrimEntry {
        channel: 0,
        gain_db: -3.0,
        ..Default::default()
    }];

    let mut g2 = Graph2Engine::from_config(&cfg, 48_000.0);
    let layout = engine::decode::ChannelLayout::from_count(6);
    g2.set_multichannel_layout(&layout);
    g2.set_volume(0.8);

    let mut interleaved = vec![0.0f32; 128 * 6];

    g2.process_block_multichannel(&mut interleaved, 6);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for block in 0..10_000 {
        let value = (block as f32 * 0.01).sin() * 0.3;
        for (i, s) in interleaved.iter_mut().enumerate() {
            *s = if i % 6 == 0 { value } else { -value * 0.5 };
        }
        g2.process_block_multichannel(&mut interleaved, 6);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "steady-state Graph2 production multichannel execution allocated on the audio path"
    );
}

/// The Graph2 production engine's generation swap (drain-adopt at the block
/// boundary, lowered-plan generation published from the control thread)
/// must be allocation-free on the audio thread — the same contract as
/// `DspGraph`.
#[test]
fn realtime_graph2_prod_swap_does_not_allocate_on_audio_thread() {
    let mut cfg = full_chain_config();
    cfg.precision_mode = config::PrecisionMode::Performance;

    let mut g2 = Graph2Engine::from_config(&cfg, 48_000.0);
    let handle = g2.control_handle();
    g2.set_volume(0.8);

    let mut left = [0.0f32; 128];
    let mut right = [0.0f32; 128];

    g2.process_block(&mut left, &mut right);
    g2.process_final_limiter_block(&mut left, &mut right);

    // Pre-build + pre-send the swap batch during warm-up (control-side
    // allocations are unmeasured). The control thread publishes during the
    // measured window; the audio thread only adopts.
    const N_SWAPS: usize = 40;
    let (tx, rx) = std::sync::mpsc::channel::<Box<engine::dsp::graph2::prod::GraphGeneration>>();
    for i in 0..N_SWAPS {
        let mut c2 = full_chain_config();
        c2.eq.bands[2].gain_db = i as f32 * 0.25;
        tx.send(engine::dsp::graph2::prod::GraphGeneration::from_config(
            &c2,
            48_000.0,
            &g2.multichannel_layout,
        ))
        .expect("pre-warm channel send");
    }
    drop(tx);

    let ctl_handle = g2.inner().control_handle();
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
        g2.process_block(&mut left, &mut right);
        g2.process_final_limiter_block(&mut left, &mut right);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());
    ctl.join().expect("control thread");

    assert!(handle.generation() >= 1, "swaps must have occurred");
    assert_eq!(
        allocations, 0,
        "Graph2 production generation swap on the audio thread allocated"
    );
}

// ── Plugin host insert ──────────────────────────────────────────

/// The plugin host's plan step (a statically-registered reference echo
/// plugin processing every block) must uphold the same zero-allocation
/// contract as every other node: the plugin's `process` runs inside the
/// plan, so an allocating plugin would fail here. The plan-window counter
/// is armed only for the steady-state loop (buffers + graph construction
/// happen before).
#[test]
fn realtime_plugin_host_step_does_not_allocate() {
    // Register the reference plugin (idempotent, process-wide).
    let host = unsafe { plugin_abi::PluginHost::from_vtable(plugin_test_echo::ECHO_VTABLE) }
        .expect("reference plugin validates");
    let _ = engine::dsp::graph2::prod::register_static_host(std::sync::Arc::new(host));
    let uid = plugin_test_echo::ECHO_UID;
    let mut source = String::with_capacity(38);
    source.push_str("static:");
    for byte in uid {
        source.push(char::from_digit((byte >> 4) as u32, 16).expect("hex"));
        source.push(char::from_digit((byte & 0xf) as u32, 16).expect("hex"));
    }

    let mut cfg = full_chain_config();
    cfg.plugins.slots.push(config::PluginSlotConfig {
        source,
        enabled: true,
        params: vec![(0, 1.0), (1, 250.0), (3, 0.5)],
        state: None,
    });

    let mut graph = engine::dsp::graph2::prod::DspGraph::from_config(&cfg, 48_000.0);
    // Warm-up: instantiation + prepare allocate on the control path.
    let mut l = vec![0.5f32; 512];
    let mut r = vec![0.5f32; 512];
    graph.process_block(&mut l, &mut r);

    THREAD_ALLOCS.with(|c| c.set(0));
    ARMED.store(true, Ordering::SeqCst);
    for _ in 0..16 {
        graph.process_block(&mut l, &mut r);
    }
    ARMED.store(false, Ordering::SeqCst);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "the plugin host plan step must not allocate (got {allocs})"
    );
}

/// The spatial master's cue path — bank step, overlay
/// evaluation, program snapshot/restore — must be allocation-free on the
/// audio thread while cues are actively evaluating (the worst case:
/// looping cues on both program objects, so every block overlays).
#[test]
fn realtime_spatial_cue_overlay_does_not_allocate() {
    use config::EngineConfig;

    let cfg = EngineConfig {
        spatial: config::SpatialConfig {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut graph = DspGraph::from_config(&cfg, 48_000.0);
    // A looping swell on both program objects: every block evaluates the
    // overlay (gain + spread + position curves) — the steady worst case.
    graph.set_cue_bank(&[
        config::SpatialCueConfig {
            name: "sway_l".to_string(),
            target: 0,
            gain: Some(config::CurveScalarConfig {
                points: vec![(0.0, 0.1), (0.5, 0.9), (1.0, 0.1)],
            }),
            spread: Some(config::CurveScalarConfig {
                points: vec![(0.0, 0.0), (1.0, 0.5)],
            }),
            position: Some(config::CurveVec3Config {
                points: vec![(0.0, [-2.0, 2.0, 0.0]), (1.0, [2.0, 2.0, 0.0])],
            }),
            looping: true,
            hold: false,
        },
        config::SpatialCueConfig {
            name: "sway_r".to_string(),
            target: 1,
            gain: Some(config::CurveScalarConfig {
                points: vec![(0.0, 0.9), (0.5, 0.1), (1.0, 0.9)],
            }),
            spread: Some(config::CurveScalarConfig {
                points: vec![(0.0, 0.5), (1.0, 0.0)],
            }),
            position: Some(config::CurveVec3Config {
                points: vec![(0.0, [2.0, 2.0, 0.0]), (1.0, [-2.0, 2.0, 0.0])],
            }),
            looping: true,
            hold: false,
        },
    ]);
    let l_idx = graph.spatial().cue_index_of("sway_l").unwrap();
    let r_idx = graph.spatial().cue_index_of("sway_r").unwrap();
    graph.trigger_spatial_cue_by_index(l_idx);
    graph.trigger_spatial_cue_by_index(r_idx);

    const FRAMES: usize = 256;
    let tone: Vec<f32> = (0..FRAMES)
        .map(|i| 0.4 * (2.0 * std::f32::consts::PI * 997.0 * i as f32 / 48_000.0).sin())
        .collect();
    let mut lo = tone.clone();
    let mut ro = tone.clone();

    // Warm up: one block through prepare + the first overlay before
    // arming the allocator.
    graph.process_block(&mut lo, &mut ro);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));
    for _ in 0..1_000 {
        graph.process_block(&mut lo, &mut ro);
    }
    ARMED.store(false, Ordering::Relaxed);
    let allocations = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocations, 0,
        "the cue overlay path allocated on the audio thread"
    );
}

#[test]
fn hybrid_spatial_renderer_allocates_zero_bytes() {
    use config::{SpatialBassConfig, SpatialBassMode};
    use engine::spatial::render::{HybridBlockInputs, RendererKind};
    use engine::spatial::HybridSpatialRenderer;

    let custom_bass = SpatialBassConfig {
        mode: SpatialBassMode::BassManaged,
        crossover_hz: 120.0,
        ..Default::default()
    };

    let layouts = [
        (SpeakerLayout::stereo(), 2),
        (SpeakerLayout::seven_point_one(), 8),
        (SpeakerLayout::seven_point_one_four(), 12),
        // 16 channels custom layout (e.g. 9.1.6)
        (
            SpeakerLayout::custom(
                (0..16)
                    .map(|i| {
                        let theta = i as f32 * std::f32::consts::TAU / 16.0;
                        Vec3::new(theta.cos(), theta.sin(), 0.0)
                    })
                    .collect(),
            ),
            16,
        ),
    ];

    let block_sizes = [64, 128, 256, 512, 1024];

    for (layout, expected_channels) in &layouts {
        let mut renderer = HybridSpatialRenderer::new(RendererKind::Vbap, custom_bass.clone());
        renderer.prepare(layout, 48_000).expect("prepare");

        let mut scene = SpatialScene::new(48_000);
        let obj_id = scene.create_audio_object(Vec3::new(1.0, 1.0, 0.0)).unwrap();
        scene.object_mut(obj_id).unwrap().bass_send = 0.5;

        for &frames in &block_sizes {
            let total_samples = frames * expected_channels;
            let mut out = vec![0.0f32; total_samples];
            let obj_data = vec![0.1f32; frames];
            let obj_slices: Vec<&[f32]> = vec![&obj_data];
            let inputs = HybridBlockInputs {
                objects: &obj_slices,
                beds: &[],
                fields: &[],
            };

            // Warm up
            renderer
                .process_hybrid_block(&scene, &inputs, frames, &mut out)
                .expect("warmup");

            ARMED.store(true, Ordering::Relaxed);
            THREAD_ALLOCS.with(|c| c.set(0));

            for _ in 0..100 {
                renderer
                    .process_hybrid_block(&scene, &inputs, frames, &mut out)
                    .expect("process");
            }

            ARMED.store(false, Ordering::Relaxed);
            let allocs = THREAD_ALLOCS.with(|c| c.get());

            assert_eq!(
                allocs, 0,
                "HybridSpatialRenderer::process_hybrid_block allocated {} times with {} channels at block size {}",
                allocs, expected_channels, frames
            );
        }
    }
}

#[test]
fn professional_meters_allocates_zero_bytes() {
    use engine::dsp::ProfessionalMeters;

    let channel_counts = [1, 2, 6, 8, 12, 16];
    let block_sizes = [64, 128, 256, 512, 1024];

    for &channels in &channel_counts {
        let meters = ProfessionalMeters::new(48000, channels);

        for &frames in &block_sizes {
            let buf = vec![0.1f32; frames * channels];

            // Warm up
            meters.process_interleaved(&buf, channels);

            ARMED.store(true, Ordering::Relaxed);
            THREAD_ALLOCS.with(|c| c.set(0));

            for _ in 0..100 {
                meters.process_interleaved(&buf, channels);
            }

            ARMED.store(false, Ordering::Relaxed);
            let allocs = THREAD_ALLOCS.with(|c| c.get());

            assert_eq!(
                allocs, 0,
                "ProfessionalMeters::process_interleaved allocated {} times with {} channels at block size {}",
                allocs, channels, frames
            );
        }
    }
}

#[test]
fn hoa_decoder_order9_allocates_zero_bytes() {
    use engine::spatial::{HoaConfig, HoaDecoder, HoaDecoding, SpeakerLayout};

    let layout = SpeakerLayout::seven_point_one_four();
    let mut decoder = HoaDecoder::new(HoaConfig {
        order: 9,
        decoding: HoaDecoding::MaxRe,
    });
    decoder.prepare(&layout, 48_000).expect("prepare");

    let bus_channels = 100;
    let bus = vec![0.1f32; bus_channels];
    let mut out = vec![0.0f32; layout.speakers.len()];

    // Warmup
    decoder.decode(&bus, 1, &mut out);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..100 {
        decoder.decode(&bus, 1, &mut out);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "HoaDecoder::decode allocated {} times during steady state",
        allocs
    );
}

#[test]
fn spherical_hrtf_interpolation_allocates_zero_bytes() {
    use engine::spatial::{Ear, HrtfCorpus, HrtfDataset, HrtfLoadOptions, HrtfMeasurement, Vec3};

    let pts = [Vec3::X, -Vec3::X, Vec3::Y, -Vec3::Y, Vec3::Z, -Vec3::Z];
    let measurements: Vec<HrtfMeasurement> = pts
        .iter()
        .map(|&p| HrtfMeasurement {
            direction: [p.x, p.y, p.z],
            left: vec![1.0; 64],
            right: vec![1.0; 64],
        })
        .collect();

    let corpus = HrtfCorpus {
        sample_rate: 48_000,
        source: None,
        measurements,
        mesh_hint: None,
    };
    let opts = HrtfLoadOptions {
        taps: 64,
        target_sample_rate: 48_000,
        normalize: engine::spatial::HrtfNormalize::None,
    };

    let ds = HrtfDataset::from_corpus_irregular(&corpus, &opts).expect("from_corpus_irregular");
    let mut scratch = [0.0f32; 64];
    let query = Vec3::new(1.0, 1.0, 1.0).normalized().unwrap();

    // Warmup
    ds.interpolate_direction(query, Ear::Left, &mut scratch);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..100 {
        ds.interpolate_direction(query, Ear::Left, &mut scratch);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "HrtfDataset::interpolate_direction allocated {} times during steady state",
        allocs
    );
}

#[test]
fn constant_spread_panning_allocates_zero_bytes() {
    use engine::spatial::{constant_power_spread_gains, Vec3, MAX_SPREAD_GAINS};

    let speakers = [
        (0, Vec3::new(-1.0, 1.0, 0.0).normalized().unwrap()),
        (1, Vec3::new(1.0, 1.0, 0.0).normalized().unwrap()),
        (2, Vec3::new(0.0, 1.0, 0.0).normalized().unwrap()),
    ];
    let mut gains = [(0usize, 0.0f32); MAX_SPREAD_GAINS];

    // Warmup
    constant_power_spread_gains(
        Vec3::Y,
        0.5,
        std::f32::consts::FRAC_PI_2,
        &mut gains,
        &speakers,
    );

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..100 {
        constant_power_spread_gains(
            Vec3::Y,
            0.5,
            std::f32::consts::FRAC_PI_2,
            &mut gains,
            &speakers,
        );
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "constant_power_spread_gains allocated {} times during steady state",
        allocs
    );
}

#[test]
fn frequency_dependent_occlusion_allocates_zero_bytes() {
    use engine::spatial::{
        BroadbandOcclusion, DiffractionOcclusion, FrequencyDependentOcclusion,
        MaterialTransmission, OcclusionBandCoeffs, OcclusionBandState,
    };

    let mut state = OcclusionBandState::default();
    let fd = FrequencyDependentOcclusion {
        broadband: BroadbandOcclusion {
            amount: 0.5,
            ..Default::default()
        },
        diffraction: DiffractionOcclusion::default(),
        material: MaterialTransmission::WOOD,
    };
    let coeffs = OcclusionBandCoeffs::new(48_000.0, &fd);

    // Warmup
    let _ = state.process(0.5, &coeffs);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..1000 {
        let _ = state.process(0.5, &coeffs);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "OcclusionBandState::process allocated {} times during steady state",
        allocs
    );
}

#[test]
fn nearfield_wavefront_curvature_allocates_zero_bytes() {
    use engine::spatial::WavefrontCurvatureState;

    let mut state = WavefrontCurvatureState::default();

    // Warmup
    state.update_wavefront(0.3, 0.0875, 0.5, 48_000.0);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..1000 {
        state.update_wavefront(0.3, 0.0875, 0.5, 48_000.0);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "WavefrontCurvatureState::update_wavefront allocated {} times during steady state",
        allocs
    );
}

#[test]
fn psychoacoustic_bass_allocates_zero_bytes() {
    use config::PsychoacousticBassConfig;
    use engine::spatial::PsychoacousticBassProcessor;

    let config = PsychoacousticBassConfig::default();
    let mut processor = PsychoacousticBassProcessor::new(&config, 1.0, 48_000.0);
    let mut block = vec![0.1f32; 256];

    // Warmup
    processor.process_block(&mut block);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..100 {
        processor.process_block(&mut block);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "PsychoacousticBassProcessor::process_block allocated {} times during steady state",
        allocs
    );
}

// ── Phase 4: Advanced Engine, Modulation, Plugins & Analysis Zero Allocation Tests ──

#[test]
fn drift_controller_allocates_zero_bytes() {
    use engine::output::DriftController;

    let mut ctrl = DriftController::new_adaptive(true, 8192);
    // Warmup
    ctrl.update(4100);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for i in 0..1000 {
        ctrl.update(4100 + (i % 5));
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "DriftController::update allocated {} times during steady state",
        allocs
    );
}

#[test]
fn automation_track_render_block_allocates_zero_bytes() {
    use engine::dsp::timeline::curve::{AutomationTrack, InterpolationMode};

    let mut track = AutomationTrack::new();
    track.insert(0, 0.0, InterpolationMode::Linear);
    track.insert(500, 1.0, InterpolationMode::SCurve);
    track.insert(1000, 0.2, InterpolationMode::Exponential);

    let mut buf = [0.0f32; 128];
    // Warmup
    track.render_block(0, &mut buf);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for i in 0..1000 {
        track.render_block((i * 128) % 1000, &mut buf);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "AutomationTrack::render_block allocated {} times during steady state",
        allocs
    );
}

#[test]
fn modulation_processors_allocate_zero_bytes() {
    use engine::dsp::modulation::{
        AdsrEnvelope, EnvelopeFollower, Lfo, ModSource, ModulationMatrix,
    };

    let mut lfo = Lfo::new(48000.0, 5.0);
    let mut env = AdsrEnvelope::new(48000.0, 0.01, 0.05, 0.5, 0.1);
    let mut follower = EnvelopeFollower::new(48000.0, 0.005, 0.05);
    let mut matrix = ModulationMatrix::new();
    matrix.connect(ModSource::Lfo(0), 1, 0.5);

    let audio = [0.5f32; 128];
    let mut out_lfo = [0.0f32; 128];
    let mut out_env = [0.0f32; 128];
    let mut out_fol = [0.0f32; 128];

    // Warmup
    lfo.render_block(&mut out_lfo);
    env.render_block(&mut out_env);
    follower.process_block(&audio, &mut out_fol);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..500 {
        lfo.render_block(&mut out_lfo);
        env.render_block(&mut out_env);
        follower.process_block(&audio, &mut out_fol);
        let _ = matrix.evaluate_param_offset(1, |_| 0.5);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "Modulation processors allocated {} times during steady state",
        allocs
    );
}

#[test]
fn creative_fx_allocate_zero_bytes() {
    use engine::fx::{
        Chorus, CombFilter, DistortionType, Flanger, Phaser, PingPongDelay, RingModulator,
        Saturator,
    };

    let mut comb = CombFilter::new(512, 100.0, 0.5, 0.2);
    let mut ping_pong = PingPongDelay::new(512, 100, 0.5);
    let mut chorus = Chorus::new(48000.0);
    let mut flanger = Flanger::new(48000.0);
    let mut phaser = Phaser::new(48000.0);
    let mut ring_mod = RingModulator::new(48000.0, 440.0);
    let mut saturator = Saturator::new(DistortionType::TubeSaturation, 2.0);

    let mut left = [0.3f32; 128];
    let mut right = [0.3f32; 128];

    // Warmup
    comb.process_plane(&mut left);
    ping_pong.process_stereo(&mut left, &mut right);
    chorus.process_stereo(&mut left, &mut right);
    flanger.process_plane(&mut left);
    phaser.process_plane(&mut left);
    ring_mod.process_plane(&mut left);
    saturator.process_plane(&mut left);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..500 {
        comb.process_plane(&mut left);
        ping_pong.process_stereo(&mut left, &mut right);
        chorus.process_stereo(&mut left, &mut right);
        flanger.process_plane(&mut left);
        phaser.process_plane(&mut left);
        ring_mod.process_plane(&mut left);
        saturator.process_plane(&mut left);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "Creative FX processors allocated {} times during steady state",
        allocs
    );
}

#[test]
fn analysis_engine_allocates_zero_bytes() {
    use engine::dsp::analysis::AnalysisEngine;

    let mut engine = AnalysisEngine::new(48000.0, 512);
    let samples = [0.2f32; 512];

    // Warmup
    let _ = engine.process_block(&samples);

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    for _ in 0..200 {
        let _ = engine.process_block(&samples);
    }

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());
    assert_eq!(
        allocs, 0,
        "AnalysisEngine::process_block allocated {} times during steady state",
        allocs
    );
}
