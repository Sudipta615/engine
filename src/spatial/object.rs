//! Spatial objects (spec Part III §13–15).
//!
//! An object is a localized or extended source placed in the scene. It is
//! authored **independently of the output speaker count**: the same scene of
//! objects renders to stereo, headphones, 5.1, 7.1.4, or a custom array.
//!
//! Source ownership follows spec §15: an object does **not** require a
//! unique decoded source. `ObjectAudioRef` is a thin, shareable reference
//! to the engine's [`crate::AudioSource`] so one decoded source can drive
//! many instances; the actual per-block audio flows through the renderer's
//! input planes.

use super::automation::SpatialAutomation;
use super::directivity::Directivity;
use super::doppler::Doppler;
use super::level::{AirAbsorption, DistanceModel};
use super::math::{Quat, Vec3};
use super::nearfield::NearField;
use super::occlusion::Occlusion;
use crate::AudioSource;

/// Stable handle to an object within a [`SpatialObjectStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub usize);

/// Runtime classification of a spatial source (spec §31). `Point` is fully
/// rendered; `Extended` is approximated by the angular-region spread model
/// ([`crate::spatial::spread`], §30); `Diffuse` maps to the field-mixer path
/// and `Bed` to the bed renderer. Classification is authored metadata the
/// scene carries forward — the renderers key off each class's concrete ports
/// (spread / room_send / lfe_send / beds / fields) rather than one switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialSourceType {
    Point,
    Extended,
    Diffuse,
    Bed,
}

/// A shareable reference to an engine audio source (spec §15). The engine's
/// [`crate::AudioSource`] is an *opening request* (file/uri/memory hint),
/// not decoded audio; objects that have been handed decoded planes can carry
/// `None`. So one source may be referenced by many object instances without
/// forcing per-instance decode ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectAudioRef {
    /// A shareable engine source reference (multiple objects may reuse it).
    Shared(AudioSource),
    /// No managed source — decoded audio is supplied directly per block.
    None,
}

impl ObjectAudioRef {
    pub fn shared(source: AudioSource) -> Self {
        ObjectAudioRef::Shared(source)
    }
}

/// A single spatialized audio object.
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialAudioObject {
    pub id: ObjectId,
    /// Audio source reference (may be `None` when driven per-block).
    pub source: ObjectAudioRef,
    /// World-space position (metres), relative to the listener reference.
    pub position: Vec3,
    /// Velocity (m/s); drives the per-block Doppler shift (spec §42) when
    /// [`Doppler`] is enabled, using the radial component of
    /// `velocity − listener.velocity`.
    pub velocity: Vec3,
    /// World-space orientation; local `+Y` is the source's facing direction
    /// (drives [`Directivity`] and future source-rotation behavior).
    pub source_orientation: Quat,
    /// Linear gain before distance/panning (1.0 = unity).
    pub gain: f32,
    /// Normalized angular spread in `[0,1]`: 0 = point, →1 = wide angular
    /// region (spec §30).
    pub spread: f32,
    /// Directional response: how output varies with the listener's angle
    /// relative to the source's facing (spec §41).
    pub directivity: Directivity,
    /// Occlusion: broadband attenuation + low-pass roll-off (spec §43–44).
    pub occlusion: Occlusion,
    /// Applied high-frequency air absorption (spec §39; disabled = exact
    /// passthrough in both gain and filter).
    pub air_absorption: AirAbsorption,
    /// Near-field correction: proximity gain + LF lift (spec §40; disabled =
    /// exact passthrough).
    pub near_field: NearField,
    /// Doppler from the relative source/listener velocity (spec §42;
    /// disabled = exact passthrough).
    pub doppler: Doppler,
    /// Optional automation override for position/orientation/gain/spread
    /// (spec §47; control path applies it to the render fields).
    pub automation: SpatialAutomation,
    /// Distance attenuation law.
    pub distance_model: DistanceModel,
    /// Distance reference (metres) for laws that use one.
    pub reference_distance: f32,
    /// Send level `[0,1]` into the internal room/reflection path (seam;
    /// stored so the scene model needs no redesign later).
    pub room_send: f32,
    /// Send level into the LFE effects path (spec §56–57). Additive, never
    /// a pan target.
    pub lfe_send: f32,
    /// Dedicated bass send level `[0,1]`.
    pub bass_send: f32,
    /// Per-object bass intent mode (DirectLfe, SubBassOnly, FullRangeManaged, Bypass).
    pub bass_intent: config::BassIntent,
    /// Psychoacoustic importance factor for adaptive CPU scaling.
    pub importance: f32,
    pub enabled: bool,
    pub source_type: SpatialSourceType,
    /// Head-locked flag: when true, source position/orientation is locked to listener head.
    pub head_locked: bool,
    /// Screen-relative positioning flag: when true, positions are relative to a display screen.
    pub screen_relative: bool,
    /// Screen reference geometry for screen-relative objects.
    pub screen_ref: Option<ScreenReference>,
    /// ITU-R BS.2076 divergence parameter in `[0.0, 1.0]`.
    pub divergence: f32,
    /// 3D extent (width, height, depth) for extended sources.
    pub extent: ObjectExtent,
    /// Diffuseness factor in `[0.0, 1.0]` for energy routed to diffuse field.
    pub diffuseness: f32,
    /// Absolute distance override in metres (optional).
    pub absolute_distance: Option<f32>,
    /// Zone exclusion bounding box.
    pub zone_exclusion: Option<ZoneExclusion>,
}

/// Screen reference geometry for screen-relative spatial objects (§4.5).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScreenReference {
    pub screen_id: String,
    pub aspect_ratio: f32,
    pub width_m: f32,
    pub height_m: f32,
}

impl Default for ScreenReference {
    fn default() -> Self {
        Self {
            screen_id: "default_screen".to_string(),
            aspect_ratio: 16.0 / 9.0,
            width_m: 2.0,
            height_m: 1.125,
        }
    }
}

/// 3D bounding extents (width, height, depth) for extended audio sources (§4.5).
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ObjectExtent {
    /// Width in metres or angular radians.
    pub width: f32,
    /// Height in metres or angular radians.
    pub height: f32,
    /// Depth in metres or angular radians.
    pub depth: f32,
}

/// 3D exclusion zone bounding box in world coordinates (§4.5).
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ZoneExclusion {
    pub min_x: f32,
    pub max_x: f32,
    pub min_y: f32,
    pub max_y: f32,
    pub min_z: f32,
    pub max_z: f32,
}

impl SpatialAudioObject {
    pub fn new(id: ObjectId, source: ObjectAudioRef, position: Vec3) -> Self {
        Self {
            id,
            source,
            position,
            velocity: Vec3::ZERO,
            source_orientation: Quat::IDENTITY,
            gain: 1.0,
            spread: 0.0,
            directivity: Directivity::default(),
            occlusion: Occlusion::default(),
            air_absorption: AirAbsorption::default(),
            near_field: NearField::default(),
            doppler: Doppler::default(),
            automation: SpatialAutomation::default(),
            distance_model: DistanceModel::InverseReference,
            reference_distance: 1.0,
            room_send: 0.0,
            lfe_send: 0.0,
            bass_send: 0.0,
            bass_intent: config::BassIntent::default(),
            importance: 1.0,
            enabled: true,
            source_type: SpatialSourceType::Point,
            head_locked: false,
            screen_relative: false,
            screen_ref: None,
            divergence: 0.0,
            extent: ObjectExtent::default(),
            diffuseness: 0.0,
            absolute_distance: None,
            zone_exclusion: None,
        }
    }

    pub fn with_head_locked(mut self, head_locked: bool) -> Self {
        self.head_locked = head_locked;
        self
    }

    pub fn with_screen_relative(
        mut self,
        screen_relative: bool,
        screen_ref: Option<ScreenReference>,
    ) -> Self {
        self.screen_relative = screen_relative;
        self.screen_ref = screen_ref;
        self
    }

    pub fn with_divergence(mut self, divergence: f32) -> Self {
        self.divergence = divergence.clamp(0.0, 1.0);
        self
    }

    pub fn with_extent(mut self, width: f32, height: f32, depth: f32) -> Self {
        self.extent = ObjectExtent {
            width: width.max(0.0),
            height: height.max(0.0),
            depth: depth.max(0.0),
        };
        self
    }

    pub fn with_diffuseness(mut self, diffuseness: f32) -> Self {
        self.diffuseness = diffuseness.clamp(0.0, 1.0);
        self
    }

    pub fn with_absolute_distance(mut self, distance_m: f32) -> Self {
        self.absolute_distance = Some(distance_m.max(0.0));
        self
    }

    pub fn with_zone_exclusion(mut self, zone: ZoneExclusion) -> Self {
        self.zone_exclusion = Some(zone);
        self
    }

    /// The listener-to-object distance in metres implied by this block's
    /// listener transform (simple Euclidean distance to the origin of the
    /// listener frame). The renderer computes this from the transformed
    /// position; this helper documents the unit convention.
    pub fn distance_from_origin(&self, listener_space_pos: Vec3) -> f32 {
        if let Some(abs_dist) = self.absolute_distance {
            abs_dist
        } else {
            listener_space_pos.length()
        }
    }
}

/// Fixed-capacity store so the render hot path is bounded (spec §75–76).
///
/// Add/remove are control-path operations; the renderer iterates a stable
/// slice. `MAX_SPATIAL_OBJECTS` is the hard ceiling per scene.
pub const MAX_SPATIAL_OBJECTS: usize = 64;

#[derive(Debug, Clone)]
pub struct SpatialObjectStore {
    /// A fixed-capacity slot array. A `None` slot is a removed object whose
    /// [`ObjectId`] (index) stays reserved so existing handles never point at
    /// a different object after a removal — ids are stable, not shifted.
    objects: Vec<Option<SpatialAudioObject>>,
}

impl Default for SpatialObjectStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SpatialObjectStore {
    pub fn new() -> Self {
        Self {
            objects: Vec::with_capacity(MAX_SPATIAL_OBJECTS),
        }
    }

    /// Add an object, returning its stable [`ObjectId`]. Returns `None` at
    /// capacity (bounded, deterministic — never unbounded growth). Reuses a
    /// freed slot if any, otherwise appends a new one.
    pub fn add(&mut self, mut object: SpatialAudioObject) -> Option<ObjectId> {
        // Reuse a freed slot first (stable id reuse is acceptable after an
        // explicit remove), else append up to capacity.
        if let Some(slot) = self.objects.iter_mut().position(|o| o.is_none()) {
            object.id = ObjectId(slot);
            self.objects[slot] = Some(object);
            return Some(ObjectId(slot));
        }
        if self.objects.len() >= MAX_SPATIAL_OBJECTS {
            return None;
        }
        let id = ObjectId(self.objects.len());
        object.id = id;
        self.objects.push(Some(object));
        Some(id)
    }

    pub fn get(&self, id: ObjectId) -> Option<&SpatialAudioObject> {
        self.objects.get(id.0).and_then(|o| o.as_ref())
    }

    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut SpatialAudioObject> {
        self.objects.get_mut(id.0).and_then(|o| o.as_mut())
    }

    /// Remove the object at `id` and return it (control-path). Freeing a
    /// slot leaves the id reserved (a later `add` may reuse it).
    pub fn remove(&mut self, id: ObjectId) -> Option<SpatialAudioObject> {
        self.objects.get_mut(id.0).and_then(|slot| slot.take())
    }

    pub fn iter(&self) -> impl Iterator<Item = &SpatialAudioObject> {
        self.objects.iter().flatten()
    }

    pub fn len(&self) -> usize {
        self.objects.iter().filter(|o| o.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate enabled objects as `(store-slot, &object)`, in store order.
    ///
    /// The slot index is the object's stable [`ObjectId`] (usize), so renderers
    /// can key per-object state by it without re-indexing after removals. This
    /// is the render-path iterator: it allocates nothing.
    pub fn iter_enabled(&self) -> impl Iterator<Item = (usize, &SpatialAudioObject)> + '_ {
        self.objects
            .iter()
            .enumerate()
            .filter_map(|(slot, o)| o.as_ref().map(|o| (slot, o)).filter(|(_, o)| o.enabled))
    }

    /// The enabled objects, as store-slot indices for panning (render path).
    pub fn active_indices(&self) -> Vec<usize> {
        self.iter_enabled().map(|(slot, _)| slot).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_bounds_object_count() {
        let mut store = SpatialObjectStore::new();
        for i in 0..(MAX_SPATIAL_OBJECTS + 5) {
            let o = SpatialAudioObject::new(
                ObjectId(i),
                ObjectAudioRef::None,
                Vec3::new(1.0, 2.0, 3.0),
            );
            let id = store.add(o);
            if i < MAX_SPATIAL_OBJECTS {
                assert!(id.is_some());
            } else {
                assert!(id.is_none(), "store must stay bounded");
            }
        }
        assert_eq!(store.len(), MAX_SPATIAL_OBJECTS);
    }

    #[test]
    fn store_assigns_stable_ids_and_remove() {
        let mut store = SpatialObjectStore::new();
        let a = store.add(SpatialAudioObject::new(
            ObjectId(99),
            ObjectAudioRef::None,
            Vec3::Y,
        ));
        let b = store.add(SpatialAudioObject::new(
            ObjectId(99),
            ObjectAudioRef::None,
            Vec3::X,
        ));
        let a = a.unwrap();
        let b = b.unwrap();
        assert_ne!(a, b);
        assert_eq!(store.get(a).unwrap().position, Vec3::Y);
        assert_eq!(store.get(b).unwrap().position, Vec3::X);
        let removed = store.remove(a).unwrap();
        assert_eq!(removed.position, Vec3::Y);
        assert_eq!(store.len(), 1);
        assert!(store.get(a).is_none());
    }
}
