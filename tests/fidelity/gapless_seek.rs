use std::path::Path;

use engine::decode::{scan_track_loudness, GaplessInfo};
use engine::dsp::pipeline::DspPipeline;
use engine::output::capabilities::OutputAccessMode;
use engine::output::output_info::OutputInfo;

#[test]
fn test_gapless_seek_coordinate_mapping() {
    let gapless = GaplessInfo {
        encoder_delay: 529,
        end_padding: 1152,
        priming_frames: 529,
        total_logical_frames: Some(200_000),
    };

    // Seek to logical start (0 frames) -> physical frame is 529
    let target_logical_0 = 0u64;
    let target_physical_0 = gapless.encoder_delay + target_logical_0;
    assert_eq!(target_physical_0, 529);

    let (skip_0, rem_0) = gapless.state_after_seek(target_physical_0);
    assert_eq!(
        skip_0, 0,
        "When seeking to logical 0, physical frame 529 is start of audio"
    );
    assert_eq!(rem_0, Some(200_000), "All logical frames remain ahead");

    // Seek inside encoder delay region (e.g. physical frame 200)
    let (skip_early, rem_early) = gapless.state_after_seek(200);
    assert_eq!(
        skip_early, 329,
        "529 - 200 = 329 remaining delay frames to skip"
    );
    assert_eq!(
        rem_early,
        Some(200_000),
        "Full track still ahead once delay skipped"
    );

    // Seek halfway into logical track (100,000 frames) -> physical frame 100,529
    let target_logical_half = 100_000u64;
    let target_physical_half = gapless.encoder_delay + target_logical_half;
    assert_eq!(target_physical_half, 100_529);

    let (skip_half, rem_half) = gapless.state_after_seek(target_physical_half);
    assert_eq!(
        skip_half, 0,
        "No encoder delay remains when seeking past it"
    );
    assert_eq!(
        rem_half,
        Some(100_000),
        "Remaining logical frames = 200,000 - 100,000"
    );

    // Seek near logical EOS (199,500 frames) -> physical frame 200,029
    let target_logical_end = 199_500u64;
    let target_physical_end = gapless.encoder_delay + target_logical_end;
    let (skip_end, rem_end) = gapless.state_after_seek(target_physical_end);
    assert_eq!(skip_end, 0);
    assert_eq!(
        rem_end,
        Some(500),
        "Remaining frames = 200,000 - 199,500 = 500"
    );

    // Seek past logical EOS (205,000 frames)
    let target_logical_past = 205_000u64;
    let target_physical_past = gapless.encoder_delay + target_logical_past;
    let (skip_past, rem_past) = gapless.state_after_seek(target_physical_past);
    assert_eq!(skip_past, 0);
    assert_eq!(rem_past, Some(0), "Saturates at 0 remaining frames");
}

fn write_5_1_wav(path: &Path, sample_rate: u32, seconds: usize) {
    let n_frames = sample_rate as usize * seconds;
    let channels = 6;
    let mut data = Vec::with_capacity(n_frames * channels * 2);

    for i in 0..n_frames {
        let t = i as f32 / sample_rate as f32;
        let s_1k = (2.0 * std::f32::consts::PI * 1000.0 * t).sin() * 0.5;
        let v_1k = (s_1k * 32767.0) as i16;
        let v_zero = 0i16;

        // 5.1 layout: FL, FR, C, LFE, SL, SR
        // FL, FR
        data.extend_from_slice(&v_1k.to_le_bytes());
        data.extend_from_slice(&v_1k.to_le_bytes());
        // C
        data.extend_from_slice(&v_zero.to_le_bytes());
        // LFE
        data.extend_from_slice(&v_zero.to_le_bytes());
        // SL, SR
        data.extend_from_slice(&v_1k.to_le_bytes());
        data.extend_from_slice(&v_1k.to_le_bytes());
    }

    let byte_rate: u32 = sample_rate * (channels as u32) * 2;
    let block_align: u16 = (channels as u16) * 2;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&(channels as u16).to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);

    std::fs::write(path, &wav).unwrap();
}

#[test]
fn test_multichannel_5_1_loudness_scan() {
    let tmp = std::env::temp_dir().join(format!(
        "multichannel_scan_test_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));

    write_5_1_wav(&tmp, 48000, 3);

    let result = scan_track_loudness(&tmp).expect("5.1 scan must succeed");
    let _ = std::fs::remove_file(&tmp);

    assert!(
        result.ebu_r128_loudness.is_some(),
        "Must produce integrated LUFS"
    );
    let lufs = result.ebu_r128_loudness.unwrap();
    assert!(lufs.is_finite());
    assert!(result.ebu_r128_peak_dbtp.is_some());
    assert!(result.frames_scanned >= 48000 * 3 - 100);
}

#[test]
fn test_end_to_end_latency_reporting() {
    let mut pipeline = DspPipeline::from_config(&config::EngineConfig::default(), 48000.0);
    pipeline.set_limiter_enabled(true);
    let lookahead = pipeline.limiter.lookahead_ms();
    assert!(lookahead > 0.0);

    let stats =
        pipeline.engine_stats_with_latency(44100, 96000, 24, 32, true, true, 10.0, 5.0, 5.0);

    assert_eq!(stats.latency_report.resampler_latency_ms, 10.0);
    assert_eq!(stats.latency_report.ring_buffer_latency_ms, 5.0);
    assert_eq!(stats.latency_report.output_device_latency_ms, 5.0);
    assert_eq!(stats.latency_report.limiter_lookahead_ms, lookahead);
    // The detector-delay breakdown must be sourced from the limiter's own
    // group-delay accessor (a component of, never more than, the total).
    assert_eq!(
        stats.latency_report.limiter_detector_delay_ms,
        pipeline.limiter.detector_delay_ms()
    );
    assert!(stats.latency_report.limiter_detector_delay_ms <= lookahead + 1e-6);
    assert!((stats.latency_report.total_latency_ms - (lookahead + 20.0)).abs() < 1e-4);
    assert_eq!(
        stats.output_latency_ms,
        stats.latency_report.total_latency_ms
    );
}

#[test]
fn test_output_access_state_tracking() {
    let exclusive_info = OutputInfo::exclusive(
        "hw:0,0".to_string(),
        192000,
        2,
        Some(config::AudioBackend::ExclusiveAlsa),
    );

    assert_eq!(
        exclusive_info.access_state.requested,
        OutputAccessMode::Exclusive
    );
    assert_eq!(
        exclusive_info.access_state.actual,
        OutputAccessMode::DirectHw
    );
    assert!(exclusive_info.access_state.verified);
    assert!(exclusive_info.access_state.is_bit_perfect());

    let shared_info = OutputInfo::shared(
        "default".to_string(),
        44100,
        48000,
        2,
        Some(config::AudioBackend::ExclusiveAlsa),
        Some(config::AudioBackend::Auto),
        "Exclusive open failed",
    );

    assert_eq!(
        shared_info.access_state.requested,
        OutputAccessMode::Exclusive
    );
    assert_eq!(shared_info.access_state.actual, OutputAccessMode::Shared);
    assert!(!shared_info.access_state.verified);
    assert!(!shared_info.access_state.is_bit_perfect());
}
