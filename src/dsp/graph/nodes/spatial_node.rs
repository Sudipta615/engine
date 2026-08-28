//! Phase 17 — the SpatialNode: the spatial master output stage in the
//! production graph.
//!
//! The graph's canonical chain ends with a spatial stage that renders the
//! mixed master through the engine's spatial layer:
//!
//! ```text
//! … → seek_fade → spatial → (limiter/dither)
//! ```
//!
//! **What it does.** The master's front pair is treated as a two-object
//! "program" (prog-L at `center − half_width`, prog-R at
//! `center + half_width`, at `elevation`, radius 2 m), rendered by the
//! [`BinauralRenderer`] (head model) with the scene's room (early
//! reflections + late field) and listener orientation. This is the
//! "spatialize stereo" output stage: the dry image sits on the configurable
//! virtual screen, and the room wraps ambience around the listener.
//!
//! **Multichannel masters** (>2-channel blocks) pass through bit-exact:
//! spatializing the MC master is explicitly deferred to the scene-audio
//! routing work (per-object/bed inputs into the node). The node stays
//! active-looking but processes nothing, matching the "enabled-but-idle"
//! contract of the aux bus.
//!
//! **Realtime discipline.** The scene, the renderer, and every scratch
//! plane are preallocated at construction/prepare; `process_block*` copies
//! the front pair into scratch, runs the (already zero-allocation) renderer,
//! and copies the interleaved result back — no allocation, no locks. Control
//! commands (enabled / screen / room / listener) are plain-data
//! [`super::super::controls::NodeCmd`]s applied at the block boundary, like
//! every other node. Disabled (`enabled = false`, the default) the node
//! returns before touching a sample — bit-exact, so the equivalence suites
//! stay pinned.
//!
//! The scene is node-private; its two program objects carry the master's
//! front pair. The [`config::SpatialConfig`] section configures it at
//! construction/reconfig; the live control surface
//! (`GraphControlHandle::set_spatial_*`) changes it at runtime.

use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};
use crate::dsp::graph::node::DspNode;
use crate::dsp::pipeline::{DspStageCapability, StageChannelSupport, StagePrecision};
use crate::spatial::{
    binaural::BinauralRenderer,
    level::DistanceModel,
    math::{Quat, Vec3},
    object::ObjectId,
    render::{HybridBlockInputs, SpatialRenderer},
    scene::SpatialScene,
    speaker::SpeakerLayout,
};

/// Program radius (m): the virtual screen sits 2 m in front, matching the
/// speaker presets' nominal radius.
const SCREEN_RADIUS: f32 = 2.0;

/// The SpatialNode (see the module docs).
pub struct SpatialNode {
    enabled: bool,
    /// The node-private scene: two program objects + room + listener.
    scene: SpatialScene,
    /// The head-model renderer (2-channel path).
    binaural: BinauralRenderer,
    /// Front-pair program scratch (f32 planes).
    prog_l: Vec<f32>,
    prog_r: Vec<f32>,
    /// Interleaved render scratch (`channels × MAX_AUDIO_BLOCK_FRAMES`).
    out: Vec<f32>,
    /// Virtual-screen geometry (degrees / linear).
    center_azimuth_deg: f32,
    half_width_deg: f32,
    elevation_deg: f32,
    screen_gain: f32,
    /// Listener orientation (degrees), kept for introspection.
    listener_yaw_deg: f32,
    listener_pitch_deg: f32,
    listener_roll_deg: f32,
    /// Program object ids (L/R).
    obj_l: ObjectId,
    obj_r: ObjectId,
    sample_rate: f32,
    /// Sample rate at last successful `prepare` (re-prepare on change).
    prepared_rate: f32,
    prepared: bool,
}

impl SpatialNode {
    pub fn new(sample_rate: f32) -> Self {
        let mut scene = SpatialScene::new(sample_rate.max(1.0) as u32);
        let obj_l = scene
            .create_audio_object(Vec3::new(-1.0, SCREEN_RADIUS, 0.0))
            .unwrap();
        let obj_r = scene
            .create_audio_object(Vec3::new(1.0, SCREEN_RADIUS, 0.0))
            .unwrap();
        for id in [obj_l, obj_r] {
            let obj = scene.object_mut(id).unwrap();
            // The program is the already-mixed master: no distance
            // attenuation, unity gain (screen gain is applied separately).
            obj.distance_model = DistanceModel::Linear;
            obj.gain = 1.0;
        }
        let mut node = Self {
            enabled: false,
            scene,
            binaural: BinauralRenderer::new(10.0),
            prog_l: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            prog_r: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            out: vec![0.0; MAX_CHANNELS * MAX_AUDIO_BLOCK_FRAMES],
            center_azimuth_deg: 0.0,
            half_width_deg: 30.0,
            elevation_deg: 0.0,
            screen_gain: 1.0,
            listener_yaw_deg: 0.0,
            listener_pitch_deg: 0.0,
            listener_roll_deg: 0.0,
            obj_l,
            obj_r,
            sample_rate: sample_rate.max(1.0),
            prepared_rate: -1.0,
            prepared: false,
        };
        // The graph only calls `DspNode::prepare` on rate changes, so the
        // node prepares its renderer eagerly at construction (control path).
        node.prepare(sample_rate.max(1.0), 2);
        node
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Screen geometry `(center_azimuth_deg, half_width_deg,
    /// elevation_deg, gain)`.
    pub fn screen(&self) -> (f32, f32, f32, f32) {
        (
            self.center_azimuth_deg,
            self.half_width_deg,
            self.elevation_deg,
            self.screen_gain,
        )
    }

    /// Room params `(enabled, width, depth, height, absorption,
    /// reflection_order, rt60_ms, late_mix, wet)`.
    pub fn room(&self) -> (bool, f32, f32, f32, f32, u8, f32, f32, f32) {
        let r = &self.scene.room;
        let wet = self
            .scene
            .object(self.obj_l)
            .map(|o| o.room_send)
            .unwrap_or(0.0);
        (
            r.enabled,
            r.width,
            r.depth,
            r.height,
            r.absorption,
            r.reflection_order,
            r.rt60_ms,
            r.late_mix,
            wet,
        )
    }

    /// Listener orientation `(yaw, pitch, roll)` in degrees.
    pub fn listener(&self) -> (f32, f32, f32) {
        (
            self.listener_yaw_deg,
            self.listener_pitch_deg,
            self.listener_roll_deg,
        )
    }

    /// Apply the config surface (construction / reconfig / `apply_config`).
    pub fn apply_config(&mut self, cfg: &config::SpatialConfig, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.enabled = cfg.enabled;
        self.apply_screen(
            cfg.center_azimuth_deg,
            cfg.half_width_deg,
            cfg.elevation_deg,
            cfg.gain,
        );
        let r = &cfg.room;
        self.apply_room(
            r.enabled,
            r.width,
            r.depth,
            r.height,
            r.absorption,
            r.reflection_order,
            r.rt60_ms,
            r.late_mix,
            r.wet,
        );
        self.apply_listener(
            cfg.listener_yaw_deg,
            cfg.listener_pitch_deg,
            cfg.listener_roll_deg,
        );
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Move the two program objects onto the virtual screen and set the
    /// screen gain.
    pub fn apply_screen(
        &mut self,
        center_azimuth_deg: f32,
        half_width_deg: f32,
        elevation_deg: f32,
        gain: f32,
    ) {
        self.center_azimuth_deg = center_azimuth_deg;
        self.half_width_deg = half_width_deg.clamp(0.0, 90.0);
        self.elevation_deg = elevation_deg.clamp(-90.0, 90.0);
        self.screen_gain = gain.clamp(0.0, 4.0);
        let az_l = (self.center_azimuth_deg - self.half_width_deg).to_radians();
        let az_r = (self.center_azimuth_deg + self.half_width_deg).to_radians();
        let el = self.elevation_deg.to_radians();
        let (sin_el, cos_el) = el.sin_cos();
        let pos = |az: f32| -> Vec3 {
            Vec3::new(
                SCREEN_RADIUS * cos_el * az.sin(),
                SCREEN_RADIUS * cos_el * az.cos(),
                SCREEN_RADIUS * sin_el,
            )
        };
        let pl = pos(az_l);
        let pr = pos(az_r);
        self.scene.object_mut(self.obj_l).unwrap().position = pl;
        self.scene.object_mut(self.obj_r).unwrap().position = pr;
        for id in [self.obj_l, self.obj_r] {
            self.scene.object_mut(id).unwrap().gain = self.screen_gain;
        }
    }

    /// Configure the room: geometry, reflection order, late field, and the
    /// program's reflection send (`wet`).
    #[allow(clippy::too_many_arguments)]
    pub fn apply_room(
        &mut self,
        enabled: bool,
        width: f32,
        depth: f32,
        height: f32,
        absorption: f32,
        reflection_order: u8,
        rt60_ms: f32,
        late_mix: f32,
        wet: f32,
    ) {
        let r = &mut self.scene.room;
        r.enabled = enabled;
        r.width = width.max(0.1);
        r.depth = depth.max(0.1);
        r.height = height.max(0.1);
        r.absorption = absorption.clamp(0.0, 0.99);
        r.reflection_order = reflection_order.clamp(1, 2);
        r.rt60_ms = rt60_ms.max(1.0);
        r.late_mix = late_mix.clamp(0.0, 1.0);
        let wet = wet.clamp(0.0, 1.0);
        for id in [self.obj_l, self.obj_r] {
            self.scene.object_mut(id).unwrap().room_send = wet;
        }
    }

    /// Set the listener orientation (yaw/pitch/roll, degrees).
    pub fn apply_listener(&mut self, yaw_deg: f32, pitch_deg: f32, roll_deg: f32) {
        self.listener_yaw_deg = yaw_deg;
        self.listener_pitch_deg = pitch_deg;
        self.listener_roll_deg = roll_deg;
        self.scene.listener.set_orientation(Quat::from_euler_rad(
            yaw_deg.to_radians(),
            pitch_deg.to_radians(),
            roll_deg.to_radians(),
        ));
    }

    /// Render the scene into the block planes in place (f32 path).
    fn render_block(&mut self, planes: &mut [&mut [f32]], frames: usize) {
        if !self.prepared || frames == 0 || frames > MAX_AUDIO_BLOCK_FRAMES {
            return;
        }
        if planes.len() < 2 {
            return;
        }
        self.prog_l[..frames].copy_from_slice(&planes[0][..frames]);
        self.prog_r[..frames].copy_from_slice(&planes[1][..frames]);
        let need = 2 * frames;
        let obj_refs = [
            self.prog_l[..frames].as_ref(),
            self.prog_r[..frames].as_ref(),
        ];
        let inputs = HybridBlockInputs {
            objects: &obj_refs,
            beds: &[],
            fields: &[],
        };
        let result =
            self.binaural
                .process_hybrid_block(&self.scene, &inputs, frames, &mut self.out[..need]);
        // After a successful prepare the render cannot fail; any error
        // leaves the block untouched (bit-exact passthrough) rather than
        // emitting garbage.
        if result.is_err() {
            return;
        }
        for (ch, plane) in planes.iter_mut().enumerate().take(2) {
            for f in 0..frames {
                plane[f] = self.out[f * 2 + ch];
            }
        }
    }

    /// f64 twin: demote the front pair, render in f32, promote back.
    fn render_block_f64(&mut self, planes: &mut [&mut [f64]], frames: usize) {
        if !self.prepared || frames == 0 || frames > MAX_AUDIO_BLOCK_FRAMES {
            return;
        }
        if planes.len() < 2 {
            return;
        }
        for (f, (&l, &r)) in planes[0]
            .iter()
            .zip(planes[1].iter())
            .take(frames)
            .enumerate()
        {
            self.prog_l[f] = l as f32;
            self.prog_r[f] = r as f32;
        }
        let need = 2 * frames;
        let obj_refs = [
            self.prog_l[..frames].as_ref(),
            self.prog_r[..frames].as_ref(),
        ];
        let inputs = HybridBlockInputs {
            objects: &obj_refs,
            beds: &[],
            fields: &[],
        };
        let result =
            self.binaural
                .process_hybrid_block(&self.scene, &inputs, frames, &mut self.out[..need]);
        if result.is_err() {
            return;
        }
        for (ch, plane) in planes.iter_mut().enumerate().take(2) {
            for f in 0..frames {
                plane[f] = self.out[f * 2 + ch] as f64;
            }
        }
    }
}

impl DspNode for SpatialNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "spatial",
            channel_support: StageChannelSupport::AllChannels,
            position: "post-mix, spatial master output",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.enabled && self.prepared
    }

    fn reset(&mut self) {
        // The renderer owns per-block state; nothing to clear here (the
        // scene parameters are user state, like volume).
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.sample_rate = sample_rate.max(1.0);
        if (sample_rate - self.prepared_rate).abs() > 1.0 || !self.prepared {
            // The binaural head model (stereo/headphone path). Multichannel
            // blocks pass through bit-exact — see the module docs.
            let layout = SpeakerLayout::stereo();
            self.prepared = self
                .binaural
                .prepare(&layout, sample_rate.max(1.0) as u32)
                .is_ok();
            self.prepared_rate = sample_rate.max(1.0);
        }
        // Re-apply the geometric params so a rebuilt node carries them.
        self.apply_screen(
            self.center_azimuth_deg,
            self.half_width_deg,
            self.elevation_deg,
            self.screen_gain,
        );
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if !self.enabled || !self.prepared || planes.len() != 2 {
            return;
        }
        let frames = planes[0].len();
        self.render_block(planes, frames);
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if !self.enabled || !self.prepared || planes.len() != 2 {
            return;
        }
        let frames = planes[0].len();
        self.render_block_f64(planes, frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::EngineConfig;

    fn argmax_abs(buf: &[f32]) -> usize {
        buf.iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Woodworth ITD (samples at 48 kHz) for the given azimuth in radians.
    fn woodworth_samples(azimuth_rad: f32, sample_rate: u32) -> f32 {
        const A: f32 = 0.0875;
        const C: f32 = 343.0;
        let az = azimuth_rad.abs().min(std::f32::consts::PI);
        let t = if az <= std::f32::consts::FRAC_PI_2 {
            (A / C) * (az.sin() + az)
        } else {
            (A / C) * (std::f32::consts::PI - az + az.sin())
        };
        t * sample_rate as f32
    }

    #[test]
    fn disabled_node_is_bit_exact_passthrough() {
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        let frames = 256;
        let l: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.001).sin()).collect();
        let r: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.002).cos()).collect();
        let mut l2 = l.clone();
        let mut r2 = r.clone();
        let mut planes: Vec<&mut [f32]> = vec![l2.as_mut_slice(), r2.as_mut_slice()];
        node.process_block_f32(&mut planes);
        assert_eq!(planes[0], l.as_slice());
        assert_eq!(planes[1], r.as_slice());
        // Enabled-but-unprepared (multichannel) is also bit-exact.
        let mut mc = SpatialNode::new(48_000.0);
        mc.set_enabled(true);
        let mut l2 = l.clone();
        let mut r2 = r.clone();
        // A 6-plane block (multichannel) passes through untouched even when
        // enabled.
        let mut extra: Vec<Vec<f32>> = (2..6).map(|ch| vec![ch as f32 * 0.01; frames]).collect();
        let mut refs: Vec<&mut [f32]> = vec![l2.as_mut_slice(), r2.as_mut_slice()];
        refs.extend(extra.iter_mut().map(|p| p.as_mut_slice()));
        mc.process_block_f32(&mut refs);
        assert_eq!(refs[0], l.as_slice());
        assert_eq!(refs[1], r.as_slice());
    }

    #[test]
    fn enabled_node_renders_binaural_image_with_itd() {
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_screen(0.0, 30.0, 0.0, 1.0);
        let frames = 1024;
        let mut l = vec![0.0f32; frames];
        l[64] = 1.0;
        let mut r = vec![0.0f32; frames];
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        // The left-only impulse at azimuth −30° reaches the right ear one
        // Woodworth ITD later (the contralateral ear carries the delay).
        let il = argmax_abs(planes[0]);
        let ir = argmax_abs(planes[1]);
        let expect = woodworth_samples(30f32.to_radians(), 48_000);
        assert!(ir > il, "right ear delayed ({ir} vs {il})");
        assert!(
            ((ir - il) as f32 - expect).abs() <= 4.0,
            "ITD {} samples vs {expect}",
            ir - il
        );
        // The ipsilateral (left) ear carries more energy.
        let e_l: f32 = planes[0].iter().map(|v| v * v).sum();
        let e_r: f32 = planes[1].iter().map(|v| v * v).sum();
        assert!(e_l > e_r, "ipsilateral ear stronger ({e_l} vs {e_r})");
    }

    #[test]
    fn listener_yaw_moves_the_image_across_the_ears() {
        // Listener yaws +90° (faces +X). The world-fixed screen (world +Y)
        // lands at local azimuth −90°: both program objects are now on the
        // listener's left, so the RIGHT ear becomes the contralateral ear
        // for both — its delay grows to itd(120°).
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_screen(0.0, 30.0, 0.0, 1.0);
        node.apply_listener(90.0, 0.0, 0.0);
        let frames = 1024;
        let mut l = vec![0.0f32; frames];
        l[64] = 1.0;
        let mut r = vec![0.0f32; frames];
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        let il = argmax_abs(planes[0]);
        let ir = argmax_abs(planes[1]);
        // Left object world −30° → local −120° → contralateral delay itd(120°).
        let expect = woodworth_samples(120f32.to_radians(), 48_000);
        assert!(ir > il, "right ear still contralateral ({ir} vs {il})");
        assert!(
            ((ir - il) as f32 - expect).abs() <= 6.0,
            "ITD {} samples vs {expect}",
            ir - il
        );
        // The image moved left: the left ear now carries most of the energy.
        let e_l: f32 = planes[0].iter().map(|v| v * v).sum();
        let e_r: f32 = planes[1].iter().map(|v| v * v).sum();
        assert!(e_l > e_r * 2.0, "image left ({e_l} vs {e_r})");
    }

    #[test]
    fn room_adds_a_decaying_tail_beyond_the_direct() {
        let mut off = SpatialNode::new(48_000.0);
        off.prepare(48_000.0, 2);
        off.set_enabled(true);
        off.apply_screen(0.0, 30.0, 0.0, 1.0);
        let mut on = SpatialNode::new(48_000.0);
        on.prepare(48_000.0, 2);
        on.set_enabled(true);
        on.apply_screen(0.0, 30.0, 0.0, 1.0);
        on.apply_room(true, 12.0, 10.0, 3.0, 0.2, 1, 800.0, 0.5, 0.5);
        let frames = 4096;
        let run = |node: &mut SpatialNode| -> (f32, f32) {
            let mut l = vec![0.0f32; frames];
            l[64] = 1.0;
            let mut r = vec![0.0f32; frames];
            let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
            node.process_block_f32(&mut planes);
            let direct: f32 = planes[0][60..80].iter().map(|v| v * v).sum::<f32>()
                + planes[1][60..80].iter().map(|v| v * v).sum::<f32>();
            let tail: f32 = planes[0][500..4096].iter().map(|v| v * v).sum::<f32>()
                + planes[1][500..4096].iter().map(|v| v * v).sum::<f32>();
            (direct, tail)
        };
        let (d_off, t_off) = run(&mut off);
        let (d_on, t_on) = run(&mut on);
        // The room must add substantial tail energy beyond the direct (the
        // reflections + late field land after ~280 samples), while the
        // direct stays comparable.
        assert!(d_on > 0.0, "direct present with room");
        assert!(
            t_on > t_off * 20.0,
            "room tail {} vs {} (no room)",
            t_on,
            t_off
        );
        assert!(
            (d_on - d_off).abs() / d_on < 0.5,
            "direct roughly unchanged"
        );
    }

    #[test]
    fn screen_geometry_moves_the_image() {
        // Narrow screen (half_width 0 → both objects at center): the
        // L-only impulse lands at azimuth 0 → both ears equal (no ITD).
        let mut node = SpatialNode::new(48_000.0);
        node.prepare(48_000.0, 2);
        node.set_enabled(true);
        node.apply_screen(0.0, 0.0, 0.0, 1.0);
        let frames = 1024;
        let mut l = vec![0.0f32; frames];
        l[64] = 1.0;
        let mut r = vec![0.0f32; frames];
        let mut planes: Vec<&mut [f32]> = vec![&mut l, &mut r];
        node.process_block_f32(&mut planes);
        let il = argmax_abs(planes[0]);
        let ir = argmax_abs(planes[1]);
        assert!(
            (ir as isize - il as isize).unsigned_abs() <= 2,
            "centered image: no ITD ({il} vs {ir})"
        );
    }

    #[test]
    fn control_handle_commands_apply_at_drain_and_survive_reconfig() {
        let mut graph =
            crate::dsp::graph::DspGraph::from_config(&EngineConfig::default(), 48_000.0);
        let handle = graph.control_handle();
        handle.set_spatial_enabled(true);
        handle.set_spatial_screen(0.0, 45.0, 5.0, 0.8);
        handle.set_spatial_room(true, 10.0, 8.0, 3.0, 0.3, 1, 600.0, 0.4, 0.6);
        handle.set_spatial_listener(10.0, 0.0, 0.0);
        graph.drain_queued_control();
        let s = graph.spatial();
        assert!(s.enabled());
        assert_eq!(s.screen(), (0.0, 45.0, 5.0, 0.8));
        assert!(s.room().0);
        assert_eq!(s.listener().0, 10.0);
        // The live enable survives a generation rebuild (mirrored at drain).
        let cfg = EngineConfig::default();
        graph.reconfigure(&cfg);
        graph.drain_queued_control(); // swap the pending generation in
        assert!(graph.spatial().enabled());
        // Reconfig replays the config-applied screen (the rebuild node is
        // re-seeded from config; live screen/room/listener are not carried
        // — documented).
        assert!((graph.spatial().screen().1 - 30.0).abs() < 1e-4);
    }

    #[test]
    fn multichannel_block_passes_through() {
        let mut node = SpatialNode::new(48_000.0);
        node.set_enabled(true);
        let frames = 128;
        let mut planes: Vec<Vec<f32>> = (0..6)
            .map(|ch| {
                (0..frames)
                    .map(|i| ch as f32 * 0.01 + (i as f32 * 0.001).sin())
                    .collect()
            })
            .collect();
        let before: Vec<Vec<f32>> = planes.clone();
        let mut refs: Vec<&mut [f32]> = planes.iter_mut().map(|p| p.as_mut_slice()).collect();
        node.process_block_f32(&mut refs);
        for (ch, p) in planes.iter().enumerate() {
            assert_eq!(p, &before[ch], "channel {ch} untouched");
        }
    }
}
