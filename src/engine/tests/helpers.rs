//! Test helpers and mocks for AudioEngine tests.

use std::sync::Arc;

/// Write a 16-bit stereo PCM WAV whose samples are the exact values in
/// `left` / `right` (L/R interleaved) at `sample_rate`.
pub fn write_i16_wav(path: &std::path::Path, sample_rate: u32, left: &[i16], right: &[i16]) {
    assert_eq!(left.len(), right.len());
    let mut data = Vec::with_capacity(left.len() * 4);
    for i in 0..left.len() {
        data.extend_from_slice(&left[i].to_le_bytes());
        data.extend_from_slice(&right[i].to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
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
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(path, &wav).unwrap();
}

/// A deterministic pseudo-random i16 pattern that exercises the full 16-bit
/// range (every value appears many times), so any bit corruption — a stray
/// gain, a filter, a reorder — changes the output detectably.
pub fn full_range_i16_pattern(seed: u32, len: usize, edge: bool) -> Vec<i16> {
    let mut out = Vec::with_capacity(len + 4);
    if edge {
        out.extend_from_slice(&[i16::MIN, -1, 0, 1, i16::MAX]);
    }
    let mut state = seed;
    while out.len() < len + 4 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((state >> 16) as i16); // high 16 bits → full range
    }
    out.truncate(len + 4);
    out
}

/// Write a minimal stereo DSF file (32 blocks × 4096 DSD frames/channel = 32
/// 768 PCM frames at 88.2 kHz ≈ 0.37 s) and return its path. The payload is
/// a deterministic byte pattern — enough to exercise decode + pipeline flow.
pub fn write_test_dsf() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("engine_dsd_test_{}_{}.dsf", std::process::id(), n));
    let block_size = 4096u32;
    let frames = 4096u64 * 32;
    let ch0: Vec<u8> = (0..frames).map(|i| (i % 256) as u8).collect();
    let ch1: Vec<u8> = (0..frames).map(|i| ((i * 7 + 3) % 256) as u8).collect();
    let padded = frames; // 32 blocks is already block-aligned

    let mut out = Vec::new();
    out.extend_from_slice(b"DSD ");
    out.extend_from_slice(&28u64.to_le_bytes());
    let total_size_pos = out.len();
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&52u64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&2_822_400u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(padded * 8).to_le_bytes());
    out.extend_from_slice(&block_size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    // Interleaved per-block channel layout: [ch0 block][ch1 block]…
    let mut audio = vec![0u8; (padded * 2) as usize];
    for (ch, data) in [&ch0[..], &ch1[..]].iter().enumerate() {
        for (b, chunk) in data.chunks(block_size as usize).enumerate() {
            let base = (b * block_size as usize) * 2 + ch * block_size as usize;
            audio[base..base + chunk.len()].copy_from_slice(chunk);
        }
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&((audio.len() as u64) + 12).to_le_bytes());
    out.extend_from_slice(&audio);

    let total = out.len() as u64;
    out[total_size_pos..total_size_pos + 8].copy_from_slice(&total.to_le_bytes());
    std::fs::write(&path, &out).unwrap();
    path
}

/// Write a 1-second stereo 16-bit PCM WAV at `sample_rate` and return its path.
pub fn write_test_wav_at(sample_rate: u32, tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "engine_xfade_{tag}_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let n_frames = sample_rate as usize; // 1 second
    let mut data = Vec::with_capacity(n_frames * 4);
    for i in 0..n_frames {
        let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.5;
        let v = (s * 32767.0) as i16;
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
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
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(&path, &wav).unwrap();
    path
}

/// Write a 16-bit stereo PCM WAV with a custom frame count (unlike
/// [`write_test_wav_at`], which always writes exactly one second).
pub fn write_custom_wav_at(sample_rate: u32, n_frames: usize, tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "engine_custom_{tag}_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut data = Vec::with_capacity(n_frames * 4);
    for i in 0..n_frames {
        let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.5;
        let v = (s * 32767.0) as i16;
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
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
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(&path, &wav).unwrap();
    path
}

/// Write a 1-second stereo 16-bit PCM WAV with `lead_frames` frames of
/// digital silence followed by a `freq` Hz sine (amplitude 0.5). Used to
/// prove that a Fade transition starts the next track from its own sample 0.
pub fn write_lede_wav_at(
    sample_rate: u32,
    freq: u32,
    lead_frames: usize,
    tag: &str,
) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "engine_fade_{tag}_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let n_frames = sample_rate as usize; // 1 second
    let mut data = Vec::with_capacity(n_frames * 4);
    for i in 0..n_frames {
        let s = if i < lead_frames {
            0.0
        } else {
            (2.0 * std::f32::consts::PI * freq as f32 * (i - lead_frames) as f32
                / sample_rate as f32)
                .sin()
                * 0.5
        };
        let v = (s * 32767.0) as i16;
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
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
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(&path, &wav).unwrap();
    path
}

/// Write a stereo 16-bit PCM WAV containing a full-scale 1 kHz sine and
/// return its path. Untagged, so a loudness scan must measure it.
pub fn write_test_sine_wav() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "engine_scan_incoming_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let sample_rate: u32 = 48000;
    let n_frames = sample_rate as usize * 2;
    let mut data = Vec::with_capacity(n_frames * 4);
    for i in 0..n_frames {
        let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32).sin();
        let v = (s * 32767.0) as i16;
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
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
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(&path, &wav).unwrap();
    path
}

/// Write `seconds` seconds of stereo 16-bit PCM WAV at `sample_rate`.
pub fn write_test_wav_duration(sample_rate: u32, seconds: u32, tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "engine_long_{tag}_{}_{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let n_frames = sample_rate as usize * seconds as usize;
    let mut data = Vec::with_capacity(n_frames * 4);
    for i in 0..n_frames {
        let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.25;
        let v = (s * 32767.0) as i16;
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
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
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(&path, &wav).unwrap();
    path
}

/// Build an output profile with a distinctive DSP bundle for tests.
pub fn test_profile(id: &str, ceiling_db: f32) -> crate::output::OutputProfile {
    crate::output::OutputProfile {
        id: id.to_string(),
        name: id.to_string(),
        device_match: vec![id.to_string()],
        dsp: crate::dsp::device_profile::DeviceProfile {
            eq_enabled: true,
            preamp_db: -2.0,
            eq_bands: vec![crate::dsp::device_profile::ProfileEqBand {
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
        volume_mode: Some(config::VolumeMode::SoftwareOnly),
        sample_rate_preference: Some(96000),
        dsd_policy: Some(config::DsdOutput::DoP),
        ..Default::default()
    }
}

/// Test double for the `Output` trait.
pub struct FakeOutput {
    pub supports_hw: bool,
    pub hardware_db: Arc<std::sync::Mutex<Option<f32>>>,
}

impl FakeOutput {
    pub fn new(supports_hw: bool) -> (Self, Arc<std::sync::Mutex<Option<f32>>>) {
        let hardware_db = Arc::new(std::sync::Mutex::new(None));
        (
            Self {
                supports_hw,
                hardware_db: Arc::clone(&hardware_db),
            },
            hardware_db,
        )
    }
}

impl crate::output::OutputVolume for FakeOutput {
    fn supports_hardware_volume(&self) -> bool {
        self.supports_hw
    }
    fn set_hardware_volume_db(&self, db: f32) -> Result<(), crate::output::OutputError> {
        if self.supports_hw {
            *self.hardware_db.lock().unwrap() = Some(db);
            Ok(())
        } else {
            Err(crate::output::OutputError::StreamError(
                "fake: no hardware volume".to_string(),
            ))
        }
    }
}

impl crate::output::Output for FakeOutput {
    fn sample_rate(&self) -> u32 {
        44_100
    }
    fn sample_format(&self) -> cpal::SampleFormat {
        cpal::SampleFormat::F32
    }
    fn buffer_size_frames(&self) -> u32 {
        256
    }
    fn output_info(&self) -> crate::output::OutputInfo {
        crate::output::OutputInfo::exclusive("fake".to_string(), 44_100, 2, None)
    }
    fn capabilities(&self) -> crate::output::OutputCapabilities {
        crate::output::OutputCapabilities {
            sample_rates: vec![44_100],
            hardware_ranges: vec![],
            formats: vec![],
            channels: vec![2],
            device_name: "fake".to_string(),
            access_mode: crate::output::OutputAccessMode::Exclusive,
            access_state: config::OutputAccessState {
                requested: crate::output::OutputAccessMode::Exclusive,
                actual: crate::output::OutputAccessMode::Exclusive,
                verified: true,
            },
            likely_direct_access: true,
            supports_exclusive: true,
        }
    }
    fn device_name(&self) -> String {
        "fake".to_string()
    }
    fn reconfigure_sample_rate(&mut self, target: u32) -> Result<u32, crate::output::OutputError> {
        Ok(target)
    }
    fn reset_buffer(&self) {}
    fn take_underruns(&self) -> u32 {
        0
    }
    fn take_clips(&self) -> u32 {
        0
    }
    fn take_nans(&self) -> u32 {
        0
    }
    fn take_stream_errors(&self) -> crate::output::StreamErrorBatch {
        crate::output::StreamErrorBatch {
            events: vec![],
            dropped: 0,
        }
    }
    fn set_dither_enabled(&self, _enabled: bool) {}
    fn pause(&self) {}
    fn resume(&self) {}
    fn start(&mut self) -> Result<(), crate::output::OutputError> {
        Ok(())
    }
    fn stop(&mut self) {}
}
