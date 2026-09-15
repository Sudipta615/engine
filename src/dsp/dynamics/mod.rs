//! Unified dynamics detector and envelope tracking architecture.
//!
//! Provides reusable envelope detection for compressors, limiters, gates,
//! expanders, dynamic EQ, and de-essers.

pub mod detector;
pub mod envelope;

pub use detector::{
    ChannelLinkMode, DetectionMode, DetectorConfig, DynamicsDetector, SidechainFilter,
};
pub use envelope::BallisticEnvelope;
