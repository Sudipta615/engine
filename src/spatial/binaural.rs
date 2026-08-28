//! Binaural renderer — a head model instead of a speaker array (spec Part
//! VII §47–48, §62, §136).
//!
//! This renderer outputs **two channels** (the ears, typically headphones)
//! and renders the *entire hybrid scene* — objects, beds, fields, and the
//! room — through the documented head model ([`crate::spatial::hrtf`]):
//! the Woodworth interaural time difference and the Duda-Martens head-shadow
//! shelf. There are no speakers to pan between; the "pan" *is* the head.
//!
//! ## Cue synthesis per class
//!
//! - **Objects** — the level chain (distance · directivity · occlusion) is
//!   shared with the speaker renderers; the direction then becomes two ear
//!   paths: the contralateral ear's signal is delayed by the Woodworth ITD
//!   (through a fractional delay line, so moving sources glide) and both
//!   ears are filtered by the head-shadow shelf (α from the azimuth). The
//!   LFE send folds into both ears at `1/√2` (the classic bass fold).
//!   Angular-region **spread** samples the exact direction plus the ring
//!   (weighted `1−s` / `s/3`), each direction producing its own ITD + shadow
//!   path — a widened source blurs the interaural cues instead of moving.
//! - **Beds** — each authored channel folds by its *semantic role's*
//!   canonical azimuth (FL → −30°, SL → −110°, …), so a 5.1 bed "sounds
//!   like 5.1" on headphones; the LFE role folds at `1/√2` to both ears.
//! - **Fields & the room's late field** — diffuse content must not become
//!   a phantom point, so it is decoded onto a **virtual 8-speaker ring**
//!   (via the ambisonic bus + the field mixer's `√N` compensation and
//!   per-speaker decorrelation) and *then* head-modeled: each virtual
//!   speaker gets its own ITD + static shadow. The decorrelated copies sum
//!   at each ear into surrounding ambience, exactly the diffuse property
//!   the speaker renderers get from their layout.
//! - **Room early reflections** — image sources are binauralized directly:
//!   per image, the excess-path delay rides the per-object room ring, the
//!   Woodworth ITD is added per ear (fractional), and each (image, ear)
//!   path has its own shadow shelf and smoothed tap gain.
//!
//! ## Documented simplifications
//!
//! - No elevation cues: the model is azimuth-only (the HRTF's spectral
//!   elevation cues are the documented future seam). An elevated source is
//!   projected onto the horizontal plane and keeps its full level — a
//!   source overhead is not artificially attenuated.
//! - No constant-power invariant: the head diffracts (the ipsilateral ear
//!   *boosts* off-axis highs), so energy grows off-axis exactly as with a
//!   real head. Symmetry is the invariant: mirroring a source swaps the
//!   ears bit-for-bit.
//!
//! ## Realtime discipline
//!
//! All per-path state (ITD rings, shelf filters, reflection gains, the
//! virtual ring) is preallocated flat at `prepare`; the per-block hot path
//! is bounded arithmetic — verified by `tests/fidelity/realtime_allocation.rs`.

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::decode::ChannelId;
use crate::spatial::object::MAX_SPATIAL_OBJECTS;

use super::bed::MAX_BEDS;
use super::directivity::listener_angle_rad;
use super::field::AmbisonicFieldMixer;
use super::hrtf::{
    ear_delay_sec, head_shadow_alpha, max_itd_sec, read_delayed, Ear, HeadShadow,
    DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND,
};
use super::math::Vec3;
use super::occlusion::OcclusionState;
use super::render::{HybridBlockInputs, RenderError, SpatialRenderer};
use super::room::{EarlyReflections, ListenerImage, RoomLateField, MAX_IMAGES};
use super::scene::{ListenerTransform, SpatialScene};
use super::speaker::SpeakerLayout;
use super::spread::{ring_directions, RING_SAMPLES, SPREAD_MAX_HALF_ANGLE_RAD};

/// Direction budget per object: the exact direction plus the 3 spread ring
/// samples (`1 + RING_SAMPLES`).
const MAX_DIRS: usize = 4;

/// Bed channel ceiling (7.1.4 has 12; bounded preallocation).
const MAX_BED_CHANNELS: usize = 16;

/// Virtual speaker ring for diffuse content (fields + room late field).
/// 8 speakers at 45° around the horizontal plane.
pub const VIRTUAL_RING_SPEAKERS: usize = 8;

/// Virtual ring azimuths, degrees (`0` = front, `+` = right).
const VIRTUAL_AZIMUTHS_DEG: [f32; VIRTUAL_RING_SPEAKERS] =
    [0.0, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0];

/// LFE bass fold into both ears (equal power: `1/√2` each).
const LFE_FOLD: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Canonical binaural azimuth for a bed channel role (nominal ITU angles,
/// spec §19–20). `Unknown` falls back to front (documented).
fn role_azimuth(role: ChannelId) -> f32 {
    use ChannelId::*;
    match role {
        FrontLeft => (-30.0_f32).to_radians(),
        FrontRight => 30.0_f32.to_radians(),
        Center => 0.0,
        SideLeft => (-110.0_f32).to_radians(),
        SideRight => 110.0_f32.to_radians(),
        RearLeft => (-135.0_f32).to_radians(),
        RearRight => 135.0_f32.to_radians(),
        BackCenter => 180.0_f32.to_radians(),
        TopFrontLeft => (-30.0_f32).to_radians(),
        TopFrontRight => 30.0_f32.to_radians(),
        TopRearLeft => (-135.0_f32).to_radians(),
        TopRearRight => 135.0_f32.to_radians(),
        Lfe => 0.0, // never used — the LFE role takes the fold path
        Unknown(_) => 0.0,
    }
}

/// The virtual ring layout: 8 speakers at unit radius on the horizontal
/// plane, no roles, no LFE.
fn virtual_ring_layout() -> SpeakerLayout {
    let positions: Vec<Vec3> = VIRTUAL_AZIMUTHS_DEG
        .iter()
        .map(|&deg| {
            let az = deg.to_radians();
            Vec3::new(az.sin(), az.cos(), 0.0)
        })
        .collect();
    SpeakerLayout::custom(positions)
}

/// The binaural renderer: renders a full hybrid scene to a stereo (two-ear)
/// interleaved buffer. `smooth_ms <= 0` disables smoothing (= exact head
/// cues), useful for precise tests.
#[derive(Debug)]
pub struct BinauralRenderer {
    head_radius: f32,
    speed_of_sound: f32,
    sample_rate: f32,
    /// Per-block one-pole factor for all smoothed state (α, tap gains).
    smooth: f32,

    out_trim: [f32; 2],
    prepared: bool,

    /// Per-object occlusion low-pass state.
    occ: Vec<OcclusionState>,
    /// Per-object ITD ring (flat `MAX_OBJECTS × itd_len`); both ears read
    /// the same ring at their own fractional delays.
    obj_itd: Vec<f32>,
    /// Per-(object, direction, ear) head-shadow shelf.
    obj_shelf: Vec<HeadShadow>,
    /// ITD ring length (from the head parameters and sample rate).
    itd_len: usize,
    /// Global ring write cursor (every ring is written each frame).
    itd_pos: usize,

    /// Room early reflections (per-object room rings + send plane).
    room_er: EarlyReflections,
    room_late: RoomLateField,
    /// Per-(object, image, ear) reflection shadow shelf + smoothed tap gain.
    ref_shelf: Vec<HeadShadow>,
    ref_gain: Vec<f32>,
    late_scratch: Vec<f32>,

    /// Per-(bed, channel) ITD ring + shelf (static azimuth).
    bed_itd: Vec<f32>,
    bed_shelf: Vec<HeadShadow>,

    /// Diffuse path: field mixer on the virtual ring, then per-(virtual
    /// speaker, ear) ITD + shadow.
    fields: AmbisonicFieldMixer,
    virtual_trim: Vec<f32>,
    virtual_scratch: Vec<f32>,
    vs_itd: Vec<f32>,
    vs_shelf: Vec<HeadShadow>,
}

impl BinauralRenderer {
    /// Create a binaural renderer. `smooth_ms <= 0` disables smoothing
    /// (exact cues), matching the panner's convention.
    pub fn new(smooth_ms: f32) -> Self {
        Self {
            head_radius: DEFAULT_HEAD_RADIUS,
            speed_of_sound: DEFAULT_SPEED_OF_SOUND,
            sample_rate: 48_000.0,
            smooth: if smooth_ms <= 0.0 {
                1.0
            } else {
                (1.0 / (1.0 + smooth_ms / 8.0)).clamp(0.0, 1.0)
            },
            out_trim: [1.0; 2],
            prepared: false,
            occ: vec![OcclusionState::default(); MAX_SPATIAL_OBJECTS],
            obj_itd: Vec::new(),
            obj_shelf: vec![HeadShadow::new(); MAX_SPATIAL_OBJECTS * MAX_DIRS * Ear::COUNT],
            itd_len: 1,
            itd_pos: 0,
            room_er: EarlyReflections::new(),
            room_late: RoomLateField::new(),
            ref_shelf: vec![HeadShadow::new(); MAX_SPATIAL_OBJECTS * MAX_IMAGES * Ear::COUNT],
            ref_gain: vec![0.0; MAX_SPATIAL_OBJECTS * MAX_IMAGES * Ear::COUNT],
            late_scratch: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            bed_itd: Vec::new(),
            bed_shelf: vec![HeadShadow::new(); MAX_BEDS * MAX_BED_CHANNELS * Ear::COUNT],
            fields: AmbisonicFieldMixer::new(),
            virtual_trim: vec![1.0; VIRTUAL_RING_SPEAKERS],
            virtual_scratch: vec![0.0; VIRTUAL_RING_SPEAKERS * MAX_AUDIO_BLOCK_FRAMES],
            vs_itd: Vec::new(),
            vs_shelf: vec![HeadShadow::new(); VIRTUAL_RING_SPEAKERS * Ear::COUNT],
        }
    }

    /// Override the head parameters (radius in metres, speed of sound in
    /// m/s). Must be set before `prepare`.
    pub fn set_head_parameters(&mut self, radius: f32, speed: f32) -> &mut Self {
        self.head_radius = radius;
        self.speed_of_sound = speed;
        self
    }

    /// Expose the current ITD ring length (samples) — used by tests to size
    /// measurement windows.
    pub fn itd_len(&self) -> usize {
        self.itd_len
    }

    /// Expose the fractional ITD in samples for an azimuth/ear — used by
    /// tests to predict lag positions.
    pub fn itd_samples(&self, azimuth: f32, ear: Ear) -> f32 {
        ear_delay_sec(azimuth, ear, self.head_radius, self.speed_of_sound) * self.sample_rate
    }

    fn prepare_layout(
        &mut self,
        layout: &SpeakerLayout,
        sample_rate: u32,
    ) -> Result<(), RenderError> {
        layout.validate()?;
        // A binaural renderer *is* the head: exactly two enabled, non-LFE
        // speakers (stereo / headphone layout).
        if layout.speakers.len() != 2
            || layout.speakers[0].is_lfe
            || layout.speakers[1].is_lfe
            || !layout.speakers[0].enabled
            || !layout.speakers[1].enabled
        {
            return Err(RenderError::InvalidLayout);
        }
        self.sample_rate = sample_rate as f32;
        self.itd_len = (max_itd_sec(self.head_radius, self.speed_of_sound) * sample_rate as f32)
            .ceil() as usize
            + 4;
        self.itd_pos = 0;
        self.obj_itd = vec![0.0; MAX_SPATIAL_OBJECTS * self.itd_len];
        self.bed_itd = vec![0.0; MAX_BEDS * MAX_BED_CHANNELS * self.itd_len];
        self.vs_itd = vec![0.0; VIRTUAL_RING_SPEAKERS * self.itd_len];

        for s in self.obj_shelf.iter_mut() {
            s.prepare(self.sample_rate, self.head_radius, self.speed_of_sound);
        }
        for s in self.ref_shelf.iter_mut() {
            s.prepare(self.sample_rate, self.head_radius, self.speed_of_sound);
        }
        for s in self.bed_shelf.iter_mut() {
            s.prepare(self.sample_rate, self.head_radius, self.speed_of_sound);
        }
        for s in self.vs_shelf.iter_mut() {
            s.prepare(self.sample_rate, self.head_radius, self.speed_of_sound);
        }
        // The virtual ring's head cues are static: pin α per (speaker, ear).
        for (k, &deg) in VIRTUAL_AZIMUTHS_DEG.iter().enumerate() {
            let az = deg.to_radians();
            for ear in 0..Ear::COUNT {
                let e = Ear::from_index(ear);
                self.vs_shelf[k * Ear::COUNT + ear].set_target(head_shadow_alpha(az, e), 1.0);
            }
        }

        self.out_trim = [
            layout.speakers[0].gain * layout.calibration.trim_gain(layout.speakers[0].id),
            layout.speakers[1].gain * layout.calibration.trim_gain(layout.speakers[1].id),
        ];
        self.room_er.prepare(2, sample_rate, self.smooth);
        self.room_late.prepare(sample_rate);
        self.fields.prepare(&virtual_ring_layout(), sample_rate)?;
        self.virtual_scratch.fill(0.0);
        self.prepared = true;
        Ok(())
    }

    /// Render the scene's objects through the head model.
    ///
    /// Allocation-free after `prepare`: per-object state is keyed by the
    /// stable store slot and all scratch is preallocated (realtime
    /// discipline, spec §71–75).
    fn render_objects(
        &mut self,
        scene: &SpatialScene,
        inputs: &[&[f32]],
        frames: usize,
        out: &mut [f32],
    ) {
        let xf = ListenerTransform::from_listener(&scene.listener);
        let room_on = scene.room.enabled && scene.room.reflection_order >= 1;
        if room_on {
            self.room_er.begin_block(frames);
        }
        let mut dirs = [(Vec3::ZERO, 0.0f32); MAX_DIRS];
        let mut ring = [Vec3::ZERO; RING_SAMPLES];
        let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
        let mut ref_az = [0.0f32; MAX_IMAGES];

        for (obj_ordinal, (slot, obj)) in scene.objects.iter_enabled().enumerate() {
            let obj_idx = slot; // stable id = store slot
            let local = xf.apply_to_point(obj.position);
            let dist = local.length();
            let dir_v = local.normalized().unwrap_or(Vec3::Y);

            // Level chain (spec §68): source gain · distance · directivity ·
            // occlusion transmission. (No cos-elevation term — the head
            // model is azimuth-only; elevation is not attenuated, documented
            // in the module docs.)
            let dist_gain = obj
                .distance_model
                .distance_gain(dist, obj.reference_distance);
            let dir_gain = obj.directivity.gain_at(listener_angle_rad(
                obj.source_orientation,
                scene.listener.orientation,
                local,
            ));
            let occ = obj.occlusion;
            let (occ_gain, occ_coeffs) = if occ.amount > 0.0 {
                let tr = occ.transmission(self.sample_rate);
                let coeffs = self.occ[obj_idx].coeffs(tr.cutoff_hz, self.sample_rate, self.smooth);
                (tr.gain(), Some(coeffs))
            } else {
                (1.0, None)
            };
            let obj_gain = obj.gain * dist_gain * dir_gain * occ_gain;

            // Directions: the exact direction plus (for spread > 0) the
            // angular-region ring samples, each with its own head cues.
            let s = obj.spread.clamp(0.0, 1.0);
            dirs[0] = (dir_v, 1.0 - s);
            let mut n_dirs = 1usize;
            if s > 0.0 {
                let n_ring = ring_directions(dir_v, s * SPREAD_MAX_HALF_ANGLE_RAD, &mut ring);
                if n_ring > 0 {
                    let w = s / n_ring as f32;
                    for rd in ring.iter().take(n_ring) {
                        if n_dirs < MAX_DIRS {
                            dirs[n_dirs] = (*rd, w);
                            n_dirs += 1;
                        }
                    }
                }
            }

            // Per-block head-shadow targets for each (direction, ear).
            for (d, &(dir, _)) in dirs.iter().take(n_dirs).enumerate() {
                let az = dir.azimuth_rad();
                for ear in 0..Ear::COUNT {
                    let e = Ear::from_index(ear);
                    let idx = obj_idx * (MAX_DIRS * Ear::COUNT) + d * Ear::COUNT + ear;
                    self.obj_shelf[idx].set_target(head_shadow_alpha(az, e), self.smooth);
                }
            }

            // Room: image sources for this object.
            let mut n_img = 0usize;
            if room_on && obj.room_send > 0.0 {
                n_img = self.room_er.images_for_object(
                    &scene.room,
                    scene.listener.position,
                    obj.position,
                    &mut imgs,
                );
                for i in 0..n_img {
                    let ldir = xf.apply_to_direction(imgs[i].dir);
                    let az = ldir.azimuth_rad();
                    ref_az[i] = az;
                    let dg = obj
                        .distance_model
                        .distance_gain(imgs[i].dist, obj.reference_distance);
                    let target = obj.gain * obj.room_send * imgs[i].coeff * dg;
                    for ear in 0..Ear::COUNT {
                        let idx = obj_idx * (MAX_IMAGES * Ear::COUNT) + i * Ear::COUNT + ear;
                        let e = Ear::from_index(ear);
                        self.ref_shelf[idx].set_target(head_shadow_alpha(az, e), self.smooth);
                        let prev = self.ref_gain[idx];
                        self.ref_gain[idx] = if self.smooth >= 1.0 {
                            target
                        } else {
                            prev + self.smooth * (target - prev)
                        };
                    }
                }
            }

            let input = inputs.get(obj_ordinal).copied().unwrap_or(&[]);
            if input.len() < frames {
                continue;
            }
            let row = obj_idx * self.itd_len;
            let rl = self.itd_len;
            for frame in 0..frames {
                let mut s = input[frame];
                // Occlusion low-passes before the head model (spec §43); the
                // filtered sample feeds the direct paths, the LFE fold and
                // the room.
                if let Some(c) = occ_coeffs {
                    s = self.occ[obj_idx].process(s, &c);
                }
                let cursor = (self.itd_pos + frame) % rl;
                self.obj_itd[row + cursor] = s;

                // Direct paths: each (direction, ear) reads its own ITD
                // delay and applies its own shadow shelf.
                for (d, &(dir, w)) in dirs.iter().take(n_dirs).enumerate() {
                    let az = dir.azimuth_rad();
                    for ear in 0..Ear::COUNT {
                        let e = Ear::from_index(ear);
                        let delay = ear_delay_sec(az, e, self.head_radius, self.speed_of_sound)
                            * self.sample_rate;
                        let vd = read_delayed(&self.obj_itd[row..], cursor, delay, rl);
                        let y = self.obj_shelf
                            [obj_idx * (MAX_DIRS * Ear::COUNT) + d * Ear::COUNT + ear]
                            .process(vd);
                        let g = y * w * obj_gain * self.out_trim[ear];
                        if g != 0.0 {
                            out[frame * 2 + ear] += g;
                        }
                    }
                }

                // LFE fold: the effects path reaches both ears at 1/√2.
                if obj.lfe_send > 0.0 {
                    let g = s * obj.lfe_send * obj_gain * LFE_FOLD;
                    out[frame * 2] += g * self.out_trim[0];
                    out[frame * 2 + 1] += g * self.out_trim[1];
                }

                // Room: store the frame in the object's ring, fire each
                // image's ear taps (room delay + ITD, fractional), and
                // accumulate the late-field send.
                if room_on {
                    let rcursor = self.room_er.cursor_at(frame);
                    self.room_er.store(obj_idx, rcursor, s);
                    for i in 0..n_img {
                        let az = ref_az[i];
                        let base_delay = imgs[i].delay as f32;
                        for ear in 0..Ear::COUNT {
                            let e = Ear::from_index(ear);
                            let delay = base_delay
                                + ear_delay_sec(az, e, self.head_radius, self.speed_of_sound)
                                    * self.sample_rate;
                            let vd = self.room_er.read_delayed(obj_idx, rcursor, delay);
                            let idx = obj_idx * (MAX_IMAGES * Ear::COUNT) + i * Ear::COUNT + ear;
                            let y = self.ref_shelf[idx].process(vd);
                            let g = self.ref_gain[idx] * self.out_trim[ear];
                            if g != 0.0 {
                                out[frame * 2 + ear] += y * g;
                            }
                        }
                    }
                    if obj.room_send > 0.0 {
                        self.room_er.add_send(frame, s * obj.gain * obj.room_send);
                    }
                }
            }
        }
        if room_on {
            self.room_er.end_block(frames);
        }
    }

    /// Render the scene's beds through the head model, routing each authored
    /// channel by its semantic role's canonical azimuth (spec §13.1 folded
    /// onto headphones). The LFE role folds to both ears at `1/√2`.
    fn render_beds(
        &mut self,
        scene: &SpatialScene,
        bed_inputs: &[&[f32]],
        frames: usize,
        out: &mut [f32],
    ) {
        let rl = self.itd_len;
        for (ordinal, (slot, bed)) in scene.beds.iter_enabled().enumerate() {
            let ch = bed.channels();
            let n_ch = ch.len();
            let base = ordinal * n_ch;
            for (c, &role) in ch.iter().enumerate() {
                let Some(input) = bed_inputs.get(base + c) else {
                    continue;
                };
                let g = bed.gain;
                if g == 0.0 || input.len() < frames {
                    continue;
                }
                if role == ChannelId::Lfe {
                    for frame in 0..frames {
                        let s = input[frame] * g * LFE_FOLD;
                        out[frame * 2] += s * self.out_trim[0];
                        out[frame * 2 + 1] += s * self.out_trim[1];
                    }
                    continue;
                }
                let az = role_azimuth(role);
                let brow = slot * (MAX_BED_CHANNELS * rl) + c * rl;
                for ear in 0..Ear::COUNT {
                    let e = Ear::from_index(ear);
                    let idx = slot * (MAX_BED_CHANNELS * Ear::COUNT) + c * Ear::COUNT + ear;
                    // Static azimuth: snap (beds are authored, static content).
                    self.bed_shelf[idx].set_target(head_shadow_alpha(az, e), 1.0);
                }
                for frame in 0..frames {
                    let s = input[frame] * g;
                    let cursor = (self.itd_pos + frame) % rl;
                    self.bed_itd[brow + cursor] = s;
                    for ear in 0..Ear::COUNT {
                        let e = Ear::from_index(ear);
                        let delay = ear_delay_sec(az, e, self.head_radius, self.speed_of_sound)
                            * self.sample_rate;
                        let vd = read_delayed(&self.bed_itd[brow..], cursor, delay, rl);
                        let y = self.bed_shelf
                            [slot * (MAX_BED_CHANNELS * Ear::COUNT) + c * Ear::COUNT + ear]
                            .process(vd);
                        out[frame * 2 + ear] += y * self.out_trim[ear];
                    }
                }
            }
        }
    }

    /// Binauralize the virtual-ring scratch (fields + room late field):
    /// each virtual speaker's (decorrelated) plane gets its own ITD and
    /// static head shadow per ear.
    fn binauralize_virtual(&mut self, frames: usize, out: &mut [f32]) {
        let len = self.itd_len;
        for f in 0..frames {
            let cursor = (self.itd_pos + f) % len;
            for (k, &deg) in VIRTUAL_AZIMUTHS_DEG.iter().enumerate() {
                let v = self.virtual_scratch[f * VIRTUAL_RING_SPEAKERS + k];
                self.vs_itd[k * len + cursor] = v;
                for ear in 0..Ear::COUNT {
                    let e = Ear::from_index(ear);
                    let az = deg.to_radians();
                    let delay = ear_delay_sec(az, e, self.head_radius, self.speed_of_sound)
                        * self.sample_rate;
                    let vd = read_delayed(&self.vs_itd[k * len..], cursor, delay, len);
                    let y = self.vs_shelf[k * Ear::COUNT + ear].process(vd);
                    out[f * 2 + ear] += y * self.out_trim[ear];
                }
            }
        }
    }
}

impl SpatialRenderer for BinauralRenderer {
    fn prepare(&mut self, layout: &SpeakerLayout, sample_rate: u32) -> Result<(), RenderError> {
        self.prepare_layout(layout, sample_rate)
    }

    fn process_block(
        &mut self,
        scene: &SpatialScene,
        object_inputs: &[&[f32]],
        frames: usize,
        out: &mut [f32],
    ) -> Result<(), RenderError> {
        let inputs = HybridBlockInputs {
            objects: object_inputs,
            beds: &[],
            fields: &[],
        };
        self.process_hybrid_block(scene, &inputs, frames, out)
    }

    fn process_hybrid_block(
        &mut self,
        scene: &SpatialScene,
        inputs: &HybridBlockInputs<'_>,
        frames: usize,
        out: &mut [f32],
    ) -> Result<(), RenderError> {
        if !self.prepared {
            return Err(RenderError::InvalidLayout);
        }
        if frames == 0 || frames > MAX_AUDIO_BLOCK_FRAMES {
            return Err(RenderError::BufferMismatch {
                expected: MAX_AUDIO_BLOCK_FRAMES,
                got: frames,
            });
        }
        let need = 2 * frames;
        if out.len() < need {
            return Err(RenderError::BufferMismatch {
                expected: need,
                got: out.len(),
            });
        }
        for sample in out[..need].iter_mut() {
            *sample = 0.0;
        }
        self.render_objects(scene, inputs.objects, frames, out);
        self.render_beds(scene, inputs.beds, frames, out);

        // Diffuse content (fields + room late field): decode onto the
        // virtual ring, then binauralize.
        let fields_active =
            !inputs.fields.is_empty() && scene.fields.iter_enabled().next().is_some();
        let late_active = scene.room.enabled && scene.room.late_mix > 0.0;
        if late_active || fields_active {
            self.virtual_scratch.fill(0.0);
            if late_active {
                let n = self.room_late.process(
                    &scene.room,
                    self.room_er.send(),
                    frames,
                    &mut self.late_scratch,
                );
                self.fields.render_extra(
                    &self.late_scratch[..n],
                    scene.room.late_mix,
                    frames,
                    &mut self.virtual_scratch,
                    &self.virtual_trim,
                );
            }
            if fields_active {
                self.fields.render(
                    scene,
                    inputs.fields,
                    frames,
                    &mut self.virtual_scratch,
                    &self.virtual_trim,
                );
            }
            self.binauralize_virtual(frames, out);
        }

        self.itd_pos = (self.itd_pos + frames) % self.itd_len;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::math::Vec3 as V;
    use crate::spatial::scene::SpatialScene;
    use crate::spatial::speaker::SpeakerLayout;
    use std::f32::consts::FRAC_PI_2;

    const EPS: f32 = 1e-3;

    fn scene_with_object(pos: V) -> (SpatialScene, usize) {
        let mut sc = SpatialScene::new(48_000);
        let id = sc.create_audio_object(pos).unwrap();
        (sc, id.0)
    }

    #[test]
    fn prepare_rejects_non_stereo_layouts() {
        let mut r = BinauralRenderer::new(0.0);
        assert!(matches!(
            r.prepare(&SpeakerLayout::five_point_one(), 48_000),
            Err(RenderError::InvalidLayout)
        ));
        assert!(matches!(
            r.prepare(&SpeakerLayout::stereo(), 48_000),
            Ok(())
        ));
    }

    #[test]
    fn front_center_is_balanced_and_unity() {
        // A front object reaches both ears at DC gain 1 (the shelf's DC gain
        // is unity for any α; ITD is zero). Level is not split between
        // "speakers" — each ear hears the source at full level, as with a
        // real head. The shelf's pole (|a1| ≈ 0.92) makes DC convergence
        // geometric, so the warm-up window must be long enough.
        let mut r = BinauralRenderer::new(0.0);
        r.prepare(&SpeakerLayout::stereo(), 48_000).unwrap();
        let (scene, _) = scene_with_object(Vec3::Y);
        let frames = 256usize;
        let input = vec![1.0f32; frames];
        let mut out = vec![0.0f32; 2 * frames];
        r.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        for f in 200..frames {
            assert!(
                (out[f * 2] - 1.0).abs() < EPS,
                "left DC unity at frame {f}: {}",
                out[f * 2]
            );
            assert!(
                (out[f * 2 + 1] - 1.0).abs() < EPS,
                "right DC unity at frame {f}: {}",
                out[f * 2 + 1]
            );
            assert!((out[f * 2] - out[f * 2 + 1]).abs() < 1e-6, "balanced front");
        }
    }

    #[test]
    fn mirror_symmetry_swaps_ears_exactly() {
        // +45° and −45° are mirror images: the renderer must swap L/R
        // bit-for-bit (same float ops, same filter states). Each scene gets
        // a *fresh* renderer — persistent filter state must not leak between
        // the two runs.
        let render = |dir: Vec3| -> Vec<f32> {
            let mut r = BinauralRenderer::new(0.0);
            r.prepare(&SpeakerLayout::stereo(), 48_000).unwrap();
            let frames = 256usize;
            let input = vec![1.0f32; frames];
            let (scene, _) = scene_with_object(dir);
            let mut out = vec![0.0f32; 2 * frames];
            r.process_block(&scene, &[&input], frames, &mut out)
                .unwrap();
            out
        };
        let a = render(Vec3::new(1.0, 1.0, 0.0).normalized().unwrap());
        let b = render(Vec3::new(-1.0, 1.0, 0.0).normalized().unwrap());
        let frames = a.len() / 2;
        for f in 0..frames {
            assert!(
                (a[f * 2] - b[f * 2 + 1]).abs() < 1e-6,
                "L(+45) vs R(−45) at {f}: {} vs {}",
                a[f * 2],
                b[f * 2 + 1]
            );
            assert!(
                (a[f * 2 + 1] - b[f * 2]).abs() < 1e-6,
                "R(+45) vs L(−45) at {f}: {} vs {}",
                a[f * 2 + 1],
                b[f * 2]
            );
        }
    }

    #[test]
    fn lfe_send_folds_into_both_ears() {
        // The LFE send is additive on top of the direct path: compare a run
        // with lfe_send 0 against lfe_send 1 and verify the *delta* is the
        // 1/√2 fold on both ears (equal, deterministic).
        let render = |lfe: f32| -> Vec<f32> {
            let mut r = BinauralRenderer::new(0.0);
            r.prepare(&SpeakerLayout::stereo(), 48_000).unwrap();
            let (mut scene, id) = scene_with_object(Vec3::Y);
            scene
                .object_mut(super::super::object::ObjectId(id))
                .unwrap()
                .lfe_send = lfe;
            let frames = 256usize;
            let input = vec![1.0f32; frames];
            let mut out = vec![0.0f32; 2 * frames];
            r.process_block(&scene, &[&input], frames, &mut out)
                .unwrap();
            out
        };
        let off = render(0.0);
        let on = render(1.0);
        let frames = on.len() / 2;
        for f in 200..frames {
            let dl = on[f * 2] - off[f * 2];
            let dr = on[f * 2 + 1] - off[f * 2 + 1];
            assert!((dl - LFE_FOLD).abs() < EPS, "LFE fold L delta at {f}: {dl}");
            assert!((dr - LFE_FOLD).abs() < EPS, "LFE fold R delta at {f}: {dr}");
            assert!((dl - dr).abs() < 1e-6, "equal fold");
        }
    }

    #[test]
    fn hard_right_source_delays_left_ear_and_shadows_it() {
        // A source at +90° (right): the right ear hears it immediately with
        // the ipsilateral boost (α→2), the left ear hears it ~31.5 samples
        // later through the shadow (α→0.1). Impulse test: argmax positions
        // differ by the Woodworth ITD.
        let mut r = BinauralRenderer::new(0.0);
        r.prepare(&SpeakerLayout::stereo(), 48_000).unwrap();
        let frames = 128usize;
        let mut input = vec![0.0f32; frames];
        input[0] = 1.0;
        let (scene, _) = scene_with_object(Vec3::X);
        let mut out = vec![0.0f32; 2 * frames];
        r.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        let argmax = |ear: usize| -> (usize, f32) {
            let mut best = (0usize, 0.0f32);
            for f in 0..frames {
                let v = out[f * 2 + ear].abs();
                if v > best.1 {
                    best = (f, v);
                }
            }
            best
        };
        let (r_peak, r_amp) = argmax(1);
        let (l_peak, l_amp) = argmax(0);
        let expect = r.itd_samples(FRAC_PI_2, Ear::Left);
        assert!(
            (l_peak as f32 - r_peak as f32 - expect).abs() < 2.0,
            "left ear lagged by ITD: L@{} R@{} expect {}",
            l_peak,
            r_peak,
            expect
        );
        // HF shadow: the ipsilateral ear is *louder* than the contralateral
        // one at the impulse (the α=2 boost vs α=0.1 shadow).
        assert!(
            r_amp > 1.5 * l_amp,
            "ipsilateral boost vs shadow: R {r_amp} vs L {l_amp}"
        );
    }

    #[test]
    fn spread_reduces_effective_interaural_delay() {
        // Spread=0 for a +45° source: the ears' impulse responses sit at
        // their own ITDs, so the temporal-energy centroids differ by the
        // full Woodworth ITD. Spread=1 blends three ring directions (each
        // with its own ITD) into both ears, so the centroid *difference*
        // shrinks — the image widens and its lateral pull weakens. The
        // centroid includes the shelf tail (decay |a1|ⁿ, centroid ≈ 5.5
        // samples), so the metric is the *difference* between ears, which
        // cancels the common tail.
        let run = |spread: f32| -> Vec<f32> {
            let mut r = BinauralRenderer::new(0.0);
            r.prepare(&SpeakerLayout::stereo(), 48_000).unwrap();
            let (mut scene, id) = scene_with_object(Vec3::new(1.0, 1.0, 0.0).normalized().unwrap());
            scene
                .object_mut(super::super::object::ObjectId(id))
                .unwrap()
                .spread = spread;
            let frames = 512usize;
            let mut input = vec![0.0f32; frames];
            input[0] = 1.0;
            let mut out = vec![0.0f32; 2 * frames];
            r.process_block(&scene, &[&input], frames, &mut out)
                .unwrap();
            out
        };
        let centroid = |ear: usize, out: &[f32], frames: usize| -> f32 {
            let mut num = 0.0f32;
            let mut den = 0.0f32;
            for f in 0..frames {
                let v = out[f * 2 + ear];
                num += f as f32 * v * v;
                den += v * v;
            }
            num / den.max(1e-9)
        };
        let out0 = run(0.0);
        let out1 = run(1.0);
        let frames = 512usize;
        let diff0 = (centroid(0, &out0, frames) - centroid(1, &out0, frames)).abs();
        let diff1 = (centroid(0, &out1, frames) - centroid(1, &out1, frames)).abs();
        assert!(
            diff0 > diff1 + 3.0,
            "spread shrinks effective ITD: {diff0} → {diff1}"
        );
        assert!(out0.iter().all(|v| v.is_finite()));
        assert!(out1.iter().all(|v| v.is_finite()));
    }
}
