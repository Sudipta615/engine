//! Production diagnostics fidelity tests.
//!
//! End-to-end integration suite covering the full production-readiness surface
//! of the engine's diagnostic and state machinery:
//!
//! 1. **Real-time Health Monitor** (§6.1) — lock-free per-callback metrics
//!    (duration, CPU load, drift, xruns, buffer fill) with zero allocations.
//! 2. **Node-Level Diagnostics** (§6.4) — per-node latency, tail, peak level,
//!    gain reduction, error and non-finite sample counts.
//! 3. **Structured Diagnostic Event Ring** (§6.5) — severity-classified events
//!    pushed from the audio thread and drained lock-free to the control side.
//! 4. **Unified Parameter Metadata** (§9.1) — descriptor round-trips: normalize,
//!    denormalize, step snapping, clamping, and curve interpolation.
//! 5. **Versioned State & Preset Serialization** (§9.2) — schema-versioned
//!    envelope round-trips and forward-incompatible schema rejection.
//! 6. **Seamless Graph Transitions** (§5.3) — equal-power crossfader and
//!    continuous parameter smoother; click/discontinuity magnitude bounds.
//! 7. **Device/Output Recovery** (§10.1) — 7-phase recovery state machine and
//!    sample-accurate clock frame rescaling across sample-rate changes.

use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

use engine::diagnostics::{
    DiagnosticKind, DiagnosticSeverity, RawDiagnosticEvent,
    RealtimeDiagnosticQueue, RealtimeHealthMonitor,
};
use engine::dsp::graph2::diagnostics::NodeDiagnostics;
use engine::dsp::graph2::transitions::{
    measure_max_discontinuity, ContinuousParameterSmoother, TransitionConfig,
    TransitionCrossfader, TransitionCurve,
};
use engine::dsp::parameters::
    {ParameterCurve, ParameterDescriptor, ParameterId, ParameterRegistry, ParameterUnit};
use engine::output::recovery::{
    rescale_clock_frames, OutputRecoveryController, PreservedPlaybackSnapshot, RecoveryPhase,
};
use engine::state::{
    NodeState, StateMigrationError, VersionedEnvelope,
    CURRENT_ENGINE_VERSION, STATE_SCHEMA_VERSION,
};

#[test]
fn test_pillar1_realtime_health_monitoring() {
    let monitor = RealtimeHealthMonitor::new();
    let snap0 = monitor.snapshot();
    assert_eq!(snap0.xruns, 0);
    assert_eq!(snap0.underruns, 0);
    assert_eq!(snap0.overruns, 0);

    // Simulate 10 audio callbacks
    for _ in 0..10 {
        let t0 = monitor.record_callback_start();
        thread::sleep(Duration::from_micros(100));
        monitor.record_callback_end(t0, 256, 48000, 128, 512);
    }

    monitor.record_underrun();
    monitor.record_overrun();
    monitor.update_drift_and_ratio(-12, 0.999988);
    monitor.update_graph_generation(5);
    monitor.update_latency(1024);

    let snap = monitor.snapshot();
    assert!(snap.callback_duration_us > 50.0);
    assert!(snap.avg_duration_us > 50.0);
    assert!(snap.worst_duration_us >= snap.callback_duration_us);
    assert_eq!(snap.xruns, 2);
    assert_eq!(snap.underruns, 1);
    assert_eq!(snap.overruns, 1);
    assert_eq!(snap.clock_drift_ppm, -12);
    assert!((snap.resampler_ratio - 0.999988).abs() < 1e-4);
    assert_eq!(snap.graph_generation, 5);
    assert_eq!(snap.latency_samples, 1024);
    assert!((snap.buffer_fill_pct - 25.0).abs() < 0.1);
}

#[test]
fn test_pillar2_node_level_diagnostics() {
    let limiter_diag = NodeDiagnostics::new(2, "limiter", true)
        .with_latency_and_tail(64, 1.33, 1200, 25.0)
        .with_metering(-0.2, -0.1, -4.5)
        .with_errors(0, 0)
        .with_cpu_cost(8.2);

    assert_eq!(limiter_diag.node_id, 2);
    assert_eq!(limiter_diag.name, "limiter");
    assert!(limiter_diag.active);
    assert_eq!(limiter_diag.gain_reduction_db, -4.5);
    assert_eq!(limiter_diag.latency_samples, 64);
    assert_eq!(limiter_diag.tail_samples, 1200);

    // Serialization round trip
    let json = serde_json::to_string(&limiter_diag).unwrap();
    let back: NodeDiagnostics = serde_json::from_str(&json).unwrap();
    assert_eq!(back, limiter_diag);
}

#[test]
fn test_pillar3_structured_diagnostics_and_ring() {
    let queue = RealtimeDiagnosticQueue::new();

    let raw = RawDiagnosticEvent::new(
        DiagnosticKind::Security,
        DiagnosticSeverity::Critical,
        "untrusted_buffer_overflow",
        "Sample buffer exceeds safety budget",
    );
    assert!(queue.push(raw));

    let events = queue.drain();
    assert_eq!(events.len(), 1);
    let evt = &events[0];
    assert_eq!(evt.category, DiagnosticKind::Security);
    assert_eq!(evt.severity, DiagnosticSeverity::Critical);
    assert_eq!(evt.code, "untrusted_buffer_overflow");
    assert_eq!(evt.message, "Sample buffer exceeds safety budget");

    // Drained queue is now empty
    assert!(queue.drain().is_empty());
}

#[test]
fn test_pillar4_unified_parameter_metadata_system() {
    let reg = ParameterRegistry::with_standard_engine_parameters();
    assert!(reg.len() >= 8);

    // Test volume parameter (Decibel curve)
    let vol_desc = reg.get(&ParameterId::new("master_volume")).unwrap();
    assert_eq!(vol_desc.unit, ParameterUnit::LinearGain);
    assert_eq!(vol_desc.curve, ParameterCurve::Decibel);
    let norm_vol = vol_desc.normalize(1.0);
    assert!((vol_desc.denormalize(norm_vol) - 1.0).abs() < 1e-4);

    // Test EQ frequency parameter (Logarithmic curve)
    let freq_desc = reg.get(&ParameterId::new("eq_band_freq")).unwrap();
    assert_eq!(freq_desc.unit, ParameterUnit::Hertz);
    assert_eq!(freq_desc.curve, ParameterCurve::Logarithmic);
    let norm_1k = freq_desc.normalize(1000.0);
    assert!((freq_desc.denormalize(norm_1k) - 1000.0).abs() < 1.0);

    // Test discrete stepping parameter
    let stepped = ParameterDescriptor::new("steps", "Steps", 0.0, 10.0, 5.0).with_step(2.0);
    assert_eq!(stepped.snap_step(3.1), 4.0);
    assert_eq!(stepped.snap_step(5.0), 6.0);
}

#[test]
fn test_pillar5_versioned_state_and_preset_serialization() {
    let node_state = NodeState {
        node_name: "parametric_equalizer".to_string(),
        enabled: true,
        parameters: BTreeMap::from([
            ("band0_gain".to_string(), 2.5),
            ("band0_freq".to_string(), 440.0),
        ]),
    };

    let env = VersionedEnvelope::new(node_state.clone(), 2);
    assert_eq!(env.schema_version, STATE_SCHEMA_VERSION);
    assert_eq!(env.engine_version, CURRENT_ENGINE_VERSION);
    assert_eq!(env.component_version, 2);

    let json = env.to_json_pretty().unwrap();
    let loaded: VersionedEnvelope<NodeState> = VersionedEnvelope::from_json(&json).unwrap();
    assert_eq!(loaded.state, node_state);

    // Verify rejection of future unsupported schema
    let future_json = r#"{
        "schema_version": 42,
        "engine_version": "99.0.0",
        "component_version": 1,
        "state": {
            "node_name": "future_node",
            "enabled": true,
            "parameters": {}
        }
    }"#;
    let res: Result<VersionedEnvelope<NodeState>, _> = VersionedEnvelope::from_json(future_json);
    assert!(matches!(res, Err(StateMigrationError::UnsupportedSchema { found: 42, .. })));
}

#[test]
fn test_pillar6_seamless_graph_transitions() {
    let mut fader = TransitionCrossfader::new(TransitionConfig {
        duration_ms: 5.0,
        curve: TransitionCurve::EqualPower,
    });
    // 48 kHz, 5 ms = 240 frames
    fader.trigger(48000.0);
    assert!(fader.is_active());

    let old_buf = vec![-0.5f32; 240];
    let new_buf = vec![0.5f32; 240];
    let mut out_buf = vec![0.0f32; 240];

    fader.crossfade_block(&[&old_buf[..]], &[&new_buf[..]], &mut [&mut out_buf[..]]);

    assert!(!fader.is_active());
    // Discontinuity should be smoothly spread over 240 frames
    let max_delta = measure_max_discontinuity(&out_buf);
    assert!(max_delta < 0.05, "maximum delta {max_delta} indicates a click!");

    // Test parameter smoother
    let mut smoother = ContinuousParameterSmoother::new(0.0, 5.0, 48000.0);
    smoother.set_target(100.0);
    for _ in 0..5000 {
        smoother.next_value();
    }
    assert!(smoother.is_settled());
    assert!((smoother.current_value() - 100.0).abs() < 0.05);
}

#[test]
fn test_pillar7_device_output_recovery_and_clock_rescaling() {
    let mut controller = OutputRecoveryController::new(5);

    // 1. Device disappears
    controller.on_device_disappeared();
    assert_eq!(controller.phase(), RecoveryPhase::DeviceDisappeared);

    // 2. Preserve engine state
    let snap = PreservedPlaybackSnapshot {
        source_frames: 96000,
        source_sample_rate: 48000,
        old_output_sample_rate: 48000,
        volume: 0.85,
        output_profile_id: Some("studio_monitors".to_string()),
        was_playing: true,
    };
    controller.preserve_state(snap);
    assert_eq!(controller.phase(), RecoveryPhase::PreserveEngineState);

    // 3. Reopen device
    controller.on_device_reopened();
    assert_eq!(controller.phase(), RecoveryPhase::ReopenDevice);

    // 4. Reconfigure format (e.g. reopened at 96 kHz)
    controller.on_format_reconfigured();
    assert_eq!(controller.phase(), RecoveryPhase::ReconfigureFormat);

    // 5. Restore clock with sample-rate frame count rescaling
    let rescaled_output_frames = rescale_clock_frames(48000, 48000, 96000);
    assert_eq!(rescaled_output_frames, 96000);
    controller.on_clock_restored();
    assert_eq!(controller.phase(), RecoveryPhase::RestoreClock);

    // 6. Restore output profile
    controller.on_profile_restored();
    assert_eq!(controller.phase(), RecoveryPhase::RestoreOutputProfile);

    // 7. Resume playback
    controller.on_playback_resumed();
    assert_eq!(controller.phase(), RecoveryPhase::Idle);
    assert_eq!(controller.attempts(), 0);
}
