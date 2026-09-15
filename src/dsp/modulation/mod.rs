//! Unified Modulation System (Item 31).
//!
//! Provides modular, realtime-safe parameter modulation:
//! - [`Lfo`]: Multi-waveform, free-running or tempo-synced LFO.
//! - [`AdsrEnvelope`]: Sample-accurate attack, decay, sustain, release envelope generator.
//! - [`EnvelopeFollower`]: Audio signal dynamic envelope tracker.
//! - [`ModulationMatrix`]: Routing matrix from modulation sources to target parameter slots.
//!
//! All modulation processors guarantee zero heap allocations during block evaluation.

pub mod envelope;
pub mod follower;
pub mod lfo;
pub mod matrix;

pub use envelope::{AdsrEnvelope, EnvelopeStage};
pub use follower::{EnvelopeFollower, FollowerMode};
pub use lfo::{Lfo, LfoSyncRate, LfoWaveform};
pub use matrix::{ModRoute, ModSource, ModulationMatrix};
