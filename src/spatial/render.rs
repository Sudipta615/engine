//! Renderer abstraction (spec Part IV §22, Part XX §106).
//!
//! A spatial renderer takes a [`crate::spatial::SpatialScene`] plus the
//! per-object audio for one block and writes a normal interleaved
//! multichannel PCM buffer that can be pushed through the engine's existing
//! output core (`SampleSink::push_frames_interleaved`). The renderer never
//! owns a low-level audio-device backend (spec §22).
//!
//! Realtime discipline (spec §71–75): all static geometry is preprocessed in
//! `prepare`; `process_block` is allocation-free and lock-free after that.
//! To make steady-state output genuinely allocation-free, the renderer
//! writes into a **caller-supplied** interleaved buffer (`out`) rather than
//! returning an owned buffer — the caller (engine/decode loop/tests) owns
//! and reuses it, exactly like the engine's
//! `DspPipeline::process_block_multichannel(&mut interleaved, ch)`.

use super::speaker::{RenderGeometryError, SpeakerLayout};
use super::SpatialScene;

pub use super::ambisonic::AmbisonicRenderer;
pub use super::binaural::BinauralRenderer;
pub use super::panner::BasicPanner;
pub use super::vbap::VbapRenderer;

/// Which renderer a host selects. `Basic` is the equal-power panner, `Vbap`
/// the VBAP-style object renderer, `Ambisonic` the FOA-bus decoder, and
/// `Binaural` the head-model renderer (spec §23, §47–48, §136). The enum is
/// non-exhaustive so renderers can be added without breaking host matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RendererKind {
    Basic,
    Vbap,
    Ambisonic,
    Binaural,
}

/// Typed render error (spec §106). Invalid geometry must surface as an error
/// here — never as NaN/Inf in the audio pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("invalid speaker layout")]
    InvalidLayout,
    #[error("degenerate speaker geometry")]
    DegenerateGeometry,
    #[error("buffer/channel mismatch: expected {expected} samples, got {got}")]
    BufferMismatch { expected: usize, got: usize },
    #[error("unsupported configuration for this renderer")]
    UnsupportedConfiguration,
    #[error("invalid spatial scene")]
    InvalidScene,
    #[error("capacity exceeded")]
    CapacityExceeded,
    #[error("HRTF unavailable")]
    HrtfUnavailable,
    #[error("internal render failure")]
    InternalFailure,
}

impl From<RenderGeometryError> for RenderError {
    fn from(e: RenderGeometryError) -> Self {
        match e {
            RenderGeometryError::InvalidLayout => RenderError::InvalidLayout,
            RenderGeometryError::DegenerateGeometry => RenderError::DegenerateGeometry,
        }
    }
}

/// Per-block input planes for the hybrid path (spec §37): object audio,
/// bed audio, and field audio. Plane-count conventions:
///
/// - `objects`: one mono plane per **enabled** object in store order.
/// - `beds`: bed-major — `ordinal × bed_channels + c` is enabled bed
///   `ordinal`'s channel `c` (channels follow the bed's authored
///   [`ChannelLayout`](crate::decode::ChannelLayout)).
/// - `fields`: one mono plane per **enabled** field in store order.
///
/// Callers with no audio for a class pass an empty slice; the renderer
/// writes silence for that class.
#[derive(Debug, Clone, Copy, Default)]
pub struct HybridBlockInputs<'a> {
    pub objects: &'a [&'a [f32]],
    pub beds: &'a [&'a [f32]],
    pub fields: &'a [&'a [f32]],
}

/// A spatial renderer. Implementations must be realtime-safe after `prepare`
/// and must never allocate/lock in `process_block`/`process_hybrid_block`.
pub trait SpatialRenderer: Send {
    /// Preprocess static speaker geometry and allocate persistent scratch.
    /// Control-path only — never called from the audio thread (spec §74).
    fn prepare(&mut self, layout: &SpeakerLayout, sample_rate: u32) -> Result<(), RenderError>;

    /// Render one block of object audio into a caller-supplied interleaved
    /// buffer.
    ///
    /// - `object_inputs`: one mono plane per **enabled** object in store
    ///   order (a caller with no decoded audio passes an empty slice — the
    ///   renderer still runs geometry and writes silence, useful for pure
    ///   coefficient/symmetry tests).
    /// - `frames`: number of frames in each input plane.
    /// - `out`: interleaved output, must have `>= frames × channels`
    ///   capacity where `channels` was the speaker count at `prepare` time.
    ///
    /// On success `out[..frames × channels]` is filled. Writing silence for
    /// missing inputs is permitted (no NaN).
    fn process_block(
        &mut self,
        scene: &SpatialScene,
        object_inputs: &[&[f32]],
        frames: usize,
        out: &mut [f32],
    ) -> Result<(), RenderError>;

    /// Render a full hybrid block — objects, beds, and fields together into
    /// one interleaved buffer (spec §37's spatial mixer).
    ///
    /// The default implementation forwards to [`Self::process_block`] with
    /// the object planes (beds/fields ignored), so renderers that predate
    /// beds/fields keep working unchanged; renderers that support them
    /// override this method.
    fn process_hybrid_block(
        &mut self,
        scene: &SpatialScene,
        inputs: &HybridBlockInputs<'_>,
        frames: usize,
        out: &mut [f32],
    ) -> Result<(), RenderError> {
        self.process_block(scene, inputs.objects, frames, out)
    }
}

/// Direction/distance helper shared by renderers: split a listener-space
/// vector into a unit direction and a distance. Never NaNs.
pub fn to_direction_and_distance(v: super::math::Vec3) -> (Option<super::math::Vec3>, f32) {
    let dist = v.length();
    (v.normalized(), dist)
}
