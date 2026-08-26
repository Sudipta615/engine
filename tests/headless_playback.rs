//! Headless Integration Tests for Independent Audio Engine.
//!
//! Tests embedding and driving the engine purely through `EngineHandle`,
//! `AudioSource`, `EngineCommand`, and `EngineEvent`.

use std::fs::File;
use std::io::Write;

use engine::events::EngineEvent;
use engine::playback_info::PlaybackState;
use engine::source::AudioSource;
use engine::{AudioEngine, EngineConfig};

fn create_test_wav(path: &std::path::Path, sample_rate: u32, duration_secs: f32) {
    let num_samples = (sample_rate as f32 * duration_secs) as usize;
    let num_channels = 2u16;
    let bits_per_sample = 16u16;
    let byte_rate = sample_rate * num_channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = num_channels * (bits_per_sample / 8);
    let data_len = (num_samples * num_channels as usize * 2) as u32;

    let mut file = File::create(path).expect("Failed to create test wav file");
    // RIFF header
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36u32 + data_len).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();
    // fmt chunk
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap(); // Subchunk1Size (16 for PCM)
    file.write_all(&1u16.to_le_bytes()).unwrap(); // AudioFormat (1 for PCM)
    file.write_all(&num_channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&byte_rate.to_le_bytes()).unwrap();
    file.write_all(&block_align.to_le_bytes()).unwrap();
    file.write_all(&bits_per_sample.to_le_bytes()).unwrap();
    // data chunk
    file.write_all(b"data").unwrap();
    file.write_all(&data_len.to_le_bytes()).unwrap();

    // Generate 440 Hz sine wave
    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let sample_val = (t * 440.0 * 2.0 * std::f32::consts::PI).sin();
        let i16_val = (sample_val * 16000.0) as i16;
        // Left
        file.write_all(&i16_val.to_le_bytes()).unwrap();
        // Right
        file.write_all(&i16_val.to_le_bytes()).unwrap();
    }
}

#[test]
fn test_headless_engine_lifecycle_and_events() {
    let temp_dir = std::env::temp_dir();
    let track1 = temp_dir.join("headless_test_1.wav");
    let track2 = temp_dir.join("headless_test_2.wav");

    create_test_wav(&track1, 44100, 1.0);
    create_test_wav(&track2, 48000, 1.5);

    let config = EngineConfig::default();
    let mut engine = AudioEngine::new(config).expect("Failed to create AudioEngine");
    let handle = engine.handle();
    let event_rx = handle.clone_event_receiver();

    // Verify initial telemetry
    assert_eq!(handle.state(), PlaybackState::Stopped);
    assert_eq!(handle.current_source(), None);
    assert!(!handle.is_playing());

    // 1. Open source via handle
    let source1 = AudioSource::File(track1.clone());
    handle.open(source1.clone());

    // Tick the engine
    engine.tick();

    // Verify source is loaded and playing state initiated
    let info = handle.playback_info();
    assert_eq!(info.current_source, Some(source1.clone()));
    assert_eq!(info.sample_rate, 44100);
    assert!((info.duration_secs - 1.0).abs() < 0.1);

    // Drain events
    let mut opened_event = false;
    let mut started_event = false;
    while let Ok(evt) = event_rx.try_recv() {
        match evt {
            EngineEvent::SourceOpened {
                source,
                sample_rate,
                ..
            } => {
                assert_eq!(source, source1);
                assert_eq!(sample_rate, 44100);
                opened_event = true;
            }
            EngineEvent::PlaybackStarted => {
                started_event = true;
            }
            _ => {}
        }
    }
    assert!(opened_event, "Expected SourceOpened event");
    assert!(started_event, "Expected PlaybackStarted event");

    // 2. Pause and Resume via handle
    handle.pause();
    engine.tick();
    assert_eq!(handle.state(), PlaybackState::Paused);

    let mut paused_event = false;
    while let Ok(evt) = event_rx.try_recv() {
        if let EngineEvent::PlaybackPaused = evt {
            paused_event = true;
        }
    }
    assert!(paused_event, "Expected PlaybackPaused event");

    handle.play();
    engine.tick();
    assert_eq!(handle.state(), PlaybackState::Playing);

    // 3. Seek via handle
    handle.seek(0.5);
    engine.tick();
    let mut seek_event = false;
    while let Ok(evt) = event_rx.try_recv() {
        if let EngineEvent::SeekCompleted { position_secs } = evt {
            assert!((position_secs - 0.5).abs() < 0.1);
            seek_event = true;
        }
    }
    assert!(seek_event, "Expected SeekCompleted event");

    // 4. Volume, Speed, DSP adjustments
    handle.set_volume(0.85);
    handle.set_speed(1.5);
    handle.set_eq_enabled(true);
    handle.set_crossfeed_enabled(true);
    engine.tick();

    assert!((handle.volume() - 0.85).abs() < 1e-4);
    assert!((handle.speed() - 1.5).abs() < 1e-4);

    // 5. Open next track via URI
    let uri2 = format!("file://{}", track2.display());
    handle.open_uri(uri2.clone());
    engine.tick();

    let info2 = handle.playback_info();
    assert_eq!(info2.sample_rate, 48000);
    assert!((info2.duration_secs - 1.5).abs() < 0.1);

    // 6. Stop and shutdown
    handle.stop();
    engine.tick();
    assert_eq!(handle.state(), PlaybackState::Stopped);

    let mut stopped_event = false;
    while let Ok(evt) = event_rx.try_recv() {
        if let EngineEvent::PlaybackStopped = evt {
            stopped_event = true;
        }
    }
    assert!(stopped_event, "Expected PlaybackStopped event");

    let _ = std::fs::remove_file(track1);
    let _ = std::fs::remove_file(track2);
}
