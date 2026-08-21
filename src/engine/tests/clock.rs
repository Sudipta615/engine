//! AudioClock and latency compensation tests.

use config::EngineConfig;

use crate::{
    buffer::EngineCommand,
    engine::{AudioClock, AudioEngine},
};
use super::helpers::*;

#[test]
fn test_clock_position_secs() {
    let mut clock = AudioClock::new(44100);
    assert_eq!(clock.position_secs(), 0.0);
    clock.advance_source(44100);
    assert!((clock.position_secs() - 1.0).abs() < 1e-4);
    clock.advance_source(44100);
    assert!((clock.position_secs() - 2.0).abs() < 1e-4);
}

#[test]
fn test_clock_position_is_exact_no_accumulation_drift() {
    // A long session (3 hours at 96 kHz) computed from integer frames must
    // stay exact — there is no per-tick float accumulation.
    let mut clock = AudioClock::new(96000);
    let frames_per_tick = 4096u64;
    for _ in 0..(3 * 3600 * 96000 / frames_per_tick) {
        clock.advance_source(frames_per_tick);
    }
    let expected = 3.0 * 3600.0;
    assert!(
        (clock.position_secs() - expected).abs() < 1e-2,
        "3h at 96kHz: got {}s, expected {}s",
        clock.position_secs(),
        expected
    );
}

#[test]
fn test_clock_reset_track_and_set_source_frames() {
    let mut clock = AudioClock::new(44100);
    clock.advance_source(12345);
    clock.reset_track(96000);
    assert_eq!(clock.source_frames, 0);
    assert_eq!(clock.source_sample_rate, 96000);
    // Seek: set the playhead directly; position reflects the new frame count.
    clock.set_source_frames((90.0f64 * 96000.0).round() as u64);
    assert!((clock.position_secs() - 90.0).abs() < 1e-3);
}

#[test]
fn test_clock_position_is_rate_consistent() {
    // The same number of source frames maps to different durations at
    // different rates — the clock always reports frames / rate.
    let mut a = AudioClock::new(44100);
    let mut b = AudioClock::new(48000);
    a.advance_source(44100);
    b.advance_source(44100);
    assert!((a.position_secs() - 1.0).abs() < 1e-4);
    assert!((b.position_secs() - 0.91875).abs() < 1e-4);
}

/// H5: playback position is latency-compensated — `position_secs_compensated`
/// must equal `max(0, position_secs − latency_ms/1000)` at every tick. Early
/// in playback (before the pipeline latency has drained) it clamps at 0;
/// once the playhead passes the latency it trails the decoded position by
/// exactly the reported graph latency.
#[test]
fn test_playback_position_is_latency_compensated() {
    let path = write_test_sine_wav();
    let mut config = EngineConfig::default();
    config.limiter.enabled = true;
    config.limiter.lookahead_ms = 5.0;
    let mut engine = AudioEngine::new(config).unwrap();
    engine.load_track(&path).expect("load WAV");
    engine.send_command(EngineCommand::Play);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        engine.tick();
        let info = engine.playback_info();
        // Invariant holds on every observed snapshot.
        let expected = (info.position_secs - info.latency_ms / 1000.0).max(0.0);
        assert!(
            (info.position_secs_compensated - expected).abs() < 1e-3,
            "compensated {:.4} != max(0, {:.4} − {:.2} ms) = {expected:.4}",
            info.position_secs_compensated,
            info.position_secs,
            info.latency_ms
        );
        if info.position_secs > 0.1 {
            // Latency must be reported once playback is underway.
            assert!(info.latency_ms > 0.0, "graph latency must be reported");
            assert!(
                info.position_secs_compensated < info.position_secs,
                "compensated position must trail the decoded playhead"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "playback did not progress"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_long_playback_clock_tracks_decoded_frames_exactly() {
    // A 60-second, 44.1 kHz track = 2,646,000 source frames. After a full
    // play-to-end session the integer frame clock must equal the exact decoded
    // frame count (zero drift) and the published playhead must match
    // frames / rate to sample precision. Any per-tick float accumulation or
    // dropped/duplicated frame would show up here.
    let path = write_test_wav_duration(44_100, 60, "drift");
    let mut engine = AudioEngine::new_default().unwrap();
    let info = engine.load_track(&path).expect("load long track");
    assert_eq!(info.sample_rate, 44_100);
    assert!((info.duration_secs - 60.0).abs() < 0.01);

    engine.send_command(EngineCommand::Play);
    let expected_frames = 44_100u64 * 60;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        engine.tick();
        if engine.stream_ended {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "long track did not reach end of stream"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    assert_eq!(
        engine.clock.source_frames, expected_frames,
        "clock must land on the exact decoded frame count"
    );
    let expected_secs = expected_frames as f64 / 44_100.0;
    let pos = engine.playback_info().position_secs;
    assert!(
        (pos as f64 - expected_secs).abs() < 0.01,
        "playhead {pos:.4}s must match {expected_secs:.4}s"
    );
    let _ = std::fs::remove_file(&path);
}
