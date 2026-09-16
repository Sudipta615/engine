//! x86 / x86_64 architecture-specific vector kernels (SSE2, AVX2, AVX-512).

pub mod avx2;
pub mod avx512;
pub mod sse2;

pub use avx2::*;
pub use avx512::*;
pub use sse2::*;
