//! Phase 4 S6: multi-track lane registry end-to-end tests.
//!
//! A lane is an independent stream mixed onto a bus slot ≥ 2. These tests
//! drive the real engine: open a silent primary + a loud lane, tick the
//! decode loop, and assert on the graph's master meter — which, with a
//! silent primary, reports exactly the lane's audible contribution, so gain
//! changes and ducking are observable end to end.

use config::EngineConfig;

use super::helpers::*;
use crate::{buffer::EngineCommand, engine::AudioEngine, source::AudioSource};

fn tick_until<F: Fn(&mut AudioEngine) -> bool>(
    engine: &mut AudioEngine,
    timeout_secs: f32,
    pred: F,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f32(timeout_secs);
    while std::time::Instant::now() < deadline {
        engine.tick();
        if pred(engine) {
            return true;
        }
    }
    false
}

/// Master-slot peak (the audible output: a silent primary + lanes only).
fn master_peak(engine: &mut AudioEngine) -> f32 {
    let (peak, _rms) = engine.pipeline_mut().control_handle().slot_meters(0);
    peak
}

#[test]
fn lane_mixes_onto_bus_and_tracks_gain_and_removal() {
    // Silent 3 s primary (so the master meter reflects lanes only).
    let silent = std::env::temp_dir().join(format!(
        "lane_silent_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_i16_wav(
        &silent,
        44100,
        &vec![0i16; 44100 * 3],
        &vec![0i16; 44100 * 3],
    );
    // 0.5-amplitude 440 Hz sine lane, 3 s.
    let lane_wav = write_custom_wav_at(44100, 44100 * 3, "lane");

    let config = EngineConfig {
        mix_slots: 3,
        ..EngineConfig::default()
    };
    let mut engine = AudioEngine::new(config).unwrap();
    engine.load_track(&silent).expect("load silent primary");
    engine.send_command(EngineCommand::Play);

    assert!(
        tick_until(&mut engine, 5.0, |e| e.playback_info().position_secs > 0.01),
        "primary playback did not progress"
    );

    // Add the lane; it lands on the first free slot ≥ 2.
    engine.send_command(EngineCommand::AddTrack(AudioSource::File(lane_wav.clone())));
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) > -40.0),
        "lane never became audible at the master"
    );

    // 0.5-amplitude sine → peak ≈ -6.02 dBFS.
    assert!(
        (master_peak(&mut engine) - (-6.02)).abs() < 2.0,
        "lane peak ≈ -6 dB, got {}",
        master_peak(&mut engine)
    );

    // Halve the lane gain → audible level drops ~6 dB.
    engine.send_command(EngineCommand::SetTrackGain { slot: 2, gain: 0.5 });
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) < -8.0),
        "lane gain 0.5 did not reduce the master level"
    );
    assert!(
        (master_peak(&mut engine) - (-12.04)).abs() < 2.0,
        "half gain → ≈ -12 dB, got {}",
        master_peak(&mut engine)
    );

    // Removing the lane silences the master (silent primary → silence).
    engine.send_command(EngineCommand::RemoveTrack(2));
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) < -60.0),
        "removed lane did not silence the master"
    );

    // Ducking the lane from its own source: the trigger sees the lane's peak
    // above threshold and attenuates it by the depth (~12 dB).
    engine.send_command(EngineCommand::AddTrack(AudioSource::File(lane_wav.clone())));
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) > -20.0),
        "re-added lane never became audible"
    );
    engine.send_command(EngineCommand::DuckTracks {
        source_slot: 2,
        targets: vec![2],
        threshold_db: -40.0,
        depth_db: 12.0,
        attack_ms: 0.0,
        release_ms: 0.0,
    });
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) < -12.0),
        "self-ducked lane did not attenuate to ≈ -18 dB, got {}",
        master_peak(&mut engine)
    );
    let (peak, _) = engine.pipeline_mut().control_handle().slot_meters(2);
    let _ = peak;

    let _ = std::fs::remove_file(&silent);
    let _ = std::fs::remove_file(&lane_wav);
}
