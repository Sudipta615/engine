//! Phase-1 gate: `DspGraph` (plan executor) ≡ `DspPipeline` (frozen oracle).
//!
//! Drives both engines through an identical, deterministic scenario matrix
//! and compares their outputs sample-by-sample. The pipeline is the reference
//! and is never modified by this suite; the graph must reproduce it exactly
//! ([`Grade::Exact`]) — or within a tight epsilon ([`Grade::Epsilon`]) where
//! a documented rounding-order difference legitimately exists.
//!
//! Every case also asserts structural parity: identical total latency and an
//! identical node active-set (from `graph_nodes()`), so a plan that changes
//! stage order or activation fails here even if the samples happened to line
//! up.
//!
//! Exclusions (documented): crossfade-active configs (the graph has no
//! `TrackMixer` node yet) and the output domain (resampler / dither), which
//! the engine drives separately in both designs.

use config::{CompressorDetector, EngineConfig, PrecisionMode};
use engine::dsp::equalizer::{EqBandParams, EqFilterType};
use engine::dsp::loudness::{LoudnessMetadata, LoudnessMode};
use engine::dsp::{DspGraph, DspPipeline};

const SR: f32 = 48_000.0;
const TAU: f32 = std::f32::consts::TAU;

// ─────────────────────────────────────────────────────────────────────────────
// Scenario model
// ─────────────────────────────────────────────────────────────────────────────

// Every scenario must be bit-identical (including NaN payloads via
// `to_bits`). A scaled-epsilon grade was contemplated for documented
// rounding-order differences, but no scenario has needed it — the graph
// reproduces the pipeline exactly everywhere it is compared.

/// A command applied identically to both engines (between blocks).
#[derive(Debug, Clone)]
enum Cmd {
    Volume(f32),
    VolumeDb(f32),
    Balance(f32),
    EqEnabled(bool),
    EqAutoHeadroom(bool),
    EqPreampDb(f32),
    EqBand {
        index: usize,
        freq: f32,
        gain_db: f32,
        q: f32,
    },
    MidsideEq(bool),
    CrossfeedEnabled(bool),
    CrossfeedProfile(config::CrossfeedProfile),
    CrossfeedCustom(f32, f32, f32),
    StereoWidth(f32),
    CompressorEnabled(bool),
    CompressorBand {
        band: usize,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    },
    CompressorFeatures {
        band: usize,
        knee_db: f32,
        stereo_link: bool,
    },
    LimiterEnabled(bool),
    LimiterParams {
        ceiling_db: f32,
        attack_ms: f32,
        release_ms: f32,
        soft_clip: bool,
    },
    LimiterTruePeak(bool),
    Speed(f32),
    ConvolutionWet(f32),
    ConvolutionIr(Vec<(f32, f32)>),
    LoudnessMode(LoudnessMode),
    LoudnessMetadata(LoudnessMetadata),
    BitPerfect(bool),
    DoP(bool),
    Reset,
}

/// One row of the scenario matrix.
struct Case {
    name: &'static str,
    config: EngineConfig,
    setup: Vec<Cmd>,
    midstream: Vec<(usize, Cmd)>,
    block_len: usize,
    channels: usize,
    blocks: usize,
    overrun: bool,
}

impl Case {
    fn new(
        name: &'static str,
        config: EngineConfig,
        setup: Vec<Cmd>,
        block_len: usize,
        channels: usize,
        blocks: usize,
    ) -> Self {
        Self {
            name,
            config,
            setup,
            midstream: Vec::new(),
            block_len,
            channels,
            blocks,
            overrun: false,
        }
    }

    fn with_midstream(mut self, cmds: Vec<(usize, Cmd)>) -> Self {
        self.midstream = cmds;
        self
    }

    fn with_overrun(mut self) -> Self {
        self.overrun = true;
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Deterministic signal + symmetric command application
// ─────────────────────────────────────────────────────────────────────────────

#[inline]
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Deterministic multi-tone + seeded-noise sample. Identical for both
/// engines by construction (the same buffer is fed to each).
fn sample(state: &mut u64, t: f32, sr: f32) -> f32 {
    let tone = 0.30 * (TAU * 997.0 * t / sr).sin() + 0.18 * (TAU * 131.0 * t / sr + 0.7).cos();
    let noise = ((xorshift(state) >> 40) as f32 / (1u64 << 24) as f32) - 0.5;
    (tone + 0.05 * noise) * 0.9
}

fn fill_interleaved(buf: &mut [f32], channels: usize, frame_offset: usize, sr: f32) {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15u64.wrapping_add(frame_offset as u64 * 0x9E37_79B9);
    for (i, s) in buf.iter_mut().enumerate() {
        *s = sample(&mut state, (i / channels + frame_offset) as f32, sr);
    }
}

/// Apply one command to BOTH engines in the same arm — the two sides cannot
/// drift.
#[allow(clippy::too_many_arguments)]
fn apply_cmd(p: &mut DspPipeline, g: &mut DspGraph, cmd: &Cmd) {
    match cmd {
        Cmd::Volume(v) => {
            p.set_volume(*v);
            g.set_volume(*v);
        }
        Cmd::VolumeDb(db) => {
            p.set_volume_db(*db);
            g.set_volume_db(*db);
        }
        Cmd::Balance(b) => {
            p.set_balance(*b);
            g.set_balance(*b);
        }
        Cmd::EqEnabled(on) => {
            p.set_eq_enabled(*on);
            g.set_eq_enabled(*on);
        }
        Cmd::EqAutoHeadroom(on) => {
            p.set_eq_auto_headroom(*on);
            g.set_eq_auto_headroom(*on);
        }
        Cmd::EqPreampDb(db) => {
            p.set_preamp_db(*db);
            g.set_preamp_db(*db);
        }
        Cmd::EqBand {
            index,
            freq,
            gain_db,
            q,
        } => {
            let params = EqBandParams {
                enabled: true,
                filter_type: EqFilterType::Peaking,
                frequency: *freq,
                gain_db: *gain_db,
                q: *q,
            };
            p.set_eq_band(*index, params);
            g.set_eq_band(*index, params);
        }
        Cmd::MidsideEq(on) => {
            p.set_midside_eq(*on);
            g.set_midside_eq(*on);
        }
        Cmd::CrossfeedEnabled(on) => {
            p.set_crossfeed_enabled(*on);
            g.set_crossfeed_enabled(*on);
        }
        Cmd::CrossfeedProfile(profile) => {
            p.set_crossfeed_profile(*profile);
            g.set_crossfeed_profile(*profile);
        }
        Cmd::CrossfeedCustom(f, q, d) => {
            p.set_crossfeed_custom_params(*f, *q, *d);
            g.set_crossfeed_custom_params(*f, *q, *d);
        }
        Cmd::StereoWidth(w) => {
            p.set_stereo_width(*w);
            g.set_stereo_width(*w);
        }
        Cmd::CompressorEnabled(on) => {
            p.set_compressor_enabled(*on);
            g.set_compressor_enabled(*on);
        }
        Cmd::CompressorBand {
            band,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_gain_db,
        } => {
            p.set_compressor_band_params(
                *band,
                *threshold_db,
                *ratio,
                *attack_ms,
                *release_ms,
                *makeup_gain_db,
            );
            g.set_compressor_band_params(
                *band,
                *threshold_db,
                *ratio,
                *attack_ms,
                *release_ms,
                *makeup_gain_db,
            );
        }
        Cmd::CompressorFeatures {
            band,
            knee_db,
            stereo_link,
        } => {
            p.set_compressor_band_features(*band, *knee_db, CompressorDetector::Peak, *stereo_link);
            g.set_compressor_band_features(*band, *knee_db, CompressorDetector::Peak, *stereo_link);
        }
        Cmd::LimiterEnabled(on) => {
            p.set_limiter_enabled(*on);
            g.set_limiter_enabled(*on);
        }
        Cmd::LimiterParams {
            ceiling_db,
            attack_ms,
            release_ms,
            soft_clip,
        } => {
            p.set_limiter_params(2.0, *attack_ms, *release_ms, *ceiling_db, *soft_clip);
            g.set_limiter_params(2.0, *attack_ms, *release_ms, *ceiling_db, *soft_clip);
        }
        Cmd::LimiterTruePeak(on) => {
            p.set_limiter_true_peak(*on);
            g.set_limiter_true_peak(*on);
        }
        Cmd::Speed(s) => {
            p.timestretcher_mut().set_speed(*s);
            g.timestretch_mut().stretcher.set_speed(*s);
        }
        Cmd::ConvolutionWet(mix) => {
            p.set_convolution_wet_mix(*mix);
            g.set_convolution_wet_mix(*mix);
        }
        Cmd::ConvolutionIr(ir) => {
            p.convolution.set_enabled(true);
            p.convolution
                .load_ir_from_samples(ir)
                .expect("pipeline IR load");
            g.convolution_mut().engine.set_enabled(true);
            g.convolution_mut()
                .engine
                .load_ir_from_samples(ir)
                .expect("graph IR load");
        }
        Cmd::LoudnessMode(mode) => {
            p.set_loudness_mode(*mode);
            g.set_loudness_mode(*mode);
        }
        Cmd::LoudnessMetadata(meta) => {
            p.apply_loudness_metadata_outgoing(Some(*meta));
            g.apply_loudness_metadata_outgoing(Some(*meta));
        }
        Cmd::BitPerfect(on) => {
            p.set_bit_perfect(*on);
            g.set_bit_perfect(*on);
        }
        Cmd::DoP(on) => {
            p.set_dop_bypass(*on);
            g.set_dop_bypass(*on);
        }
        Cmd::Reset => {
            p.reset();
            g.reset();
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Driver + comparison
// ─────────────────────────────────────────────────────────────────────────────

/// Drive both engines through one case; returns interleaved outputs and the
/// structural parity verdict.
fn run_case(case: &Case) -> (Vec<f32>, Vec<f32>) {
    let mut p = DspPipeline::from_config(&case.config, SR);
    let mut g = DspGraph::from_config(&case.config, SR);

    if case.channels > 2 {
        let layout = engine::decode::ChannelLayout::from_count(case.channels);
        p.set_multichannel_layout(&layout);
        g.set_multichannel_layout(&layout);
    }
    for cmd in &case.setup {
        apply_cmd(&mut p, &mut g, cmd);
    }

    let mut out_p = Vec::new();
    let mut out_g = Vec::new();

    for b in 0..case.blocks {
        for (at, cmd) in &case.midstream {
            if *at == b {
                apply_cmd(&mut p, &mut g, cmd);
            }
        }
        let frames = if case.overrun {
            case.block_len * 3
        } else {
            case.block_len
        };

        if case.channels == 2 {
            // Stereo via process_block (plan `Normal`).
            let mut input = vec![0.0f32; frames * 2];
            fill_interleaved(&mut input, 2, b * case.block_len, SR);
            let mut in_l = Vec::with_capacity(frames);
            let mut in_r = Vec::with_capacity(frames);
            for pair in input.chunks_exact(2) {
                in_l.push(pair[0]);
                in_r.push(pair[1]);
            }
            let mut p_l = in_l.clone();
            let mut p_r = in_r.clone();
            let mut g_l = in_l;
            let mut g_r = in_r;
            p.process_block(&mut p_l, &mut p_r);
            g.process_block(&mut g_l, &mut g_r);
            out_p.extend_from_slice(&p_l);
            out_p.extend_from_slice(&p_r);
            out_g.extend_from_slice(&g_l);
            out_g.extend_from_slice(&g_r);
        } else {
            // Mono / multichannel via process_block_multichannel (plan
            // `NormalMc`; ≤2 channels delegate to the stereo path).
            let mut input = vec![0.0f32; frames * case.channels];
            fill_interleaved(&mut input, case.channels, b * case.block_len, SR);
            let mut g_in = input.clone();
            p.process_block_multichannel(&mut input, case.channels);
            g.process_block_multichannel(&mut g_in, case.channels);
            out_p.extend_from_slice(&input);
            out_g.extend_from_slice(&g_in);
        }
    }

    // Structural parity: per-node active set + latency must match, and the
    // aggregate latency must agree. The aggregate is summed from each side's
    // own `graph_nodes()` (both model bit-perfect/DoP bypass as zero) rather
    // than `latency_report()`, which the pipeline does not bypass-model.
    let pn = p.graph_nodes();
    let gn = g.graph_nodes();
    let p_total: f32 = pn.iter().map(|n| n.latency_ms).sum();
    let g_total: f32 = gn.iter().map(|n| n.latency_ms).sum();
    assert!(
        (p_total - g_total).abs() < 1e-3,
        "{}: total latency {p_total:.4} ms (pipeline) vs {g_total:.4} ms (graph)",
        case.name
    );
    assert_eq!(
        pn.len(),
        gn.len(),
        "{}: node-set length mismatch",
        case.name
    );
    for (a, b) in pn.iter().zip(&gn) {
        assert_eq!(
            a.name, b.name,
            "{}: node order mismatch: {} vs {}",
            case.name, a.name, b.name
        );
        assert_eq!(
            a.active, b.active,
            "{}: node {} active mismatch",
            case.name, a.name
        );
        assert!(
            (a.latency_ms - b.latency_ms).abs() < 1e-3,
            "{}: node {} latency {:.4} vs {:.4}",
            case.name,
            a.name,
            a.latency_ms,
            b.latency_ms
        );
        assert!(
            (a.tail_ms - b.tail_ms).abs() < 1e-3,
            "{}: node {} tail {:.4} vs {:.4}",
            case.name,
            a.name,
            a.tail_ms,
            b.tail_ms
        );
    }

    (out_p, out_g)
}

fn compare(case: &Case, ref_out: &[f32], got: &[f32]) {
    let label = case.name;
    assert_eq!(
        ref_out.len(),
        got.len(),
        "{label}: output length mismatch ({} vs {})",
        ref_out.len(),
        got.len()
    );
    for (i, (a, b)) in ref_out.iter().zip(got).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "{label}: sample {i} differs exactly: {a} (pipeline) vs {b} (graph)"
        );
    }
}

fn check_case(case: &Case) {
    let (ref_out, got) = run_case(case);
    compare(case, &ref_out, &got);
}

// ─────────────────────────────────────────────────────────────────────────────
// Config helpers
// ─────────────────────────────────────────────────────────────────────────────

fn cfg_default() -> EngineConfig {
    EngineConfig::default()
}

fn cfg_quality() -> EngineConfig {
    EngineConfig {
        precision_mode: PrecisionMode::Quality,
        ..EngineConfig::default()
    }
}

/// Every DSP stage enabled with real (non-trivial) parameters.
fn cfg_all_stages() -> EngineConfig {
    let mut c = EngineConfig::default();
    c.eq.enabled = true;
    if c.eq.bands.len() >= 5 {
        c.eq.bands[0].gain_db = 3.0;
        c.eq.bands[2].gain_db = -2.0;
        c.eq.bands[4].gain_db = 1.5;
    }
    c.limiter.enabled = true;
    c.limiter.lookahead_ms = 2.0;
    c.multiband_compressor.enabled = true;
    c.crossfeed.enabled = true;
    c.stereo_enhancer.enabled = true;
    c.stereo_enhancer.width = 1.3;
    c
}

fn synthetic_ir() -> Vec<(f32, f32)> {
    (0..512)
        .map(|i| {
            let e = (-i as f32 / 128.0).exp() * 0.5;
            (e, e * 0.9)
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario matrix
// ─────────────────────────────────────────────────────────────────────────────

fn cases() -> Vec<Case> {
    let mut v = Vec::with_capacity(21);

    // 1–2. Default config: nothing dynamic enabled → must be bit-exact.
    v.push(Case::new(
        "default_stereo_f32",
        cfg_default(),
        vec![],
        256,
        2,
        16,
    ));
    v.push(Case::new(
        "default_stereo_f64",
        cfg_quality(),
        vec![],
        256,
        2,
        16,
    ));

    // 3–4. Full chain (f32 / f64).
    v.push(Case::new(
        "all_stages_stereo_f32",
        cfg_all_stages(),
        vec![Cmd::Volume(0.8), Cmd::Balance(-0.25)],
        256,
        2,
        16,
    ));
    v.push(Case::new(
        "all_stages_stereo_f64",
        EngineConfig {
            precision_mode: PrecisionMode::Quality,
            ..cfg_all_stages()
        },
        vec![Cmd::Volume(0.8), Cmd::Balance(-0.25)],
        256,
        2,
        16,
    ));

    // 5. Per-frame blocks (stateful stages, one frame at a time).
    v.push(Case::new(
        "all_stages_block_len_1",
        cfg_all_stages(),
        vec![Cmd::Volume(0.8)],
        1,
        2,
        4096,
    ));

    // 6. Block at the internal maximum.
    v.push(Case::new(
        "all_stages_block_max",
        cfg_all_stages(),
        vec![Cmd::Volume(0.8)],
        engine::buffer::MAX_AUDIO_BLOCK_FRAMES,
        2,
        4,
    ));

    // 7. Oversized caller buffer (internal sub-block splitting path).
    v.push(
        Case::new(
            "all_stages_overrun_buffer",
            cfg_all_stages(),
            vec![Cmd::Volume(0.8)],
            engine::buffer::MAX_AUDIO_BLOCK_FRAMES,
            2,
            4,
        )
        .with_overrun(),
    );

    // 8. Mid-stream control changes (gain ramps, EQ retune, balance moves).
    v.push(
        Case::new(
            "midstream_control_changes",
            cfg_all_stages(),
            vec![Cmd::Volume(0.9)],
            256,
            2,
            16,
        )
        .with_midstream(vec![
            (1, Cmd::Volume(0.4)),
            (2, Cmd::Balance(0.3)),
            (3, Cmd::VolumeDb(-6.0)),
            (
                4,
                Cmd::EqBand {
                    index: 2,
                    freq: 1200.0,
                    gain_db: -4.0,
                    q: 1.4,
                },
            ),
            (5, Cmd::Speed(1.5)),
            (6, Cmd::StereoWidth(1.6)),
            (7, Cmd::Reset),
        ]),
    );

    // 9. Bit-perfect: pure passthrough (stereo).
    v.push(Case::new(
        "bit_perfect_stereo",
        cfg_default(),
        vec![Cmd::Volume(0.5), Cmd::BitPerfect(true)],
        256,
        2,
        8,
    ));

    // 10. Bit-perfect on the multichannel path: also pure passthrough (the
    // entry points return before any stage, exactly like the pipeline).
    v.push(Case::new(
        "bit_perfect_multichannel",
        cfg_all_stages(),
        vec![Cmd::Volume(0.5), Cmd::BitPerfect(true)],
        256,
        6,
        8,
    ));

    // 11. DoP bypass: pure passthrough.
    v.push(Case::new(
        "dop_bypass_stereo",
        cfg_all_stages(),
        vec![Cmd::DoP(true)],
        256,
        2,
        8,
    ));

    // 12. Loudness normalization with injected metadata.
    v.push(Case::new(
        "loudness_ebu_r128",
        EngineConfig {
            loudness: config::LoudnessConfig {
                mode: config::LoudnessMode::EbuR128,
                target_lufs: -14.0,
                ..Default::default()
            },
            ..EngineConfig::default()
        },
        vec![Cmd::LoudnessMetadata(LoudnessMetadata {
            ebu_r128_loudness: Some(-20.0),
            ..Default::default()
        })],
        256,
        2,
        16,
    ));

    // 13. Convolution with the same synthetic IR injected into both.
    v.push(Case::new(
        "convolution_synthetic_ir",
        cfg_default(),
        vec![Cmd::ConvolutionIr(synthetic_ir()), Cmd::ConvolutionWet(0.5)],
        256,
        2,
        16,
    ));

    // 14. Mono through the multichannel entry (≤2ch delegates to stereo).
    v.push(Case::new(
        "mono_via_mc_entry",
        cfg_all_stages(),
        vec![Cmd::Volume(0.8)],
        256,
        1,
        16,
    ));

    // 15. 5.1 multichannel with routing/trim configured.
    let mut mc_cfg = cfg_all_stages();
    mc_cfg.channel_trim.enabled = true;
    mc_cfg.channel_trim.entries = vec![config::ChannelTrimEntry {
        channel: 0,
        gain_db: -3.0,
        ..Default::default()
    }];
    v.push(Case::new(
        "multichannel_5_1_trim",
        mc_cfg,
        vec![Cmd::Volume(0.8)],
        256,
        6,
        16,
    ));

    // 16. 7.1 multichannel, f64 config (MC runs f32 in both — same parity).
    v.push(Case::new(
        "multichannel_7_1",
        cfg_all_stages(),
        vec![Cmd::Volume(0.8), Cmd::Balance(-0.3)],
        256,
        8,
        16,
    ));

    // 17. EQ control surface: preamp, auto-headroom, mid/side processing,
    // and live enable toggles.
    v.push(
        Case::new(
            "eq_control_surface",
            cfg_all_stages(),
            vec![
                Cmd::EqPreampDb(-2.5),
                Cmd::EqAutoHeadroom(true),
                Cmd::MidsideEq(true),
            ],
            256,
            2,
            16,
        )
        .with_midstream(vec![
            (2, Cmd::EqEnabled(false)),
            (3, Cmd::EqEnabled(true)),
            (4, Cmd::EqAutoHeadroom(false)),
        ]),
    );

    // 18. Crossfeed control surface: enable, profile presets, custom params,
    // and live disable.
    v.push(
        Case::new(
            "crossfeed_control_surface",
            cfg_default(),
            vec![
                Cmd::CrossfeedEnabled(true),
                Cmd::CrossfeedProfile(config::CrossfeedProfile::ChuMoy),
            ],
            256,
            2,
            16,
        )
        .with_midstream(vec![
            (2, Cmd::CrossfeedProfile(config::CrossfeedProfile::Custom)),
            (3, Cmd::CrossfeedCustom(650.0, 0.6, 0.45)),
            (4, Cmd::CrossfeedEnabled(false)),
            (5, Cmd::CrossfeedEnabled(true)),
        ]),
    );

    // 19. Multiband compressor control surface: enable, per-band params,
    // knee/stereo-link features, live changes.
    v.push(
        Case::new(
            "compressor_control_surface",
            cfg_default(),
            vec![
                Cmd::CompressorEnabled(true),
                Cmd::CompressorBand {
                    band: 0,
                    threshold_db: -30.0,
                    ratio: 3.0,
                    attack_ms: 5.0,
                    release_ms: 120.0,
                    makeup_gain_db: 2.0,
                },
                Cmd::CompressorBand {
                    band: 2,
                    threshold_db: -25.0,
                    ratio: 2.5,
                    attack_ms: 2.0,
                    release_ms: 90.0,
                    makeup_gain_db: 1.0,
                },
            ],
            256,
            2,
            16,
        )
        .with_midstream(vec![
            (
                2,
                Cmd::CompressorFeatures {
                    band: 1,
                    knee_db: 6.0,
                    stereo_link: false,
                },
            ),
            (
                3,
                Cmd::CompressorBand {
                    band: 1,
                    threshold_db: -20.0,
                    ratio: 4.0,
                    attack_ms: 1.0,
                    release_ms: 200.0,
                    makeup_gain_db: 0.0,
                },
            ),
            (4, Cmd::CompressorEnabled(false)),
            (5, Cmd::CompressorEnabled(true)),
        ]),
    );

    // 20. Limiter control surface: params, true-peak mode, live toggles.
    v.push(
        Case::new(
            "limiter_control_surface",
            cfg_all_stages(),
            vec![
                Cmd::LimiterParams {
                    ceiling_db: -1.0,
                    attack_ms: 1.0,
                    release_ms: 80.0,
                    soft_clip: true,
                },
                Cmd::LimiterTruePeak(true),
            ],
            256,
            2,
            16,
        )
        .with_midstream(vec![
            (3, Cmd::LimiterEnabled(false)),
            (4, Cmd::LimiterEnabled(true)),
            (5, Cmd::LimiterTruePeak(false)),
        ]),
    );

    // 21. Loudness mode switch via the control surface (Track ReplayGain
    // metadata path), distinct from case 12's config-selected EBU R128.
    v.push(Case::new(
        "loudness_track_replaygain",
        cfg_default(),
        vec![
            Cmd::LoudnessMode(engine::dsp::LoudnessMode::TrackReplayGain),
            Cmd::LoudnessMetadata(LoudnessMetadata {
                replaygain_track_db: Some(-7.5),
                replaygain_track_peak: Some(0.93),
                ..Default::default()
            }),
        ],
        256,
        2,
        16,
    ));

    v
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn graph_matches_pipeline_across_scenario_matrix() {
    for case in cases() {
        check_case(&case);
    }
}

#[test]
fn graph_plan_block_throughput_within_tolerance_of_pipeline() {
    // Fixed-iteration wall-clock gate (generous bound so shared CI runners do
    // not flake): the enum-dispatch plan executor must not be dramatically
    // slower than the direct-call pipeline. Run in one process, alternating,
    // after a warm-up pass. (Criterion's statistical CI is in
    // benches/graph_plan_bench.rs for reporting.)
    let cfg = cfg_all_stages();
    let mut p = DspPipeline::from_config(&cfg, SR);
    let mut g = DspGraph::from_config(&cfg, SR);
    p.set_volume(0.8);
    g.set_volume(0.8);

    const BLOCK: usize = 256;
    const ITERS: usize = 2_000;
    let mut left = vec![0.3f32; BLOCK];
    let mut right = vec![-0.2f32; BLOCK];
    // Warm up both (stateful stages, code cache).
    for _ in 0..100 {
        p.process_block(&mut left, &mut right);
        g.process_block(&mut left, &mut right);
    }

    let t0 = std::time::Instant::now();
    for _ in 0..ITERS {
        p.process_block(&mut left, &mut right);
    }
    let pipeline_elapsed = t0.elapsed();

    let t1 = std::time::Instant::now();
    for _ in 0..ITERS {
        g.process_block(&mut left, &mut right);
    }
    let graph_elapsed = t1.elapsed();

    let ratio = graph_elapsed.as_secs_f64() / pipeline_elapsed.as_secs_f64().max(1e-9);
    eprintln!("pipeline {pipeline_elapsed:?} vs graph {graph_elapsed:?} ({ratio:.2}×)");
    assert!(
        ratio < 1.5,
        "graph plan executor is {ratio:.2}× slower than the pipeline — the enum \
         dispatch regressed the hot path"
    );
}
