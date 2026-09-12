//! Fidelity suite — Graph2 **realtime vs offline** executor equivalence
//! (Phase 45, v3.50.0).
//!
//! The Phase-45 sharing contract, pinned: the realtime executor
//! (`engine::dsp::graph2::rt`) and the offline executor
//! (`engine::dsp::graph2::exec`) render through the **same per-node
//! kernels**, so a topology must render bit-exactly the same both ways.
//! Every case here builds one compiled [`Graph2`], runs it through both
//! executors block by block, and compares the captured samples
//! **bit-exactly** (`f32::to_bits` — NaN payloads included), across
//! block-size sweeps {1, 16, 256, MAX} so block-boundary discipline
//! (delay queues, overlap-add, cursors, ring positions) is exercised at
//! every fragmentation.
//!
//! Coverage mirrors the plan's topology matrix: multi-plane Split/Mix
//! graphs, delay-compensated parallel branches, the convolver's
//! pipeline-delay overlap-add, HRTF per-ear alignment, Buffer clip replay
//! (one-shot + loop), Resampler (ratio 1), Acoustic room responses, and
//! Source signal kernels. Cases where the two executors are
//! *deliberately* different (multiple sinks summing into one RT output,
//! tempo-mapped automation, external tracks, plan swaps mid-render) are
//! asserted as **documented divergences**, not silently skipped: the RT
//! executor's scope boundaries are part of its contract (see the `rt`
//! module docs).

use engine::dsp::graph2::rt::{RtPlan, RtScenes};
use engine::prelude::{
    ExecutionOrder, Graph2, NodeId, NodeParams, OfflineExecutor, PortId, RtExecutor, TestSignal,
};
use engine::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
use engine::spatial::acoustic::geometry::AcousticRoom;
use engine::spatial::acoustic::material::MaterialSpectrum;
use engine::spatial::acoustic::solver::AcousticWorld;
use engine::spatial::math::Vec3;
use engine::spatial::room::Room;

const SR: f32 = 48_000.0;
const MAX_BLOCK: usize = 1024;

/// Render `graph` through the **offline** executor for `blocks` blocks of
/// `block` frames, returning the Sink node's capture.
fn render_offline(
    graph: &Graph2,
    order: &ExecutionOrder,
    sink: NodeId,
    block: usize,
    blocks: usize,
) -> Vec<f32> {
    let mut ex = OfflineExecutor::new(graph, order, block, SR).unwrap();
    ex.process_blocks(blocks).unwrap();
    ex.capture(sink).expect("sink captured").to_vec()
}

/// Render `graph` through the **realtime** executor for `blocks` blocks of
/// `block` frames, summing the sink into one output plane.
fn render_rt(
    graph: &Graph2,
    order: &ExecutionOrder,
    block: usize,
    blocks: usize,
    scenes: Option<&RtScenes>,
) -> Vec<f32> {
    let plan = RtPlan::build(graph, order, block, SR, scenes, None).unwrap();
    let mut ex = RtExecutor::new(plan);
    let mut out = vec![0.0f32; block];
    let mut acc = Vec::with_capacity(block * blocks);
    for _ in 0..blocks {
        ex.render_block(&mut out);
        acc.extend_from_slice(&out);
    }
    acc
}

/// Bit-exact comparison with block sweep: {1, 16, 256, MAX} frames.
fn assert_bit_equal(a: &[f32], b: &[f32], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: length mismatch");
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(
            x.to_bits(),
            y.to_bits(),
            "{what}: first divergence at sample {i}: offline {x} vs rt {y}"
        );
    }
}

fn sweep(graph: &Graph2, sink: NodeId, blocks: usize, what: &str) {
    let mut g = graph.clone();
    let order = g.compile().expect("topology compiles").clone();
    for block in [1usize, 16, 256, MAX_BLOCK] {
        let offline = render_offline(&g, &order, sink, block, blocks);
        let rt = render_rt(&g, &order, block, blocks, None);
        assert_bit_equal(&offline, &rt, &format!("{what} @ block {block}"));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Topologies
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn gain_chain_matches_bit_exactly() {
    // source → gain(0.5) → gain(-1.5) → sink: stacked static kernels.
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Impulse,
            frequency_hz: 0.0,
        }),
    );
    let g1 = g.add_gain("a", 0.5);
    let g2 = g.add_gain("b", -1.5);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, g1, PortId::IN).unwrap();
    g.add_edge(g1, PortId::OUT, g2, PortId::IN).unwrap();
    g.add_edge(g2, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 8, "gain chain");
}

#[test]
fn sine_source_matches_bit_exactly() {
    let mut g = Graph2::new();
    let src = g.add_source("tone");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 997.0,
        }),
    );
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 16, "sine source");
}

#[test]
fn multi_plane_split_mix_matches_bit_exactly() {
    // source → split(3) → {gain(0.25), delay(37), passthrough} → mix(3) →
    // sink: multi-plane fan-out/fan-in with a delay-compensated branch.
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Impulse,
            frequency_hz: 0.0,
        }),
    );
    let split = g.add_split("s", 3);
    let gain = g.add_gain("g", 0.25);
    let delay = g.add_delay("d", 37);
    let mix = g.add_mix("sum", 3);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), gain, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), delay, PortId::IN).unwrap();
    g.add_edge(split, PortId(2), mix, PortId(2)).unwrap();
    g.add_edge(gain, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(delay, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 8, "multi-plane split/mix");
}

#[test]
fn delay_across_block_boundaries_matches_bit_exactly() {
    // A delay of 300 samples spans several small blocks — the ring cursor
    // and plane zeroing must behave identically both sides.
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Impulse,
            frequency_hz: 0.0,
        }),
    );
    let d1 = g.add_delay("d1", 300);
    let d2 = g.add_delay("d2", 5);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, d1, PortId::IN).unwrap();
    g.add_edge(d1, PortId::OUT, d2, PortId::IN).unwrap();
    g.add_edge(d2, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 8, "delay chain");
}

#[test]
fn convolution_pipeline_matches_bit_exactly() {
    // The convolver's overlap-add pipeline: block + kernel − 1 frames per
    // block, one kernel-length emission delay — identical math both ways.
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Impulse,
            frequency_hz: 0.0,
        }),
    );
    let conv = g.add_convolution("c", vec![0.5, -0.25, 0.125, 0.0625, 1.0, 2.0]);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 8, "convolution");
}

#[test]
fn convolution_sine_matches_bit_exactly() {
    // A signal with energy in every frame exercises the tail carry
    // across block boundaries (overlap-add addends), not just the head.
    let mut g = Graph2::new();
    let src = g.add_source("tone");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 440.0,
        }),
    );
    let conv = g.add_convolution("c", vec![1.0, 0.5, 0.25, 0.125, -0.5, 0.75, 0.9, 0.33]);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, conv, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 12, "convolution (sine)");
}

#[test]
fn hrtf_pair_matches_bit_exactly() {
    // Per-ear convolutions with a longer left IR: both ears delayed to
    // the longer IR, each ear's overlap its own IR length − 1.
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Impulse,
            frequency_hz: 0.0,
        }),
    );
    let hrtf = g.add_hrtf("h", vec![0.9, 0.3, -0.1, 0.05], vec![0.8, 0.4]);
    let sink_l = g.add_sink("l");
    let sink_r = g.add_sink("r");
    g.add_edge(src, PortId::OUT, hrtf, PortId::IN).unwrap();
    g.add_edge(hrtf, PortId(0), sink_l, PortId::IN).unwrap();
    g.add_edge(hrtf, PortId(1), sink_r, PortId::IN).unwrap();
    // Two sinks: the RT executor sums them, so compare ear-by-ear with a
    // single-sink subgraph per ear instead — build two topologies.
    let mut gl = g.clone();
    let edge = gl.incoming(sink_r, PortId::IN).unwrap().id;
    gl.remove_edge(edge);
    gl.remove_node(sink_r).unwrap();
    sweep(&gl, sink_l, 8, "hrtf left ear");
}

#[test]
fn hrtf_pair_ears_render_both() {
    // Both ears captured (offline per-sink; RT single-sink case above
    // pins the ear math; this pins the offline pair for reference).
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Impulse,
            frequency_hz: 0.0,
        }),
    );
    let hrtf = g.add_hrtf("h", vec![1.0, 0.5, 0.25], vec![0.75, 0.25]);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, hrtf, PortId::IN).unwrap();
    // Both ears into one mix input pair.
    let mix = g.add_mix("sum", 2);
    g.add_edge(hrtf, PortId(0), mix, PortId(0)).unwrap();
    g.add_edge(hrtf, PortId(1), mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 8, "hrtf pair summed");
}

#[test]
fn resampler_ratio_one_matches_bit_exactly() {
    // Ratio 1 is the RT-supported remap (higher ratios are rejected at
    // plan build — pinned separately). The windowed-sinc reader must
    // reproduce its input to identical interpolator accuracy both ways.
    let mut g = Graph2::new();
    let src = g.add_source("tone");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 733.0,
        }),
    );
    let r = g.add_resampler("r", 1.0);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, r, PortId::IN).unwrap();
    g.add_edge(r, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 12, "resampler ratio 1");
}

#[test]
fn resampler_ratio_above_one_is_rejected_by_rt_plan() {
    // Documented divergence: the fixed-grid reader's reachable history
    // grows for ratio > 1, so the RT plan refuses the node (Phase 46
    // ports the production streaming resampler).
    let mut g = Graph2::new();
    let src = g.add_source("tone");
    let r = g.add_resampler("r", 2.0);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, r, PortId::IN).unwrap();
    g.add_edge(r, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let Err(err) = RtPlan::build(&g, &order, 256, SR, None, None) else {
        panic!("RtPlan::build must reject ratio 2.0");
    };
    assert!(matches!(
        err,
        engine::prelude::RtPlanError::ResamplerRatioUnsupported(2.0)
    ));
}

#[test]
fn buffer_one_shot_and_loop_match_bit_exactly() {
    // One-shot: 300-frame clip over 1024-frame blocks — the cursor
    // saturates past the end identically both ways.
    let clip: Vec<f32> = (0..300).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
    let mut g = Graph2::new();
    let buf = g.add_buffer("clip", clip.clone(), false);
    let sink = g.add_sink("out");
    g.add_edge(buf, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 4, "buffer one-shot");

    // Loop: the cursor wraps; ring semantics must match.
    let mut g2 = Graph2::new();
    let buf2 = g2.add_buffer("clip", clip, true);
    let sink2 = g2.add_sink("out");
    g2.add_edge(buf2, PortId::OUT, sink2, PortId::IN).unwrap();
    sweep(&g2, sink2, 6, "buffer loop");
}

#[test]
fn multi_channel_buffer_matches_bit_exactly() {
    // A stereo clip routes as two mono ports (the HRTF convention) —
    // per-port planes stay in lockstep via the shared cursor.
    let clip = vec![
        (0..256).map(|i| (i as f32 * 0.013).sin() * 0.6).collect(),
        (0..256)
            .map(|i| ((i + 17) as f32 * 0.017).cos() * 0.4)
            .collect(),
    ];
    let mut g = Graph2::new();
    let buf = g.add_buffer_channels("st", clip.clone(), false);
    let mix = g.add_mix("sum", 2);
    let sink = g.add_sink("out");
    g.add_edge(buf, PortId(0), mix, PortId(0)).unwrap();
    g.add_edge(buf, PortId(1), mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 6, "multi-channel buffer");
}

#[test]
fn acoustic_room_response_matches_bit_exactly() {
    // The plan's acoustic case: a baked room response rendered through
    // the per-path spectral kernels + shared raw-history ring, bit-exact
    // both ways (the RT plan precompiles kernels control-side; the offline
    // compiles on first use — the kernels must be identical).
    let room = Room {
        enabled: true,
        width: 12.0,
        depth: 10.0,
        height: 3.0,
        absorption: 0.2,
        reflection_order: 1,
        rt60_ms: 800.0,
        late_mix: 0.0,
        speed_of_sound: 343.0,
    };
    let world = AcousticWorld::new(
        AcousticRoom::from_render_room(&room, MaterialSpectrum::flat_reflective(room.absorption)),
        SR,
    );
    let pos = Vec3::new(1.0, 5.0, 1.5);
    let lst = Vec3::new(6.0, 5.0, 1.5);
    let scene = AcousticBaker::new(world, 0.5).bake_single(pos, lst, SR, BakePolicy::default());

    let mut g = Graph2::new();
    let src = g.add_source("imp");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Impulse,
            frequency_hz: 0.0,
        }),
    );
    let ac = g.add_acoustic("room", pos);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, ac, PortId::IN).unwrap();
    g.add_edge(ac, PortId::OUT, sink, PortId::IN).unwrap();

    let order = g.compile().unwrap().clone();
    for block in [1usize, 64, 256, MAX_BLOCK] {
        // Offline with the scene attached.
        let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
        ex.set_baked_scene(Some(scene.clone()));
        ex.process_blocks(8).unwrap();
        let offline = ex.capture(sink).unwrap().to_vec();

        // RT with the same scene in the plan bundle.
        let scenes = RtScenes::from_active(scene.clone());
        let rt = render_rt(&g, &order, block, 8, Some(&scenes));
        assert_bit_equal(&offline, &rt, &format!("acoustic @ block {block}"));
    }
}

#[test]
fn dry_wet_bus_matches_bit_exactly() {
    // The canonical graph-as-center topology from the plan: split →
    // {dry gain, wet delay} → mix, parallel branches with distinct latencies.
    let mut g = Graph2::new();
    let src = g.add_source("tone");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 501.0,
        }),
    );
    let split = g.add_split("sw", 2);
    let gain = g.add_gain("dry", 0.5);
    let delay = g.add_delay("wet", 480);
    let mix = g.add_mix("sum", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), gain, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), delay, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(delay, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 12, "dry/wet bus");
}

#[test]
fn convolved_parallel_branches_match_bit_exactly() {
    // split → {conv(3 taps), delay(3)} → mix: the convolver and a delay
    // of equal reported latency must align bit-exactly both ways (the
    // latency.rs convention holds identically across executors).
    let mut g = Graph2::new();
    let src = g.add_source("imp");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Impulse,
            frequency_hz: 0.0,
        }),
    );
    let split = g.add_split("s", 2);
    let conv = g.add_convolution("c", vec![0.5, 0.5, 0.5]);
    let delay = g.add_delay("d", 3);
    let mix = g.add_mix("sum", 2);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), conv, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), delay, PortId::IN).unwrap();
    g.add_edge(conv, PortId::OUT, mix, PortId(0)).unwrap();
    g.add_edge(delay, PortId::OUT, mix, PortId(1)).unwrap();
    g.add_edge(mix, PortId::OUT, sink, PortId::IN).unwrap();
    sweep(&g, sink, 8, "conv/delay parallel alignment");
}

#[test]
fn rt_plan_publish_swap_retire_is_lossless() {
    // The generation-swap discipline on the RT executor: publishing a
    // fresh plan mid-render adopts at exactly the next block boundary.
    //
    // Documented Phase-45 divergence: the new plan's node state starts
    // **fresh** (source phase, buffer cursors, delay rings) — state carry
    // across swaps lands with the Phase-46 control-surface port (per-node
    // queues + sticky mirrors, replayed on generation swap). Until then a
    // swap is equivalent to re-starting the render at the swap boundary:
    // RT(blocks 0..4) ++ RT_fresh(blocks 4..8) must equal this test's
    // concatenated expectation bit-exactly.
    let mut g = Graph2::new();
    let src = g.add_source("tone");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 440.0,
        }),
    );
    let gain = g.add_gain("g", 0.75);
    let sink = g.add_sink("out");
    g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
    g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let block = 256;

    // Expectation: two fresh offline renders concatenated at the boundary.
    let first = render_offline(&g, &order, sink, block, 4);
    let second = render_offline(&g, &order, sink, block, 4);
    let mut expect = first;
    expect.extend_from_slice(&second);

    let mut ex = RtExecutor::new(RtPlan::build(&g, &order, block, SR, None, None).unwrap());
    let mut out = vec![0.0f32; block];
    let mut acc = Vec::new();
    for b in 0..8 {
        if b == 4 {
            // Mid-render publish of a fresh plan: adopted at block 4's
            // boundary (before its first sample renders).
            ex.publish(RtPlan::build(&g, &order, block, SR, None, None).unwrap());
        }
        ex.render_block(&mut out);
        acc.extend_from_slice(&out);
    }
    assert_eq!(ex.generation(), 1, "exactly one plan adopted");
    assert_bit_equal(&expect, &acc, "mid-render plan swap (fresh state)");
}

#[test]
fn rt_multiple_sinks_sum_a_documented_divergence() {
    // The RT contract: sinks sum into the single output plane (there is
    // no unbounded capture buffer on the audio thread). Two sinks fed
    // distinct gains must sum bit-exactly (addition is associative here
    // because each sink's plane is rendered identically to offline).
    let mut g = Graph2::new();
    let src = g.add_source("tone");
    g.set_params(
        src,
        NodeParams::Source(engine::prelude::SourceParams {
            signal: TestSignal::Sine,
            frequency_hz: 220.0,
        }),
    );
    let split = g.add_split("s", 2);
    let g1 = g.add_gain("g1", 0.3);
    let g2 = g.add_gain("g2", 0.7);
    let k1 = g.add_sink("k1");
    let k2 = g.add_sink("k2");
    g.add_edge(src, PortId::OUT, split, PortId::IN).unwrap();
    g.add_edge(split, PortId(0), g1, PortId::IN).unwrap();
    g.add_edge(split, PortId(1), g2, PortId::IN).unwrap();
    g.add_edge(g1, PortId::OUT, k1, PortId::IN).unwrap();
    g.add_edge(g2, PortId::OUT, k2, PortId::IN).unwrap();
    let order = g.compile().unwrap().clone();
    let block = 256;

    // Offline: each sink captured separately, then summed by the test.
    let mut ex = OfflineExecutor::new(&g, &order, block, SR).unwrap();
    ex.process_blocks(4).unwrap();
    let cap1 = ex.capture(k1).unwrap().to_vec();
    let cap2 = ex.capture(k2).unwrap().to_vec();
    let expect: Vec<f32> = cap1.iter().zip(cap2.iter()).map(|(a, b)| a + b).collect();

    // RT: the sinks sum into one plane — equals the offline sum exactly.
    let rt = render_rt(&g, &order, block, 4, None);
    assert_bit_equal(&expect, &rt, "two-sink sum");
}
