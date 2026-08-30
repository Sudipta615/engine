//! Recording side of aelog (v3.29).
//!
//! [`AelogRecorder`] wraps a [`Timeline`] and mirrors its mutation surface,
//! appending a [`RecordedCommand`] for every call. Because the recorder is
//! the *only* way to touch the timeline it owns, a session cannot silently
//! drift from its log — `finish()` yields an [`Aelog`] that is a pure
//! function of the recording.

use super::{Aelog, RecordedCommand, SessionHeader};
use crate::dsp::timeline::{
    CurveBeats, EventError, EventId, EventPayload, EventTime, Quantize, ScheduledEvent, TempoMap,
    Timeline, TransportState,
};
use crate::spatial::acoustic::bake::BakedScene;
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
    /// graph's unaddressed `Buffer` source this block) — **mono**
    /// convenience: the chunk becomes the single channel plane. Chunks
    /// concatenate in order during replay to reconstruct the full track.
    pub fn record_audio_input(&mut self, chunk: &[f32]) {
        self.commands.push(RecordedCommand::InputAudio {
            clip: None,
            chunk: vec![chunk.to_vec()],
        });
    }

    /// Record a chunk of the session's audio input as **channel-major
    /// planes** (`chunk[0]` = channel 0, …) — the stereo/spatial path.
    /// Channels concatenate in order during replay; the channel count is
    /// part of the recording.
    pub fn record_audio_input_channels(&mut self, chunk: &[Vec<f32>]) {
        self.commands.push(RecordedCommand::InputAudio {
            clip: None,
            chunk: chunk.to_vec(),
        });
    }

    /// Record a chunk of a **clip-addressed** audio input: the audio fed
    /// into the `Buffer` node(s) bearing clip address `clip` this block —
    /// **mono** convenience (the chunk becomes the single channel plane).
    /// Replay reconstructs one track per clip and feeds each only to the
    /// nodes with that address — the multi-input path (one graph mixes
    /// several recorded inputs, each routed by its clip name).
    pub fn record_clip_audio(&mut self, clip: impl Into<String>, chunk: &[f32]) {
        self.commands.push(RecordedCommand::InputAudio {
            clip: Some(clip.into()),
            chunk: vec![chunk.to_vec()],
        });
    }

    /// Record a chunk of a **clip-addressed** audio input as
    /// **channel-major planes** — the stereo/spatial multi-input path
    /// (each clip's track carries its own channel count).
    pub fn record_clip_audio_channels(&mut self, clip: impl Into<String>, chunk: &[Vec<f32>]) {
        self.commands.push(RecordedCommand::InputAudio {
            clip: Some(clip.into()),
            chunk: chunk.to_vec(),
        });
    }

    /// Record a listener-motion sample: at the current master sample the
    /// listener position becomes `position` (applies from there onward),
    /// so spatial sessions replay the exact trajectory.
    pub fn record_listener_position(&mut self, position: Vec3) {
        let at = self.timeline.clock().master_position();
        self.commands
            .push(RecordedCommand::SetListenerPosition { at, position });
    }

    /// Record an **acoustic world swap**: at the current master sample the
    /// baked scene becomes `scene` (applies from there onward), so an
    /// animated acoustic world replays its exact geometry timeline. The
    /// scene is embedded in the log verbatim (deterministic serde — the
    /// response cache is order-stable, the solver world is not logged);
    /// replay re-attaches it to the graph's `Acoustic` nodes without
    /// resetting their tapped delay lines.
    pub fn record_baked_scene(&mut self, scene: &BakedScene) {
        let at = self.timeline.clock().master_position();
        self.commands.push(RecordedCommand::SetBakedScene {
            at,
            scene: scene.clone(),
        });
    }

    /// Record the **tempo map** musical automation evaluates against (beat
    /// → sample across tempo changes). Idempotent: re-recording replaces
    /// the map on replay. Call before scheduling gain automation.
    pub fn record_tempo_map(&mut self, map: &TempoMap) {
        self.commands
            .push(RecordedCommand::SetTempoMap(map.clone()));
    }

    /// Record a **tempo-mapped gain automation** for a graph Gain node
    /// (`node` is the `NodeId.0` value): `curve` is authored in beats and
    /// evaluated against the recorded tempo map, so the gain sweeps
    /// smoothly over the session and replays deterministically.
    pub fn record_gain_automation(&mut self, node: u32, curve: &CurveBeats) {
        self.commands.push(RecordedCommand::SetGainAutomation {
            node,
            curve: curve.clone(),
        });
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
    fn scene_swaps_record_deterministically() {
        // A scene swap is a first-class recorded command: stamped with the
        // master at record time, embedded verbatim, and identical sessions
        // serialize to byte-equal logs (the BTreeMap cache keeps serde
        // order-stable).
        use crate::spatial::acoustic::bake::{AcousticBaker, BakePolicy};
        use crate::spatial::acoustic::geometry::AcousticRoom;
        use crate::spatial::math::Vec3;

        let world =
            crate::spatial::acoustic::solver::AcousticWorld::new(AcousticRoom::default(), 48_000.0);
        let scene = AcousticBaker::new(world, 0.5).bake_single(
            Vec3::new(1.0, 2.0, 1.5),
            Vec3::new(6.0, 2.0, 1.5),
            48_000.0,
            BakePolicy::default(),
        );

        let record = |at_block: u64| {
            let mut r = AelogRecorder::new(48_000.0, 128);
            r.set_state(TransportState::Playing, 0);
            for _ in 0..at_block {
                r.advance_block(128);
            }
            r.record_baked_scene(&scene);
            r.advance_block(128);
            r.finish()
        };
        let a = record(3);
        let b = record(3);
        assert_eq!(a, b, "identical sessions, identical logs");
        assert_eq!(a.to_json().unwrap(), b.to_json().unwrap());
        // The scene swap is stamped at the master when recorded (3 blocks
        // in: commands[0] = SetState, commands[1..3] = advances) and the
        // scene survives a JSON round-trip verbatim.
        match &a.commands[4] {
            RecordedCommand::SetBakedScene { at, scene: s } => {
                assert_eq!(*at, 3 * 128);
                assert_eq!(*s, scene, "scene embeds verbatim");
            }
            other => panic!("expected SetBakedScene, got {other:?}"),
        }
        // The scene round-trips through the aelog JSON byte-stably (the
        // solver world is intentionally not serialized, so compare the
        // serialized form rather than PartialEq).
        let back: Aelog = Aelog::from_json(&a.to_json().unwrap()).unwrap();
        assert_eq!(back.to_json().unwrap(), a.to_json().unwrap());
    }

    #[test]
    fn musical_automation_records_deterministically() {
        use crate::dsp::timeline::automation::CurveBeats;
        use crate::dsp::timeline::tempo::TempoMap;

        let mut map = TempoMap::new();
        map.push(0.0, 120.0);
        map.push(4.0, 200.0);
        let curve = CurveBeats::from_points(&[(0.0, 0.0), (2.0, 0.8), (8.0, 0.2)]).unwrap();

        let mut a = AelogRecorder::new(48_000.0, 256);
        a.record_tempo_map(&map);
        a.record_gain_automation(3, &curve);
        a.set_state(TransportState::Playing, 0);
        a.advance_block(256);
        a.advance_block(256);
        let log_a = a.finish();

        let mut b = AelogRecorder::new(48_000.0, 256);
        b.record_tempo_map(&map);
        b.record_gain_automation(3, &curve);
        b.set_state(TransportState::Playing, 0);
        b.advance_block(256);
        b.advance_block(256);
        let log_b = b.finish();

        // Identical sessions: verbatim commands (the hash is a pure function).
        assert_eq!(log_a, log_b);
        // A different curve changes the recording.
        let mut diff = AelogRecorder::new(48_000.0, 256);
        diff.record_tempo_map(&map);
        diff.record_gain_automation(
            3,
            &CurveBeats::from_points(&[(0.0, 1.0), (8.0, 1.0)]).unwrap(),
        );
        diff.set_state(TransportState::Playing, 0);
        diff.advance_block(256);
        assert_ne!(diff.finish(), log_a);
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
