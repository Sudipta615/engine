//! Room acoustics — early reflections + late field (spec §49, §43–44, §55).
//!
//! The room is a scene-level acoustic space (axis-aligned box in world
//! space) that turns every source into a *small acoustic event*:
//!
//! - **Early reflections** — the image-source method: mirror the source
//!   across the six walls (order 1 = 6 images, order 2 = 24 distinct
//!   images), render each image as its own virtual source (its own pan
//!   solve, distance attenuation, and reflection-coefficient amplitude),
//!   and delay it by the excess path length `(dist_image − dist_direct)/c`
//!   through a per-object delay ring. Occlusion's
//!   [`AcousticTransmission`](crate::spatial::occlusion::AcousticTransmission)
//!   is the transmission seam (§43–44): the same low-passed sample that
//!   feeds the direct path also feeds the reflections.
//! - **Late field** — a Schroeder tail (parallel feedback combs + serial
//!   allpasses) shaped by the room's `rt60_ms`, driven by each object's
//!   `room_send`, whose output **encodes into the ambisonic bus** (§55) and
//!   is decoded onto every pan speaker with the field mixer's `√N` diffuse
//!   compensation and per-speaker decorrelation — the diffuse decay of the
//!   room, not a point source.
//!
//! ```text
//! object ──┬─ direct path (existing level chain + pan)
//!          ├─ early reflections: image i ── pan solve + coeff·dist_level
//!          │        ── delayed by (dist_i − direct)/c via per-object ring
//!          └─ room send ── Schroeder tail ── ambisonic W ── decode ── speakers
//! ```
//!
//! ## Participation
//!
//! The room is **opt-in per scene and per object**: `Room::default()` is
//! disabled (the render path is bit-identical to the no-room render), and an
//! object participates only when its `room_send` is non-zero — the seam the
//! scene model declared (§15). Walls share one absorption coefficient
//! (per-wall absorption is a documented seam).
//!
//! ## Realtime discipline
//!
//! All image-source geometry is computed per object per block from the
//! scene's current positions (pure arithmetic, no allocation); the delay
//! rings, smoothing matrix, tap lists, and the Schroeder buffers are
//! preallocated at `prepare`. The per-frame hot path is one ring store plus
//! one delayed read per active tap — bounded, lock-free, allocation-free.

use super::math::Vec3;
use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::spatial::object::MAX_SPATIAL_OBJECTS;

/// Ceiling on distinct image sources per object (an order-2 box yields 24).
pub const MAX_IMAGES: usize = 32;

/// Early-reflection delay ring length in samples (≈ 171 ms @ 48 kHz).
/// Reflections whose excess delay exceeds it are clamped to the last ring
/// sample (documented bound — the ring is a fixed preallocation).
pub const MAX_ROOM_DELAY_SAMPLES: usize = 8192;

/// Per-object reflection-tap ceiling (`MAX_IMAGES` images × 4 pan writes).
const MAX_TAPS_PER_OBJECT: usize = MAX_IMAGES * 4;

/// The room (spec §49): an axis-aligned box in world space.
///
/// Defaults are **disabled** so existing scenes render bit-identically; a
/// host enables the room and opts objects in via their `room_send`.
#[derive(Debug, Clone, PartialEq)]
pub struct Room {
    pub enabled: bool,
    /// Room width (x), metres, origin at a corner.
    pub width: f32,
    /// Room depth (y), metres.
    pub depth: f32,
    /// Room height (z), metres.
    pub height: f32,
    /// Wall absorption `0..1` (one coefficient for all walls; per-wall
    /// absorption is a documented seam). Reflection coefficient per wall =
    /// `1 − absorption`.
    pub absorption: f32,
    /// Early-reflection order: `1` = the six first-order images, `2` = plus
    /// the second-order set (24 distinct images total).
    pub reflection_order: u8,
    /// Late-field RT60 (ms) — the Schroeder tail's decay time.
    pub rt60_ms: f32,
    /// Late-field wet mix `0..1` — scales the tail before the ambisonic
    /// decode.
    pub late_mix: f32,
    /// Speed of sound (m/s), used for the reflection delays.
    pub speed_of_sound: f32,
}

impl Default for Room {
    fn default() -> Self {
        Self {
            enabled: false,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 1,
            rt60_ms: 800.0,
            late_mix: 0.3,
            speed_of_sound: 343.0,
        }
    }
}

/// A wall reflection coefficient: `1 − absorption`.
#[inline]
pub fn reflection_coefficient(room: &Room) -> f32 {
    (1.0 - room.absorption.clamp(0.0, 0.999)).max(0.0)
}

/// A raw image source in **world space**: the mirrored source position and
/// the product of the crossed walls' reflection coefficients.
#[derive(Debug, Clone, Copy)]
pub struct ReflectionImage {
    pub position: Vec3,
    pub coeff: f32,
}

impl ReflectionImage {
    pub const ZERO: Self = Self {
        position: Vec3::ZERO,
        coeff: 0.0,
    };
}

/// Enumerate the image sources of `source` in `room` up to `order` (`1` or
/// `2`), by breadth-first reflection across the six walls with deduplication.
///
/// Order 1 → 6 images; order 2 → 24 distinct images (two crossings on one
/// axis, one crossing each on two axes). Returns the count written to `out`.
pub fn image_sources(room: &Room, source: Vec3, out: &mut [ReflectionImage; MAX_IMAGES]) -> usize {
    let w = room.width.max(0.1);
    let d = room.depth.max(0.1);
    let h = room.height.max(0.1);
    let r = reflection_coefficient(room);
    let order = room.reflection_order.clamp(1, 2);

    // BFS frontiers; order ≤ 2 bounds the total to 1 + 6 + 36 = 43 nodes.
    let mut current = [(Vec3::ZERO, 0.0f32); MAX_IMAGES * 4];
    let mut next = [(Vec3::ZERO, 0.0f32); MAX_IMAGES * 4];
    let mut seen = [Vec3::ZERO; MAX_IMAGES * 4];
    current[0] = (source, 1.0);
    let mut n_cur = 1usize;
    seen[0] = source;
    let mut n_seen = 1usize;
    let mut count = 0usize;

    for _ in 0..order {
        let mut n_next = 0usize;
        for &(pos, coeff) in current[..n_cur].iter() {
            // Reflect across each of the six walls.
            let mut cand = [
                Vec3::new(-pos.x, pos.y, pos.z),
                Vec3::new(2.0 * w - pos.x, pos.y, pos.z),
                Vec3::new(pos.x, -pos.y, pos.z),
                Vec3::new(pos.x, 2.0 * d - pos.y, pos.z),
                Vec3::new(pos.x, pos.y, -pos.z),
                Vec3::new(pos.x, pos.y, 2.0 * h - pos.z),
            ];
            for c in cand.iter_mut() {
                let mut fresh = true;
                for s in seen[..n_seen].iter() {
                    let d = *c - *s;
                    if d.dot(d) < 1e-8 {
                        fresh = false;
                        break;
                    }
                }
                if !fresh {
                    continue;
                }
                seen[n_seen] = *c;
                n_seen += 1;
                if count < MAX_IMAGES {
                    out[count] = ReflectionImage {
                        position: *c,
                        coeff: coeff * r,
                    };
                    count += 1;
                }
                if n_next < MAX_IMAGES * 4 {
                    next[n_next] = (*c, coeff * r);
                    n_next += 1;
                }
            }
        }
        current[..n_next].copy_from_slice(&next[..n_next]);
        n_cur = n_next;
    }
    count
}

/// A listener-relative image source ready for the renderer: direction
/// (world space, from the listener), distance, reflection coefficient, and
/// the excess-path delay in samples (clamped to the ring).
#[derive(Debug, Clone, Copy)]
pub struct ListenerImage {
    /// Unit direction from the listener to the image (world space).
    pub dir: Vec3,
    /// Distance from the listener to the image (m).
    pub dist: f32,
    /// Product of crossed walls' reflection coefficients.
    pub coeff: f32,
    /// Relative delay in samples: `(dist − direct)/c · fs`, clamped.
    pub delay: u32,
}

impl ListenerImage {
    pub const ZERO: Self = Self {
        dir: Vec3::ZERO,
        dist: 0.0,
        coeff: 0.0,
        delay: 0,
    };
}

/// Renderer-owned early-reflection engine: per-object delay rings, a
/// per-(object, image, speaker) smoothed tap-gain matrix, and a room-send
/// accumulator that feeds the late field. Allocation-free after `prepare`.
#[derive(Debug)]
pub struct EarlyReflections {
    /// Flat `MAX_OBJECTS × ring_len` delay-ring storage (one ring per
    /// object, all written at a common cursor).
    ring: Vec<f32>,
    ring_len: usize,
    block_write: usize,
    /// Smoothed per-(object, image, speaker) tap gains, flat
    /// `MAX_OBJECTS × MAX_IMAGES × speaker_count`.
    rsm: Vec<f32>,
    speaker_count: usize,
    /// Per-object active tap lists `(image, delay, speaker)` packed.
    taps: [[(u16, u16, u16); MAX_TAPS_PER_OBJECT]; MAX_SPATIAL_OBJECTS],
    tap_len: [u16; MAX_SPATIAL_OBJECTS],
    /// Room-send accumulator (one sample per frame of the block).
    send: Vec<f32>,
    smooth: f32,
    sample_rate: f32,
    prepared: bool,
}

impl Default for EarlyReflections {
    fn default() -> Self {
        Self::new()
    }
}

impl EarlyReflections {
    pub fn new() -> Self {
        Self {
            ring: Vec::new(),
            ring_len: MAX_ROOM_DELAY_SAMPLES,
            block_write: 0,
            rsm: Vec::new(),
            speaker_count: 0,
            taps: [[(0, 0, 0); MAX_TAPS_PER_OBJECT]; MAX_SPATIAL_OBJECTS],
            tap_len: [0; MAX_SPATIAL_OBJECTS],
            send: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            smooth: 1.0,
            sample_rate: 48_000.0,
            prepared: false,
        }
    }

    /// Control path: allocate the rings, the tap matrix, and the send
    /// scratch. `smooth` is the renderer's per-block one-pole factor.
    pub fn prepare(&mut self, speaker_count: usize, sample_rate: u32, smooth: f32) {
        self.speaker_count = speaker_count.max(1);
        self.sample_rate = sample_rate as f32;
        self.smooth = smooth;
        self.ring_len = MAX_ROOM_DELAY_SAMPLES;
        self.ring = vec![0.0; MAX_SPATIAL_OBJECTS * self.ring_len];
        self.rsm = vec![0.0; MAX_SPATIAL_OBJECTS * MAX_IMAGES * self.speaker_count];
        self.send.fill(0.0);
        self.block_write = 0;
        self.prepared = true;
    }

    /// Compute the object's listener-relative image sources (order from the
    /// room). Pure arithmetic; writes into `out` (fixed array). Returns the
    /// count.
    pub fn images_for_object(
        &self,
        room: &Room,
        listener_pos: Vec3,
        obj_pos: Vec3,
        out: &mut [ListenerImage; MAX_IMAGES],
    ) -> usize {
        if !self.prepared {
            return 0;
        }
        let mut raw = [ReflectionImage::ZERO; MAX_IMAGES];
        let n = image_sources(room, obj_pos, &mut raw);
        let direct = (obj_pos - listener_pos).length();
        let speed = room.speed_of_sound.max(1.0);
        let max_delay = (self.ring_len - 1) as u32;
        let mut count = 0usize;
        for r in raw.iter().take(n) {
            let to = r.position - listener_pos;
            let dist = to.length();
            let dir = if dist > 1e-6 {
                to * (1.0 / dist)
            } else {
                Vec3::Y
            };
            let rel = ((dist - direct).max(0.0) / speed * self.sample_rate).round() as u32;
            out[count] = ListenerImage {
                dir,
                dist,
                coeff: r.coeff,
                delay: rel.min(max_delay),
            };
            count += 1;
        }
        count
    }

    /// Zero the tap list for one object (start of its block work).
    pub fn begin_object(&mut self, obj_slot: usize) {
        self.tap_len[obj_slot] = 0;
    }

    /// Smooth one (object, image, speaker) tap gain toward `target` and
    /// register it in the object's tap list. Zero targets are skipped.
    pub fn add_tap(&mut self, obj_slot: usize, img: usize, spk: usize, delay: u32, target: f32) {
        if target == 0.0 {
            return;
        }
        let stride = self.speaker_count;
        let idx = obj_slot * (MAX_IMAGES * stride) + img * stride + spk;
        let prev = self.rsm[idx];
        let next = if self.smooth >= 1.0 {
            target
        } else {
            prev + self.smooth * (target - prev)
        };
        self.rsm[idx] = next;
        let tlen = self.tap_len[obj_slot] as usize;
        if tlen < MAX_TAPS_PER_OBJECT {
            self.taps[obj_slot][tlen] = (img as u16, delay as u16, spk as u16);
            self.tap_len[obj_slot] = (tlen + 1) as u16;
        }
    }

    /// Zero the room-send accumulator for this block. Called once per block
    /// before the object loop when the room is active.
    pub fn begin_block(&mut self, frames: usize) {
        for s in self.send.iter_mut().take(frames) {
            *s = 0.0;
        }
        self.block_write %= self.ring_len;
    }

    /// Per-frame, per-object: store the (filtered) sample in the object's
    /// ring at this frame's cursor, read every active tap delayed by its
    /// image's excess path, and accumulate the room send.
    ///
    /// `room_send_gain` = `obj.gain × obj.room_send` (0 skips the send).
    #[allow(clippy::too_many_arguments)] // per-frame hot-path helper shared by both renderers
    pub fn object_frame(
        &mut self,
        obj_slot: usize,
        sample: f32,
        room_send_gain: f32,
        frame: usize,
        n_spk: usize,
        out: &mut [f32],
        out_trim: &[f32],
    ) {
        let rl = self.ring_len;
        let w = (self.block_write + frame) % rl;
        let row = obj_slot * rl;
        self.ring[row + w] = sample;
        let taps = &self.taps[obj_slot];
        let len = self.tap_len[obj_slot] as usize;
        for t in taps.iter().take(len) {
            let (img, delay, spk) = (t.0 as usize, t.1 as usize, t.2 as usize);
            let r = (w + rl - delay) % rl;
            let gain = self.rsm[obj_slot * (MAX_IMAGES * n_spk) + img * n_spk + spk]
                * out_trim.get(spk).copied().unwrap_or(0.0);
            if gain != 0.0 {
                out[frame * n_spk + spk] += self.ring[row + r] * gain;
            }
        }
        if room_send_gain != 0.0 {
            self.send[frame] += sample * room_send_gain;
        }
    }

    /// Advance the ring cursor to the end of this block.
    pub fn end_block(&mut self, frames: usize) {
        self.block_write = (self.block_write + frames) % self.ring_len;
    }

    /// The accumulated room-send plane (one sample per frame of the block).
    pub fn send(&self) -> &[f32] {
        &self.send
    }
}

/// Late-field tail: a Schroeder reverb (4 parallel feedback combs whose
/// gains are derived from the room's RT60, 2 serial allpasses for density).
/// The tail output is a mono plane that the renderer encodes into the
/// ambisonic bus via the field mixer's diffuse path (§55).
#[derive(Debug)]
pub struct RoomLateField {
    comb_delay: [usize; 4],
    comb_gain: [f32; 4],
    comb_off: [usize; 4],
    comb_pos: [usize; 4],
    comb_buf: Vec<f32>,
    ap_delay: [usize; 2],
    ap_off: [usize; 2],
    ap_pos: [usize; 2],
    ap_buf: Vec<f32>,
    sample_rate: f32,
    prepared: bool,
}

/// Serial-allpass feedback coefficient (fixed; density shaping).
const AP_FEEDBACK: f32 = 0.7;

impl Default for RoomLateField {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomLateField {
    pub fn new() -> Self {
        Self {
            comb_delay: [1553, 1619, 1777, 2053],
            comb_gain: [0.0; 4],
            comb_off: [0; 4],
            comb_pos: [0; 4],
            comb_buf: Vec::new(),
            ap_delay: [225, 261],
            ap_off: [0; 2],
            ap_pos: [0; 2],
            ap_buf: Vec::new(),
            sample_rate: 48_000.0,
            prepared: false,
        }
    }

    /// Control path: size the comb/allpass delay lines to the sample rate
    /// (the fixed delay set is scaled from the 48 kHz reference).
    pub fn prepare(&mut self, sample_rate: u32) {
        let scale = sample_rate as f32 / 48_000.0;
        let mut off = 0usize;
        for c in 0..4 {
            self.comb_delay[c] = ((self.comb_delay[c] as f32 * scale).round() as usize).max(64);
            self.comb_off[c] = off;
            off += self.comb_delay[c];
        }
        self.comb_buf = vec![0.0; off];
        let mut off = 0usize;
        for a in 0..2 {
            self.ap_delay[a] = ((self.ap_delay[a] as f32 * scale).round() as usize).max(8);
            self.ap_off[a] = off;
            off += self.ap_delay[a];
        }
        self.ap_buf = vec![0.0; off];
        self.sample_rate = sample_rate as f32;
        self.prepared = true;
    }

    /// Process the room-send plane through the tail, writing the late-field
    /// samples into `out` (the caller's preallocated scratch — the renderer
    /// passes its block-sized buffer, tests pass larger ones). Returns the
    /// number of frames written (`min(frames, send.len(), out.len())`).
    /// Comb gains are re-derived from the room's current RT60 each block
    /// (cheap, deterministic — no clicks on config change beyond the
    /// natural rate limit). Allocation-free.
    pub fn process(&mut self, room: &Room, send: &[f32], frames: usize, out: &mut [f32]) -> usize {
        if !self.prepared {
            return 0;
        }
        let frames = frames.min(send.len()).min(out.len());
        let rt60 = room.rt60_ms.max(1.0) / 1000.0;
        let fs = self.sample_rate;
        for c in 0..4 {
            let d = self.comb_delay[c] as f32;
            self.comb_gain[c] = 10f32.powf(-3.0 * d / (rt60 * fs));
        }
        for f in 0..frames {
            let x = send[f];
            let mut acc = 0.0f32;
            for c in 0..4 {
                let pos = self.comb_pos[c];
                let idx = self.comb_off[c] + pos;
                let y = self.comb_buf[idx];
                self.comb_buf[idx] = x + self.comb_gain[c] * y;
                // (1 − g) normalizes each comb to unit DC gain → the summed
                // tail stays bounded whatever the RT60.
                acc += y * (1.0 - self.comb_gain[c]);
                self.comb_pos[c] = (pos + 1) % self.comb_delay[c];
            }
            let mut s = acc * 0.25;
            for a in 0..2 {
                let pos = self.ap_pos[a];
                let idx = self.ap_off[a] + pos;
                let w_old = self.ap_buf[idx];
                let y = AP_FEEDBACK * s + w_old;
                self.ap_buf[idx] = s - AP_FEEDBACK * y;
                s = y;
                self.ap_pos[a] = (pos + 1) % self.ap_delay[a];
            }
            out[f] = s;
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-4;

    #[test]
    fn default_room_is_disabled_and_sane() {
        let r = Room::default();
        assert!(!r.enabled, "default room is disabled (bit-exact)");
        assert!(r.width > 0.0 && r.depth > 0.0 && r.height > 0.0);
        assert!((reflection_coefficient(&r) - 0.8).abs() < EPS);
        assert!(r.reflection_order >= 1 && r.reflection_order <= 2);
    }

    #[test]
    fn first_order_images_match_closed_form() {
        let room = Room {
            enabled: true,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 1,
            ..Default::default()
        };
        let mut imgs = [ReflectionImage::ZERO; MAX_IMAGES];
        let n = image_sources(&room, Vec3::new(3.0, 4.0, 2.0), &mut imgs);
        assert_eq!(n, 6, "order 1 → 6 images");
        let r = 0.8f32;
        let expected = [
            (Vec3::new(-3.0, 4.0, 2.0), r), // x = 0
            (Vec3::new(21.0, 4.0, 2.0), r), // x = 12
            (Vec3::new(3.0, -4.0, 2.0), r), // y = 0
            (Vec3::new(3.0, 16.0, 2.0), r), // y = 10
            (Vec3::new(3.0, 4.0, -2.0), r), // z = 0
            (Vec3::new(3.0, 4.0, 4.0), r),  // z = 3
        ];
        for (pos, coeff) in expected {
            let found = imgs[..n].iter().any(|i| {
                let d = i.position - pos;
                d.dot(d) < 1e-6 && (i.coeff - coeff).abs() < EPS
            });
            assert!(found, "missing image at {pos:?} coeff {coeff}");
        }
    }

    #[test]
    fn second_order_images_count_and_coefficients() {
        let room = Room {
            enabled: true,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 2,
            ..Default::default()
        };
        let mut imgs = [ReflectionImage::ZERO; MAX_IMAGES];
        let n = image_sources(&room, Vec3::new(3.0, 4.0, 2.0), &mut imgs);
        assert_eq!(n, 24, "order 2 → 24 distinct images");
        let r = 0.8f32;
        // Two different walls → coeff r².
        for (pos, coeff) in [
            (Vec3::new(-3.0, -4.0, 2.0), r * r),
            (Vec3::new(21.0, 16.0, 2.0), r * r),
            (Vec3::new(-3.0, 4.0, -2.0), r * r),
        ] {
            let found = imgs[..n].iter().any(|i| {
                let d = i.position - pos;
                d.dot(d) < 1e-6 && (i.coeff - coeff).abs() < EPS
            });
            assert!(found, "missing two-wall image at {pos:?}");
        }
        // Same axis, both walls (2 crossings on x) → coeff r², positions
        // 2W+sx and sx−2W.
        for (pos, coeff) in [
            (Vec3::new(27.0, 4.0, 2.0), r * r),
            (Vec3::new(-21.0, 4.0, 2.0), r * r),
        ] {
            let found = imgs[..n].iter().any(|i| {
                let d = i.position - pos;
                d.dot(d) < 1e-6 && (i.coeff - coeff).abs() < EPS
            });
            assert!(found, "missing same-axis image at {pos:?}");
        }
    }

    #[test]
    fn listener_relative_delays_match_path_difference() {
        let mut er = EarlyReflections::new();
        er.prepare(6, 48_000, 1.0);
        let room = Room {
            enabled: true,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 1,
            ..Default::default()
        };
        // Listener at centre, object 4 m to the left of the left wall's
        // mirror plane: direct = 5 m; left-wall image dist = 7 m.
        let listener = Vec3::new(6.0, 5.0, 1.5);
        let obj = Vec3::new(1.0, 5.0, 1.5);
        let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
        let n = er.images_for_object(&room, listener, obj, &mut imgs);
        assert_eq!(n, 6);
        let direct = 5.0f32;
        for img in imgs.iter().take(n) {
            let expect_delay = ((img.dist - direct).max(0.0) / 343.0 * 48_000.0).round() as u32;
            assert_eq!(
                img.delay, expect_delay,
                "delay = excess path / c for dist {}",
                img.dist
            );
        }
        // The left-wall image: dir −X (world), dist 7, delay 280.
        let left = imgs
            .iter()
            .find(|i| {
                let d = i.dir - Vec3::new(-1.0, 0.0, 0.0);
                d.dot(d) < 1e-6
            })
            .expect("left-wall image");
        assert!((left.dist - 7.0).abs() < 1e-4);
        assert_eq!(left.delay, 280);
        assert!((left.coeff - 0.8).abs() < EPS);
    }

    #[test]
    fn engine_delays_an_impulse_by_the_image_excess_path() {
        // End-to-end engine check: an impulse object renders its left-wall
        // reflection 280 frames later, at the tap gain, on the pan speaker.
        let mut er = EarlyReflections::new();
        er.prepare(6, 48_000, 1.0); // smoothing off → exact taps
        let room = Room {
            enabled: true,
            width: 12.0,
            depth: 10.0,
            height: 3.0,
            absorption: 0.2,
            reflection_order: 1,
            ..Default::default()
        };
        let listener = Vec3::new(6.0, 5.0, 1.5);
        let obj_pos = Vec3::new(1.0, 5.0, 1.5);
        let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
        let n = er.images_for_object(&room, listener, obj_pos, &mut imgs);
        er.begin_object(0);
        // Simulate the renderer: pan the left-wall image to speaker 4 with a
        // hard gain (1.0), all others to speaker 0 with small gains.
        let target_for = |img: &ListenerImage| -> (usize, f32) {
            if img.delay == 280 {
                (4, 0.8)
            } else {
                (0, 0.05)
            }
        };
        for (i, img) in imgs.iter().take(n).enumerate() {
            let (spk, g) = target_for(img);
            er.add_tap(0, i, spk, img.delay, g);
        }
        let frames = 512usize;
        let mut out = vec![0.0f32; 6 * frames];
        let trim = vec![1.0f32; 6];
        er.begin_block(frames);
        let mut input = vec![0.0f32; frames];
        input[0] = 1.0;
        for (f, &s) in input.iter().enumerate() {
            er.object_frame(0, s, 1.0, f, 6, &mut out, &trim);
        }
        er.end_block(frames);
        // The delayed tap: frame 280, speaker 4, amplitude 1.0 × 0.8.
        assert!(
            (out[280 * 6 + 4] - 0.8).abs() < 1e-4,
            "reflection at 280 on spk 4: {}",
            out[280 * 6 + 4]
        );
        // Nothing on speaker 4 before the delay.
        assert!(out[279 * 6 + 4].abs() < 1e-6);
        // The direct impulse (send) is not panned by the engine itself.
        assert!(out[4].abs() < 1e-6);
    }

    #[test]
    fn schroeder_tail_rt60_matches_config() {
        // Measure the tail the way reverb tails are measured in practice:
        // excite with a steady source for 1 s, cut it off, and fit the
        // post-cutoff envelope decay. This avoids the comb-onset transients
        // an impulse response shows (each comb's first echo arrives at its
        // own delay, so a single-impulse envelope is not monotonic).
        let mut tail = RoomLateField::new();
        tail.prepare(48_000);
        let room = Room {
            enabled: true,
            rt60_ms: 500.0,
            ..Default::default()
        };
        let total = 240_000usize; // 5 s
        let cutoff = 48_000usize; // 1 s of excitation, then silence
        let mut send = vec![0.0f32; total];
        send[..cutoff].fill(1.0);
        let mut out = vec![0.0f32; total];
        let n = tail.process(&room, &send, total, &mut out);
        assert_eq!(n, total);
        // Steady state ≈ unity (the (1 − g) normalization), then a decay.
        let steady = out[40_000..44_000]
            .iter()
            .fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(steady > 0.5, "tail reaches steady state ({steady})");

        // Fit the log-magnitude envelope in 50 ms windows from 100 ms after
        // the cutoff (all combs ringing, no onset transient) down to −70 dB.
        let win = 2_400usize; // 50 ms
        let start = cutoff + 4_800; // +100 ms
        let mut env: Vec<(f32, f32)> = Vec::new();
        let mut w = start;
        while w + win <= total {
            let e = out[w..w + win].iter().map(|v| v * v).sum::<f32>() / win as f32;
            let db = 10.0 * e.log10();
            if db > -70.0 {
                env.push((w as f32 / 48_000.0, db));
            }
            w += win;
        }
        assert!(env.len() > 5, "enough decay windows ({})", env.len());
        // Linear fit db = a·t + b (ordinary least squares).
        let seg = &env[..env.len() - 1];
        let m = seg.len() as f32;
        let sx: f32 = seg.iter().map(|&(t, _)| t).sum();
        let sy: f32 = seg.iter().map(|&(_, d)| d).sum();
        let sxx: f32 = seg.iter().map(|&(t, _)| t * t).sum();
        let sxy: f32 = seg.iter().map(|&(t, d)| t * d).sum();
        let a = (m * sxy - sx * sy) / (m * sxx - sx * sx).max(1e-9);
        assert!(a < 0.0, "envelope decays (slope {a})");
        let measured_secs = 60.0 / (-a).max(1e-3);
        assert!(
            (measured_secs - 0.5).abs() / 0.5 < 0.15,
            "measured RT60 {:.0} ms vs 500 ms",
            measured_secs * 1000.0
        );
    }

    #[test]
    fn schroeder_tail_is_bounded_and_deterministic() {
        let mut tail = RoomLateField::new();
        tail.prepare(48_000);
        let room = Room {
            enabled: true,
            rt60_ms: 1500.0, // long tail — worst case for runaway feedback
            late_mix: 1.0,
            ..Default::default()
        };
        let mut send = vec![0.0f32; 480_000]; // 10 s of noise
        let mut seed = 0x1234_5678u32;
        for s in send.iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *s = ((seed >> 8) as f32 / 16_777_216.0 - 0.5) * 2.0;
        }
        let mut out = vec![0.0f32; send.len()];
        let n = tail.process(&room, &send, send.len(), &mut out);
        assert_eq!(n, send.len());
        assert!(out.iter().all(|v| v.is_finite()));
        let max_abs = out.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(max_abs < 4.0, "bounded tail output (max {max_abs})");
        // Deterministic: same input → identical output from a fresh tail.
        let mut tail2 = RoomLateField::new();
        tail2.prepare(48_000);
        let mut out2 = vec![0.0f32; send.len()];
        tail2.process(&room, &send, send.len(), &mut out2);
        assert!(
            out.iter().zip(out2.iter()).all(|(a, b)| a == b),
            "tail is deterministic"
        );
    }
}
