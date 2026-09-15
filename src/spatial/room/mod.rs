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
//! - **Late field** — an 8-line Feedback Delay Network (FDN) with coprime delay
//!   lines, an orthogonal Householder matrix, and frequency-dependent absorption
//!   damping shaped by the room's `rt60_ms`, driven by each object's
//!   `room_send`, whose output **encodes into the ambisonic bus** (§55) and
//!   is decoded onto every pan speaker with the field mixer's `√N` diffuse
//!   compensation and per-speaker decorrelation — the diffuse decay of the
//!   room, not a point source.
//!
//! ```text
//! object ──┬─ direct path (existing level chain + pan)
//!          ├─ early reflections: image i ── pan solve + coeff·dist_level
//!          │        ── delayed by (dist_i − direct)/c via per-object ring
//!          └─ room send ── 8-line FDN tail ── ambisonic W ── decode ── speakers
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
//! rings, smoothing matrix, tap lists, and the FDN buffers are
//! preallocated at `prepare`. The per-frame hot path is one ring store plus
//! one delayed read per active tap — bounded, lock-free, allocation-free.

mod early;
mod late;

#[cfg(test)]
mod tests;

pub use early::{EarlyReflections, ListenerImage};
pub use late::{RoomLateField, FDN_BASE_DELAYS, FDN_LINES};

use super::math::Vec3;
use crate::spatial::object::MAX_SPATIAL_OBJECTS;

/// Ceiling on distinct image sources per object (an order-2 box yields 24).
pub const MAX_IMAGES: usize = 32;

/// Early-reflection delay ring length in samples (≈ 171 ms @ 48 kHz).
/// Reflections whose excess delay exceeds it are clamped to the last ring
/// sample (documented bound — the ring is a fixed preallocation).
pub const MAX_ROOM_DELAY_SAMPLES: usize = 8192;

/// Per-object reflection-tap ceiling (`MAX_IMAGES` images × 4 pan writes).
const MAX_TAPS_PER_OBJECT: usize = MAX_IMAGES * 4;

/// Flat count of per-(object, image) reflection low-pass filters.
const MAX_REFLECTION_FILTERS: usize = MAX_SPATIAL_OBJECTS * MAX_IMAGES;

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
    /// Late-field RT60 (ms) — the FDN tail's decay time.
    pub rt60_ms: f32,
    /// Late-field wet mix `0..1` — scales the tail before the ambisonic
    /// decode.
    pub late_mix: f32,
    /// Late-field distance roll-off (item 3): when enabled, each
    /// object's room-send is attenuated by the object's own distance model
    /// at its direct distance — the tail then rolls off with source
    /// distance exactly as the direct and early-reflection paths do
    /// (acoustic agreement). Default `false` keeps the send (and
    /// therefore every legacy render) bit-identical.
    pub late_distance: bool,
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
            late_distance: false,
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
