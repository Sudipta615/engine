//! The spatial scene (spec Part III §12, §16–18).
//!
//! The scene is **independent of the output speaker count**: it holds the
//! listener and the three authored content classes — objects (world-space,
//! transformed into listener space for rendering), beds (channel-based),
//! and fields (diffuse) — and a renderer places them on whatever layout is
//! active.
//!
//! Listener orientation is a first-class transform (spec §48): world-fixed
//! objects appear to move exactly as the listener rotates, which is the
//! foundation for head tracking / VR later.

use super::bed::{BedId, SpatialBed, SpatialBedStore};
use super::field::{FieldId, SpatialField, SpatialFieldStore};
use super::math::{Quat, Vec3};
use super::object::{ObjectAudioRef, ObjectId, SpatialAudioObject, SpatialObjectStore};
use super::room::Room;
use crate::decode::ChannelLayout;

/// The listener: the reference frame for spatial perception.
#[derive(Debug, Clone, PartialEq)]
pub struct Listener {
    /// World-space position (metres).
    pub position: Vec3,
    /// World-space orientation (a unit quaternion; +Y = facing).
    pub orientation: Quat,
    /// Velocity (m/s); used for future Doppler (declared seam).
    pub velocity: Vec3,
    /// Whether the listener-head rotation is applied. Always true in this
    /// phase; kept as a field so disabling world-locked audio is trivial.
    pub rendered_orientation: bool,
}

impl Default for Listener {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            orientation: Quat::IDENTITY,
            velocity: Vec3::ZERO,
            rendered_orientation: true,
        }
    }
}

impl Listener {
    pub fn set_position(&mut self, position: Vec3) -> &mut Self {
        self.position = position;
        self
    }

    pub fn set_orientation(&mut self, orientation: Quat) -> &mut Self {
        self.orientation = orientation;
        self
    }
}

/// A precomputed world→listener-space transform (spec §16, §48).
///
/// Built once per scene render on the control side and used read-only on the
/// render path. Rotation is the conjugate of the listener's orientation
/// (so world-fixed objects move opposite to the head); translation subtracts
/// the listener's position before rotation.
#[derive(Debug, Clone, Copy)]
pub struct ListenerTransform {
    pub position: Vec3,
    pub orientation: Quat,
}

impl ListenerTransform {
    pub fn from_listener(listener: &Listener) -> Self {
        let orientation = if listener.rendered_orientation {
            listener.orientation.inverse_rotation()
        } else {
            Quat::IDENTITY
        };
        Self {
            position: listener.position,
            orientation,
        }
    }

    /// Convert a world-space point into listener space.
    pub fn apply_to_point(&self, world: Vec3) -> Vec3 {
        // translate: world - listener.position, then rotate by conjugate.
        let translated = world - self.position;
        self.orientation.rotate_vec3(translated)
    }

    /// Convert a world-space direction (unit or not) into listener space,
    /// ignoring the listener's position. Used for panning spread.
    pub fn apply_to_direction(&self, world_dir: Vec3) -> Vec3 {
        self.orientation.rotate_vec3(world_dir)
    }
}

/// The scene: listener + objects + beds + fields + room. The three content
/// classes (spec §13) are independent of the output speaker count: objects
/// are point/extended sources, beds are channel-based content, fields are
/// diffuse environments. The room (§49) is an acoustic space the renderers
/// apply to participating objects; it is deliberately **not** a field (spec
/// §136) — it is a scene-level structure, disabled by default so existing
/// scenes render bit-identically. The renderer reads the scene read-only.
#[derive(Debug, Clone)]
pub struct SpatialScene {
    pub listener: Listener,
    pub objects: SpatialObjectStore,
    /// Channel-based beds (spec §13.1).
    pub beds: SpatialBedStore,
    /// Diffuse fields (spec §13.3).
    pub fields: SpatialFieldStore,
    /// Room acoustics (spec §49): early reflections + late field.
    pub room: Room,
    pub sample_rate: u32,
}

impl SpatialScene {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            listener: Listener::default(),
            objects: SpatialObjectStore::new(),
            beds: SpatialBedStore::new(),
            fields: SpatialFieldStore::new(),
            room: Room::default(),
            sample_rate,
        }
    }

    /// Create an object in the scene with a shareable engine source
    /// reference (spec §15). The object is positioned at `position`.
    pub fn create_object(&mut self, source: ObjectAudioRef, position: Vec3) -> Option<ObjectId> {
        let id = ObjectId(self.objects.len());
        let obj = SpatialAudioObject::new(id, source, position);
        self.objects.add(obj)
    }

    /// Convenience: create a no-source object (audio supplied per block).
    pub fn create_audio_object(&mut self, position: Vec3) -> Option<ObjectId> {
        self.create_object(ObjectAudioRef::None, position)
    }

    pub fn object(&self, id: ObjectId) -> Option<&SpatialAudioObject> {
        self.objects.get(id)
    }

    pub fn object_mut(&mut self, id: ObjectId) -> Option<&mut SpatialAudioObject> {
        self.objects.get_mut(id)
    }

    /// Create a channel-based bed (spec §13.1) and return its stable id.
    pub fn create_bed(&mut self, layout: ChannelLayout) -> Option<BedId> {
        let id = BedId(self.beds.len());
        self.beds.add(SpatialBed::new(id, layout))
    }

    pub fn bed(&self, id: BedId) -> Option<&SpatialBed> {
        self.beds.get(id)
    }

    pub fn bed_mut(&mut self, id: BedId) -> Option<&mut SpatialBed> {
        self.beds.get_mut(id)
    }

    /// Create a diffuse field (spec §13.3) and return its stable id.
    pub fn create_field(&mut self) -> Option<FieldId> {
        let id = FieldId(self.fields.len());
        self.fields.add(SpatialField::new(id))
    }

    pub fn field(&self, id: FieldId) -> Option<&SpatialField> {
        self.fields.get(id)
    }

    pub fn field_mut(&mut self, id: FieldId) -> Option<&mut SpatialField> {
        self.fields.get_mut(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::math::Vec3 as V;

    #[test]
    fn listener_transform_keeps_world_fixed_object_stable() {
        // Listener at the origin yawing +90°. World-fixed object at +X must
        // appear at the listener's front (+Y).
        let mut scene = SpatialScene::new(48_000);
        scene
            .listener
            .set_orientation(Quat::from_euler_rad(std::f32::consts::FRAC_PI_2, 0.0, 0.0));
        let xf = ListenerTransform::from_listener(&scene.listener);
        let local = xf.apply_to_point(Vec3::X);
        assert!((local - Vec3::Y).length() < 1e-5);
    }

    #[test]
    fn listener_translation_then_rotation_is_applied() {
        // Listener at (10, 0, 0) facing +Y, object at world (12, 0, 0).
        // Object is 2 m to the listener's right (+X local).
        let mut scene = SpatialScene::new(48_000);
        scene.listener.set_position(Vec3::new(10.0, 0.0, 0.0));
        let xf = ListenerTransform::from_listener(&scene.listener);
        let local = xf.apply_to_point(Vec3::new(12.0, 0.0, 0.0));
        assert!((local.x - 2.0).abs() < 1e-5);
        assert!((local.y).abs() < 1e-5);
    }

    #[test]
    fn scene_creates_objects_and_reuses_sources() {
        let mut scene = SpatialScene::new(48_000);
        // One shareable source reused by many instances (spec §15).
        let src = ObjectAudioRef::shared(crate::AudioSource::from_file("/tmp/voice.wav"));
        let a = scene
            .create_object(src.clone(), Vec3::new(1.0, 0.0, 0.0))
            .unwrap();
        let b = scene.create_object(src, Vec3::new(-1.0, 0.0, 0.0)).unwrap();
        let c = scene.create_audio_object(V::ZERO).unwrap();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_eq!(scene.objects.len(), 3);
        assert!(matches!(
            scene.object(a).unwrap().source,
            ObjectAudioRef::Shared(_)
        ));
        assert!(matches!(
            scene.object(c).unwrap().source,
            ObjectAudioRef::None
        ));
    }
}
