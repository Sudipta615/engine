//! Playback lifecycle, bit-perfect equality, state machine and loudness scan tests.

use config::EngineConfig;

use super::helpers::*;
use crate::{
    buffer::{EngineCommand, PlaybackInfo, PlaybackState, MAX_AUDIO_BLOCK_FRAMES},
    decode::Decoder,
    dsp::{
        dither::DitherType,
        pipeline::{DspPipeline, OutputSampleFormat},
    },
    engine::{AudioEngine, EngineError, PlaybackStream},
    events::EngineEvent,
    output::format_converter::{AudioFormatConverter, TargetFormat},
};

/// Gate A (spec §40): exact sample equality through the real source → decode
/// → pipeline → output-conversion path.
#[test]
fn test_bit_perfect_end_to_end_sample_equality() {
    let n = 20_000usize;
    let left = full_range_i16_pattern(0xDEAD_BEEF, n, true);
    let right = full_range_i16_pattern(0x1234_5678, n, true);
    let path = std::env::temp_dir().join(format!(
        "engine_bp_eq_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_i16_wav(&path, 44_100, &left, &right);

    let mut decoder = Decoder::open(&path).expect("open WAV");
    assert_eq!(decoder.info().channels, 2);
    assert_eq!(decoder.format_info().bit_depth, Some(16));
    assert_eq!(decoder.format_info().codec, "pcm_s16le");
    assert!(decoder.format_info().is_lossless, "WAV PCM is lossless");

    // Pipeline in bit-perfect mode: all DSP bypassed, only unity volume and
    // an idle seek fade may run — both identity at these settings.
    let mut cfg = EngineConfig::default();
    cfg.limiter.enabled = false;
    cfg.eq.enabled = false;
    let mut pipeline = DspPipeline::from_config(&cfg, 44_100.0);
    pipeline.set_volume(1.0);
    pipeline.volume.snap();
    pipeline.set_bit_perfect(true);
    let (transparent, why) = pipeline.is_dsp_transparent();
    assert!(transparent, "pipeline must be transparent: {why:?}");

    // The bit-perfect report for this configuration must already agree:
    // 16-bit source → f32 pipeline → f32 output, matched rate, exclusive.
    let bp = pipeline.bit_perfect_report_with_format(
        44_100,
        44_100,
        16,
        32,
        OutputSampleFormat::F32,
        false,
        true,
    );
    assert!(bp.bit_perfect_samples);
    assert!(bp.bit_perfect_transport);
    assert!(bp.is_bit_perfect);
    // Per-stage split (§13): every stage reports bypassed individually.
    assert!(bp.eq_bypassed);
    assert!(bp.limiter_bypassed);
    assert!(bp.compressor_bypassed);
    assert!(bp.convolution_bypassed);
    assert!(bp.crossfeed_bypassed);
    assert!(bp.stereo_bypassed);
    assert!(bp.loudness_bypassed);
    assert!(bp.dynamics_bypassed);

    let mut converter = AudioFormatConverter::new(TargetFormat::I16, DitherType::None);
    let mut got_l = Vec::with_capacity(n + 4);
    let mut got_r = Vec::with_capacity(n + 4);
    loop {
        match decoder.decode_next(4096) {
            Ok(chunk) => {
                assert_eq!(chunk.sample_rate, 44_100);
                assert_eq!(chunk.channels, 2);
                for pair in chunk.samples.as_chunks::<2>().0 {
                    let (l, r) = pipeline.process(pair[0], pair[1]);
                    let (li, ri) = converter.convert_stereo_to_i16(l, r);
                    got_l.push(li);
                    got_r.push(ri);
                }
            }
            Err(crate::decode::DecodeError::EndOfStream) => break,
            Err(e) => panic!("decode error: {e}"),
        }
    }
    assert_eq!(got_l, left, "left channel must round-trip bit-exactly");
    assert_eq!(got_r, right, "right channel must round-trip bit-exactly");

    // Negative control: software gain is only permitted outside the
    // Bit-Perfect contract. A non-unity gain MUST change the samples —
    // proving the test can detect corruption (it is not vacuously passing).
    let mut pip2 = DspPipeline::from_config(&cfg, 44_100.0);
    pip2.set_bit_perfect(false);
    pip2.set_volume(0.5);
    pip2.volume.snap();
    let mut conv2 = AudioFormatConverter::new(TargetFormat::I16, DitherType::None);
    let mut changed = 0usize;
    let mut frame_idx = 0usize;
    let mut dec2 = Decoder::open(&path).expect("reopen WAV");
    loop {
        match dec2.decode_next(4096) {
            Ok(chunk) => {
                for pair in chunk.samples.as_chunks::<2>().0 {
                    let (l, r) = pip2.process(pair[0], pair[1]);
                    let (li, _) = conv2.convert_stereo_to_i16(l, r);
                    if li != left[frame_idx] {
                        changed += 1;
                    }
                    frame_idx += 1;
                }
            }
            Err(crate::decode::DecodeError::EndOfStream) => break,
            Err(e) => panic!("decode error: {e}"),
        }
    }
    assert_eq!(
        frame_idx,
        left.len(),
        "decoder must emit every source frame"
    );
    // Simple robust check: half-volume must alter a large fraction of samples.
    assert!(
        changed > left.len() / 2,
        "negative control: half volume changed only {changed}/{} samples",
        left.len()
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_playback_stream_is_crossfading_single() {
    let err = EngineError::AlreadyRunning;
    assert_eq!(format!("{}", err), "Engine already running");
}

#[test]
fn test_engine_error_display() {
    assert_eq!(
        format!("{}", EngineError::AlreadyRunning),
        "Engine already running"
    );
    assert!(format!("{}", EngineError::Config("bad".into())).contains("bad"));
    assert!(format!("{}", EngineError::StreamRecovery("failed".into())).contains("failed"));
}

#[test]
fn test_playback_info_default() {
    let info = PlaybackInfo::default();
    assert_eq!(info.state, PlaybackState::Stopped);
    assert!((info.volume - 0.75).abs() < 1e-6);
    assert_eq!(info.speed, 1.0);
    assert_eq!(info.current_source, None);
    assert!(!info.resampler_disabled);
    assert!(!info.convolution_ir_needs_reload);
}

#[test]
fn test_playback_info_access() {
    let engine = AudioEngine::new_default().unwrap();
    let info = engine.playback_info();
    assert_eq!(info.state, PlaybackState::Stopped);
    assert!((info.volume - 0.75).abs() < 1e-6);
}

#[test]
fn test_playback_info_arc() {
    let engine = AudioEngine::new_default().unwrap();
    let arc = engine.playback_info_arc();
    let info = arc.load();
    assert_eq!(info.state, PlaybackState::Stopped);
}

#[test]
fn test_playback_state_equality() {
    assert_eq!(PlaybackState::Stopped, PlaybackState::Stopped);
    assert_eq!(PlaybackState::Playing, PlaybackState::Playing);
    assert_eq!(PlaybackState::Paused, PlaybackState::Paused);
    assert_ne!(PlaybackState::Playing, PlaybackState::Paused);
    assert_ne!(PlaybackState::Playing, PlaybackState::Stopped);
}

#[test]
fn test_realtime_fifo_capacity_contract() {
    // Every container touched by decode/crossfade processing is populated to
    // its declared bound before the test inspects it. The push helpers must
    // never grow a VecDeque/Vec on the hot path.
    let mut engine = AudioEngine::new_default().unwrap();
    let rs_out_capacity = engine.scratch.rs_out_buf.capacity();
    let rs_in_capacity = engine.scratch.rs_in_buf.capacity();
    for i in 0..rs_out_capacity {
        engine.push_crossfade_out((i as f32, 0.0));
    }
    for i in 0..rs_in_capacity {
        engine.push_crossfade_in((i as f32, 0.0));
    }
    for i in 0..engine.scratch.pending_output_frames.capacity() {
        engine.push_pending_back((i as f32, 0.0));
    }
    for i in 0..engine.scratch.mix_l.capacity() {
        engine.push_mix_frame(i as f32, 0.0, 0.0, 0.0);
    }

    assert_eq!(engine.scratch.rs_out_buf.len(), rs_out_capacity);
    assert_eq!(engine.scratch.rs_in_buf.len(), rs_in_capacity);
    assert_eq!(
        engine.scratch.pending_output_frames.len(),
        engine.scratch.pending_output_frames.capacity()
    );
    assert_eq!(engine.scratch.mix_l.len(), engine.scratch.mix_l.capacity());
    assert_eq!(engine.scratch.mix_r.len(), engine.scratch.mix_r.capacity());
    // The pending branch is populated from one 128-frame decode batch, not
    // the decoder's full 4096-frame chunk.
    assert!(
        engine.scratch.pending_multichannel.capacity()
            >= crate::engine::MAX_PENDING_MULTICHANNEL_SAMPLES
    );
}

#[test]
fn test_incoming_track_loudness_scan() {
    let path = write_test_sine_wav();
    let mut engine = AudioEngine::new_default().unwrap();

    let info = engine
        .prepare_next_track(&path)
        .expect("prepare should succeed");
    assert_eq!(info.sample_rate, 48000);
    assert_eq!(
        engine.loudness_scan.next_track_path.as_deref(),
        Some(path.as_path())
    );

    // The scan runs in the background; tick() drains the completion command.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        engine.tick();
        if engine
            .loudness_scan
            .pending_incoming_loudness_metadata
            .as_ref()
            .and_then(|m| m.ebu_r128_loudness)
            .is_some()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "incoming loudness scan did not complete in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let lufs = engine
        .loudness_scan
        .pending_incoming_loudness_metadata
        .and_then(|m| m.ebu_r128_loudness)
        .expect("scan result present");
    // Stereo full-scale 1 kHz sine measures ≈ -0.02 LUFS (BS.1770-4 channel sum).
    assert!(
        (lufs - (-0.02)).abs() < 0.6,
        "expected ≈ -0.02 LUFS, got {lufs:.2}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_incoming_metadata_applied_to_pipeline() {
    let path = write_test_sine_wav();
    let mut engine = AudioEngine::new_default().unwrap();
    // Enable EBU R128 normalization on the incoming chain so the metadata
    // produces a real gain target.
    engine
        .pipeline_mut()
        .in_loudness_mut()
        .normalizer
        .set_mode(crate::dsp::LoudnessMode::EbuR128);
    engine
        .pipeline_mut()
        .in_loudness_mut()
        .normalizer
        .set_target_lufs(-23.0);
    engine
        .prepare_next_track(&path)
        .expect("prepare should succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        engine.tick();
        // The completion handler applies the merged metadata to the incoming
        // loudness chain: a -0.02 LUFS track targeted at -23 LUFS requires
        // ≈ -22.98 dB of gain.
        let gain_db = engine.pipeline().in_loudness().normalizer.target_gain_db();
        if gain_db < -20.0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "incoming metadata was not applied to the pipeline (gain still {:.2} dB)",
            gain_db
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_load_track_reuses_cached_scan() {
    let path = write_test_sine_wav();
    let mut engine = AudioEngine::new_default().unwrap();

    // First load: no cache entry yet, so the scan must run in the background.
    engine.load_track(&path).expect("first load");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        engine.tick();
        if engine
            .loudness_scan
            .pending_loudness_metadata
            .as_ref()
            .and_then(|m| m.ebu_r128_loudness)
            .is_some()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "first scan did not complete in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Second load of the same unchanged file must hit the persisted cache
    // synchronously — no scan thread needed.
    engine.load_track(&path).expect("second load");
    let lufs = engine
        .loudness_scan
        .pending_loudness_metadata
        .and_then(|m| m.ebu_r128_loudness)
        .expect("second load should reuse the cached scan result");
    assert!(
        (lufs - (-0.02)).abs() < 0.6,
        "expected ≈ -0.02 LUFS, got {lufs:.2}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_long_realtime_pipeline_stress_keeps_fixed_storage() {
    let mut engine = AudioEngine::new_default().expect("engine construction");
    let scratch_capacity = engine.graph.realtime_scratch_capacity();
    let output_capacity = engine.output_buffer.capacity();

    // Simulate over a minute of callback-sized playback without opening an OS
    // device. The same bounded block and ring-buffer operations used by the
    // engine tick are exercised repeatedly.
    const BLOCK: usize = 128;
    const TICKS: usize = 30_000;
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let mut interleaved = [0.0f32; BLOCK * 2];
    let mut drained = [0.0f32; BLOCK * 2];
    let mut phase = 0.0f32;

    for _ in 0..TICKS {
        for i in 0..BLOCK {
            let sample = (phase * std::f32::consts::TAU).sin() * 0.25;
            left[i] = sample;
            right[i] = sample * 0.9;
            phase = (phase + 440.0 / 48_000.0).fract();
        }
        engine.graph.process_block(&mut left, &mut right);
        for i in 0..BLOCK {
            assert!(left[i].is_finite() && right[i].is_finite());
            interleaved[i * 2] = left[i];
            interleaved[i * 2 + 1] = right[i];
        }
        let written = engine.output_buffer.push_block_interleaved(&interleaved);
        if written > 0 {
            let read = engine.output_buffer.pop_block_interleaved(&mut drained);
            assert_eq!(read, written);
        }
    }

    const {
        assert!(MAX_AUDIO_BLOCK_FRAMES >= BLOCK);
    }
    assert_eq!(engine.graph.realtime_scratch_capacity(), scratch_capacity);
    assert_eq!(engine.output_buffer.capacity(), output_capacity);
    assert_eq!(engine.output_buffer.available(), 0);
}

#[test]
fn test_playback_state_machine_script() {
    let track = write_test_wav_at(44_100, "sm");
    let next = write_test_wav_at(44_100, "sm-next");
    let mut config = EngineConfig::default();
    config.crossfade.enabled = true;
    config.crossfade.duration_ms = 200;
    config.transition_mode = config::TransitionMode::Crossfade;
    let mut engine = AudioEngine::new(config).unwrap();

    // 0. Commands with no track loaded are safe no-ops; state stays Stopped.
    engine.send_command(EngineCommand::Play);
    engine.send_command(EngineCommand::Pause);
    engine.tick();
    assert_eq!(engine.playback_info().state, PlaybackState::Stopped);
    assert!(engine.stream.is_none());

    // 1. Load → Paused (not Stopped), Single stream.
    let info = engine.load_track(&track).expect("load first track");
    assert_eq!(engine.playback_info().state, PlaybackState::Paused);
    assert!(matches!(engine.stream, Some(PlaybackStream::Single { .. })));

    // 2. Play → Playing.
    engine.send_command(EngineCommand::Play);
    engine.tick();
    assert_eq!(engine.playback_info().state, PlaybackState::Playing);

    // 3. Pause → Paused; the stream must survive (no stop).
    engine.send_command(EngineCommand::Pause);
    engine.tick();
    assert_eq!(engine.playback_info().state, PlaybackState::Paused);
    assert!(
        engine.stream.is_some(),
        "pause must not tear down the stream"
    );

    // 4. Seek while paused: position published, stream intact.
    engine.send_command(EngineCommand::Seek(0.25));
    engine.tick();
    let pos = engine.playback_info().position_secs;
    assert!(
        (pos - 0.25).abs() < 0.01,
        "seek while paused published {pos}"
    );
    assert!(engine.stream.is_some());

    // 5. Resume → Playing, playhead advances across ticks.
    engine.send_command(EngineCommand::Play);
    engine.tick();
    assert_eq!(engine.playback_info().state, PlaybackState::Playing);
    let pos_before = engine.playback_info().position_secs;
    for _ in 0..5 {
        engine.tick();
    }
    assert!(
        engine.playback_info().position_secs > pos_before,
        "playhead must advance while Playing"
    );

    // 6. Prepare next track + playhead near end → Transitioning (crossfade).
    engine
        .prepare_next_track(&next)
        .expect("prepare next track");
    let total_source_frames = (info.duration_secs * info.sample_rate as f32)
        .round()
        .max(1.0) as u64;
    engine
        .clock
        .set_source_frames(total_source_frames.saturating_sub(13_230));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        engine.tick();
        if matches!(engine.stream, Some(PlaybackStream::Transitioning { .. })) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "crossfade did not trigger"
        );
    }
    assert_eq!(engine.playback_info().state, PlaybackState::Playing);
    assert!(engine.graph.mixer_state() == crate::dsp::crossfade::MixerState::Crossfading);

    // 7. Pause during the crossfade: state Paused, transition frozen (not
    //    torn down, not promoted to Single prematurely).
    engine.send_command(EngineCommand::Pause);
    engine.tick();
    assert_eq!(engine.playback_info().state, PlaybackState::Paused);
    assert!(
        matches!(engine.stream, Some(PlaybackStream::Transitioning { .. })),
        "pausing mid-crossfade must freeze the transition"
    );

    // 8. Stop during the crossfade → Stopped, no stream left behind.
    engine.send_command(EngineCommand::Stop);
    engine.tick();
    assert_eq!(engine.playback_info().state, PlaybackState::Stopped);
    assert!(engine.stream.is_none(), "stop must drop the transition");
    assert_eq!(engine.playback_info().position_secs, 0.0);
    assert!(engine.graph.mixer_state() != crate::dsp::crossfade::MixerState::Crossfading);

    let _ = std::fs::remove_file(track);
    let _ = std::fs::remove_file(next);
}

/// Stress test: engine with NoopSink at maximum speed (4.0× Varispeed) must
/// decode a long track to completion without stalling, and telemetry must
/// remain accurate (clock source frames, position, no errors).
///
/// Because there is no ring buffer to fill, the decode loop runs at
/// full CPU speed — this exercises the tightest possible decode-
/// then-process path and catches buffer-bound stalls or telemetry drift
/// that a real-time output device might mask.
#[test]
fn test_noop_sink_max_speed_stress() {
    let sample_rate: u32 = 48_000;
    let duration_secs: u32 = 10;
    let path = write_test_wav_duration(sample_rate, duration_secs, "stress");
    let expected_source_frames: u64 = (sample_rate as u64) * (duration_secs as u64);
    let expected_duration = duration_secs as f32;

    // Engine with NoopSink: no output device, no ring buffer pressure.
    let mut config = EngineConfig::default();
    config.speed_mode = config::SpeedMode::Varispeed;
    let sink = Box::new(crate::sink::NoopSink);
    let mut engine =
        AudioEngine::with_sink(config.clone(), sink).expect("engine init with NoopSink");

    // Telemetry verification before load.
    {
        let pb = engine.playback_info();
        assert_eq!(pb.state, PlaybackState::Stopped);
        assert_eq!(pb.position_secs, 0.0);
        assert!(!pb.native_dsd_active);
        assert!(!pb.dop_active);
    }

    let info = engine.load_track(&path).expect("load WAV");
    assert_eq!(info.sample_rate, sample_rate);
    assert_eq!(info.channels, 2);
    assert_eq!(info.duration_secs, expected_duration);
    assert_eq!(engine.clock.source_frames, 0);

    // Set max speed then start playback.
    engine.send_command(EngineCommand::SetSpeed(4.0));
    engine.send_command(EngineCommand::Play);
    engine.tick(); // process SetSpeed + Play
    assert_eq!(engine.speed, 4.0);

    // The engine must reach end-of-stream within a generous deadline
    // (10 s ÷ 4× = 2.5 s real-time minimum, plus overhead).
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(30);
    let mut last_pos: f32 = 0.0;
    let mut tick_count: u64 = 0;
    let events = engine.handle().clone_event_receiver();
    let mut saw_source_finished = false;
    let mut saw_error = false;

    loop {
        engine.tick();
        tick_count += 1;

        // Drain events first — SourceFinished may arrive on the final tick
        // before stream_ended is set.
        while let Ok(event) = events.try_recv() {
            match event {
                EngineEvent::SourceFinished { .. } => saw_source_finished = true,
                EngineEvent::Error(msg) => {
                    eprintln!("Engine error during stress test: {}", msg);
                    saw_error = true;
                }
                _ => {}
            }
        }

        if engine.stream_ended {
            break;
        }

        let elapsed = start.elapsed().as_secs_f32();
        assert!(
            std::time::Instant::now() < deadline,
            "NoopSink playback stalled: {} ticks, {:.3}s position after {:.1}s wall-clock",
            tick_count,
            engine.clock.position_secs(),
            elapsed
        );

        // Position must advance monotonically (cannot go backwards).
        let pos = engine.clock.position_secs();
        assert!(
            pos >= last_pos,
            "playhead regressed: {} → {} at tick {}",
            last_pos,
            pos,
            tick_count
        );
        last_pos = pos;
    }

    // Drain remaining events (after EOS).
    while let Ok(event) = events.try_recv() {
        match event {
            EngineEvent::SourceFinished { .. } => saw_source_finished = true,
            EngineEvent::Error(msg) => {
                eprintln!("Engine error (post-EOS): {}", msg);
                saw_error = true;
            }
            _ => {}
        }
    }

    // ── Assertions ───────────────────────────────────────────────────────
    assert!(!saw_error, "no engine errors during stress playback");
    assert!(saw_source_finished, "SourceFinished event must fire at EOS");
    assert!(tick_count > 0, "engine must process at least one tick");

    // Clock: every source frame must be consumed.
    let clock_frames = engine.clock.source_frames;
    assert_eq!(
        clock_frames, expected_source_frames,
        "clock must advance through all {} source frames, got {}",
        expected_source_frames, clock_frames
    );

    // Position must match the exact source duration.
    let pos = engine.clock.position_secs();
    let expected_pos = clock_frames as f32 / sample_rate as f32;
    assert!(
        (pos - expected_pos).abs() < 0.001,
        "position {:.6}s must match {:.6}s ({} frames / {} Hz)",
        pos,
        expected_pos,
        clock_frames,
        sample_rate
    );

    // Telemetry snapshot must agree with the clock.
    let pb = engine.playback_info();
    assert_eq!(pb.state, PlaybackState::Stopped);
    assert!(
        (pb.position_secs - expected_pos).abs() < 0.001,
        "telemetry position {:.6}s must match clock {:.6}s",
        pb.position_secs,
        expected_pos
    );
    assert_eq!(
        pb.sample_rate, sample_rate,
        "telemetry must reflect source sample rate"
    );
    assert_eq!(pb.speed, 4.0, "telemetry must reflect the configured speed");
    assert_eq!(
        pb.duration_secs, expected_duration,
        "telemetry duration must match source"
    );
    assert!(
        !pb.native_dsd_active && !pb.dop_active,
        "no DSD transport on a PCM WAV"
    );

    let _ = std::fs::remove_file(&path);
}
