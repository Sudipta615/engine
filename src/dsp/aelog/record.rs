//! Recording side of aelog (v3.29).
//!
//! [`AelogRecorder`] wraps a [`Timeline`] and mirrors its mutation surface,
//! appending a [`RecordedCommand`] for every call. Because the recorder is
//! the *only* way to touch the timeline it owns, a session cannot silently
//! drift from its log — `finish()` yields an [`Aelog`] that is a pure
//! function of the recording.

use super::{Aelog, RecordedCommand, SessionHeader};
use crate::dsp::timeline::{
    EventError, EventId, EventPayload, EventTime, Quantize, ScheduledEvent, Timeline,
    TransportState,
};
use crate::spatial::math::Vec3;

/// A recording session: owns a [`Timeline`] and logs every mutation.
#[derive(Debug, Clone)]
pub struct AelogRecorder {
    timeline: Timeline,
    header: SessionHeader,
    commands: Vec<RecordedCommand>,
}

impl AelogRecorder {
    pub fn new(sample_rate: f32, block_frames: u64) -> Self {
        Self {
            timeline: Timeline::new(sample_rate),
            header: SessionHeader::new(sample_rate, block_frames),
            commands: Vec::new(),
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.header = self.header.with_label(label);
        self
    }

    /// The current clock (read-only; mutations go through the recorded
    /// methods below).
    pub fn clock(&self) -> &crate::dsp::timeline::AudioClock {
        self.timeline.clock()
    }

    pub fn pending(&self) -> usize {
        self.timeline.pending()
    }

    // ── Recorded mutations ──────────────────────────────────────────────────

    pub fn schedule(
        &mut self,
        time: EventTime,
        payload: EventPayload,
    ) -> Result<EventId, EventError> {
        let id = self.timeline.schedule(time, payload)?;
        self.commands
            .push(RecordedCommand::Schedule { time, payload });
        Ok(id)
    }

    pub fn set_tempo(&mut self, bpm: f32) {
        self.timeline.set_tempo(bpm);
        self.commands.push(RecordedCommand::SetTempo(bpm));
    }

    pub fn set_time_signature(&mut self, beats_per_bar: f32) {
        self.timeline.set_time_signature(beats_per_bar);
        self.commands
            .push(RecordedCommand::SetTimeSignature(beats_per_bar));
    }

    pub fn set_loop(&mut self, start: u64, end: u64) {
        self.timeline.set_loop(start, end);
        self.commands.push(RecordedCommand::SetLoop { start, end });
    }

    pub fn set_loop_enabled(&mut self, enabled: bool) {
        self.timeline.set_loop_enabled(enabled);
        self.commands.push(RecordedCommand::SetLoopEnabled(enabled));
    }

    pub fn set_tempo_ramp(&mut self, target: f32, duration_samples: f32) {
        self.timeline
            .clock_mut()
            .set_tempo_ramp(target, duration_samples);
        self.commands.push(RecordedCommand::SetTempoRamp {
            target,
            duration_samples,
        });
    }

    pub fn set_state(&mut self, state: TransportState, scratch: u64) {
        self.timeline.set_state(state, scratch);
        self.commands
            .push(RecordedCommand::SetState(state, scratch));
    }

    pub fn set_quantization(&mut self, q: Quantize) {
        self.timeline.set_quantization(q);
        self.commands.push(RecordedCommand::SetQuantize(q));
    }

    /// Advance the clock one block and return the events that fired
    /// (recorded as an `Advance` command).
    pub fn advance_block(&mut self, samples: u64) -> Vec<ScheduledEvent> {
        let fired = self.timeline.advance_block(samples);
        self.commands.push(RecordedCommand::Advance(samples));
        fired
    }

    /// Record a chunk of the session's audio input (what was fed into the
    /// graph's `Buffer` source this block). Chunks concatenate in order
    /// during replay to reconstruct the full mono track.
    pub fn record_audio_input(&mut self, chunk: &[f32]) {
        self.commands
            .push(RecordedCommand::InputAudio(chunk.to_vec()));
    }

    /// Record a listener-motion sample: at the current master sample the
    /// listener position becomes `position` (applies from there onward),
    /// so spatial sessions replay the exact trajectory.
    pub fn record_listener_position(&mut self, position: Vec3) {
        let at = self.timeline.clock().master_position();
        self.commands
            .push(RecordedCommand::SetListenerPosition { at, position });
    }

    // ── Finish ──────────────────────────────────────────────────────────────

    /// Seal the session into an [`Aelog`].
    pub fn finish(self) -> Aelog {
        Aelog {
            header: self.header,
            commands: self.commands,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_sessions_record_identical_logs() {
        let mut a = AelogRecorder::new(48_000.0, 128);
        a.set_state(TransportState::Playing, 0);
        a.set_tempo(120.0);
        a.schedule(EventTime::Beat(1.0), EventPayload::Host(7))
            .unwrap();
        for _ in 0..3 {
            a.advance_block(128);
        }
        let log_a = a.finish();

        let mut b = AelogRecorder::new(48_000.0, 128);
        b.set_state(TransportState::Playing, 0);
        b.set_tempo(120.0);
        b.schedule(EventTime::Beat(1.0), EventPayload::Host(7))
            .unwrap();
        for _ in 0..3 {
            b.advance_block(128);
        }
        let log_b = b.finish();

        assert_eq!(log_a, log_b, "deterministic logs");
        assert_eq!(log_a.to_json().unwrap(), log_b.to_json().unwrap());
        assert_eq!(log_a.commands.len(), 6, "state+tempo+schedule+3 advances");
    }

    #[test]
    fn recorder_captures_past_rejection_as_no_command() {
        let mut r = AelogRecorder::new(48_000.0, 128);
        r.set_state(TransportState::Playing, 0);
        r.advance_block(1000);
        // Scheduling in the past must fail and record nothing.
        assert!(r
            .schedule(EventTime::Sample(10), EventPayload::Host(1))
            .is_err());
        let log = r.finish();
        assert_eq!(log.commands.len(), 2);
    }
}
