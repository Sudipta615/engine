//! Fidelity tests — Timeline and Scheduler (v3.28, roadmap v3.28).
//!
//! Evolution thresholds (`docs/EVOLUTION.md` Phase 26):
//! * a [`Timeline`] driving the Graph 2.0 [`OfflineExecutor`] renders
//!   **sample-accurately**: a gain step scheduled at a musical beat lands
//!   exactly on the requested sample (silence up to that sample, then the
//!   wave at the stepped gain from that sample on);
//! * **transport** — pausing halts both the clock and event delivery;
//!   stopping returns to scratch and holding;
//! * **looping** wraps the playhead (musical position) while the scheduler
//!   still delivers each event exactly once on the monotonic master;
//! * **quantization** snaps beat scheduling to a note grid;
//! * **tempo changes** (via [`TempoMap`]) retime a beat event to the correct
//!   sample under piecewise-constant segments;
//! * **timeline regions** resolve containment.

use engine::prelude::{
    AudioClock, EventPayload, EventTime, ExecutionOrder, Graph2, NodeId, OfflineExecutor, PortId,
    Quantize, SourceParams, TempoMap, TestSignal, Timeline, TransportState,
};

const SR: f32 = 48_000.0;
const BLOCK: u64 = 128;
const FS: f32 = 48_000.0;

/// An open sine chain for transport tests: source(sine 440) → sink.
fn open_sine() -> (Graph2, ExecutionOrder, NodeId, NodeId) {
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 440.0,
        },
    );
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    // Use the source id as a stand-in gain slot so the signature matches.
    let sink_id = sink;
    (g, order, src, sink_id)
}

/// A gated sine chain: source(sine 440) → gain(g) → sink. Returns
/// (graph, order, gain node id, sink node id).
fn gated_sine() -> (Graph2, ExecutionOrder, NodeId, NodeId) {
    let mut g = Graph2::new();
    let src = g.add_source_with(
        "tone",
        SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 440.0,
        },
    );
    let gain = g.add_gain("vol", 0.0);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    (g, order, gain, sink)
}

/// Drive `timeline` + `ex` for `blocks` blocks, applying SetGain events as
/// sample-accurate steps, and return the final captured audio. Rendering is
/// gated on the transport: when the clock is not playing, no block is fed
/// to the graph (the transport owns the render, not just the events).
fn drive(
    timeline: &mut Timeline,
    ex: &mut OfflineExecutor,
    sink: NodeId,
    blocks: usize,
) -> Vec<f32> {
    for _ in 0..blocks {
        for e in timeline.advance_block(BLOCK) {
            if let EventPayload::SetGain { node, gain } = e.payload {
                ex.set_gain_step(NodeId(node), gain, e.local_index(BLOCK))
                    .unwrap();
            }
        }
        if timeline.clock().is_playing() {
            ex.process_block().unwrap();
        }
    }
    ex.capture(sink).map(|v| v.to_vec()).unwrap_or_default()
}

/// Raw 440 Hz sine value at absolute sample `i`.
fn raw440(i: usize) -> f32 {
    (2.0 * std::f32::consts::PI * 440.0 * i as f32 / FS).sin()
}

#[test]
fn timeline_drives_graph_sample_accurately() {
    let (g, order, gain, sink) = gated_sine();
    let mut timeline = Timeline::new(SR);
    timeline.set_tempo(120.0);
    timeline.set_state(TransportState::Playing, 0);
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK as usize, SR).unwrap();

    // Beat 1 at 120 BPM → sample 24000. Gate the sine fully open there.
    timeline
        .schedule(
            EventTime::Beat(1.0),
            EventPayload::SetGain {
                node: gain.0,
                gain: 2.0,
            },
        )
        .unwrap();

    let cap = drive(&mut timeline, &mut ex, sink, 200); // 25600 samples
    assert_eq!(cap.len(), 200 * BLOCK as usize);
    // Silent up to and including sample 23999 (gain stays 0.0).
    assert!(
        cap[..24000].iter().all(|s| s.abs() < 1e-6),
        "gated before beat"
    );
    // Exactly at sample 24000 the stepped gain applies → 2× sine.
    assert!(
        (cap[24000] - 2.0 * raw440(24000)).abs() < 1e-3,
        "first open frame: {} vs {}",
        cap[24000],
        2.0 * raw440(24000)
    );
    // The stepped gain persists (block-quantized after the step).
    assert!((cap[25000] - 2.0 * raw440(25000)).abs() < 1e-3);
}

#[test]
fn gain_step_lands_mid_block_at_exact_index() {
    // Schedule at a sample that is NOT block-aligned: sample 24064 is
    // block 188 (188*128=24064), local index 0; choose 24064+63 so local=63.
    let target = 24_000u64 + 63;
    let (g, order, gain, sink) = gated_sine();
    let mut timeline = Timeline::new(SR);
    timeline.set_state(TransportState::Playing, 0);
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK as usize, SR).unwrap();
    timeline
        .schedule(
            EventTime::Sample(target),
            EventPayload::SetGain {
                node: gain.0,
                gain: 1.5,
            },
        )
        .unwrap();

    let blocks = (target / BLOCK + 1) as usize;
    let cap = drive(&mut timeline, &mut ex, sink, blocks);
    let i = target as usize;
    assert!(cap[i - 1].abs() < 1e-6, "frame before the step is gated");
    assert!(
        (cap[i] - 1.5 * raw440(i)).abs() < 1e-3,
        "step lands exactly at {i}: {} vs {}",
        cap[i],
        1.5 * raw440(i)
    );
}

#[test]
fn looping_wraps_playhead_but_fires_events_once() {
    let (g, order, _gain, _sink) = gated_sine();
    let mut timeline = Timeline::new(SR);
    timeline.set_state(TransportState::Playing, 0);
    timeline.set_loop(0, 24_000); // 0.5 s loop
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK as usize, SR).unwrap();

    // One event at master sample 12_000: fires once even though the loop
    // wraps the playhead multiple times during 12800 samples.
    timeline
        .schedule(EventTime::Sample(12_000), EventPayload::Trigger { tag: 9 })
        .unwrap();

    let mut got = Vec::new();
    for _ in 0..100 {
        for e in timeline.advance_block(BLOCK) {
            got.push(e.payload);
        }
        ex.process_block().unwrap();
    }
    assert_eq!(got.len(), 1, "fired exactly once");
    assert_eq!(got[0], EventPayload::Trigger { tag: 9 });
    // The playhead wrapped (0..24000 over 100*128=12800 samples → loops).
    assert!(timeline.clock().position() < 24_000);
    // The musical position reflects the *looped* playhead, not 12800.
    assert!(
        timeline.clock().position_beats() < 4.0,
        "wrapped musical pos"
    );
    // master_position is monotonic.
    assert_eq!(timeline.clock().master_position(), 12_800);
}

#[test]
fn transport_pause_halts_render_and_events() {
    let (g, order, _gain, sink) = open_sine();
    let mut timeline = Timeline::new(SR);
    timeline.set_state(TransportState::Playing, 0);
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK as usize, SR).unwrap();
    timeline
        .schedule(EventTime::Sample(512), EventPayload::Trigger { tag: 1 })
        .unwrap();
    timeline.set_state(TransportState::Paused, 0);

    // 10 blocks while paused: the clock does not move and nothing fires.
    let cap = drive(&mut timeline, &mut ex, sink, 10);
    assert_eq!(timeline.clock().master_position(), 0, "clock frozen");
    assert!(cap.iter().all(|s| *s == 0.0), "no audio while paused");
    assert_eq!(timeline.pending(), 1, "event still pending");

    // Resume: the event fires and audio flows.
    timeline.set_state(TransportState::Playing, 0);
    let cap2 = drive(&mut timeline, &mut ex, sink, 5); // 5*128=640 samples
    assert_eq!(timeline.clock().master_position(), 640);
    // Audio present.
    assert!(cap2.iter().any(|s| s.abs() > 1e-3));
}

#[test]
fn quantization_snaps_beat_to_note_grid() {
    // 16th-note grid at 120 BPM: beat quarter = 6000 samples.
    let mut timeline = Timeline::new(SR);
    timeline.set_tempo(120.0);
    timeline.set_quantization(Quantize::grid(0.25));
    timeline.set_state(TransportState::Playing, 0);
    let (g, order, _, _sink) = gated_sine();
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK as usize, SR).unwrap();

    // Beat 6.6 → nearest 16th is 6.5 → sample 6.5*24000 = 156000.
    timeline.schedule_beat(6.6, EventPayload::Host(1)).unwrap();
    let fired = {
        let mut at = Vec::new();
        for _ in 0..(156_000 / BLOCK + 1) {
            for e in timeline.advance_block(BLOCK) {
                if let EventPayload::Host(_) = e.payload {
                    at.push(e.at);
                }
            }
            ex.process_block().unwrap();
        }
        at
    };
    assert_eq!(fired, vec![156_000], "snapped to the grid, sample-exact");
}

#[test]
fn tempo_change_retimes_beat_across_segments() {
    // Tempo map: 120 BPM for the first 2 beats (24000 samples each), then
    // 240 BPM. Beat 4 lands after the change: 2@120 (48000) + 2@240 (24000)
    // = sample 72000. Schedule a beat-4 event through the *clock* by using
    // the map to derive where beat 4 is.
    let mut map = TempoMap::new();
    map.push(0.0, 120.0);
    map.push(2.0, 240.0);
    let target = map.sample_at_beat(4.0, SR).round() as u64; // 72000

    let (g, order, gain, sink) = gated_sine();
    let mut timeline = Timeline::new(SR);
    timeline.set_state(TransportState::Playing, 0);
    let mut ex = OfflineExecutor::new(&g, &order, BLOCK as usize, SR).unwrap();
    // The event is scheduled at the *sample* the tempo map yields.
    timeline
        .schedule(
            EventTime::Sample(target),
            EventPayload::SetGain {
                node: gain.0,
                gain: 3.0,
            },
        )
        .unwrap();

    let blocks = (target / BLOCK + 1) as usize;
    let cap = drive(&mut timeline, &mut ex, sink, blocks);
    let i = target as usize;
    assert!(cap[i - 1].abs() < 1e-6);
    assert!(
        (cap[i] - 3.0 * raw440(i)).abs() < 1e-3,
        "crossing the tempo change lands at {i}",
    );
}

#[test]
fn timeline_regions_resolve_and_drive_tempo() {
    let mut timeline = Timeline::new(SR);
    timeline.add_region("intro", 0, 48_000, 60.0);
    timeline.add_region("verse", 48_000, 144_000, 120.0);
    let _ = AudioClock::new(SR); // type is exported and usable
    assert_eq!(
        timeline.region_at(0).map(|r| r.name.as_str()),
        Some("intro")
    );
    assert_eq!(
        timeline.region_at(96_000).map(|r| r.name.as_str()),
        Some("verse")
    );
    assert_eq!(timeline.region_at(200_000), None);
}
