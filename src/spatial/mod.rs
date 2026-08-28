//! # Spatial Audio — speaker-independent object rendering (spec Parts I–V)
//!
//! An independent, open spatial-audio layer layered **above** the engine's
//! conventional multichannel core. It follows the spec's central rule:
//!
//! > **Channels describe the output reproduction system; spatial objects and
//! > fields describe the content.**
//!
//! A spatial scene ([`SpatialScene`]) is authored in world space with a
//! listener and a set of objects, **independently of the output speaker
//! count**. A renderer ([`SpatialRenderer`]) then places that scene on
//! whatever layout is active — stereo, 5.1, 7.1, 7.1.4, or a custom array —
//! and writes a normal interleaved multichannel PCM buffer the engine's
//! existing output core can deliver.
//!
//! This layer ships the full scene / speaker / listener / object data model,
//! four renderers — the **equal-power [`BasicPanner`]**, the 3D
//! **VBAP-style [`VbapRenderer`]** (speaker-geometry triangulation, 2D
//! reduction for coplanar layouts, out-of-coverage fallback), the
//! **ambisonic path** ([`AmbisonicRenderer`]: a documented FOA bus — ACN /
//! SN3D — encoded by any spatial source and decoded onto any layout, Part
//! VI), and the **binaural renderer** ([`BinauralRenderer`]: a head model —
//! Woodworth ITD + Duda-Martens head shadow, Part VII) that renders the
//! whole hybrid scene straight to headphones — object behavior
//! (**directivity** [`Directivity`], **occlusion** [`Occlusion`],
//! **angular-region spread** [`spread`]), and the three content classes
//! mixed by the hybrid renderer: **objects** ([`SpatialAudioObject`]), **beds**
//! ([`SpatialBed`], channel-based), and **fields** ([`SpatialField`],
//! diffuse — encoded into the ambisonic bus and decoded with the `√N`
//! diffuse compensation). The conventional PCM & DSP path is untouched;
//! spatial rendering is opt-in.
//!
//! ## Module map
//!
//! - `math.rs` — [`Vec3`], [`Quat`] and the single documented coordinate
//!   system (§17–18).
//! - `level.rs` — [`DistanceModel`], [`AirAbsorption`] (level laws, §38–39).
//! - `speaker.rs` — [`Speaker`], [`SpeakerLayout`] (named presets + custom),
//!   [`LayoutCalibration`] (§19–20).
//! - `object.rs` — [`SpatialAudioObject`], [`ObjectAudioRef`] (sharable
//!   source), [`SpatialObjectStore`], [`SpatialSourceType`] (§13–15, §31).
//! - `scene.rs` — [`SpatialScene`], [`Listener`], [`ListenerTransform`]
//!   (§12, §16, §48).
//! - `directivity.rs` — [`Directivity`], [`CustomDirectivity`], and the
//!   shared `listener_angle_rad` transform (§41).
//! - `occlusion.rs` — [`Occlusion`], [`AcousticTransmission`], per-object
//!   low-pass state (§43–44).
//! - `spread.rs` — angular-region spread sampling + energy-normalized
//!   aggregation (§29–30).
//! - `bed.rs` — [`SpatialBed`] (channel-based content) routed by semantic
//!   role, [`SpatialBedStore`] (§13.1).
//! - `field.rs` — [`SpatialField`] (diffuse content) encoded into the
//!   ambisonic bus + decoded with `√N` compensation, per-speaker
//!   decorrelation ([`AmbisonicFieldMixer`], §13.3, §33).
//! - `ambisonic.rs` — the FOA core: [`sh_foa`] SH basis, plane-wave
//!   encoder, order-1 bus rotation, [`DecoderPolicy`] (Basic / Max-rE),
//!   [`AmbisonicDecoder`], and the [`AmbisonicRenderer`] (Part VI §32–37,
//!   §55).
//! - `render.rs` — [`SpatialRenderer`] trait (incl. [`HybridBlockInputs`] /
//!   `process_hybrid_block`), [`RenderError`], [`RendererKind`] (§22, §37,
//!   §106).
//! - `room.rs` — [`Room`] (spec §49): image-source early reflections
//!   ([`EarlyReflections`], per-object delay rings + tap smoothing + the
//!   binaural ring primitives) and the [`RoomLateField`] Schroeder tail
//!   whose output encodes into the ambisonic bus (§55); occlusion's
//!   `AcousticTransmission` is the transmission seam (§43–44).
//! - `hrtf.rs` — the binaural head model ([`hrtf`]): Woodworth ITD,
//!   Duda-Martens head-shadow shelf, fractional-delay ring read (§47–48,
//!   §62).
//! - `binaural.rs` — [`BinauralRenderer`]: the whole hybrid scene through
//!   the head model, with a virtual 8-speaker ring for diffuse content
//!   (Part VII).
//! - `tracking.rs` — head tracking ([`HeadTracker`], [`HeadSample`],
//!   [`TrackingConfig`]): interpolates and smooths a stream of IMU/VR
//!   orientation samples into the listener's orientation at block rate —
//!   the VR/AR seam (§48, §136).
//! - `panner.rs` — [`BasicPanner`] (§24–30, §46, §56–57).
//! - `vbap.rs` — [`VbapRenderer`]: 3-triplet VBAP, Delaunay region
//!   preprocessor, out-of-coverage fallback (§21, §25–29).
//!
//! ## Conventions (documented, spec §18 & §153)
//!
//! - **Position**: metres, world/listener space.
//! - **Coordinate frame**: `+X = right, +Y = front, +Z = up`; azimuth `0` =
//!   front, `+π/2` = right; elevation `0` = horizon, `+π/2` = up (see
//!   [`math`]).
//! - **Angles**: radians internally, degrees at API boundaries.
//! - **Gain**: linear (1.0 = unity); dB only at the trim/calibration
//!   boundary ([`LayoutCalibration`]).
//! - **LFE**: an effects path, never a spatial pan target (spec Part X).
//! - **Directivity angle**: 0 = the source faces the listener, π = facing
//!   away (spec §41; see [`directivity::listener_angle_rad`]).
//! - **Equal-power law**: `la² + lb² = 1` across a bracketing speaker pair
//!   ([`BasicPanner`]); VBAP energy-normalizes solved triplet gains instead
//!   ([`VbapRenderer`], §29).
//!
//! ## Binaural rendering
//!
//! The binaural path (Part VII) renders the full hybrid scene — objects,
//! beds, fields, and the room's reflections — to two ears through the
//! documented head model: Woodworth ITD + a Duda-Martens head-shadow shelf
//! ([`hrtf`]), with diffuse content decoded onto a virtual 8-speaker ring
//! before the head model. It is azimuth-only by design (no elevation
//! spectral cues yet — the documented HRTF seam, §136); mirror symmetry is
//! the exact invariant, not constant power (the head diffracts).
//!
//! Head tracking is a live seam ([`HeadTracker`]): the listener's
//! orientation is a first-class scene transform, so a host feeds IMU/VR
//! samples and applies the smoothed result to the listener before each
//! render block — the renderers never change (spec §48, §136). Still
//! future: higher-order Ambisonics (the order-1 basis + per-order decoder
//! weights are the documented §34 extension), full spectral HRTFs, and the
//! scene file format (per the spec's dependency order, Part XXVI); the
//! data model is shaped so they slot in without redesign (§136).
//!
//! The spatial layer contains **no** Dolby/DTS codecs, bitstreams, metadata,
//! or trademarks — it is an independent implementation (§3, §115).

pub mod ambisonic;
pub mod bed;
pub mod binaural;
pub mod directivity;
pub mod field;
pub mod hrtf;
pub mod level;
pub mod math;
pub mod object;
pub mod occlusion;
pub mod panner;
pub mod render;
pub mod room;
pub mod scene;
pub mod speaker;
pub mod spread;
pub mod tracking;
pub mod vbap;

pub use ambisonic::{
    encode_plane_wave, rotate_bus_frame, sh_foa, AmbisonicDecoder, AmbisonicRenderer, DecoderPolicy,
};
pub use bed::{BedId, SpatialBed, SpatialBedStore, MAX_BEDS};
pub use binaural::{BinauralRenderer, VIRTUAL_RING_SPEAKERS};
pub use directivity::{CustomDirectivity, Directivity};
pub use field::{FieldId, SpatialField, SpatialFieldStore, MAX_FIELDS};
pub use hrtf::{
    ear_delay_sec, head_shadow_alpha, max_itd_sec, woodworth_itd_sec, Ear, HeadShadow,
    DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND,
};
pub use level::{AirAbsorption, DistanceModel};
pub use math::{Quat, Vec3};
pub use object::{
    ObjectAudioRef, ObjectId, SpatialAudioObject, SpatialObjectStore, SpatialSourceType,
    MAX_SPATIAL_OBJECTS,
};
pub use occlusion::{AcousticTransmission, Occlusion};
pub use panner::BasicPanner;
pub use render::{HybridBlockInputs, RenderError, RendererKind, SpatialRenderer, VbapRenderer};
pub use room::{EarlyReflections, Room, RoomLateField};
pub use scene::{Listener, ListenerTransform, SpatialScene};
pub use speaker::{LayoutCalibration, Speaker, SpeakerId, SpeakerLayout};
pub use tracking::{HeadSample, HeadTracker, TrackingConfig};
