//! Configurable multi-band parametric equalizer.

mod core;
#[cfg(test)]
mod tests;
mod types;

pub use core::ParametricEq;
pub use types::{EqBandParams, EqFilterType, MAX_EQ_BANDS, NUM_EQ_BANDS};
