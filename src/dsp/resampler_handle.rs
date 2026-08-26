//! Feature-gate-free resampler handle — future-facing design.
//!
//! This is a **design artifact** showing how to eliminate the 62 `#[cfg(feature =
//! "resample")]` branches scattered through the engine hot path. The engine
//! currently does NOT use this type — it still reads/writes the raw
//! `Option<GenericResampler>` on `PlaybackStream` variants directly.
//!
//! # Migration path
//!
//! 1. Replace `Option<GenericResampler>` in `PlaybackStream` with `ResamplerHandle`.
//! 2. Call `handle.process(src, dst)` unconditionally in the decode loop.
//! 3. Delete all `#[cfg(feature = "resample")]` / `#[cfg(not(feature = "resample"))]`
//!    pairs from `decode_loop.rs` and `mod.rs`.
//!
//! # Why this is a pure win
//!
//! - The `cfg` check happens once at construction instead of at every call site.
//! - New features (`simd`, `neon`) can be added behind one enum variant instead
//!   of `#[cfg]` on every function in the hot path.
//! - The passthrough variant is zero-cost: all methods are trivial.

/// A resampler that is either active (feature-gated) or a no-op passthrough.
/// All methods are available unconditionally — no `#[cfg]` in the callers.
#[derive(Default)]
// `Active` carries the full resampler state; `Passthrough` is a zero-cost
// tag — the size gap is the point of the wrapper (see the module docs).
#[allow(clippy::large_enum_variant)]
pub enum ResamplerHandle {
    /// The resampler is active (only possible when the `resample` feature is
    /// enabled). Operations delegate to the inner `GenericResampler`.
    #[cfg(feature = "resample")]
    Active(crate::dsp::resampler::GenericResampler),
    /// No resampler compiled in or the resampler is not needed (rate match).
    #[default]
    Passthrough,
}

impl ResamplerHandle {
    /// Create from an optional `GenericResampler` — the same `Option` the
    /// engine currently uses directly.
    #[cfg(feature = "resample")]
    pub fn from_option(r: Option<crate::dsp::resampler::GenericResampler>) -> Self {
        match r {
            Some(r) => Self::Active(r),
            None => Self::Passthrough,
        }
    }

    #[cfg(not(feature = "resample"))]
    pub fn from_option(_r: Option<()>) -> Self {
        Self::Passthrough
    }

    /// Reset internal state (no-op on passthrough).
    pub fn reset(&mut self) {
        #[cfg(feature = "resample")]
        if let Self::Active(ref mut r) = self {
            r.reset();
        }
    }

    /// Set the speed ratio (no-op on passthrough).
    pub fn set_speed(&mut self, speed: f32) {
        #[cfg(feature = "resample")]
        if let Self::Active(ref mut r) = self {
            r.set_speed(speed);
        }
        #[cfg(not(feature = "resample"))]
        let _ = speed;
    }

    /// The resampler's group delay in milliseconds (0.0 on passthrough).
    pub fn latency_ms(&self) -> f32 {
        #[cfg(feature = "resample")]
        if let Self::Active(ref r) = self {
            return r.latency_ms();
        }
        0.0
    }

    /// Whether the resampler is disabled or absent.
    pub fn is_disabled(&self) -> bool {
        #[cfg(feature = "resample")]
        if let Self::Active(ref r) = self {
            return r.is_disabled();
        }
        true
    }

    /// Set resampler quality (no-op on passthrough).
    pub fn set_quality(&mut self, quality: config::ResamplerQuality) {
        #[cfg(feature = "resample")]
        if let Self::Active(ref mut r) = self {
            r.set_quality(quality);
        }
        #[cfg(not(feature = "resample"))]
        let _ = quality;
    }
}

#[cfg(feature = "resample")]
#[cfg(not(feature = "resample"))]
impl Default for ResamplerHandle {
    fn default() -> Self {
        Self::Passthrough
    }
}
