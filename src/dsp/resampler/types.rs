//! Resampler configuration, error types, and fixed processing limits.

use config::ResamplerQuality;

/// Error type for resampler construction failures.
#[derive(Debug, thiserror::Error)]
pub enum ResamplerError {
    #[error("Failed to create {quality:?} resampler: {reason}")]
    CreationFailed {
        quality: ResamplerQuality,
        reason: String,
    },
    #[error("Invalid sample rate: source={source_rate}, output={output_rate}")]
    InvalidRates {
        source_rate: usize,
        output_rate: usize,
    },
}

/// Number of channels (stereo)
pub(crate) const CHANNELS: usize = 2;

/// Processing chunk size in frames
pub(crate) const CHUNK_SIZE: usize = 1024;

/// Maximum upsample ratio supported. 44100 → 768000 ≈ 17.4×; round up to 20×.
pub(crate) const MAX_RATIO: usize = 20;

/// Maximum output buffer frames: enough for the worst supported ratio.
/// Sized at CHUNK_SIZE × MAX_RATIO plus a filter margin.
///
/// Public so the engine can size its crossfade scratch buffers to the same
/// worst-case expansion: a realtime block of source frames can produce up to
/// this many resampled output frames before the resampler's own output buffer
/// would overflow.
pub const MAX_OUTPUT_BUFFER_FRAMES: usize = CHUNK_SIZE * MAX_RATIO + 512;

/// Maximum consecutive rebuild failures before disabling the resampler
pub(crate) const MAX_REBUILD_FAILURES: u32 = 5;
