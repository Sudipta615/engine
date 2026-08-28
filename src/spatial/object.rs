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

use super::level::DistanceModel;
use super::math::Vec3;
use crate::AudioSource;

/// Stable handle to an object within a [`SpatialObjectStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub usize);

/// Runtime classification of a spatial source (spec §31). This phase renders
/// `Point` (and treats `Extended`/`Diffuse`/`Bed` as documented seams whose
/// simplest forms are approximated); the full angular-region model for
/// extended/diffuse sources is a later-phase enrichment.
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
    /// Velocity (m/s); used for future Doppler (declared seam).
    pub velocity: Vec3,
    /// Linear gain before distance/panning (1.0 = unity).
    pub gain: f32,
    /// Normalized angular spread in `[0,1]`: 0 = point, →1 = diffuse-ish.
    pub spread: f32,
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
    pub enabled: bool,
    pub source_type: SpatialSourceType,
}

impl SpatialAudioObject {
    pub fn new(id: ObjectId, source: ObjectAudioRef, position: Vec3) -> Self {
        Self {
            id,
            source,
            position,
            velocity: Vec3::ZERO,
            gain: 1.0,
            spread: 0.0,
            distance_model: DistanceModel::InverseReference,
            reference_distance: 1.0,
            room_send: 0.0,
            lfe_send: 0.0,
            enabled: true,
            source_type: SpatialSourceType::Point,
        }
    }

    /// The listener-to-object distance in metres implied by this block's
    /// listener transform (simple Euclidean distance to the origin of the
    /// listener frame). The renderer computes this from the transformed
    /// position; this helper documents the unit convention.
    pub fn distance_from_origin(&self, listener_space_pos: Vec3) -> f32 {
        listener_space_pos.length()
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
