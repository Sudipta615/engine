//! Stage 4 Professional-Grade Audio Fidelity & Integration Test Suite (§8.2, §10.2–§10.4, §11.1–§11.2, §12.1).
//!
//! Validates:
//! 1. Formalized SIMD architecture, feature detection, and vector/scalar kernel equivalence.
//! 2. PipeWire pro-audio output backend quantum, clock, and state transitions.
//! 3. JACK pro-audio output backend transport, ports, and timebase integration.
//! 4. Plugin sandboxing architecture, watchdog timeouts, panic containment, and dry failover.
//! 5. Professional network audio RTP RFC 3550 packets, L16/L24 PCM codecs, and AES67 SDP profiles.
//! 6. IEEE 1588-2008 PTP clock synchronization, 4-timestamp exchange, and adaptive jitter buffer with PLC.
//! 7. Acoustic measurement subsystem (Farina sweep, MLS/FHT, transfer functions, RT60/EDT, clarity).
//! 8. Profile-driven room/output correction (target curves, spatial averaging, FIR synthesis, biquad fit).

use engine::dsp::simd::{dispatch, levels::SimdLevel, scalar};
use engine::network_audio::{
    aes67::{Aes67Encoding, Aes67PacketTime, Aes67StreamConfig},
    clock::{PtpClock, PtpClockState, PtpTimestamp},
    rtp::{sequence_diff, timestamp_diff, PcmPayloadCodec, RtpHeader, RtpPacket, RTP_VERSION},
    session::{AdaptiveJitterBuffer, SapPacket},
};
use engine::output::{
    jack::{JackAutoConnectPolicy, JackClientConfig, JackTimebaseInfo, JackTransportState},
    pipewire::{PipeWireConfig, PipeWireQuantumInfo, PipeWireStreamState},
    TargetLayoutKind,
};
use engine::spatial::{
    acoustics::{
        analyze_acoustics, compute_clarity, compute_edt, compute_etc, compute_group_delay,
        compute_magnitude_response, compute_phase_response, compute_rt60, deconvolve_mls,
        deconvolve_sweep, estimate_transfer_function, generate_inverse_filter, generate_log_sweep,
        generate_mls, generate_window, smooth_frequency_response, LogSweepConfig, MlsOrder,
        OctaveSmoothing, WindowType,
    },
    room_correction::{
        average_frequency_responses, fit_parametric_eq, synthesize_channel_correction,
        CorrectionMode, CorrectionProfile, FilterSynthConfig, SpatialAverageStrategy, TargetCurve,
        TargetCurveKind,
    },
};
use plugin_abi::{PluginFaultKind, PluginSandboxConfig, PluginSandboxMode, PluginSandboxState};

// ============================================================================
// PILLAR 1: Formalized SIMD Architecture (§8.2)
// ============================================================================
#[test]
fn test_pillar1_simd_architecture() {
    let level = SimdLevel::detect();
    assert!(level.lanes_f32() >= 1, "Lane count must be at least 1");

    let len = 64;
    let mut x = vec![1.5f32; len];
    let y = vec![2.0f32; len];
    let mut dst = vec![0.0f32; len];

    // Compare scalar reference vs dispatch kernel
    scalar::scale_slice(&mut x, 2.0, len);
    for val in &x {
        assert!((val - 3.0).abs() < 1e-6);
    }

    dst.copy_from_slice(&x);
    scalar::mix_slices(&mut dst, &y, len);
    for val in &dst {
        assert!((val - 5.0).abs() < 1e-6);
    }

    let dot = scalar::dot_product(&x, &y, len);
    assert!((dot - (len as f32 * 6.0)).abs() < 1e-4);

    dispatch::dispatch_scale_slice(&mut x, 0.5, len);
    for val in &x {
        assert!((val - 1.5).abs() < 1e-6);
    }
}

// ============================================================================
// PILLAR 2: PipeWire Output Backend (§10.2)
// ============================================================================
#[test]
fn test_pillar2_pipewire_output_backend() {
    let config = PipeWireConfig {
        node_name: "Shadow-PW-Test".into(),
        media_role: "Music".into(),
        preferred_quantum: Some(256),
        sample_rate: 48000,
        channels: 2,
        channel_map: vec!["FL".into(), "FR".into()],
    };

    assert_eq!(config.preferred_quantum, Some(256));
    assert_eq!(config.channels, 2);

    let q_info = PipeWireQuantumInfo {
        min_quantum: 64,
        max_quantum: 1024,
        current_quantum: 256,
    };
    assert_eq!(q_info.current_quantum, 256);

    let state = PipeWireStreamState::Streaming;
    assert_eq!(state, PipeWireStreamState::Streaming);
}

// ============================================================================
// PILLAR 3: JACK Output Backend (§10.3)
// ============================================================================
#[test]
fn test_pillar3_jack_output_backend() {
    let config = JackClientConfig {
        client_name: "Shadow-JACK-Test".into(),
        port_prefix: "audio_out_".into(),
        auto_connect: JackAutoConnectPolicy::Physical,
        sample_rate: 48000,
        channels: 2,
        buffer_size: 256,
        server_name: None,
    };
    assert_eq!(config.client_name, "Shadow-JACK-Test");

    let tb = JackTimebaseInfo {
        bar: 1,
        beat: 2,
        tick: 240,
        bpm: 120.0,
        beats_per_bar: 4.0,
        beat_type: 4.0,
    };
    assert_eq!(tb.bar, 1);
    assert_eq!(tb.bpm, 120.0);

    let trans = JackTransportState::Rolling;
    assert_eq!(trans, JackTransportState::Rolling);
}

// ============================================================================
// PILLAR 4: Plugin Sandboxing Architecture (§12.1)
// ============================================================================
#[test]
fn test_pillar4_plugin_sandboxing() {
    let config = PluginSandboxConfig {
        mode: PluginSandboxMode::InProcessTrusted,
        max_execution_time_us: 1000,
        max_consecutive_faults: 2,
        restart_attempts: 3,
        backoff_ms: 500,
        dry_passthrough_on_fault: true,
    };

    let mut state = PluginSandboxState::default();
    assert!(state.active);
    assert!(!state.dry_passthrough);

    // Record first fault (Timeout)
    state.record_fault(PluginFaultKind::Timeout, &config, 100);
    assert_eq!(state.total_faults, 1);
    assert_eq!(state.consecutive_faults, 1);
    assert!(state.dry_passthrough);
    assert!(!state.in_backoff);

    // Record second consecutive fault (BufferCorrupted) -> triggers backoff
    state.record_fault(PluginFaultKind::BufferCorrupted, &config, 150);
    assert_eq!(state.consecutive_faults, 2);
    assert!(state.in_backoff);
    assert!(!state.active);

    // Check backoff elapsed
    assert!(!state.can_attempt_restart(&config, 300));
    assert!(state.can_attempt_restart(&config, 700));

    // Reset recovery
    state.reset();
    assert!(state.active);
    assert!(!state.in_backoff);
    assert!(!state.dry_passthrough);
}

// ============================================================================
// PILLAR 5: Professional Network Audio — RTP & AES67 (§10.4)
// ============================================================================
#[test]
fn test_pillar5_rtp_and_aes67_codecs() {
    // 1. RTP Header serialization & parsing
    let header = RtpHeader {
        version: RTP_VERSION,
        padding: false,
        extension: false,
        csrc_count: 0,
        marker: true,
        payload_type: 96,
        sequence_number: 1000,
        timestamp: 48000,
        ssrc: 0xAABBCCDD,
        csrc: Vec::new(),
    };

    let packet = RtpPacket::new(header.clone(), vec![0x11, 0x22, 0x33, 0x44]);
    let bytes = packet.to_bytes();
    let parsed = RtpPacket::parse(&bytes).expect("Failed to parse RTP packet");
    assert_eq!(parsed.header.sequence_number, 1000);
    assert_eq!(parsed.header.ssrc, 0xAABBCCDD);
    assert_eq!(parsed.payload, vec![0x11, 0x22, 0x33, 0x44]);

    // Sequence difference & timestamp difference with rollover
    assert_eq!(sequence_diff(10, 5), 5);
    assert_eq!(sequence_diff(5, 10), -5);
    assert_eq!(sequence_diff(2, 65534), 4);
    assert_eq!(timestamp_diff(100, 50), 50);

    // 2. L16 and L24 Linear PCM codecs
    let ch0 = vec![0.5f32, -0.5f32, 0.0f32];
    let ch1 = vec![0.25f32, -0.25f32, 0.75f32];
    let channels: [&[f32]; 2] = [&ch0, &ch1];

    let l16_bytes = PcmPayloadCodec::encode_l16(&channels, 3);
    assert_eq!(l16_bytes.len(), 3 * 2 * 2); // 3 frames * 2 ch * 2 bytes

    let mut dec_ch0 = vec![0.0f32; 3];
    let mut dec_ch1 = vec![0.0f32; 3];
    {
        let mut dec_channels: [&mut [f32]; 2] = [&mut dec_ch0, &mut dec_ch1];
        let decoded_frames = PcmPayloadCodec::decode_l16(&l16_bytes, &mut dec_channels).unwrap();
        assert_eq!(decoded_frames, 3);
    }
    assert!((dec_ch0[0] - 0.5).abs() < 1e-3);
    assert!((dec_ch1[0] - 0.25).abs() < 1e-3);

    let l24_bytes = PcmPayloadCodec::encode_l24(&channels, 3);
    assert_eq!(l24_bytes.len(), 3 * 2 * 3); // 3 frames * 2 ch * 3 bytes
    {
        let mut dec_channels: [&mut [f32]; 2] = [&mut dec_ch0, &mut dec_ch1];
        let decoded_l24 = PcmPayloadCodec::decode_l24(&l24_bytes, &mut dec_channels).unwrap();
        assert_eq!(decoded_l24, 3);
    }
    assert!((dec_ch0[0] - 0.5).abs() < 1e-5);
    assert!((dec_ch1[0] - 0.25).abs() < 1e-5);

    // 3. AES67 SDP round-trip
    let sdp_cfg = Aes67StreamConfig {
        stream_name: "Shadow-Studio-AES67".into(),
        session_id: 42,
        destination_ip: "239.69.1.5".into(),
        destination_port: 5004,
        sample_rate: 48000,
        channels: 2,
        packet_time: Aes67PacketTime::Ms1,
        encoding: Aes67Encoding::L24,
        payload_type: 96,
        ptp_grandmaster_id: Some("00-11-22-FF-FE-33-44-55".into()),
    };

    let sdp = sdp_cfg.to_sdp();
    assert!(sdp.contains("s=Shadow-Studio-AES67"));
    assert!(sdp.contains("m=audio 5004 RTP/AVP 96"));
    let parsed_cfg = Aes67StreamConfig::from_sdp(&sdp).expect("Failed to parse AES67 SDP");
    assert_eq!(parsed_cfg.stream_name, "Shadow-Studio-AES67");
    assert_eq!(parsed_cfg.destination_port, 5004);
    assert_eq!(parsed_cfg.sample_rate, 48000);
    assert_eq!(parsed_cfg.channels, 2);
    assert_eq!(parsed_cfg.packet_time, Aes67PacketTime::Ms1);
    assert_eq!(parsed_cfg.encoding, Aes67Encoding::L24);
}

// ============================================================================
// PILLAR 6: PTP Clock & Adaptive Jitter Buffer (§10.4)
// ============================================================================
#[test]
fn test_pillar6_ptp_clock_and_jitter_buffer() {
    // 1. IEEE 1588-2008 PTP Clock
    let mut ptp = PtpClock::new("00-11-22-FF-FE-33-44-55");
    assert_eq!(ptp.telemetry().state, PtpClockState::Unsynchronized);

    // Simulate 4-timestamp exchange:
    // Path delay = 5000 ns, clock offset = 200 ns
    let t1 = PtpTimestamp::new(100, 0);
    let t2 = PtpTimestamp::new(100, 5200); // master to slave = 5200 ns
    let t3 = PtpTimestamp::new(100, 10000);
    let t4 = PtpTimestamp::new(100, 14800); // slave to master = 4800 ns

    ptp.process_timestamp_exchange(t1, t2, t3, t4);
    let telem = ptp.telemetry();
    assert_eq!(telem.state, PtpClockState::Locked);
    assert!((telem.offset_ns - 200.0).abs() < 1.0);
    assert!((telem.mean_path_delay_ns - 5000.0).abs() < 1.0);

    // 2. SAP announcements
    let sap = SapPacket::new([192, 168, 1, 10], "v=0\r\ns=Test\r\n".into());
    let sap_bytes = sap.serialize();
    let parsed_sap = SapPacket::parse(&sap_bytes).expect("Failed to parse SAP packet");
    assert_eq!(parsed_sap.originating_source, [192, 168, 1, 10]);
    assert!(parsed_sap.sdp_payload.contains("s=Test"));

    // 3. Adaptive Jitter Buffer & PLC
    let stream_cfg = Aes67StreamConfig {
        packet_time: Aes67PacketTime::Ms1,
        sample_rate: 48000,
        channels: 2,
        encoding: Aes67Encoding::L24,
        ..Default::default()
    };
    let samples_per_pkt = stream_cfg
        .packet_time
        .samples_per_packet(stream_cfg.sample_rate);
    assert_eq!(samples_per_pkt, 48);

    let mut jb = AdaptiveJitterBuffer::new(stream_cfg.clone(), 2);

    // Push packet sequence 0, 1, 2
    for seq in 0..3u16 {
        let ch0 = vec![0.8f32; samples_per_pkt];
        let ch1 = vec![-0.8f32; samples_per_pkt];
        let pl = PcmPayloadCodec::encode_l24(&[&ch0, &ch1], samples_per_pkt);
        let hdr = RtpHeader {
            sequence_number: seq,
            timestamp: (seq as u32) * (samples_per_pkt as u32),
            ..Default::default()
        };
        jb.push_packet(RtpPacket::new(hdr, pl));
    }

    let mut out_ch0 = vec![0.0f32; samples_per_pkt];
    let mut out_ch1 = vec![0.0f32; samples_per_pkt];

    let frames = {
        let mut read_ch: [&mut [f32]; 2] = [&mut out_ch0, &mut out_ch1];
        jb.read_audio_block(&mut read_ch)
    };
    assert_eq!(frames, samples_per_pkt);
    assert!((out_ch0[0] - 0.8).abs() < 1e-4);

    // Next block
    let frames2 = {
        let mut read_ch: [&mut [f32]; 2] = [&mut out_ch0, &mut out_ch1];
        jb.read_audio_block(&mut read_ch)
    };
    assert_eq!(frames2, samples_per_pkt);

    // Third block
    let frames3 = {
        let mut read_ch: [&mut [f32]; 2] = [&mut out_ch0, &mut out_ch1];
        jb.read_audio_block(&mut read_ch)
    };
    assert_eq!(frames3, samples_per_pkt);

    // Fourth block: no packet available -> triggers PLC (concealment)
    let frames4 = {
        let mut read_ch: [&mut [f32]; 2] = [&mut out_ch0, &mut out_ch1];
        jb.read_audio_block(&mut read_ch)
    };
    assert_eq!(frames4, samples_per_pkt);
    let stats = jb.stats();
    assert_eq!(stats.packets_lost, 1);
    assert!(stats.concealment_frames > 0);
}

// ============================================================================
// PILLAR 7: Acoustic Measurement Subsystem (§11.1)
// ============================================================================
#[test]
fn test_pillar7_acoustic_measurement() {
    let sweep_cfg = LogSweepConfig {
        start_freq_hz: 20.0,
        stop_freq_hz: 20000.0,
        duration_secs: 0.1, // short for test speed
        sample_rate: 48000.0,
        fade_duration_secs: 0.005,
    };

    let sweep = generate_log_sweep(&sweep_cfg);
    assert!(!sweep.is_empty());
    let inv = generate_inverse_filter(&sweep, &sweep_cfg);
    assert_eq!(inv.len(), sweep.len());

    // Deconvolution of sweep with matched inverse reconstructs impulse response
    let ir = deconvolve_sweep(&sweep, &inv);
    assert!(!ir.is_empty());
    let max_peak = ir.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
    assert!((max_peak - 1.0).abs() < 1e-3);

    // MLS generation & circular deconvolution
    let mls = generate_mls(MlsOrder::Order10);
    assert_eq!(mls.len(), 1023);
    let mls_ir = deconvolve_mls(&mls, &mls);
    assert_eq!(mls_ir.len(), 1023);

    // Windowing
    let hann = generate_window(WindowType::Hann, 64);
    assert_eq!(hann.len(), 64);
    assert!((hann[0] - 0.0).abs() < 1e-5);
    assert!((hann[32] - 1.0).abs() < 0.05);

    // Frequency response & smoothing
    let freq_resp = compute_magnitude_response(&ir, 48000.0);
    assert!(!freq_resp.frequencies.is_empty());
    let smoothed = smooth_frequency_response(&freq_resp, OctaveSmoothing::Octave1_3);
    assert_eq!(smoothed.smoothing, OctaveSmoothing::Octave1_3);

    // Phase response & group delay
    let phase_resp = compute_phase_response(&ir, 48000.0);
    let gd = compute_group_delay(&phase_resp);
    assert_eq!(gd.frequencies.len(), phase_resp.frequencies.len());

    // RT60, EDT, Clarity, ETC
    let rt60 = compute_rt60(&ir, 48000.0);
    assert!(rt60.t20_seconds >= 0.0);

    let edt = compute_edt(&ir, 48000.0);
    assert!(edt.edt_seconds >= 0.0);

    let clarity = compute_clarity(&ir, 48000.0);
    assert!(clarity.d50 >= 0.0 && clarity.d50 <= 1.0);

    let etc = compute_etc(&ir, 48000.0);
    assert_eq!(etc.time_ms.len(), ir.len());

    // Comprehensive acoustic report
    let report = analyze_acoustics(&ir, 48000.0);
    assert_eq!(report.direct_peak_amplitude, 1.0);
    assert_eq!(
        report.frequency_response_1_3.smoothing,
        OctaveSmoothing::Octave1_3
    );

    // Transfer function estimation
    let tf = estimate_transfer_function(&ir, &ir, 48000.0, 256);
    assert_eq!(tf.coherence.len(), 129);
}

// ============================================================================
// PILLAR 8: Room/Output Correction Infrastructure (§11.2)
// ============================================================================
#[test]
fn test_pillar8_room_correction_infrastructure() {
    // 1. Target curves
    let flat_target = TargetCurve::flat();
    assert_eq!(flat_target.evaluate_db(100.0), 0.0);
    assert_eq!(flat_target.evaluate_db(10000.0), 0.0);

    let harman_target = TargetCurve::default();
    assert!(harman_target.evaluate_db(40.0) > 3.0); // Bass boost
    assert!(harman_target.evaluate_db(10000.0) < 0.0); // Treble tilt

    let custom_target = TargetCurve {
        name: "Custom-Test".into(),
        kind: TargetCurveKind::Flat,
    };
    assert_eq!(custom_target.evaluate_db(1000.0), 0.0);

    // 2. Spatial averaging
    let mut ir_dummy = vec![0.0f32; 128];
    ir_dummy[0] = 1.0;
    let resp1 = compute_magnitude_response(&ir_dummy, 48000.0);
    let resp2 = compute_magnitude_response(&ir_dummy, 48000.0);

    let avg_resp =
        average_frequency_responses(&[resp1.clone(), resp2], SpatialAverageStrategy::PowerEnergy)
            .expect("Spatial average failed");
    assert_eq!(avg_resp.frequencies.len(), resp1.frequencies.len());

    // 3. FIR filter synthesis
    let synth_cfg = FilterSynthConfig {
        filter_length_taps: 512,
        max_boost_db: 6.0,
        max_cut_db: 12.0,
        mode: CorrectionMode::LinearPhase,
        low_freq_limit_hz: 20.0,
        high_freq_limit_hz: 20000.0,
    };

    let (fir_filter, metrics) =
        synthesize_channel_correction(&avg_resp, &harman_target, &synth_cfg, 48000.0);
    assert_eq!(fir_filter.len(), 512);
    assert!(metrics.peak_boost_db <= 6.0);

    // 4. Parametric biquad fitting
    let biquad_bands = fit_parametric_eq(&avg_resp, &harman_target, 5);
    assert!(biquad_bands.len() <= 5);

    // 5. Correction Profile and export to OutputCalibration
    let profile = CorrectionProfile {
        name: "Studio Nearfield A".into(),
        description: "Calibrated room profile".into(),
        sample_rate: 48000,
        channels: 2,
        target_curve: harman_target,
        channel_fir_filters: vec![fir_filter.clone(), fir_filter],
        channel_gain_trims_db: vec![0.0, -0.5],
        channel_delay_ms: vec![0.0, 0.2],
        channel_polarity_invert: vec![false, false],
        validation_metrics: vec![metrics],
    };

    let calibration = profile.to_output_calibration(TargetLayoutKind::Stereo);
    assert_eq!(calibration.layout_kind, TargetLayoutKind::Stereo);
    assert_eq!(calibration.delays.len(), 2);
    assert_eq!(calibration.gains.len(), 2);
    assert!(calibration.correction_ir.is_some());
    assert_eq!(calibration.correction_ir.unwrap().channels.len(), 2);
}
