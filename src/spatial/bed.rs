//! Beds — channel-based spatial content (spec §13.1).
//!
//! A bed is content that already has a spatial structure: 5.1 music, a 7.1
//! effects bed, multichannel ambience. Beds are **not** routed through the
//! object pan solvers (VBAP/equal-power) — each authored channel is placed
//! by its *semantic role* ([`ChannelId`]) onto the matching output speaker,
//! so a 5.1 bed renders correctly on any layout that contains its roles
//! without re-authoring. Beds are not inherently inferior to objects; they
//! are the right representation when content is already a coherent spatial
//! field (§13.1).
//!
//! ## Mapping policy (documented)
//!
//! - A bed channel whose role exists on the output layout routes to that
//!   speaker (calibration trim applied, LFE included — bed LFE is authored
//!   content for the LFE path).
//! - A bed channel with no matching output speaker is **dropped**. Full
//!   BS.775 rematrixing between arbitrary layouts remains the conventional
//!   PCM path's job; the spatial bed path routes by identity.
//!
//! ## Realtime discipline
//!
//! [`render_beds`] never allocates: the bed's authored channel roles are
//! cached in the bed at construction (control path), and speaker lookups are
//! a linear scan of the small role table built by the renderer at `prepare`.

use super::scene::SpatialScene;
use crate::decode::ChannelId;
use crate::decode::ChannelLayout;

/// Hard ceiling on beds per scene (render path stays bounded, spec §75–76).
pub const MAX_BEDS: usize = 16;

/// Stable handle to a bed within a [`SpatialBedStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BedId(pub usize);

/// A channel-based spatial bed (spec §13.1).
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialBed {
    pub id: BedId,
    /// The bed's authored layout (introspection). Channel `i` of the bed
    /// carries role [`Self::channels`]`[i]`.
    pub layout: ChannelLayout,
    /// Linear gain applied to every channel of the bed.
    pub gain: f32,
    pub enabled: bool,
    /// Authored channel roles, cached at construction so the render path
    /// never re-derives (and never allocates) the semantic order.
    channels: Vec<ChannelId>,
}

impl SpatialBed {
    /// Build a bed for `layout`. `channels` caches `layout.channel_ids()`
    /// once, on the control path.
    pub fn new(id: BedId, layout: ChannelLayout) -> Self {
        let channels = layout.channel_ids();
        Self {
            id,
            layout,
            gain: 1.0,
            enabled: true,
            channels,
        }
    }

    /// The authored channel roles in order (one per bed input plane).
    pub fn channels(&self) -> &[ChannelId] {
        &self.channels
    }

    /// Re-target the bed onto a new layout (control path; rebuilds the
    /// cached role list).
    pub fn set_layout(&mut self, layout: ChannelLayout) {
        self.channels = layout.channel_ids();
        self.layout = layout;
    }
}

/// Bounded, stable-handle store for beds (same contract as the object
/// store: ids are slot indices and stay reserved after removal).
#[derive(Debug, Clone)]
pub struct SpatialBedStore {
    beds: Vec<Option<SpatialBed>>,
}

impl Default for SpatialBedStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SpatialBedStore {
    pub fn new() -> Self {
        Self {
            beds: Vec::with_capacity(MAX_BEDS),
        }
    }

    /// Add a bed, returning its stable [`BedId`]; `None` at capacity.
    pub fn add(&mut self, mut bed: SpatialBed) -> Option<BedId> {
        if let Some(slot) = self.beds.iter_mut().position(|b| b.is_none()) {
            bed.id = BedId(slot);
            self.beds[slot] = Some(bed);
            return Some(BedId(slot));
        }
        if self.beds.len() >= MAX_BEDS {
            return None;
        }
        let id = BedId(self.beds.len());
        bed.id = id;
        self.beds.push(Some(bed));
        Some(id)
    }

    pub fn get(&self, id: BedId) -> Option<&SpatialBed> {
        self.beds.get(id.0).and_then(|b| b.as_ref())
    }

    pub fn get_mut(&mut self, id: BedId) -> Option<&mut SpatialBed> {
        self.beds.get_mut(id.0).and_then(|b| b.as_mut())
    }

    pub fn remove(&mut self, id: BedId) -> Option<SpatialBed> {
        self.beds.get_mut(id.0).and_then(|slot| slot.take())
    }

    pub fn iter(&self) -> impl Iterator<Item = &SpatialBed> {
        self.beds.iter().flatten()
    }

    /// Iterate enabled beds as `(store-slot, &bed)`, in store order
    /// (render path; allocates nothing).
    pub fn iter_enabled(&self) -> impl Iterator<Item = (usize, &SpatialBed)> + '_ {
        self.beds
            .iter()
            .enumerate()
            .filter_map(|(slot, b)| b.as_ref().map(|b| (slot, b)).filter(|(_, b)| b.enabled))
    }

    pub fn len(&self) -> usize {
        self.beds.iter().filter(|b| b.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Mix enabled beds into `out` (interleaved `frames × speakers`).
///
/// - `bed_inputs`: one plane per **enabled** bed in store order, bed-major —
///   plane `ordinal * bed_channels + c` is enabled bed `ordinal`'s channel
///   `c`. Missing planes are skipped.
/// - `roles`: the output layout's role table — `(speaker_index, role)` for
///   every speaker that carries a semantic role (built by the renderer at
///   `prepare`).
/// - `out_trim`: per-speaker calibration level (as applied to objects).
///
/// Allocation-free. Bed gains are applied directly (no per-path smoothing;
/// beds are authored, static content).
pub fn render_beds(
    scene: &SpatialScene,
    bed_inputs: &[&[f32]],
    frames: usize,
    out: &mut [f32],
    roles: &[(usize, ChannelId)],
    out_trim: &[f32],
) {
    let speakers = out.len().checked_div(frames).unwrap_or(0);
    if speakers == 0 {
        return;
    }
    for (ordinal, (_, bed)) in scene.beds.iter_enabled().enumerate() {
        let ch = bed.channels();
        let n_ch = ch.len();
        let base = ordinal * n_ch;
        for (c, role) in ch.iter().enumerate() {
            // Find the output speaker carrying this role.
            let Some(&(spk, _)) = roles.iter().find(|(_, r)| r == role) else {
                continue; // no matching speaker on this layout → dropped
            };
            let Some(input) = bed_inputs.get(base + c) else {
                continue;
            };
            let g = bed.gain * out_trim.get(spk).copied().unwrap_or(0.0);
            if g == 0.0 {
                continue;
            }
            let len = input.len().min(frames);
            for f in 0..len {
                out[f * speakers + spk] += input[f] * g;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::math::Vec3;
    use crate::spatial::scene::SpatialScene;
    use crate::spatial::speaker::SpeakerLayout;

    fn scene_with_5_1_bed() -> (SpatialScene, BedId) {
        let mut scene = SpatialScene::new(48_000);
        let id = scene.create_bed(ChannelLayout::FivePointOne).expect("bed");
        (scene, id)
    }

    #[test]
    fn store_is_bounded_and_ids_stable() {
        let mut store = SpatialBedStore::new();
        for i in 0..(MAX_BEDS + 3) {
            let id = store.add(SpatialBed::new(BedId(i), ChannelLayout::Stereo));
            if i < MAX_BEDS {
                assert!(id.is_some());
            } else {
                assert!(id.is_none(), "store must stay bounded");
            }
        }
        assert_eq!(store.len(), MAX_BEDS);
        // Remove keeps the slot reserved; add reuses it.
        let mut store = SpatialBedStore::new();
        let a = store
            .add(SpatialBed::new(BedId(0), ChannelLayout::Stereo))
            .unwrap();
        store.remove(a);
        assert!(store.get(a).is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn channels_cache_matches_layout() {
        let bed = SpatialBed::new(BedId(0), ChannelLayout::FivePointOne);
        assert_eq!(bed.channels().len(), 6);
        assert_eq!(bed.channels()[2], ChannelId::Center);
        assert_eq!(bed.channels()[3], ChannelId::Lfe);
        let bed = SpatialBed::new(
            BedId(0),
            ChannelLayout::Custom(vec![ChannelId::FrontLeft, ChannelId::FrontRight]),
        );
        assert_eq!(bed.channels().len(), 2);
    }

    #[test]
    fn render_routes_5_1_bed_onto_5_1_output() {
        // Each bed channel carries a distinct unit impulse: it must land on
        // exactly the matching output speaker (FL FR C LFE SL SR).
        let mut scene = SpatialScene::new(48_000);
        let id = scene.create_bed(ChannelLayout::FivePointOne).unwrap();
        scene.bed_mut(id).unwrap().gain = 1.0;

        let layout = SpeakerLayout::five_point_one();
        let roles: Vec<(usize, ChannelId)> = layout
            .speakers
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.role.map(|r| (i, r)))
            .collect();
        let out_trim = vec![1.0f32; 6];

        // Bed planes: FL=1, FR=2, C=3, LFE=4, SL=5, SR=6 (impulses).
        let inputs: Vec<Vec<f32>> = vec![
            vec![1.0],
            vec![2.0],
            vec![3.0],
            vec![4.0],
            vec![5.0],
            vec![6.0],
        ];
        let refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
        let mut out = vec![0.0f32; 6];
        render_beds(&scene, &refs, 1, &mut out, &roles, &out_trim);

        for (spk, expected) in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0].iter().enumerate() {
            assert!(
                (out[spk] - expected).abs() < 1e-5,
                "speaker {spk} got {} want {expected}",
                out[spk]
            );
        }
    }

    #[test]
    fn render_drops_unmatched_channels() {
        // A 7.1 bed on a 5.1 output: FL FR C LFE SL SR route; RL/RR (index
        // 6/7) have no 5.1 speaker and are dropped.
        let mut scene = SpatialScene::new(48_000);
        let id = scene.create_bed(ChannelLayout::SevenPointOne).unwrap();
        scene.bed_mut(id).unwrap().gain = 1.0;

        let layout = SpeakerLayout::five_point_one();
        let roles: Vec<(usize, ChannelId)> = layout
            .speakers
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.role.map(|r| (i, r)))
            .collect();
        let out_trim = vec![1.0f32; 6];
        let inputs: Vec<Vec<f32>> = (0..8).map(|v| vec![v as f32 + 1.0]).collect();
        let refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
        let mut out = vec![0.0f32; 6];
        render_beds(&scene, &refs, 1, &mut out, &roles, &out_trim);
        // RL plane (7.0) and RR plane (7.0) must not appear anywhere.
        for (spk, v) in out.iter().enumerate() {
            assert!((*v - (spk as f32 + 1.0)).abs() < 1e-5, "spk {spk} = {v}");
        }
        let _ = Vec3::ZERO;
    }

    #[test]
    fn render_applies_bed_gain_and_calibration_trim() {
        let (mut scene, id) = scene_with_5_1_bed();
        scene.bed_mut(id).unwrap().gain = 0.5;
        let layout = SpeakerLayout::five_point_one();
        let roles: Vec<(usize, ChannelId)> = layout
            .speakers
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.role.map(|r| (i, r)))
            .collect();
        let mut out_trim = vec![1.0f32; 6];
        out_trim[0] = 0.25; // FL trim −12 dB
        let inputs: Vec<Vec<f32>> = (0..6).map(|_| vec![1.0f32]).collect();
        let refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
        let mut out = vec![0.0f32; 6];
        render_beds(&scene, &refs, 1, &mut out, &roles, &out_trim);
        assert!(
            (out[0] - 0.5 * 0.25).abs() < 1e-5,
            "FL gain × trim {}",
            out[0]
        );
        assert!((out[1] - 0.5).abs() < 1e-5, "FR gain only {}", out[1]);
    }
}
