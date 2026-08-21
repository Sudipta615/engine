//! Per-output / per-device profile acceptance suite (§10).
//!
//! Verifies, through the public API:
//! - device-match ranking (exact > case-insensitive > substring) and
//!   **deterministic selection** (the same device always selects the same
//!   profile),
//! - profile application reaches the DSP (EQ bands, crossfeed, limiter
//!   ceiling) and the engine config (sample-rate policy, DSD policy, volume
//!   mode) via the public command path,
//! - JSON persistence round-trips a full profile,
//! - device changes select different matching profiles.

use engine::buffer::EngineCommand;
use engine::dsp::device_profile::{DeviceProfile, ProfileEqBand};
use engine::output::{OutputProfile, OutputProfileLibrary};
use engine::AudioEngine;

/// A profile with a distinctive, easily asserted DSP bundle.
fn profile(id: &str, device_pattern: &str, ceiling_db: f32) -> OutputProfile {
    OutputProfile {
        id: id.to_string(),
        name: id.to_string(),
        device_match: vec![device_pattern.to_string()],
        dsp: DeviceProfile {
            eq_enabled: true,
            preamp_db: -2.0,
            eq_bands: vec![ProfileEqBand {
                frequency: 1000.0,
                gain_db: 4.0,
                q: 1.0,
                enabled: true,
            }],
            crossfeed_enabled: true,
            stereo_width: 1.2,
            true_peak_limiter: true,
            limiter_ceiling_db: ceiling_db,
            ..Default::default()
        },
        sample_rate_preference: Some(96000),
        dsd_policy: Some(config::DsdOutput::DoP),
        volume_mode: Some(config::VolumeMode::SoftwareOnly),
        ..Default::default()
    }
}

#[test]
fn device_selection_is_deterministic_and_ranked() {
    let lib = OutputProfileLibrary::with_profiles(vec![
        profile("generic-usb", "usb", -0.3),
        profile("dac-exact", "USB DAC", -1.0),
        profile("speakers", "speaker", -0.3),
    ]);

    // Same device → same profile, every time.
    assert_eq!(lib.select_for_device("USB DAC").unwrap().id, "dac-exact");
    assert_eq!(lib.select_for_device("USB DAC").unwrap().id, "dac-exact");
    assert_eq!(lib.select_for_device("USB DAC").unwrap().id, "dac-exact");

    // "USB DAC Pro" matches "USB DAC" and "usb" only by substring, so the
    // tie breaks deterministically by insertion order (generic-usb first).
    assert_eq!(
        lib.select_for_device("USB DAC Pro").unwrap().id,
        "generic-usb"
    );

    // Different device → different profile.
    assert_eq!(
        lib.select_for_device("Desktop Speakers").unwrap().id,
        "speakers"
    );

    // No match → None (no accidental default application).
    assert!(lib.select_for_device("HDMI TV").is_none());
}

#[test]
fn profile_application_reaches_the_dsp_and_config() {
    let mut engine = AudioEngine::new_default().unwrap();
    engine.send_command(EngineCommand::SetOutputProfile(profile(
        "dac-exact",
        "USB DAC",
        -1.5,
    )));
    engine.tick();

    // EQ: bands + preamp reached the parametric stage.
    assert!(engine.pipeline().eq.is_enabled());
    assert!((engine.pipeline().eq.preamp_db() - (-2.0)).abs() < 1e-4);
    let band = engine.pipeline().eq.band_params(0).unwrap();
    assert!((band.frequency - 1000.0).abs() < 1e-3);
    assert!((band.gain_db - 4.0).abs() < 1e-4);

    // Crossfeed and limiter ceiling reached the pipeline.
    assert!(engine.pipeline().crossfeed.is_enabled());
    assert!((engine.pipeline().limiter.ceiling_db() - (-1.5)).abs() < 1e-3);

    // Transport preferences landed in the engine configuration.
    assert_eq!(
        engine.config().sample_rate_policy,
        config::SampleRatePolicy::Fixed(96000)
    );
    assert_eq!(engine.config().dsd_output, config::DsdOutput::DoP);
    assert_eq!(
        engine.config().volume_mode,
        config::VolumeMode::SoftwareOnly
    );

    // Diagnostics expose the active profile.
    assert_eq!(
        engine.playback_info().active_output_profile.as_deref(),
        Some("dac-exact")
    );
}

#[test]
fn profile_json_round_trip_preserves_all_fields() {
    let mut p = profile("roundtrip", "USB DAC", -1.0);
    p.backend_preference = Some(config::AudioBackend::ExclusiveAlsa);
    let json = serde_json::to_string(&p).unwrap();
    let back: OutputProfile = serde_json::from_str(&json).unwrap();
    assert_eq!(back, p);
}

#[test]
fn profile_change_switches_dsp_bundle() {
    // Applying a different profile must replace the DSP state, not stack it.
    let mut engine = AudioEngine::new_default().unwrap();
    engine.send_command(EngineCommand::SetOutputProfile(profile(
        "dac-a", "DAC A", -1.0,
    )));
    engine.tick();
    assert!((engine.pipeline().limiter.ceiling_db() - (-1.0)).abs() < 1e-3);

    engine.send_command(EngineCommand::SetOutputProfile(profile(
        "dac-b", "DAC B", -3.0,
    )));
    engine.tick();
    assert!((engine.pipeline().limiter.ceiling_db() - (-3.0)).abs() < 1e-3);
    assert_eq!(
        engine.playback_info().active_output_profile.as_deref(),
        Some("dac-b")
    );

    // Clearing the profile removes the active id (auto-selection resumes).
    engine.send_command(EngineCommand::ClearOutputProfile);
    engine.tick();
    assert_eq!(engine.playback_info().active_output_profile, None);
}
