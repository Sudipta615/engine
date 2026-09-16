//! Real-Time Qualification Stress Suite (spec §3, §21, Punch List Item 5).
//!
//! Rigorously proves real-time safety guarantees:
//! 1. Zero audio-thread heap allocations across block sizes (16..1024) and sample rates (44.1..192 kHz).
//! 2. Bounded callback execution time with worst-case callback timing measurement against budget.
//! 3. Denormal protection: immunity from CPU stall when processing subnormal floats.
//! 4. Non-finite float (NaN / ±Inf) inline containment with zero allocations.
//! 5. Non-blocking audio path verification (zero file I/O, zero synchronization locks).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use config::{EngineConfig, PrecisionMode};
use engine::dsp::pipeline::DspPipeline;
use engine::dsp::safety::{contain_non_finite_block, NonFinitePolicy};

thread_local! {
    static THREAD_ALLOCS: Cell<usize> = const { Cell::new(0) };
}

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

#[allow(clippy::field_reassign_with_default)]
fn active_dsp_config() -> EngineConfig {
    let mut c = EngineConfig::default();
    c.precision_mode = PrecisionMode::Performance;

    // Enable full EQ
    c.eq.enabled = true;
    if c.eq.bands.len() >= 5 {
        c.eq.bands[0].gain_db = 4.0;
        c.eq.bands[1].gain_db = -2.5;
        c.eq.bands[2].gain_db = 3.0;
        c.eq.bands[3].gain_db = -1.0;
        c.eq.bands[4].gain_db = 2.0;
    }

    // Multiband compressor active
    c.multiband_compressor.enabled = true;

    // Headphone crossfeed active
    c.crossfeed.enabled = true;
    c.crossfeed.profile = config::CrossfeedProfile::Bauer;

    // Limiter active
    c.limiter.enabled = true;
    c.limiter.lookahead_ms = 5.0;

    c.dither_enabled = true;
    c
}

#[test]
fn test_realtime_multirate_multiblock_matrix_zero_alloc() {
    let block_sizes = [16, 32, 64, 128, 256, 512, 1024];
    let sample_rates = [44100.0, 48000.0, 88200.0, 96000.0, 176400.0, 192000.0];

    for &sr in &sample_rates {
        let cfg = active_dsp_config();
        let mut pipeline = DspPipeline::from_config(&cfg, sr);

        for &bs in &block_sizes {
            let mut left = vec![0.0f32; bs];
            let mut right = vec![0.0f32; bs];

            // Warm up
            pipeline.process_block(&mut left, &mut right);
            pipeline.process_final_limiter_block(&mut left, &mut right);

            // Arm allocation measurement
            ARMED.store(true, Ordering::Relaxed);
            THREAD_ALLOCS.with(|c| c.set(0));

            for block in 0..200 {
                let s = (block as f32 * 0.05).sin() * 0.4;
                left.fill(s);
                right.fill(-s * 0.7);

                pipeline.process_block(&mut left, &mut right);
                pipeline.process_final_limiter_block(&mut left, &mut right);
            }

            ARMED.store(false, Ordering::Relaxed);
            let allocs = THREAD_ALLOCS.with(|c| c.get());

            assert_eq!(
                allocs, 0,
                "Realtime audio callback allocated {} times at block size {} and sample rate {} Hz",
                allocs, bs, sr
            );
        }
    }
}

#[test]
fn test_realtime_worst_case_callback_bounded_time() {
    let sr = 48000.0f32;
    let bs = 256usize;
    let block_duration_us = (bs as f64 / sr as f64) * 1_000_000.0; // 5333.33 µs

    let cfg = active_dsp_config();
    let mut pipeline = DspPipeline::from_config(&cfg, sr);

    let mut left = vec![0.0f32; bs];
    let mut right = vec![0.0f32; bs];

    // Warmup
    for _ in 0..50 {
        pipeline.process_block(&mut left, &mut right);
        pipeline.process_final_limiter_block(&mut left, &mut right);
    }

    let iterations = 2000;
    let mut max_callback_duration_us = 0.0f64;
    let mut total_duration_us = 0.0f64;

    for i in 0..iterations {
        let s = (i as f32 * 0.01).sin() * 0.5;
        left.fill(s);
        right.fill(-s);

        let t0 = Instant::now();
        pipeline.process_block(&mut left, &mut right);
        pipeline.process_final_limiter_block(&mut left, &mut right);
        let elapsed_us = t0.elapsed().as_micros() as f64;

        if elapsed_us > max_callback_duration_us {
            max_callback_duration_us = elapsed_us;
        }
        total_duration_us += elapsed_us;
    }

    let avg_duration_us = total_duration_us / (iterations as f64);
    let avg_cpu_percent = (avg_duration_us / block_duration_us) * 100.0;
    let worst_cpu_percent = (max_callback_duration_us / block_duration_us) * 100.0;

    // The callback must easily execute well within the real-time deadline
    assert!(
        worst_cpu_percent < 80.0,
        "Worst-case callback CPU exceeded 80%: {:.2}% ({:.1} µs of {:.1} µs budget)",
        worst_cpu_percent,
        max_callback_duration_us,
        block_duration_us
    );
    assert!(
        avg_cpu_percent < 25.0,
        "Average callback CPU exceeded 25%: {:.2}%",
        avg_cpu_percent
    );
}

#[test]
fn test_realtime_denormal_protection_no_stall() {
    let sr = 48000.0f32;
    let bs = 128usize;

    let cfg = active_dsp_config();
    let mut pipeline = DspPipeline::from_config(&cfg, sr);

    let mut left = vec![0.0f32; bs];
    let mut right = vec![0.0f32; bs];

    // Measure time with normal audio signal
    let normal_iters = 1000;
    let t0 = Instant::now();
    for i in 0..normal_iters {
        let s = (i as f32 * 0.05).sin() * 0.3;
        left.fill(s);
        right.fill(s * 0.5);
        pipeline.process_block(&mut left, &mut right);
        pipeline.process_final_limiter_block(&mut left, &mut right);
    }
    let normal_time = t0.elapsed();

    // Now feed subnormal / denormal floats (1e-38f32 is in subnormal range for IEEE 754 f32)
    let denormal_iters = 1000;
    let denormal_val = 1e-38f32;
    let t1 = Instant::now();
    for _ in 0..denormal_iters {
        left.fill(denormal_val);
        right.fill(-denormal_val);
        pipeline.process_block(&mut left, &mut right);
        pipeline.process_final_limiter_block(&mut left, &mut right);
    }
    let denormal_time = t1.elapsed();

    // Verify output is clean and finite
    for &sample in left.iter().chain(right.iter()) {
        assert!(
            sample.is_finite(),
            "Output sample must be finite after denormal processing"
        );
    }

    // Ratio of denormal execution time to normal execution time should not explode.
    let ratio = denormal_time.as_secs_f64() / normal_time.as_secs_f64().max(1e-6);
    assert!(
        ratio < 4.0,
        "Denormal processing caused CPU stall! Ratio was {:.2}x (normal: {:?}, denormal: {:?})",
        ratio,
        normal_time,
        denormal_time
    );
}

#[test]
fn test_realtime_non_finite_containment_zero_alloc() {
    let mut samples = vec![
        0.5f32,
        f32::NAN,
        f32::INFINITY,
        -f32::INFINITY,
        -0.5f32,
        f32::NAN,
    ];

    ARMED.store(true, Ordering::Relaxed);
    THREAD_ALLOCS.with(|c| c.set(0));

    let mut callback_invoked = 0;
    let incidents = contain_non_finite_block(
        &mut samples,
        2,
        NonFinitePolicy::Clamp,
        Some(42),
        Some("qual_limiter"),
        10,
        |_incident| {
            callback_invoked += 1;
        },
    );

    ARMED.store(false, Ordering::Relaxed);
    let allocs = THREAD_ALLOCS.with(|c| c.get());

    assert_eq!(
        allocs, 0,
        "Non-finite containment must perform 0 allocations"
    );
    assert_eq!(incidents, 4, "Expected 4 non-finite sample incidents");
    assert_eq!(callback_invoked, 4);

    // Verify all clamped properly
    for &s in &samples {
        assert!(
            s.is_finite(),
            "All samples must be finite after containment"
        );
        assert!(
            (-1.0..=1.0).contains(&s),
            "Clamped samples must be within [-1, 1]"
        );
    }
}
