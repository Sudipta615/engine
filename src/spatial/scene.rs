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

use super::automation::{CurveQuat, CurveScalar, CurveVec3, SpatialAutomation};
use super::bed::{BedId, SpatialBed, SpatialBedStore, MAX_BEDS};
use super::field::{FieldId, SpatialField, SpatialFieldStore, MAX_FIELDS};
use super::math::{Quat, Vec3};
use super::object::MAX_SPATIAL_OBJECTS;
use super::object::{ObjectAudioRef, ObjectId, SpatialAudioObject, SpatialObjectStore};
use super::render::RenderError;
use super::room::Room;
use crate::decode::{ChannelId, ChannelLayout};

/// The listener: the reference frame for spatial perception.
#[derive(Debug, Clone, PartialEq)]
pub struct Listener {
    /// World-space position (metres).
    pub position: Vec3,
    /// World-space orientation (a unit quaternion; +Y = facing).
    pub orientation: Quat,
    /// Velocity (m/s); offsets the per-object Doppler reference frame — each
    /// object's shift uses `object.velocity − listener.velocity` (spec §42).
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

    /// Build a live scene from the Serde scene-file model (spec Part XXVI).
    /// Control path. `Err(CapacityExceeded)` when a class exceeds the engine
    /// caps; `Err(InvalidScene)` for an invalid model (call
    /// `config::SpatialSceneConfig::validate` first for rich errors).
    pub fn from_config(cfg: &config::SpatialSceneConfig) -> Result<Self, RenderError> {
        if cfg.objects.len() > MAX_SPATIAL_OBJECTS
            || cfg.beds.len() > MAX_BEDS
            || cfg.fields.len() > MAX_FIELDS
        {
            return Err(RenderError::CapacityExceeded);
        }
        let mut scene = SpatialScene::new(cfg.sample_rate.max(1));
        let l = &cfg.listener;
        scene.listener.position = Vec3::new(l.position[0], l.position[1], l.position[2]);
        let q = Quat::new(
            l.orientation[0],
            l.orientation[1],
            l.orientation[2],
            l.orientation[3],
        );
        scene
            .listener
            .set_orientation(q.normalized().unwrap_or(Quat::IDENTITY));
        for o in &cfg.objects {
            let id = scene
                .create_audio_object(Vec3::new(o.position[0], o.position[1], o.position[2]))
                .ok_or(RenderError::CapacityExceeded)?;
            let obj = scene.object_mut(id).expect("just created");
            obj.gain = o.gain;
            obj.spread = o.spread;
            obj.room_send = o.room_send;
            obj.lfe_send = o.lfe_send;
            obj.enabled = o.enabled;
            let a = &o.automation;
            obj.automation = SpatialAutomation {
                position: a.position.as_ref().and_then(|c| {
                    CurveVec3::from_points(
                        &c.points
                            .iter()
                            .map(|(t, p)| (*t, Vec3::new(p[0], p[1], p[2])))
                            .collect::<Vec<_>>(),
                    )
                }),
                orientation: a.orientation.as_ref().and_then(|c| {
                    CurveQuat::from_points(
                        &c.points
                            .iter()
                            .map(|(t, q)| (*t, Quat::new(q[0], q[1], q[2], q[3])))
                            .collect::<Vec<_>>(),
                    )
                }),
                gain: a
                    .gain
                    .as_ref()
                    .and_then(|c| CurveScalar::from_points(&c.points)),
                spread: a
                    .spread
                    .as_ref()
                    .and_then(|c| CurveScalar::from_points(&c.points)),
                sample_rate: cfg.sample_rate as f32,
            };
        }
        for b in &cfg.beds {
            let ids: Result<Vec<ChannelId>, RenderError> = b
                .channels
                .iter()
                .map(|name| ChannelId::from_name(name).ok_or(RenderError::InvalidScene))
                .collect();
            let id = scene
                .create_bed(ChannelLayout::Custom(ids?))
                .ok_or(RenderError::CapacityExceeded)?;
            let bed = scene.bed_mut(id).expect("just created");
            bed.gain = b.gain;
            bed.enabled = b.enabled;
        }
        for f in &cfg.fields {
            let id = scene.create_field().ok_or(RenderError::CapacityExceeded)?;
            let field = scene.field_mut(id).expect("just created");
            field.gain = f.gain;
            field.enabled = f.enabled;
        }
        let r = &cfg.room;
        scene.room = Room {
            enabled: r.enabled,
            width: r.width,
            depth: r.depth,
            height: r.height,
            absorption: r.absorption,
            reflection_order: r.reflection_order,
            rt60_ms: r.rt60_ms,
            late_mix: r.late_mix,
            speed_of_sound: r.speed_of_sound,
        };
        Ok(scene)
    }

    /// Serialize the scene into the Serde scene-file model (the inverse of
    /// [`Self::from_config`]). Content only — the renderer/layout stay host
    /// choices.
    pub fn to_config(&self) -> config::SpatialSceneConfig {
        use config::{
            SceneListenerConfig, SpatialBedConfig, SpatialFieldConfig, SpatialObjectConfig,
            SpatialSceneConfig,
        };
        let l = &self.listener;
        SpatialSceneConfig {
            sample_rate: self.sample_rate,
            listener: SceneListenerConfig {
                position: [l.position.x, l.position.y, l.position.z],
                orientation: [
                    l.orientation.x,
                    l.orientation.y,
                    l.orientation.z,
                    l.orientation.w,
                ],
            },
            objects: self
                .objects
                .iter()
                .map(|o| SpatialObjectConfig {
                    name: String::new(),
                    position: [o.position.x, o.position.y, o.position.z],
                    gain: o.gain,
                    spread: o.spread,
                    room_send: o.room_send,
                    lfe_send: o.lfe_send,
                    automation: config::SpatialAutomationConfig {
                        position: o
                            .automation
                            .position
                            .as_ref()
                            .map(|c| config::CurveVec3Config {
                                points: c
                                    .keyframes()
                                    .iter()
                                    .map(|(t, p)| (*t, [p.x, p.y, p.z]))
                                    .collect(),
                            }),
                        orientation: o.automation.orientation.as_ref().map(|c| {
                            config::CurveQuatConfig {
                                points: c
                                    .keyframes()
                                    .iter()
                                    .map(|(t, q)| (*t, [q.x, q.y, q.z, q.w]))
                                    .collect(),
                            }
                        }),
                        gain: o
                            .automation
                            .gain
                            .as_ref()
                            .map(|c| config::CurveScalarConfig {
                                points: c.keyframes().to_vec(),
                            }),
                        spread: o
                            .automation
                            .spread
                            .as_ref()
                            .map(|c| config::CurveScalarConfig {
                                points: c.keyframes().to_vec(),
                            }),
                    },
                    enabled: o.enabled,
                })
                .collect(),
            beds: self
                .beds
                .iter()
                .map(|b| SpatialBedConfig {
                    name: String::new(),
                    channels: b.channels().iter().map(|c| c.name()).collect(),
                    gain: b.gain,
                    enabled: b.enabled,
                })
                .collect(),
            fields: self
                .fields
                .iter()
                .map(|f| SpatialFieldConfig {
                    name: String::new(),
                    gain: f.gain,
                    enabled: f.enabled,
                })
                .collect(),
            room: config::SpatialRoomConfig {
                enabled: self.room.enabled,
                width: self.room.width,
                depth: self.room.depth,
                height: self.room.height,
                absorption: self.room.absorption,
                reflection_order: self.room.reflection_order,
                rt60_ms: self.room.rt60_ms,
                late_mix: self.room.late_mix,
                wet: self
                    .objects
                    .iter()
                    .next()
                    .map(|o| o.room_send)
                    .unwrap_or(0.0),
                speed_of_sound: self.room.speed_of_sound,
            },
        }
    }
}

/// Scene-file I/O errors (spec Part XXVI): disk/JSON failures and scene
/// validation failures surface as typed errors — never panics.
#[derive(Debug, thiserror::Error)]
pub enum SceneFileError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("scene: {0}")]
    Scene(#[from] RenderError),
}

/// Save a scene to a JSON file (the scene-file format, spec Part XXVI).
/// The scene is first serialized to its config model, so only content is
/// persisted — the renderer and output layout remain host choices.
pub fn save_scene_json(path: &std::path::Path, scene: &SpatialScene) -> Result<(), SceneFileError> {
    let cfg = scene.to_config();
    let file = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(file, &cfg)?;
    Ok(())
}

/// Load a scene from a JSON file, validating it against the engine caps.
pub fn load_scene_json(path: &std::path::Path) -> Result<SpatialScene, SceneFileError> {
    let file = std::fs::File::open(path)?;
    let cfg: config::SpatialSceneConfig = serde_json::from_reader(file)?;
    Ok(SpatialScene::from_config(&cfg)?)
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
    fn config_round_trip_preserves_content() {
        let mut sc = SpatialScene::new(48_000);
        sc.listener
            .set_position(V::new(1.0, 2.0, 0.5))
            .set_orientation(Quat::from_euler_rad(0.7, 0.3, -0.2));
        let o = sc.create_audio_object(V::new(-3.0, 4.0, 1.0)).unwrap();
        let obj = sc.object_mut(o).unwrap();
        obj.gain = 0.8;
        obj.spread = 0.4;
        obj.room_send = 0.5;
        obj.lfe_send = 0.2;
        let b = sc
            .create_bed(crate::decode::ChannelLayout::SevenPointOne)
            .unwrap();
        sc.bed_mut(b).unwrap().gain = 0.9;
        let f = sc.create_field().unwrap();
        sc.field_mut(f).unwrap().gain = 0.7;
        sc.room.enabled = true;
        sc.room.width = 10.0;
        sc.room.rt60_ms = 650.0;

        let cfg = sc.to_config();
        assert!(cfg.validate().is_ok(), "saved scene validates");
        let back = SpatialScene::from_config(&cfg).unwrap();
        assert_eq!(back.sample_rate, 48_000);
        assert!((back.listener.position - sc.listener.position).length() < 1e-5);
        assert!((back.listener.orientation.dot(sc.listener.orientation)).abs() > 0.9999);
        let bo = back.object(o).unwrap();
        let so = sc.object(o).unwrap();
        assert!((bo.position - so.position).length() < 1e-5);
        assert!((bo.gain - so.gain).abs() < 1e-6);
        assert!((bo.spread - so.spread).abs() < 1e-6);
        assert!((bo.room_send - so.room_send).abs() < 1e-6);
        assert!((bo.lfe_send - so.lfe_send).abs() < 1e-6);
        let bb = back.bed(b).unwrap();
        assert_eq!(bb.channels(), sc.bed(b).unwrap().channels());
        assert!((bb.gain - 0.9).abs() < 1e-6);
        let bf = back.field(f).unwrap();
        assert!((bf.gain - 0.7).abs() < 1e-6);
        assert!(back.room.enabled);
        assert!((back.room.width - 10.0).abs() < 1e-6);
        assert!((back.room.rt60_ms - 650.0).abs() < 1e-6);
    }

    #[test]
    fn save_load_json_round_trip() {
        let mut sc = SpatialScene::new(48_000);
        sc.create_audio_object(V::new(-1.0, 2.0, 0.0));
        sc.create_bed(crate::decode::ChannelLayout::FivePointOne);
        sc.create_field();
        sc.room.enabled = true;
        let path = std::env::temp_dir().join(format!(
            "shadow_scene_roundtrip_{}.json",
            std::process::id()
        ));
        save_scene_json(&path, &sc).unwrap();
        let back = load_scene_json(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(back.objects.len(), 1);
        assert_eq!(back.beds.len(), 1);
        assert_eq!(back.fields.len(), 1);
        assert!(back.room.enabled);
    }

    #[test]
    fn from_config_rejects_overflow_and_bad_roles() {
        use config::{SpatialObjectConfig, SpatialSceneConfig};
        let cfg = SpatialSceneConfig {
            objects: (0..65).map(|_| SpatialObjectConfig::default()).collect(),
            ..Default::default()
        };
        assert!(matches!(
            SpatialScene::from_config(&cfg),
            Err(RenderError::CapacityExceeded)
        ));
        let mut cfg = SpatialSceneConfig::default();
        cfg.objects.push(SpatialObjectConfig::default());
        cfg.beds.push(config::SpatialBedConfig {
            channels: vec!["FL".to_string(), "BOGUS".to_string()],
            ..Default::default()
        });
        assert!(matches!(
            SpatialScene::from_config(&cfg),
            Err(RenderError::InvalidScene)
        ));
        assert!(cfg.validate().is_err());
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

    #[test]
    fn automation_curves_round_trip_through_config() {
        // A scene authored with automation must survive config -> live ->
        // config losslessly (curves drive object params over time, spec §47).
        let cfg = config::SpatialSceneConfig {
            objects: vec![config::SpatialObjectConfig {
                position: [2.0, 0.0, 0.0],
                automation: config::SpatialAutomationConfig {
                    position: Some(config::CurveVec3Config {
                        points: vec![(0.0, [0.0, 0.0, 0.0]), (1.0, [0.0, 2.0, 0.0])],
                    }),
                    gain: Some(config::CurveScalarConfig {
                        points: vec![(0.0, 1.0), (2.0, 0.0)],
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        cfg.validate().expect("valid automation config");

        // Config -> live scene: the object carries the automation curves.
        let scene = SpatialScene::from_config(&cfg).expect("build");
        let obj = scene.objects.iter().next().expect("one object");
        assert!(obj.automation.position.is_some());
        assert!(obj.automation.gain.is_some());
        assert!(obj.automation.orientation.is_none());
        assert!(obj.automation.spread.is_none());
        // Position path interpolates: at t=0.5 the object should sit at y=1.
        let mut af = crate::spatial::SpatialAudioAutomationFrame::default();
        obj.automation.apply(0.5, &mut af);
        let pos = af.position.expect("position curve drives");
        assert!((pos.y - 1.0).abs() < 1e-4, "curved position {pos:?}");

        // Live scene -> config: curves round-trip with the same keyframes.
        let out = scene.to_config();
        let oa = &out.objects[0].automation;
        let pos_key = oa.position.as_ref().expect("position curve");
        assert_eq!(pos_key.points.len(), 2);
        assert!(pos_key.points[1].0 == 1.0 && (pos_key.points[1].1[1] - 2.0).abs() < 1e-6);
        let gain_key = oa.gain.as_ref().expect("gain curve");
        assert_eq!(gain_key.points, vec![(0.0, 1.0), (2.0, 0.0)]);
    }
}
