//! # Acoustic World (v3.25) + Baking (v3.26) — simulation layer
//!
//! The guide's core directive for v3.25: **separate acoustic simulation from
//! acoustic rendering.** This module is the simulation side. It turns a
//! geometric description of a space (walls with frequency-dependent
//! materials, openings/portals, diffraction edges) into a concrete set of
//! acoustic propagation paths ([`AcousticPath`]) that a renderer consumes.
//! v3.26 makes the simulation pay for itself: [`AcousticBaker`] caches the
//! solved path set for static source positions into a position-dependent
//! [`BakedScene`] response cache, which the renderers look up at audio time.
//!
//! ```text
//! AcousticWorld (geometry + materials + portals + edges)
//!         |   solve(source, listener)
//!         v
//!     [AcousticPath; N]   ── direct / reflected / diffracted / transmitted
//!         |   AcousticBaker::bake (control path, run-once)
//!         v
//!   BakedScene (position → BakedObject)
//!         |   listener_images() → [ListenerImage; MAX_IMAGES]
//!         v
//!    (renderers: binaural, pan, VBAP — set_baked)
//! ```
//!
//! ## What lives here
//!
//! - [`material`] — [`MaterialSpectrum`]: per-octave-band absorption /
//!   reflection / transmission, plus the named presets
//!   ([`MaterialKind`]) the guide's Direction 8 calls for.
//! - [`geometry`] — [`AcousticRoom`] (axis-aligned box with per-wall
//!   materials), [`Portal`] (openings coupling spaces), [`DiffractionEdge`]
//!   (fins/mullions sound bends around).
//! - [`path`] — [`AcousticPath`], [`PathKind`], [`PathFlags`]: the simulation→
//!   render contract.
//! - [`solver`] — [`AcousticWorld`]: owns the geometry and produces the
//!   path set (direct + image-source reflections + wedge diffraction +
//!   portal transmission).
//! - [`bake`] — [`AcousticBaker`] / [`BakedScene`] / [`BakedObject`] /
//!   [`BakedPath`]: the position-dependent response cache and its
//!   renderer-facing [`BakedScene::listener_images`] bridge.
//!
//! ## Offline / control discipline
//!
//! Everything in this module runs on the control or offline path: it
//! computes propagation for baking, measurement, and reference rendering and
//! is heap-happy by design (baking *is* the expensive work being cached).
//! The realtime renderers consume only fixed-size [`BakedPath`]s copied
//! into a pre-allocated `[ListenerImage; MAX_IMAGES]` — no solving, no
//! allocation, no locks.

pub mod bake;
pub mod geometry;
pub mod material;
pub mod path;
pub mod solver;

pub use bake::{
    spectral_taps, AcousticBaker, BakePolicy, BakedObject, BakedPath, BakedScene, ACOUSTIC_IR_LEN,
    DEFAULT_BAKE_CELL_M,
};
pub use geometry::{
    portal_diffraction_edges, AcousticRoom, DiffractionEdge, Portal, Wall, WallSurface, ALL_WALLS,
};
pub use material::{
    surface_lowpass_hz, MaterialKind, MaterialSpectrum, OCTAVE_BANDS, OCTAVE_BANDS_HZ,
};
pub use path::{AcousticPath, PathFlags, PathKind};
pub use solver::{
    diffract_around_edge, wall_index, AcousticWorld, MAX_PATHS, MAX_REFLECTION_ORDER,
};
