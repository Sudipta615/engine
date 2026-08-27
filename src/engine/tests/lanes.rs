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
        &vec![0i16; 44100 * 30],
        &vec![0i16; 44100 * 30],
    );
    // 0.5-amplitude 440 Hz sine lane, 30 s (the test's tight tick loop
    // consumes audio much faster than wall-clock, so a short fixture would
    // finish before the later send assertions).
    let lane_wav = write_custom_wav_at(44100, 44100 * 30, "lane");

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

    // Phase 5 S2/S3: make the lane send-only (master-send 0, aux-send 1)
    // with the aux return enabled at unity. The master silences (silent
    // primary + no master contribution) while the aux meter reports the
    // lane's level — the post-fader tap is observable end to end.
    // Send-only observation: master-send 0 + aux-send 1, with the aux
    // RETURN at zero — the aux accumulator still taps the lane (meter hot)
    // but returns nothing into the master (master silent). This isolates
    // the post-fader tap from the return path.
    engine.send_command(EngineCommand::SetTrackGain { slot: 2, gain: 1.0 });
    engine.send_command(EngineCommand::SetTrackMasterGain { slot: 2, gain: 0.0 });
    engine.send_command(EngineCommand::SetTrackSend { slot: 2, gain: 1.0 });
    engine.pipeline_mut().control_handle().set_aux(true, 0.0);
    assert!(
        tick_until(&mut engine, 10.0, |e| {
            let (aux_peak, _) = e.pipeline_mut().control_handle().aux_meters();
            aux_peak > -20.0 && master_peak(e) < -40.0
        }),
        "send-only lane: aux tapped but master silent (return 0), aux={}, master={}",
        engine.pipeline_mut().control_handle().aux_meters().0,
        master_peak(&mut engine)
    );
    let (aux_peak, _) = engine.pipeline_mut().control_handle().aux_meters();
    assert!(
        (aux_peak - (-6.02)).abs() < 2.0,
        "aux meters the 0.5-amplitude send ≈ -6 dB, got {aux_peak}"
    );
    engine.pipeline_mut().control_handle().set_aux(false, 1.0);
    engine.send_command(EngineCommand::SetTrackMasterGain { slot: 2, gain: 1.0 });
    engine.send_command(EngineCommand::SetTrackSend { slot: 2, gain: 0.0 });
    let (aux_peak, _) = engine.pipeline_mut().control_handle().aux_meters();
    assert!(
        (aux_peak - (-6.02)).abs() < 2.0,
        "aux meters the 0.5-amplitude send ≈ -6 dB, got {aux_peak}"
    );
    engine.pipeline_mut().control_handle().set_aux(false, 1.0);
    engine.send_command(EngineCommand::SetTrackMasterGain { slot: 2, gain: 1.0 });
    engine.send_command(EngineCommand::SetTrackSend { slot: 2, gain: 0.0 });

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

#[test]
fn lane_crossfade_with_active_lanes_completes_without_panic() {
    // Phase 5 regression: a crossfade flush while lanes are registered built
    // the secondaries array with MAX_LANES+1 iterator pulls from a
    // MAX_LANES-element scratch and panicked on the 7th — any crossfade with
    // a lane present aborted the engine. The transition must run to
    // completion with the lane staying audible.
    let silent = std::env::temp_dir().join(format!(
        "lane_xfade_silent_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_i16_wav(
        &silent,
        44100,
        &vec![0i16; 44100 * 30],
        &vec![0i16; 44100 * 30],
    );
    let lane_wav = write_custom_wav_at(44100, 44100 * 30, "lane-xfade");

    let config = EngineConfig {
        mix_slots: 3,
        crossfade: config::CrossfadeConfig {
            enabled: true,
            duration_ms: 200,
            ..config::CrossfadeConfig::default()
        },
        transition_mode: config::TransitionMode::Crossfade,
        ..EngineConfig::default()
    };
    let mut engine = AudioEngine::new(config).unwrap();

    engine.load_track(&silent).expect("load silent primary");
    engine.send_command(EngineCommand::Play);
    assert!(
        tick_until(&mut engine, 5.0, |e| e.playback_info().position_secs > 0.01),
        "primary playback did not progress"
    );

    // A lane is registered + audible when the crossfade starts.
    engine.send_command(EngineCommand::AddTrack(AudioSource::File(lane_wav.clone())));
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) > -40.0),
        "lane never became audible"
    );

    // Force the near-end transition with the lane still active.
    let info = engine.playback_info();
    let total_source_frames = (info.duration_secs * info.sample_rate as f32)
        .round()
        .max(1.0) as u64;
    engine
        .prepare_next_track(&silent)
        .expect("prepare next silent track");
    engine
        .clock
        .set_source_frames(total_source_frames.saturating_sub(13_230));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        engine.tick();
        if matches!(
            engine.stream,
            Some(crate::engine::PlaybackStream::Transitioning { .. })
        ) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "crossfade did not trigger with lanes active"
        );
    }

    // Run the full transition (this is the flush path that used to panic).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        engine.tick();
        if matches!(
            engine.stream,
            Some(crate::engine::PlaybackStream::Single { .. })
        ) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "crossfade with lanes did not complete"
        );
    }

    assert!(
        engine.graph.mixer_state() != crate::dsp::crossfade::MixerState::Crossfading,
        "transition settled after completion"
    );
    // The lane survived the transition and stays audible (~-6 dB).
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) > -20.0),
        "lane dropped out after the crossfade"
    );
    assert!(
        (master_peak(&mut engine) - (-6.02)).abs() < 2.0,
        "lane still ≈ -6 dB after transition, got {}",
        master_peak(&mut engine)
    );

    let _ = std::fs::remove_file(&silent);
    let _ = std::fs::remove_file(&lane_wav);
}

#[test]
fn lane_slot_addresses_survive_removal_and_readd() {
    // Phase 5 regression: `fill_lane_scratch` fed the graph by LANE INDEX,
    // but every control command addresses the lane's SLOT. After a removal
    // created a hole, a re-added lane landed on a lower slot than its index:
    // audio and controls disagreed (a lane feeding a detached slot went
    // silent; gains/ducks hit the wrong stream). Placement must be
    // slot-addressed end to end.
    let silent = std::env::temp_dir().join(format!(
        "lane_slot_silent_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_i16_wav(
        &silent,
        44100,
        &vec![0i16; 44100 * 30],
        &vec![0i16; 44100 * 30],
    );
    let lane_a = write_custom_wav_at(44100, 44100 * 30, "lane-slot-a");
    let lane_b = write_custom_wav_at(44100, 44100 * 30, "lane-slot-b");
    let lane_c = write_custom_wav_at(44100, 44100 * 30, "lane-slot-c");

    let config = EngineConfig {
        mix_slots: 4,
        ..EngineConfig::default()
    };
    let mut engine = AudioEngine::new(config).unwrap();
    engine.load_track(&silent).expect("load silent primary");
    engine.send_command(EngineCommand::Play);
    assert!(
        tick_until(&mut engine, 5.0, |e| e.playback_info().position_secs > 0.01),
        "primary playback did not progress"
    );

    // A → slot 2, B → slot 3.
    engine.send_command(EngineCommand::AddTrack(AudioSource::File(lane_a.clone())));
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) > -20.0),
        "lane A never became audible"
    );
    engine.send_command(EngineCommand::AddTrack(AudioSource::File(lane_b.clone())));
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) > -10.0),
        "lane B never became audible (two lanes ≈ 0 dB)"
    );

    // Remove A (slot 2): B is now index 0 but still slot 3. It must stay
    // audible — index-based placement would have fed it the detached slot 2.
    engine.send_command(EngineCommand::RemoveTrack(2));
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) > -15.0),
        "lane B went silent after lane A was removed (slot mismatch)"
    );
    assert!(
        (master_peak(&mut engine) - (-6.02)).abs() < 2.0,
        "lane B alone ≈ -6 dB, got {}",
        master_peak(&mut engine)
    );

    // C re-fills the freed slot 2; lanes = [B(slot 3), C(slot 2)] — C's
    // index (1) is now ABOVE its slot. Commands on slot 2 must govern C and
    // leave B untouched: attenuating slot 2 must NOT quiet B.
    engine.send_command(EngineCommand::AddTrack(AudioSource::File(lane_c.clone())));
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) > -6.5),
        "lane C never became audible"
    );
    engine.send_command(EngineCommand::SetTrackGain { slot: 2, gain: 0.1 });
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) > -15.0),
        "slot 2 gain attenuated lane B (controls not slot-addressed)"
    );

    // And slot 3's gain governs B.
    engine.send_command(EngineCommand::SetTrackGain { slot: 3, gain: 0.1 });
    assert!(
        tick_until(&mut engine, 10.0, |e| master_peak(e) < -15.0),
        "slot 3 gain did not attenuate lane B"
    );

    let _ = std::fs::remove_file(&silent);
    let _ = std::fs::remove_file(&lane_a);
    let _ = std::fs::remove_file(&lane_b);
    let _ = std::fs::remove_file(&lane_c);
}
