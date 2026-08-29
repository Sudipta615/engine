//! # Timeline and Scheduler (v3.28)
//!
//! Direction 5's "make time a first-class render primitive" — a
//! sample-accurate **event scheduler + transport clock** on the offline /
//! control path. The [`Timeline`] owns an [`AudioClock`] (tempo, transport,
//! loop, bars/beats/ticks) and a queue of [`ScheduledEvent`]s. Each block
//! the render loop calls [`Timeline::advance_block`], which advances the
//! clock and returns the events whose master sample it crossed — **exactly**
//! once and **exactly** when due, regardless of looping.
//!
//! ```text
//! Host / Control Threads
//!         │  schedule(event)
//!         ▼
//!   Timeline (clock + event queue)
//!         │  advance_block(n) → &[ScheduledEvent]
//!         ▼
//!   Block / Sample Execution  (e.g. OfflineExecutor)
//!         │  apply at event.at % block — sample-accurate
//! ```
//!
//! An event is timed in samples or musical beats ([`EventTime`]). Beat
//! events resolve to an absolute master sample at schedule time from the
//! current clock tempo; a [`Quantize`] grid snaps them to bars/beats/16ths.
//! [`TimelineRegion`]s attach names + tempo to spans for inspection. The
//! default tempo math matches the clock; a [`TempoMap`] converts musical
//! position across *tempo changes* exactly.
//!
//! ## Module map
//!
//! - `clock.rs` — [`AudioClock`], [`TransportState`], [`TempoRamp`]
//! - `tempo.rs` — [`TempoMap`], [`TempoPoint`]
//! - `event.rs` — [`ScheduledEvent`], [`EventTime`], [`EventPayload`],
//!   [`EventId`], [`EventError`]
//! - this module — [`Timeline`], [`Quantize`], [`TimelineRegion`]
//!
//! ## Discipline
//!
//! The timeline is control/offline-path and heap-happy by design; it drives
//! an offline renderer (e.g. the Graph 2.0 `OfflineExecutor`). It adds no
//! allocation or lock to any realtime audio thread.

pub mod clock;
pub mod event;
pub mod tempo;

pub use clock::{AudioClock, TempoRamp, TransportState};
pub use event::{EventError, EventId, EventPayload, EventTime, ScheduledEvent};
pub use tempo::{TempoMap, TempoPoint};

use std::collections::BTreeMap;

/// A named tempo/cue span on the master timeline, for inspection.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineRegion {
    pub name: String,
    pub start: u64,
    pub end: u64,
    pub bpm: f32,
}

use serde::{Deserialize, Serialize};

/// Beat-grid quantization: `Some(fraction)` snaps a beat to multiples of
/// `fraction` (1.0 = beats, 0.5 = eighth notes, 0.25 = 16ths, 0.333 = 8th
/// triplets …). `None` disables snapping.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quantize {
    pub beat_fraction: f64,
}

impl Quantize {
    pub fn none() -> Self {
        Self { beat_fraction: 0.0 }
    }

    pub fn grid(beat_fraction: f64) -> Self {
        Self {
            beat_fraction: beat_fraction.max(1e-6),
        }
    }

    fn snap(&self, beats: f64) -> f64 {
        if self.beat_fraction <= 0.0 {
            return beats;
        }
        (beats / self.beat_fraction).round() * self.beat_fraction
    }
}

/// The unified audio timeline + scheduler (v3.28): owns the [`AudioClock`]
/// and a queue of sample-accurate events, yielding due events per block.
#[derive(Debug, Clone)]
pub struct Timeline {
    clock: AudioClock,
    /// master sample → events firing there (BTreeMap keeps order).
    events: BTreeMap<u64, Vec<ScheduledEvent>>,
    next_id: u64,
    regions: Vec<TimelineRegion>,
    quantize: Quantize,
}

impl Timeline {
    pub fn new(sample_rate: f32) -> Self {
        Self::with_clock(AudioClock::new(sample_rate))
    }

    pub fn with_clock(clock: AudioClock) -> Self {
        Self {
            clock,
            events: BTreeMap::new(),
            next_id: 0,
            regions: Vec::new(),
            quantize: Quantize::none(),
        }
    }

    // ── Clock access & transport convenience ─────────────────────────────────

    pub fn clock(&self) -> &AudioClock {
        &self.clock
    }

    pub fn clock_mut(&mut self) -> &mut AudioClock {
        &mut self.clock
    }

    pub fn set_tempo(&mut self, bpm: f32) {
        self.clock.set_tempo(bpm);
    }

    pub fn set_time_signature(&mut self, beats_per_bar: f32) {
        self.clock.set_time_signature(beats_per_bar);
    }

    pub fn set_loop(&mut self, start: u64, end: u64) {
        self.clock.set_loop(start, end);
    }

    pub fn set_loop_enabled(&mut self, enabled: bool) {
        self.clock.set_loop_enabled(enabled);
    }

    pub fn set_state(&mut self, state: TransportState, scratch: u64) {
        self.clock.set_state(state, scratch);
    }

    pub fn set_quantization(&mut self, q: Quantize) {
        self.quantize = q;
    }

    // ── Scheduling ──────────────────────────────────────────────────────────

    /// Schedule an event to fire when the master clock next crosses `at`
    /// samples. Rejects past times (events are one-shot, never re-fire).
    pub fn schedule(
        &mut self,
        time: EventTime,
        payload: EventPayload,
    ) -> Result<EventId, EventError> {
        let at = event::resolve_time(time, &self.clock)?;
        let master = self.clock.master_position();
        if at <= master {
            return Err(EventError::AlreadyPast { at, master });
        }
        let id = EventId(self.next_id);
        self.next_id += 1;
        self.events.entry(at).or_default().push(ScheduledEvent {
            id,
            at,
            time,
            payload,
        });
        Ok(id)
    }

    /// Schedule at an absolute master sample.
    pub fn schedule_sample(
        &mut self,
        at: u64,
        payload: EventPayload,
    ) -> Result<EventId, EventError> {
        self.schedule(EventTime::Sample(at), payload)
    }

    /// Schedule at a musical beat (converted at the current tempo), snapped
    /// to the quantization grid when one is set.
    pub fn schedule_beat(
        &mut self,
        beats: f64,
        payload: EventPayload,
    ) -> Result<EventId, EventError> {
        let snapped = self.quantize.snap(beats);
        self.schedule(EventTime::Beat(snapped), payload)
    }

    /// Snap a beat position to the configured grid (no-op when quantization
    /// is off).
    pub fn quantize_beat(&self, beats: f64) -> f64 {
        self.quantize.snap(beats)
    }

    /// Number of pending (not yet fired) events.
    pub fn pending(&self) -> usize {
        self.events.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    // ── Block advance (sample-accurate firing) ──────────────────────────────

    /// Advance the clock `samples` and return every event whose master
    /// sample lies in `(prev_master, prev_master + samples]`. Fired events
    /// are removed (once semantics). Returns empty when the transport is not
    /// playing or nothing is due.
    pub fn advance_block(&mut self, samples: u64) -> Vec<ScheduledEvent> {
        let prev = self.clock.master_position();
        self.clock.advance(samples);
        let now = self.clock.master_position();
        if now <= prev {
            return Vec::new();
        }
        let mut fired = Vec::new();
        let keys: Vec<u64> = self.events.range(prev + 1..=now).map(|(k, _)| *k).collect();
        for k in keys {
            if let Some(mut e) = self.events.remove(&k) {
                fired.append(&mut e);
            }
        }
        fired
    }

    // ── Timeline regions ────────────────────────────────────────────────────

    /// Register a named region spanning `[start, end)` samples.
    pub fn add_region(&mut self, name: impl Into<String>, start: u64, end: u64, bpm: f32) {
        self.regions.push(TimelineRegion {
            name: name.into(),
            start,
            end: end.max(start),
            bpm,
        });
    }

    /// The region containing `sample`, if any (last matching wins).
    pub fn region_at(&self, sample: u64) -> Option<&TimelineRegion> {
        let mut found = None;
        for r in &self.regions {
            if sample >= r.start && sample < r.end {
                found = Some(r);
            }
        }
        found
    }

    pub fn regions(&self) -> &[TimelineRegion] {
        &self.regions
    }
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn playing() -> Timeline {
        let mut t = Timeline::new(SR);
        t.set_state(TransportState::Playing, 0);
        t
    }

    #[test]
    fn fires_events_once_sample_accurately() {
        let mut t = playing();
        t.schedule(EventTime::Sample(100), EventPayload::Trigger { tag: 1 })
            .unwrap();
        t.schedule(EventTime::Sample(200), EventPayload::Trigger { tag: 2 })
            .unwrap();
        assert_eq!(t.pending(), 2);
        // First block 0..128: fires tag 1 at sample 100.
        let fired = t.advance_block(128);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].payload, EventPayload::Trigger { tag: 1 });
        assert_eq!(fired[0].at, 100);
        // Second block 128..256: fires tag 2 at 200.
        let fired = t.advance_block(128);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].payload, EventPayload::Trigger { tag: 2 });
        assert_eq!(t.pending(), 0);
        // Nothing more ever fires (once semantics even past the sample).
        assert!(t.advance_block(128).is_empty());
    }

    #[test]
    fn past_events_are_rejected() {
        let mut t = playing();
        t.advance_block(512);
        assert!(matches!(
            t.schedule(EventTime::Sample(100), EventPayload::Host(1)),
            Err(EventError::AlreadyPast { .. })
        ));
        // Equal to master is also past.
        let master = t.clock().master_position();
        assert!(matches!(
            t.schedule(EventTime::Sample(master), EventPayload::Host(1)),
            Err(EventError::AlreadyPast { .. })
        ));
    }

    #[test]
    fn paused_advance_fires_nothing() {
        let mut t = Timeline::new(SR);
        t.set_state(TransportState::Paused, 0);
        t.schedule(EventTime::Sample(10), EventPayload::Host(1))
            .unwrap();
        assert!(
            t.advance_block(64).is_empty(),
            "paused: no advance, no fire"
        );
    }

    #[test]
    fn beat_events_resolve_at_current_tempo() {
        // 120 BPM → 1 beat = 24000 samples. Beat 1 fires at master 24000.
        let mut t = playing();
        t.set_tempo(120.0);
        t.schedule_beat(1.0, EventPayload::Host(7)).unwrap();
        let fired = t.advance_block(24_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].at, 24_000);
    }

    #[test]
    fn quantization_snaps_beats_to_grid() {
        let mut t = playing();
        t.set_tempo(120.0);
        t.set_quantization(Quantize::grid(0.25)); // 16th notes
                                                  // Beat 1.1 → nearest 16th is 1.0 → sample 24000.
        t.schedule_beat(1.1, EventPayload::Host(3)).unwrap();
        let first = {
            let mut t2 = t.clone();
            t2.advance_block(24_000 - 1).len()
        };
        assert_eq!(first, 0, "not fired one sample before the grid point");
        let fired = t.advance_block(24_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].at, 24_000);
    }

    #[test]
    fn regions_resolve_containment() {
        let mut t = Timeline::new(SR);
        t.add_region("intro", 0, 48_000, 60.0);
        t.add_region("verse", 48_000, 144_000, 120.0);
        assert_eq!(t.region_at(24_000).map(|r| r.name.as_str()), Some("intro"));
        assert_eq!(t.region_at(72_000).map(|r| r.name.as_str()), Some("verse"));
        assert_eq!(t.region_at(200_000), None);
    }
}
