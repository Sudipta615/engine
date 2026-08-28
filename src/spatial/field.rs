//! Diffuse fields — environmental spatial content (spec §13.3).
//!
//! Rain, wind, forest ambience, crowds, ocean: content that must **not** be
//! forced into a point-source model. A field is positionless; its mono input
//! is spread across every pan speaker with equal power (`1/√N` per speaker)
//! and decorrelated per speaker through a short deterministic delay line, so
//! it reads as surrounding ambience instead of a phantom image.
//!
//! This is the spec's *spatial mixer → speaker* path for fields (§37), the
//! natural completion of large object spread. Ambisonic field encoding is a
//! later phase (Part VI); this phase renders diffuse fields directly.
//!
//! ## Decorrelation (documented)
//!
//! Speaker `k` delays the mixed field by `2.0 + ((k·5) mod 12) · 0.75` ms
//! (2.0–10.25 ms, all distinct for up to 12 speakers — 5 is coprime with
//! 12). The pattern is deterministic, so renders are reproducible, and the
//! same delay applies to every field (all fields' contributions are summed
//! into the same per-speaker line before the delay — decorrelation is
//! between speakers, which is what diffuseness needs).
//!
//! ## Realtime discipline
//!
//! [`DiffuseFieldMixer::render`] is allocation-free: the ring buffers are
//! preallocated at `prepare` (per-speaker, sized to the maximum delay),
//! enabled-field planes are hoisted into a fixed stack array per block, and
//! per-sample work is one accumulate + one delayed read.

use super::scene::SpatialScene;
use super::speaker::SpeakerLayout;

/// Hard ceiling on fields per scene (render path stays bounded).
pub const MAX_FIELDS: usize = 16;

/// Stable handle to a field within a [`SpatialFieldStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldId(pub usize);

/// A diffuse spatial field (spec §13.3): rain, wind, ambience, crowd.
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialField {
    pub id: FieldId,
    /// Linear gain (1.0 = unit power before the equal-power spread).
    pub gain: f32,
    pub enabled: bool,
}

impl SpatialField {
    pub fn new(id: FieldId) -> Self {
        Self {
            id,
            gain: 1.0,
            enabled: true,
        }
    }
}

/// Bounded, stable-handle store for fields (same contract as the object and
/// bed stores).
#[derive(Debug, Clone)]
pub struct SpatialFieldStore {
    fields: Vec<Option<SpatialField>>,
}

impl Default for SpatialFieldStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SpatialFieldStore {
    pub fn new() -> Self {
        Self {
            fields: Vec::with_capacity(MAX_FIELDS),
        }
    }

    pub fn add(&mut self, mut field: SpatialField) -> Option<FieldId> {
        if let Some(slot) = self.fields.iter_mut().position(|f| f.is_none()) {
            field.id = FieldId(slot);
            self.fields[slot] = Some(field);
            return Some(FieldId(slot));
        }
        if self.fields.len() >= MAX_FIELDS {
            return None;
        }
        let id = FieldId(self.fields.len());
        field.id = id;
        self.fields.push(Some(field));
        Some(id)
    }

    pub fn get(&self, id: FieldId) -> Option<&SpatialField> {
        self.fields.get(id.0).and_then(|f| f.as_ref())
    }

    pub fn get_mut(&mut self, id: FieldId) -> Option<&mut SpatialField> {
        self.fields.get_mut(id.0).and_then(|f| f.as_mut())
    }

    pub fn remove(&mut self, id: FieldId) -> Option<SpatialField> {
        self.fields.get_mut(id.0).and_then(|slot| slot.take())
    }

    pub fn iter(&self) -> impl Iterator<Item = &SpatialField> {
        self.fields.iter().flatten()
    }

    /// Iterate enabled fields as `(store-slot, &field)`, in store order
    /// (render path; allocates nothing).
    pub fn iter_enabled(&self) -> impl Iterator<Item = (usize, &SpatialField)> + '_ {
        self.fields
            .iter()
            .enumerate()
            .filter_map(|(slot, f)| f.as_ref().map(|f| (slot, f)).filter(|(_, f)| f.enabled))
    }

    pub fn len(&self) -> usize {
        self.fields.iter().filter(|f| f.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Renderer-owned diffuse-field mixer: per-speaker decorrelation delay rings
/// (one shared ring buffer per pan speaker, written at a common cursor and
/// read at each speaker's own delay).
#[derive(Debug)]
pub struct DiffuseFieldMixer {
    /// Flat `speakers × delay_len` ring storage.
    delay: Vec<f32>,
    delay_len: usize,
    write_pos: usize,
    /// Per-speaker decorrelation delay in samples (distinct, deterministic).
    delay_samples: Vec<usize>,
    /// Pan (enabled, non-LFE) speaker output indices.
    speakers: Vec<usize>,
    /// Equal-power spread factor `1/√N` for the field's speakers.
    n_pan_inv: f32,
    sample_rate: f32,
    prepared: bool,
}

impl Default for DiffuseFieldMixer {
    fn default() -> Self {
        Self::new()
    }
}

impl DiffuseFieldMixer {
    pub fn new() -> Self {
        Self {
            delay: Vec::new(),
            delay_len: 1,
            write_pos: 0,
            delay_samples: Vec::new(),
            speakers: Vec::new(),
            n_pan_inv: 1.0,
            sample_rate: 48_000.0,
            prepared: false,
        }
    }

    /// Control-path setup: select the pan speakers (enabled, non-LFE), size
    /// the ring buffers to the maximum decorrelation delay, and assign each
    /// speaker its deterministic delay.
    pub fn prepare(&mut self, layout: &SpeakerLayout, sample_rate: u32) {
        self.speakers.clear();
        self.delay_samples.clear();
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                continue;
            }
            let k = self.speakers.len();
            // Deterministic, distinct delays: (k·5 mod 12) permutes 0..11.
            let delay_ms = 2.0 + ((k * 5) % 12) as f32 * 0.75;
            self.delay_samples
                .push(((delay_ms / 1000.0) * sample_rate as f32).round().max(1.0) as usize);
            self.speakers.push(idx);
        }
        self.sample_rate = sample_rate as f32;
        let n = self.speakers.len().max(1);
        self.n_pan_inv = 1.0 / (n as f32).sqrt();
        self.delay_len = self
            .delay_samples
            .iter()
            .copied()
            .max()
            .unwrap_or(1)
            .saturating_add(2);
        self.delay.clear();
        self.delay.resize(n * self.delay_len, 0.0);
        self.write_pos = 0;
        self.prepared = true;
    }

    /// Mix all enabled fields into `out` (interleaved `frames × speakers`).
    ///
    /// `field_inputs` is one plane per **enabled** field in store order.
    /// `out_trim` is the renderer's per-speaker calibration level. Allocation-
    /// free after `prepare`.
    pub fn render(
        &mut self,
        scene: &SpatialScene,
        field_inputs: &[&[f32]],
        frames: usize,
        out: &mut [f32],
        out_trim: &[f32],
    ) {
        if !self.prepared || frames == 0 {
            return;
        }
        let n = self.speakers.len();
        let n_spk = out.len().checked_div(frames).unwrap_or(0);
        if n == 0 || n_spk == 0 {
            return;
        }
        // Hoist enabled fields into a fixed stack array (bounded, no alloc).
        let mut planes: [(f32, &[f32]); MAX_FIELDS] = [(0.0, &[]); MAX_FIELDS];
        let mut n_planes = 0usize;
        for (ordinal, (_, field)) in scene.fields.iter_enabled().enumerate() {
            if let Some(input) = field_inputs.get(ordinal) {
                let g = field.gain * self.n_pan_inv;
                if g != 0.0 && !input.is_empty() && n_planes < MAX_FIELDS {
                    planes[n_planes] = (g, input);
                    n_planes += 1;
                }
            }
        }
        if n_planes == 0 {
            return;
        }
        let dl = self.delay_len;
        let mut w = self.write_pos;
        for f in 0..frames {
            // Accumulate all fields into this frame's write slot.
            let slot = w;
            for k in 0..n {
                self.delay[k * dl + slot] = 0.0;
            }
            for &(g, input) in planes[..n_planes].iter() {
                if f >= input.len() {
                    continue;
                }
                let s = input[f] * g;
                if s == 0.0 {
                    continue;
                }
                for k in 0..n {
                    self.delay[k * dl + slot] += s;
                }
            }
            // Read each speaker's decorrelated value into the output.
            for (k, &spk) in self.speakers.iter().enumerate() {
                let r = (slot + dl - self.delay_samples[k]) % dl;
                let v = self.delay[k * dl + r] * out_trim.get(spk).copied().unwrap_or(0.0);
                if v != 0.0 {
                    out[f * n_spk + spk] += v;
                }
            }
            w += 1;
            if w >= dl {
                w = 0;
            }
        }
        self.write_pos = w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::math::Vec3;
    use crate::spatial::scene::SpatialScene;

    #[test]
    fn store_is_bounded_and_stable() {
        let mut store = SpatialFieldStore::new();
        for i in 0..(MAX_FIELDS + 3) {
            let id = store.add(SpatialField::new(FieldId(i)));
            if i < MAX_FIELDS {
                assert!(id.is_some());
            } else {
                assert!(id.is_none());
            }
        }
        assert_eq!(store.len(), MAX_FIELDS);
    }

    #[test]
    fn delays_are_distinct_and_deterministic() {
        let layout = crate::spatial::speaker::SpeakerLayout::seven_point_one_four();
        let mut m = DiffuseFieldMixer::new();
        m.prepare(&layout, 48_000);
        assert_eq!(m.speakers.len(), 11, "7.1.4 → 11 pan speakers");
        let mut seen = std::collections::HashSet::new();
        for &d in &m.delay_samples {
            assert!(d > 0 && d < m.delay_len, "delay {d} in range");
            assert!(seen.insert(d), "delays distinct: {d}");
        }
        // LFE excluded: 7.1.4 output index 3 must not be a field speaker.
        assert!(!m.speakers.contains(&3));
    }

    #[test]
    fn impulse_field_spreads_equal_power_at_distinct_delays() {
        // An impulse field must deliver 1/√N to every pan speaker, each at
        // its own decorrelation delay — and nothing to the LFE.
        let layout = crate::spatial::speaker::SpeakerLayout::seven_point_one_four();
        let mut m = DiffuseFieldMixer::new();
        m.prepare(&layout, 48_000);
        let n = m.speakers.len();
        let per = 1.0 / (n as f32).sqrt();

        let mut scene = SpatialScene::new(48_000);
        scene.create_field();
        let frames = m.delay_len + 4;
        let input = vec![1.0f32]; // impulse at frame 0
        let refs: Vec<&[f32]> = vec![input.as_slice()];
        let mut out = vec![0.0f32; 12 * frames];
        let trim = vec![1.0f32; 12];
        m.render(&scene, &refs, frames, &mut out, &trim);

        for (k, &spk) in m.speakers.iter().enumerate() {
            let d = m.delay_samples[k];
            let got = out[d * 12 + spk];
            assert!(
                (got - per).abs() < 1e-4,
                "speaker {spk} impulse at frame {d} = {got}, want {per}"
            );
            // Nothing before the delay (ring warm-up is silent).
            assert!(out[(d - 1) * 12 + spk].abs() < 1e-6);
        }
        // LFE receives nothing.
        for f in 0..frames {
            assert!(out[f * 12 + 3].abs() < 1e-6, "LFE silent at frame {f}");
        }
        // Total impulse energy ≈ 1 (before trim).
        let e: f32 = m
            .speakers
            .iter()
            .map(|&spk| (0..frames).map(|f| out[f * 12 + spk]).sum::<f32>())
            .map(|s| s * s)
            .sum();
        assert!((e - 1.0).abs() < 1e-3, "field energy {e}");
    }

    #[test]
    fn field_gain_scales_energy() {
        let layout = crate::spatial::speaker::SpeakerLayout::five_point_one();
        let mut m = DiffuseFieldMixer::new();
        m.prepare(&layout, 48_000);
        let mut scene = SpatialScene::new(48_000);
        let id = scene.create_field().unwrap();
        scene.field_mut(id).unwrap().gain = 0.5;
        let frames = m.delay_len + 4;
        let input = vec![1.0f32];
        let refs: Vec<&[f32]> = vec![input.as_slice()];
        let mut out = vec![0.0f32; 6 * frames];
        let trim = vec![1.0f32; 6];
        m.render(&scene, &refs, frames, &mut out, &trim);
        let n = m.speakers.len();
        let per = 0.5 / (n as f32).sqrt();
        for (k, &spk) in m.speakers.iter().enumerate() {
            let got = out[m.delay_samples[k] * 6 + spk];
            assert!((got - per).abs() < 1e-4, "gain-scaled {got} vs {per}");
        }
        let _ = Vec3::ZERO;
    }
}
