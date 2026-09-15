//! Configurable multi-band parametric equalizer.

mod core;
pub mod dynamic;
#[cfg(test)]
mod tests;
mod types;

pub use core::ParametricEq;
pub use dynamic::{DynamicEq, DynamicEqBand, DynamicEqBandParams, MAX_DYNAMIC_EQ_BANDS};
pub use types::{EqBandParams, EqFilterType, MAX_EQ_BANDS, NUM_EQ_BANDS};
