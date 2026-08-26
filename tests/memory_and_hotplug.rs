//! Integration tests for In-Memory Audio Decoding and Device Hotplug Monitoring.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use engine::{
    events::{EngineEvent, OutputEvent},
    output::DeviceMonitor,
    AudioBackend, AudioEngine, EngineConfig,
};

/// Helper: generate valid 16-bit stereo PCM WAV file bytes in-memory for testing.
fn generate_pcm_wav_bytes(
    sample_rate: u32,
    channels: u16,
    duration_secs: f32,
    freq_hz: f32,
) -> Vec<u8> {
    let total_frames = (sample_rate as f32 * duration_secs) as usize;
    let block_align = channels * 2;
    let byte_rate = sample_rate * block_align as u32;
    let data_size = (total_frames * block_align as usize) as u32;
    let riff_chunk_size = 36 + data_size;

    let mut buf = Vec::with_capacity((44 + data_size) as usize);

    // RIFF Header
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_chunk_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");

    // fmt subchunk
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // subchunk size (16 for PCM)
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM audio format = 1
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data subchunk
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());

    // Generate sinusoidal sample bytes
    for i in 0..total_frames {
        let t = i as f32 / sample_rate as f32;
        let sample_val = (t * freq_hz * 2.0 * std::f32::consts::PI).sin();
        let sample_i16 = (sample_val * 32000.0) as i16;
        for _ in 0..channels {
            buf.extend_from_slice(&sample_i16.to_le_bytes());
        }
    }

    buf
}

#[test]
fn test_in_memory_source_decoding_and_playback() {
    let config = EngineConfig::default();
    let mut engine = AudioEngine::new(config).expect("Engine initialization should succeed");
    let handle = engine.handle();

    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();

    let worker = std::thread::spawn(move || {
        while engine_running.load(Ordering::Relaxed) {
            engine.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        engine.stop();
    });

    // 1. Generate in-memory WAV stream (48000 Hz, 2 channels, 1.0s, 440 Hz tone)
    let wav_bytes = generate_pcm_wav_bytes(48000, 2, 1.0, 440.0);
    assert!(!wav_bytes.is_empty());

    // 2. Open in-memory source via handle helper
    handle.open_memory(wav_bytes.clone(), Some("wav".to_string()));
    handle.play();

    // 3. Verify event reception and telemetry
    let events = handle.clone_event_receiver();
    let mut source_opened = false;
    let mut playback_started = false;

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(600) {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(20)) {
            match event {
                EngineEvent::SourceOpened {
                    source,
                    sample_rate,
                    channels,
                    duration_secs,
                } => {
                    assert!(source.is_memory());
                    assert_eq!(sample_rate, 48000);
                    assert_eq!(channels, 2);
                    assert!((duration_secs - 1.0).abs() < 0.1);
                    source_opened = true;
                }
                EngineEvent::PlaybackStarted => {
                    playback_started = true;
                }
                _ => {}
            }
        }
    }

    assert!(
        source_opened,
        "Expected EngineEvent::SourceOpened for memory source"
    );
    assert!(playback_started, "Expected EngineEvent::PlaybackStarted");

    let info = handle.playback_info();
    assert!(info.current_source.is_some());
    assert!(info.current_source.as_ref().unwrap().is_memory());
    assert_eq!(info.sample_rate, 48000);

    // 4. Shutdown worker cleanly
    running.store(false, Ordering::Relaxed);
    handle.shutdown();
    let _ = worker.join();
}

#[test]
fn test_device_monitor_enumeration_and_hotplug_polling() {
    let mut monitor = DeviceMonitor::new(AudioBackend::default(), Duration::from_millis(50));
    let initial_devices = monitor.current_devices().to_vec();

    // Verify current devices snapshot
    println!("Initial devices: {:?}", initial_devices);

    // Immediate poll should not trigger if poll_interval hasn't elapsed unless forced
    let delta_forced = monitor.poll(true);
    assert!(delta_forced.is_some());
    let delta = delta_forced.unwrap();
    assert_eq!(delta.current_devices, initial_devices);

    // Verify handle exposes available devices
    let config = EngineConfig::default();
    let engine = AudioEngine::new(config).expect("Engine initialization should succeed");
    let handle = engine.handle();
    let handle_devices = handle.available_devices();
    assert_eq!(handle_devices, initial_devices);
}

#[test]
fn test_output_device_switch_command_and_event() {
    let config = EngineConfig::default();
    let mut engine = AudioEngine::new(config).expect("Engine initialization should succeed");
    let handle = engine.handle();

    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();

    let worker = std::thread::spawn(move || {
        while engine_running.load(Ordering::Relaxed) {
            engine.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        engine.stop();
    });

    let output_events = handle.clone_output_event_receiver();

    // Switch output device to a specific DAC name
    handle.set_output_device(Some("DAC-1".to_string()));

    let mut device_changed_event = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(OutputEvent::OutputDeviceChanged { device }) =
            output_events.recv_timeout(Duration::from_millis(20))
        {
            assert_eq!(device, Some("DAC-1".to_string()));
            device_changed_event = true;
            break;
        }
    }

    assert!(
        device_changed_event,
        "Expected OutputDeviceChanged event on device switch"
    );

    running.store(false, Ordering::Relaxed);
    handle.shutdown();
    let _ = worker.join();
}

/// Verify that device events and playback lifecycle events are strictly
/// separated between the two channels: output-device events must appear on
/// the `OutputEvent` channel and never leak into `EngineEvent`, and
/// vice-versa.
#[test]
fn test_event_channel_isolation_no_leakage() {
    let config = EngineConfig::default();
    let mut engine = AudioEngine::new(config).expect("Engine initialization should succeed");
    let handle = engine.handle();

    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();

    let worker = std::thread::spawn(move || {
        while engine_running.load(Ordering::Relaxed) {
            engine.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        engine.stop();
    });

    let main_events = handle.clone_event_receiver();
    let output_events = handle.clone_output_event_receiver();

    // Drain any startup noise from both channels before the test.
    while main_events.try_recv().is_ok() {}
    while output_events.try_recv().is_ok() {}

    // ── Phase 1: trigger a device switch and verify it only lands on the
    //    output event channel, never on the main engine event channel.
    handle.set_output_device(Some("IsolationTestDAC".to_string()));

    let mut saw_device_change_on_output = false;
    let mut saw_device_change_on_main = false;

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        // Drain main events — any device-related event here is a leak.
        while let Ok(event) = main_events.try_recv() {
            match event {
                EngineEvent::Error(_) => {}
                _ => {
                    // The EngineEvent enum no longer carries device variants;
                    // this branch catches any future refactor that re-adds them.
                    saw_device_change_on_main = true;
                }
            }
        }

        // Drain output events — expect OutputDeviceChanged.
        while let Ok(event) = output_events.try_recv() {
            if let OutputEvent::OutputDeviceChanged { ref device } = event {
                assert_eq!(
                    *device,
                    Some("IsolationTestDAC".to_string()),
                    "device name must match the switch request"
                );
                saw_device_change_on_output = true;
            }
        }

        if saw_device_change_on_output {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        saw_device_change_on_output,
        "OutputDeviceChanged must arrive on the output event channel"
    );
    assert!(
        !saw_device_change_on_main,
        "no device-related event must leak into the main EngineEvent channel"
    );

    // ── Phase 2: trigger a playback lifecycle event (open → play) and verify
    //    it only lands on the main event channel, never on the output channel.
    let wav_bytes = generate_pcm_wav_bytes(48000, 2, 0.5, 440.0);
    let tmp = std::env::temp_dir().join("isolation_test_sine.wav");
    std::fs::write(&tmp, &wav_bytes).expect("write temp WAV");
    handle.open_file(tmp.clone());
    handle.play();

    let mut saw_playback_on_main = false;
    let mut saw_playback_on_output = false;

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        while let Ok(event) = main_events.try_recv() {
            match event {
                EngineEvent::PlaybackStarted => saw_playback_on_main = true,
                EngineEvent::Error(_) => {}
                _ => {}
            }
        }

        while let Ok(event) = output_events.try_recv() {
            // Any non-device event here is a leak. OutputEvent only has
            // device variants, so every match means a lifecycle event
            // leaked into the wrong channel.
            let _ = event;
            saw_playback_on_output = true;
        }

        if saw_playback_on_main {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        saw_playback_on_main,
        "PlaybackStarted must arrive on the main EngineEvent channel"
    );
    assert!(
        !saw_playback_on_output,
        "no playback lifecycle event must leak into the OutputEvent channel"
    );

    let _ = std::fs::remove_file(&tmp);
    running.store(false, Ordering::Relaxed);
    handle.shutdown();
    let _ = worker.join();
}
