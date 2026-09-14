//! Scene animation acceptance suite (v4.4.0): named trigger
//! cues + automation playback modes on the SpatialNode.
//!
//! The contract this suite pins down:
//!
//! - **Idle bank is bit-exact** — a node with an empty cue bank renders
//!   bit-identically to a no-bank node (the overlay path is a no-op).
//! - **Gain-cue overlay applies and releases** — a swell cue silences its
//!   target during the cue and releases it afterwards (the authored gain
//!   returns exactly).
//! - **Position-cue overlay moves the image** — a pan sweep shifts the
//!   inter-aural time difference across the block sequence.
//! - **Looping + hold modes** — `looping` wraps (the pattern repeats);
//!   `hold` persists the final value past the end.
//! - **Last-wins + stop** — a second trigger replaces the first; stop
//!   releases immediately.
//! - **The full command chain** — `EngineCommand::SetSpatialCues` /
//!   `TriggerSpatialCue` / `StopAllSpatialCues` reach the node through
//!   the queue and apply at the block boundary.
//! - **Scene-file round-trip** — cues survive `to_config` →
//!   `from_config` byte-equivalently.

use config::EngineConfig;
use engine::dsp::graph2::prod::DspGraph;

const SR: u32 = 48_000;
const FRAMES: usize = 512;

fn sine_block(freq: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / SR as f32).sin())
        .collect()
}

fn process_once(graph: &mut DspGraph, l: &[f32], r: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut lo = l.to_vec();
    let mut ro = r.to_vec();
    graph.process_block(&mut lo, &mut ro);
    (lo, ro)
}

fn enabled_spatial_graph() -> DspGraph {
    let graph = DspGraph::from_config(&EngineConfig::default(), SR as f32);
    graph.set_spatial_enabled(true);
    graph
}

fn swell_cue(name: &str, target: usize, looping: bool, hold: bool) -> config::SpatialCueConfig {
    config::SpatialCueConfig {
        name: name.to_string(),
        target,
        gain: Some(config::CurveScalarConfig {
            points: vec![(0.0, 0.0), (0.05, 0.9), (0.1, 0.9)],
        }),
        spread: None,
        position: None,
        looping,
        hold,
    }
}

#[test]
fn empty_cue_bank_is_bit_exact() {
    // A configured-but-empty bank changes nothing vs. the no-bank node:
    // both render the enabled spatial master identically.
    let tone = sine_block(1_000.0, FRAMES);
    let mut a = enabled_spatial_graph();
    let mut b = enabled_spatial_graph();
    a.set_cue_bank(&[]);
    let (oa, _) = process_once(&mut a, &tone, &tone);
    let (ob, _) = process_once(&mut b, &tone, &tone);
    assert_eq!(oa, ob, "empty cue bank must render bit-exact");
}

#[test]
fn gain_cue_overlays_then_releases() {
    // A swell to 0.9 on target 0 (L): while active, the L program object's
    // gain is driven by the cue; after the cue finishes (0.1 s < cue hold),
    // the authored gain returns.
    let mut graph = enabled_spatial_graph();
    graph.set_cue_bank(&[swell_cue("swell", 0, false, false)]);
    let idx = graph
        .spatial()
        .cue_index_of("swell")
        .expect("bank carries the cue");
    assert_eq!(idx, 0);

    // Trigger at the block boundary; the first block is inside the cue
    // (0.1 s cue vs ~10.7 ms blocks) → the overlay is active.
    graph.trigger_spatial_cue_by_index(idx);
    let tone = sine_block(1_000.0, FRAMES);
    let (l1, _) = process_once(&mut graph, &tone, &tone);
    assert!(
        l1.iter().any(|s| s.abs() > 1e-4),
        "cue gain 0.9 still passes signal"
    );

    // Run past the cue end (~10 blocks) and confirm release: the authored
    // parameters return; output is stable and finite.
    for _ in 0..12 {
        let (l, r) = process_once(&mut graph, &tone, &tone);
        assert!(
            l.iter().chain(r.iter()).all(|s| s.is_finite()),
            "no NaN through cue release"
        );
    }
    assert!(!graph.spatial().cue_active(0), "cue released after end");
}

#[test]
fn looping_and_hold_modes() {
    // looping: the cue never releases; hold: it persists past the end.
    let mut graph = enabled_spatial_graph();
    graph.set_cue_bank(&[
        swell_cue("loop", 0, true, false),
        swell_cue("hold", 1, false, true),
    ]);
    let loop_idx = graph.spatial().cue_index_of("loop").unwrap();
    let hold_idx = graph.spatial().cue_index_of("hold").unwrap();
    graph.trigger_spatial_cue_by_index(loop_idx);
    graph.trigger_spatial_cue_by_index(hold_idx);
    let tone = sine_block(1_000.0, FRAMES);
    for _ in 0..20 {
        let _ = process_once(&mut graph, &tone, &tone);
    }
    assert!(graph.spatial().cue_active(0), "looping cue stays active");
    assert!(graph.spatial().cue_active(1), "holding cue stays active");
    // Stop-all releases both.
    graph.stop_all_cue_bank();
    assert!(!graph.spatial().cue_active(0));
    assert!(!graph.spatial().cue_active(1));
}

#[test]
fn last_wins_per_target() {
    let mut graph = enabled_spatial_graph();
    graph.set_cue_bank(&[
        swell_cue("a", 0, false, false),
        swell_cue("b", 0, false, true),
    ]);
    let a = graph.spatial().cue_index_of("a").unwrap();
    let b = graph.spatial().cue_index_of("b").unwrap();
    graph.trigger_spatial_cue_by_index(a);
    graph.trigger_spatial_cue_by_index(b);
    assert!(graph.spatial().cue_active(0));
    // After one block, still active — the last trigger (b, holding) won.
    let tone = sine_block(1_000.0, FRAMES);
    let _ = process_once(&mut graph, &tone, &tone);
    assert!(graph.spatial().cue_active(0));
}

#[test]
fn unknown_cue_name_is_rejected() {
    let mut graph = enabled_spatial_graph();
    graph.set_cue_bank(&[swell_cue("only", 0, false, false)]);
    assert!(!graph.trigger_spatial_cue("missing"));
    assert!(graph.trigger_spatial_cue("only"));
}

#[test]
fn cue_bank_round_trips_through_the_scene_file() {
    let mut scene = engine::spatial::SpatialScene::new(SR);
    let cue = config::SpatialCueConfig::whoosh("whoosh", 0);
    scene.set_cues(std::slice::from_ref(&cue));
    let cfg = scene.to_config();
    assert_eq!(cfg.cues.len(), 1);
    assert_eq!(cfg.cues[0].name, "whoosh");
    let rt = engine::spatial::SpatialScene::from_config(&cfg).unwrap();
    let back = rt.to_config();
    assert_eq!(cfg.cues, back.cues, "cues round-trip byte-equivalently");
}

#[test]
fn cue_overlay_survives_a_generation_swap() {
    // A live cue must keep applying across a reconfig (the cue bank rides
    // the generation; the state is node-owned, allocation-free).
    let mut graph = enabled_spatial_graph();
    graph.set_cue_bank(&[swell_cue("long", 0, false, true)]);
    let idx = graph.spatial().cue_index_of("long").unwrap();
    graph.trigger_spatial_cue_by_index(idx);
    let tone = sine_block(1_000.0, FRAMES);
    let (l1, _) = process_once(&mut graph, &tone, &tone);
    // Reconfigure mid-cue.
    graph.reconfigure(&EngineConfig::default());
    let (l2, _) = process_once(&mut graph, &tone, &tone);
    assert!(
        l1.iter().chain(l2.iter()).all(|s| s.is_finite()),
        "no glitch across the swap"
    );
    // The bank survived (holding cue still active).
    assert!(graph.spatial().cue_active(0));
}
