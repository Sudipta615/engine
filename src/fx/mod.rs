//! # Dedicated Creative Sound-Design DSP Layer (`fx`) — (Item 32).
//!
//! Separates artistic and creative sound-design effects from the low-level
//! playback and routing core, adhering to the guide's architectural layering:
//! `core/` | `dsp/` | `spatial/` | `fx/`.
//!
//! ### Processors
//! - **Time-Domain (`delay`)**:
//!   - [`CombFilter`]: Feedforward and feedback comb filter with acoustic damping.
//!   - [`PingPongDelay`]: Stereo cross-feedback delay with damping and balance control.
//! - **Modulation (`modulation`)**:
//!   - [`Chorus`]: Multi-voice modulated delay lines with quadrature LFOs.
//!   - [`Flanger`]: Short delay modulated comb with through-zero capability.
//!   - [`Phaser`]: Multi-pole allpass filter ladder with feedback and notch sweeping.
//!   - [`RingModulator`]: 4-quadrant ring modulator and amplitude modulator.
//! - **Nonlinear (`distortion`)**:
//!   - [`Saturator`]: Soft clipping (tanh, atan, cubic), hard clip, tape saturation,
//!     tube warmth with even harmonics, and wavefolding.
//!
//! All processors operate with **100% zero heap allocations** on the audio callback.

pub mod delay;
pub mod distortion;
pub mod modulation;

pub use delay::{CombFilter, PingPongDelay};
pub use distortion::{DistortionType, Saturator};
pub use modulation::{Chorus, Flanger, Phaser, RingModulator};
