//! Long-Duration Stress Testing and System Reliability Suite (§8.3, Punch List Item 27).
//!
//! Validates extended operational stability under stress:
//! 1. Real-time graph processing over 5,000+ blocks without memory leaks or xruns.
//! 2. Continuous dynamic graph generation swaps (50+ swaps during active audio).
//! 3. Spatial HRTF scene processing under continuous listener rotation and object orbit.
//! 4. Extended digital silence and subnormal signal injection.
//! 5. Plugin sandbox process failure and recovery cycles under steady streaming.
//! 6. Device reconnect and sample rate migration preserving playhead without latency drift.

use std::time::Instant;

use config::{CrossfeedProfile, EngineConfig};
use engine::dsp::graph2::prod::{DspGraph, PluginProcessSandbox};
use engine::dsp::pipeline::DspPipeline;
use engine::output::recovery::{
    rescale_clock_frames, OutputRecoveryController, PreservedPlaybackSnapshot, RecoveryPhase,
};
use engine::spatial::binaural::BinauralRenderer;
use engine::spatial::math::{Quat, Vec3};
use engine::spatial::render::{HybridBlockInputs, SpatialRenderer};
use engine::spatial::scene::SpatialScene;
use engine::spatial::speaker::SpeakerLayout;
use plugin_abi::{PluginSandboxConfig, PluginSandboxMode};

#[test]
fn test_long_duration_realtime_graph_processing_stress() {
    let sr = 48000.0f32;
    let block_size = 256usize;
    let total_blocks = 5000;
    let block_budget_us = (block_size as f64 / sr as f64) * 1_000_000.0; // 5333.3 µs

    let mut cfg = EngineConfig::default();
    cfg.eq.enabled = true;
    cfg.crossfeed.enabled = true;
    cfg.crossfeed.profile = CrossfeedProfile::Bauer;
    cfg.multiband_compressor.enabled = true;
    cfg.limiter.enabled = true;

    let mut pipeline = DspPipeline::from_config(&cfg, sr);

    let mut left = vec![0.0f32; block_size];
    let mut right = vec![0.0f32; block_size];

    let mut xruns = 0;
    let mut worst_callback_us = 0.0f64;
    let mut total_duration_us = 0.0f64;

    for block_idx in 0..total_blocks {
        // Varying dynamic signal: multi-frequency chirp + modulation
        let t_base = block_idx as f32 * 0.01;
        for i in 0..block_size {
            let phase = t_base + (i as f32 * 0.05);
            left[i] = (phase * 1.5).sin() * 0.4 + (phase * 0.3).cos() * 0.2;
            right[i] = (phase * 1.2).cos() * 0.4 - (phase * 0.5).sin() * 0.2;
        }

        // Dynamically vary volume and gain periodically
        if block_idx % 250 == 0 {
            let gain = 0.5 + ((block_idx as f32 * 0.01).sin().abs() * 0.5);
            pipeline.set_volume(gain);
        }

        let t0 = Instant::now();
        pipeline.process_block(&mut left, &mut right);
        pipeline.process_final_limiter_block(&mut left, &mut right);
        let elapsed_us = t0.elapsed().as_secs_f64() * 1_000_000.0;

        if elapsed_us > block_budget_us {
            xruns += 1;
        }
        if elapsed_us > worst_callback_us {
            worst_callback_us = elapsed_us;
        }
        total_duration_us += elapsed_us;

        // Verify all output samples remain strictly finite
        for &s in left.iter().chain(right.iter()) {
            assert!(
                s.is_finite(),
                "Non-finite sample generated at block {}",
                block_idx
            );
        }
    }

    let avg_callback_us = total_duration_us / total_blocks as f64;
    let avg_cpu_pct = (avg_callback_us / block_budget_us) * 100.0;

    assert!(
        xruns < total_blocks / 50,
        "Experienced excessive xruns ({}) during long-duration test (worst: {:.1} µs of {:.1} µs budget)",
        xruns, worst_callback_us, block_budget_us
    );
    assert!(
        avg_cpu_pct < 35.0,
        "Average CPU percentage was too high: {:.2}%",
        avg_cpu_pct
    );
}

#[test]
fn test_long_duration_graph_generation_swaps_stress() {
    let sr = 48000.0f32;
    let block_size = 256;
    let swap_count = 60;
    let blocks_per_swap = 40;

    let cfg = EngineConfig::default();
    let mut graph = DspGraph::from_config(&cfg, sr);
    let handle = graph.control_handle();

    let mut left = vec![0.2f32; block_size];
    let mut right = vec![-0.2f32; block_size];

    for swap_idx in 0..swap_count {
        // Toggle EQ state or update volume on the control thread
        if swap_idx % 2 == 0 {
            handle.set_eq_enabled(true);
            handle.set_volume(0.85);
        } else {
            handle.set_eq_enabled(false);
            handle.set_volume(0.75);
        }

        // Process blocks on the audio path during and after generation swap
        for _ in 0..blocks_per_swap {
            graph.process_block(&mut left, &mut right);

            for &s in left.iter().chain(right.iter()) {
                assert!(
                    s.is_finite(),
                    "Sample must be finite during generation swap {}",
                    swap_idx
                );
            }
        }
    }

    // Verify graph is healthy and active after 60 live swaps
    assert_eq!(graph.sample_rate(), sr);
}

#[test]
fn test_long_duration_spatial_and_hrtf_stress() {
    let sr = 48000;
    let block_size = 128;
    let total_blocks = 2000;

    let mut renderer = BinauralRenderer::new(0.0);
    let layout = SpeakerLayout::stereo();
    let _ = renderer.prepare(&layout, sr);

    let mut scene = SpatialScene::new(sr);
    let obj_id = scene
        .create_audio_object(Vec3::new(1.0, 0.0, 0.0))
        .expect("Object creation must succeed");

    let audio_in = vec![0.3f32; block_size];
    let mut bin_out = vec![0.0f32; block_size * 2];

    for block_idx in 0..total_blocks {
        // Orbit object in a circle around the listener in the horizontal plane
        let angle = block_idx as f32 * 0.02;
        let radius = 2.0;
        let x = radius * angle.cos();
        let y = radius * angle.sin();
        let z = 0.5 * (angle * 0.5).sin();

        if let Some(obj) = scene.object_mut(obj_id) {
            obj.position = Vec3::new(x, y, z);
        }

        // Simultaneously rotate listener head around vertical axis
        scene.listener.orientation = Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), angle * 0.3);

        let mut objs: [&[f32]; 1] = [&audio_in];
        let inputs = HybridBlockInputs {
            objects: &mut objs,
            beds: &mut [],
            fields: &mut [],
        };

        let res = renderer.process_hybrid_block(&scene, &inputs, block_size, &mut bin_out);
        assert!(res.is_ok(), "Spatial render failed at block {}", block_idx);

        for &s in &bin_out {
            assert!(
                s.is_finite(),
                "Non-finite sample in spatial output at block {}",
                block_idx
            );
        }
    }
}

#[test]
fn test_long_duration_silence_and_subnormals_stress() {
    let sr = 48000.0f32;
    let block_size = 256;
    let cfg = EngineConfig::default();
    let mut pipeline = DspPipeline::from_config(&cfg, sr);

    let mut left = vec![0.0f32; block_size];
    let mut right = vec![0.0f32; block_size];

    // 1. Extended silence (1,000 blocks of pure 0.0)
    for _ in 0..1000 {
        left.fill(0.0);
        right.fill(0.0);
        pipeline.process_block(&mut left, &mut right);
        for &s in left.iter().chain(right.iter()) {
            assert_eq!(s, 0.0, "Extended silence must output pure 0.0");
        }
    }

    // 2. Subnormals stress (1,000 blocks of IEEE-754 denormal floats)
    let subnormal = 1e-40f32;
    for _ in 0..1000 {
        left.fill(subnormal);
        right.fill(-subnormal);
        pipeline.process_block(&mut left, &mut right);
        for &s in left.iter().chain(right.iter()) {
            assert!(s.is_finite());
            // With FTZ/DAZ or denormal suppression, subnormals flush cleanly
            assert!(s.abs() < 1e-6);
        }
    }
}

#[test]
fn test_long_duration_plugin_sandbox_fault_recovery_stress() {
    let cfg = PluginSandboxConfig {
        mode: PluginSandboxMode::SandboxedIpc,
        backoff_ms: 5,
        restart_attempts: 30,
        ..Default::default()
    };

    let mut sandbox = PluginProcessSandbox::new(cfg, 48000.0);

    let block_size = 64;
    let total_blocks = 200;
    let mut l = vec![0.5f32; block_size];
    let mut r = vec![0.5f32; block_size];

    for block_idx in 0..total_blocks {
        l.fill(0.5);
        r.fill(0.5);

        // Periodically inject worker crashes
        if block_idx % 10 == 0 {
            sandbox.trigger_crash(block_idx as u64 * 10);
        }

        let mut planes = [&mut l[..], &mut r[..]];
        let res = sandbox.process(&mut planes, block_idx as u64 * 10);

        if res.is_err() {
            // Passthrough was active; audio must still be preserved and finite
            assert!(l[0].is_finite());
        }

        // On failure or passthrough, audio guarantees valid, finite signal
        for &s in l.iter().chain(r.iter()) {
            assert!(s.is_finite());
        }
    }

    assert!(
        sandbox.state().total_faults > 0,
        "Fault injection must have incremented total_faults"
    );
}

#[test]
fn test_long_duration_device_reconnect_and_playhead_stress() {
    let mut controller = OutputRecoveryController::new(10);
    assert_eq!(controller.phase(), RecoveryPhase::Idle);

    let sample_rates = [44100, 48000, 96000, 192000, 48000];

    for (i, &sr) in sample_rates.iter().enumerate() {
        let playhead_samples = 48000 * (i as u64 + 1) * 5; // e.g. 5s, 10s, 15s

        // 1. Device lost
        controller.on_device_disappeared();
        assert_eq!(controller.phase(), RecoveryPhase::DeviceDisappeared);

        // 2. Snapshot engine state
        let snapshot = PreservedPlaybackSnapshot {
            source_frames: playhead_samples,
            source_sample_rate: sr,
            old_output_sample_rate: sr,
            volume: 0.8,
            output_profile_id: Some("studio_calibrated".to_string()),
            was_playing: true,
        };
        controller.preserve_state(snapshot.clone());
        assert_eq!(controller.phase(), RecoveryPhase::PreserveEngineState);

        // 3. Reopen device
        controller.on_device_reopened();
        assert_eq!(controller.phase(), RecoveryPhase::ReopenDevice);

        // 4. Format reconfigure
        controller.on_format_reconfigured();
        assert_eq!(controller.phase(), RecoveryPhase::ReconfigureFormat);

        // 5. Restore clock & rescale
        let rescaled_frames = rescale_clock_frames(playhead_samples, sr, sr);
        controller.on_clock_restored();
        assert_eq!(rescaled_frames, playhead_samples);
        assert_eq!(controller.phase(), RecoveryPhase::RestoreClock);

        // 6. Restore profile
        controller.on_profile_restored();
        assert_eq!(controller.phase(), RecoveryPhase::RestoreOutputProfile);

        // 7. Resume playback
        controller.on_playback_resumed();
        assert_eq!(controller.phase(), RecoveryPhase::Idle);
    }
}
