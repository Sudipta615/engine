//! `AudioFloat` — a minimal sealed trait over `f32` and `f64` that allows
//! the DSP graph to be generic over precision.
//!
//! # Why a custom trait instead of `num_traits::Float`?
//!
//! `num_traits::Float` is large and requires many methods we don't need.
//! A targeted trait gives cleaner bounds, enables `#[inline]` specialization,
//! and keeps the dependency surface minimal.  The trait is **sealed** (via the
//! private `Sealed` marker) so that only `f32` and `f64` can ever implement it.
//!
//! # Usage
//!
//! ```rust
//! use engine::dsp::float::AudioFloat;
//!
//! fn apply_gain<T: AudioFloat>(sample: T, gain: T) -> T {
//!     sample * gain
//! }
//! ```

use std::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

/// Private sealing module — prevents external crates from implementing
/// `AudioFloat` for arbitrary types.
mod private {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

/// A precision-agnostic floating-point scalar for audio DSP.
///
/// Only `f32` and `f64` implement this trait. Code generic over `<T: AudioFloat>`
/// compiles to two concrete monomorphizations: a fast `f32` "Performance" path
/// and a high-precision `f64` "Quality" path.
pub trait AudioFloat:
    private::Sealed
    + Copy
    + Clone
    + PartialOrd
    + PartialEq
    + std::fmt::Debug
    + Add<Output = Self>
    + AddAssign
    + Sub<Output = Self>
    + SubAssign
    + Mul<Output = Self>
    + MulAssign
    + Div<Output = Self>
    + Neg<Output = Self>
    + Send
    + Sync
    + 'static
{
    fn zero() -> Self;
    fn one() -> Self;
    fn from_f64(v: f64) -> Self;
    fn from_f32(v: f32) -> Self;
    fn to_f32(self) -> f32;
    fn to_f64(self) -> f64;
    fn abs(self) -> Self;
    fn sqrt(self) -> Self;
    fn exp(self) -> Self;
    fn ln(self) -> Self;
    fn log10(self) -> Self;
    fn powf(self, exp: Self) -> Self;
    fn sin(self) -> Self;
    fn cos(self) -> Self;
    fn is_finite(self) -> bool;
    fn is_nan(self) -> bool;
    fn clamp(self, lo: Self, hi: Self) -> Self;
    fn signum(self) -> Self;
    fn max(self, other: Self) -> Self;
    fn min(self, other: Self) -> Self;
    /// Flush denormals to zero. Critical for IIR filter stability.
    fn flush_denormal(self) -> Self;
    fn pi() -> Self;
}

impl AudioFloat for f32 {
    #[inline]
    fn zero() -> Self {
        0.0
    }
    #[inline]
    fn one() -> Self {
        1.0
    }
    #[inline]
    fn from_f64(v: f64) -> Self {
        v as f32
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        v
    }
    #[inline]
    fn to_f32(self) -> f32 {
        self
    }
    #[inline]
    fn to_f64(self) -> f64 {
        self as f64
    }
    #[inline]
    fn abs(self) -> Self {
        f32::abs(self)
    }
    #[inline]
    fn sqrt(self) -> Self {
        f32::sqrt(self)
    }
    #[inline]
    fn exp(self) -> Self {
        f32::exp(self)
    }
    #[inline]
    fn ln(self) -> Self {
        f32::ln(self)
    }
    #[inline]
    fn log10(self) -> Self {
        f32::log10(self)
    }
    #[inline]
    fn powf(self, exp: Self) -> Self {
        f32::powf(self, exp)
    }
    #[inline]
    fn sin(self) -> Self {
        f32::sin(self)
    }
    #[inline]
    fn cos(self) -> Self {
        f32::cos(self)
    }
    #[inline]
    fn is_finite(self) -> bool {
        f32::is_finite(self)
    }
    #[inline]
    fn is_nan(self) -> bool {
        f32::is_nan(self)
    }
    #[inline]
    fn clamp(self, lo: Self, hi: Self) -> Self {
        f32::clamp(self, lo, hi)
    }
    #[inline]
    fn signum(self) -> Self {
        f32::signum(self)
    }
    #[inline]
    fn max(self, other: Self) -> Self {
        f32::max(self, other)
    }
    #[inline]
    fn min(self, other: Self) -> Self {
        f32::min(self, other)
    }
    #[inline]
    fn flush_denormal(self) -> Self {
        crate::buffer::flush_denormal(self)
    }
    #[inline]
    fn pi() -> Self {
        std::f32::consts::PI
    }
}

impl AudioFloat for f64 {
    #[inline]
    fn zero() -> Self {
        0.0
    }
    #[inline]
    fn one() -> Self {
        1.0
    }
    #[inline]
    fn from_f64(v: f64) -> Self {
        v
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        v as f64
    }
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32
    }
    #[inline]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline]
    fn abs(self) -> Self {
        f64::abs(self)
    }
    #[inline]
    fn sqrt(self) -> Self {
        f64::sqrt(self)
    }
    #[inline]
    fn exp(self) -> Self {
        f64::exp(self)
    }
    #[inline]
    fn ln(self) -> Self {
        f64::ln(self)
    }
    #[inline]
    fn log10(self) -> Self {
        f64::log10(self)
    }
    #[inline]
    fn powf(self, exp: Self) -> Self {
        f64::powf(self, exp)
    }
    #[inline]
    fn sin(self) -> Self {
        f64::sin(self)
    }
    #[inline]
    fn cos(self) -> Self {
        f64::cos(self)
    }
    #[inline]
    fn is_finite(self) -> bool {
        f64::is_finite(self)
    }
    #[inline]
    fn is_nan(self) -> bool {
        f64::is_nan(self)
    }
    #[inline]
    fn clamp(self, lo: Self, hi: Self) -> Self {
        f64::clamp(self, lo, hi)
    }
    #[inline]
    fn signum(self) -> Self {
        f64::signum(self)
    }
    #[inline]
    fn max(self, other: Self) -> Self {
        f64::max(self, other)
    }
    #[inline]
    fn min(self, other: Self) -> Self {
        f64::min(self, other)
    }
    #[inline]
    fn flush_denormal(self) -> Self {
        // flush_denormal_f64 lives in buffer; keep the trait self-contained
        // by using the bit-trick directly.
        let bits = self.to_bits();
        let exp = (bits >> 52) & 0x7FF;
        if exp == 0 {
            0.0
        } else {
            self
        }
    }
    #[inline]
    fn pi() -> Self {
        std::f64::consts::PI
    }
}
