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
//! This phase ships the **equal-power [`BasicPanner`]** and the full scene /
//! speaker / listener / object data model. The conventional PCM & DSP path is
//! untouched; spatial rendering is opt-in.
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
//! - `render.rs` — [`SpatialRenderer`] trait, [`RenderError`],
//!   [`RendererKind`] (§22, §106).
//! - `panner.rs` — [`BasicPanner`] (§24–30, §46, §56–57).
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
//! - **Equal-power law**: `la² + lb² = 1` across a bracketing speaker pair.
//!
//! ## Room / beds / fields / binaural — future seams
//!
//! Beds, diffuse fields, the room, Ambisonics/HOA, HRTF/binaural, head
//! tracking and the scene file format are explicitly **not** part of this
//! phase (per the spec's dependency order, Part XXVI); the data model is
//! shaped so they slot in without redesign (§136).
//!
//! The spatial layer contains **no** Dolby/DTS codecs, bitstreams, metadata,
//! or trademarks — it is an independent implementation (§3, §115).

pub mod level;
pub mod math;
pub mod object;
pub mod panner;
pub mod render;
pub mod scene;
pub mod speaker;

pub use level::{AirAbsorption, DistanceModel};
pub use math::{Quat, Vec3};
pub use object::{
    ObjectAudioRef, ObjectId, SpatialAudioObject, SpatialObjectStore, SpatialSourceType,
    MAX_SPATIAL_OBJECTS,
};
pub use panner::BasicPanner;
pub use render::{RenderError, RendererKind, SpatialRenderer};
pub use scene::{Listener, ListenerTransform, SpatialScene};
pub use speaker::{LayoutCalibration, Speaker, SpeakerId, SpeakerLayout};
