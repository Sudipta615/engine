//! Modular SIMD/vectorization architecture for hot DSP loops (§8.2, Item 29).
//!
//! Provides a formalized hierarchical vector execution tier across x86_64 and ARM:
//! `AVX-512` → `AVX2/FMA` → `SSE2` → `Scalar`, and `NEON` on aarch64.
//! All kernels are zero-allocation, lock-free, and real-time safe, with guaranteed
//! bit-exact / numerical equivalent fallback across every execution level.

#[cfg(target_arch = "aarch64")]
pub mod arm;
pub mod biquad;
pub mod dispatch;
pub mod gain;
pub mod interpolate;
pub mod levels;
pub mod limiter;
pub mod mix;
pub mod scalar;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub mod x86;

pub use biquad::process_biquad_stereo;
pub use dispatch::{
    dispatch_accumulate_scaled, dispatch_dot_product, dispatch_mix_slices, dispatch_scale_slice,
    dispatch_scale_slice_f64, execute_accumulate_scaled_at_level, execute_at_level,
    execute_dot_product_at_level, execute_mix_at_level, execute_scale_at_level,
    execute_scale_f64_at_level,
};
pub use gain::{ramp_slice, ramp_slice_f64, scale_slice, scale_slice_f64};
pub use interpolate::{vector_bilinear, vector_lerp};
pub use levels::SimdLevel;
pub use limiter::{vector_abs_max, vector_abs_max_f64};
pub use mix::{mix_crossfade, mix_slices, mix_slices_f64};
pub use scalar::dot_product;
