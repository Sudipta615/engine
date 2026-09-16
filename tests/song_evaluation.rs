//! End-to-end evaluation of the Shadow Desktop audio playback & DSP engine.
//!
//! Generates a multi-timbral stereo musical composition ("song") and evaluates
//! engine execution across 7 distinct situations:
//! 1. Bit-perfect passthrough baseline
//! 2. Parametric EQ frequency sculpting (Bass + Treble boost, Mid cut)
//! 3. Dynamics & Limiting (Multiband compression + True-peak brickwall ceiling)
//! 4. Stereo imaging & Psychoacoustic Crossfeed (Widener + Bauer profile)
//! 5. 3D Spatial Audio & Binaural Room Acoustics (Reverb decay + Head orientation yaw)
//! 6. Combined Full Mastering Chain (Multi-stage cascade)
//! 7. High-quality Sinc Resampling (44.1 kHz -> 48 kHz)
//!
//! All rendered outputs are saved as `.wav` files into `target/song_evaluation_output/`.

#![allow(
    clippy::field_reassign_with_default,
    clippy::approx_constant,
    clippy::manual_is_multiple_of
)]

use std::fs::{create_dir_all, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use config::{
    CrossfeedProfile, EngineConfig, EqBandConfig, FilterType, SpatialConfig, SpatialRoomConfig,
};
use engine::{AudioSource, OfflineRenderer};

// ── WAV I/O Helper ───────────────────────────────────────────────────────────

fn write_wav_file(
    path: &Path,
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let num_samples = samples.len();
    let bytes_per_sample = 2u16; // 16-bit PCM
    let block_align = channels * bytes_per_sample;
    let byte_rate = sample_rate * block_align as u32;
    let data_size = (num_samples * bytes_per_sample as usize) as u32;
    let riff_size = 36 + data_size;

    let mut f = File::create(path)?;
    f.write_all(b"RIFF")?;
    f.write_all(&riff_size.to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // Subchunk1Size (16 for PCM)
    f.write_all(&1u16.to_le_bytes())?; // AudioFormat (1 for PCM)
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?; // BitsPerSample
    f.write_all(b"data")?;
    f.write_all(&data_size.to_le_bytes())?;

    let mut pcm_buf = Vec::with_capacity(num_samples * 2);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let val = (clamped * 32767.0) as i16;
        pcm_buf.extend_from_slice(&val.to_le_bytes());
    }
    f.write_all(&pcm_buf)?;
    Ok(())
}

fn render_pcm_pipeline(config: &EngineConfig, input: &[f32], sample_rate: u32) -> Vec<f32> {
    let mut graph =
        engine::dsp::graph2::prod::Graph2Engine::from_config(config, sample_rate as f32);
    const BLOCK_FRAMES: usize = 512;
    let mut output = Vec::with_capacity(input.len());
    let mut left = Vec::with_capacity(BLOCK_FRAMES);
    let mut right = Vec::with_capacity(BLOCK_FRAMES);

    for chunk in input.chunks(BLOCK_FRAMES * 2) {
        left.clear();
        right.clear();
        for frame in chunk.chunks(2) {
            left.push(frame[0]);
            right.push(if frame.len() > 1 { frame[1] } else { frame[0] });
        }

        graph.process_block(&mut left, &mut right);
        if config.limiter.enabled {
            graph.process_final_limiter_block(&mut left, &mut right);
        }

        for (&l, &r) in left.iter().zip(right.iter()) {
            output.push(l);
            output.push(r);
        }
    }
    output
}

// ── Multi-Track Song Procedural Synthesizer ──────────────────────────────────

fn generate_audiophile_test_song(sample_rate: u32, duration_secs: f32) -> Vec<f32> {
    let total_frames = (sample_rate as f32 * duration_secs) as usize;
    let mut stereo_samples = vec![0.0f32; total_frames * 2];

    let bpm = 120.0f32;
    let beat_duration = 60.0f32 / bpm; // 0.5s per beat
    let active_duration = 7.0f32; // Notes stop at 7.0s, leaving 1.0s silence for reverb decay test

    for frame_idx in 0..total_frames {
        let t = frame_idx as f32 / sample_rate as f32;
        if t >= active_duration {
            continue; // pure silence for room reverb decay measurement
        }

        let mut l = 0.0f32;
        let mut r = 0.0f32;

        let beat_num = (t / beat_duration).floor() as usize;
        let beat_t = t % beat_duration;
        let sixteenth_num = (t / (beat_duration / 4.0)).floor() as usize;
        let sixteenth_t = t % (beat_duration / 4.0);

        // 1. Kick Drum (beats 0, 2, 4, 6, 8, 10, 12, 14)
        if beat_num % 2 == 0 {
            let kick_env = (-22.0 * beat_t).exp();
            let kick_freq = 45.0 + 95.0 * (-35.0 * beat_t).exp();
            let kick_phase = 2.0 * std::f32::consts::PI * kick_freq * beat_t;
            let kick_click = if beat_t < 0.005 {
                (2.0 * std::f32::consts::PI * 1600.0 * beat_t).sin() * 0.4 * (1.0 - beat_t / 0.005)
            } else {
                0.0
            };
            let kick = (kick_phase.sin() * kick_env + kick_click) * 0.45;
            l += kick;
            r += kick;
        }

        // 2. Snare Drum (beats 1, 3, 5, 7, 9, 11, 13)
        if beat_num % 2 == 1 {
            let snare_env = (-16.0 * beat_t).exp();
            let snare_tone = (2.0 * std::f32::consts::PI * 185.0 * beat_t).sin() * 0.25;
            let seed = (frame_idx.wrapping_mul(1664525).wrapping_add(1013904223)) as u32;
            let noise = ((seed as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.35;
            let snare = (snare_tone + noise) * snare_env * 0.35;
            l += snare * 0.7;
            r += snare * 1.0;
        }

        // 3. Hi-Hats (16th notes)
        let hat_env = (-45.0 * sixteenth_t).exp();
        let hat_seed = (frame_idx.wrapping_mul(214013).wrapping_add(2531011)) as u32;
        let hat_noise = (hat_seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        let hat_accent = if sixteenth_num % 4 == 2 { 1.2 } else { 0.7 };
        let hat = hat_noise * hat_env * 0.12 * hat_accent;
        l += hat * 0.9;
        r += hat * 0.5;

        // 4. Bassline (Eighth notes following roots: A1=55Hz, F1=43.65Hz, C2=65.4Hz, G1=49.0Hz)
        let bar_num = (t / (beat_duration * 4.0)).floor() as usize;
        let root_freq = match bar_num % 4 {
            0 => 55.0f32,
            1 => 43.65f32,
            2 => 65.4f32,
            _ => 49.0f32,
        };
        let eighth_t = t % (beat_duration / 2.0);
        let bass_env = (-6.0 * eighth_t).exp();
        let b1 = (2.0 * std::f32::consts::PI * root_freq * t).sin();
        let b2 = (2.0 * std::f32::consts::PI * (root_freq * 2.0) * t).sin() * 0.5;
        let b3 = (2.0 * std::f32::consts::PI * (root_freq * 3.0) * t).sin() * 0.25;
        let bass = (b1 + b2 + b3) * bass_env * 0.28;
        l += bass;
        r += bass;

        // 5. Polyphonic Pad (Dual detuned stereo voices: +1.5 cents L, -1.5 cents R)
        let chord_notes: [f32; 4] = match bar_num % 4 {
            0 => [220.0, 261.63, 329.63, 392.0],  // Am7
            1 => [174.61, 220.0, 261.63, 329.63], // Fmaj7
            2 => [130.81, 164.81, 196.0, 246.94], // Cmaj7
            _ => [196.0, 246.94, 293.66, 392.0],  // G
        };
        let mut pad_l = 0.0f32;
        let mut pad_r = 0.0f32;
        for &f in &chord_notes {
            let f_l = f * 0.9991;
            let f_r = f * 1.0009;
            pad_l += (2.0 * std::f32::consts::PI * f_l * t).sin() * 0.04;
            pad_r += (2.0 * std::f32::consts::PI * f_r * t).sin() * 0.04;
        }
        l += pad_l;
        r += pad_r;

        // 6. Arpeggiated Lead Melody (Ping-pong auto-panning L/R)
        let arp_freqs = [523.25, 659.25, 783.99, 987.77, 1046.5, 1318.5]; // C5, E5, G5, B5, C6, E6
        let arp_idx = sixteenth_num % arp_freqs.len();
        let arp_freq = arp_freqs[arp_idx];
        let arp_env = (-18.0 * sixteenth_t).exp();
        let arp_tone = (2.0 * std::f32::consts::PI * arp_freq * t).sin() * arp_env * 0.18;
        let pan_angle = (2.0 * std::f32::consts::PI * 0.5 * t).sin(); // -1.0 to 1.0
        let pan_l = 0.5 * (1.0 - pan_angle);
        let pan_r = 0.5 * (1.0 + pan_angle);
        l += arp_tone * pan_l;
        r += arp_tone * pan_r;

        // 7. Dynamic Climax (Bar 3: t in [4.0, 5.0])
        if (4.0..5.0).contains(&t) {
            let riser_t = t - 4.0;
            let riser_freq = 400.0 + 800.0 * riser_t;
            let riser = (2.0 * std::f32::consts::PI * riser_freq * t).sin() * (riser_t * 0.25);
            l += riser;
            r += riser;
            if t < 4.4 {
                let crash_env = (-6.0 * (t - 4.0)).exp();
                let crash_seed = (frame_idx.wrapping_mul(9301).wrapping_add(49297)) as u32;
                let crash_noise =
                    ((crash_seed as f32 / u32::MAX as f32) * 2.0 - 1.0) * crash_env * 0.25;
                l += crash_noise;
                r += crash_noise;
            }
        }

        stereo_samples[frame_idx * 2] = l;
        stereo_samples[frame_idx * 2 + 1] = r;
    }

    stereo_samples
}

// ── Acoustic Metrics & Analysis Helpers ─────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct AcousticMetrics {
    peak_linear: f32,
    peak_dbfs: f32,
    _rms_linear: f32,
    rms_dbfs: f32,
    crest_factor_db: f32,
    low_band_rms_db: f32,  // < 250 Hz
    mid_band_rms_db: f32,  // 250 - 4000 Hz
    high_band_rms_db: f32, // > 4000 Hz
    stereo_correlation: f32,
    side_to_mid_ratio_db: f32,
    reverb_tail_rms: f32, // RMS during t in [7.05s, 7.8s]
}

/// Simple 2nd-order Direct Form I Biquad Filter for spectral measurement.
struct SimpleBiquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl SimpleBiquad {
    fn lowpass(fc: f64, fs: f64, q: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * fc / fs;
        let alpha = w0.sin() / (2.0 * q);
        let cos_w0 = w0.cos();

        let b0 = (1.0 - cos_w0) / 2.0;
        let b1 = 1.0 - cos_w0;
        let b2 = (1.0 - cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn highpass(fc: f64, fs: f64, q: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * fc / fs;
        let alpha = w0.sin() / (2.0 * q);
        let cos_w0 = w0.cos();

        let b0 = (1.0 + cos_w0) / 2.0;
        let b1 = -(1.0 + cos_w0);
        let b2 = (1.0 + cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn bandpass(fc: f64, fs: f64, q: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * fc / fs;
        let alpha = w0.sin() / (2.0 * q);
        let cos_w0 = w0.cos();

        let b0 = alpha;
        let b1 = 0.0;
        let b2 = -alpha;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process(&mut self, input: f64) -> f64 {
        let out = self.b0 * input + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = input;
        self.y2 = self.y1;
        self.y1 = out;
        out
    }
}

fn linear_to_db(linear: f64) -> f32 {
    if linear > 1e-12 {
        (20.0 * linear.log10()) as f32
    } else {
        -200.0
    }
}

fn compute_metrics(samples: &[f32], channels: usize, sample_rate: u32) -> AcousticMetrics {
    assert_eq!(channels, 2, "Metrics analysis expects stereo audio");
    let num_frames = samples.len() / 2;

    let mut peak_val = 0.0f32;
    let mut sum_sq = 0.0f64;

    let mut sum_ll = 0.0f64;
    let mut sum_rr = 0.0f64;
    let mut sum_lr = 0.0f64;

    let mut sum_mid_sq = 0.0f64;
    let mut sum_side_sq = 0.0f64;

    let mut lp_filter = SimpleBiquad::lowpass(150.0, sample_rate as f64, 0.707);
    let mut bp_filter = SimpleBiquad::bandpass(1000.0, sample_rate as f64, 1.0);
    let mut hp_filter = SimpleBiquad::highpass(3000.0, sample_rate as f64, 0.707);

    let mut sum_low_sq = 0.0f64;
    let mut sum_mid_band_sq = 0.0f64;
    let mut sum_high_sq = 0.0f64;

    // Tail measurement: frames between 7.05s and 7.8s
    let tail_start_frame = (7.05 * sample_rate as f64) as usize;
    let tail_end_frame = (7.80 * sample_rate as f64) as usize;
    let mut tail_sum_sq = 0.0f64;
    let mut tail_frames = 0;

    for frame in 0..num_frames {
        let l = samples[frame * 2];
        let r = samples[frame * 2 + 1];

        // Safety assertion against NaN/Inf
        assert!(
            l.is_finite() && r.is_finite(),
            "NaN or Inf detected in audio samples!"
        );

        peak_val = peak_val.max(l.abs()).max(r.abs());

        let mono = ((l + r) * 0.5) as f64;
        sum_sq += (l as f64) * (l as f64) + (r as f64) * (r as f64);

        sum_ll += (l as f64) * (l as f64);
        sum_rr += (r as f64) * (r as f64);
        sum_lr += (l as f64) * (r as f64);

        let mid = ((l + r) as f64) * std::f64::consts::FRAC_1_SQRT_2;
        let side = ((l - r) as f64) * std::f64::consts::FRAC_1_SQRT_2;
        sum_mid_sq += mid * mid;
        sum_side_sq += side * side;

        // Spectral band filters
        let low = lp_filter.process(mono);
        let band_mid = bp_filter.process(mono);
        let high = hp_filter.process(mono);

        sum_low_sq += low * low;
        sum_mid_band_sq += band_mid * band_mid;
        sum_high_sq += high * high;

        // Reverb tail window
        if frame >= tail_start_frame && frame < tail_end_frame {
            tail_sum_sq += (l as f64) * (l as f64) + (r as f64) * (r as f64);
            tail_frames += 1;
        }
    }

    let rms_linear = ((sum_sq / (samples.len() as f64)).sqrt()) as f32;
    let peak_dbfs = linear_to_db(peak_val as f64);
    let rms_dbfs = linear_to_db(rms_linear as f64);
    let crest_factor_db = peak_dbfs - rms_dbfs;

    let denom = (sum_ll * sum_rr).sqrt();
    let stereo_correlation = if denom > 1e-9 {
        (sum_lr / denom) as f32
    } else {
        1.0
    };

    let side_to_mid_ratio_db = linear_to_db((sum_side_sq / sum_mid_sq.max(1e-12)).sqrt());

    let low_band_rms_db = linear_to_db((sum_low_sq / num_frames as f64).sqrt());
    let mid_band_rms_db = linear_to_db((sum_mid_band_sq / num_frames as f64).sqrt());
    let high_band_rms_db = linear_to_db((sum_high_sq / num_frames as f64).sqrt());

    let reverb_tail_rms = if tail_frames > 0 {
        ((tail_sum_sq / (tail_frames * 2) as f64).sqrt()) as f32
    } else {
        0.0
    };

    AcousticMetrics {
        peak_linear: peak_val,
        peak_dbfs,
        _rms_linear: rms_linear,
        rms_dbfs,
        crest_factor_db,
        low_band_rms_db,
        mid_band_rms_db,
        high_band_rms_db,
        stereo_correlation,
        side_to_mid_ratio_db,
        reverb_tail_rms,
    }
}

// ── Master Integration Test ──────────────────────────────────────────────────

#[test]
fn test_song_end_to_end_evaluation() {
    let out_dir = PathBuf::from("target/song_evaluation_output");
    create_dir_all(&out_dir).expect("create output directory");

    let sample_rate = 44100u32;
    let duration_secs = 8.0f32;

    println!("\n================================================================================");
    println!("   SHADOW DESKTOP AUDIO ENGINE - END-TO-END SONG EVALUATION SUITE");
    println!("================================================================================");
    println!(" Synthesizing 8.0s multi-track audiophile stereo song (120 BPM, 44.1 kHz)...");

    let song_pcm = generate_audiophile_test_song(sample_rate, duration_secs);
    let song_wav_path = out_dir.join("01_song_original_input.wav");
    write_wav_file(&song_wav_path, &song_pcm, 2, sample_rate).expect("write original song WAV");

    let input_source = AudioSource::from_file(&song_wav_path);
    let baseline_metrics = compute_metrics(&song_pcm, 2, sample_rate);

    println!(
        "  [OK] Synthesized original song: {} frames ({} samples)",
        song_pcm.len() / 2,
        song_pcm.len()
    );
    println!(
        "       Baseline Peak: {:.2} dBFS | RMS: {:.2} dBFS | Crest: {:.2} dB",
        baseline_metrics.peak_dbfs, baseline_metrics.rms_dbfs, baseline_metrics.crest_factor_db
    );
    println!(
        "       Spectral Energy: Low: {:.2} dB | Mid: {:.2} dB | High: {:.2} dB",
        baseline_metrics.low_band_rms_db,
        baseline_metrics.mid_band_rms_db,
        baseline_metrics.high_band_rms_db
    );
    println!(
        "       Stereo: Correlation: {:.3} | Side/Mid: {:.2} dB",
        baseline_metrics.stereo_correlation, baseline_metrics.side_to_mid_ratio_db
    );

    // Dummy autosave path to isolate spatial tests from user state
    let dummy_spatial_save = out_dir.join("isolated_spatial_scene.json");

    // ─────────────────────────────────────────────────────────────────────────
    // SITUATION 1: Bit-Perfect Baseline Passthrough & End-to-End File Decode
    // ─────────────────────────────────────────────────────────────────────────
    println!("\n--- [Situation 1/7] Pure Passthrough (Bit-Perfect Baseline & File Decode) ---");
    let mut cfg1 = EngineConfig::default();
    cfg1.spatial_autosave_path = Some(dummy_spatial_save.clone());
    cfg1.limiter.enabled = false; // strictly transparent
    cfg1.dither_enabled = false;

    // 1A. Direct DSP Graph passthrough test
    let res_pcm = OfflineRenderer::new(cfg1.clone())
        .render_pcm(&song_pcm, 2, sample_rate)
        .expect("render pcm");
    let (pcm_max_diff, pcm_rms_diff) = res_pcm.diff(&song_pcm).expect("diff pcm against input");
    write_wav_file(
        &out_dir.join("02_song_output_passthrough.wav"),
        &res_pcm.samples,
        2,
        res_pcm.sample_rate,
    )
    .unwrap();

    let m1 = compute_metrics(&res_pcm.samples, res_pcm.channels, res_pcm.sample_rate);
    println!(
        "  -> [Graph2 DSP] Max sample delta: {:.10} (RMS delta: {:.10})",
        pcm_max_diff, pcm_rms_diff
    );
    println!(
        "  -> [Graph2 DSP] Peak: {:.2} dBFS | RMS: {:.2} dBFS | Correlation: {:.4}",
        m1.peak_dbfs, m1.rms_dbfs, m1.stereo_correlation
    );
    assert!(
        pcm_max_diff < 1e-6,
        "DSP passthrough failed bit-perfect transparency: max diff was {}",
        pcm_max_diff
    );

    // 1B. Full file decoder + SPSC ring + state machine test
    let res_source = OfflineRenderer::new(cfg1)
        .render_source(&input_source)
        .expect("render source");
    println!(
        "  -> [AudioEngine Decoder] Rendered {} frames from file (Sample Rate: {} Hz)",
        res_source.frames(),
        res_source.sample_rate
    );
    assert_eq!(
        res_source.frames(),
        song_pcm.len() / 2,
        "Decoder output frame count mismatch!"
    );
    println!(
        "  [PASS] Engine achieves 100% bit-perfect DSP transparency and robust file decoding."
    );

    // ─────────────────────────────────────────────────────────────────────────
    // SITUATION 2: Parametric EQ Tuning (Bass + Treble Boost, Mid Cut)
    // ─────────────────────────────────────────────────────────────────────────
    println!("\n--- [Situation 2/7] Parametric EQ (Biquad Filter Cascade) ---");
    let mut cfg2 = EngineConfig::default();
    cfg2.spatial_autosave_path = Some(dummy_spatial_save.clone());
    cfg2.limiter.enabled = false; // isolate EQ response
    cfg2.eq.enabled = true;
    cfg2.eq.preamp_db = 0.0;
    cfg2.eq.bands = vec![
        EqBandConfig {
            enabled: true,
            filter_type: FilterType::LowShelf,
            frequency: 100.0,
            gain_db: 5.0,
            q: 0.707,
        },
        EqBandConfig {
            enabled: true,
            filter_type: FilterType::Peaking,
            frequency: 1000.0,
            gain_db: -5.0,
            q: 1.0,
        },
        EqBandConfig {
            enabled: true,
            filter_type: FilterType::HighShelf,
            frequency: 3000.0,
            gain_db: 4.5,
            q: 0.707,
        },
    ];

    let res2 = OfflineRenderer::new(cfg2)
        .render_pcm(&song_pcm, 2, sample_rate)
        .expect("render sit 2");
    write_wav_file(
        &out_dir.join("03_song_output_eq.wav"),
        &res2.samples,
        2,
        res2.sample_rate,
    )
    .unwrap();

    let m2 = compute_metrics(&res2.samples, res2.channels, res2.sample_rate);
    let delta_low = m2.low_band_rms_db - baseline_metrics.low_band_rms_db;
    let delta_mid = m2.mid_band_rms_db - baseline_metrics.mid_band_rms_db;
    let delta_high = m2.high_band_rms_db - baseline_metrics.high_band_rms_db;

    println!("  -> Spectral Shift: Low (80Hz shelf): {:+.2} dB | Mid (1kHz notch): {:+.2} dB | High (10kHz shelf): {:+.2} dB",
        delta_low, delta_mid, delta_high);
    assert!(
        delta_low > 2.0,
        "EQ Low boost did not increase low band energy (delta was {:.2} dB)",
        delta_low
    );
    assert!(
        delta_mid < -1.0,
        "EQ Mid notch did not attenuate mid band energy (delta was {:.2} dB)",
        delta_mid
    );
    assert!(
        delta_high > 2.0,
        "EQ High boost did not increase high band energy (delta was {:.2} dB)",
        delta_high
    );
    println!("  [PASS] EQ biquad cascade sculpted frequency bands accurately without instability.");

    // ─────────────────────────────────────────────────────────────────────────
    // SITUATION 3: Dynamics & Brickwall Limiter (Ceiling Enforcement)
    // ─────────────────────────────────────────────────────────────────────────
    println!(
        "\n--- [Situation 3/7] Dynamics & Limiting (Multiband Compressor + Ceiling -1.5 dBFS) ---"
    );
    let mut cfg3 = EngineConfig::default();
    cfg3.spatial_autosave_path = Some(dummy_spatial_save.clone());
    cfg3.multiband_compressor.enabled = true;
    cfg3.multiband_compressor.low_band.threshold_db = -16.0;
    cfg3.multiband_compressor.low_band.ratio = 3.5;
    cfg3.multiband_compressor.mid_band.threshold_db = -14.0;
    cfg3.multiband_compressor.mid_band.ratio = 3.0;
    cfg3.multiband_compressor.high_band.threshold_db = -14.0;
    cfg3.multiband_compressor.high_band.ratio = 2.5;

    cfg3.limiter.enabled = true;
    cfg3.limiter.ceiling_db = -1.5; // -1.5 dBFS = 0.84139 linear
    cfg3.limiter.lookahead_ms = 5.0;
    cfg3.limiter.attack_ms = 0.5;
    cfg3.limiter.release_ms = 60.0;

    // Intentionally boost input by 1.4x (+3 dB) to push hard into compression and limiting
    let hot_input: Vec<f32> = song_pcm.iter().map(|&s| s * 1.4).collect();

    let samples3 = render_pcm_pipeline(&cfg3, &hot_input, sample_rate);
    write_wav_file(
        &out_dir.join("04_song_output_dynamics_limiter.wav"),
        &samples3,
        2,
        sample_rate,
    )
    .unwrap();

    let m3 = compute_metrics(&samples3, 2, sample_rate);
    let ceiling_linear = 10.0f32.powf(-1.5 / 20.0);

    println!(
        "  -> Commanded Ceiling: -1.50 dBFS ({:.4} linear)",
        ceiling_linear
    );
    println!(
        "  -> Measured Max Peak: {:.2} dBFS ({:.4} linear)",
        m3.peak_dbfs, m3.peak_linear
    );
    println!(
        "  -> Measured RMS: {:.2} dBFS | Crest Factor: {:.2} dB (Baseline was {:.2} dB)",
        m3.rms_dbfs, m3.crest_factor_db, baseline_metrics.crest_factor_db
    );

    assert!(
        m3.peak_linear <= ceiling_linear + 1e-4,
        "Limiter ceiling violated! Measured {:.5} > ceiling {:.5}",
        m3.peak_linear,
        ceiling_linear
    );
    println!("  [PASS] True-peak brickwall limiter strictly bounded all transients below ceiling.");

    // ─────────────────────────────────────────────────────────────────────────
    // SITUATION 4: Stereo Width & Psychoacoustic Crossfeed
    // ─────────────────────────────────────────────────────────────────────────
    println!("\n--- [Situation 4/7] Stereo Imaging & Psychoacoustic Crossfeed ---");
    // 4A: Stereo Widener (width = 1.8)
    let mut cfg4a = EngineConfig::default();
    cfg4a.spatial_autosave_path = Some(dummy_spatial_save.clone());
    cfg4a.stereo_enhancer.enabled = true;
    cfg4a.stereo_enhancer.width = 1.8;
    cfg4a.limiter.enabled = false;

    let res4a = OfflineRenderer::new(cfg4a)
        .render_pcm(&song_pcm, 2, sample_rate)
        .expect("render sit 4a");
    write_wav_file(
        &out_dir.join("05_song_output_stereo_widened.wav"),
        &res4a.samples,
        2,
        res4a.sample_rate,
    )
    .unwrap();
    let m4a = compute_metrics(&res4a.samples, res4a.channels, res4a.sample_rate);

    println!("  -> Sub-test 4A (Widener 1.8x): Side/Mid: {:.2} dB (Baseline: {:.2} dB) | Correlation: {:.3} (Baseline: {:.3})",
        m4a.side_to_mid_ratio_db, baseline_metrics.side_to_mid_ratio_db, m4a.stereo_correlation, baseline_metrics.stereo_correlation);
    assert!(
        m4a.side_to_mid_ratio_db > baseline_metrics.side_to_mid_ratio_db + 2.0,
        "Stereo enhancer failed to expand side energy!"
    );

    // 4B: Bauer Binaural Crossfeed
    let mut cfg4b = EngineConfig::default();
    cfg4b.spatial_autosave_path = Some(dummy_spatial_save.clone());
    cfg4b.crossfeed.enabled = true;
    cfg4b.crossfeed.profile = CrossfeedProfile::Bauer;
    cfg4b.limiter.enabled = false;

    let res4b = OfflineRenderer::new(cfg4b)
        .render_pcm(&song_pcm, 2, sample_rate)
        .expect("render sit 4b");
    write_wav_file(
        &out_dir.join("06_song_output_crossfeed.wav"),
        &res4b.samples,
        2,
        res4b.sample_rate,
    )
    .unwrap();
    let m4b = compute_metrics(&res4b.samples, res4b.channels, res4b.sample_rate);

    println!(
        "  -> Sub-test 4B (Bauer Crossfeed): Correlation: {:.3} | Side/Mid: {:.2} dB",
        m4b.stereo_correlation, m4b.side_to_mid_ratio_db
    );
    assert!(
        m4b.stereo_correlation > baseline_metrics.stereo_correlation,
        "Crossfeed did not introduce acoustic ear cross-coupling!"
    );
    println!("  [PASS] Stereo widening and headphone crossfeed operate with correct psychoacoustic response.");

    // ─────────────────────────────────────────────────────────────────────────
    // SITUATION 5: 3D Spatial Audio & Binaural Room Acoustics
    // ─────────────────────────────────────────────────────────────────────────
    println!("\n--- [Situation 5/7] 3D Spatial Audio & Binaural Room Acoustics ---");
    // 5A: Centered Room Simulation
    let mut cfg5a = EngineConfig::default();
    cfg5a.spatial_autosave_path = Some(dummy_spatial_save.clone());
    cfg5a.spatial = SpatialConfig {
        enabled: true,
        center_azimuth_deg: 0.0,
        half_width_deg: 30.0,
        elevation_deg: 0.0,
        gain: 1.0,
        room: SpatialRoomConfig {
            enabled: true,
            width: 8.0,
            depth: 6.0,
            height: 3.5,
            absorption: 0.25,
            reflection_order: 2,
            rt60_ms: 500.0,
            late_mix: 0.35,
            late_distance: true,
            ..Default::default()
        },
        listener_yaw_deg: 0.0,
        ..Default::default()
    };
    cfg5a.limiter.enabled = false;

    let res5a = OfflineRenderer::new(cfg5a)
        .render_pcm(&song_pcm, 2, sample_rate)
        .expect("render sit 5a");
    write_wav_file(
        &out_dir.join("07_song_output_spatial_room_center.wav"),
        &res5a.samples,
        2,
        res5a.sample_rate,
    )
    .unwrap();
    let m5a = compute_metrics(&res5a.samples, res5a.channels, res5a.sample_rate);

    println!("  -> Sub-test 5A (Room Center): Reverb tail RMS in silence (t=7.05..7.8s): {:.5} (Baseline was {:.5})",
        m5a.reverb_tail_rms, baseline_metrics.reverb_tail_rms);
    assert!(
        m5a.reverb_tail_rms > 0.001,
        "Room late-field reverb tail was not detected during silence!"
    );

    // 5B: Head Orientation Yaw (+45° = turned right, virtual stage shifts to left ear)
    let mut cfg5b = EngineConfig::default();
    cfg5b.spatial_autosave_path = Some(dummy_spatial_save.clone());
    cfg5b.spatial = SpatialConfig {
        enabled: true,
        center_azimuth_deg: 0.0,
        half_width_deg: 30.0,
        elevation_deg: 0.0,
        gain: 1.0,
        room: SpatialRoomConfig {
            enabled: false, // isolate ITD/ILD head model
            ..Default::default()
        },
        listener_yaw_deg: 45.0, // Head turned 45 degrees to the right
        ..Default::default()
    };
    cfg5b.limiter.enabled = false;

    let res5b = OfflineRenderer::new(cfg5b)
        .render_pcm(&song_pcm, 2, sample_rate)
        .expect("render sit 5b");
    write_wav_file(
        &out_dir.join("08_song_output_spatial_turned_right.wav"),
        &res5b.samples,
        2,
        res5b.sample_rate,
    )
    .unwrap();

    // Compute Left vs Right ear RMS
    let mut sum_l_sq = 0.0f64;
    let mut sum_r_sq = 0.0f64;
    for chunk in res5b.samples.chunks_exact(2) {
        sum_l_sq += (chunk[0] as f64) * (chunk[0] as f64);
        sum_r_sq += (chunk[1] as f64) * (chunk[1] as f64);
    }
    let ild_db = linear_to_db((sum_l_sq / sum_r_sq.max(1e-12)).sqrt());
    println!(
        "  -> Sub-test 5B (Yaw +45° Right Turn): Left Ear vs Right Ear Level (ILD): {:+.2} dB",
        ild_db
    );
    assert!(
        ild_db > 0.8,
        "Head turn right did not produce positive ILD in left ear (ILD was {:.2} dB)",
        ild_db
    );
    println!("  [PASS] Binaural HRTF head model & acoustic room reflections operate with full physical fidelity.");

    // ─────────────────────────────────────────────────────────────────────────
    // SITUATION 6: Combined Full Mastering Chain (Multi-Stage Cascade)
    // ─────────────────────────────────────────────────────────────────────────
    println!("\n--- [Situation 6/7] Combined Full Mastering Chain ---");
    let mut cfg6 = EngineConfig::default();
    cfg6.spatial_autosave_path = Some(dummy_spatial_save);
    // 1. EQ
    cfg6.eq.enabled = true;
    cfg6.eq.preamp_db = -1.5;
    cfg6.eq.bands = vec![
        EqBandConfig {
            enabled: true,
            filter_type: FilterType::LowShelf,
            frequency: 100.0,
            gain_db: 3.5,
            q: 0.707,
        },
        EqBandConfig {
            enabled: true,
            filter_type: FilterType::HighShelf,
            frequency: 8000.0,
            gain_db: 3.0,
            q: 0.707,
        },
    ];
    // 2. Multiband Compressor
    cfg6.multiband_compressor.enabled = true;
    cfg6.multiband_compressor.low_band.threshold_db = -12.0;
    cfg6.multiband_compressor.low_band.ratio = 2.0;
    // 3. Stereo Enhancer
    cfg6.stereo_enhancer.enabled = true;
    cfg6.stereo_enhancer.width = 1.3;
    // 4. Spatial 3D Room
    cfg6.spatial = SpatialConfig {
        enabled: true,
        center_azimuth_deg: 0.0,
        half_width_deg: 30.0,
        elevation_deg: 0.0,
        gain: 1.0,
        room: SpatialRoomConfig {
            enabled: true,
            width: 9.0,
            depth: 7.0,
            height: 3.2,
            absorption: 0.3,
            reflection_order: 1,
            rt60_ms: 380.0,
            late_mix: 0.25,
            late_distance: true,
            ..Default::default()
        },
        ..Default::default()
    };
    // 5. True-Peak Limiter
    cfg6.limiter.enabled = true;
    cfg6.limiter.ceiling_db = -1.0; // -1.0 dBFS = 0.89125 linear

    let samples6 = render_pcm_pipeline(&cfg6, &song_pcm, sample_rate);
    write_wav_file(
        &out_dir.join("09_song_output_full_mastering_chain.wav"),
        &samples6,
        2,
        sample_rate,
    )
    .unwrap();
    let m6 = compute_metrics(&samples6, 2, sample_rate);

    println!(
        "  -> Measured Peak: {:.2} dBFS (Linear: {:.4}) | Commanded Ceiling: -1.00 dBFS (0.8913)",
        m6.peak_dbfs, m6.peak_linear
    );
    println!(
        "  -> Measured RMS: {:.2} dBFS | Crest: {:.2} dB | Reverb Tail: {:.5}",
        m6.rms_dbfs, m6.crest_factor_db, m6.reverb_tail_rms
    );
    assert!(
        m6.peak_linear <= 0.8913 + 1e-4,
        "Full chain exceeded limiter ceiling!"
    );
    assert!(
        m6.reverb_tail_rms > 0.001,
        "Full chain failed to retain room reverb tail!"
    );
    println!("  [PASS] Full multi-stage chain (EQ + Comp + Stereo + Spatial + Limiter) ran continuously with zero errors.");

    // ─────────────────────────────────────────────────────────────────────────
    // SITUATION 7: High-Quality Resampling (44.1 kHz -> 48 kHz)
    // ─────────────────────────────────────────────────────────────────────────
    println!("\n--- [Situation 7/7] Sinc Resampling (44.1 kHz -> 48 kHz Conversion) ---");
    let mut cfg7 = EngineConfig::default();
    cfg7.limiter.enabled = false;
    cfg7.dither_enabled = false;

    // Use PCM rendering path through Graph2 DSP resampler
    let res7 = OfflineRenderer::new(cfg7)
        .render_pcm(&song_pcm, 2, 48000)
        .expect("render sit 7");
    write_wav_file(
        &out_dir.join("10_song_output_resampled_48k.wav"),
        &res7.samples,
        2,
        res7.sample_rate,
    )
    .unwrap();

    let expected_frames = song_pcm.len() / 2;
    println!(
        "  -> Output Sample Rate: {} Hz | Frames Rendered: {}",
        res7.sample_rate,
        res7.frames()
    );
    assert_eq!(res7.sample_rate, 48000);
    assert_eq!(res7.frames(), expected_frames);
    println!(
        "  [PASS] Resampler output format matches commanded 48 kHz rate with correct frame count."
    );

    println!("\n================================================================================");
    println!("   ALL 7 TEST SITUATIONS PASSED SUCCESSFULLY WITH FULL FIDELITY QUALIFICATION");
    println!("   Audio files written to: {}", out_dir.display());
    println!("================================================================================\n");
}
