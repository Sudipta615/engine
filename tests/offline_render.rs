//! Test suite for OfflineRenderer faster-than-realtime deterministic rendering.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use config::EngineConfig;
use engine::{AudioSource, OfflineRenderer};

static OFFLINE_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_test_wav(sample_rate: u32, duration_ms: u32) -> PathBuf {
    let id = OFFLINE_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "offline_render_test_{}_{id}.wav",
        std::process::id()
    ));
    let n_frames = (sample_rate as u64 * duration_ms as u64 / 1000) as usize;
    let mut pcm = Vec::with_capacity(n_frames * 4);

    for i in 0..n_frames {
        let t = i as f32 / sample_rate as f32;
        let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
        let v = (s * 32767.0) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);

    let mut f = File::create(&path).expect("create wav");
    f.write_all(&wav).expect("write wav");
    path
}

#[test]
fn test_offline_render_pcm_buffer() {
    let cfg = EngineConfig::default();
    let renderer = OfflineRenderer::new(cfg);

    // Synthetic stereo ramp with peak 0.8
    let input: Vec<f32> = (0..1024)
        .flat_map(|i| {
            let v = 0.8 * (i as f32) / 1024.0;
            [v, -v]
        })
        .collect();

    let result = renderer.render_pcm(&input, 2, 48000).expect("render pcm");

    assert_eq!(result.sample_rate, 48000);
    assert_eq!(result.channels, 2);
    assert_eq!(result.frames(), 1024);

    let peak = result.peak();
    assert!((0.75..=0.85).contains(&peak), "Peak was {}", peak);

    let rms = result.rms();
    assert!(rms > 0.0);
}

#[test]
fn test_offline_render_source_file() {
    let wav = write_test_wav(44100, 250);
    let cfg = EngineConfig::default();

    let renderer = OfflineRenderer::new(cfg);
    let result = renderer
        .render_source(&AudioSource::from_file(&wav))
        .expect("render file");

    assert_eq!(result.sample_rate, 44100);
    assert_eq!(result.channels, 2);
    assert!(result.duration_secs >= 0.24 && result.duration_secs <= 0.26);

    let peak = result.peak();
    assert!(peak > 0.4 && peak < 0.6);

    let _ = std::fs::remove_file(&wav);
}

#[test]
fn test_offline_render_diff_verification() {
    let cfg = EngineConfig::default();
    let renderer = OfflineRenderer::new(cfg);

    let pcm = vec![0.5f32; 512];
    let res1 = renderer.render_pcm(&pcm, 2, 44100).expect("render 1");
    let res2 = renderer.render_pcm(&pcm, 2, 44100).expect("render 2");

    // Identical renders must produce zero delta RMS
    let (max_diff, rms_diff) = res1.diff(&res2.samples).expect("diff");
    assert!(
        max_diff < 1e-6,
        "Deterministic offline renders should match exactly"
    );
    assert!(rms_diff < 1e-6);
}
