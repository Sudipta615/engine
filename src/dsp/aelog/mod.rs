//! # Aelog — deterministic event recording & replay (v3.29)
//!
//! The guide's Direction 17, made concrete for the offline stack: a
//! **replayable log** (`recording.aelog`) of a render session — every
//! timeline mutation (schedules, tempo, transport, looping, quantization)
//! and every block advance — that can be re-executed to reproduce identical
//! output. Aelog is the project's **golden-render substrate**: replaying a
//! recorded session against the same graph yields byte-identical audio, so a
//! bug becomes *"at event 18 392 the gain changed"* instead of *"sometimes
//! it sounds wrong"*.
//!
//! ```text
//! AelogRecorder (wraps a Timeline, logs every mutation)
//!         │  finish() → Aelog { header, commands }
//!         ▼
//!   recording.aelog  (versioned JSON; save_json / load_json)
//!         │
//!         ▼
//! replay_events(log)          → identical fired-event stream
//! replay_render(log, graph)   → byte-identical captured audio (golden render)
//! ```
//!
//! Determinism contract: the log stores the *commands*, not their outcomes.
//! Replaying applies them in order against a fresh [`Timeline`], so the
//! fired events and end clock state are pure functions of the log. Two
//! identical recorder sessions serialize to byte-equal logs.
//!
//! ## Module map
//!
//! - this module — [`SessionHeader`], [`RecordedCommand`], [`Aelog`]
//! - `record.rs` — [`AelogRecorder`]
//! - `replay.rs` — [`replay_events`], [`replay_render`], [`ReplayOutcome`]
//! - `cache.rs` — [`AelogCache`], [`log_hash`], [`graph_fingerprint`]
//!
//! ## Discipline
//!
//! Recording, replay, and caching are control/offline-path by design (file
//! IO, JSON, and graph rendering are the expensive work being cached);
//! nothing here touches a realtime audio thread.

pub mod cache;
pub mod record;
pub mod replay;

pub use cache::{content_address, graph_fingerprint, log_hash, render_cached, AelogCache};
pub use record::AelogRecorder;
pub use replay::{replay_events, replay_render, ReplayError, ReplayOutcome};

use crate::dsp::timeline::{
    CurveBeats, EventPayload, EventTime, Quantize, TempoMap, TransportState,
};
use crate::spatial::acoustic::bake::BakedScene;
use crate::spatial::math::Vec3;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The aelog format version. Bump on any breaking layout change; the
/// `version` field lets a future loader reject old files explicitly.
/// v2: `InputAudio` gained a `clip` address (v3.35). v3 (v3.36): chunks
/// became channel-major planes (`Vec<Vec<f32>>`), so stereo/spatial
/// sessions record and replay every channel. v3 remains current for the
/// *additive* `SetBakedScene` variant (v3.37) — old v3 files still load;
/// the variant simply never appears in them.
pub const AELOG_VERSION: u32 = 3;

/// Session metadata. Deliberately free of wall-clock timestamps — a log
/// must be a pure function of its commands to be reproducible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub format_version: u32,
    /// The sample rate every sample/beat conversion used.
    pub sample_rate: f32,
    /// The block size the recorded session advanced in.
    pub block_frames: u64,
    /// A host-provided label (song name, take id, …).
    pub label: String,
}

impl SessionHeader {
    pub fn new(sample_rate: f32, block_frames: u64) -> Self {
        Self {
            format_version: AELOG_VERSION,
            sample_rate,
            block_frames,
            label: String::new(),
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

/// One recorded step of a render session. Commands are applied **in order**
/// during replay; every command is a timeline mutation plus the per-block
/// advance (block processing itself is implied by `Advance` — the graph is
/// supplied at replay time).
///
/// `SetBakedScene` embeds a whole [`BakedScene`] verbatim, making this
/// enum large by design — a log is a file format, not a hot-path value,
/// so the size difference between variants is intentional (the same
/// allowance the production graph uses for its payload enums).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RecordedCommand {
    /// Schedule an event (resolved to a master sample at schedule time).
    Schedule {
        time: EventTime,
        payload: EventPayload,
    },
    SetTempo(f32),
    SetTimeSignature(f32),
    SetLoop {
        start: u64,
        end: u64,
    },
    SetLoopEnabled(bool),
    SetTempoRamp {
        target: f32,
        duration_samples: f32,
    },
    SetState(TransportState, u64),
    SetQuantize(Quantize),
    /// Advance the clock `samples` and fire due events; the render driver
    /// feeds one block of that size to the graph.
    Advance(u64),
    /// A chunk of the session's **audio input** (what was fed into the
    /// graph's `Buffer` source that block). Chunks concatenate in order to
    /// reconstruct the full track; each chunk is **channel-major planes**
    /// (`chunk[0]` = channel 0, …), so stereo/spatial inputs replay
    /// exactly. `clip` addresses a specific `Buffer` node (a multi-input
    /// session records one chunk stream per clip and each is routed only
    /// to the nodes bearing that address); `None` is the unaddressed
    /// single-track path.
    InputAudio {
        clip: Option<String>,
        chunk: Vec<Vec<f32>>,
    },
    /// A **listener motion** sample: at master sample `at`, the listener
    /// position becomes `position` (applies from `at` onward).
    SetListenerPosition {
        at: u64,
        position: Vec3,
    },
    /// An **acoustic world swap**: at master sample `at` the baked scene
    /// becomes `scene` (applies from `at` onward) — an animated acoustic
    /// world recorded as a snapshot per geometry change. Replay re-attaches
    /// the scene to the executor's `Acoustic` nodes without resetting their
    /// tapped delay lines, so the room keeps ringing through the swap.
    SetBakedScene {
        at: u64,
        scene: BakedScene,
    },
    /// The **tempo map** musical automation evaluates against: beat →
    /// sample conversion across tempo *changes*. Recorded once (idempotent;
    /// the same map is additive & overwritable on replay).
    SetTempoMap(TempoMap),
    /// Drive a Gain node from a **tempo-mapped control curve** (`node` is
    /// the `NodeId.0` value, like `EventPayload::SetGain`): the curve is
    /// authored in beats and evaluated against the recorded tempo map, so
    /// the gain sweeps smoothly over the session.
    SetGainAutomation {
        node: u32,
        curve: CurveBeats,
    },
}

/// A complete recorded render session — the `recording.aelog` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aelog {
    pub header: SessionHeader,
    pub commands: Vec<RecordedCommand>,
}

impl Aelog {
    pub fn new(header: SessionHeader) -> Self {
        Self {
            header,
            commands: Vec::new(),
        }
    }

    /// Serialize to a compact JSON string (deterministic — command order and
    /// the enum layout fully determine the bytes).
    pub fn to_json(&self) -> Result<String, AelogError> {
        serde_json::to_string(self).map_err(AelogError::Serialize)
    }

    /// Parse from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, AelogError> {
        let log: Aelog = serde_json::from_str(json).map_err(AelogError::Deserialize)?;
        log.check_version()?;
        Ok(log)
    }

    /// Write to `path` (offline/control path; a golden render is stored
    /// alongside the audio it produced).
    pub fn save_json(&self, path: impl AsRef<Path>) -> Result<(), AelogError> {
        let json = self.to_json()?;
        std::fs::write(path, json).map_err(AelogError::Io)
    }

    /// Load from `path`.
    pub fn load_json(path: impl AsRef<Path>) -> Result<Self, AelogError> {
        let json = std::fs::read_to_string(path).map_err(AelogError::Io)?;
        Self::from_json(&json)
    }

    /// Reject logs from a different format version.
    pub fn check_version(&self) -> Result<(), AelogError> {
        if self.header.format_version != AELOG_VERSION {
            return Err(AelogError::Version(self.header.format_version));
        }
        Ok(())
    }
}

/// Errors produced by aelog serialization / load.
#[derive(Debug)]
pub enum AelogError {
    Version(u32),
    Serialize(serde_json::Error),
    Deserialize(serde_json::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for AelogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AelogError::Version(v) => write!(f, "unsupported aelog format version {v}"),
            AelogError::Serialize(e) => write!(f, "aelog serialize: {e}"),
            AelogError::Deserialize(e) => write!(f, "aelog deserialize: {e}"),
            AelogError::Io(e) => write!(f, "aelog io: {e}"),
        }
    }
}

impl std::error::Error for AelogError {}
