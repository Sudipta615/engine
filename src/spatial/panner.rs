//! Equal-power basic panner (spec Part V §24–30, §46, §56–57).
//!
//! This is the first serious object-to-speaker renderer: an object is
//! transformed into listener space, its azimuth bracketed by the two
//! neighbouring pan-capable speakers, and the pair solved with an
//! equal-power law so perceived level stays stable across the panorama.
//!
//! Design points that keep it deterministic and realtime-safe:
//!
//! - **Geometry is preprocessed in `prepare`** (speaker azimuth ring, per-
//!   speaker direction/calibration). Never rebuilt in the callback (spec
//!   §74). The renderer writes into a caller-supplied interleaved buffer, so
//!   the steady-state hot path allocates nothing.
//! - **Coefficient smoothing** (spec §46): each object→speaker path has a
//!   persisted, smoothed gain. An object crossing a speaker-region boundary
//!   ramps those gains across a few blocks instead of clicking, while each
//!   object keeps its own separation (a left object never leaks to the right
//!   speaker at the same level as a coincident right object).
//! - **LFE is additive, never a pan target** (spec §56–57): an object's
//!   `lfe_send` feeds the layout's LFE slot through its own smoothed gain;
//!   panning never writes to LFE and LFE never folds back into the mains.
//! - **Spread** (spec §30, simplified): non-zero spread blends energy onto
//!   the flanking speakers, widening the angular extent rather than merely
//!   reducing localization.
//! - **Elevation**: handled as broad `cos(elevation)` attenuation — a source
//!   above/below the ring stays localized by azimuth but is slightly
//!   quieter, matching being off the horizontal plane. The equal-power
//!   invariant (§94) holds exactly on the horizontal plane at spread 0.
//!
//! ## Realtime contract
//!
//! `process_block` performs no allocation and takes no locks after `prepare`
//! (verified by `tests/fidelity/realtime_allocation.rs`). Bounded work per
//! callback: at most `MAX_SPATIAL_OBJECTS` objects × 4 speaker paths × block.

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::decode::ChannelId;
use crate::spatial::object::MAX_SPATIAL_OBJECTS;

use super::bed::render_beds;
use super::directivity::listener_angle_rad;
use super::field::AmbisonicFieldMixer;
use super::level::AirAbsorption;
use super::math::Vec3;
use super::occlusion::OcclusionState;
use super::render::{HybridBlockInputs, RenderError, SpatialRenderer};
use super::room::{EarlyReflections, ListenerImage, RoomLateField, MAX_IMAGES};
use super::scene::{ListenerTransform, SpatialScene};
use super::speaker::SpeakerLayout;
use super::spread::{add_gain, normalize_gains, ring_directions, MAX_SPREAD_GAINS, RING_SAMPLES};

/// Default smoothing time constant (ms) for coefficient changes.
pub const DEFAULT_SMOOTHING_MS: f32 = 24.0;

/// A precomputed pan speaker: output index, azimuth about the ring, and its
/// combined per-channel level (Speaker::gain × calibration trim).
#[derive(Debug, Clone, Copy)]
struct PanSpeaker {
    /// Output channel (speaker) index.
    idx: usize,
    /// Azimuth in radians.
    azimuth: f32,
    /// Linear level multiplier (speaker geometry × calibration).
    level: f32,
}

/// The equal-power basic panner.
#[derive(Debug)]
pub struct BasicPanner {
    /// Pan-capable speakers (enabled, non-LFE) sorted by azimuth.
    pan: Vec<PanSpeaker>,
    /// Total output speakers (incl. LFE) = output channel count.
    speaker_count: usize,
    /// LFE output index, if present.
    lfe_index: Option<usize>,
    /// Per-output-channel combined level (incl. calibration trim).
    out_trim: Vec<f32>,
    /// Persisted per-(object, speaker) smoothed coefficients. Flat
    /// `MAX_OBJECTS × speaker_count`, indexed `obj*count + spk`.
    sm: Vec<f32>,
    /// One-pole factor applied each block toward each path's target. `1.0`
    /// disables smoothing (= exact target), `0.0` holds (not useful).
    smooth: f32,
    /// Low-cost air-absorption seam (disabled by default ⇒ exact ×1.0).
    air_absorption: AirAbsorption,
    /// Per-object LFE smoothed gain (bounded by MAX_OBJECTS).
    sm_lfe: Vec<f32>,
    /// Per-object occlusion low-pass state (bounded by MAX_OBJECTS).
    occ: Vec<OcclusionState>,
    /// Output-speaker semantic roles (index → ChannelId) for bed routing.
    bed_roles: Vec<(usize, ChannelId)>,
    /// Diffuse-field mixer (per-speaker decorrelation delay rings).
    fields: AmbisonicFieldMixer,
    /// Room early reflections (per-object delay rings + tap smoothing).
    room_er: EarlyReflections,
    /// Room late field (Schroeder tail feeding the ambisonic bus).
    room_late: RoomLateField,
    /// Block-sized scratch for the late-field plane.
    late_scratch: Vec<f32>,

    sample_rate: f32,
    prepared: bool,
}

impl BasicPanner {
    /// Create a panner. `smooth_ms <= 0.0` disables smoothing (= exact
    /// target gains), useful for precise mathematical tests.
    pub fn new(smooth_ms: f32) -> Self {
        Self {
            pan: Vec::new(),
            speaker_count: 0,
            lfe_index: None,
            out_trim: Vec::new(),
            sm: vec![0.0; MAX_SPATIAL_OBJECTS * 16],
            smooth: coefficient_for_ms(smooth_ms),
            air_absorption: AirAbsorption::default(),
            sm_lfe: vec![0.0; MAX_SPATIAL_OBJECTS],
            occ: vec![OcclusionState::default(); MAX_SPATIAL_OBJECTS],
            bed_roles: Vec::new(),
            fields: AmbisonicFieldMixer::new(),
            room_er: EarlyReflections::new(),
            room_late: RoomLateField::new(),
            late_scratch: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            sample_rate: 44_100.0,
            prepared: false,
        }
    }

    /// Configure the optional air-absorption model (disabled by default =
    /// exact ×1.0). The model's cutoff is computed here and exposed for a
    /// future filter stage; this phase applies the broad distance gain only,
    /// keeping the HF roll-off as a documented seam.
    pub fn set_air_absorption(&mut self, a: AirAbsorption) -> &mut Self {
        self.air_absorption = a;
        self
    }

    /// Public accessor (used by tests): the number of output speakers.
    pub fn channels(&self) -> usize {
        self.speaker_count
    }

    /// Return the current (smoothed) total gain for an object/speaker path.
    /// Indexed by the object's **stable store slot** [`ObjectId`]
    /// (`crate::spatial::object::ObjectId`) and `spk` (0..channels). Exposed
    /// for symmetry/discontinuity tests. Out-of-range returns 0.
    pub fn coefficient(&self, obj_slot: usize, spk: usize) -> f32 {
        if obj_slot >= MAX_SPATIAL_OBJECTS || spk >= self.speaker_count {
            return 0.0;
        }
        self.sm[obj_slot * self.speaker_count + spk]
    }

    fn prepare_layout(
        &mut self,
        layout: &SpeakerLayout,
        sample_rate: u32,
    ) -> Result<(), RenderError> {
        layout.validate()?;
        let mut pan: Vec<PanSpeaker> = Vec::with_capacity(layout.speakers.len());
        let mut lfe_index = None;
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe {
                lfe_index = Some(idx);
                continue;
            }
            if !s.enabled {
                continue;
            }
            let az = s.position.azimuth_rad();
            let level = s.gain * layout.calibration.trim_gain(s.id);
            pan.push(PanSpeaker {
                idx,
                azimuth: az,
                level,
            });
        }
        pan.sort_by(|a, b| {
            a.azimuth
                .partial_cmp(&b.azimuth)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let speaker_count = layout.speakers.len();
        let mut out_trim = Vec::with_capacity(speaker_count);
        for s in &layout.speakers {
            out_trim.push(s.gain * layout.calibration.trim_gain(s.id));
        }

        self.pan = pan;
        self.speaker_count = speaker_count;
        self.lfe_index = lfe_index;
        self.out_trim = out_trim;
        self.sm = vec![0.0; MAX_SPATIAL_OBJECTS * speaker_count.max(1)];
        self.sm_lfe = vec![0.0; MAX_SPATIAL_OBJECTS];
        self.occ = vec![OcclusionState::default(); MAX_SPATIAL_OBJECTS];
        self.bed_roles = layout
            .speakers
            .iter()
            .enumerate()
            .filter_map(|(idx, s)| s.role.map(|r| (idx, r)))
            .collect();
        self.fields.prepare(layout, sample_rate)?;
        self.room_er
            .prepare(speaker_count, sample_rate, self.smooth);
        self.room_late.prepare(sample_rate);
        self.sample_rate = sample_rate as f32;
        self.prepared = true;
        Ok(())
    }

    /// Solve the equal-power/pan coefficients for an object azimuth into
    /// a caller-provided scratch list of `(speaker_idx, coefficient)` pairs.
    /// The `level` of each pan speaker is folded in, so a coefficient already
    /// includes the per-speaker geometry×calibration multiplier. Callers pass
    /// up to 4 pairs. Core solve only — spread is handled by
    /// [`Self::solve_spread`].
    fn solve_pan(&self, azimuth: f32, out_pairs: &mut [(usize, f32)]) {
        let n = self.pan.len();
        for p in out_pairs.iter_mut() {
            *p = (0, 0.0);
        }
        if n == 0 {
            return;
        }
        if n == 1 {
            let sp = self.pan[0];
            out_pairs[0] = (sp.idx, sp.level);
            return;
        }
        // Find the cyclic arc `[i, i+1]` that brackets `azimuth`.
        let mut best = 0usize;
        let mut best_width = f32::INFINITY;
        for i in 0..n {
            let a0 = self.pan[i].azimuth;
            let a1 = self.pan[(i + 1) % n].azimuth;
            let width = angle_delta(a0, a1);
            if width < best_width {
                // `fraction_between` tells us if the azimuth is on [0,width].
                let (_, inside) = fraction_between(a0, a1, azimuth, width);
                if inside {
                    best = i;
                    best_width = width;
                }
            }
        }
        let sa = self.pan[best];
        let sb = self.pan[(best + 1) % n];
        let width = angle_delta(sa.azimuth, sb.azimuth);
        let (t, _) = fraction_between(sa.azimuth, sb.azimuth, azimuth, width);
        // Equal-power law (spec §24): la = cos(t·π/2), lb = sin(t·π/2).
        let la = (t * std::f32::consts::FRAC_PI_2).cos();
        let lb = (t * std::f32::consts::FRAC_PI_2).sin();
        out_pairs[0] = (sa.idx, la * sa.level);
        out_pairs[1] = (sb.idx, lb * sb.level);
    }

    /// Angular-region spread solve (spec §30): solve the exact direction
    /// plus a fixed ring of samples around it at `spread × 60°`, aggregate
    /// by speaker, and energy-normalise. `dir` is the listener-space 3D
    /// direction (needed for the ring geometry; the panner consumes only
    /// azimuths). Writes into `out` (sized [`MAX_SPREAD_GAINS`]).
    fn solve_spread(&self, dir: Vec3, spread: f32, out: &mut [(usize, f32)]) {
        let s = spread.clamp(0.0, 1.0);
        let base_w = 1.0 - s;
        let mut scratch = [(0usize, 0.0f32); 4];
        let mut len = 0usize;
        self.solve_pan(dir.azimuth_rad(), &mut scratch);
        for &(spk, v) in scratch.iter() {
            len = add_gain(out, len, spk, v * base_w);
        }
        let mut ring = [Vec3::ZERO; RING_SAMPLES];
        let n_ring = ring_directions(dir, s * super::spread::SPREAD_MAX_HALF_ANGLE_RAD, &mut ring);
        if n_ring > 0 {
            let ring_w = s / n_ring as f32;
            for rd in ring.iter().take(n_ring) {
                self.solve_pan(rd.azimuth_rad(), &mut scratch);
                for &(spk, v) in scratch.iter() {
                    len = add_gain(out, len, spk, v * ring_w);
                }
            }
        }
        normalize_gains(&mut out[..len]);
    }

    /// Render the current scene's objects into `out`.
    ///
    /// Allocation-free after `prepare`: no `Vec` growth here — per-object
    /// state is keyed by the stable store slot and reuses preallocated
    /// buffers (realtime discipline, spec §71–75).
    fn render_objects(
        &mut self,
        scene: &SpatialScene,
        inputs: &[&[f32]],
        frames: usize,
        out: &mut [f32],
    ) {
        let n_spk = self.speaker_count;
        for sample in out[..n_spk * frames].iter_mut() {
            *sample = 0.0;
        }
        let xf = ListenerTransform::from_listener(&scene.listener);

        let room = &scene.room;
        let room_on = room.enabled && room.reflection_order >= 1;
        if room_on {
            self.room_er.begin_block(frames);
        }
        let mut pairs = [(0usize, 0.0f32); MAX_SPREAD_GAINS];
        let mut rpairs = [(0usize, 0.0f32); MAX_SPREAD_GAINS];
        let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
        // Iterate enabled objects in store order, zipping each with the
        // matching input plane by ordinal (`iter_enabled` yields `(slot, obj)`
        // and never allocates).
        for (obj_ordinal, (slot, obj)) in scene.objects.iter_enabled().enumerate() {
            let obj_idx = slot; // stable id = store slot
            let local = xf.apply_to_point(obj.position);
            let dist = local.length();
            let dir_v = local.normalized().unwrap_or(Vec3::Y);
            let azimuth = dir_v.azimuth_rad();
            let elevation = dir_v.elevation_rad();

            // Level chain (spec §68): source gain · distance · cos(elevation)
            // off-plane term · directivity · occlusion transmission · pan
            // coefficients.
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
            let obj_gain =
                obj.gain * dist_gain * elevation.cos().clamp(0.0, 1.0) * dir_gain * occ_gain;

            if obj.spread > 0.0 {
                self.solve_spread(dir_v, obj.spread, &mut pairs);
            } else {
                self.solve_pan(azimuth, &mut pairs);
            }

            // Smooth each object→speaker path (spec §46) and store into the
            // flat matrix keyed by obj_idx. The stored value is the **total**
            // per-path gain = pan coefficient × object level chain, so distance
            // and elevation attenuation reach the output (spec §68).
            let row = obj_idx * n_spk;
            for &(spk, hard) in pairs.iter() {
                if hard == 0.0 {
                    continue;
                }
                let total = hard * obj_gain;
                let prev = self.sm[row + spk];
                let next = if self.smooth >= 1.0 {
                    total
                } else {
                    prev + self.smooth * (total - prev)
                };
                self.sm[row + spk] = next;
            }

            // LFE path (spec §56): smoothed object→LFE send.
            if let Some(lfe) = self.lfe_index {
                let lfe_target = obj.lfe_send * obj_gain;
                let prev_lfe = self.sm_lfe[obj_idx];
                let next_lfe = if self.smooth >= 1.0 {
                    lfe_target
                } else {
                    prev_lfe + self.smooth * (lfe_target - prev_lfe)
                };
                self.sm_lfe[obj_idx] = next_lfe;
                self.sm[row + lfe] = next_lfe;
            }

            // Room path (spec §49): early reflections as virtual sources +
            // room-send accumulation for the late field. The tap list is
            // cleared for every object when the room is active (even at
            // room_send 0) so stale taps from a previous block never fire.
            if room_on {
                self.room_er.begin_object(obj_idx);
            }
            if room_on && obj.room_send > 0.0 {
                let n_img = self.room_er.images_for_object(
                    room,
                    scene.listener.position,
                    obj.position,
                    &mut imgs,
                );
                for (img_i, img) in imgs.iter().take(n_img).enumerate() {
                    let ldir = xf.apply_to_direction(img.dir);
                    let dg = obj
                        .distance_model
                        .distance_gain(img.dist, obj.reference_distance);
                    self.solve_pan(ldir.azimuth_rad(), &mut rpairs);
                    for &(spk, g) in rpairs.iter() {
                        if g == 0.0 {
                            continue;
                        }
                        let target = obj.gain * obj.room_send * img.coeff * dg * g;
                        if target != 0.0 {
                            self.room_er.add_tap(obj_idx, img_i, spk, img.delay, target);
                        }
                    }
                }
            }

            // Bake into the interleaved output, applying trim per channel.
            // `inputs` is indexed by the enabled-object ordinal; smoothing
            // state is keyed by the object's stable slot (`obj_idx`).
            let input = inputs.get(obj_ordinal).copied().unwrap_or(&[]);
            if input.len() < frames {
                continue;
            }
            for frame in 0..frames {
                let mut s = input[frame];
                // Occlusion low-passes the object input *before* panning
                // (spec §43); the filtered sample feeds both the pan paths
                // and the LFE send. Passthrough when not occluded.
                if let Some(c) = occ_coeffs {
                    s = self.occ[obj_idx].process(s, &c);
                }
                for &(spk, coeff) in pairs.iter() {
                    // Skip zero-coefficient sentinel slots (only a few of the
                    // entries are real); re-read the smoothed gain.
                    if coeff == 0.0 {
                        continue;
                    }
                    let gain = self.sm[row + spk] * self.out_trim[spk];
                    if gain != 0.0 {
                        out[frame * n_spk + spk] += s * gain;
                    }
                }
                if let Some(lfe) = self.lfe_index {
                    let g = self.sm_lfe[obj_idx] * self.out_trim[lfe];
                    if g != 0.0 {
                        out[frame * n_spk + lfe] += s * g;
                    }
                }
                // Room: store this frame in the object's reflection ring,
                // fire the delayed taps, and accumulate the late-field send.
                if room_on {
                    self.room_er.object_frame(
                        obj_idx,
                        s,
                        obj.gain * obj.room_send,
                        frame,
                        n_spk,
                        out,
                        &self.out_trim,
                    );
                }
            }
        }
        if room_on {
            self.room_er.end_block(frames);
        }
    }
}

impl SpatialRenderer for BasicPanner {
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
        let need = self.speaker_count * frames;
        if out.len() < need {
            return Err(RenderError::BufferMismatch {
                expected: need,
                got: out.len(),
            });
        }
        // Hybrid spatial mixer (spec §37): objects → beds → fields all sum
        // into the same interleaved output.
        self.render_objects(scene, inputs.objects, frames, out);
        render_beds(
            scene,
            inputs.beds,
            frames,
            out,
            &self.bed_roles,
            &self.out_trim,
        );
        // Room late field (spec §55): the Schroeder tail encodes into the
        // ambisonic bus and decodes as a diffuse source. Skipped when the
        // room is off or the wet mix is zero (bit-exact).
        if scene.room.enabled && scene.room.late_mix > 0.0 {
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
                out,
                &self.out_trim,
            );
        }
        self.fields
            .render(scene, inputs.fields, frames, out, &self.out_trim);
        Ok(())
    }
}

/// Turn a smoothing duration (ms) into a per-block one-pole factor. Kept as
/// a simple, deterministic ramp per max-size block; `<= 0` returns 1 (exact).
fn coefficient_for_ms(ms: f32) -> f32 {
    if ms <= 0.0 {
        return 1.0;
    }
    (1.0 / (1.0 + ms / 8.0)).clamp(0.0, 1.0)
}

/// Signed angle from `a` to `b` counter-clockwise (radians), in `[0, 2π)`.
fn angle_delta(a: f32, b: f32) -> f32 {
    (b - a).rem_euclid(std::f32::consts::TAU)
}

/// Fraction `t ∈ [0,1]` along arc `[a,b]` of width `width` where `x` sits;
/// `inside` is true when `x` is on that arc (starting at `a`).
fn fraction_between(a: f32, _b: f32, x: f32, width: f32) -> (f32, bool) {
    let d = angle_delta(a, x);
    if d <= width + 1e-5 {
        (if width > 1e-9 { d / width } else { 0.0 }, true)
    } else {
        (0.0, false)
    }
}

#[cfg(test)]
mod tests {
    use super::super::math::Vec3;
    use super::super::{object, speaker};
    use super::*;

    const EPS: f32 = 1e-4;

    fn stereo_layout() -> SpeakerLayout {
        SpeakerLayout::stereo()
    }

    /// Build a one-object scene at `pos` (world space), gain 1, point source.
    fn scene_with_object(pos: Vec3) -> (SpatialScene, usize) {
        let mut sc = SpatialScene::new(48_000);
        let id = sc.create_audio_object(pos).unwrap();
        (sc, id.0)
    }

    #[test]
    fn front_object_is_evenly_split_floor_pair() {
        let layout = stereo_layout();
        let mut p = BasicPanner::new(0.0); // exact, no smoothing
        p.prepare(&layout, 48_000).unwrap();
        let (scene, _) = scene_with_object(Vec3::Y); // front
        let frames = 128usize;
        let mut out = vec![0.0f32; 2 * frames];
        // Mono input gain 1.0 at front.
        let input = vec![1.0f32; frames];
        p.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        let fl = out[0];
        let fr = out[128];
        // Equal-power halves.
        assert!(
            (fl - std::f32::consts::FRAC_1_SQRT_2).abs() < EPS,
            "fl={fl} fr={fr}"
        );
        assert!(
            (fr - std::f32::consts::FRAC_1_SQRT_2).abs() < EPS,
            "fl={fl} fr={fr}"
        );
        // Energy invariant across the pair.
        let e = fl * fl + fr * fr;
        assert!((e - 1.0).abs() < 1e-4);
        // Both channels equal (discrete input) each frame.
        for i in 0..frames {
            assert!((out[i] - fl).abs() < 1e-5);
            assert!((out[i + frames] - fr).abs() < 1e-5);
        }
    }

    #[test]
    fn hard_left_is_strongly_left() {
        let layout = stereo_layout();
        let mut p = BasicPanner::new(0.0);
        p.prepare(&layout, 48_000).unwrap();
        let (scene, _) = scene_with_object(-Vec3::X); // listener's left
        let frames = 8usize;
        let mut out = vec![0.0f32; 2 * frames];
        let input = vec![1.0f32; frames];
        p.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        // Interleaved `2×frames`: frame 0 is [L, R] at out[0], out[1].
        let left = out[0];
        let right = out[1];
        assert!(
            left > 0.9,
            "front-left-dominant on left speaker (left={left} right={right})"
        );
        assert!(right < left, "left source must favour left speaker");
    }

    #[test]
    fn energy_invariant_holds_across_panorama() {
        let layout = SpeakerLayout::five_point_one();
        let mut p = BasicPanner::new(0.0);
        p.prepare(&layout, 48_000).unwrap();
        let frames = 16usize;
        let input = vec![1.0f32; frames];
        let mut out = vec![0.0f32; 6 * frames];
        for deg in 0..=360 {
            let rad = (deg as f32).to_radians();
            // azimuth deg around ring in 5.1.
            let dir = Vec3::new(rad.sin(), rad.cos(), 0.0);
            let (scene, _) = scene_with_object(dir);
            p.process_block(&scene, &[&input], frames, &mut out)
                .unwrap();
            let mut energy = 0.0f32;
            for (spk, v) in out.iter().take(6).enumerate() {
                // In five_point_one() order (FL FR C LFE SL SR) the LFE slot
                // is index 3 — it is the only non-pan speaker.
                if spk == 3 {
                    continue; // LFE is not a pan speaker
                }
                energy += v * v;
            }
            assert!(
                (energy - 1.0).abs() < 2e-3,
                "energy drift at {deg}°: {energy}"
            );
        }
    }

    #[test]
    fn lfe_is_never_a_pan_target() {
        let layout = SpeakerLayout::five_point_one();
        let mut p = BasicPanner::new(0.0);
        p.prepare(&layout, 48_000).unwrap();
        let frames = 8usize;
        let input = vec![1.0f32; frames];
        // An object directly at the LFE's "position" (which is ZERO) — with
        // no lfe_send, nothing may reach the LFE channel.
        let (scene, _) = scene_with_object(Vec3::ZERO);
        let mut out = vec![0.0f32; 6 * frames];
        p.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        for i in 0..frames {
            // In five_point_one() order (FL FR C LFE SL SR) LFE is index 3;
            // never written without lfe_send.
            assert_eq!(
                out[i * 6 + 3],
                0.0,
                "LFE must receive nothing without lfe_send"
            );
        }
    }

    #[test]
    fn lfe_send_routes_into_lfe_channel_only() {
        let layout = SpeakerLayout::five_point_one();
        let mut p = BasicPanner::new(0.0);
        p.prepare(&layout, 48_000).unwrap();
        let (mut scene, id) = scene_with_object(Vec3::Y);
        scene.object_mut(object::ObjectId(id)).unwrap().lfe_send = 1.0;
        scene.object_mut(object::ObjectId(id)).unwrap().gain = 1.0;
        let frames = 8usize;
        let input = vec![1.0f32; frames];
        let mut out = vec![0.0f32; 6 * frames];
        p.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        for i in 0..frames {
            // LFE is index 3 in the five_point_one() ordering.
            assert!(out[i * 6 + 3] > 0.5, "lfe_send=1 reaches LFE channel");
        }
    }

    #[test]
    fn smoothing_converges_to_target_without_click() {
        let layout = stereo_layout();
        let mut p = BasicPanner::new(DEFAULT_SMOOTHING_MS);
        p.prepare(&layout, 48_000).unwrap();
        let frames = 64usize;
        let input = vec![1.0f32; frames];
        let mut out = vec![0.0f32; 2 * frames];
        let mut scene = SpatialScene::new(48_000);
        // Object hard left initially.
        let id = scene.create_audio_object(-Vec3::X).unwrap();
        let mut prev_l = 0.0f32;
        let mut jumped = false;
        for _ in 0..40 {
            p.process_block(&scene, &[&input], frames, &mut out)
                .unwrap();
            let l = out[0];
            if prev_l > 0.0 {
                let delta = (l - prev_l).abs();
                assert!(delta < 0.2, "smoothing must ramp, not jump ({delta})");
                if delta > 0.05 {
                    jumped = true;
                }
            }
            prev_l = l;
        }
        // Object now hard right.
        scene.object_mut(id).unwrap().position = Vec3::X;
        prev_l = out[0];
        for _ in 0..40 {
            p.process_block(&scene, &[&input], frames, &mut out)
                .unwrap();
            let l = out[0];
            let delta = (l - prev_l).abs();
            assert!(delta < 0.2, "region transition must be smoothed ({delta})");
            prev_l = l;
        }
        assert!(jumped, "the smoothing should have demonstrably ramped once");
        // Outputs are finite throughout.
        assert!(out.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn elevated_source_is_quieter_but_still_localizes() {
        let layout = stereo_layout();
        let mut p = BasicPanner::new(0.0);
        p.prepare(&layout, 48_000).unwrap();
        let frames = 8usize;
        let input = vec![1.0f32; frames];
        let (scene_up, _) = scene_with_object(Vec3::new(0.0, 1.0, 1.0)); // front, slightly up
        let mut out_up = vec![0.0f32; 2 * frames];
        p.process_block(&scene_up, &[&input], frames, &mut out_up)
            .unwrap();
        let up_pair = out_up[0] * out_up[0] + out_up[1] * out_up[1];
        let (scene_flat, _) = scene_with_object(Vec3::Y);
        let mut out_flat = vec![0.0f32; 2 * frames];
        p.process_block(&scene_flat, &[&input], frames, &mut out_flat)
            .unwrap();
        let flat_pair = out_flat[0] * out_flat[0] + out_flat[1] * out_flat[1];
        assert!(
            up_pair < flat_pair,
            "elevated source is quieter at the ring (up={up_pair} flat={flat_pair})"
        );
        assert!(out_up.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn zero_spread_is_point_like_and_disabled_speakers_get_zero() {
        let layout = SpeakerLayout::seven_point_one();
        let mut cal = layout.calibration.clone();
        cal.per_speaker_trim_db = vec![(speaker::SpeakerId(7), -600.0)]; // RL -inf
        let layout = SpeakerLayout {
            calibration: cal,
            ..layout
        };
        let mut p = BasicPanner::new(0.0);
        p.prepare(&layout, 48_000).unwrap();
        let frames = 8usize;
        let input = vec![1.0f32; frames];
        let (scene, _) = scene_with_object(Vec3::Y);
        let mut out = vec![0.0f32; 8 * frames];
        p.process_block(&scene, &[&input], frames, &mut out)
            .unwrap();
        for i in 0..frames {
            let mut total = 0.0f32;
            for spk in 0..8 {
                if spk == 3 {
                    continue; // LFE
                }
                let v = out[i * 8 + spk];
                total += v * v;
            }
            // Front pair carries the horizontal energy.
            assert!((total - 1.0).abs() < 1e-3);
            // RL (index 7) is heavily trimmed; REAR gets ~0.
            assert!(out[i * 8 + 7].abs() < 1e-4);
        }
    }
}
