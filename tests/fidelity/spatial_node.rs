//! Acceptance suite for the SpatialNode (spec Phase 17 / roadmap Phase 17):
//! the spatial master output stage in the production DSP graph.
//!
//! The contract this suite pins down:
//!
//! - **Bit-exact passthrough** — the node is disabled by default and its
//!   plan step returns before touching a sample: `graph.process_block`
//!   output equals input bit-for-bit (the equivalence suites rely on this).
//! - **Spatialize stereo** — once enabled through the control handle, the
//!   master's front pair renders through the binaural head model: an
//!   L-only impulse at the ±30° screen reaches the right ear one Woodworth
//!   ITD later.
//! - **Room** — enabling the room adds a decaying tail (early reflections +
//!   late field) beyond the direct, without changing the direct materially.
//! - **Listener yaw** — the world-fixed screen moves across the ears as the
//!   listener turns.
//! - **Control surface** — enable/screen/room/listener apply at the block
//!   boundary (drain), and a live enable survives a generation rebuild
//!   (reconfig).
//! - **Multichannel blocks** — the node renders the stereo path only;
//!   multichannel masters pass through untouched (documented seam).

use config::EngineConfig;
use engine::decode::ChannelLayout;
use engine::dsp::graph::{DspGraph, DspNode};
use engine::spatial::math::Vec3;
use engine::spatial::{Ear, SpeakerLayout, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND};
use std::f32::consts::FRAC_PI_2;

const SR: u32 = 48_000;

/// Woodworth ITD in samples for an azimuth in degrees.
fn woodworth_samples(azimuth_deg: f32) -> f32 {
    let az = azimuth_deg.to_radians().abs().min(std::f32::consts::PI);
    let t = if az <= FRAC_PI_2 {
        (DEFAULT_HEAD_RADIUS / DEFAULT_SPEED_OF_SOUND) * (az.sin() + az)
    } else {
        (DEFAULT_HEAD_RADIUS / DEFAULT_SPEED_OF_SOUND) * (std::f32::consts::PI - az + az.sin())
    };
    t * SR as f32
}

fn argmax_abs(buf: &[f32]) -> usize {
    buf.iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

#[test]
fn disabled_node_is_bit_exact_passthrough() {
    let mut graph = DspGraph::from_config(&EngineConfig::default(), SR as f32);
    let frames = 256;
    let left: Vec<f32> = (0..frames)
        .map(|i| (i as f32 * 0.003).sin() * 0.5)
        .collect();
    let right: Vec<f32> = (0..frames)
        .map(|i| (i as f32 * 0.001).cos() * 0.5)
        .collect();
    let (mut l, mut r) = (left.clone(), right.clone());
    graph.process_block(&mut l, &mut r);
    assert_eq!(l, left, "left bit-exact");
    assert_eq!(r, right, "right bit-exact");
    // The spatial step is present in the plan but inactive.
    assert!(!graph.spatial().enabled());
    assert!(!graph.spatial().is_active());
}

#[test]
fn enabled_node_renders_binaural_itd_on_the_graph_output() {
    let mut graph = DspGraph::from_config(&EngineConfig::default(), SR as f32);
    graph.set_spatial_enabled(true);
    graph.set_spatial_screen(0.0, 30.0, 0.0, 1.0);
    graph.drain_queued_control();
    assert!(graph.spatial().enabled());

    let frames = 1024;
    let mut l = vec![0.0f32; frames];
    l[64] = 1.0;
    let mut r = vec![0.0f32; frames];
    graph.process_block(&mut l, &mut r);
    // L-only impulse at the screen's left edge (azimuth −30°): the right
    // (contralateral) ear hears it one Woodworth ITD later.
    let il = argmax_abs(&l);
    let ir = argmax_abs(&r);
    let expect = woodworth_samples(30.0);
    assert!(ir > il, "right ear delayed ({ir} vs {il})");
    assert!(
        ((ir - il) as f32 - expect).abs() <= 4.0,
        "ITD {} samples vs {expect}",
        ir - il
    );
    // The ipsilateral ear carries more energy.
    let e_l: f32 = l.iter().map(|v| v * v).sum();
    let e_r: f32 = r.iter().map(|v| v * v).sum();
    assert!(e_l > e_r, "ipsilateral ear stronger ({e_l} vs {e_r})");
}

#[test]
fn room_adds_a_decaying_tail_beyond_the_direct() {
    let run = |room_on: bool| -> (f32, f32) {
        let mut graph = DspGraph::from_config(&EngineConfig::default(), SR as f32);
        graph.set_spatial_enabled(true);
        graph.set_spatial_screen(0.0, 30.0, 0.0, 1.0);
        if room_on {
            graph.set_spatial_room(true, 12.0, 10.0, 3.0, 0.2, 1, 800.0, 0.5, 0.5);
        }
        graph.drain_queued_control();
        let frames = 4096;
        let mut l = vec![0.0f32; frames];
        l[64] = 1.0;
        let mut r = vec![0.0f32; frames];
        graph.process_block(&mut l, &mut r);
        let direct: f32 = l[60..80].iter().map(|v| v * v).sum::<f32>()
            + r[60..80].iter().map(|v| v * v).sum::<f32>();
        let tail: f32 = l[500..4096].iter().map(|v| v * v).sum::<f32>()
            + r[500..4096].iter().map(|v| v * v).sum::<f32>();
        (direct, tail)
    };
    let (d_off, t_off) = run(false);
    let (d_on, t_on) = run(true);
    assert!(d_on > 0.0, "direct present with room");
    assert!(t_on > t_off * 20.0, "room tail {t_on} vs no-room {t_off}");
    assert!(
        (d_on - d_off).abs() / d_on < 0.5,
        "direct roughly unchanged"
    );
}

#[test]
fn listener_yaw_moves_the_screen_across_the_ears() {
    let mut graph = DspGraph::from_config(&EngineConfig::default(), SR as f32);
    graph.set_spatial_enabled(true);
    graph.set_spatial_screen(0.0, 30.0, 0.0, 1.0);
    graph.set_spatial_listener(90.0, 0.0, 0.0);
    graph.drain_queued_control();
    let frames = 1024;
    let mut l = vec![0.0f32; frames];
    l[64] = 1.0;
    let mut r = vec![0.0f32; frames];
    graph.process_block(&mut l, &mut r);
    // Facing +X, the world-fixed screen sits at the listener's left: the
    // right ear is contralateral for both program objects — its delay grows
    // to itd(120°), and the left ear dominates.
    let il = argmax_abs(&l);
    let ir = argmax_abs(&r);
    let expect = woodworth_samples(120.0);
    assert!(ir > il, "right ear contralateral ({ir} vs {il})");
    assert!(
        ((ir - il) as f32 - expect).abs() <= 6.0,
        "ITD {} vs {expect}",
        ir - il
    );
    let e_l: f32 = l.iter().map(|v| v * v).sum();
    let e_r: f32 = r.iter().map(|v| v * v).sum();
    assert!(e_l > e_r * 2.0, "image left ({e_l} vs {e_r})");
}

#[test]
fn live_enable_survives_a_generation_rebuild() {
    let mut graph = DspGraph::from_config(&EngineConfig::default(), SR as f32);
    graph.set_spatial_enabled(true);
    graph.set_spatial_screen(0.0, 45.0, 5.0, 0.8);
    graph.drain_queued_control();
    // Rebuild from a default config: the live enable is mirrored at drain
    // and replayed into the fresh generation; screen/room/listener fall back
    // to the config (documented).
    graph.reconfigure(&EngineConfig::default());
    graph.drain_queued_control();
    assert!(graph.spatial().enabled(), "enable survives the swap");
    assert!(
        (graph.spatial().screen().1 - 30.0).abs() < 1e-4,
        "screen re-seeded from config"
    );
    // The rebuilt node still renders (a quick impulse stays finite).
    let frames = 256;
    let mut l = vec![0.0f32; frames];
    l[0] = 1.0;
    let mut r = vec![0.0f32; frames];
    graph.process_block(&mut l, &mut r);
    assert!(l.iter().all(|v| v.is_finite()));
    assert!(r.iter().all(|v| v.is_finite()));
}

#[test]
fn multichannel_master_passes_through_bit_exact() {
    // The node renders the stereo path only (documented seam): an MC block
    // through the graph's multichannel entry point stays untouched.
    let mut graph = DspGraph::from_config(&EngineConfig::default(), SR as f32);
    graph.set_spatial_enabled(true);
    graph.drain_queued_control();
    let layout = ChannelLayout::FivePointOne;
    let channels = layout.channel_count();
    let frames = 128;
    let run = |enabled: bool| -> Vec<f32> {
        let mut g = DspGraph::from_config(&EngineConfig::default(), SR as f32);
        g.set_spatial_enabled(enabled);
        g.drain_queued_control();
        let mut interleaved: Vec<f32> = (0..channels * frames)
            .map(|i| (i as f32 * 0.001).sin() * 0.25)
            .collect();
        g.process_block_multichannel(&mut interleaved, channels);
        interleaved
    };
    let on = run(true);
    let off = run(false);
    assert_eq!(on, off, "MC output bit-exact with the node enabled");
    let _ = graph; // the local `graph` was built with the node enabled
}

#[test]
fn renderer_layout_helpers_line_up_with_the_head_model() {
    // The virtual screen's ±30° half-width matches the ITD formula the
    // acceptance tests measure against (guards the fixtures, not the node).
    let az = -30f32;
    let itd = woodworth_samples(az.abs());
    assert!(itd > 12.0 && itd < 13.0, "30° ITD ≈ 12.5 samples: {itd}");
    // Ear type sanity for the fixtures.
    assert_eq!(Ear::Left.index(), 0);
    let _ = SpeakerLayout::stereo();
    let _ = Vec3::ZERO;
}
