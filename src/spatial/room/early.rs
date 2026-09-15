//! Early-reflection engine: delay rings, per-object tap lists, and reflection filtering.

use super::{
    image_sources, Room, MAX_IMAGES, MAX_REFLECTION_FILTERS, MAX_ROOM_DELAY_SAMPLES,
    MAX_TAPS_PER_OBJECT,
};
use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};
use crate::spatial::math::Vec3;
use crate::spatial::object::MAX_SPATIAL_OBJECTS;

/// A listener-relative image source ready for the renderer: direction
/// (world space, from the listener), distance, reflection coefficient,
/// the excess-path delay in samples (clamped to the ring), and the
/// reflection's low-pass corner in Hz (v3.47).
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
    /// Spectral low-pass corner imparted by the surface cascade (Hz).
    /// `f32::INFINITY` = spectrally flat (no filtering). A material's
    /// per-band spectrum or a diffraction/transmission corner is collapsed
    /// to this corner so the realtime renderers colour the reflection the
    /// way the offline [`crate::spatial::acoustic::bake`] pipeline does,
    /// without carrying the full FIR on the audio thread.
    pub lowpass_hz: f32,
}

impl ListenerImage {
    pub const ZERO: Self = Self {
        dir: Vec3::ZERO,
        dist: 0.0,
        coeff: 0.0,
        delay: 0,
        lowpass_hz: f32::INFINITY,
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

    // v3.47: per-(object, image) reflection spectral low-pass. A material's
    // per-band spectrum (or a diffraction corner) collapses to a corner here
    // so a reflection's delayed ring read is coloured by a one-pole low-
    // pass — the realtime IIR realization of the same spectral model the
    // offline `Acoustic` node renders exactly with a minimum-phase FIR.
    // When no corner is set (`lowpass_hz = ∞`) the filter is a strict
    // passthrough — the live-solve path, and therefore every pre-v3.47
    // outcome, is bit-identical.
    /// Target low-pass corner (Hz) per (object, image); ∞ = flat.
    ref_cut: Vec<f32>,
    /// Smoothed log-corner (`0.0` = uninitialised first block).
    ref_cut_log: Vec<f32>,
    /// Whether the (object, image) filter is active this block.
    ref_active: Vec<bool>,
    /// Per-(object, image) one-pole biquad filter states.
    ref_state: Vec<BiquadStateF32>,
    /// Current block's per-(object, image) low-pass coefficients.
    ref_coeffs: Vec<BiquadCoeffsF32>,

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
            ref_cut: vec![f32::INFINITY; MAX_REFLECTION_FILTERS],
            ref_cut_log: vec![0.0; MAX_REFLECTION_FILTERS],
            ref_active: vec![false; MAX_REFLECTION_FILTERS],
            ref_state: vec![BiquadStateF32::default(); MAX_REFLECTION_FILTERS],
            ref_coeffs: vec![BiquadCoeffsF32::default(); MAX_REFLECTION_FILTERS],
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
        self.ref_cut.fill(f32::INFINITY);
        self.ref_cut_log.fill(0.0);
        self.ref_active.fill(false);
        self.ref_state.fill(BiquadStateF32::default());
        self.ref_coeffs.fill(BiquadCoeffsF32::default());
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
        let mut raw = [super::ReflectionImage::ZERO; MAX_IMAGES];
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
                // The live scalar `Room` carries no per-wall spectral data,
                // so live-solve reflections are spectrally flat (v3.47). The
                // baked path supplies a corner via its per-band spectra.
                lowpass_hz: f32::INFINITY,
            };
            count += 1;
        }
        count
    }

    /// Zero the tap list for one object (start of its block work).
    pub fn begin_object(&mut self, obj_slot: usize) {
        self.tap_len[obj_slot] = 0;
    }

    /// Set one (object, image) reflection's spectral low-pass corner (Hz) at
    /// block rate and refresh its one-pole coefficients. `∞` / a non-finite
    /// corner disables the filter and resets its state to a strict
    /// passthrough, so the live scalar-`Room` solve (which never supplies a
    /// corner) stays bit-identical. The corner is one-pole smoothed across
    /// blocks against zipper; the first block snaps to target.
    pub fn set_reflection_filter(&mut self, obj_slot: usize, img: usize, lowpass_hz: f32) {
        if obj_slot >= MAX_SPATIAL_OBJECTS || img >= MAX_IMAGES {
            return;
        }
        let idx = obj_slot * MAX_IMAGES + img;
        self.ref_cut[idx] = lowpass_hz;
        let active = lowpass_hz.is_finite() && lowpass_hz > 1.0;
        self.ref_active[idx] = active;
        if !active {
            self.ref_state[idx] = BiquadStateF32::default();
            return;
        }
        let nyq = (self.sample_rate * 0.5).max(20.0);
        let target = lowpass_hz.clamp(20.0, nyq).ln();
        if self.ref_cut_log[idx] <= 0.0 {
            self.ref_cut_log[idx] = target;
        } else if self.smooth < 1.0 {
            self.ref_cut_log[idx] += self.smooth * (target - self.ref_cut_log[idx]);
        }
        let cutoff = self.ref_cut_log[idx].exp();
        self.ref_coeffs[idx] = BiquadCoeffsF32::lowpass(self.sample_rate, cutoff, 0.707);
    }

    /// Filter one delayed reflection sample through the (object, image)
    /// low-pass (binaural mode, which reads the ring directly rather than
    /// through `object_frame`). Strict passthrough when no corner is set.
    pub fn filter_reflection(&mut self, obj_slot: usize, img: usize, sample: f32) -> f32 {
        let idx = obj_slot * MAX_IMAGES + img;
        if self.ref_active[idx] {
            self.ref_state[idx].process(sample, &self.ref_coeffs[idx])
        } else {
            sample
        }
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
            let mut x = self.ring[row + r];
            // v3.47: colour the reflection with its per-image spectral
            // low-pass when a corner is set; otherwise strict passthrough.
            let fidx = obj_slot * MAX_IMAGES + img;
            if self.ref_active[fidx] {
                x = self.ref_state[fidx].process(x, &self.ref_coeffs[fidx]);
            }
            let gain = self.rsm[obj_slot * (MAX_IMAGES * n_spk) + img * n_spk + spk]
                * out_trim.get(spk).copied().unwrap_or(0.0);
            if gain != 0.0 {
                out[frame * n_spk + spk] += x * gain;
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

    /// The ring cursor for `frame` of the current block (binaural mode;
    /// see the renderer's module docs).
    pub fn cursor_at(&self, frame: usize) -> usize {
        (self.block_write + frame) % self.ring_len
    }

    /// Store one sample in an object's ring at an explicit cursor (binaural
    /// mode).
    pub fn store(&mut self, obj_slot: usize, cursor: usize, sample: f32) {
        self.ring[obj_slot * self.ring_len + cursor] = sample;
    }

    /// Fractional (linearly interpolated) delayed read from an object's ring
    /// (binaural mode). `delay_samples` is clamped to `[0, ring_len − 1]` so
    /// any delay (room excess path + ITD) stays in bounds; the interpolation
    /// keeps moving sources' ITDs continuous.
    pub fn read_delayed(&self, obj_slot: usize, cursor: usize, delay_samples: f32) -> f32 {
        let rl = self.ring_len;
        let d = delay_samples.clamp(0.0, (rl - 1) as f32);
        let i = d.floor() as usize;
        let f = d - i as f32;
        let row = obj_slot * rl;
        let a = self.ring[row + (cursor + rl - i) % rl];
        let b = self.ring[row + (cursor + rl - i - 1) % rl];
        a + f * (b - a)
    }

    /// Accumulate one sample into the room-send plane (binaural mode).
    pub fn add_send(&mut self, frame: usize, v: f32) {
        self.send[frame] += v;
    }

    /// The accumulated room-send plane (one sample per frame of the block).
    pub fn send(&self) -> &[f32] {
        &self.send
    }
}
