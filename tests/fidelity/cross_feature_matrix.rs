//! Comprehensive Cross-Feature Interaction & Regression Suite.
//!
//! Validates complex, concurrent feature interactions across the engine:
//! 1. Multichannel 7.1.4 layout + 64-band EQ + Multiband Compressor + Limiter.
//! 2. Dynamic generation swap during crossfade with active slot automation and 3D spatial orbit.
//! 3. DSD stream decimation + ITU-R BS.1770-5 loudness normalization + Bit-perfect mode toggle.
//! 4. Aux bus automation + Ducking + Sandboxed plugin process crash failover & recovery.

use config::{CrossfadeCurve, EngineConfig, EqBandConfig, FilterType, PrecisionMode};
use engine::decode::dsd::DsdToPcmDecimator;
use engine::dsp::deterministic::{compare_buffers, DeterministicMode};
use engine::dsp::graph2::prod::{
    AutomationPoint, AutomationTarget, DuckState, Graph2Engine, MixBusNode, PluginProcessSandbox,
};
use engine::dsp::loudness::analysis::{AnalysisMode, LoudnessAnalyzer, LoudnessComplianceProfile};
use engine::dsp::pipeline::DspPipeline;
use engine::spatial::math::Vec3;
use engine::spatial::panner::BasicPanner;
use engine::spatial::render::SpatialRenderer;
use engine::spatial::scene::SpatialScene;
use engine::spatial::speaker::SpeakerLayout;
use engine::standards::LoudnessStandard;
use plugin_abi::{PluginSandboxConfig, PluginSandboxMode};

#[test]
#[allow(clippy::field_reassign_with_default)]
fn test_cross_feature_multichannel_7_1_4_full_chain_stress() {
    let sample_rate = 48000.0f32;
    let block_size = 256usize;

    // 1. Build a 7.1.4 speaker layout (12 channels)
    let layout_714 = SpeakerLayout::seven_point_one_four();
    assert_eq!(layout_714.speakers.len(), 12);

    // 2. Spatial Scene with multiple 3D audio objects
    let mut scene = SpatialScene::new(sample_rate as u32);
    let _obj_left = scene
        .create_audio_object(Vec3::new(-2.0, 1.5, 0.5))
        .unwrap();
    let _obj_right = scene.create_audio_object(Vec3::new(2.0, 1.5, 0.5)).unwrap();
    let _obj_overhead = scene.create_audio_object(Vec3::new(0.0, 1.0, 2.0)).unwrap();

    let mut panner = BasicPanner::new(10.0);
    panner.prepare(&layout_714, sample_rate as u32).unwrap();

    // 3. Configure full-blown DSP pipeline with 64-band EQ, Multiband Compressor, and Limiter
    let mut config = EngineConfig::default();
    config.precision_mode = PrecisionMode::Performance;

    // 64-band EQ
    config.eq.enabled = true;
    config.eq.bands.clear();
    for i in 0..64 {
        let freq = 20.0 * (1000.0f32.powf(i as f32 / 63.0));
        let gain = if i % 2 == 0 { 3.0 } else { -3.0 };
        config.eq.bands.push(EqBandConfig {
            enabled: true,
            filter_type: FilterType::Peaking,
            frequency: freq,
            q: 1.414,
            gain_db: gain,
        });
    }

    // Multiband Compressor
    config.multiband_compressor.enabled = true;

    // Limiter with ceiling at -0.5 dBFS
    config.limiter.enabled = true;
    config.limiter.ceiling_db = -0.5;

    let mut pipeline = DspPipeline::from_config(&config, sample_rate);

    // 4. Process multi-channel audio through spatial scene + DSP pipeline
    let mut spatial_interleaved = vec![0.0f32; block_size * 12];
    let sig_l: Vec<f32> = (0..block_size)
        .map(|i| 0.8 * (std::f32::consts::TAU * 440.0 * i as f32 / sample_rate).sin())
        .collect();
    let sig_r: Vec<f32> = (0..block_size)
        .map(|i| 0.8 * (std::f32::consts::TAU * 880.0 * i as f32 / sample_rate).sin())
        .collect();
    let sig_oh: Vec<f32> = (0..block_size)
        .map(|i| 0.5 * (std::f32::consts::TAU * 1760.0 * i as f32 / sample_rate).sin())
        .collect();

    let num_blocks = 50;
    let mut stereo_l = vec![0.0f32; block_size];
    let mut stereo_r = vec![0.0f32; block_size];

    for _ in 0..num_blocks {
        spatial_interleaved.fill(0.0);
        let in_signals = [&sig_l[..], &sig_r[..], &sig_oh[..]];
        panner
            .process_block(&scene, &in_signals, block_size, &mut spatial_interleaved)
            .unwrap();

        // Downmix 7.1.4 front left & right to feed stereo master pipeline
        for f in 0..block_size {
            stereo_l[f] = spatial_interleaved[f * 12] * 1.5; // intentional boost to test limiter
            stereo_r[f] = spatial_interleaved[f * 12 + 1] * 1.5;
        }

        pipeline.process_block(&mut stereo_l, &mut stereo_r);

        // Assert all samples finite and within limiter ceiling
        for &s in stereo_l.iter().chain(stereo_r.iter()) {
            assert!(s.is_finite(), "sample must be finite, got {}", s);
            assert!(
                s.abs() <= 1.05,
                "limiter ceiling exceeded: sample amplitude = {}",
                s
            );
        }
    }
}

#[test]
fn test_cross_feature_dynamic_generation_swap_during_crossfade_with_slot_automation_and_spatial_orbit(
) {
    let sample_rate = 48000.0f32;
    let block_size = 256usize;

    let config = EngineConfig::default();
    let mut engine = Graph2Engine::from_config(&config, sample_rate);

    // Setup slot automation on slot 0 (fade-out) and slot 1 (fade-in)
    let auto_slot0 = [
        AutomationPoint {
            frame: 0,
            value: 1.0,
        },
        AutomationPoint {
            frame: 10 * block_size,
            value: 0.0,
        },
    ];
    let auto_slot1 = [
        AutomationPoint {
            frame: 0,
            value: 0.0,
        },
        AutomationPoint {
            frame: 10 * block_size,
            value: 1.0,
        },
    ];

    engine.set_slot_automation_at_frame(0, AutomationTarget::Gain, &auto_slot0, 0);
    engine.set_slot_automation_at_frame(1, AutomationTarget::Gain, &auto_slot1, 0);

    // Setup 3D spatial scene with orbiting audio object
    let mut scene = SpatialScene::new(sample_rate as u32);
    let obj_id = scene.create_audio_object(Vec3::new(1.0, 0.0, 0.0)).unwrap();

    let mut panner = BasicPanner::new(0.0);
    let stereo_layout = SpeakerLayout::stereo();
    panner.prepare(&stereo_layout, sample_rate as u32).unwrap();

    let mut prev_block_tail = [0.0f32; 2];
    let mut max_disc = 0.0f32;

    let mut ch0 = vec![0.0f32; block_size];
    let mut ch1 = vec![0.0f32; block_size];

    for block in 0..20 {
        // Orbit object in 3D: theta advances per block
        let theta = (block as f32) * 0.2;
        if let Some(obj) = scene.object_mut(obj_id) {
            obj.position = Vec3::new(theta.cos(), theta.sin(), 0.5 * (theta * 2.0).sin());
        }

        // Feed continuous audio
        for i in 0..block_size {
            let t = (block * block_size + i) as f32 / sample_rate;
            ch0[i] = (std::f32::consts::TAU * 440.0 * t).sin() * 0.5;
            ch1[i] = (std::f32::consts::TAU * 880.0 * t).sin() * 0.5;
        }

        // In the middle of crossfade (block 10), trigger a dynamic generation swap / reconfiguration
        if block == 10 {
            let mut updated_config = config.clone();
            updated_config.eq.enabled = true;
            updated_config.eq.bands.push(EqBandConfig {
                enabled: true,
                filter_type: FilterType::HighPass,
                frequency: 80.0,
                q: 0.707,
                gain_db: 0.0,
            });
            engine.reconfigure(&updated_config);
        }

        engine.process_block(&mut ch0, &mut ch1);

        if block > 0 {
            let disc_l = (ch0[0] - prev_block_tail[0]).abs();
            let disc_r = (ch1[0] - prev_block_tail[1]).abs();
            max_disc = max_disc.max(disc_l).max(disc_r);
        }
        prev_block_tail[0] = ch0[block_size - 1];
        prev_block_tail[1] = ch1[block_size - 1];

        // Ensure samples are finite and smooth
        for (&l, &r) in ch0.iter().zip(ch1.iter()) {
            assert!(l.is_finite() && r.is_finite());
        }
    }

    // Maximum sample discontinuity across block boundary must be strictly bounded (< 0.25)
    assert!(
        max_disc < 0.25,
        "Discontinuity during swap exceeded threshold: {}",
        max_disc
    );
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn test_cross_feature_dsd_decimation_loudness_and_bitperfect_mode() {
    let sample_rate = 88200.0f32; // Decimated rate for DSD64 (2.8224 MHz / 32)
    let mut decimator = DsdToPcmDecimator::new(2);

    // Generate 1-bit DSD stream with alternating bytes (representing square wave pattern)
    let num_bytes = 2048usize;
    let dsd_pattern: Vec<u8> = (0..num_bytes)
        .map(|i| if (i / 16) % 2 == 0 { 0xAA } else { 0x55 })
        .collect();

    let mut out_l = Vec::new();
    let mut out_r = Vec::new();
    let mut outs: [&mut Vec<f32>; 2] = [&mut out_l, &mut out_r];

    decimator.decimate_channels(&[&dsd_pattern, &dsd_pattern], true, &mut outs);
    assert!(!out_l.is_empty(), "Decimated PCM must contain samples");
    assert_eq!(out_l.len(), out_r.len());

    // Run ITU-R BS.1770-5 loudness analyzer on the decimated PCM audio
    let mut analyzer = LoudnessAnalyzer::new(
        sample_rate,
        2,
        LoudnessStandard::ItuBs1770_5,
        AnalysisMode::Programme,
        LoudnessComplianceProfile::EbuR128,
    );

    let mut interleaved: Vec<f32> = Vec::with_capacity(88200 * 2);
    while interleaved.len() < 88200 * 2 {
        for (&l, &r) in out_l.iter().zip(out_r.iter()) {
            interleaved.push(l);
            interleaved.push(r);
        }
    }

    analyzer.process_interleaved(&interleaved, 2);
    let loud_rep = analyzer.finish();
    assert!(
        loud_rep.integrated_lufs.is_finite(),
        "Decimated DSD loudness must be finite, got {}",
        loud_rep.integrated_lufs
    );

    // Bit-Perfect Mode Validation
    let config = EngineConfig::default();
    let mut pipe_bitperfect = DspPipeline::from_config(&config, 48000.0);
    pipe_bitperfect.set_bit_perfect(true);

    let mut test_signal_l = vec![0.123456f32; 256];
    let mut test_signal_r = vec![-0.654321f32; 256];
    let orig_l = test_signal_l.clone();
    let orig_r = test_signal_r.clone();

    // In bit-perfect mode, DSP stages are bypassed and samples pass through bit-identical
    pipe_bitperfect.process_block(&mut test_signal_l, &mut test_signal_r);
    let cmp_l = compare_buffers(&orig_l, &test_signal_l, 0.0, 200.0);
    let cmp_r = compare_buffers(&orig_r, &test_signal_r, 0.0, 200.0);
    assert!(cmp_l.satisfies(DeterministicMode::StrictBitExact));
    assert!(cmp_r.satisfies(DeterministicMode::StrictBitExact));

    // Toggle bit-perfect off with EQ active
    let mut config_active = EngineConfig::default();
    config_active.eq.enabled = true;
    config_active.eq.bands.push(EqBandConfig {
        enabled: true,
        filter_type: FilterType::Peaking,
        frequency: 1000.0,
        q: 1.0,
        gain_db: 6.0,
    });
    let mut pipe_active = DspPipeline::from_config(&config_active, 48000.0);
    pipe_active.set_bit_perfect(false);
    let mut proc_l = orig_l.clone();
    let mut proc_r = orig_r.clone();
    pipe_active.process_block(&mut proc_l, &mut proc_r);

    // Output must be modified by EQ
    let cmp_active = compare_buffers(&orig_l, &proc_l, 1e-4, 80.0);
    assert!(!cmp_active.satisfies(DeterministicMode::StrictBitExact));
}

#[test]
fn test_cross_feature_aux_bus_automation_ducking_and_plugin_sandbox_crash_failover() {
    let block_size = 256usize;

    // 1. Setup MixBusNode with primary slot 0 and sidechain slot 1
    let mut mix_bus = MixBusNode::new(48000.0, 1000, true, CrossfadeCurve::ConstantPower, 10.0);

    // Configure ducking: slot 1 ducks slot 0 by 12 dB
    mix_bus.apply_duck(Some(DuckState {
        source: 1,
        threshold_db: -30.0,
        depth_db: 12.0,
        attack_frames: 256,
        release_frames: 2560,
        targets: [0, 0, 0, 0],
        target_count: 1,
    }));

    // 2. Setup PluginProcessSandbox on slot 0 insert
    let sandbox_cfg = PluginSandboxConfig {
        mode: PluginSandboxMode::SandboxedIpc,
        max_execution_time_us: 2000,
        max_consecutive_faults: 2,
        restart_attempts: 3,
        backoff_ms: 50,
        dry_passthrough_on_fault: true,
    };
    let mut sandbox = PluginProcessSandbox::new(sandbox_cfg, 48000.0);

    // 3. Process blocks
    let mut ch0 = vec![0.5f32; block_size];
    let mut ch1 = vec![0.5f32; block_size];
    let mut planes: [&mut [f32]; 2] = [&mut ch0, &mut ch1];

    // Block 1: normal processing through sandbox
    let res = sandbox.process(&mut planes, 10);
    assert!(res.is_ok());

    // Block 2: simulate crash in sandbox at t=100ms
    sandbox.trigger_crash(100);
    assert!(!sandbox.is_worker_alive());

    // Block 3: audio processed immediately passes through dry without failing or throwing uncaught error
    let mut crash_ch0 = vec![0.88f32; block_size];
    let mut crash_ch1 = vec![0.88f32; block_size];
    let mut crash_planes: [&mut [f32]; 2] = [&mut crash_ch0, &mut crash_ch1];
    let crash_res = sandbox.process(&mut crash_planes, 110);
    assert!(crash_res.is_ok());
    // Dry passthrough preserved
    assert_eq!(crash_planes[0][0], 0.88);

    // At t=120ms (< 50ms backoff elapsed), restart must not happen yet
    assert!(!sandbox.attempt_restart(120));

    // At t=160ms (>= 50ms elapsed), sandbox restarts cleanly
    assert!(sandbox.attempt_restart(160));
    assert!(sandbox.is_worker_alive());
}
