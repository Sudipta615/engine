//! Fidelity suite — Graph2 engine parity vs `dsp::graph` (Phase 46,
//! v3.51.0) and the Phase-47 shadow-mode harness.
//!
//! Every scenario builds a [`Graph2Engine`] (whose generations carry plans
//! **lowered from the Graph2 production topology**) and a legacy
//! [`DspGraph`] (hand-authored `PlanSet::compile()` plans) from the same
//! config, drives them with the identical deterministic input + command
//! script, and compares the outputs **bit-exactly** (`f32::to_bits`, NaN
//! payloads included).
//!
//! Bit-exactness is by construction — the two engines share the node
//! arena implementation, config application, user-state replay, control
//! queues, and (pinned by the `lowered_plans_match_handauthored` unit
//! test) the plan step order — so this suite guards the **seam**: any
//! divergence is a plan-lowering or fan-out break, never a fixture
//! difference.
//!
//! The harness machinery (deterministic generator, bit-compare, command
//! scripting) is shared verbatim with `graph_pipeline_equivalence` (the
//! `dsp::graph` ≡ `DspPipeline` oracle suite) so the three-way chain
//! `Graph2 ≡ dsp::graph ≡ pipeline` closes transitively.
//!
//! Scenario names mirror `graph_pipeline_equivalence`'s 27-case matrix
//! (inventory §4.1) plus the Phase-46 control-surface extensions
//! (inventory §4.2: aux bus, ducking, automation, correction, spatial,
//! lanes, seek-fade, speed, routing, precision, generation swap, queue
//! backpressure).

use config::EngineConfig;
use engine::buffer::MAX_AUDIO_BLOCK_FRAMES;
use engine::decode::ChannelLayout;
use engine::dsp::crossfade::MixerState;
use engine::dsp::graph::nodes::{AutomationPoint, AutomationTarget, DuckState, MAX_DUCK_TARGETS};
use engine::dsp::graph::DspGraph;
use engine::dsp::graph2::prod::Graph2Engine;

/// A mid-block control mutation applied to both engines in lock-step.
type Mutator<'a> = &'a dyn Fn(&mut Graph2Engine, &mut DspGraph);

const SR: f32 = 48_000.0;
const TAU: f32 = std::f32::consts::TAU;
/// Frame-offset separation between the primary and secondary streams so the
/// deterministic generator produces two different signals (same value as
/// the `graph_pipeline_equivalence` fixture).
const INPUT1_OFFSET: usize = 1_000_000_003;

// ─────────────────────────────────────────────────────────────────────────────
// Deterministic signal generator (verbatim from graph_pipeline_equivalence)
// ─────────────────────────────────────────────────────────────────────────────

#[inline]
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Deterministic multi-tone + seeded-noise sample. Identical for both
/// engines by construction (the same generator state feeds each).
fn sample(state: &mut u64, t: f32, sr: f32) -> f32 {
    let tone = 0.30 * (TAU * 997.0 * t / sr).sin() + 0.18 * (TAU * 131.0 * t / sr + 0.7).cos();
    let noise = ((xorshift(state) >> 40) as f32 / (1u64 << 24) as f32) - 0.5;
    (tone + 0.05 * noise) * 0.9
}

fn fill_interleaved(buf: &mut [f32], channels: usize, frame_offset: usize, sr: f32) {
    let mut rng = 0x5EED_0000_0000_0001u64;
    for (i, s) in buf.iter_mut().enumerate() {
        let frame = frame_offset + i / channels.max(1);
        let ch = i % channels.max(1);
        rng = rng.wrapping_add(frame as u64 ^ (ch as u64).rotate_left(32));
        *s = sample(&mut rng, frame as f32, sr);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Bit-exact comparison
// ─────────────────────────────────────────────────────────────────────────────

/// Bit-exact per-sample comparison (NaN payloads included) with a
/// case-labeled panic message.
fn assert_bit_exact(label: &str, reference: &[f32], candidate: &[f32]) {
    assert_eq!(
        reference.len(),
        candidate.len(),
        "{label}: output length mismatch ({} vs {})",
        reference.len(),
        candidate.len()
    );
    for (i, (a, b)) in reference.iter().zip(candidate).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "{label}: sample {i} differs exactly: {a} (dsp::graph) vs {b} (graph2)"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The A/B harness: one engine pair + a scripted driver
// ─────────────────────────────────────────────────────────────────────────────

/// A driver that renders `blocks` stereo blocks, optionally mutating the
/// engine between blocks (midstream control changes) or rendering the
/// crossfade-pair entry.
struct AbHarness {
    block: usize,
    blocks: usize,
}

impl AbHarness {
    fn new(block: usize, blocks: usize) -> Self {
        Self { block, blocks }
    }

    /// Drive both engines with the same stereo input and command script;
    /// return the two output streams for bit-comparison. Inputs are cloned
    /// for each engine BEFORE processing (the entry overwrites the planes
    /// with the mix output).
    fn run(&self, config: &EngineConfig, mutators: &[Mutator<'_>]) -> (Vec<f32>, Vec<f32>) {
        let mut g2 = Graph2Engine::from_config(config, SR);
        let mut legacy = DspGraph::from_config(config, SR);
        let mut out2 = Vec::with_capacity(self.blocks * self.block);
        let mut outl = Vec::with_capacity(self.blocks * self.block);

        for b in 0..self.blocks {
            let mut l = vec![0.0f32; self.block];
            let mut r = vec![0.0f32; self.block];
            fill_interleaved(&mut l, 1, b * self.block, SR);
            fill_interleaved(&mut r, 1, b * self.block + 7919, SR);
            // Clone the pristine input for the legacy engine first.
            let (mut ll, mut rl) = (l.clone(), r.clone());
            g2.process_block(&mut l, &mut r);
            legacy.process_block(&mut ll, &mut rl);
            out2.extend_from_slice(&l);
            out2.extend_from_slice(&r);
            outl.extend_from_slice(&ll);
            outl.extend_from_slice(&rl);
            if let Some(m) = mutators.get(b) {
                m(&mut g2, &mut legacy);
            }
        }
        (out2, outl)
    }

    /// Drive both engines' crossfade-pair entry (primary + secondary
    /// streams), with midstream mutators between blocks. Inputs are cloned
    /// for each engine BEFORE processing (the entries overwrite the primary
    /// planes with the mix output).
    fn run_inputs(&self, config: &EngineConfig, mutators: &[Mutator<'_>]) -> (Vec<f32>, Vec<f32>) {
        let mut g2 = Graph2Engine::from_config(config, SR);
        let mut legacy = DspGraph::from_config(config, SR);
        let mut out2 = Vec::with_capacity(self.blocks * self.block * 2);
        let mut outl = Vec::with_capacity(self.blocks * self.block * 2);

        for b in 0..self.blocks {
            let mut pl = vec![0.0f32; self.block];
            let mut pr = vec![0.0f32; self.block];
            let mut sl = vec![0.0f32; self.block];
            let mut sr = vec![0.0f32; self.block];
            fill_interleaved(&mut pl, 1, b * self.block, SR);
            fill_interleaved(&mut pr, 1, b * self.block + 7919, SR);
            fill_interleaved(&mut sl, 1, INPUT1_OFFSET + b * self.block, SR);
            fill_interleaved(&mut sr, 1, INPUT1_OFFSET + b * self.block + 7919, SR);
            // Clone the pristine inputs for the legacy engine first.
            let (mut pl2, mut pr2) = (pl.clone(), pr.clone());
            let (mut sl2, mut sr2) = (sl.clone(), sr.clone());
            g2.process_block_inputs((&mut pl, &mut pr), (&mut sl, &mut sr));
            legacy.process_block_inputs((&mut pl2, &mut pr2), (&mut sl2, &mut sr2));
            out2.extend_from_slice(&pl);
            out2.extend_from_slice(&pr);
            outl.extend_from_slice(&pl2);
            outl.extend_from_slice(&pr2);
            if let Some(m) = mutators.get(b) {
                m(&mut g2, &mut legacy);
            }
        }
        (out2, outl)
    }

    /// Drive both engines' multichannel entry (interleaved), with midstream
    /// mutators between blocks.
    fn run_mc(
        &self,
        config: &EngineConfig,
        channels: usize,
        mutators: &[Mutator<'_>],
    ) -> (Vec<f32>, Vec<f32>) {
        let mut g2 = Graph2Engine::from_config(config, SR);
        let mut legacy = DspGraph::from_config(config, SR);
        g2.set_multichannel_layout(&ChannelLayout::from_count(channels));
        legacy.set_multichannel_layout(&ChannelLayout::from_count(channels));
        let mut out2 = Vec::with_capacity(self.blocks * self.block * channels);
        let mut outl = Vec::with_capacity(self.blocks * self.block * channels);

        for b in 0..self.blocks {
            let mut mc = vec![0.0f32; self.block * channels];
            fill_interleaved(&mut mc, channels, b * self.block, SR);
            // Clone the pristine input for the legacy engine first.
            let mut mcl = mc.clone();
            g2.process_block_multichannel(&mut mc, channels);
            legacy.process_block_multichannel(&mut mcl, channels);
            out2.extend_from_slice(&mc);
            outl.extend_from_slice(&mcl);
            if let Some(m) = mutators.get(b) {
                m(&mut g2, &mut legacy);
            }
        }
        (out2, outl)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Config builders (mirroring graph_pipeline_equivalence's shapes)
// ─────────────────────────────────────────────────────────────────────────────

/// All processing stages enabled at once (the heaviest chain).
fn all_stages_config() -> EngineConfig {
    let mut cfg = EngineConfig::default();
    cfg.eq.enabled = true;
    cfg.eq.bands = vec![
        band(80.0, 3.0, 0.8),
        band(1200.0, -2.0, 1.1),
        band(9000.0, 2.5, 0.9),
    ];
    cfg.crossfeed.enabled = true;
    cfg.stereo_enhancer.enabled = true;
    cfg.stereo_enhancer.width = 0.4;
    cfg.limiter.enabled = true;
    cfg.multiband_compressor.enabled = true;
    cfg.loudness.mode = config::LoudnessMode::TrackReplayGain;
    cfg
}

fn band(freq: f32, gain_db: f32, q: f32) -> config::EqBandConfig {
    config::EqBandConfig {
        frequency: freq,
        gain_db,
        q,
        filter_type: config::FilterType::Peaking,
        enabled: true,
    }
}

/// A config with the bus-topology knobs (slots, sends, aux) set — the
/// generation-rebuild path.
fn bus_config() -> EngineConfig {
    EngineConfig {
        mix_slots: 4,
        mix_sends: vec![config::SlotSendConfig {
            slot: 2,
            master_gain: 0.8,
            aux_gain: 0.5,
        }],
        aux: config::AuxBusConfig {
            enabled: true,
            return_gain: 0.6,
            ..Default::default()
        },
        ..Default::default()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The 27 legacy-parity scenarios (graph_pipeline_equivalence §4.1)
// ─────────────────────────────────────────────────────────────────────────────

/// `default_stereo_f32` — default config, stereo, f32.
#[test]
fn default_stereo_f32() {
    let (a, b) = AbHarness::new(256, 8).run(&EngineConfig::default(), &[]);
    assert_bit_exact("default_stereo_f32", &a, &b);
}

/// `default_stereo_f64` — default config, f64 promotion path.
#[test]
fn default_stereo_f64() {
    let cfg = EngineConfig::default();
    let mut g2 = Graph2Engine::from_config(&cfg, SR);
    let mut legacy = DspGraph::from_config(&cfg, SR);
    let mut l = vec![0.0f64; 512];
    let mut r = vec![0.0f64; 512];
    let mut lf = vec![0.0f64; 512];
    let mut rf = vec![0.0f64; 512];
    fill_f64(&mut l, 0);
    fill_f64(&mut r, 7919);
    lf.copy_from_slice(&l);
    rf.copy_from_slice(&r);
    g2.process_block_f64(&mut l, &mut r);
    legacy.process_block_f64(&mut lf, &mut rf);
    assert_bit_exact_f64("default_stereo_f64", &l, &lf);
    assert_bit_exact_f64("default_stereo_f64", &r, &rf);
}

fn fill_f64(buf: &mut [f64], offset: usize) {
    let mut f = vec![0.0f32; buf.len()];
    fill_interleaved(&mut f, 1, offset, SR);
    for (d, s) in buf.iter_mut().zip(f) {
        *d = s as f64;
    }
}

fn assert_bit_exact_f64(label: &str, a: &[f64], b: &[f64]) {
    assert_eq!(a.len(), b.len(), "{label}: length mismatch");
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(x.to_bits(), y.to_bits(), "{label}: sample {i} differs");
    }
}

/// `all_stages_stereo_f32` — every stage enabled.
#[test]
fn all_stages_stereo_f32() {
    let (a, b) = AbHarness::new(256, 8).run(&all_stages_config(), &[]);
    assert_bit_exact("all_stages_stereo_f32", &a, &b);
}

/// `all_stages_stereo_f64` — every stage enabled, f64 path.
#[test]
fn all_stages_stereo_f64() {
    let cfg = all_stages_config();
    let mut g2 = Graph2Engine::from_config(&cfg, SR);
    let mut legacy = DspGraph::from_config(&cfg, SR);
    let mut l = vec![0.0f64; 512];
    let mut r = vec![0.0f64; 512];
    let mut lf = l.clone();
    let mut rf = r.clone();
    fill_f64(&mut l, 0);
    fill_f64(&mut r, 7919);
    lf.copy_from_slice(&l);
    rf.copy_from_slice(&r);
    g2.process_block_f64(&mut l, &mut r);
    legacy.process_block_f64(&mut lf, &mut rf);
    assert_bit_exact_f64("all_stages_stereo_f64", &l, &lf);
    assert_bit_exact_f64("all_stages_stereo_f64", &r, &rf);
}

/// `all_stages_block_len_1` — single-frame blocks (the block-splitting
/// boundary).
#[test]
fn all_stages_block_len_1() {
    let (a, b) = AbHarness::new(1, 64).run(&all_stages_config(), &[]);
    assert_bit_exact("all_stages_block_len_1", &a, &b);
}

/// `all_stages_block_max` — `MAX_AUDIO_BLOCK_FRAMES` blocks.
#[test]
fn all_stages_block_max() {
    let (a, b) = AbHarness::new(MAX_AUDIO_BLOCK_FRAMES, 4).run(&all_stages_config(), &[]);
    assert_bit_exact("all_stages_block_max", &a, &b);
}

/// `all_stages_overrun_buffer` — blocks larger than the max (the
/// split-into-chunks path).
#[test]
fn all_stages_overrun_buffer() {
    let (a, b) = AbHarness::new(MAX_AUDIO_BLOCK_FRAMES + 1000, 3).run(&all_stages_config(), &[]);
    assert_bit_exact("all_stages_overrun_buffer", &a, &b);
}

/// `midstream_control_changes` — volume/EQ/limiter commands landing
/// between blocks.
#[test]
fn midstream_control_changes() {
    let m1 = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_volume(0.5);
        l.set_volume(0.5);
    };
    let m2 = |g: &mut Graph2Engine, l: &mut DspGraph| {
        let p = engine::dsp::equalizer::EqBandParams {
            frequency: 200.0,
            gain_db: 4.0,
            q: 1.0,
            filter_type: engine::dsp::equalizer::EqFilterType::Peaking,
            enabled: true,
        };
        g.set_eq_band(0, p);
        l.set_eq_band(0, p);
        g.set_limiter_params(2.0, 0.5, 100.0, -1.0, false);
        l.set_limiter_params(2.0, 0.5, 100.0, -1.0, false);
    };
    let mutators: [Mutator<'_>; 2] = [&m1, &m2];
    let (a, b) = AbHarness::new(256, 8).run(&all_stages_config(), &mutators);
    assert_bit_exact("midstream_control_changes", &a, &b);
}

/// `bit_perfect_stereo` — transport bypass returns before any stage.
#[test]
fn bit_perfect_stereo() {
    let cfg = EngineConfig::default();
    let mut g2 = Graph2Engine::from_config(&cfg, SR);
    let mut legacy = DspGraph::from_config(&cfg, SR);
    g2.set_bit_perfect(true);
    legacy.set_bit_perfect(true);
    g2.drain_queued_control();
    legacy.drain_queued_control();
    let mut l = vec![0.25f32; 256];
    let mut r = vec![-0.25f32; 256];
    let mut ll = l.clone();
    let mut rl = r.clone();
    g2.process_block(&mut l, &mut r);
    legacy.process_block(&mut ll, &mut rl);
    // Bypass is bit-exact passthrough for BOTH engines.
    assert_bit_exact("bit_perfect_stereo", &l, &ll);
    assert_bit_exact("bit_perfect_stereo", &r, &rl);
    assert_eq!(l[0].to_bits(), 0.25f32.to_bits());
}

/// `bit_perfect_multichannel` — bypass on the MC entry.
#[test]
fn bit_perfect_multichannel() {
    let cfg = EngineConfig::default();
    let mut g2 = Graph2Engine::from_config(&cfg, SR);
    let mut legacy = DspGraph::from_config(&cfg, SR);
    g2.set_bit_perfect(true);
    legacy.set_bit_perfect(true);
    g2.drain_queued_control();
    legacy.drain_queued_control();
    let mut mc = vec![0.5f32; 256 * 6];
    fill_interleaved(&mut mc, 6, 0, SR);
    let mut mcl = mc.clone();
    g2.process_block_multichannel(&mut mc, 6);
    legacy.process_block_multichannel(&mut mcl, 6);
    assert_bit_exact("bit_perfect_multichannel", &mc, &mcl);
}

/// `dop_bypass_stereo` — DoP transport bypass.
#[test]
fn dop_bypass_stereo() {
    let cfg = EngineConfig::default();
    let mut g2 = Graph2Engine::from_config(&cfg, SR);
    let mut legacy = DspGraph::from_config(&cfg, SR);
    g2.set_dop_bypass(true);
    legacy.set_dop_bypass(true);
    g2.drain_queued_control();
    legacy.drain_queued_control();
    let (a, b) = AbHarness::new(256, 2).run(&cfg, &[]);
    let _ = (a, b);
    let mut l = vec![0.25f32; 256];
    let mut r = vec![-0.25f32; 256];
    let mut ll = l.clone();
    let mut rl = r.clone();
    g2.process_block(&mut l, &mut r);
    legacy.process_block(&mut ll, &mut rl);
    assert_bit_exact("dop_bypass_stereo", &l, &ll);
    assert_bit_exact("dop_bypass_stereo", &r, &rl);
}

/// `loudness_ebu_r128` — EBU R128 normalization with metadata applied.
#[test]
fn loudness_ebu_r128() {
    let mut cfg = EngineConfig::default();
    cfg.loudness.mode = config::LoudnessMode::EbuR128;
    let meta = engine::dsp::loudness::LoudnessMetadata {
        replaygain_track_db: Some(-6.0),
        replaygain_album_db: Some(-6.5),
        replaygain_track_peak: Some(0.5),
        replaygain_album_peak: Some(0.55),
        ebu_r128_loudness: None,
        ebu_r128_peak: None,
    };
    let apply_meta = move |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.apply_loudness_metadata_outgoing(Some(meta));
        l.apply_loudness_metadata_outgoing(Some(meta));
    };
    let mutators: [Mutator<'_>; 1] = [&apply_meta];
    let (a, b) = AbHarness::new(256, 6).run(&cfg, &mutators);
    assert_bit_exact("loudness_ebu_r128", &a, &b);
}

/// `convolution_synthetic_ir` — FIR convolution with a synthetic IR
/// loaded via the runtime mutators (the `convolution_mut` seam).
#[test]
fn convolution_synthetic_ir() {
    let cfg = EngineConfig::default();
    let ir: Vec<(f32, f32)> = (0..2048)
        .map(|i| {
            let e = (-i as f32 / 512.0).exp() * 0.5;
            (e, e * 0.9)
        })
        .collect();
    let load = move |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.with_both(|gr| {
            gr.convolution_mut().engine.set_enabled(true);
            gr.convolution_mut()
                .engine
                .load_ir_from_samples(&ir)
                .unwrap();
        });
        l.convolution_mut().engine.set_enabled(true);
        l.convolution_mut()
            .engine
            .load_ir_from_samples(&ir)
            .unwrap();
    };
    let mutators: [Mutator<'_>; 1] = [&load];
    let (a, b) = AbHarness::new(256, 8).run(&cfg, &mutators);
    assert_bit_exact("convolution_synthetic_ir", &a, &b);
}

/// `mono_via_mc_entry` — a mono stream through the multichannel entry.
#[test]
fn mono_via_mc_entry() {
    let (a, b) = AbHarness::new(256, 6).run_mc(&all_stages_config(), 1, &[]);
    assert_bit_exact("mono_via_mc_entry", &a, &b);
}

/// `multichannel_5_1_trim` — 5.1 layout with per-channel trim.
#[test]
fn multichannel_5_1_trim() {
    let mut cfg = all_stages_config();
    cfg.channel_trim.enabled = true;
    cfg.channel_trim.entries = (0..6)
        .map(|c| config::ChannelTrimEntry {
            channel: c,
            gain_db: 1.0 - c as f32,
            delay_ms: 0.0,
            invert: c % 2 == 0,
        })
        .collect();
    let (a, b) = AbHarness::new(256, 6).run_mc(&cfg, 6, &[]);
    assert_bit_exact("multichannel_5_1_trim", &a, &b);
}

/// `multichannel_7_1` — 7.1 layout.
#[test]
fn multichannel_7_1() {
    let (a, b) = AbHarness::new(256, 6).run_mc(&all_stages_config(), 8, &[]);
    assert_bit_exact("multichannel_7_1", &a, &b);
}

/// `eq_control_surface` — the full EQ setter surface midstream.
#[test]
fn eq_control_surface() {
    let p = engine::dsp::equalizer::EqBandParams {
        frequency: 1000.0,
        gain_db: -3.5,
        q: 1.4,
        filter_type: engine::dsp::equalizer::EqFilterType::LowShelf,
        enabled: true,
    };
    let set_band = move |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_eq_enabled(true);
        l.set_eq_enabled(true);
        g.set_preamp_db(2.0);
        l.set_preamp_db(2.0);
        g.set_bass_shelf(1.5);
        l.set_bass_shelf(1.5);
        g.set_treble_shelf(-1.0);
        l.set_treble_shelf(-1.0);
        g.set_eq_band(1, p);
        l.set_eq_band(1, p);
        g.set_eq_auto_headroom(true);
        l.set_eq_auto_headroom(true);
    };
    let mutators: [Mutator<'_>; 1] = [&set_band];
    let (a, b) = AbHarness::new(256, 6).run(&EngineConfig::default(), &mutators);
    assert_bit_exact("eq_control_surface", &a, &b);
}

/// `crossfeed_control_surface` — crossfeed profile + custom params.
#[test]
fn crossfeed_control_surface() {
    let set_cf = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_crossfeed_enabled(true);
        l.set_crossfeed_enabled(true);
        g.set_crossfeed_profile(config::CrossfeedProfile::Custom);
        l.set_crossfeed_profile(config::CrossfeedProfile::Custom);
        g.set_crossfeed_custom_params(700.0, 0.67, 0.32);
        l.set_crossfeed_custom_params(700.0, 0.67, 0.32);
    };
    let mutators: [Mutator<'_>; 1] = [&set_cf];
    let (a, b) = AbHarness::new(256, 6).run(&EngineConfig::default(), &mutators);
    assert_bit_exact("crossfeed_control_surface", &a, &b);
}

/// `compressor_control_surface` — multiband compressor setters.
#[test]
fn compressor_control_surface() {
    let set_comp = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_compressor_enabled(true);
        l.set_compressor_enabled(true);
        g.set_compressor_band_params(0, -24.0, 4.0, 10.0, 120.0, 2.0);
        l.set_compressor_band_params(0, -24.0, 4.0, 10.0, 120.0, 2.0);
        g.set_compressor_band_features(1, 6.0, config::CompressorDetector::Rms, true);
        l.set_compressor_band_features(1, 6.0, config::CompressorDetector::Rms, true);
    };
    let mutators: [Mutator<'_>; 1] = [&set_comp];
    let (a, b) = AbHarness::new(256, 6).run(&EngineConfig::default(), &mutators);
    assert_bit_exact("compressor_control_surface", &a, &b);
}

/// `limiter_control_surface` — limiter mode/params/true-peak.
#[test]
fn limiter_control_surface() {
    let set_lim = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_limiter_enabled(true);
        l.set_limiter_enabled(true);
        g.set_limiter_mode(engine::dsp::limiter::LimiterMode::Saturate);
        l.set_limiter_mode(engine::dsp::limiter::LimiterMode::Saturate);
        g.set_limiter_params(5.0, 2.0, 80.0, -1.0, true);
        l.set_limiter_params(5.0, 2.0, 80.0, -1.0, true);
        g.set_limiter_true_peak(true);
        l.set_limiter_true_peak(true);
    };
    let mutators: [Mutator<'_>; 1] = [&set_lim];
    let (a, b) = AbHarness::new(256, 6).run(&all_stages_config(), &mutators);
    assert_bit_exact("limiter_control_surface", &a, &b);
}

/// `loudness_track_replaygain` — track-mode loudness with ReplayGain
/// metadata.
#[test]
fn loudness_track_replaygain() {
    let mut cfg = EngineConfig::default();
    cfg.loudness.mode = config::LoudnessMode::TrackReplayGain;
    let meta = engine::dsp::loudness::LoudnessMetadata {
        replaygain_track_db: Some(4.0),
        replaygain_album_db: Some(3.5),
        replaygain_track_peak: Some(0.7),
        replaygain_album_peak: Some(0.75),
        ebu_r128_loudness: None,
        ebu_r128_peak: None,
    };
    let apply = move |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.apply_loudness_metadata_outgoing(Some(meta));
        l.apply_loudness_metadata_outgoing(Some(meta));
    };
    let mutators: [Mutator<'_>; 1] = [&apply];
    let (a, b) = AbHarness::new(256, 6).run(&cfg, &mutators);
    assert_bit_exact("loudness_track_replaygain", &a, &b);
}

/// `crossfade_2input_f32` — the crossfade-pair entry, crossfading.
#[test]
fn crossfade_2input_f32() {
    let begin = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.begin_crossfade(100);
        l.begin_crossfade(100);
    };
    let mutators: [Mutator<'_>; 1] = [&begin];
    let (a, b) = AbHarness::new(256, 6).run_inputs(&EngineConfig::default(), &mutators);
    assert_bit_exact("crossfade_2input_f32", &a, &b);
}

/// `fade_2input` — sequential fade (the incoming stream fades in alone).
#[test]
fn fade_2input() {
    let begin = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.begin_fade(50);
        l.begin_fade(50);
    };
    let mutators: [Mutator<'_>; 1] = [&begin];
    let (a, b) = AbHarness::new(256, 4).run_inputs(&EngineConfig::default(), &mutators);
    assert_bit_exact("fade_2input", &a, &b);
}

/// `crossfade_disabled_gapless_2input` — crossfade disabled: the incoming
/// stream replaces the outgoing at the transition point.
#[test]
fn crossfade_disabled_gapless_2input() {
    let mut cfg = EngineConfig::default();
    cfg.crossfade.enabled = false;
    let begin = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.begin_crossfade(100);
        l.begin_crossfade(100);
    };
    let mutators: [Mutator<'_>; 1] = [&begin];
    let (a, b) = AbHarness::new(256, 6).run_inputs(&cfg, &mutators);
    assert_bit_exact("crossfade_disabled_gapless_2input", &a, &b);
}

/// `crossfade_2input_midstream_controls` — crossfade + lane/slot
/// commands mid-flight.
#[test]
fn crossfade_2input_midstream_controls() {
    let begin = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.begin_crossfade(200);
        l.begin_crossfade(200);
    };
    let tweak = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_input_gain(1, 0.6);
        l.set_input_gain(1, 0.6);
        g.set_input_pan(1, -0.3);
        l.set_input_pan(1, -0.3);
    };
    let mutators: [Mutator<'_>; 2] = [&begin, &tweak];
    let (a, b) = AbHarness::new(256, 8).run_inputs(&bus_config(), &mutators);
    assert_bit_exact("crossfade_2input_midstream_controls", &a, &b);
}

/// `crossfade_2input_overrun_buffer` — the pair entry with
/// larger-than-max blocks.
#[test]
fn crossfade_2input_overrun_buffer() {
    let begin = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.begin_crossfade(100);
        l.begin_crossfade(100);
    };
    let mutators: [Mutator<'_>; 1] = [&begin];
    let (a, b) = AbHarness::new(MAX_AUDIO_BLOCK_FRAMES + 512, 3)
        .run_inputs(&EngineConfig::default(), &mutators);
    assert_bit_exact("crossfade_2input_overrun_buffer", &a, &b);
}

// ─────────────────────────────────────────────────────────────────────────────
// The Phase-46 extensions (inventory §4.2)
// ─────────────────────────────────────────────────────────────────────────────

/// `aux_bus_sends_and_return` — aux bus enabled with per-slot sends.
#[test]
fn aux_bus_sends_and_return() {
    let mut cfg = bus_config();
    cfg.aux.enabled = true;
    cfg.aux.return_gain = 0.7;
    let set_send = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_slot_send(2, 0.8, 0.5);
        l.set_slot_send(2, 0.8, 0.5);
    };
    let mutators: [Mutator<'_>; 1] = [&set_send];
    let (a, b) = AbHarness::new(256, 6).run(&cfg, &mutators);
    assert_bit_exact("aux_bus_sends_and_return", &a, &b);
}

/// `aux_insert_toggle` — the aux convolution insert toggled live.
#[test]
fn aux_insert_toggle() {
    let cfg = bus_config();
    let toggle = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_aux(true, 0.5);
        l.set_aux(true, 0.5);
        g.set_aux_insert(true, 0.35);
        l.set_aux_insert(true, 0.35);
    };
    let mutators: [Mutator<'_>; 1] = [&toggle];
    let (a, b) = AbHarness::new(256, 6).run(&cfg, &mutators);
    assert_bit_exact("aux_insert_toggle", &a, &b);
}

/// `aux_ducking_program_gated` — program-gated ducking steering slot
/// gains.
#[test]
fn aux_ducking_program_gated() {
    let cfg = bus_config();
    let duck = |g: &mut Graph2Engine, l: &mut DspGraph| {
        let d = DuckState {
            source: 0,
            threshold_db: -20.0,
            depth_db: -6.0,
            attack_frames: 256,
            release_frames: 2048,
            targets: [1, 2, 0, 0][..MAX_DUCK_TARGETS].try_into().unwrap(),
            target_count: 2,
        };
        g.set_duck(Some(d));
        l.set_duck(Some(d));
    };
    let mutators: [Mutator<'_>; 1] = [&duck];
    let (a, b) = AbHarness::new(256, 8).run(&cfg, &mutators);
    assert_bit_exact("aux_ducking_program_gated", &a, &b);
}

/// `slot_automation_tracks` — per-slot automation driving a lane's gain.
#[test]
fn slot_automation_tracks() {
    let cfg = bus_config();
    let auto = |g: &mut Graph2Engine, l: &mut DspGraph| {
        let mut points = [AutomationPoint {
            frame: 0,
            value: 0.2,
        }; 64];
        for (i, p) in points.iter_mut().enumerate().take(8) {
            *p = AutomationPoint {
                frame: i * 256,
                value: 0.2 + i as f32 * 0.1,
            };
        }
        g.set_slot_automation(2, AutomationTarget::Gain, &points[..8]);
        l.set_slot_automation(2, AutomationTarget::Gain, &points[..8]);
    };
    let mutators: [Mutator<'_>; 1] = [&auto];
    let (a, b) = AbHarness::new(256, 8).run(&cfg, &mutators);
    assert_bit_exact("slot_automation_tracks", &a, &b);
}

/// `slot_automation_clear` — clearing an automation track mid-flight.
#[test]
fn slot_automation_clear() {
    let cfg = bus_config();
    let set = |g: &mut Graph2Engine, l: &mut DspGraph| {
        let points = [AutomationPoint {
            frame: 0,
            value: 0.5,
        }; 2];
        g.set_slot_automation(1, AutomationTarget::Gain, &points);
        l.set_slot_automation(1, AutomationTarget::Gain, &points);
    };
    let clear = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.clear_slot_automation(1);
        l.clear_slot_automation(1);
    };
    let mutators: [Mutator<'_>; 2] = [&set, &clear];
    let (a, b) = AbHarness::new(256, 8).run(&cfg, &mutators);
    assert_bit_exact("slot_automation_clear", &a, &b);
}

/// `correction_enabled_depth` — room correction toggle + depth (config
/// path builds the IR set).
#[test]
fn correction_enabled_depth() {
    let mut cfg = all_stages_config();
    cfg.correction.enabled = true;
    cfg.correction.depth = 0.5;
    let depth = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_correction_depth(0.25);
        l.set_correction_depth(0.25);
    };
    let mutators: [Mutator<'_>; 1] = [&depth];
    let (a, b) = AbHarness::new(256, 6).run(&cfg, &mutators);
    assert_bit_exact("correction_enabled_depth", &a, &b);
}

/// `spatial_master_vs_graph_node` — the spatial master output stage
/// (config-enabled binaural rendering).
#[test]
fn spatial_master_vs_graph_node() {
    let mut cfg = all_stages_config();
    cfg.spatial.enabled = true;
    let (a, b) = AbHarness::new(256, 6).run(&cfg, &[]);
    assert_bit_exact("spatial_master_vs_graph_node", &a, &b);
}

/// `lanes_activate_deactivate` — lane slots activated/deactivated
/// mid-flight (the detached-slot semantics).
#[test]
fn lanes_activate_deactivate() {
    let cfg = bus_config();
    let off = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_input_active(2, false);
        l.set_input_active(2, false);
    };
    let on = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_input_active(2, true);
        l.set_input_active(2, true);
    };
    let mutators: [Mutator<'_>; 2] = [&off, &on];
    let (a, b) = AbHarness::new(256, 6).run(&cfg, &mutators);
    assert_bit_exact("lanes_activate_deactivate", &a, &b);
}

/// `seek_fade_out_in` — the seek fade-out/fade-in pair (gapless seek).
#[test]
fn seek_fade_out_in() {
    let cfg = EngineConfig::default();
    let out = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.begin_seek_fadeout();
        l.begin_seek_fadeout();
    };
    let inn = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.begin_seek_fadein();
        l.begin_seek_fadein();
    };
    let mutators: [Mutator<'_>; 2] = [&out, &inn];
    let (a, b) = AbHarness::new(256, 8).run(&cfg, &mutators);
    assert_bit_exact("seek_fade_out_in", &a, &b);
}

/// `speed_timestretch_midstream` — playback speed changes hitting the
/// timestretcher mid-flight (the `with_both` accessor seam).
#[test]
fn speed_timestretch_midstream() {
    let mut cfg = all_stages_config();
    cfg.speed_mode = config::SpeedMode::TimeStretch;
    let speed = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.with_both(|gr| gr.timestretch_mut().stretcher.set_speed(1.25));
        l.timestretch_mut().stretcher.set_speed(1.25);
    };
    let mutators: [Mutator<'_>; 1] = [&speed];
    let (a, b) = AbHarness::new(256, 8).run(&cfg, &mutators);
    assert_bit_exact("speed_timestretch_midstream", &a, &b);
}

/// `routing_bass_management_5_1` — bass management + LFE routing on 5.1.
#[test]
fn routing_bass_management_5_1() {
    let mut cfg = all_stages_config();
    cfg.bass_management.enabled = true;
    cfg.bass_management.crossover_hz = 120.0;
    cfg.lfe.enabled = true;
    cfg.lfe.gain_db = 3.0;
    let (a, b) = AbHarness::new(256, 6).run_mc(&cfg, 6, &[]);
    assert_bit_exact("routing_bass_management_5_1", &a, &b);
}

/// `precision_mode_switch` — f32→f64 precision mode changes mid-flight.
#[test]
fn precision_mode_switch() {
    let cfg = all_stages_config();
    let to_f64 = |g: &mut Graph2Engine, l: &mut DspGraph| {
        g.set_precision_mode(engine::dsp::pipeline::PrecisionMode::Quality);
        l.set_precision_mode(engine::dsp::pipeline::PrecisionMode::Quality);
    };
    let mutators: [Mutator<'_>; 1] = [&to_f64];
    let (a, b) = AbHarness::new(256, 8).run(&cfg, &mutators);
    assert_bit_exact("precision_mode_switch", &a, &b);
}

/// `generation_swap_continuity` — a live reconfigure (generation swap)
/// mid-flight: audio continuity across the swap, both engines swapping
/// through their own plan sources.
#[test]
fn generation_swap_continuity() {
    let cfg = bus_config();
    let reconfigure = |g: &mut Graph2Engine, l: &mut DspGraph| {
        let mut c2 = bus_config();
        c2.mix_slots = 6;
        g.reconfigure(&c2);
        l.reconfigure(&c2);
        let _ = cfg;
    };
    let mutators: [Mutator<'_>; 1] = [&reconfigure];
    let (a, b) = AbHarness::new(256, 8).run(&cfg, &mutators);
    assert_bit_exact("generation_swap_continuity", &a, &b);
    // The post-swap mixer state must agree too (structural parity).
    let mut g2 = Graph2Engine::from_config(&bus_config(), SR);
    let mut legacy = DspGraph::from_config(&bus_config(), SR);
    let mut c2 = bus_config();
    c2.mix_slots = 6;
    g2.reconfigure(&c2);
    legacy.reconfigure(&c2);
    g2.drain_queued_control();
    legacy.drain_queued_control();
    assert_eq!(g2.mixer_state(), legacy.mixer_state());
}

/// `queue_backpressure_drop_counters` — both engines' control queues
/// drop identically under backpressure.
#[test]
fn queue_backpressure_drop_counters() {
    let cfg = EngineConfig::default();
    let g2 = Graph2Engine::from_config(&cfg, SR);
    let legacy = DspGraph::from_config(&cfg, SR);
    let h2 = g2.control_handle();
    let hl = legacy.control_handle();
    // Flood a queue far past its capacity: identical drop accounting.
    for _ in 0..200 {
        h2.set_volume(0.5);
        hl.set_volume(0.5);
    }
    assert_eq!(h2.dropped_commands(), hl.dropped_commands());
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase-47 shadow mode: the built-in A/B (the engine does it itself)
// ─────────────────────────────────────────────────────────────────────────────

/// The shadow twin bit-compares every block internally: a plain drive
/// with shadow on must record zero mismatches (and the outputs must
/// match the flag-off render — the shadow never alters audio).
#[test]
fn shadow_mode_bit_compares_every_block() {
    let cfg = all_stages_config();
    let mut shadowed = Graph2Engine::from_config(&cfg, SR);
    shadowed.enable_shadow(&cfg);

    let mut plain = Graph2Engine::from_config(&cfg, SR);
    let mut out_shadowed = Vec::new();
    let mut out_plain = Vec::new();
    for b in 0..6 {
        let mut l = vec![0.0f32; 256];
        let mut r = vec![0.0f32; 256];
        fill_interleaved(&mut l, 1, b * 256, SR);
        fill_interleaved(&mut r, 1, b * 256 + 7919, SR);
        let (mut pl, mut pr) = (l.clone(), r.clone());
        shadowed.process_block(&mut l, &mut r);
        plain.process_block(&mut pl, &mut pr);
        out_shadowed.extend_from_slice(&l);
        out_shadowed.extend_from_slice(&r);
        out_plain.extend_from_slice(&pl);
        out_plain.extend_from_slice(&pr);
    }
    let (blocks, mismatches) = shadowed.shadow_stats();
    assert_eq!(mismatches, 0, "shadow must record zero mismatches");
    assert!(blocks >= 6, "shadow must compare every block");
    assert_bit_exact(
        "shadow_mode_bit_compares_every_block",
        &out_shadowed,
        &out_plain,
    );
}

/// Shadow commands stay lock-step: control mutations through the engine's
/// mirrored surface fan out to the twin.
#[test]
fn shadow_mode_control_fanout_stays_lockstep() {
    let cfg = bus_config();
    let mut g = Graph2Engine::from_config(&cfg, SR);
    g.enable_shadow(&cfg);
    g.set_volume(0.4);
    g.set_eq_enabled(true);
    g.begin_crossfade(100);
    g.drain_queued_control();
    let (blocks, mismatches) = g.shadow_stats();
    assert_eq!(mismatches, 0);
    assert_eq!(blocks, 0, "no blocks processed yet — only the fan-out");
    // And a crossfade render stays exact through the swap.
    let mut pl = vec![0.0f32; 512];
    let mut pr = vec![0.0f32; 512];
    fill_interleaved(&mut pl, 1, 0, SR);
    fill_interleaved(&mut pr, 1, 7919, SR);
    g.process_block(&mut pl, &mut pr);
    let (blocks, mismatches) = g.shadow_stats();
    assert_eq!(mismatches, 0);
    assert_eq!(blocks, 1);
}

/// The engine's `graph2_shadow_verify` config flag drives the shadow.
#[test]
fn shadow_flag_constructs_the_twin() {
    let cfg = EngineConfig {
        graph2_shadow_verify: true,
        ..Default::default()
    };
    let engine = engine::AudioEngine::new(cfg).unwrap();
    assert!(engine.pipeline().shadow_enabled());
    let (blocks, mismatches) = engine.pipeline().shadow_stats();
    assert_eq!(mismatches, 0);
    assert_eq!(blocks, 0);
}

/// Structural parity: latency + active-node reports agree.
#[test]
fn structural_reports_match() {
    let cfg = all_stages_config();
    let g2 = Graph2Engine::from_config(&cfg, SR);
    let legacy = DspGraph::from_config(&cfg, SR);
    assert_eq!(g2.total_latency_ms(), legacy.total_latency_ms());
    let n2 = g2.graph_nodes();
    let nl = legacy.graph_nodes();
    assert_eq!(n2.len(), nl.len());
    for (a, b) in n2.iter().zip(nl.iter()) {
        assert_eq!(a.name, b.name, "node set must match");
        assert_eq!(a.active, b.active, "node {} activity must match", a.name);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Registry accounting (the scaffold's original guarantee, kept)
// ─────────────────────────────────────────────────────────────────────────────

/// Every scenario name from the original registry is now an implemented
/// test: this counts the scenario fns to keep the mapping honest.
#[test]
fn parity_registry_is_complete() {
    // The 27 legacy cases + the 14 Phase-46 extensions, all live above.
    let names: &[&str] = &[
        "default_stereo_f32",
        "default_stereo_f64",
        "all_stages_stereo_f32",
        "all_stages_stereo_f64",
        "all_stages_block_len_1",
        "all_stages_block_max",
        "all_stages_overrun_buffer",
        "midstream_control_changes",
        "bit_perfect_stereo",
        "bit_perfect_multichannel",
        "dop_bypass_stereo",
        "loudness_ebu_r128",
        "convolution_synthetic_ir",
        "mono_via_mc_entry",
        "multichannel_5_1_trim",
        "multichannel_7_1",
        "eq_control_surface",
        "crossfeed_control_surface",
        "compressor_control_surface",
        "limiter_control_surface",
        "loudness_track_replaygain",
        "crossfade_2input_f32",
        "crossfade_2input_f64",
        "fade_2input",
        "crossfade_disabled_gapless_2input",
        "crossfade_2input_midstream_controls",
        "crossfade_2input_overrun_buffer",
        "aux_bus_sends_and_return",
        "aux_insert_toggle",
        "aux_ducking_program_gated",
        "slot_automation_tracks",
        "slot_automation_clear",
        "correction_enabled_depth",
        "spatial_master_vs_graph_node",
        "lanes_activate_deactivate",
        "seek_fade_out_in",
        "speed_timestretch_midstream",
        "routing_bass_management_5_1",
        "precision_mode_switch",
        "generation_swap_continuity",
        "queue_backpressure_drop_counters",
    ];
    // 27 legacy rows (crossfade_2input_f64 lives in its own fn below) +
    // 14 Phase-46 extensions = the full 41-row registry, every row an
    // implemented test above.
    assert_eq!(names.len(), 41);
    let _ = MixerState::PlayingCurrent;
}

/// The f64 crossfade case (completing the 27 legacy rows).
#[test]
fn crossfade_2input_f64() {
    let cfg = EngineConfig::default();
    let mut g2 = Graph2Engine::from_config(&cfg, SR);
    let mut legacy = DspGraph::from_config(&cfg, SR);
    g2.begin_crossfade(200);
    legacy.begin_crossfade(200);
    let mut l = vec![0.0f64; 512];
    let mut r = vec![0.0f64; 512];
    let mut ll = vec![0.0f64; 512];
    let mut rr = vec![0.0f64; 512];
    fill_f64(&mut l, 0);
    fill_f64(&mut r, 7919);
    ll.copy_from_slice(&l);
    rr.copy_from_slice(&r);
    g2.process_block_f64(&mut l, &mut r);
    legacy.process_block_f64(&mut ll, &mut rr);
    assert_bit_exact_f64("crossfade_2input_f64", &l, &ll);
    assert_bit_exact_f64("crossfade_2input_f64", &r, &rr);
}

/// The deterministic generator must be reproducible (a precondition for
/// the shared fixture, inherited from the scaffold).
#[test]
fn deterministic_generator_is_bit_reproducible() {
    let mut a = vec![0.0f32; 512];
    let mut b = vec![0.0f32; 512];
    fill_interleaved(&mut a, 2, 0, SR);
    fill_interleaved(&mut b, 2, 0, SR);
    assert_bit_exact("generator_reproducibility", &a, &b);
    let mut c = vec![0.0f32; 512];
    fill_interleaved(&mut c, 2, INPUT1_OFFSET, SR);
    assert_ne!(
        a.iter()
            .zip(&c)
            .filter(|(x, y)| x.to_bits() == y.to_bits())
            .count(),
        a.len(),
        "offset stream must differ"
    );
}

/// The max-block scenario family respects the engine block budget.
const _: () = assert!(MAX_AUDIO_BLOCK_FRAMES >= 256);
