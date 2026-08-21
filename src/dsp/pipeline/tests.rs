use super::*;
use config::PrecisionMode;

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;

/// A config that turns on every in-chain DSP stage so the equivalence
/// test exercises the whole pre-mix/post-mix chain. The safety limiter is
/// deliberately excluded here: it is a separate final output stage, not
/// part of [`DspPipeline::process`] / [`DspPipeline::process_block`].
fn full_config() -> EngineConfig {
    let mut c = EngineConfig::default();
    c.eq.enabled = true;
    if c.eq.bands.len() >= 5 {
        c.eq.bands[0].enabled = true;
        c.eq.bands[0].gain_db = 3.0;
        c.eq.bands[2].enabled = true;
        c.eq.bands[2].gain_db = -2.0;
        c.eq.bands[4].gain_db = 1.5;
    }
    c.limiter.enabled = true;
    c.limiter.lookahead_ms = 2.0;
    c.multiband_compressor.enabled = true;
    c.crossfeed.enabled = true;
    c.stereo_enhancer.enabled = true;
    c.stereo_enhancer.width = 1.3;
    c.loudness.mode = config::LoudnessMode::EbuR128;
    c.dither_enabled = true;
    c
}

/// Block processing must produce the same output as per-frame
/// processing, for any block size, in both precision modes.
fn assert_block_matches_per_frame(mode: PrecisionMode) {
    let sr = 44100.0;
    let mut cfg = full_config();
    cfg.precision_mode = mode;

    let mut frame_pipe = DspPipeline::from_config(&cfg, sr);
    frame_pipe.set_volume(0.7);
    frame_pipe.set_balance(-0.3);
    frame_pipe.begin_seek_fadein();

    let n = 4096;
    let mut input_l = Vec::with_capacity(n);
    let mut input_r = Vec::with_capacity(n);
    let mut x = 0.1234567f32;
    for i in 0..n {
        x = (x * 1.0001 + 0.61803398875).fract();
        let t = i as f32;
        input_l.push((t * 0.01).sin() * 0.5 + (x - 0.5) * 0.25);
        input_r.push((t * 0.013).cos() * 0.5 + (x - 0.5) * 0.25);
    }

    // Per-frame reference.
    let mut ref_l = vec![0.0f32; n];
    let mut ref_r = vec![0.0f32; n];
    for i in 0..n {
        let (l, r) = frame_pipe.process(input_l[i], input_r[i]);
        ref_l[i] = l;
        ref_r[i] = r;
    }

    // Block processing with several block sizes, including ones that do
    // not divide the input length evenly.
    for block in [7usize, 64, 128, 256, 1024] {
        let mut bp = DspPipeline::from_config(&cfg, sr);
        bp.set_volume(0.7);
        bp.set_balance(-0.3);
        bp.begin_seek_fadein();
        let mut l = input_l.clone();
        let mut r = input_r.clone();
        for (lc, rc) in l.chunks_mut(block).zip(r.chunks_mut(block)) {
            bp.process_block(lc, rc);
        }
        for i in 0..n {
            assert!(
                (l[i] - ref_l[i]).abs() < 1e-5,
                "{:?} block={} L mismatch at {}: {} vs {}",
                mode,
                block,
                i,
                l[i],
                ref_l[i]
            );
            assert!(
                (r[i] - ref_r[i]).abs() < 1e-5,
                "{:?} block={} R mismatch at {}: {} vs {}",
                mode,
                block,
                i,
                r[i],
                ref_r[i]
            );
        }
    }
}

#[test]
fn output_format_controls_bit_perfect_verdict() {
    let mut pipeline = DspPipeline::from_config(&EngineConfig::default(), 44100.0);
    pipeline.set_volume(1.0);
    pipeline.set_eq_enabled(false);
    pipeline.set_limiter_enabled(false);

    let float_report = pipeline.bit_perfect_report_with_format(
        44100,
        44100,
        24,
        32,
        OutputSampleFormat::F32,
        false,
        true,
    );
    assert!(float_report.is_bit_perfect);
    assert_eq!(float_report.output_format, OutputSampleFormat::F32);
    // Samples-vs-transport split (§13): both must hold for the verdict, and
    // the coarse result must be BitPerfect.
    assert!(float_report.bit_perfect_samples);
    assert!(float_report.bit_perfect_transport);
    assert_eq!(float_report.result, BitPerfectResult::BitPerfect);
    // The legacy boolean API synthesizes an exclusive access state.
    assert_eq!(
        float_report.access_requested,
        config::OutputAccessMode::Exclusive
    );
    assert_eq!(
        float_report.access_actual,
        config::OutputAccessMode::Exclusive
    );
    assert!(float_report.access_verified);
    assert!(!float_report.fallback_occurred);

    // Shared transport with an untouched sample path: samples stay perfect,
    // transport is not — the verdict is UNKNOWN (cannot be proven), never
    // a heuristic claim.
    let shared_report = pipeline.bit_perfect_report_with_format(
        44100,
        44100,
        24,
        32,
        OutputSampleFormat::F32,
        false,
        false,
    );
    assert!(shared_report.bit_perfect_samples);
    assert!(!shared_report.bit_perfect_transport);
    assert!(!shared_report.is_bit_perfect);
    assert_eq!(shared_report.result, BitPerfectResult::Unknown);
    assert_eq!(
        shared_report.access_actual,
        config::OutputAccessMode::Shared
    );
    assert!(!shared_report.access_verified);

    // Active DSP with a verified transport: samples are provably modified →
    // the verdict is DSP.
    pipeline.set_eq_enabled(true);
    let dsp_report = pipeline.bit_perfect_report_with_format(
        44100,
        44100,
        24,
        32,
        OutputSampleFormat::F32,
        false,
        true,
    );
    assert!(!dsp_report.bit_perfect_samples);
    assert!(dsp_report.bit_perfect_transport);
    assert!(!dsp_report.is_bit_perfect);
    assert_eq!(dsp_report.result, BitPerfectResult::Dsp);
    pipeline.set_eq_enabled(false);

    // The access-aware API fills the requested/actual/verified/fallback
    // fields from the backend's own report.
    let access_report = pipeline.bit_perfect_report_with_access(
        44100,
        44100,
        24,
        32,
        OutputSampleFormat::F32,
        false,
        config::OutputAccessState {
            requested: config::OutputAccessMode::Exclusive,
            actual: config::OutputAccessMode::Shared,
            verified: false,
        },
        true, // fallback occurred
    );
    assert!(!access_report.is_bit_perfect);
    assert_eq!(
        access_report.access_requested,
        config::OutputAccessMode::Exclusive
    );
    assert_eq!(
        access_report.access_actual,
        config::OutputAccessMode::Shared
    );
    assert!(!access_report.access_verified);
    assert!(access_report.fallback_occurred);
    // Fallback leaves the sample path untouched but transport unverified.
    assert!(access_report.bit_perfect_samples);
    assert!(!access_report.bit_perfect_transport);
    assert_eq!(access_report.result, BitPerfectResult::Unknown);

    let legacy_depth_only = pipeline.bit_perfect_report(44100, 44100, 24, 32, false, true);
    assert_eq!(legacy_depth_only.output_format, OutputSampleFormat::Unknown);
    assert!(!legacy_depth_only.is_bit_perfect);

    let i16_report = pipeline.bit_perfect_report_with_format(
        44100,
        44100,
        24,
        16,
        OutputSampleFormat::I16,
        false,
        true,
    );
    assert!(!i16_report.is_bit_perfect);
    assert!(!i16_report.bit_depth_not_truncated);

    // Depth-only 32-bit metadata is intentionally ambiguous and fails
    // closed; the format-aware API is required to identify f32/I32.
    assert_eq!(
        OutputSampleFormat::from_bit_depth(32),
        OutputSampleFormat::Unknown
    );
    assert_eq!(
        OutputSampleFormat::from_bit_depth_and_float(32, true),
        OutputSampleFormat::F32
    );
    assert_eq!(
        OutputSampleFormat::from_bit_depth_and_float(32, false),
        OutputSampleFormat::I32
    );
    // 24-bit-in-32: a 24-bit source fits exactly (no truncation, lossless).
    assert_eq!(
        OutputSampleFormat::from_bit_depth(24),
        OutputSampleFormat::I24Le
    );
    assert_eq!(OutputSampleFormat::I24Le.bit_depth(), Some(24));
    let i24le_report = pipeline.bit_perfect_report_with_format(
        44100,
        44100,
        24,
        24,
        OutputSampleFormat::I24Le,
        false,
        true,
    );
    assert!(
        i24le_report.is_bit_perfect,
        "24-bit source in I24Le should be bit-perfect"
    );
    // A 24-bit source truncated by I24Le is impossible (exact fit); a
    // deeper source cannot exist, but 16-bit stays bit-perfect too.
    let i24le_16_report = pipeline.bit_perfect_report_with_format(
        44100,
        44100,
        16,
        24,
        OutputSampleFormat::I24Le,
        false,
        true,
    );
    assert!(i24le_16_report.is_bit_perfect);

    let unknown_report = pipeline.bit_perfect_report_with_format(
        44100,
        44100,
        24,
        0,
        OutputSampleFormat::Unknown,
        false,
        true,
    );
    assert!(!unknown_report.is_bit_perfect);
    assert!(!unknown_report.format_conversion_lossless);
}

/// C4 (spec §13): the engine-owned report fields (channels, decoder
/// losslessness, crossfade, dither, volume path) are finalized by
/// `BitPerfectReport::finalize_with_engine_state`, which re-derives the
/// verdict — dither and crossfade perturb samples and must flip the verdict
/// to DSP even with a verified transport and a clean pipeline.
#[test]
fn engine_owned_fields_finalize_bit_perfect_verdict() {
    let mut pipeline = DspPipeline::from_config(&EngineConfig::default(), 44100.0);
    pipeline.set_volume(1.0);
    pipeline.set_eq_enabled(false);
    pipeline.set_limiter_enabled(false);

    let clean = || {
        let mut bp = pipeline.bit_perfect_report_with_format(
            44_100,
            44_100,
            16,
            32,
            OutputSampleFormat::F32,
            false,
            true,
        );
        bp.source_channels = 2;
        bp.output_channels = 2;
        bp.decoder_lossless = true;
        bp
    };

    // All engine-owned fields consistent → BitPerfect.
    let mut bp = clean();
    assert!(bp.bit_perfect_samples);
    assert!(bp.bit_perfect_transport);
    bp.finalize_with_engine_state();
    assert!(bp.is_bit_perfect);
    assert_eq!(bp.result, BitPerfectResult::BitPerfect);
    assert!(bp.reason.is_none());

    // A lossy source decoder must be visible in the report (the verdict here
    // is driven by the sample conditions; decoder_lossless is reported so a
    // UI can explain lossy playback without a bogus "bit-perfect" claim).
    let mut bp_lossy = clean();
    bp_lossy.decoder_lossless = false;
    bp_lossy.finalize_with_engine_state();
    assert!(!bp_lossy.decoder_lossless);

    // Dither at the quantization boundary: samples provably perturbed, the
    // transport stays perfect → verdict DSP, never a heuristic claim.
    let mut bp_dither = clean();
    bp_dither.dither_active = true;
    bp_dither.finalize_with_engine_state();
    assert!(!bp_dither.bit_perfect_samples);
    assert!(bp_dither.bit_perfect_transport);
    assert!(!bp_dither.is_bit_perfect);
    assert_eq!(bp_dither.result, BitPerfectResult::Dsp);
    assert!(
        bp_dither
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("Dither")),
        "reason must name the dither stage"
    );

    // Crossfade blending two tracks: same invalidation path.
    let mut bp_xfade = clean();
    bp_xfade.crossfade_active = true;
    bp_xfade.finalize_with_engine_state();
    assert!(!bp_xfade.bit_perfect_samples);
    assert!(!bp_xfade.is_bit_perfect);
    assert_eq!(bp_xfade.result, BitPerfectResult::Dsp);

    // Volume path is informational but must round-trip.
    let mut bp_vol = clean();
    bp_vol.volume_path = VolumePath::Hardware;
    bp_vol.finalize_with_engine_state();
    assert_eq!(bp_vol.volume_path, VolumePath::Hardware);
    assert!(bp_vol.is_bit_perfect);
}

#[test]
fn sample_rate_update_preserves_transition_and_stretcher_state() {
    let mut config = EngineConfig::default();
    config.crossfade.enabled = true;
    let mut pipeline = DspPipeline::from_config(&config, 48_000.0);
    pipeline.mixer_mut().set_duration_ms(1_000, 48_000.0);
    pipeline.mixer_mut().start_crossfade();
    for _ in 0..12_000 {
        pipeline.mixer_mut().process(0.0, 0.0, 0.0, 0.0);
    }
    let progress_before = pipeline.mixer().crossfade_progress().expect("active fade");
    pipeline.timestretcher_mut().set_pitch_semitones(7.0);
    let mut left = vec![0.1f32; 256];
    let mut right = vec![0.1f32; 256];
    pipeline
        .timestretcher_mut()
        .process_block(&mut left, &mut right);

    pipeline.update_sample_rate(96_000.0);

    let progress_after = pipeline.mixer().crossfade_progress().expect("active fade");
    assert!((progress_before - progress_after).abs() < 1e-5);
    assert_eq!(pipeline.timestretcher().sample_rate(), 96_000.0);
    assert!(pipeline.timestretcher().is_enabled());
}

#[test]
fn oversized_block_is_rejected_by_checked_api() {
    let mut pipeline = DspPipeline::from_config(&EngineConfig::default(), 44100.0);
    let mut left = vec![0.0f32; MAX_AUDIO_BLOCK_FRAMES + 1];
    let mut right = vec![0.0f32; MAX_AUDIO_BLOCK_FRAMES + 1];
    assert!(pipeline.try_process_block(&mut left, &mut right).is_err());
    assert!(pipeline
        .try_process_block_f64(
            &mut vec![0.0f64; MAX_AUDIO_BLOCK_FRAMES + 1],
            &mut vec![0.0f64; MAX_AUDIO_BLOCK_FRAMES + 1],
        )
        .is_err());
}

#[test]
fn oversized_block_is_split_without_scratch_growth() {
    let mut pipeline = DspPipeline::from_config(&EngineConfig::default(), 44100.0);
    let left_capacity = pipeline.scratch_f64_l.capacity();
    let right_capacity = pipeline.scratch_f64_r.capacity();
    let n = MAX_AUDIO_BLOCK_FRAMES + 257;
    let mut left = vec![0.0f32; n];
    let mut right = vec![0.0f32; n];
    pipeline.process_block(&mut left, &mut right);
    assert_eq!(pipeline.scratch_f64_l.capacity(), left_capacity);
    assert_eq!(pipeline.scratch_f64_r.capacity(), right_capacity);
}

#[test]
fn block_matches_per_frame_performance() {
    assert_block_matches_per_frame(PrecisionMode::Performance);
}

#[test]
fn block_matches_per_frame_quality() {
    assert_block_matches_per_frame(PrecisionMode::Quality);
}

#[test]
fn final_limiter_enforces_ceiling_after_post_mix() {
    let mut cfg = EngineConfig::default();
    cfg.limiter.enabled = true;
    cfg.limiter.ceiling_db = -0.3;
    let ceiling = 10.0_f32.powf(-0.3 / 20.0);

    let mut pipeline = DspPipeline::from_config(&cfg, 48000.0);

    let n = 512usize;
    let mut l = vec![2.0f32; n];
    let mut r = vec![2.0f32; n];

    // The post-mix chain no longer clamps — the safety limiter is a
    // separate final stage in the output domain.
    pipeline.process_post_mix_block(&mut l, &mut r);
    assert!(
        l.iter().any(|&x| x > ceiling + 0.1),
        "post-mix should not clamp to the limiter ceiling"
    );

    pipeline.process_final_limiter_block(&mut l, &mut r);
    let lookahead = pipeline.limiter.lookahead_samples();
    assert!(lookahead < n, "lookahead must fit within the test block");
    for &x in &l[lookahead..] {
        assert!(x <= ceiling + 1e-3, "final limiter exceeded ceiling: {x}");
    }
}

#[test]
fn multichannel_stereo_path_matches_process_block() {
    let mut cfg = full_config();
    cfg.precision_mode = PrecisionMode::Performance;
    let n = 256usize;

    let mut interleaved = Vec::with_capacity(n * 2);
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    let mut x = 0.5f32;
    for i in 0..n {
        x = (x * 1.01 + 0.3).fract();
        let t = i as f32;
        let ls = (t * 0.02).sin() * 0.4 + x;
        let rs = (t * 0.015).cos() * 0.4 + (1.0 - x);
        interleaved.push(ls);
        interleaved.push(rs);
        l.push(ls);
        r.push(rs);
    }

    let mut mc = DspPipeline::from_config(&cfg, 44100.0);
    mc.set_volume(0.7);
    mc.set_balance(-0.3);
    mc.begin_seek_fadein();
    mc.process_block_multichannel(&mut interleaved, 2);

    let mut stereo = DspPipeline::from_config(&cfg, 44100.0);
    stereo.set_volume(0.7);
    stereo.set_balance(-0.3);
    stereo.begin_seek_fadein();
    stereo.process_block(&mut l, &mut r);

    for i in 0..n {
        assert!(
            (interleaved[i * 2] - l[i]).abs() < 1e-5,
            "L mismatch at {i}"
        );
        assert!(
            (interleaved[i * 2 + 1] - r[i]).abs() < 1e-5,
            "R mismatch at {i}"
        );
    }
}

#[test]
fn multichannel_path_scales_every_channel() {
    let cfg = EngineConfig::default();
    let mut mc = DspPipeline::from_config(&cfg, 48000.0);
    mc.set_volume(0.5);
    mc.volume.snap();

    let channels = 6usize;
    let n = 64usize;
    let mut interleaved = vec![0.0f32; n * channels];
    for i in 0..n {
        for ch in 0..channels {
            interleaved[i * channels + ch] = 0.25 * (ch as f32 + 1.0);
        }
    }
    let before = interleaved.clone();
    mc.process_block_multichannel(&mut interleaved, channels);

    for i in 0..n {
        for ch in 0..channels {
            let want = before[i * channels + ch] * 0.5;
            assert!(
                (interleaved[i * channels + ch] - want).abs() < 1e-6,
                "channel {ch} at frame {i}: {} vs {want}",
                interleaved[i * channels + ch]
            );
        }
    }
}

#[test]
fn multichannel_front_filters_affect_only_front_pair() {
    let mut cfg = EngineConfig::default();
    cfg.eq.enabled = true;
    cfg.eq.auto_headroom = false;
    if let Some(b) = cfg.eq.bands.first_mut() {
        b.enabled = true;
        b.filter_type = config::FilterType::Peaking;
        b.frequency = 1000.0;
        b.gain_db = 6.0;
        b.q = 1.0;
    }

    let mut mc = DspPipeline::from_config(&cfg, 48000.0);
    let channels = 6usize;
    let n = 512usize;
    let mut interleaved = vec![0.0f32; n * channels];
    for i in 0..n {
        let s = ((i as f32) * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin() * 0.5;
        for ch in 0..channels {
            interleaved[i * channels + ch] = s;
        }
    }
    let center_before: Vec<f32> = (0..n).map(|i| interleaved[i * channels + 2]).collect();

    mc.process_block_multichannel(&mut interleaved, channels);

    // The front pair runs the EQ; the sine at the band center is boosted.
    let mut front_changed = false;
    for i in 0..n {
        let raw = ((i as f32) * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin() * 0.5;
        if (interleaved[i * channels] - raw).abs() > 1e-3 {
            front_changed = true;
            break;
        }
    }
    assert!(
        front_changed,
        "front left should be affected by the EQ band"
    );

    // Center receives only the scalar chain (no EQ), so it is unchanged.
    for i in 0..n {
        assert!(
            (interleaved[i * channels + 2] - center_before[i]).abs() < 1e-6,
            "center channel should be unaffected by the stereo EQ"
        );
    }
}

/// H2: the channel management stage (trim + routing + LFE) must run on every
/// channel of the multichannel passthrough path, before the pre-mix chain,
/// and must be inert when disabled.
#[test]
fn multichannel_trim_routing_and_lfe_apply() {
    let mut cfg = EngineConfig::default();
    cfg.limiter.enabled = false;
    cfg.channel_trim.enabled = true;
    cfg.channel_trim.entries = vec![
        config::ChannelTrimEntry {
            channel: 0,
            gain_db: -6.0206, // ≈ 0.5×
            ..Default::default()
        },
        config::ChannelTrimEntry {
            channel: 1,
            invert: true,
            ..Default::default()
        },
        config::ChannelTrimEntry {
            channel: 2,
            delay_ms: 1.0,
            ..Default::default()
        },
    ];
    // Swap L and R via the routing matrix; trim then acts on the swapped
    // signal.
    cfg.channel_routing.enabled = true;
    cfg.channel_routing.matrix = vec![
        vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
        vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
        vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
        vec![0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
    ];
    cfg.lfe.enabled = true;
    cfg.lfe.gain_db = 6.0;

    let mut mc = DspPipeline::from_config(&cfg, 1000.0); // 1 kHz → 1 sample per ms
    mc.set_multichannel_layout(&crate::decode::ChannelLayout::FivePointOne);
    mc.volume.snap();

    let channels = 6usize;
    let n = 16usize;
    let mut interleaved = vec![0.0f32; n * channels];
    for i in 0..n {
        for ch in 0..channels {
            interleaved[i * channels + ch] = (i + 1) as f32; // per-frame constant across channels
        }
    }
    let before = interleaved.clone();
    mc.process_block_multichannel(&mut interleaved, channels);

    for i in 0..n {
        let v = (i + 1) as f32;
        // Routing swapped L/R first: dst0 gets src1 (v), then ch0 gain 0.5×.
        assert!(
            (interleaved[i * channels] - v * 0.5).abs() < 1e-3,
            "ch0 (swapped L) trimmed by 0.5"
        );
        // dst1 gets src0 (v), then ch1 polarity inverted.
        assert!(
            (interleaved[i * channels + 1] + v).abs() < 1e-5,
            "ch1 (swapped R) inverted"
        );
        // ch2 keeps identity and is delayed by 1 sample.
        if i == 0 {
            assert!((interleaved[2] - 0.0).abs() < 1e-5, "ch2 delayed");
        } else {
            assert!(
                (interleaved[i * channels + 2] - (i as f32)).abs() < 1e-5,
                "ch2 delayed by one frame"
            );
        }
        // ch3 is the LFE slot (5.1): boosted by +6 dB ≈ 1.995×.
        let lfe_gain = 10f32.powf(6.0 / 20.0);
        assert!(
            (interleaved[i * channels + 3] - v * lfe_gain).abs() < 1e-3,
            "LFE ch3 boosted: got {} want {}",
            interleaved[i * channels + 3],
            v * lfe_gain
        );
        // ch4/ch5 untouched (unity trim, identity routing, not LFE).
        assert!((interleaved[i * channels + 4] - v).abs() < 1e-5);
        assert!((interleaved[i * channels + 5] - v).abs() < 1e-5);
    }

    // Disabled config: pure passthrough.
    let mut off = DspPipeline::from_config(&EngineConfig::default(), 48000.0);
    off.volume.snap();
    let mut data = before.clone();
    off.process_block_multichannel(&mut data, channels);
    for i in 0..n {
        for ch in 0..channels {
            assert!(
                (data[i * channels + ch] - before[i * channels + ch]).abs() < 1e-6,
                "disabled trim must be passthrough"
            );
        }
    }
}

/// Bit-Perfect is a hard sample-path contract: software volume and seek
/// fades must not be allowed to modify samples even if they were configured
/// before the mode was enabled.
#[test]
fn bit_perfect_bypasses_volume_and_fade_in_all_precisions() {
    for mode in [PrecisionMode::Performance, PrecisionMode::Quality] {
        let mut cfg = EngineConfig::default();
        cfg.precision_mode = mode;
        let mut pipeline = DspPipeline::from_config(&cfg, 48_000.0);
        pipeline.set_volume(0.25);
        pipeline.volume.snap();
        pipeline.begin_seek_fadeout();
        pipeline.set_bit_perfect(true);

        let mut left = [0.375f32; 64];
        let mut right = [-0.625f32; 64];
        let before_l = left;
        let before_r = right;
        pipeline.process_block(&mut left, &mut right);
        assert_eq!(left, before_l, "{mode:?} Bit-Perfect changed left samples");
        assert_eq!(
            right, before_r,
            "{mode:?} Bit-Perfect changed right samples"
        );

        let (l, r) = pipeline.process(0.125, -0.25);
        assert_eq!((l, r), (0.125, -0.25));
    }
}

/// H2: the channel management stage must be skipped in bit-perfect mode.
#[test]
fn multichannel_trim_skipped_in_bit_perfect_mode() {
    let mut cfg = EngineConfig::default();
    cfg.channel_trim.enabled = true;
    cfg.channel_trim.entries = vec![config::ChannelTrimEntry {
        channel: 0,
        gain_db: -12.0,
        ..Default::default()
    }];

    let mut mc = DspPipeline::from_config(&cfg, 48000.0);
    mc.set_bit_perfect(true);
    mc.volume.snap();

    let channels = 4usize;
    let n = 32usize;
    let mut interleaved = vec![0.25f32; n * channels];
    let before = interleaved.clone();
    mc.process_block_multichannel(&mut interleaved, channels);
    assert_eq!(interleaved, before, "bit-perfect must bypass channel trim");
}

/// H5: the latency report must account for the crossfeed delay line and the
/// WSOLA analysis lookahead when those stages are active, and both must be
/// zero when the stages are idle.
#[test]
fn latency_report_includes_crossfeed_and_timestretch_terms() {
    let mut cfg = EngineConfig::default();
    let sr = 48_000.0f32;
    let mut pipeline = DspPipeline::from_config(&cfg, sr);

    // Idle: all new terms zero.
    let idle = pipeline.latency_report(0.0, 0.0, 0.0);
    assert_eq!(idle.crossfeed_delay_ms, 0.0);
    assert_eq!(idle.timestretch_latency_ms, 0.0);
    assert!((idle.total_latency_ms - idle.limiter_lookahead_ms).abs() < 1e-6);

    // Crossfeed active: delay-line term appears and is included in the total.
    cfg.crossfeed.enabled = true;
    cfg.crossfeed.custom_delay_ms = 0.3;
    pipeline.apply_config(&cfg);
    let cf = pipeline.latency_report(0.0, 0.0, 0.0);
    assert!(
        cf.crossfeed_delay_ms > 0.0,
        "crossfeed delay must be reported when enabled"
    );
    assert!(
        (cf.crossfeed_delay_ms - 0.3).abs() < 0.05,
        "crossfeed delay {:.3} ms should match the configured 0.3 ms",
        cf.crossfeed_delay_ms
    );
    assert!(
        (cf.total_latency_ms - cf.limiter_lookahead_ms - cf.crossfeed_delay_ms).abs() < 1e-6,
        "crossfeed delay must be summed into the total"
    );

    // Time-stretcher active (speed ≠ 1): WSOLA lookahead term appears.
    pipeline.timestretcher_mut().set_speed(1.5);
    let ts = pipeline.latency_report(0.0, 0.0, 0.0);
    let expected_ts = (1024 / 2 + 128) as f32 / sr * 1000.0; // window/2 + search
    assert!(
        (ts.timestretch_latency_ms - expected_ts).abs() < 0.05,
        "timestretch latency {:.3} vs expected {expected_ts:.3}",
        ts.timestretch_latency_ms
    );
    assert!(
        (ts.total_latency_ms
            - ts.limiter_lookahead_ms
            - ts.crossfeed_delay_ms
            - ts.timestretch_latency_ms)
            .abs()
            < 1e-6,
        "timestretch delay must be summed into the total"
    );

    // Idle again: term drops back to zero. (Speed alone does not idle the
    // stretcher — it ramps, so disable it explicitly.)
    pipeline.timestretcher_mut().set_enabled(false);
    let idle2 = pipeline.latency_report(0.0, 0.0, 0.0);
    assert_eq!(idle2.timestretch_latency_ms, 0.0);
}

/// H2: the stage capability table must be non-empty, name every stage in
/// the chain, and declare the multichannel split accurately.
#[test]
fn stage_capability_table_is_complete_and_consistent() {
    assert!(!DSP_STAGE_CAPABILITIES.is_empty());

    let all: Vec<&str> = DSP_STAGE_CAPABILITIES.iter().map(|s| s.name).collect();
    // Every stage in the MC chain is declared.
    for stage in [
        "channel_trim",
        "out_preamp",
        "out_loudness",
        "eq",
        "multiband_compressor",
        "convolution",
        "balance",
        "crossfeed",
        "stereo_enhancer",
        "timestretch",
        "volume",
        "seek_fade",
        "resampler",
        "limiter",
        "dither",
    ] {
        assert!(
            all.contains(&stage),
            "stage capability row missing for {stage}"
        );
    }

    // The stereo-only stages are exactly the ones the MC path routes to the
    // front pair; the per-channel stages are declared AllChannels.
    for stage in [
        "eq",
        "convolution",
        "crossfeed",
        "stereo_enhancer",
        "timestretch",
        "balance",
    ] {
        let row = DSP_STAGE_CAPABILITIES
            .iter()
            .find(|s| s.name == stage)
            .expect("row");
        assert_eq!(
            row.channel_support,
            StageChannelSupport::StereoOnly,
            "{stage} must be declared stereo-only"
        );
    }
    for stage in [
        "channel_trim",
        "out_preamp",
        "volume",
        "seek_fade",
        "limiter",
        "dither",
    ] {
        let row = DSP_STAGE_CAPABILITIES
            .iter()
            .find(|s| s.name == stage)
            .expect("row");
        assert_eq!(
            row.channel_support,
            StageChannelSupport::AllChannels,
            "{stage} must be declared all-channels"
        );
    }
}

/// §24: the capability table carries node metadata — realtime safety,
/// bit-perfect compatibility, sample-rate sensitivity, precision.
#[test]
fn stage_capability_metadata_is_consistent() {
    // Every hosted stage must be realtime-safe (spec §3.7/§27) and every
    // sample-altering stage must declare itself bit-perfect-incompatible
    // (spec §5.1: a bit-perfect chain contains no DSP).
    for row in DSP_STAGE_CAPABILITIES {
        assert!(row.realtime_safe, "{} must be realtime-safe", row.name);
        assert!(
            !row.bit_perfect_compatible,
            "{} must not be bit-perfect-compatible (it can alter samples)",
            row.name
        );
    }

    // Sample-rate-sensitive stages: coefficient tables, delay lines, and
    // time constants all live at the configured rate.
    for name in [
        "eq",
        "crossfeed",
        "limiter",
        "convolution",
        "timestretch",
        "resampler",
        "out_loudness",
        "volume",
    ] {
        let row = DSP_STAGE_CAPABILITIES
            .iter()
            .find(|s| s.name == name)
            .expect("row");
        assert!(
            row.sample_rate_sensitive,
            "{name} must be sample-rate sensitive"
        );
    }
    // Rate-independent stages: pure per-sample math.
    for name in ["balance", "channel_mix", "dither"] {
        let row = DSP_STAGE_CAPABILITIES
            .iter()
            .find(|s| s.name == name)
            .expect("row");
        assert!(
            !row.sample_rate_sensitive,
            "{name} must be rate-independent"
        );
    }

    // Precision declarations match the documented cores: WSOLA synthesis and
    // the final safety limiter run in f32; R128 measurement in f64.
    let ts = DSP_STAGE_CAPABILITIES
        .iter()
        .find(|s| s.name == "timestretch")
        .unwrap();
    assert_eq!(ts.precision, StagePrecision::F32);
    let lim = DSP_STAGE_CAPABILITIES
        .iter()
        .find(|s| s.name == "limiter")
        .unwrap();
    assert_eq!(lim.precision, StagePrecision::F32);
    let loud = DSP_STAGE_CAPABILITIES
        .iter()
        .find(|s| s.name == "out_loudness")
        .unwrap();
    assert_eq!(loud.precision, StagePrecision::F64);
    let eq = DSP_STAGE_CAPABILITIES
        .iter()
        .find(|s| s.name == "eq")
        .unwrap();
    assert_eq!(eq.precision, StagePrecision::Any);
}

/// §19/§24: `graph_nodes()` merges the capability table with live state —
/// active flags follow the enabled stages and the latency/tail terms match
/// what `latency_report()` sums.
#[test]
fn graph_nodes_reflect_active_stages_and_latency_terms() {
    use std::collections::HashMap;

    let sr = 48_000.0f32;
    let mut cfg = EngineConfig::default();
    cfg.crossfeed.enabled = true;
    cfg.crossfeed.custom_delay_ms = 0.3;
    cfg.limiter.enabled = true;
    cfg.limiter.lookahead_ms = 2.0;
    cfg.multiband_compressor.enabled = true;
    cfg.eq.enabled = true;
    cfg.stereo_enhancer.enabled = true;
    let mut pipeline = DspPipeline::from_config(&cfg, sr);
    pipeline.timestretcher_mut().set_speed(1.5);

    let nodes = pipeline.graph_nodes();
    assert_eq!(nodes.len(), DSP_STAGE_CAPABILITIES.len());
    let by_name: HashMap<&str, &DspNodeInfo> = nodes.iter().map(|n| (n.name, n)).collect();
    let get = |name: &str| by_name[name];

    // Enabled stages are active; disabled/unity stages are not.
    assert!(get("eq").active);
    assert!(get("multiband_compressor").active);
    assert!(get("stereo_enhancer").active);
    assert!(get("crossfeed").active);
    assert!(get("timestretch").active);
    assert!(get("limiter").active);
    assert!(!get("balance").active);
    assert!(!get("volume").active, "unity gain must report inactive");
    assert!(!get("seek_fade").active);
    // Engine-owned stages are honest about not being hosted by the pipeline.
    assert!(!get("resampler").active);
    assert!(!get("dither").active);

    // Latency terms match the stages' own reports.
    assert_eq!(get("crossfeed").latency_ms, get("crossfeed").tail_ms);
    assert!((get("crossfeed").latency_ms - 0.3).abs() < 0.05);
    assert!((get("timestretch").latency_ms - pipeline.timestretcher.latency_ms()).abs() < 1e-6);
    assert!((get("limiter").latency_ms - pipeline.limiter.lookahead_ms()).abs() < 1e-6);
    assert!((get("limiter").tail_ms - pipeline.limiter.release_ms()).abs() < 1e-6);
    assert_eq!(get("eq").latency_ms, 0.0);

    // The node latency sum must equal latency_report's own per-stage sum
    // (with resampler/ring/device terms supplied as zero).
    let report = pipeline.latency_report(0.0, 0.0, 0.0);
    let graph_sum: f32 = nodes.iter().map(|n| n.latency_ms).sum();
    let report_sum = report.crossfeed_delay_ms
        + report.timestretch_latency_ms
        + report.limiter_lookahead_ms
        + report.limiter_detector_delay_ms;
    assert!(
        (graph_sum - report_sum).abs() < 1e-3,
        "graph latency sum {graph_sum} must match latency_report sum {report_sum}"
    );
}

/// §5.1/§24: bit-perfect mode must report the graph as a pure passthrough —
/// every node inactive, zero latency and tail.
#[test]
fn graph_nodes_report_full_passthrough_in_bit_perfect_mode() {
    let mut cfg = EngineConfig::default();
    cfg.crossfeed.enabled = true;
    cfg.limiter.enabled = true;
    let mut pipeline = DspPipeline::from_config(&cfg, 48_000.0);
    pipeline.timestretcher_mut().set_speed(1.5);
    pipeline.set_bit_perfect(true);

    let nodes = pipeline.graph_nodes();
    assert!(
        nodes.iter().all(|n| !n.active),
        "bit-perfect must deactivate every DSP node"
    );
    assert!(
        nodes
            .iter()
            .all(|n| n.latency_ms == 0.0 && n.tail_ms == 0.0),
        "bit-perfect must report zero latency and tail per node"
    );
}
