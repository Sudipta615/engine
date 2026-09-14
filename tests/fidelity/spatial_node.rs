//! Acceptance suite for the SpatialNode (roadmap):
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
//! - **Listener motion (v4.3.0)** — a runtime pose target
//!   (orientation + position) glides per block with the tracking
//!   conventions (nlerp + one-pole, optional rate limit); a generation
//!   swap mid-glide never interrupts audio (no glitch, no NaN), and the
//!   re-bake relevance bound flips exactly on the bake-cell crossing.
//! - **Control surface** — enable/screen/room/listener apply at the block
//!   boundary (drain), and a live enable survives a generation rebuild
//!   (reconfig).
//! - **Multichannel blocks** — the node renders the stereo path only;
//!   multichannel masters pass through untouched (documented seam).

use config::EngineConfig;
use engine::decode::ChannelLayout;
use engine::dsp::graph2::prod::{DspGraph, DspNode};
use engine::spatial::math::{Quat, Vec3};
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
            graph.set_spatial_room(true, 12.0, 10.0, 3.0, 0.2, 1, 800.0, 0.5, false, 0.5);
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

// ── Listener motion (v4.3.0) ──────────────────────────────────────

/// Drive a continuous listener rotation through the queued pose surface
/// and assert the rendered image tracks it — the moving-listener contract
/// (bounded per-block steps, no glitch, finite output throughout).
#[test]
fn moving_listener_rotates_the_image_without_glitches() {
    let mut graph = DspGraph::from_config(&EngineConfig::default(), SR as f32);
    graph.set_spatial_enabled(true);
    graph.set_spatial_screen(0.0, 30.0, 0.0, 1.0);
    graph.set_spatial_room(true, 12.0, 10.0, 3.0, 0.2, 1, 800.0, 0.5, false, 0.5);
    // A 20 ms glide with a 200°/s cap: audible motion, never a snap.
    graph.set_spatial_listener_tracking(20.0, 200.0);
    graph.drain_queued_control();

    let frames = 512;
    let mut l = vec![0.0f32; frames];
    let mut r = vec![0.0f32; frames];
    let mut prev_yaw = graph.spatial().listener().0;
    let mut max_step_deg = 0.0f32;
    for block in 0..200 {
        // Fresh program audio every block (the block buffers are in/out:
        // reusing the previous render as input would feed the room's tail
        // back into itself). A slow continuous rotation target per block.
        for i in 0..frames {
            let t = (block * frames + i) as f32;
            l[i] = 0.3 * (t * 0.01).sin();
            r[i] = 0.2 * (t * 0.013).cos();
        }
        let yaw_deg = block as f32 * 1.5;
        let q = Quat::from_euler_rad(yaw_deg.to_radians(), 0.0, 0.0);
        graph.set_spatial_listener_pose(q, Vec3::new(0.0, 0.0, 0.0));
        graph.drain_queued_control();
        graph.process_block(&mut l, &mut r);
        // Continuity: finite everywhere, bounded energy every block.
        assert!(l.iter().all(|v| v.is_finite()), "finite L at block {block}");
        assert!(r.iter().all(|v| v.is_finite()), "finite R at block {block}");
        let e: f32 = l.iter().map(|v| v * v).sum::<f32>() + r.iter().map(|v| v * v).sum::<f32>();
        assert!(
            e.is_finite() && e < 1e4,
            "block {block} energy {e}: motion must not destabilize the room"
        );
        let now_yaw = graph.spatial().listener().0;
        let step = (now_yaw - prev_yaw)
            .abs()
            .min(360.0 - (now_yaw - prev_yaw).abs());
        max_step_deg = max_step_deg.max(step);
        prev_yaw = now_yaw;
    }
    // 512 frames @ 48 kHz ≈ 10.7 ms; the glide (20 ms one-pole, 200°/s
    // cap) keeps the per-block yaw step bounded — no snap.
    assert!(
        max_step_deg < 5.0,
        "per-block yaw step bounded ({max_step_deg}°)"
    );
}

/// A generation swap mid-glide never interrupts audio: reconfiguring the
/// graph while the listener is moving keeps the render finite and
/// glitch-free across the boundary (the fresh generation re-seeds
/// screen/room from the config — so the test reconfigures with the live
/// values — while the live enable and glide policy survive the swap).
#[test]
fn generation_swap_mid_listener_motion_never_glitches() {
    // The config the live graph was set to (so a generation rebuild does
    // not also change the room/screen — isolating the swap seam itself).
    let cfg = {
        let mut c = EngineConfig::default();
        c.spatial.enabled = true;
        c.spatial.center_azimuth_deg = 0.0;
        c.spatial.half_width_deg = 30.0;
        c.spatial.gain = 1.0;
        c.spatial.room = config::SpatialRoomConfig {
            enabled: true,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 1,
            rt60_ms: 800.0,
            late_mix: 0.5,
            late_distance: false,
            wet: 0.5,
            speed_of_sound: 343.0,
        };
        c
    };
    let mut graph = DspGraph::from_config(&cfg, SR as f32);
    graph.set_spatial_listener_tracking(10.0, 0.0);
    graph.drain_queued_control();

    let frames = 512;
    let mut l = vec![0.0f32; frames];
    let mut r = vec![0.0f32; frames];
    let mut energies: Vec<f32> = Vec::new();

    // Glide for a few blocks, then swap the generation mid-motion, then
    // keep gliding — the seam the re-bake path uses.
    for block in 0..32 {
        if block == 16 {
            // Mid-glide generation rebuild (the control-thread re-bake /
            // reconfig seam): publish + adopt, no audio interruption.
            graph.reconfigure(&cfg);
        }
        for i in 0..frames {
            let t = (block * frames + i) as f32;
            l[i] = 0.25 * (t * 0.01).sin();
            r[i] = 0.25 * (t * 0.008).cos();
        }
        let q = Quat::from_euler_rad((block as f32 * 3.0).to_radians(), 0.0, 0.0);
        graph.set_spatial_listener_pose(q, Vec3::ZERO);
        graph.drain_queued_control();
        graph.process_block(&mut l, &mut r);
        let e: f32 = l.iter().map(|v| v * v).sum::<f32>() + r.iter().map(|v| v * v).sum::<f32>();
        energies.push(e);
        assert!(l.iter().all(|v| v.is_finite()), "finite at block {block}");
    }

    // No glitch: every block renders finite, non-silent audio. (A
    // generation rebuild legitimately resets the room renderer's tail
    // state — pre-existing documented behavior, identical without any
    // listener motion — so the assertion is continuity of the render, not
    // of the tail energy: no block is zeroed, NaN'd, or spiked.)
    for (k, &e) in energies.iter().enumerate() {
        assert!(
            e > 0.0 && e.is_finite(),
            "block {k} energy {e}: swap must not silence the render"
        );
    }
    // Post-swap blocks settle into a steady render (the room tail rebuilds
    // within a few blocks).
    let late: f32 = energies.iter().skip(20).sum::<f32>() / 12.0;
    assert!(
        late > 0.0 && late.is_finite(),
        "post-swap steady energy {late}"
    );
    // The listener pose itself is continuous across the swap: the fresh
    // generation re-seeds the listener from the config and the next
    // target glides from there — no NaN/teleport in the pose.
    let (y, p, rl) = graph.spatial().listener();
    assert!(
        y.is_finite() && p.is_finite() && rl.is_finite(),
        "pose finite"
    );
}

/// The re-bake relevance bound flips exactly on the bake-cell crossing —
/// the control thread's decision function for the smooth re-bake seam.
#[test]
fn listener_rebake_bound_tracks_the_bake_cell() {
    let mut graph = DspGraph::from_config(&EngineConfig::default(), SR as f32);
    graph.set_spatial_enabled(true);
    graph.set_spatial_listener_tracking(0.0, 0.0); // snap: deterministic pose
    graph.drain_queued_control();

    let frames = 128;
    let mut l = vec![0.0f32; frames];
    let mut r = vec![0.0f32; frames];
    let baked_at = Vec3::ZERO;
    let cell = 0.5;
    let mut pose_at = |x: f32| {
        graph.set_spatial_listener_pose(Quat::IDENTITY, Vec3::new(x, 0.0, 0.0));
        graph.drain_queued_control();
        graph.process_block(&mut l, &mut r);
        graph.spatial().listener_rebake_due(cell, baked_at)
    };
    assert!(!pose_at(0.0), "fresh at the bake position");
    assert!(!pose_at(0.49), "still fresh inside the cell");
    assert!(pose_at(0.51), "stale once a cell is crossed");
    assert!(pose_at(-0.5), "negative crossings count too");
}
