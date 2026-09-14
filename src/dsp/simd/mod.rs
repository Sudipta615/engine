//! Modular SIMD/vectorization strategy for hot DSP loops.
//!
//! Provides architecture-accelerated kernels (SSE2 on x86_64, NEON on aarch64)
//! with exact, deterministic scalar fallbacks across all target platforms.
//! All kernels are zero-allocation, lock-free, and real-time safe.

pub mod biquad;
pub mod gain;
pub mod interpolate;
pub mod limiter;
pub mod mix;

pub use biquad::process_biquad_stereo;
pub use gain::{ramp_slice, ramp_slice_f64, scale_slice, scale_slice_f64};
pub use interpolate::{vector_bilinear, vector_lerp};
pub use limiter::{vector_abs_max, vector_abs_max_f64};
pub use mix::{mix_crossfade, mix_slices, mix_slices_f64};
