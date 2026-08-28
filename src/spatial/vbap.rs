//! Vector-Based Amplitude Panning — VBAP-style object renderer (spec Part V
//! §25–29, Part IV §21–22).
//!
//! VBAP is the first serious object-to-speaker renderer: an object's
//! listener-space direction is expressed as a non-negative linear combination
//! of the vivid speakers that surround it. For a 3D layout those speakers
//! form a **triplet**; the solved gains are then energy-normalised so the
//! perceived level stays constant as the object moves.
//!
//! The key rule (spec §25) is: *compute from actual speaker positions, not a
//! hard-coded 5.1/7.1 table.* Every layout — named preset or arbitrary custom
//! array — is reduced to geometry at `prepare` time.
//!
//! ## Degenerate geometry and reduced-dimension fallbacks (spec §27)
//!
//! Pure 3-triplet VBAP needs ≥3 non-coplanar, non-collinear speakers. The
//! renderer therefore classifies the layout at `prepare`:
//!
//! - **`ThreeDim`** — the pan speakers are not coplanar (at least one valid
//!   triplet exists normally oriented). Objects are solved against the
//!   enclosing triplet (§27).
//! - **`Planar`** — every pan speaker lies in one plane (e.g. a horizontal
//!   ring or a stereo pair). 3D coverage is impossible, so the renderer falls
//!   back to a 2D equal-power azimuth pair (§26, §27 "coplanar layouts where
//!   3D coverage is impossible").
//! - **`Single`** — a single pan speaker; all energy goes to it.
//!
//! **Out-of-coverage** (spec §28): when no triplet encloses the direction
//! (e.g. below the floor or above the rig with no overhead speakers), the
//! renderer applies a deterministic **direction-preserving fallback** — the
//! full energy goes to the nearest speaker by dot product. NaN/Inf can never
//! be emitted: every degenerate triplet is skipped and every solve is
//! guarded.
//!
//! ## Realtime discipline (spec §71–75)
//!
//! All triangulation/inverse work happens in `prepare` (control thread,
//! heap-happy). `process_block` is allocation-free and lock-free: per-object
//! solved coefficients feed the same persisted, per-(object,speaker) one-pole
//! smoothing and caller-supplied output buffer as the `BasicPanner`, so
//! region crossings (a moving object hopping triplets) ramp rather than click
//! (§46).

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::spatial::object::MAX_SPATIAL_OBJECTS;

use super::level::AirAbsorption;
use super::math::Vec3;
use super::panner::DEFAULT_SMOOTHING_MS;
use super::render::{RenderError, SpatialRenderer};
use super::scene::{ListenerTransform, SpatialScene};
use super::speaker::SpeakerLayout;

/// Determinant threshold below which a speaker triplet is degenerate
/// (near-coplanar / near-collinear). Controls the geometry acceptance test.
pub const VBAP_DET_EPSILON: f32 = 1e-6;

/// A precomputed speaker direction plus its calibration level.
#[derive(Debug, Clone, Copy)]
struct PanSpeaker {
    /// Output channel index.
    idx: usize,
    /// Unit direction (listener-space).
    dir: Vec3,
    /// Linear level (speaker geometry × calibration trim).
    level: f32,
}

/// Reduced-dimension classification of the pan-speaker geometry, exposed
/// for hosts and tests to introspect how a layout was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanMode {
    /// Speakers span 3D → use 3-triplet VBAP.
    ThreeDim,
    /// Speakers are coplanar → 2D equal-power azimuth pairs.
    Planar,
    /// A single pan speaker.
    Single,
}

/// One precomputed, normally-oriented speaker triplet.
#[derive(Debug, Clone, Copy)]
struct Triplet {
    /// Output indices of the three speakers.
    idx: [usize; 3],
    /// Row-major 3×3 inverse of the loudspeaker matrix `L` (columns = the
    /// three speaker directions): `g_rows = inv[0]·d, inv[1]·d, inv[2]·d`.
    inv: [[f32; 3]; 3],
}

/// The VBAP-style object renderer.
#[derive(Debug)]
pub struct VbapRenderer {
    pan: Vec<PanSpeaker>,
    mode: PanMode,
    triplets: Vec<Triplet>,
    /// Precomputed valid 2D azimuth pairs (Planar mode): consecutive speakers
    /// around the horizontal ring.
    pairs: Vec<(usize, usize, f32)>, // (left_idx, right_idx, arc_width)
    speaker_count: usize,
    lfe_index: Option<usize>,
    out_trim: Vec<f32>,
    /// Per-(object,speaker) smoothed total gains, flat `MAX_OBJECTS × count`.
    sm: Vec<f32>,
    sm_lfe: Vec<f32>,
    smooth: f32,
    air_absorption: AirAbsorption,
    sample_rate: f32,
    prepared: bool,
}

impl VbapRenderer {
    pub fn new() -> Self {
        Self::with_smoothing(DEFAULT_SMOOTHING_MS)
    }

    /// Create a renderer with a custom smoothing time constant (ms).
    /// `smooth_ms <= 0.0` disables smoothing (= exact target gains).
    pub fn with_smoothing(smooth_ms: f32) -> Self {
        Self {
            pan: Vec::new(),
            mode: PanMode::Single,
            triplets: Vec::new(),
            pairs: Vec::new(),
            speaker_count: 0,
            lfe_index: None,
            out_trim: Vec::new(),
            sm: vec![0.0; MAX_SPATIAL_OBJECTS * 16],
            sm_lfe: vec![0.0; MAX_SPATIAL_OBJECTS],
            smooth: one_pole_factor(smooth_ms),
            air_absorption: AirAbsorption::default(),
            sample_rate: 44_100.0,
            prepared: false,
        }
    }

    /// Configure the optional air-absorption model (disabled by default =
    /// exact ×1.0). The cutoff is computed here and exposed for a future
    /// filter stage; this phase applies the broad distance gain only.
    pub fn set_air_absorption(&mut self, a: AirAbsorption) -> &mut Self {
        self.air_absorption = a;
        self
    }

    /// Number of output speakers.
    pub fn channels(&self) -> usize {
        self.speaker_count
    }

    /// Current (smoothed) total gain on an object→speaker path, keyed by the
    /// object's stable store slot. Exposed for symmetry/continuity tests.
    pub fn coefficient(&self, obj_slot: usize, spk: usize) -> f32 {
        if obj_slot >= MAX_SPATIAL_OBJECTS || spk >= self.speaker_count {
            return 0.0;
        }
        self.sm[obj_slot * self.speaker_count + spk]
    }

    /// The pan mode this renderer resolved to (exposed for tests).
    pub fn pan_mode(&self) -> PanMode {
        self.mode
    }

    fn prepare_layout(
        &mut self,
        layout: &SpeakerLayout,
        sample_rate: u32,
    ) -> Result<(), RenderError> {
        layout.validate()?;
        self.sample_rate = sample_rate as f32;

        // Normalise enabled, non-LFE speakers to unit directions.
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
            let dir = s.position.normalized().unwrap_or(Vec3::Y);
            let level = s.gain * layout.calibration.trim_gain(s.id);
            pan.push(PanSpeaker { idx, dir, level });
        }
        let speaker_count = layout.speakers.len();
        let mut out_trim = Vec::with_capacity(speaker_count);
        for s in &layout.speakers {
            out_trim.push(s.gain * layout.calibration.trim_gain(s.id));
        }

        // Classify dimensionality and precompute triplets / pairs.
        let (mode, triplets, pairs) = classify(&pan);

        self.pan = pan;
        self.mode = mode;
        self.triplets = triplets;
        self.pairs = pairs;
        self.speaker_count = speaker_count;
        self.lfe_index = lfe_index;
        self.out_trim = out_trim;
        self.sm = vec![0.0; MAX_SPATIAL_OBJECTS * speaker_count.max(1)];
        self.sm_lfe = vec![0.0; MAX_SPATIAL_OBJECTS];
        self.prepared = true;
        Ok(())
    }

    /// Solve panning coefficients for a listener-space unit direction into
    /// a caller-provided `(oz speaker index, coefficient)` list (≤ 3 entries,
    /// plus room for a spread-fallback entry). `out_gains` is zeroed first.
    fn solve(&self, direction: Vec3, spread: f32, out_gains: &mut [(usize, f32)]) {
        for p in out_gains.iter_mut() {
            *p = (0, 0.0);
        }
        if self.pan.is_empty() {
            return;
        }
        if self.mode == PanMode::ThreeDim {
            self.solve_3d(direction, out_gains);
        } else if self.mode == PanMode::Planar {
            self.solve_planar(direction, out_gains);
        } else {
            let sp = self.pan[0];
            out_gains[0] = (sp.idx, sp.level);
        }
        // Simplified spread (spec §30): blend `spread` of the energy onto the
        // nearest speaker to the direction (a direction-preserving widening),
        // scaling the core down by `(1 - spread)`.
        let spread = spread.clamp(0.0, 1.0);
        if spread > 0.0 && self.pan.len() >= 2 {
            let core = 1.0 - spread;
            for g in out_gains.iter_mut() {
                g.1 *= core;
            }
            let nearest = self.nearest_speaker(direction);
            // Add the spread portion to the nearest speaker slot.
            let mut found = false;
            for g in out_gains.iter_mut() {
                if g.0 == nearest.idx {
                    g.1 += spread * nearest.level;
                    found = true;
                    break;
                }
            }
            if !found {
                // Nearest speaker wasn't in the (non-empty) set; append.
                out_gains[out_gains.len() - 1] = (nearest.idx, spread * nearest.level);
            }
        }
    }

    fn nearest_speaker(&self, direction: Vec3) -> &PanSpeaker {
        let mut best = &self.pan[0];
        let mut best_dot = f32::NEG_INFINITY;
        for sp in &self.pan {
            let d = sp.dir.dot(direction);
            if d > best_dot {
                best_dot = d;
                best = sp;
            }
        }
        best
    }

    fn solve_planar(&self, direction: Vec3, out_gains: &mut [(usize, f32)]) {
        let azimuth = direction.azimuth_rad();
        if self.pairs.is_empty() {
            // Two speakers not in the ring-pair table: hard left/right by az.
            self.solve_2d_fallback(azimuth, out_gains);
            return;
        }
        // Find the pair arc that brackets `azimuth` (smallest width arcs
        // win deterministically).
        let mut best = 0usize;
        let mut best_width = f32::INFINITY;
        for (i, &(_, _, width)) in self.pairs.iter().enumerate() {
            if width < best_width {
                let (_, inside) =
                    fraction_between(self.pan[self.pairs[i].0].dir.azimuth_rad(), azimuth, width);
                if inside {
                    best_width = width;
                    best = i;
                }
            }
        }
        let (la, lb, width) = self.pairs[best];
        let a0 = self.pan[la].dir.azimuth_rad();
        let (t, _) = fraction_between(a0, azimuth, width);
        // Equal-power law across the pair.
        let ga = (t * std::f32::consts::FRAC_PI_2).cos();
        let gb = (t * std::f32::consts::FRAC_PI_2).sin();
        out_gains[0] = (self.pan[la].idx, ga * self.pan[la].level);
        out_gains[1] = (self.pan[lb].idx, gb * self.pan[lb].level);
    }

    fn solve_2d_fallback(&self, azimuth: f32, out_gains: &mut [(usize, f32)]) {
        // Two-speaker archive: apply equal-power from -90° (left) to +90°
        // (right) mapped onto the full azimuth range.
        let t = (azimuth + std::f32::consts::FRAC_PI_2) * std::f32::consts::FRAC_1_PI;
        let t = t.clamp(0.0, 1.0);
        let la = self.pan[0];
        let lb = self.pan[1 % self.pan.len()];
        out_gains[0] = (la.idx, t.cos() * la.level);
        out_gains[1] = (lb.idx, t.sin() * lb.level);
    }

    fn solve_3d(&self, direction: Vec3, out_gains: &mut [(usize, f32)]) {
        // Find the enclosing triplet (all solved coefficients ≥ 0). Among
        // those, prefer the **most balanced** triangle: the one whose smallest
        // coefficient is largest. This is the standard stabiliser over a
        // dense, overlapping triangulation — a point between two adjacent
        // triangles is handled by whichever keeps the three gains closest to
        // equal, so the winning triple stays continuous across region
        // boundaries (§25, §29). Ties fall back to the tightest (small L2)
        // triangle.
        let d = direction;
        let mut best_ti: Option<usize> = None;
        let mut best_min = f32::NEG_INFINITY;
        let mut best_norm_sq = f32::INFINITY;
        let mut cand = [0.0f32; 3];
        for (ti, t) in self.triplets.iter().enumerate() {
            if !solve_triplet(t, d, &mut cand) {
                continue; // degenerate or not enclosing
            }
            let norm_sq = cand[0] * cand[0] + cand[1] * cand[1] + cand[2] * cand[2];
            let min_gain = cand[0].min(cand[1]).min(cand[2]);
            if min_gain > best_min
                || ((min_gain - best_min).abs() <= 1e-6 && norm_sq < best_norm_sq)
            {
                best_min = min_gain;
                best_norm_sq = norm_sq;
                best_ti = Some(ti);
            }
        }
        if let Some(ti) = best_ti {
            let t = &self.triplets[ti];
            // Re-solve so we normalise the winning triangle's coefficients.
            let _ = solve_triplet(t, d, &mut cand);
            let norm = best_norm_sq.max(f32::EPSILON).sqrt();
            for k in 0..3 {
                let spk = t.idx[k];
                out_gains[k] = (
                    spk,
                    (cand[k] / norm) * self.pan[index_of(&self.pan, spk)].level,
                );
            }
        } else {
            // Out-of-coverage: deterministic nearest-speaker fallback.
            let nearest = self.nearest_speaker(direction);
            out_gains[0] = (nearest.idx, nearest.level);
        }
    }
}

impl Default for VbapRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl SpatialRenderer for VbapRenderer {
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
        self.render(scene, object_inputs, frames, out);
        Ok(())
    }
}

/// The shared per-block render core: object level chain → solve → smooth →
/// write. Allocation-free after `prepare`.
impl VbapRenderer {
    fn render(&mut self, scene: &SpatialScene, inputs: &[&[f32]], frames: usize, out: &mut [f32]) {
        let n_spk = self.speaker_count;
        for sample in out[..n_spk * frames].iter_mut() {
            *sample = 0.0;
        }
        let xf = ListenerTransform::from_listener(&scene.listener);
        let mut gains = [(0usize, 0.0f32); 4];
        for (obj_ordinal, (slot, obj)) in scene.objects.iter_enabled().enumerate() {
            let obj_idx = slot;
            let local = xf.apply_to_point(obj.position);
            let dist = local.length();
            let dir = local.normalized().unwrap_or(Vec3::Y);

            // Level chain (spec §68): VBAP places full 3D, so no off-plane
            // `cos(elevation)` term — distance models amplitude only.
            let dist_gain = obj
                .distance_model
                .distance_gain(dist, obj.reference_distance);
            let obj_gain = obj.gain * dist_gain;

            self.solve(dir, obj.spread, &mut gains);

            // Smooth per-object→speaker paths (spec §46) and store totals.
            let row = obj_idx * n_spk;
            for &(spk, coeff) in gains.iter() {
                if coeff == 0.0 {
                    continue;
                }
                let total = coeff * obj_gain;
                let prev = self.sm[row + spk];
                let next = if self.smooth >= 1.0 {
                    total
                } else {
                    prev + self.smooth * (total - prev)
                };
                self.sm[row + spk] = next;
            }

            // LFE send (spec §56) — additive, never a pan target.
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

            let input = inputs.get(obj_ordinal).copied().unwrap_or(&[]);
            if input.len() < frames {
                continue;
            }
            for frame in 0..frames {
                let s = input[frame];
                for &(spk, coeff) in gains.iter() {
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
            }
        }
    }
}

/// Classify pan geometry into a rendering mode and precompute the triplet or
/// pair tables.
fn classify(pan: &[PanSpeaker]) -> (PanMode, Vec<Triplet>, Vec<(usize, usize, f32)>) {
    if pan.is_empty() {
        return (PanMode::Single, Vec::new(), Vec::new());
    }
    if pan.len() == 1 {
        return (PanMode::Single, Vec::new(), Vec::new());
    }
    // Build 3-triplets; if any is normally-oriented, use 3D VBAP.
    let mut triplets: Vec<Triplet> = Vec::new();
    let n = pan.len();
    for i in 0..n {
        for j in (i + 1)..n {
            for k in (j + 1)..n {
                let a = pan[i].dir;
                let b = pan[j].dir;
                let c = pan[k].dir;
                let det = a.dot(b.cross(c));
                if det.abs() <= VBAP_DET_EPSILON {
                    continue; // degenerate / coplanar triangle
                }
                // Inverse of L = [a b c] via adjugate; rows = cross of the
                // other two columns, scaled by 1/det.
                let inv = [
                    [b.cross(c).x / det, b.cross(c).y / det, b.cross(c).z / det],
                    [c.cross(a).x / det, c.cross(a).y / det, c.cross(a).z / det],
                    [a.cross(b).x / det, a.cross(b).y / det, a.cross(b).z / det],
                ];
                let t = Triplet {
                    idx: [pan[i].idx, pan[j].idx, pan[k].idx],
                    inv,
                };
                // Delaunay-style empty-triangle filter (spec §21 "panning
                // regions / convex-hull relationships"): a triplet is a
                // valid panning region only if no other speaker lies inside
                // it or on its boundary. Without this, triangles overlap
                // and a direction sitting exactly on a spurious edge (e.g.
                // the front-centre speaker lying on the FL–FR base of
                // {FL, FR, height}) flips between wildly different
                // coefficient vectors — a knife-edge discontinuity.
                if triplet_is_empty(&t, pan) {
                    triplets.push(t);
                }
            }
        }
    }
    if !triplets.is_empty() {
        return (PanMode::ThreeDim, triplets, Vec::new());
    }
    // Coplanar / pair layout: build the horizontal azimuth-pair ring.
    let mut sorted: Vec<(usize, f32)> = pan
        .iter()
        .map(|sp| (sp.idx, sp.dir.azimuth_rad()))
        .collect();
    sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut pairs: Vec<(usize, usize, f32)> = Vec::with_capacity(sorted.len());
    for i in 0..sorted.len() {
        let j = (i + 1) % sorted.len();
        let width = angle_delta(sorted[i].1, sorted[j].1);
        pairs.push((sorted[i].0, sorted[j].0, width));
    }
    (PanMode::Planar, Vec::new(), pairs)
}

/// Solve one triplet for a direction. Returns Ok(Some([g0,g1,g2])) when the
/// direction is enclosed (all gains ≥ ~0) and the matrix is sane; Ok(None)
/// when not enclosing; Err on degenerate.
fn solve_triplet(t: &Triplet, direction: Vec3, out: &mut [f32; 3]) -> bool {
    let d = direction;
    // g = d · inv (row-major): g_i = d·inv_row_i
    let g0 = d.dot(Vec3::new(t.inv[0][0], t.inv[0][1], t.inv[0][2]));
    let g1 = d.dot(Vec3::new(t.inv[1][0], t.inv[1][1], t.inv[1][2]));
    let g2 = d.dot(Vec3::new(t.inv[2][0], t.inv[2][1], t.inv[2][2]));
    const NEG_TOL: f32 = 1e-4;
    if g0 < -NEG_TOL || g1 < -NEG_TOL || g2 < -NEG_TOL {
        return false;
    }
    // Clamp tiny negatives to zero to keep coefficients non-negative.
    out[0] = if g0 < 0.0 { 0.0 } else { g0 };
    out[1] = if g1 < 0.0 { 0.0 } else { g1 };
    out[2] = if g2 < 0.0 { 0.0 } else { g2 };
    true
}

fn index_of(pan: &[PanSpeaker], idx: usize) -> usize {
    pan.iter().position(|s| s.idx == idx).unwrap_or(0)
}

/// True when no speaker in `pan` other than the triplet's own three lies
/// inside the spherical triangle or on its boundary (the empty-triangle
/// property that makes the triplet set a proper tessellation of the pan
/// region, spec §21).
fn triplet_is_empty(t: &Triplet, pan: &[PanSpeaker]) -> bool {
    for sp in pan {
        if t.idx.contains(&sp.idx) {
            continue;
        }
        // Raw barycentric coordinates of `sp` in the triplet's basis.
        let d = sp.dir;
        let g0 = d.dot(Vec3::new(t.inv[0][0], t.inv[0][1], t.inv[0][2]));
        let g1 = d.dot(Vec3::new(t.inv[1][0], t.inv[1][1], t.inv[1][2]));
        let g2 = d.dot(Vec3::new(t.inv[2][0], t.inv[2][1], t.inv[2][2]));
        const TOL: f32 = 1e-4;
        if g0 >= -TOL && g1 >= -TOL && g2 >= -TOL {
            return false; // another speaker inside or on the boundary
        }
    }
    true
}

/// Turn a smoothing duration (ms) into a per-block one-pole factor.
fn one_pole_factor(ms: f32) -> f32 {
    if ms <= 0.0 {
        return 1.0;
    }
    (1.0 / (1.0 + ms / 8.0)).clamp(0.0, 1.0)
}

/// Signed angle from `a` to `b` counter-clockwise, in `[0, 2π)`.
fn angle_delta(a: f32, b: f32) -> f32 {
    (b - a).rem_euclid(std::f32::consts::TAU)
}

/// Fraction `t ∈ [0,1]` along arc starting at `a` of width `width`, and
/// whether `x` is on that arc (spec geometry pairing).
fn fraction_between(a: f32, x: f32, width: f32) -> (f32, bool) {
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
    use super::super::speaker::SpeakerLayout;
    use super::*;

    const EPS: f32 = 1e-3;

    fn scene_at(pos: Vec3, lfe_send: f32) -> SpatialScene {
        let mut sc = SpatialScene::new(48_000);
        let id = sc.create_audio_object(pos).unwrap();
        if lfe_send > 0.0 {
            sc.object_mut(id).unwrap().lfe_send = lfe_send;
        }
        sc
    }

    fn render_once(r: &mut VbapRenderer, scene: &SpatialScene, frames: usize) -> (Vec<f32>, bool) {
        let ch = r.channels();
        let mut out = vec![0.0f32; ch * frames];
        let input = vec![1.0f32; frames];
        r.process_block(scene, &[&input], frames, &mut out).unwrap();
        let finite = out.iter().all(|x| x.is_finite());
        (out, finite)
    }

    #[test]
    fn seven_one_four_is_three_dim() {
        let mut r = VbapRenderer::with_smoothing(0.0);
        r.prepare(&SpeakerLayout::seven_point_one_four(), 48_000)
            .unwrap();
        assert_eq!(r.pan_mode(), PanMode::ThreeDim);
    }

    #[test]
    fn stereo_is_planar_pair() {
        let mut r = VbapRenderer::with_smoothing(0.0);
        r.prepare(&SpeakerLayout::stereo(), 48_000).unwrap();
        assert_eq!(r.pan_mode(), PanMode::Planar);
    }

    #[test]
    fn planar_four_ring_is_planar() {
        let layout = SpeakerLayout::custom(vec![
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, -1.0, 0.0),
        ]);
        let mut r = VbapRenderer::with_smoothing(0.0);
        r.prepare(&layout, 48_000).unwrap();
        assert_eq!(r.pan_mode(), PanMode::Planar);
    }

    #[test]
    fn front_object_in_stereo_is_equal_split() {
        let layout = SpeakerLayout::stereo();
        let mut r = VbapRenderer::with_smoothing(0.0);
        r.prepare(&layout, 48_000).unwrap();
        let scene = scene_at(Vec3::Y, 0.0);
        let (out, finite) = render_once(&mut r, &scene, 8);
        assert!(finite);
        let fl = out[0];
        let fr = out[1];
        assert!(
            (fl - std::f32::consts::FRAC_1_SQRT_2).abs() < EPS,
            "fl={fl}"
        );
        assert!(
            (fr - std::f32::consts::FRAC_1_SQRT_2).abs() < EPS,
            "fr={fr}"
        );
        let energy = fl * fl + fr * fr;
        assert!((energy - 1.0).abs() < 1e-4, "energy={energy}");
    }

    #[test]
    fn planar_ring_energy_and_symmetry() {
        let layout = SpeakerLayout::custom(vec![
            Vec3::new(-1.0, 0.0, 0.0), // left
            Vec3::new(0.0, 1.0, 0.0),  // front
            Vec3::new(1.0, 0.0, 0.0),  // right
            Vec3::new(0.0, -1.0, 0.0), // rear
        ]);
        let mut r = VbapRenderer::with_smoothing(0.0);
        r.prepare(&layout, 48_000).unwrap();
        let mut energy_ok = true;
        let frames = 8usize;
        for deg in 0..=360 {
            let rad = (deg as f32).to_radians();
            let dir = Vec3::new(rad.sin(), rad.cos(), 0.0);
            let scene = scene_at(dir, 0.0);
            let (out, finite) = render_once(&mut r, &scene, frames);
            assert!(finite, "no NaN at {deg}°");
            let e: f32 = (0..4).map(|s| out[s] * out[s]).sum();
            if (e - 1.0).abs() > 4e-2 {
                energy_ok = false;
                break;
            }
        }
        assert!(energy_ok, "planar ring preserves energy");

        // Symmetry: left ↔ right mirrored.
        let (lout, _) = render_once(&mut r, &scene_at(Vec3::new(-1.0, 1.0, 0.0), 0.0), 8);
        let (rout, _) = render_once(&mut r, &scene_at(Vec3::new(1.0, 1.0, 0.0), 0.0), 8);
        assert!((lout[0] - rout[2]).abs() < 1e-3, "left [0] vs right [2]");
        assert!((lout[2] - rout[0]).abs() < 1e-3, "left [2] vs right [0]");
    }

    #[test]
    fn three_dim_enclosing_triplet_favors_speaker_and_energy() {
        let mut r = VbapRenderer::with_smoothing(0.0);
        r.prepare(&SpeakerLayout::seven_point_one_four(), 48_000)
            .unwrap();
        let frames = 8usize;
        let mut covered = 0usize;
        let mut energy_max = 0.0f32;
        // Sample the sphere (fine-ish grid) and check energy for covered dirs.
        for el_deg in (-60..=60).step_by(15) {
            for az_deg in (0..360).step_by(15) {
                let el = (el_deg as f32).to_radians();
                let az = (az_deg as f32).to_radians();
                let x = el.cos() * az.sin();
                let y = el.cos() * az.cos();
                let z = el.sin();
                let scene = scene_at(Vec3::new(x, y, z), 0.0);
                let (out, finite) = render_once(&mut r, &scene, frames);
                assert!(finite, "NaN at el={el_deg} az={az_deg}");
                // Sum over all 12 channels (LFE excluded: index 3 in 7.1.4).
                let mut e = 0.0f32;
                for (spk, v) in out.iter().take(12).enumerate() {
                    if spk == 3 {
                        continue; // LFE
                    }
                    e += v * v;
                }
                if e > 2e-3 {
                    covered += 1;
                    energy_max = energy_max.max(e);
                }
            }
        }
        assert!(covered > 0, "some directions must be covered");
        // Energy for covered directions stays near 1 (bound the max drift).
        assert!(energy_max < 1.15, "3D VBAP energy overshoot: {energy_max}");
    }

    #[test]
    fn out_of_coverage_falls_back_to_nearest_deterministically() {
        // 7.1.4 has no speakers below the floor. A downward-pointing object
        // must still render (nearest-speaker fallback), deterministically and
        // without NaN.
        let mut r = VbapRenderer::with_smoothing(0.0);
        r.prepare(&SpeakerLayout::seven_point_one_four(), 48_000)
            .unwrap();
        let scene = scene_at(Vec3::new(0.0, 0.0, -1.0), 0.0); // straight down
        let (out, finite) = render_once(&mut r, &scene, 8);
        assert!(finite);
        let energy: f32 = (0..12).map(|s| out[s] * out[s]).sum();
        assert!(
            energy > 0.5,
            "out-of-coverage still delivers energy ({energy})"
        );
        // Deterministic: same scene → same output.
        let (out2, _) = render_once(&mut r, &scene, 8);
        assert!(
            (0..12).all(|s| (out[s] - out2[s]).abs() < 1e-6),
            "out-of-coverage fallback must be deterministic"
        );
    }

    #[test]
    fn degenerate_coplanar_only_is_not_nan() {
        // Three speakers on the same plane (no height) → no 3D triplet, must
        // reduce to Planar and stay finite and energy-sane on the plane.
        let layout = SpeakerLayout::custom(vec![
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        ]);
        let mut r = VbapRenderer::with_smoothing(0.0);
        r.prepare(&layout, 48_000).unwrap();
        assert_eq!(r.pan_mode(), PanMode::Planar);
        for deg in 0..=360 {
            let rad = (deg as f32).to_radians();
            let scene = scene_at(Vec3::new(rad.sin(), rad.cos(), 0.0), 0.0);
            let (_out, finite) = render_once(&mut r, &scene, 8);
            assert!(finite, "NaN in planar coplanar layout at {deg}°");
        }
    }

    #[test]
    fn lfe_is_additive_send_never_pan_target() {
        let mut r = VbapRenderer::with_smoothing(0.0);
        r.prepare(&SpeakerLayout::seven_point_one_four(), 48_000)
            .unwrap();
        // Object low, slight lfe send.
        let scene = scene_at(Vec3::new(0.5, 0.5, 0.0), 0.8);
        let (out, _) = render_once(&mut r, &scene, 8);
        // LFE index 3 in seven_point_one_four order.
        for i in 0..8 {
            assert!(out[i * 12 + 3] > 0.4, "lfe_send reaches LFE channel");
        }
    }

    #[test]
    fn continuity_around_azimuth_with_smoothing() {
        let mut r = VbapRenderer::with_smoothing(DEFAULT_SMOOTHING_MS);
        r.prepare(&SpeakerLayout::seven_point_one_four(), 48_000)
            .unwrap();
        let mut scene = scene_at(Vec3::Y, 0.0);
        let frames = 64usize;
        let ch = r.channels();
        let mut out = vec![0.0f32; ch * frames];
        let input = vec![1.0f32; frames];
        let mut prev_fl = 0.0f32;
        // Fine sweep (0.25° steps). The per-block delta of a *continuous*
        // gain field scales with step size; a true region-boundary jump
        // (target hop ≳ 0.2) would still show up as a ≥ α·0.2 ≈ 0.05 step
        // after smoothing, while the legitimate steep-but-continuous slope
        // near the exact front-center direction stays under it.
        for step in 0..1440 {
            let deg = step as f32 * 0.25;
            // Rotate the listener to sweep apparent direction by yaw.
            scene
                .listener
                .set_orientation(super::super::math::Quat::from_euler_rad(
                    deg.to_radians(),
                    0.0,
                    0.0,
                ));
            r.process_block(&scene, &[&input], frames, &mut out)
                .unwrap();
            assert!(out.iter().all(|x| x.is_finite()), "NaN at yaw {deg}°");
            let fl = out[0];
            if step > 0 {
                let delta = (fl - prev_fl).abs();
                assert!(delta < 0.05, "no jump in FL at yaw {deg}° ({delta})");
            }
            prev_fl = fl;
        }
    }
}
