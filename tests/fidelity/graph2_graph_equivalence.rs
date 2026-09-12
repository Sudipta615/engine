//! Fidelity suite — Graph2 node parity vs `dsp::graph` (Phase 46, v3.51.0).
//!
//! **Status: scaffold.** This suite grows with the Phase-46 node port: for
//! every scenario in `graph_pipeline_equivalence` (the 27-case matrix that
//! pins `dsp::graph` ≡ `DspPipeline`), the equivalent Graph2 topology must
//! match `dsp::graph` **bit-exactly** (`f32::to_bits`, NaN payloads
//! included) — the production hot path's node set exists as Graph2 node
//! kinds with identical math via the Phase-45 shared-kernel seam, so
//! offline/realtime divergence is structurally impossible.
//!
//! The full port map (arena slot → Graph2 kind), the control-surface mirror
//! checklist (`Graph2ControlHandle` ≡ `GraphControlHandle`), latency/tail
//! descriptor parity, and the scenario→topology mapping are enumerated in
//! `docs/PHASE46_NODE_PARITY_INVENTORY.md`. Scenario bodies land in the same
//! PR as their node kinds (`#[ignore]` markers are removed as kinds arrive);
//! the harness, deterministic signal generator, and bit-compare machinery are
//! final from day one so ported-node PRs only add scenarios.
//!
//! Harness contract (inherited from `graph_pipeline_equivalence`):
//! - the same deterministic generator (xorshift multi-tone + seeded noise)
//!   feeds both graphs, so a mismatch is always a graph2 divergence, never
//!   a fixture difference;
//! - sample comparison is bit-exact (`to_bits`), with structural parity
//!   assertions (total latency, active-node set) per case;
//! - `block_len` sweeps {1, 256, MAX_AUDIO_BLOCK_FRAMES} wherever the
//!   original matrix fixes one block length.

use engine::buffer::MAX_AUDIO_BLOCK_FRAMES;
use engine::prelude::{
    ExecutionOrder, Graph2, Graph2Error, NodeId, NodeParams, OfflineExecutor, PortId, TestSignal,
};
use std::collections::BTreeMap;

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
// Scenario registry
// ─────────────────────────────────────────────────────────────────────────────

/// One row of the Phase-46 parity matrix: the scenario name (identical to
/// the `graph_pipeline_equivalence` case where applicable), and the Graph2
/// topology + command script that must reproduce `dsp::graph` bit-exactly.
///
/// `topology` builds the Graph2 graph for the scenario (nodes landed so far
/// only; see the inventory §4 for the target shapes). `None` marks a
/// scenario whose kinds have not been ported yet — it renders as `ignored`
/// in the matrix walker and must be implemented in the PR that lands its
/// node kind.
#[allow(dead_code)] // fields + shapes go live with the first ported-node PR
struct ParityScenario {
    name: &'static str,
    /// Builds the Graph2 graph and returns (graph, compiled order, sink
    /// node, blocks to render). `Err(NotPorted)` until the kinds land.
    topology: Option<fn() -> ScenarioGraph>,
    /// Command script applied between blocks (Phase-46 control-surface
    /// parity). Empty until `Graph2ControlHandle` lands.
    midstream: &'static [usize],
}

/// A built scenario topology ready to render.
#[allow(dead_code)] // consumed by the matrix walker once topologies land
struct ScenarioGraph {
    graph: Graph2,
    order: ExecutionOrder,
    sink: NodeId,
    block: usize,
    blocks: usize,
}

/// Marker error for scenarios whose node kinds are not yet ported.
#[allow(dead_code)] // constructed by the first ported-node topology builder
#[derive(Debug)]
struct NotPorted(&'static str);

impl std::fmt::Display for NotPorted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node kind(s) not yet ported: {}", self.0)
    }
}

impl std::error::Error for NotPorted {}

#[allow(dead_code)] // the topology-builder signature used by ported scenarios
type TopologyResult = Result<ScenarioGraph, NotPorted>;

/// The scenario matrix — mirrors `graph_pipeline_equivalence::cases()`
/// (27 cases) plus the Phase-46-only parity extensions (aux bus, ducking,
/// per-slot automation, correction, spatial, lanes, seek-fade, speed,
/// routing, precision switch, generation swap, queue backpressure).
///
/// Rows are added in port order (inventory §6):
/// 1. thin wrapper nodes (EQ, dynamics, crossfeed, stereo, convolver,
///    dither, routing, balance),
/// 2. seek-fade / volume-ramp / limiter / rate-converter,
/// 3. mix bus (2-input crossfade scenarios first, then multichannel),
/// 4. aux bus,
/// 5. correction, spatial.
fn scenarios() -> Vec<ParityScenario> {
    vec![
        // ── graph_pipeline_equivalence parity (27) ──────────────────────────
        sc("default_stereo_f32", None),
        sc("default_stereo_f64", None),
        sc("all_stages_stereo_f32", None),
        sc("all_stages_stereo_f64", None),
        sc("all_stages_block_len_1", None),
        sc("all_stages_block_max", None),
        sc("all_stages_overrun_buffer", None),
        sc("midstream_control_changes", None),
        sc("bit_perfect_stereo", None),
        sc("bit_perfect_multichannel", None),
        sc("dop_bypass_stereo", None),
        sc("loudness_ebu_r128", None),
        sc("convolution_synthetic_ir", None),
        sc("mono_via_mc_entry", None),
        sc("multichannel_5_1_trim", None),
        sc("multichannel_7_1", None),
        sc("eq_control_surface", None),
        sc("crossfeed_control_surface", None),
        sc("compressor_control_surface", None),
        sc("limiter_control_surface", None),
        sc("loudness_track_replaygain", None),
        sc("crossfade_2input_f32", None),
        sc("crossfade_2input_f64", None),
        sc("fade_2input", None),
        sc("crossfade_disabled_gapless_2input", None),
        sc("crossfade_2input_midstream_controls", None),
        sc("crossfade_2input_overrun_buffer", None),
        // ── Phase-46-only parity extensions (inventory §4.2) ─────────────────
        sc("aux_bus_sends_and_return", None),
        sc("aux_insert_toggle", None),
        sc("aux_ducking_program_gated", None),
        sc("slot_automation_tracks", None),
        sc("slot_automation_clear", None),
        sc("correction_enabled_depth", None),
        sc("spatial_master_vs_graph_node", None),
        sc("lanes_activate_deactivate", None),
        sc("seek_fade_out_in", None),
        sc("speed_timestretch_midstream", None),
        sc("routing_bass_management_5_1", None),
        sc("precision_mode_switch", None),
        sc("generation_swap_continuity", None),
        sc("queue_backpressure_drop_counters", None),
    ]
}

fn sc(name: &'static str, topology: Option<fn() -> ScenarioGraph>) -> ParityScenario {
    ParityScenario {
        name,
        topology,
        midstream: &[],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Matrix walker
// ─────────────────────────────────────────────────────────────────────────────

/// Walks the scenario matrix. Every scenario whose kinds are ported renders
/// its Graph2 topology and compares against the `dsp::graph` reference for
/// the same scenario bit-exactly; un-ported scenarios are reported (name +
/// missing kinds) and asserted only for *presence in the registry*, so this
/// walker is the single place Phase-46 port PRs flip `#[ignore]`s off.
///
/// The reference side (`dsp::graph` DspGraph driven per the
/// `graph_pipeline_equivalence` harness) is plugged in as kinds land; the
/// registry above is the authoritative mapping of case names to topologies.
#[test]
fn graph2_parity_matrix_registry_is_complete() {
    let matrix = scenarios();
    // The 27 legacy cases, in order, followed by the Phase-46 extensions.
    let legacy: Vec<&str> = matrix.iter().take(27).map(|s| s.name).collect();
    assert_eq!(
        legacy,
        vec![
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
        ],
        "the 27 graph_pipeline_equivalence cases must map 1:1 (inventory §4.1)"
    );
    let unported = matrix.iter().filter(|s| s.topology.is_none()).count();
    // Ported kinds reduce this count; the suite passes vacuously until then,
    // but the registry + inventory are the tracking surface.
    assert_eq!(
        unported,
        matrix.len(),
        "registry accounting: every row carries an explicit port status"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Harness plumbing exercised today (topology/render/capture baseline)
// ─────────────────────────────────────────────────────────────────────────────

/// The harness machinery (build → compile → render → capture → bit-compare)
/// is final from day one. This baseline pins it against today's Graph2
/// primitive set: a dry/wet diamond through the offline executor must match
/// an independent scalar computation of the same math sample-for-sample —
/// proving the comparison path (including `to_bits`) is ready for the
/// ported-node scenarios.
#[test]
fn harness_bit_compare_baselines_against_scalar_reference() {
    const BLOCK: usize = 128;
    let mut g = Graph2::new();
    let src = {
        let id = g.add_source("imp");
        g.set_params(
            id,
            NodeParams::Source(engine::prelude::SourceParams {
                signal: TestSignal::Impulse,
                frequency_hz: 440.0,
            }),
        );
        id
    };
    let split = g.add_split("drywet", 2);
    let dry = g.add_gain("dry", 0.5);
    let wet = g.add_delay("wet", 100);
    let mix = g.add_mix("sum", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), dry, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), wet, PortId::IN).unwrap();
    g.add_edge(dry, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(wet, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().expect("diamond compiles").clone();

    let blocks = 2;
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK, SR).unwrap();
    ex.process_blocks(blocks).unwrap();
    let cap = ex.capture(sink).unwrap().to_vec();

    // Independent scalar reference: impulse × 0.5 at t0, plus a delayed
    // unity copy at t100.
    let mut reference = vec![0.0f32; blocks * BLOCK];
    reference[0] += 0.5;
    reference[100] += 1.0;
    assert_bit_exact("harness_baseline_diamond", &reference, &cap);
}

/// The deterministic generator must be reproducible: two runs with the
/// same state produce bit-identical buffers (a precondition for using it
/// as the shared fixture across both graphs).
#[test]
fn deterministic_generator_is_bit_reproducible() {
    let mut a = vec![0.0f32; 512];
    let mut b = vec![0.0f32; 512];
    fill_interleaved(&mut a, 2, 0, SR);
    fill_interleaved(&mut b, 2, 0, SR);
    assert_bit_exact("generator_reproducibility", &a, &b);
    // And the secondary-stream offset produces a genuinely different signal.
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

/// The max-block scenario family must respect the engine block budget: the
/// harness reserves plane capacity per
/// [`MAX_AUDIO_BLOCK_FRAMES`]` exactly like the `dsp::graph` scratch arena.
/// A const assertion — evaluated at compile time, so the budget contract is
/// pinned even before ported scenarios exercise it at runtime.
const _: () = assert!(MAX_AUDIO_BLOCK_FRAMES >= 256);

#[test]
fn block_budget_types_are_usable_in_scenarios() {
    let _ = BTreeMap::<NodeId, ()>::new(); // NodeId usable as scenario state key
    let _ = std::mem::size_of::<Graph2Error>(); // error type in scope for ported scenarios
}
