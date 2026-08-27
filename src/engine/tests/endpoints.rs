//! Multi-endpoint routing matrix integration tests (roadmap Phase 5).
//!
//! The fan-out runs inside the decode loop: every master-domain block the
//! engine produces (post final limiter) is also fed to each additional
//! endpoint's transport (resample → endpoint limiter → gain → the
//! endpoint's own ring). These tests inject an endpoint transport with a
//! fake `Output` and assert the endpoint ring receives the engine's audio —
//! rate-matched or resampled.

use config::{EndpointConfig, EngineConfig, PrecisionMode, ResamplerQuality};

use super::helpers::*;
use crate::buffer::EngineCommand;
use crate::engine::endpoints::EndpointTransport;
use crate::engine::AudioEngine;

/// Build an endpoint transport on a fake 48 kHz output with a master rate of
/// 44.1 kHz (forces the resampler path).
fn endpoint_48k() -> (
    EndpointTransport,
    std::sync::Arc<crate::buffer::FixedFrameBuffer>,
) {
    let out = FakeEndpointOutput::at_rate(48_000);
    let ring = std::sync::Arc::new(
        crate::buffer::FixedFrameBuffer::new(crate::buffer::OUTPUT_BUFFER_FRAMES)
            .expect("endpoint ring"),
    );
    let mut ep = EndpointTransport::open(
        EndpointConfig {
            device: "fake-48k".to_string(),
            ..EndpointConfig::default()
        },
        Box::new(out),
        ring.clone(),
        44_100,
        ResamplerQuality::Balanced,
        PrecisionMode::Performance,
        &EngineConfig::default().limiter,
    )
    .expect("endpoint transport");
    ep.gain = 1.0;
    (ep, ring)
}

#[test]
fn decode_loop_fans_out_to_additional_endpoint() {
    // A 30 s silent primary + the engine's own audio (the master mix is the
    // primary stream). With an injected 48 kHz endpoint and a 44.1 kHz
    // master, the endpoint ring must receive resampled copies of the
    // engine's output.
    let track = write_custom_wav_at(44_100, 44_100 * 20, "ep-master");
    let config = EngineConfig::default();
    let mut engine = AudioEngine::new(config).unwrap();

    let (ep, ring) = endpoint_48k();
    engine.extra_endpoints.push(ep);
    assert_eq!(engine.additional_endpoint_count(), 1);
    assert_eq!(engine.additional_endpoint_sample_rates(), vec![48_000]);

    engine.load_track(&track).expect("load track");
    engine.send_command(EngineCommand::Play);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        engine.tick();
        if ring.available() > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "endpoint ring never received audio"
        );
    }

    // The endpoint's ring holds resampled stereo frames at 48 kHz. A quick
    // content check: the track is a 0.5-amplitude sine, so the resampled
    // peak is ≈ 0.5 and all samples are finite.
    let mut out = vec![0.0f32; 16_384];
    let got = ring.pop_block_interleaved(&mut out);
    let frames = got / 2;
    assert!(frames > 0, "endpoint ring has frames");
    let mut peak = 0.0f32;
    for i in 0..frames {
        let v = out[i * 2];
        assert!(v.is_finite(), "non-finite endpoint sample at {i}");
        peak = peak.max(v.abs());
    }
    assert!(
        (peak - 0.5).abs() < 0.1,
        "endpoint receives the ≈0.5-amplitude master, got {peak}"
    );

    // The engine reports the endpoint in telemetry (refreshed on the 2 s
    // telemetry cadence).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.playback_info().endpoints.is_empty() {
        engine.tick();
        assert!(
            std::time::Instant::now() < deadline,
            "endpoint telemetry never populated"
        );
    }
    let pb = engine.playback_info();
    assert_eq!(pb.endpoints.len(), 1, "endpoint telemetry populated");
    assert_eq!(pb.endpoints[0].sample_rate, 48_000);

    let _ = std::fs::remove_file(&track);
}

#[test]
fn same_rate_endpoint_receives_master_unchanged() {
    // A 44.1 kHz endpoint beside a 44.1 kHz master: no resampler, the ring
    // carries the master block at gain 1.0.
    let track = write_custom_wav_at(44_100, 44_100 * 20, "ep-same");
    let config = EngineConfig::default();
    let mut engine = AudioEngine::new(config).unwrap();

    let out = FakeEndpointOutput::at_rate(44_100);
    let ring = std::sync::Arc::new(
        crate::buffer::FixedFrameBuffer::new(crate::buffer::OUTPUT_BUFFER_FRAMES)
            .expect("endpoint ring"),
    );
    let ep = EndpointTransport::open(
        EndpointConfig {
            device: "fake-44k1".to_string(),
            ..EndpointConfig::default()
        },
        Box::new(out),
        ring.clone(),
        44_100,
        ResamplerQuality::Balanced,
        PrecisionMode::Performance,
        &EngineConfig::default().limiter,
    )
    .expect("endpoint transport");
    assert!(ep.resampler.is_none(), "same-rate: no resampler");
    engine.extra_endpoints.push(ep);

    engine.load_track(&track).expect("load track");
    engine.send_command(EngineCommand::Play);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        engine.tick();
        if ring.available() > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "same-rate endpoint ring never received audio"
        );
    }
    let mut out = vec![0.0f32; 8192];
    let got = ring.pop_block_interleaved(&mut out);
    assert!(got > 0, "same-rate endpoint ring has frames");
    let peak = out[..got].iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    assert!(
        (peak - 0.5).abs() < 0.1,
        "same-rate endpoint sees the master level, got {peak}"
    );

    let _ = std::fs::remove_file(&track);
}
