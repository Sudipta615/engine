//! Timestamped events (v3.28) — Direction 5's move "beyond control messages
//! toward timestamped events".
//!
//! A [`ScheduledEvent`] carries a resolution time (a raw sample, or a
//! musical beat) and a payload. The [`Timeline`](super::Timeline) resolves
//! every event to an absolute **master sample** at schedule time and fires
//! it once, sample-accurately, as the clock crosses it — Direction 5's
//!
//! ```text
//! sample 812340:  object 4 -> position (2,1,-3)
//! sample 812768:  object 4 -> gain -3 dB
//! ```
//!
//! realised for the offline path. Payloads are deliberately small and
//! domain-neutral enough for any host to interpret: a gain step typed to a
//! Graph 2.0 node (`.0` is the `NodeId` under `crate::dsp::graph2`), a
//! bare trigger, and an opaque host tag.

use super::clock::AudioClock;

/// Unique identifier of a scheduled event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventId(pub u64);

/// When an event should fire — before [`Timeline::schedule`] resolves it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EventTime {
    /// An absolute sample position on the monotonic master timeline.
    Sample(u64),
    /// A musical beat position (converted to a master sample at schedule
    /// time using the current clock tempo).
    Beat(f64),
}

/// What a scheduled event does when it fires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EventPayload {
    /// Set a Graph 2.0 node's gain. `node` is the `NodeId.0` value under
    /// `crate::dsp::graph2::NodeId`, kept as a plain number so the timeline
    /// stays free of a graph dependency.
    SetGain { node: u32, gain: f32 },
    /// A bare "fire now" marker with an identifying tag.
    Trigger { tag: u64 },
    /// An opaque host tag for arbitrary hosts to interpret.
    Host(u64),
}

/// A resolved, schedule-ready event. `at` is the absolute master sample the
/// event fires on (already converted from `time`); the scheduler stores and
/// returns events with `at` filled so a driver can apply a gain step at the
/// exact in-block index.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScheduledEvent {
    pub id: EventId,
    /// Absolute master sample on which this event fires.
    pub at: u64,
    /// The original scheduling time.
    pub time: EventTime,
    pub payload: EventPayload,
}

impl ScheduledEvent {
    /// The block-relative index of `at` within a block of `block_frames`,
    /// for drivers that apply events mid-block (sample-accurate).
    pub fn local_index(&self, block_frames: u64) -> usize {
        (self.at % block_frames) as usize
    }
}

/// Errors when scheduling or resolving an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventError {
    /// A beat event cannot be resolved (no valid tempo / sample rate).
    BeatUnresolvable,
    /// The event time is in the past (at or before the current master
    /// position); events can only be scheduled in the future.
    AlreadyPast { at: u64, master: u64 },
}

impl std::fmt::Display for EventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventError::BeatUnresolvable => write!(f, "beat is not resolvable to a sample"),
            EventError::AlreadyPast { at, master } => {
                write!(f, "event at sample {at} is already past (master {master})")
            }
        }
    }
}

impl std::error::Error for EventError {}

/// Resolve an [`EventTime`] to an absolute master sample using `clock`.
pub(crate) fn resolve_time(time: EventTime, clock: &AudioClock) -> Result<u64, EventError> {
    match time {
        EventTime::Sample(s) => Ok(s),
        EventTime::Beat(b) => {
            let samples = clock.samples_for_beats(b);
            if samples.is_finite() && samples >= 0.0 {
                Ok(samples.round() as u64)
            } else {
                Err(EventError::BeatUnresolvable)
            }
        }
    }
}
