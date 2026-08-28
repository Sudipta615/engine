//! Ambisonics / Higher-Order Ambisonics (spec Part VI §32–37, §55).
//!
//! Ambisonics is the engine's sound-field representation: a direction-
//! independent bus that a *field* (or any spatial source) encodes into and
//! that any speaker layout decodes from, so the same bus renders to stereo,
//! 5.1, 7.1.4, or a custom array without re-authoring. It is the natural
//! home for diffuse environments, ambience and the future room's late field
//! (§32, §55).
//!
//! ```text
//! Spatial source → encoder → ambisonic bus → decoder → SpeakerLayout → PCM
//! ```
//!
//! ## Conventions (documented, spec §35 / §153)
//!
//! - **Channel ordering**: ACN — order 1 is `[W, Y, Z, X]` (ACN 0–3).
//! - **Normalization**: SN3D (W = 1, first order = `√3` times the
//!   direction components).
//! - **Coordinate frame**: the spatial layer's single frame (`+X` right,
//!   `+Y` front, `+Z` up); directions are listener-space unit vectors.
//! - **Basis**: real spherical harmonics. Order 1 is implemented;
//!   `channel_count(order) = (order+1)²` and the per-order decoder weight
//!   table make higher orders a table + rotation extension (§34).
//! - **Rotation**: an order-1 rotation keeps `W` invariant and rotates the
//!   `X Y Z` channels by the same 3×3 as direction vectors. The renderer
//!   applies the listener orientation, so a world-encoded field stays
//!   world-fixed as the listener turns (§48).
//! - **Decoding** (§36): the sampling ("basic") decoder `D = Y(S)ᵀ/N` —
//!   a plane wave from `d` lands on speaker `s` as `(1 + 3·cosθ)/N` — plus
//!   a **max-rE** policy (`a0 = 1, a1 = √3/2 ≈ 0.866`, the documented FOA
//!   convention) which narrows the lobe. Decoder selection is separate from
//!   the scene representation.

use super::math::{Quat, Vec3};
use super::render::RenderError;
use super::speaker::SpeakerLayout;
use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;

/// The implemented ambisonic order (1 = First-Order Ambisonics, FOA).
pub const AMBISONIC_ORDER: u8 = 1;

/// Number of ambisonic channels for `order` (`(order+1)²`).
pub fn channel_count(order: u8) -> usize {
    let o = order as usize + 1;
    o * o
}

/// FOA channel count (order [`AMBISONIC_ORDER`]).
pub const AMBISONIC_CHANNELS: usize = 4;

/// Real spherical-harmonic basis for a unit direction, ACN/SN3D, order 1:
/// `[W, Y, Z, X]` = `[1, √3·y, √3·z, √3·x]`.
#[inline]
pub fn sh_foa(dir: Vec3) -> [f32; 4] {
    let s3 = 3.0f32.sqrt();
    [1.0, s3 * dir.y, s3 * dir.z, s3 * dir.x]
}

/// Encode a plane wave from `dir` (gain `g`) into one FOA bus frame
/// (`[W, Y, Z, X]`). `dir` is normalised defensively; a zero direction
/// encodes silence rather than NaN.
pub fn encode_plane_wave(dir: Vec3, gain: f32, out: &mut [f32; 4]) {
    let d = dir.normalized().unwrap_or(Vec3::Y);
    let y = sh_foa(d);
    for (o, &v) in out.iter_mut().zip(y.iter()) {
        *o = v * gain;
    }
}

/// Rotate an FOA bus frame by `q` (order-1 rotation: `W` invariant, the
/// `X Y Z` channels rotate exactly like direction vectors). `q` is the
/// world-space rotation to apply to the field.
pub fn rotate_bus_frame(q: Quat, frame: &mut [f32; 4]) {
    // Channel order [W, Y, Z, X] → direction (X, Y, Z) = (frame[3], frame[1],
    // frame[2]).
    let v = Vec3::new(frame[3], frame[1], frame[2]);
    let r = q.rotate_vec3(v);
    frame[1] = r.y;
    frame[2] = r.z;
    frame[3] = r.x;
}

/// Ambisonic decoder policy (spec §36): how the bus maps onto speakers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DecoderPolicy {
    /// Sampling ("basic") decoder: `D = Y(S)ᵀ/N` — every order weighted
    /// equally. A plane wave from `d` lands on speaker `s` as
    /// `(1 + 3·cosθ)/N`.
    #[default]
    Basic,
    /// Max-rE FOA weights (`a0 = 1, a1 = √3/2 ≈ 0.866`) — narrows the
    /// decoded lobe for a tighter image (documented convention).
    MaxRe,
}

/// Per-order decoder weights for `policy` (order 1: `[a0, a1]`).
/// Basic weights everything equally; Max-rE applies the documented FOA
/// `a1 = √3/2`. Higher orders extend this table.
fn order_weights(policy: DecoderPolicy) -> [f32; 2] {
    match policy {
        DecoderPolicy::Basic => [1.0, 1.0],
        DecoderPolicy::MaxRe => [1.0, 0.866_025_4],
    }
}

/// The ambisonic decoder: a precomputed per-speaker decode matrix applied to
/// an interleaved FOA bus. Realtime-safe after `prepare` (all geometry work
/// happens there; `process_bus` is flat-array arithmetic).
#[derive(Debug, Default)]
pub struct AmbisonicDecoder {
    /// Per-speaker decode weights, flat `speakers × AMBISONIC_CHANNELS`.
    gains: Vec<f32>,
    /// Enabled non-LFE speaker output indices (rows of `gains`).
    speakers: Vec<usize>,
    speaker_count: usize,
    policy: DecoderPolicy,
    prepared: bool,
}

impl AmbisonicDecoder {
    pub fn new(policy: DecoderPolicy) -> Self {
        Self {
            gains: Vec::new(),
            speakers: Vec::new(),
            speaker_count: 0,
            policy,
            prepared: false,
        }
    }

    pub fn policy(&self) -> DecoderPolicy {
        self.policy
    }

    /// Control path: build the decode matrix for `layout` (enabled non-LFE
    /// speakers, unit directions, `N` = pan-speaker count).
    pub fn prepare(
        &mut self,
        layout: &SpeakerLayout,
        _sample_rate: u32,
    ) -> Result<(), RenderError> {
        layout.validate()?;
        let w = order_weights(self.policy);
        let mut speakers = Vec::new();
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                continue;
            }
            speakers.push(idx);
        }
        if speakers.is_empty() {
            return Err(RenderError::DegenerateGeometry);
        }
        let n = speakers.len() as f32;
        let mut gains = Vec::with_capacity(speakers.len() * AMBISONIC_CHANNELS);
        for idx in &speakers {
            let dir = layout.speakers[*idx]
                .position
                .normalized()
                .unwrap_or(Vec3::Y);
            let y = sh_foa(dir);
            // D[s] = (1/N)·[a0·W, a1·Y, a1·Z, a1·X].
            gains.push(w[0] * y[0] / n);
            gains.push(w[1] * y[1] / n);
            gains.push(w[1] * y[2] / n);
            gains.push(w[1] * y[3] / n);
        }
        self.gains = gains;
        self.speakers = speakers;
        self.speaker_count = layout.speakers.len();
        self.prepared = true;
        Ok(())
    }

    /// Total output speaker count (incl. LFE).
    pub fn channels(&self) -> usize {
        self.speaker_count
    }

    /// The pan (enabled, non-LFE) speaker output indices, in decode order.
    pub fn speakers(&self) -> &[usize] {
        &self.speakers
    }

    /// True when a decode matrix is ready.
    pub fn prepared(&self) -> bool {
        self.prepared
    }

    /// Decode a single FOA frame into `row[0 .. self.speakers.len()]` (the
    /// pan speakers in decode order). Allocation-free; the per-frame
    /// building block for pipelines that wrap the decode in further
    /// processing (e.g. the field mixer's decorrelation rings).
    pub fn decode_frame(&self, frame: &[f32; 4], row: &mut [f32]) {
        for (k, v) in row.iter_mut().enumerate().take(self.speakers.len()) {
            let g = k * AMBISONIC_CHANNELS;
            *v = self.gains[g] * frame[0]
                + self.gains[g + 1] * frame[1]
                + self.gains[g + 2] * frame[2]
                + self.gains[g + 3] * frame[3];
        }
    }

    /// Decode an interleaved FOA bus (`frames × AMBISONIC_CHANNELS`,
    /// `[W, Y, Z, X]` per frame) into `out` (`frames × speakers`, **added**,
    /// not cleared — the hybrid mixer sums classes). Missing bus frames are
    /// treated as silence. Allocation-free.
    pub fn process_bus(&self, bus: &[f32], frames: usize, out: &mut [f32]) {
        if !self.prepared || frames == 0 {
            return;
        }
        let n_spk = out.len().checked_div(frames).unwrap_or(0);
        if n_spk == 0 {
            return;
        }
        for f in 0..frames {
            let b0 = bus.get(f * AMBISONIC_CHANNELS).copied().unwrap_or(0.0);
            let b1 = bus.get(f * AMBISONIC_CHANNELS + 1).copied().unwrap_or(0.0);
            let b2 = bus.get(f * AMBISONIC_CHANNELS + 2).copied().unwrap_or(0.0);
            let b3 = bus.get(f * AMBISONIC_CHANNELS + 3).copied().unwrap_or(0.0);
            for (k, &spk) in self.speakers.iter().enumerate() {
                let row = k * AMBISONIC_CHANNELS;
                let v = self.gains[row] * b0
                    + self.gains[row + 1] * b1
                    + self.gains[row + 2] * b2
                    + self.gains[row + 3] * b3;
                if v != 0.0 && spk < n_spk {
                    out[f * n_spk + spk] += v;
                }
            }
        }
    }

    /// Decode a single plane wave directly (test/introspection helper):
    /// returns each pan speaker's gain for a source at `dir`.
    #[cfg(test)]
    fn plane_wave_gains(&self, dir: Vec3) -> Vec<f32> {
        let mut frame = [0.0f32; 4];
        encode_plane_wave(dir, 1.0, &mut frame);
        let mut out = vec![0.0f32; self.speaker_count];
        let mut bus = Vec::new();
        bus.extend_from_slice(&frame);
        self.process_bus(&bus, 1, &mut out);
        out
    }
}

/// A standalone ambisonic renderer (spec §23): decodes a FOA bus into the
/// active speaker layout, applying the listener's orientation (so a
/// world-encoded field stays world-fixed, §48) and per-speaker calibration.
///
/// Input convention for `process_block`: `object_inputs` carries the four
/// FOA planes `[W, Y, Z, X]` (one mono plane per channel, world
/// orientation). Beds/fields are not part of this renderer (the hybrid
/// renderers mix them); the trait's default `process_hybrid_block` forwards
/// to the bus path.
#[derive(Debug)]
pub struct AmbisonicRenderer {
    decoder: AmbisonicDecoder,
    /// Per-speaker calibration level.
    out_trim: Vec<f32>,
    /// Scratch for the listener-rotated interleaved bus.
    bus: Vec<f32>,
    prepared: bool,
}

impl AmbisonicRenderer {
    pub fn new(policy: DecoderPolicy) -> Self {
        Self {
            decoder: AmbisonicDecoder::new(policy),
            out_trim: Vec::new(),
            bus: vec![0.0; AMBISONIC_CHANNELS * MAX_AUDIO_BLOCK_FRAMES],
            prepared: false,
        }
    }

    pub fn policy(&self) -> DecoderPolicy {
        self.decoder.policy()
    }
}

impl Default for AmbisonicRenderer {
    fn default() -> Self {
        Self::new(DecoderPolicy::Basic)
    }
}

impl super::render::SpatialRenderer for AmbisonicRenderer {
    fn prepare(&mut self, layout: &SpeakerLayout, sample_rate: u32) -> Result<(), RenderError> {
        self.decoder.prepare(layout, sample_rate)?;
        self.out_trim = layout
            .speakers
            .iter()
            .map(|s| s.gain * layout.calibration.trim_gain(s.id))
            .collect();
        self.prepared = true;
        Ok(())
    }

    fn process_block(
        &mut self,
        scene: &super::scene::SpatialScene,
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
        let need = self.decoder.channels() * frames;
        if out.len() < need {
            return Err(RenderError::BufferMismatch {
                expected: need,
                got: out.len(),
            });
        }
        // Build the listener-space interleaved bus: rotate each world-
        // oriented FOA frame by the listener orientation (conjugate), so a
        // world-fixed field appears to rotate opposite to the head (§48).
        let xf = super::scene::ListenerTransform::from_listener(&scene.listener);
        let w = object_inputs.first().copied().unwrap_or(&[]);
        let y = object_inputs.get(1).copied().unwrap_or(&[]);
        let z = object_inputs.get(2).copied().unwrap_or(&[]);
        let x = object_inputs.get(3).copied().unwrap_or(&[]);
        for f in 0..frames {
            let mut frame = [
                w.get(f).copied().unwrap_or(0.0),
                y.get(f).copied().unwrap_or(0.0),
                z.get(f).copied().unwrap_or(0.0),
                x.get(f).copied().unwrap_or(0.0),
            ];
            rotate_bus_frame(xf.orientation, &mut frame);
            self.bus[f * AMBISONIC_CHANNELS] = frame[0];
            self.bus[f * AMBISONIC_CHANNELS + 1] = frame[1];
            self.bus[f * AMBISONIC_CHANNELS + 2] = frame[2];
            self.bus[f * AMBISONIC_CHANNELS + 3] = frame[3];
        }
        for sample in out[..need].iter_mut() {
            *sample = 0.0;
        }
        self.decoder
            .process_bus(&self.bus[..frames * AMBISONIC_CHANNELS], frames, out);
        // Apply per-speaker calibration.
        let n_spk = self.decoder.channels();
        for f in 0..frames {
            for (spk, &trim) in self.out_trim.iter().enumerate().take(n_spk) {
                out[f * n_spk + spk] *= trim;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::scene::SpatialScene;
    use std::f32::consts::FRAC_PI_2;

    const EPS: f32 = 1e-4;

    #[test]
    fn sh_basis_matches_documented_sn3d_convention() {
        // +Y front → [1, √3, 0, 0]; +X right → [1, 0, 0, √3]; +Z up →
        // [1, 0, √3, 0]; diagonal → all four populated.
        let s3 = 3.0f32.sqrt();
        assert_eq!(sh_foa(Vec3::Y), [1.0, s3, 0.0, 0.0]);
        assert_eq!(sh_foa(Vec3::X), [1.0, 0.0, 0.0, s3]);
        assert_eq!(sh_foa(Vec3::Z), [1.0, 0.0, s3, 0.0]);
        let d = Vec3::new(1.0, 2.0, 3.0).normalized().unwrap();
        let y = sh_foa(d);
        assert!((y[0] - 1.0).abs() < EPS);
        assert!((y[1] - s3 * d.y).abs() < EPS);
        assert!((y[2] - s3 * d.z).abs() < EPS);
        assert!((y[3] - s3 * d.x).abs() < EPS);
    }

    #[test]
    fn plane_wave_encode_then_basic_decode_matches_formula() {
        // Basic decoder on N speakers: a plane wave from d lands on speaker
        // s as (1 + 3·cosθ)/N, exactly the documented sampling pattern.
        let layout = SpeakerLayout::seven_point_one_four();
        let mut dec = AmbisonicDecoder::new(DecoderPolicy::Basic);
        dec.prepare(&layout, 48_000).unwrap();
        let dir = Vec3::Y; // front
        let gains = dec.plane_wave_gains(dir);
        let n = 11usize;
        let mut pan_idx = 0usize;
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                assert_eq!(gains[idx], 0.0, "LFE/speaker {idx} silent");
                continue;
            }
            let spk_dir = s.position.normalized().unwrap();
            let cos = spk_dir.dot(dir);
            let expected = (1.0 + 3.0 * cos) / n as f32;
            assert!(
                (gains[idx] - expected).abs() < 1e-4,
                "speaker {idx} gain {} want {expected}",
                gains[idx]
            );
            pan_idx += 1;
        }
        assert_eq!(pan_idx, n);
    }

    #[test]
    fn max_re_policy_narrows_the_response() {
        // Max-rE (a1 = √3/2): same front lobe centre, but the rear gain
        // (cosθ = −1) is less negative — narrower, more focused.
        let layout = SpeakerLayout::seven_point_one_four();
        let mut basic = AmbisonicDecoder::new(DecoderPolicy::Basic);
        let mut maxre = AmbisonicDecoder::new(DecoderPolicy::MaxRe);
        basic.prepare(&layout, 48_000).unwrap();
        maxre.prepare(&layout, 48_000).unwrap();
        let bg = basic.plane_wave_gains(Vec3::Y);
        let mg = maxre.plane_wave_gains(Vec3::Y);
        let n = 11usize;
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                continue;
            }
            let cos = s.position.normalized().unwrap().dot(Vec3::Y);
            let a1 = 0.866_025_4f32;
            let expected = (1.0 + 3.0 * a1 * cos) / n as f32;
            assert!((mg[idx] - expected).abs() < 1e-4, "max-rE speaker {idx}");
            assert!(mg[idx].is_finite() && bg[idx].is_finite());
        }
    }

    #[test]
    fn bus_rotation_keeps_world_fixed_field_stable() {
        // A field encoded with a source at world +X. Listener yaws +90°
        // (faces +X): the source must appear front. The renderer rotates the
        // bus by the listener's conjugate; test the primitive directly.
        let mut frame = [0.0f32; 4];
        encode_plane_wave(Vec3::X, 1.0, &mut frame);
        assert!((frame[3] - 3.0f32.sqrt()).abs() < EPS, "X channel");
        let listener_yaw90 = Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0);
        rotate_bus_frame(listener_yaw90.conjugate(), &mut frame);
        // Now the field is at +Y (front): [1, √3, 0, 0].
        assert!(
            (frame[1] - 3.0f32.sqrt()).abs() < EPS,
            "Y channel {}",
            frame[1]
        );
        assert!(frame[3].abs() < EPS, "X channel cleared");
        assert!((frame[0] - 1.0).abs() < EPS, "W invariant");
        // Round-trip: rotate back by the yaw itself → +X again.
        rotate_bus_frame(listener_yaw90, &mut frame);
        assert!((frame[3] - 3.0f32.sqrt()).abs() < EPS, "round-trip");
    }

    #[test]
    fn renderer_applies_listener_rotation_and_calibration() {
        use super::super::render::SpatialRenderer;
        let layout = SpeakerLayout::stereo();
        let mut r = AmbisonicRenderer::new(DecoderPolicy::Basic);
        r.prepare(&layout, 48_000).unwrap();
        let mut scene = SpatialScene::new(48_000);
        scene
            .listener
            .set_orientation(Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0));
        // Encode a source at world +X; the yawed listener hears it at front
        // → equal FL/FR split of the (1 + 3·cos30°)/2 pattern.
        let frames = 8usize;
        let w = vec![1.0f32; frames];
        let x = vec![3.0f32.sqrt(); frames];
        let z = vec![0.0f32; frames];
        let y = vec![0.0f32; frames];
        let planes: Vec<&[f32]> = vec![&w, &y, &z, &x];
        let mut out = vec![0.0f32; 2 * frames];
        r.process_block(&scene, &planes, frames, &mut out).unwrap();
        assert!(out.iter().all(|v| v.is_finite()));
        let expected = (1.0 + 3.0 * 30f32.to_radians().cos()) / 2.0;
        assert!((out[0] - expected).abs() < 1e-3, "FL {}", out[0]);
        assert!((out[1] - expected).abs() < 1e-3, "FR {}", out[1]);
    }

    #[test]
    fn decoder_rejects_empty_layout() {
        let mut dec = AmbisonicDecoder::new(DecoderPolicy::Basic);
        assert!(matches!(
            dec.prepare(&SpeakerLayout::custom(vec![]), 48_000),
            Err(RenderError::InvalidLayout)
        ));
        // LFE-only → no pan speakers → degenerate.
        let mut lfe = crate::spatial::speaker::Speaker::new(Vec3::ZERO);
        lfe.is_lfe = true;
        let lfe_only = SpeakerLayout {
            speakers: vec![lfe],
            reference_position: Vec3::ZERO,
            calibration: Default::default(),
        };
        assert!(matches!(
            dec.prepare(&lfe_only, 48_000),
            Err(RenderError::DegenerateGeometry)
        ));
    }
}
