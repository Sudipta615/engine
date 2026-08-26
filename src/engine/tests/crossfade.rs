//! Crossfade, Fade transitions, and gapless handoff tests.

use config::EngineConfig;

use super::helpers::*;
use crate::{
    buffer::EngineCommand,
    engine::{AudioEngine, PlaybackStream},
};

/// 16-bit-PCM-quantized stereo samples, exactly as `write_custom_wav_at`
/// stores them (so a reference chain feeds the resampler what the decoder
/// will actually decode). Interleaved [l, r, l, r, ...].
#[allow(dead_code)]
fn quantized_sine_samples(sample_rate: u32, n_frames: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(n_frames * 2);
    for i in 0..n_frames {
        let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.5;
        let v = ((s * 32767.0) as i16) as f32 / 32768.0;
        out.push(v);
        out.push(v);
    }
    out
}

/// Reference output (interleaved stereo f32) for a gapless sequence of
/// tracks, mirroring the engine's DSP chain exactly.
#[cfg(feature = "resample")]
fn reference_gapless_output(
    segments: &[(u32, Vec<f32>)],
    output_rate: u32,
    config: &EngineConfig,
    metadata: &[crate::dsp::LoudnessMetadata],
) -> Vec<f32> {
    use crate::dsp::equalizer::{EqBandParams, EqFilterType, ParametricEq, MAX_EQ_BANDS};
    use crate::dsp::limiter::LookaheadLimiter;
    use crate::dsp::resampler::GenericResampler;
    use crate::dsp::{LoudnessMode, LoudnessNormalizer};

    let quality = config.precision_mode == config::PrecisionMode::Quality;

    // Final safety limiter — mirror `DspPipeline::from_config` (f32 in both
    // precision modes, exactly like the engine's output domain).
    let mut limiter = LookaheadLimiter::new_with_params(
        output_rate as f32,
        config.limiter.lookahead_ms,
        config.limiter.attack_ms,
        config.limiter.release_ms,
        config.limiter.ceiling_db,
        config.limiter.soft_clip,
    );
    limiter.set_enabled(config.limiter.enabled);

    // Loudness normalizer — mirror `from_config`.
    let mut loudness = LoudnessNormalizer::new(output_rate as f32);
    loudness.set_mode(match config.loudness.mode {
        config::LoudnessMode::Off => LoudnessMode::Off,
        config::LoudnessMode::TrackReplayGain => LoudnessMode::TrackReplayGain,
        config::LoudnessMode::AlbumReplayGain => LoudnessMode::AlbumReplayGain,
        config::LoudnessMode::EbuR128 => LoudnessMode::EbuR128,
    });
    loudness.set_target_lufs(config.loudness.target_lufs);
    loudness.set_true_peak_guard(
        config.loudness.true_peak_guard,
        config.loudness.true_peak_dbtp,
    );

    // EQ — mirror `from_config` band-for-band, built at the output rate.
    let num_bands = config.eq.bands.len().clamp(10, MAX_EQ_BANDS);
    let mut eq = ParametricEq::new(num_bands, output_rate as f32);
    eq.set_enabled(config.eq.enabled);
    eq.set_preamp_db(config.eq.preamp_db);
    eq.set_post_gain_db(config.eq.post_gain_db);
    eq.set_headroom_db(config.eq.headroom_db);
    for (i, band_cfg) in config.eq.bands.iter().enumerate() {
        if i >= eq.num_bands() {
            break;
        }
        let filter_type = match band_cfg.filter_type {
            config::FilterType::Peaking => EqFilterType::Peaking,
            config::FilterType::LowShelf => EqFilterType::LowShelf,
            config::FilterType::HighShelf => EqFilterType::HighShelf,
            config::FilterType::LowPass => EqFilterType::LowPass,
            config::FilterType::HighPass => EqFilterType::HighPass,
            config::FilterType::Bandpass => EqFilterType::Bandpass,
            config::FilterType::Notch => EqFilterType::Notch,
            config::FilterType::AllPass => EqFilterType::AllPass,
        };
        eq.set_band(
            i,
            EqBandParams {
                enabled: band_cfg.enabled,
                filter_type,
                frequency: band_cfg.frequency,
                gain_db: band_cfg.gain_db,
                q: band_cfg.q,
            },
        );
    }
    eq.set_auto_headroom(config.eq.auto_headroom);

    let mut out = Vec::new();
    let mut resampler: Option<GenericResampler> = None;
    let mut prev_rate: Option<u32> = None;
    for (i, (sr, samples)) in segments.iter().enumerate() {
        if prev_rate != Some(*sr) {
            // Rate change: release the previous resampler's tail first —
            // BEFORE the new segment's metadata is applied, mirroring
            // `swap_to_next_track` (the flush precedes the metadata swap).
            if let Some(mut old) = resampler.take() {
                old.flush();
                while let Some((l, r)) = old.read_f32() {
                    let (l, r) = limiter.process(l, r);
                    out.push(l);
                    out.push(r);
                }
            }
            resampler = Some(
                crate::engine::recovery::build_resampler(
                    config.resampler_quality,
                    *sr as f32,
                    output_rate as f32,
                    1.0,
                    config.precision_mode,
                )
                .expect("reference resampler"),
            );
            prev_rate = Some(*sr);
        }
        // Apply this segment's loudness metadata at its start (mirrors
        // load_track / swap_to_next_track).
        if let Some(meta) = metadata.get(i) {
            loudness.set_track_metadata(meta);
        }
        let r = resampler.as_mut().unwrap();
        for pair in samples.chunks(2) {
            if quality {
                // f64 chain (loudness -> EQ), demoted to f32 exactly as the
                // engine's `process_block` demotes before the resampler,
                // then fed as f64 into the F64 resampler.
                let (l, rr) = loudness.process_f64(pair[0] as f64, pair[1] as f64);
                let (l, rr) = eq.process_f64(l, rr);
                r.feed_f64(l as f32 as f64, rr as f32 as f64);
            } else {
                let (l, rr) = loudness.process(pair[0], pair[1]);
                let (l, rr) = eq.process(l, rr);
                r.feed_f32(l, rr);
            }
        }
    }
    // Final flush: the last resampler, then the shared limiter's delay tail.
    if let Some(mut r) = resampler {
        r.flush();
        while let Some((l, rr)) = r.read_f32() {
            let (l, rr) = limiter.process(l, rr);
            out.push(l);
            out.push(rr);
        }
    }
    for (l, r) in limiter.flush() {
        out.push(l);
        out.push(r);
    }
    out
}

/// Assert the engine's output matches the reference chain sample-for-sample.
#[allow(dead_code)]
fn assert_samples_match(engine_out: &[f32], reference: &[f32], context: &str) {
    assert_eq!(
        engine_out.len(),
        reference.len(),
        "{context}: output length {} != reference length {}",
        engine_out.len(),
        reference.len()
    );
    let max_diff = engine_out
        .iter()
        .zip(reference.iter())
        .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
    assert!(
        max_diff < 1e-3,
        "{context}: max sample diff {max_diff} — boundary audio content diverged \
         (inserted silence or dropped tail)"
    );
}

#[test]
fn test_end_of_stream_emits_resampler_and_limiter_tails() {
    let path = write_test_wav_duration(48_000, 1, "tail");
    let mut config = EngineConfig::default();
    config.limiter.enabled = true;
    config.limiter.lookahead_ms = 5.0;
    let mut engine = AudioEngine::new(config).unwrap();
    engine.load_track(&path).expect("load 48 kHz track");
    engine.send_command(EngineCommand::Play);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut produced_lower = 0u64;
    let mut empty_ticks = 0u64;
    let mut drained = [0.0f32; 8192];
    loop {
        engine.tick();
        let mid = engine.output_buffer.available() as u64;
        if mid > 0 {
            produced_lower += mid + 512;
        } else {
            empty_ticks += 1;
        }
        loop {
            let n = engine.output_buffer.pop_block_interleaved(&mut drained);
            if n == 0 {
                break;
            }
            let _ = n;
        }
        if engine.stream_ended {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "48 kHz track did not reach EOS"
        );
    }
    loop {
        let n = engine.output_buffer.pop_block_interleaved(&mut drained);
        if n == 0 {
            break;
        }
    }

    let base_output = 44_100u64;
    let produced_upper = produced_lower + 511 * empty_ticks;
    assert!(
        produced_upper >= base_output + 100,
        "EOS tails missing: produced <= {produced_upper} frames, expected >= {}",
        base_output + 100
    );
    assert!(
        produced_lower <= base_output + 2_000,
        "EOS tails over-emitted: produced >= {produced_lower} frames, expected <= {}",
        base_output + 2_000
    );
    assert_eq!(
        engine.output_buffer.available(),
        0,
        "ring must be fully drained after EOS"
    );
    assert!(
        engine.scratch.pending_output_frames.is_empty(),
        "no pending frames may be stranded at EOS"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_crossfade_at_192k_does_not_drop_resampled_frames() {
    let outgoing = write_test_wav_at(44_100, "out");
    let incoming = write_test_wav_at(44_100, "in");

    let mut config = EngineConfig::default();
    config.crossfade.enabled = true;
    config.crossfade.duration_ms = 200;
    config.transition_mode = config::TransitionMode::Crossfade;
    config.resampler_quality = config::ResamplerQuality::Balanced;

    let mut engine = AudioEngine::new(config).unwrap();
    engine.output_sample_rate = 192_000;
    engine.graph.update_sample_rate(192_000.0);

    let info = engine.load_track(&outgoing).expect("load outgoing");
    engine
        .prepare_next_track(&incoming)
        .expect("prepare incoming");

    let total_source_frames = (info.duration_secs * info.sample_rate as f32)
        .round()
        .max(1.0) as u64;
    engine
        .clock
        .set_source_frames(total_source_frames.saturating_sub(13_230)); // ~0.3 s

    let scratch_cap_out = engine.scratch.rs_out_buf.capacity();
    let scratch_cap_in = engine.scratch.rs_in_buf.capacity();
    assert!(
        scratch_cap_out >= crate::dsp::resampler::MAX_OUTPUT_BUFFER_FRAMES,
        "outgoing crossfade scratch is under-sized"
    );
    assert!(
        scratch_cap_in >= crate::dsp::resampler::MAX_OUTPUT_BUFFER_FRAMES,
        "incoming crossfade scratch is under-sized"
    );
    const {
        assert!(
            crate::engine::CROSSFADE_SCRATCH_FRAMES
                >= crate::dsp::resampler::MAX_OUTPUT_BUFFER_FRAMES,
            "CROSSFADE_SCRATCH_FRAMES is smaller than the resampler output bound"
        );
    }

    engine.send_command(EngineCommand::Play);
    engine.tick();
    assert!(
        matches!(engine.stream, Some(PlaybackStream::Transitioning { .. })),
        "crossfade transition should have triggered"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        engine.tick();
        if matches!(engine.stream, Some(PlaybackStream::Single { .. })) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "crossfade did not complete"
        );
    }

    assert!(engine.graph.mixer_state() != crate::dsp::crossfade::MixerState::Crossfading);
    assert_eq!(engine.scratch.rs_out_buf.capacity(), scratch_cap_out);
    assert_eq!(engine.scratch.rs_in_buf.capacity(), scratch_cap_in);
    assert!(
        engine.scratch.rs_out_buf.is_empty(),
        "outgoing scratch not drained"
    );
    assert!(
        engine.scratch.rs_in_buf.is_empty(),
        "incoming scratch not drained"
    );

    let _ = std::fs::remove_file(outgoing);
    let _ = std::fs::remove_file(incoming);
}

#[test]
fn test_fade_transition_is_sequential_fade_out_gap_fade_in() {
    let sr = 44_100u32;
    let a = write_test_wav_at(sr, "fade-a"); // 440 Hz, 1 s, no lead
    let b = write_lede_wav_at(sr, 880, sr as usize * 300 / 1000, "fade-b");

    let mut config = EngineConfig::default();
    config.transition_mode = config::TransitionMode::Fade;
    config.crossfade.duration_ms = 300; // fade-out 100 ms, gap 100 ms, fade-in 100 ms
    let mut engine = AudioEngine::new(config).unwrap();
    engine.output_sample_rate = sr;
    engine.graph.update_sample_rate(sr as f32);

    let info = engine.load_track(&a).expect("load outgoing A");
    engine.prepare_next_track(&b).expect("prepare incoming B");

    let total_source_frames = (info.duration_secs * info.sample_rate as f32)
        .round()
        .max(1.0) as u64;
    engine
        .clock
        .set_source_frames(total_source_frames.saturating_sub(26_460)); // ~0.6 s left

    engine.send_command(EngineCommand::Play);
    engine.tick();
    assert!(
        matches!(engine.stream, Some(PlaybackStream::Transitioning { .. })),
        "fade transition should have triggered"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut engine_out: Vec<f32> = Vec::new();
    let mut saw_swap = false;
    while !engine.stream_ended {
        engine.decode_and_process();
        let mut buf = [0.0f32; 4096];
        loop {
            let n = engine.output_buffer.pop_block_interleaved(&mut buf);
            if n == 0 {
                break;
            }
            engine_out.extend_from_slice(&buf[..n]);
        }
        if engine.loudness_scan.current_track_path.as_deref() == Some(b.as_path()) {
            saw_swap = true;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fade transition did not finish ({} output frames so far)",
            engine_out.len() / 2
        );
    }
    assert!(saw_swap, "engine never handed off to track B");
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);

    let amp = |i: usize| engine_out[i * 2].abs().max(engine_out[i * 2 + 1].abs());
    let total_frames = engine_out.len() / 2;

    let mut longest_run = 0usize;
    let mut run = 0usize;
    for i in 0..total_frames {
        if amp(i) < 0.05 {
            run += 1;
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
    }
    assert!(
        longest_run as f32 / sr as f32 >= 0.30,
        "expected a >= 300 ms near-silence run (fade tail + gap + fade-in + \
         B's 300 ms lead), got {:.3}s",
        longest_run as f32 / sr as f32
    );

    let run_start = {
        let mut start = 0usize;
        let mut cur = 0usize;
        for i in 0..total_frames {
            if amp(i) < 0.05 {
                cur += 1;
                if cur > longest_run - 1 {
                    start = i - cur + 1;
                    break;
                }
            } else {
                cur = 0;
            }
        }
        start
    };
    let run_end = run_start + longest_run - 1;

    let mut last_loud_a = None;
    for i in (0..run_start).rev() {
        if amp(i) > 0.15 {
            last_loud_a = Some(i);
            break;
        }
    }
    let mut first_loud_b = None;
    for i in (run_end + 1)..total_frames {
        if amp(i) > 0.15 {
            first_loud_b = Some(i);
            break;
        }
    }
    let last_loud_a = last_loud_a.expect("A's tone must precede the fade");
    let first_loud_b = first_loud_b.expect("B's tone must follow the fade");
    let silence_secs = (first_loud_b - last_loud_a) as f32 / sr as f32;
    assert!(
        silence_secs >= 0.25,
        "expected >= 250 ms between A's last loud frame and B's tone onset, \
         got {silence_secs:.3}s — the fade is overlapping or B's head was consumed"
    );
}

#[test]
fn test_single_track_resampler_tail_flushed_at_eos() {
    let sr = 44_100u32;
    let out_rate = 48_000u32;
    let n_frames = 6 * 1024 + 856; // 7000
    let path = write_custom_wav_at(sr, n_frames, "rs-tail");

    let mut config = EngineConfig::default();
    config.resampler_quality = config::ResamplerQuality::Balanced;

    let mut engine = AudioEngine::new(config.clone()).unwrap();
    engine.output_sample_rate = out_rate;
    engine.graph.update_sample_rate(out_rate as f32);

    #[cfg(feature = "resample")]
    let ref_count = {
        let mut ref_rs = crate::engine::recovery::build_resampler(
            config::ResamplerQuality::Balanced,
            sr as f32,
            out_rate as f32,
            1.0,
            config.precision_mode,
        )
        .expect("reference resampler");
        for i in 0..n_frames {
            let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin() * 0.5;
            ref_rs.feed_f32(s, s);
        }
        ref_rs.flush();
        let mut count = 0usize;
        while ref_rs.read_f32().is_some() {
            count += 1;
        }
        assert!(
            count > n_frames,
            "sanity: 44.1k→48k must expand the signal ({count} output vs {n_frames} input)"
        );
        let theoretical = (n_frames as f64 * out_rate as f64 / sr as f64).round() as i64;
        assert!(
            (count as i64 - theoretical).abs() < 100,
            "sanity: flushed reference should be near {theoretical}, got {count}"
        );
        count
    };

    #[cfg(not(feature = "resample"))]
    let ref_count = n_frames;

    engine.load_track(&path).expect("load track");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut total = 0usize;
    while !engine.stream_ended {
        engine.decode_and_process();
        let mut buf = [0.0f32; 4096];
        loop {
            let n = engine.output_buffer.pop_block_interleaved(&mut buf);
            if n == 0 {
                break;
            }
            total += n / 2;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "decode did not reach EOS (total {total} output frames)"
        );
    }

    let _ = std::fs::remove_file(&path);

    let limiter_delay = engine.graph.limiter().limiter.lookahead_samples();
    let expected = ref_count as i64 + limiter_delay as i64;
    let diff = total as i64 - expected;
    assert!(
        diff.abs() <= 16,
        "single-track playback emitted {total} output frames; expected {expected} \
         (flushed resampler {ref_count} + limiter lookahead {limiter_delay}); \
         diff {diff} — the resampler tail was {}",
        if diff < 0 { "dropped" } else { "over-emitted" }
    );
}

#[test]
#[cfg(feature = "resample")]
fn test_gapless_handoff_preserves_resampler_and_limiter() {
    for precision in [
        config::PrecisionMode::Performance,
        config::PrecisionMode::Quality,
    ] {
        same_rate_gapless_handoff_impl(precision);
    }
}

#[cfg(feature = "resample")]
fn same_rate_gapless_handoff_impl(precision: config::PrecisionMode) {
    let sr = 44_100u32;
    let out_rate = 48_000u32;
    let n_frames = 6 * 1024 + 856;
    let a = write_custom_wav_at(sr, n_frames, "gapless-a");
    let b = write_custom_wav_at(sr, n_frames, "gapless-b");

    let mut config = EngineConfig::default();
    config.transition_mode = config::TransitionMode::Gapless;
    config.resampler_quality = config::ResamplerQuality::Balanced;
    config.precision_mode = precision;
    let mut engine = AudioEngine::new(config.clone()).unwrap();
    engine.output_sample_rate = out_rate;
    engine.graph.update_sample_rate(out_rate as f32);
    engine.set_volume(1.0);

    let seg_a = quantized_sine_samples(sr, n_frames);
    let seg_b = quantized_sine_samples(sr, n_frames);
    let reference = reference_gapless_output(&[(sr, seg_a), (sr, seg_b)], out_rate, &config, &[]);

    engine.load_track(&a).expect("load A");
    engine.prepare_next_track(&b).expect("prepare B");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut engine_out: Vec<f32> = Vec::new();
    let mut saw_swap = false;
    while !engine.stream_ended {
        engine.decode_and_process();
        let mut buf = [0.0f32; 4096];
        loop {
            let n = engine.output_buffer.pop_block_interleaved(&mut buf);
            if n == 0 {
                break;
            }
            engine_out.extend_from_slice(&buf[..n]);
        }
        if engine.loudness_scan.current_track_path.as_deref() == Some(b.as_path()) {
            saw_swap = true;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "decode did not finish ({} output frames so far)",
            engine_out.len() / 2
        );
    }
    assert!(saw_swap, "engine never handed off to track B");

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);

    let total = engine_out.len() / 2;
    let expected = reference.len() / 2;
    let diff = total as i64 - expected as i64;
    assert!(
        diff.abs() <= 24,
        "gapless handoff ({precision:?}) emitted {total} output frames; continuous \
         reference {expected} (diff {diff}) — frames were {} at the boundary",
        if diff < 0 { "dropped" } else { "over-emitted" }
    );
    assert_samples_match(
        &engine_out,
        &reference,
        &format!("same-rate gapless handoff ({precision:?})"),
    );
}

#[test]
#[cfg(feature = "resample")]
fn test_gapless_handoff_different_rates_rebuilds_resampler() {
    for precision in [
        config::PrecisionMode::Performance,
        config::PrecisionMode::Quality,
    ] {
        different_rates_gapless_handoff_impl(precision);
    }
}

#[cfg(feature = "resample")]
fn different_rates_gapless_handoff_impl(precision: config::PrecisionMode) {
    let sr_a = 44_100u32;
    let sr_b = 96_000u32;
    let out_rate = 48_000u32;
    let n_frames = 6 * 1024 + 856; // 7000 per track
    let a = write_custom_wav_at(sr_a, n_frames, "gap-rate-a");
    let b = write_custom_wav_at(sr_b, n_frames, "gap-rate-b");

    let mut config = EngineConfig::default();
    config.transition_mode = config::TransitionMode::Gapless;
    config.resampler_quality = config::ResamplerQuality::Balanced;
    config.precision_mode = precision;
    let mut engine = AudioEngine::new(config.clone()).unwrap();
    engine.output_sample_rate = out_rate;
    engine.graph.update_sample_rate(out_rate as f32);
    engine.set_volume(1.0);

    let seg_a = quantized_sine_samples(sr_a, n_frames);
    let seg_b = quantized_sine_samples(sr_b, n_frames);
    let reference =
        reference_gapless_output(&[(sr_a, seg_a), (sr_b, seg_b)], out_rate, &config, &[]);

    engine.load_track(&a).expect("load A");
    engine.prepare_next_track(&b).expect("prepare B");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut engine_out: Vec<f32> = Vec::new();
    let mut saw_swap = false;
    while !engine.stream_ended {
        engine.decode_and_process();
        let mut buf = [0.0f32; 4096];
        loop {
            let n = engine.output_buffer.pop_block_interleaved(&mut buf);
            if n == 0 {
                break;
            }
            engine_out.extend_from_slice(&buf[..n]);
        }
        if engine.loudness_scan.current_track_path.as_deref() == Some(b.as_path()) {
            saw_swap = true;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "decode did not finish ({} output frames so far)",
            engine_out.len() / 2
        );
    }
    assert!(saw_swap, "engine never handed off to track B");

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);

    let total = engine_out.len() / 2;
    let expected = reference.len() / 2;
    let diff = total as i64 - expected as i64;
    assert!(
        diff.abs() <= 24,
        "rate-changing gapless handoff ({precision:?}) emitted {total} output frames; \
         reference {expected} (diff {diff}) — the outgoing track's tail was {}",
        if diff < 0 { "dropped" } else { "over-emitted" }
    );
    assert_samples_match(
        &engine_out,
        &reference,
        &format!("rate-changing gapless handoff ({precision:?})"),
    );
}

#[test]
#[cfg(feature = "resample")]
fn test_gapless_handoff_smooths_loudness_gain_and_eq_across_boundary() {
    for precision in [
        config::PrecisionMode::Performance,
        config::PrecisionMode::Quality,
    ] {
        loudness_eq_gapless_handoff_impl(precision);
    }
}

#[cfg(feature = "resample")]
fn loudness_eq_gapless_handoff_impl(precision: config::PrecisionMode) {
    let sr = 44_100u32;
    let out_rate = 48_000u32;
    let n_frames = 6 * 1024 + 856; // 7000 per track
    let a = write_custom_wav_at(sr, n_frames, "gap-loud-a");
    let b = write_custom_wav_at(sr, n_frames, "gap-loud-b");

    use crate::dsp::LoudnessMetadata;

    let mut config = EngineConfig::default();
    config.transition_mode = config::TransitionMode::Gapless;
    config.resampler_quality = config::ResamplerQuality::Balanced;
    config.precision_mode = precision;
    config.loudness.mode = config::LoudnessMode::TrackReplayGain;
    config.eq.enabled = true;
    if let Some(band) = config.eq.bands.iter_mut().find(|b| b.frequency == 1000.0) {
        band.gain_db = 6.0;
        band.q = 1.41;
    }

    let mut engine = AudioEngine::new(config.clone()).unwrap();
    engine.output_sample_rate = out_rate;
    engine.graph.update_sample_rate(out_rate as f32);
    engine.set_volume(1.0);

    let meta_a = LoudnessMetadata {
        replaygain_track_db: Some(-6.0),
        replaygain_track_peak: Some(0.5),
        ..Default::default()
    };
    let meta_b = LoudnessMetadata {
        replaygain_track_db: Some(-3.0),
        replaygain_track_peak: Some(0.7),
        ..Default::default()
    };

    engine.load_track(&a).expect("load A");
    engine.graph.apply_loudness_metadata_outgoing(Some(meta_a));
    engine.prepare_next_track(&b).expect("prepare B");
    engine.loudness_scan.pending_incoming_loudness_metadata = Some(meta_b);

    let seg_a = quantized_sine_samples(sr, n_frames);
    let seg_b = quantized_sine_samples(sr, n_frames);
    let reference = reference_gapless_output(
        &[(sr, seg_a), (sr, seg_b)],
        out_rate,
        &config,
        &[meta_a, meta_b],
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut engine_out: Vec<f32> = Vec::new();
    let mut saw_swap = false;
    while !engine.stream_ended {
        engine.decode_and_process();
        let mut buf = [0.0f32; 4096];
        loop {
            let n = engine.output_buffer.pop_block_interleaved(&mut buf);
            if n == 0 {
                break;
            }
            engine_out.extend_from_slice(&buf[..n]);
        }
        if engine.loudness_scan.current_track_path.as_deref() == Some(b.as_path()) {
            saw_swap = true;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "decode did not finish ({} output frames so far)",
            engine_out.len() / 2
        );
    }
    assert!(saw_swap, "engine never handed off to track B");

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);

    let total = engine_out.len() / 2;
    let expected = reference.len() / 2;
    let diff = total as i64 - expected as i64;
    assert!(
        diff.abs() <= 24,
        "loudness+EQ gapless handoff ({precision:?}) emitted {total} output frames; \
         reference {expected} (diff {diff}) — frames were {} at the boundary",
        if diff < 0 { "dropped" } else { "over-emitted" }
    );
    assert_samples_match(
        &engine_out,
        &reference,
        &format!("loudness+EQ same-rate gapless handoff ({precision:?})"),
    );
}
