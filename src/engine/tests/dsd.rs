//! DSD playback, DoP packing and Native DSD routing tests.

use config::EngineConfig;
use std::sync::Arc;

use super::helpers::*;
use crate::{
    buffer::EngineCommand,
    decode::Decoder,
    engine::{AudioEngine, PlaybackStream},
};

/// C1 acceptance: known DSD vectors must produce **identical** DoP packing.
#[test]
fn test_dop_decode_matches_reference_packer() {
    use crate::decode::dsd::DopPacker;

    let path = write_test_dsf();
    let mut decoder = Decoder::open(&path).expect("open DSF");
    assert!(decoder.is_dsd());
    decoder.set_dop_mode(true);
    assert_eq!(decoder.dop_rate(), Some(176_400), "DSD64 DoP rate");

    // Independent reference packer over the same source bytes: rebuild the
    // 16-bit words exactly as `decode_next_dop` does (LSB-first payload).
    let mut reference = DopPacker::new();

    let mut frame_index = 0usize;
    let mut marker_seen = [false; 256];
    let mut substitution_seen = false;
    loop {
        match decoder.decode_next(1 << 16) {
            Ok(chunk) => {
                assert_eq!(chunk.samples.len() % 2, 0);
                assert_eq!(chunk.sample_rate, 176_400);
                for pair in chunk.samples.as_chunks::<2>().0 {
                    let (l, r) = (pair[0], pair[1]);
                    // Recover the left-aligned 24-bit word from the
                    // normalized f32 (the exact round trip the output
                    // callback performs: f32 * 2^31).
                    let li = (l as f64 * 2_147_483_648.0) as i64;
                    let ri = (r as f64 * 2_147_483_648.0) as i64;
                    let marker_l = ((li >> 24) & 0xFF) as u8;
                    let marker_r = ((ri >> 24) & 0xFF) as u8;
                    let payload_l = ((li >> 8) & 0xFFFF) as u16;
                    let payload_r = ((ri >> 8) & 0xFFFF) as u16;

                    // Markers must be the alternating base 0x05/0xFA, or the
                    // deterministic per-channel substitution when a payload
                    // byte mimics the marker (DoP v1.1).
                    let base = if frame_index.is_multiple_of(2) {
                        0x05u8
                    } else {
                        0xFAu8
                    };
                    let expected = |payload: u16| -> u8 {
                        if (payload >> 8) == 0x05 {
                            0x06
                        } else if (payload >> 8) == 0xFA {
                            0xFB
                        } else {
                            base
                        }
                    };
                    assert_eq!(
                        marker_l,
                        expected(payload_l),
                        "frame {frame_index} L marker"
                    );
                    assert_eq!(
                        marker_r,
                        expected(payload_r),
                        "frame {frame_index} R marker"
                    );
                    marker_seen[marker_l as usize] = true;
                    marker_seen[marker_r as usize] = true;
                    if matches!(marker_l, 0x06 | 0xFB) || matches!(marker_r, 0x06 | 0xFB) {
                        substitution_seen = true;
                    }

                    // The payload must be the source words bit-exactly.
                    let b0l = ((frame_index * 2) % 256) as u8;
                    let b1l = ((frame_index * 2 + 1) % 256) as u8;
                    let b0r = ((frame_index * 2 * 7 + 3) % 256) as u8;
                    let b1r = ((frame_index * 2 * 7 + 3 + 7) % 256) as u8;
                    let expect_l = b0l as u16 | ((b1l as u16) << 8);
                    let expect_r = b0r as u16 | ((b1r as u16) << 8);
                    assert_eq!(payload_l, expect_l, "frame {frame_index} left payload");
                    assert_eq!(payload_r, expect_r, "frame {frame_index} right payload");

                    // Bitwise-identical to the independent reference packer.
                    let (ref_l, ref_r) = reference.pack_stereo_frame_f32(expect_l, expect_r);
                    assert_eq!(l.to_bits(), ref_l.to_bits(), "frame {frame_index} left");
                    assert_eq!(r.to_bits(), ref_r.to_bits(), "frame {frame_index} right");

                    frame_index += 1;
                }
            }
            Err(crate::decode::DecodeError::EndOfStream) => break,
            Err(e) => panic!("decode error: {e}"),
        }
    }

    assert!(frame_index > 0, "decoded at least one DoP frame");
    assert!(marker_seen[0x05] && marker_seen[0xFA], "marker alternation");
    assert!(substitution_seen, "payload must have triggered 0x06/0xFB");
    let _ = std::fs::remove_file(&path);
}

/// C1 acceptance: track transitions across DSD/PCM boundaries must not
/// corrupt decoder mode, output rate, or the playback clock. Loading a PCM
/// track after a DSD track (and back) resets the DSD mode and rate cleanly
/// and both play to end.
#[test]
fn test_dsd_pcm_dsd_track_transition_sequence() {
    let dsf = write_test_dsf();
    let wav = write_test_wav_at(44_100, "dspcm");
    let mut config = EngineConfig::default();
    config.dsd_output = config::DsdOutput::DoP; // exercises the DoP negotiation path
    let mut engine = AudioEngine::new(config).unwrap();

    // 1. DSD track (no output device → explicit DoP→PCM fallback).
    let info = engine.load_track(&dsf).expect("load DSF");
    assert_eq!(info.sample_rate, 88_200, "DSD64 PCM fallback rate");
    assert!(!engine.playback_info().dop_active);

    engine.send_command(EngineCommand::Play);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        engine.tick();
        if engine.stream_ended {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "DSD did not finish");
    }
    let dsd_frames = engine.clock.source_frames;

    // 2. PCM track after DSD: decoder must be a plain Symphonia stream.
    let info = engine.load_track(&wav).expect("load WAV after DSD");
    assert_eq!(info.sample_rate, 44_100);
    assert!(!engine.dsd.dop_active && !engine.dsd.native_dsd_active);
    assert!(
        !engine.pipeline().is_dop_bypass(),
        "PCM must leave DoP bypass"
    );

    engine.send_command(EngineCommand::Play);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        engine.tick();
        if engine.stream_ended {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "WAV did not finish");
    }
    assert!(
        engine.clock.source_frames > dsd_frames,
        "clock advanced across transition"
    );

    // 3. DSD track again after PCM: the transport report must show the
    // requested DoP transport downgraded explicitly to PCM conversion
    // (no DSD-capable exclusive output exists in this test environment).
    let _info = engine.load_track(&dsf).expect("reload DSF");
    assert_eq!(_info.sample_rate, 88_200);
    let report = &engine.dsd.dsd_transport_report;
    assert_eq!(report.requested, crate::decode::DsdTransport::Dop);
    assert_eq!(report.actual, crate::decode::DsdTransport::PcmConversion);
    assert!(
        report
            .fallback_steps
            .iter()
            .any(|s| s.contains("fallback: DSD→PCM conversion")),
        "DoP→PCM downgrade must be recorded: {:?}",
        report.fallback_steps
    );

    let _ = std::fs::remove_file(&dsf);
    let _ = std::fs::remove_file(&wav);
}

#[test]
fn test_seek_during_transition_cancels_outgoing_at_every_boundary() {
    for (label, transition_remaining) in [
        ("early", 400usize),
        ("mid", 200usize),
        ("completed", 0usize),
    ] {
        let first = write_test_dsf();
        let second = write_test_dsf();
        let mut config = EngineConfig::default();
        config.crossfade.enabled = true;
        config.crossfade.duration_ms = 10;
        config.transition_mode = config::TransitionMode::Crossfade;
        let mut engine = AudioEngine::new(config).unwrap();
        let first_info = engine.load_track(&first).expect("load outgoing track");
        engine
            .prepare_next_track(&second)
            .expect("prepare incoming track");

        let total_source_frames = (first_info.duration_secs * first_info.sample_rate as f32)
            .round()
            .max(1.0) as u64;
        let near_end = total_source_frames.saturating_sub(1);
        engine.clock.set_source_frames(near_end);
        engine.send_command(EngineCommand::Play);
        engine.tick();
        assert!(
            matches!(engine.stream, Some(PlaybackStream::Transitioning { .. })),
            "{label}: expected dual-decoder state before seek"
        );

        if let Some(PlaybackStream::Transitioning {
            crossfade_frames_remaining,
            ..
        }) = engine.stream.as_mut()
        {
            *crossfade_frames_remaining = transition_remaining;
        }

        let seek_position = 0.02f32;
        engine.send_command(EngineCommand::Seek(seek_position));
        engine.tick();

        assert!(
            matches!(engine.stream, Some(PlaybackStream::Single { .. })),
            "{label}: seek must promote the incoming decoder to Single"
        );
        assert!(!engine.pipeline().mixer().is_crossfading());
        assert_eq!(
            engine.loudness_scan.current_track_path.as_deref(),
            Some(second.as_path())
        );
        assert_eq!(engine.clock.source_sample_rate, first_info.sample_rate);
        assert!(
            (engine.playback_info().position_secs - seek_position).abs() < 0.01,
            "{label}: seek position was not published"
        );

        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_file(second);
    }
}

/// H6: a crossfade whose *outgoing* track is DSD (decimated to PCM at the
/// decoder boundary, then resampled to the output rate) must run through the
/// dual-decoder Transitioning state and land on the incoming PCM track with
/// every emitted sample finite and the clock advancing past the DSD track.
#[test]
fn test_dsd_to_pcm_crossfade_transition() {
    let dsf = write_test_dsf();
    let wav = write_test_wav_at(44_100, "dsd-out");
    let mut config = EngineConfig::default();
    config.dsd_output = config::DsdOutput::DoP; // exercises the negotiation path
    config.crossfade.enabled = true;
    config.crossfade.duration_ms = 20;
    config.transition_mode = config::TransitionMode::Crossfade;
    let mut engine = AudioEngine::new(config).unwrap();

    let first_info = engine.load_track(&dsf).expect("load DSF");
    assert_eq!(
        first_info.sample_rate, 88_200,
        "DSD64 decimates to 88.2 kHz PCM"
    );
    let total_source_frames = (first_info.duration_secs * first_info.sample_rate as f32)
        .round()
        .max(1.0) as u64;

    engine
        .prepare_next_track(&wav)
        .expect("prepare incoming PCM");
    engine
        .clock
        .set_source_frames(total_source_frames.saturating_sub(1));
    engine.send_command(EngineCommand::Play);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        engine.tick();
        if matches!(engine.stream, Some(PlaybackStream::Transitioning { .. })) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "DSD→PCM crossfade did not trigger"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let mut drained_frames = 0u64;
    let mut drained = [0.0f32; 8192];
    loop {
        engine.tick();
        loop {
            let n = engine.output_buffer.pop_block_interleaved(&mut drained);
            if n == 0 {
                break;
            }
            for &s in &drained[..n] {
                assert!(s.is_finite(), "non-finite sample in DSD→PCM crossfade");
            }
            drained_frames += (n / 2) as u64;
        }
        if engine.stream_ended {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "DSD→PCM transition did not complete"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    assert!(
        engine.clock.source_frames > total_source_frames,
        "clock must advance into the incoming PCM track"
    );
    assert!(drained_frames > 0, "crossfade must emit output");
    let _ = std::fs::remove_file(&dsf);
    let _ = std::fs::remove_file(&wav);
}

/// H6: the mirror case — a PCM track crossfading into a DSD track.
#[test]
fn test_pcm_to_dsd_crossfade_transition() {
    let wav = write_test_wav_at(44_100, "pcm-out");
    let dsf = write_test_dsf();
    let mut config = EngineConfig::default();
    config.dsd_output = config::DsdOutput::PcmConvert;
    config.crossfade.enabled = true;
    config.crossfade.duration_ms = 20;
    config.transition_mode = config::TransitionMode::Crossfade;
    let mut engine = AudioEngine::new(config).unwrap();

    let first_info = engine.load_track(&wav).expect("load WAV");
    let total_source_frames = (first_info.duration_secs * first_info.sample_rate as f32)
        .round()
        .max(1.0) as u64;

    engine
        .prepare_next_track(&dsf)
        .expect("prepare incoming DSD");
    engine
        .clock
        .set_source_frames(total_source_frames.saturating_sub(1));
    engine.send_command(EngineCommand::Play);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        engine.tick();
        if matches!(engine.stream, Some(PlaybackStream::Transitioning { .. })) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "PCM→DSD crossfade did not trigger"
        );
    }

    let mut drained = [0.0f32; 8192];
    loop {
        engine.tick();
        loop {
            let n = engine.output_buffer.pop_block_interleaved(&mut drained);
            if n == 0 {
                break;
            }
            for &s in &drained[..n] {
                assert!(s.is_finite(), "non-finite sample in PCM→DSD crossfade");
            }
        }
        if engine.stream_ended {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "PCM→DSD transition did not complete"
        );
    }

    assert!(
        engine.clock.source_frames > 0 && engine.clock.source_frames < total_source_frames,
        "clock must switch to the incoming DSD domain and advance: clock={} outgoing_total={}",
        engine.clock.source_frames,
        total_source_frames
    );
    let _ = std::fs::remove_file(&wav);
    let _ = std::fs::remove_file(&dsf);
}

#[test]
fn test_engine_loads_dsf_and_plays_to_end() {
    let path = write_test_dsf();
    let mut engine = AudioEngine::new_default().unwrap();

    let info = engine.load_track(&path).expect("load DSF");
    assert_eq!(info.sample_rate, 88_200, "DSD64 must decimate to 88.2 kHz");
    assert_eq!(info.channels, 2);
    assert!(info.duration_secs > 0.0);

    engine.send_command(EngineCommand::Play);
    // Tight loop (no sleep): the wall-clock deadline would otherwise be
    // hostage to parallel test load on shared CI runners. 0.37 s of audio
    // finishes in well under a second even when CPU-starved.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        engine.tick();
        if engine.stream_ended {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "DSF playback did not reach end of stream"
        );
    }

    assert_eq!(
        engine.clock.source_frames, 32_768,
        "all DSD frames must decode to PCM and advance the clock"
    );
    let pos = engine.playback_info().position_secs;
    assert!(
        (pos - 32_768.0 / 88_200.0).abs() < 0.01,
        "playhead {pos:.4}s should match the decoded duration"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dop_exclusive_reason_suggests_backend() {
    use crate::output::OutputInfo;

    let info = OutputInfo {
        requested_backend: Some(config::AudioBackend::Auto),
        actual_backend: Some(config::AudioBackend::Auto),
        requested_rate: 44_100,
        actual_rate: 44_100,
        channels: 2,
        buffer_size_frames: 0,
        buffer_size_estimated: true,
        sample_format: crate::dsp::pipeline::OutputSampleFormat::Unknown,
        dither_enabled: false,
        access_mode: crate::output::OutputAccessMode::Shared,
        access_state: crate::output::capabilities::OutputAccessState::default(),
        is_fallback: false,
        fallback_reason: None,
        is_exclusive: false,
        device_name: "default".into(),
    };
    let reason = crate::engine::dop_exclusive_reason(&info, config::AudioBackend::Auto);
    assert!(
        reason.contains("Auto"),
        "should name the shared backend: {reason}"
    );
    assert!(
        reason.contains("Exclusive"),
        "must suggest an exclusive backend by name: {reason}"
    );
    assert!(
        reason.contains("switch to an exclusive backend"),
        "{reason}"
    );

    let info = OutputInfo::shared(
        "default".into(),
        44_100,
        44_100,
        2,
        Some(config::AudioBackend::ExclusiveAlsa),
        Some(config::AudioBackend::Auto),
        "cannot open hw:0",
    );
    let reason = crate::engine::dop_exclusive_reason(&info, config::AudioBackend::ExclusiveAlsa);
    assert!(reason.contains("fell back"), "{reason}");
    assert!(reason.contains("cannot open hw:0"), "{reason}");

    let info = OutputInfo {
        requested_backend: Some(config::AudioBackend::Custom),
        actual_backend: Some(config::AudioBackend::Custom),
        requested_rate: 44_100,
        actual_rate: 44_100,
        channels: 2,
        buffer_size_frames: 0,
        buffer_size_estimated: true,
        sample_format: crate::dsp::pipeline::OutputSampleFormat::Unknown,
        dither_enabled: false,
        access_mode: crate::output::OutputAccessMode::Shared,
        access_state: crate::output::capabilities::OutputAccessState::default(),
        is_fallback: false,
        fallback_reason: None,
        is_exclusive: false,
        device_name: "pipewire".into(),
    };
    let reason = crate::engine::dop_exclusive_reason(&info, config::AudioBackend::Custom);
    assert!(
        reason.contains("did not provide exclusive access"),
        "{reason}"
    );
}

#[test]
fn test_native_dsd_config_falls_back_explicitly_without_dsd_output() {
    let path = write_test_dsf();
    let mut config = EngineConfig::default();
    config.dsd_output = config::DsdOutput::NativeDsd;
    let mut engine = AudioEngine::new(config).unwrap();

    let info = engine.load_track(&path).expect("load DSF");
    assert_eq!(
        info.sample_rate, 88_200,
        "native DSD must fall back to DSD→PCM decimation without a DSD-capable output"
    );
    assert!(
        !engine.dsd.native_dsd_active,
        "native DSD must not be active"
    );
    assert!(
        !engine.dsd.dop_active,
        "DoP must also fall back (no exclusive I32 output)"
    );
    let report = &engine.dsd.dsd_transport_report;
    assert_eq!(report.requested, crate::decode::DsdTransport::Native);
    assert_eq!(report.actual, crate::decode::DsdTransport::PcmConversion);
    assert!(
        report.fell_back() && !report.fallback_steps.is_empty(),
        "fallback must be recorded explicitly: {}",
        report.summary()
    );
    assert!(
        report
            .fallback_steps
            .iter()
            .any(|s| s.contains("native DSD unavailable")),
        "report must state native DSD unavailability: {:?}",
        report.fallback_steps
    );
    assert!(
        report
            .fallback_steps
            .iter()
            .any(|s| s.contains("fallback: DSD→PCM conversion")),
        "report must record the final DSD→PCM downgrade: {:?}",
        report.fallback_steps
    );

    engine.send_command(EngineCommand::Play);
    // Tight loop (no sleep): see test_engine_loads_dsf_and_plays_to_end.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        engine.tick();
        if engine.stream_ended {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "DSF did not play to end in native-DSD fallback mode"
        );
    }
    assert_eq!(
        engine.clock.source_frames, 32_768,
        "fallback path must still decode every frame"
    );
    assert!(
        !engine.playback_info().native_dsd_active,
        "playback info must reflect the fallback"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dop_config_falls_back_without_suitable_output() {
    let path = write_test_dsf();
    let mut config = EngineConfig::default();
    config.dsd_output = config::DsdOutput::DoP;
    let mut engine = AudioEngine::new(config).unwrap();

    let info = engine.load_track(&path).expect("load DSF");
    assert_eq!(
        info.sample_rate, 88_200,
        "DoP must fall back to decimation without a DoP-capable output"
    );
    assert!(
        !engine.dsd.dop_active,
        "dop_active must be false after fallback"
    );
    assert_eq!(engine.dsd.dop_rate, 0);
    assert!(!engine.pipeline().is_dop_bypass());
    assert!(!engine.playback_info().dop_active);

    engine.send_command(EngineCommand::Play);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        engine.tick();
        if engine.stream_ended {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "DSF did not play to end in DoP-fallback mode"
        );
    }
    assert_eq!(
        engine.clock.source_frames, 32_768,
        "fallback path must still decode every frame"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dop_config_does_not_affect_pcm_tracks() {
    let path = write_test_sine_wav();
    let mut config = EngineConfig::default();
    config.dsd_output = config::DsdOutput::DoP;
    let mut engine = AudioEngine::new(config).unwrap();

    let info = engine.load_track(&path).expect("load WAV");
    assert_eq!(info.sample_rate, 48_000);
    assert!(!engine.dsd.dop_active);
    assert!(!engine.pipeline().is_dop_bypass());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_native_dsd_chunk_route_bypasses_pipeline_and_preserves_bytes() {
    use crate::buffer::DsdByteBuffer;
    use crate::decode::dsd::DsdWireFormat;

    let path = write_test_dsf();
    let mut decoder = Decoder::open(&path).expect("open DSF");
    decoder.set_native_dsd_mode(true);
    assert!(decoder.is_native_dsd());
    assert_eq!(decoder.dsd_bit_rate(), Some(2_822_400));

    let wire = DsdWireFormat::U8;
    let buffer = Arc::new(DsdByteBuffer::new(1 << 20));
    let mut engine = AudioEngine::new(EngineConfig::default()).unwrap();
    engine.dsd.native_dsd_active = true;
    engine.dsd.dsd_wire_format = Some(wire);
    engine.dsd.dsd_byte_buffer = Some(buffer.clone());

    let mut expected: Vec<u8> = Vec::new();
    loop {
        match decoder.decode_next(1 << 16) {
            Ok(chunk) => {
                let raw = chunk
                    .raw_dsd
                    .as_ref()
                    .expect("native chunk carries raw payload");
                assert!(chunk.samples.is_empty(), "no f32 samples in native mode");
                let words = raw
                    .channel_bytes
                    .iter()
                    .map(Vec::len)
                    .min()
                    .expect("at least one channel");
                for w in 0..words {
                    for ch in &raw.channel_bytes {
                        expected.push(ch[w]);
                    }
                }
                engine.decode_native_dsd_chunk(chunk, 0);
            }
            Err(crate::decode::DecodeError::EndOfStream) => break,
            Err(e) => panic!("decode error: {e}"),
        }
    }
    assert!(!expected.is_empty(), "must have decoded payload bytes");

    let mut got = vec![0u8; expected.len()];
    let mut drained = 0usize;
    while drained < expected.len() {
        let n = buffer.pop_bytes(&mut got[drained..]);
        assert!(n > 0, "ring starved before expected bytes drained");
        drained += n;
    }
    assert_eq!(
        got, expected,
        "native DSD wire bytes must equal the source payload"
    );

    assert!(engine.dsd.dsd_byte_buffer.is_some());
    let _ = std::fs::remove_file(&path);
}
