//! High-quality audio resampler using rubato.

mod core;
#[cfg(test)]
mod tests;
mod types;

pub use core::{AudioResampler, AudioResamplerF32, AudioResamplerF64, GenericResampler};
pub use types::ResamplerError;
pub use types::MAX_OUTPUT_BUFFER_FRAMES;

/// Common interface for components that introduce and report audio latency.
pub trait LatencyProvider {
    /// Latency in samples at the specified sample rate.
    fn latency_samples(&self, sample_rate: f64) -> f64;
}
