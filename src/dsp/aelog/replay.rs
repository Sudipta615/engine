//! Replay side of aelog (v3.29) — the golden-render oracle.
//!
//! [`replay_events`] re-executes a recorded [`Aelog`] against a fresh
//! [`Timeline`] and returns the identical fired-event stream and end clock
//! state — pure functions of the log. [`replay_render`] additionally feeds
//! blocks to a provided Graph 2.0 [`OfflineExecutor`], reproducing
//! **byte-identical captured audio**: replaying a session against the same
//! graph is the project's golden-render check.

use super::{Aelog, RecordedCommand};
use crate::dsp::graph2::{ExecutionOrder, Graph2, NodeId, OfflineExecutor};
use crate::dsp::timeline::{AudioClock, EventPayload, ScheduledEvent, Timeline};
use crate::spatial::math::Vec3;

/// Errors produced while replaying a log.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayError {
    /// The log's sample rate is not usable (≤ 0).
    BadSampleRate(f32),
    /// The graph was not compiled for this log (order covers wrong nodes).
    BadOrder,
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::BadSampleRate(sr) => write!(f, "bad sample rate {sr} in aelog header"),
            ReplayError::BadOrder => write!(f, "execution order does not match the graph"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// The deterministic outcome of replaying a log.
#[derive(Debug, Clone)]
pub struct ReplayOutcome {
    /// The end-state timeline (clock position, tempo, transport, pending
    /// events) — reproducible from the log alone.
    pub timeline: Timeline,
    /// Every event that fired during replay, in firing order.
    pub fired: Vec<ScheduledEvent>,
    /// For [`replay_render`]: the audio captured at the recorded sink. Empty
    /// for [`replay_events`].
    pub captured: Vec<f32>,
    /// The reconstructed audio-input track (all `InputAudio` chunks
    /// concatenated in order).
    pub audio_input: Vec<f32>,
    /// The reconstructed listener trajectory: `(master sample, position)`
    /// pairs in recorded order — the listener motion a spatial renderer
    /// re-applies sample-exactly.
    pub listener_motion: Vec<(u64, Vec3)>,
}

/// Replay a log against a fresh [`Timeline`], reproducing the fired-event
/// stream and end clock state. Pure and deterministic.
pub fn replay_events(log: &Aelog) -> Result<ReplayOutcome, ReplayError> {
    log.check_version()
        .map_err(|_| ReplayError::BadSampleRate(log.header.sample_rate))?;
    if log.header.sample_rate <= 0.0 {
        return Err(ReplayError::BadSampleRate(log.header.sample_rate));
    }
    let mut timeline = Timeline::new(log.header.sample_rate);
    let mut fired = Vec::new();
    apply_commands(log, &mut timeline, &mut fired)?;
    let (audio_input, listener_motion) = reconstruct_inputs(log);
    Ok(ReplayOutcome {
        timeline,
        fired,
        captured: Vec::new(),
        audio_input,
        listener_motion,
    })
}

/// Replay a log while also rendering through the provided Graph 2.0 graph,
/// reproducing byte-identical captured audio at `sink` — the golden-render
/// check. `SetGain` events are applied to the executor sample-accurately
/// exactly as a live driver would.
pub fn replay_render(
    log: &Aelog,
    graph: &Graph2,
    order: &ExecutionOrder,
    sink: NodeId,
) -> Result<ReplayOutcome, ReplayError> {
    log.check_version()
        .map_err(|_| ReplayError::BadSampleRate(log.header.sample_rate))?;
    if log.header.sample_rate <= 0.0 {
        return Err(ReplayError::BadSampleRate(log.header.sample_rate));
    }
    let block = log.header.block_frames.max(1) as usize;
    let (audio_input, listener_motion) = reconstruct_inputs(log);
    let mut ex = OfflineExecutor::new(graph, order, block, log.header.sample_rate)
        .map_err(|_| ReplayError::BadOrder)?;
    // Feed the recorded audio input into the graph's Buffer sources so the
    // render uses the exact session audio.
    if !audio_input.is_empty() {
        ex.set_external_input(Some(audio_input.clone()));
    }

    let mut timeline = Timeline::new(log.header.sample_rate);
    let mut fired = Vec::new();
    for command in &log.commands {
        if let RecordedCommand::Advance(_) = command {
            // Fire due events first, applying SetGain steps sample-accurately.
            for e in timeline.advance_block(block as u64) {
                if let EventPayload::SetGain { node, gain } = e.payload {
                    ex.set_gain_step(NodeId(node), gain, e.local_index(block as u64))
                        .map_err(|_| ReplayError::BadOrder)?;
                }
                fired.push(e);
            }
            if timeline.clock().is_playing() {
                ex.process_block().map_err(|_| ReplayError::BadOrder)?;
            }
        } else {
            // Non-advance commands: fire nothing, but still keep the fired
            // stream consistent by running through the shared apply helper.
            // (None of the non-advance commands can fire events.)
            let mut nothing = Vec::new();
            apply_command(command, &mut timeline, &mut nothing)?;
        }
    }

    let captured = ex.capture(sink).map(|v| v.to_vec()).unwrap_or_default();
    Ok(ReplayOutcome {
        timeline,
        fired,
        captured,
        audio_input,
        listener_motion,
    })
}

/// Apply every command in order, collecting fired events.
fn apply_commands(
    log: &Aelog,
    timeline: &mut Timeline,
    fired: &mut Vec<ScheduledEvent>,
) -> Result<(), ReplayError> {
    for command in &log.commands {
        apply_command(command, timeline, fired)?;
    }
    Ok(())
}

/// Reassemble the recorded audio-input track (chunks in order) and the
/// listener trajectory from the log's commands.
fn reconstruct_inputs(log: &Aelog) -> (Vec<f32>, Vec<(u64, Vec3)>) {
    let mut audio = Vec::new();
    let mut motion = Vec::new();
    for c in &log.commands {
        match c {
            RecordedCommand::InputAudio(chunk) => audio.extend_from_slice(chunk),
            RecordedCommand::SetListenerPosition { at, position } => {
                motion.push((*at, *position));
            }
            _ => {}
        }
    }
    (audio, motion)
}

/// Apply one recorded command to a timeline.
fn apply_command(
    command: &RecordedCommand,
    timeline: &mut Timeline,
    fired: &mut Vec<ScheduledEvent>,
) -> Result<(), ReplayError> {
    match command {
        RecordedCommand::Schedule { time, payload } => {
            // Past-time schedules were rejected during recording, so this
            // cannot fail on a faithful log; ignore err to stay total.
            let _ = timeline.schedule(*time, *payload);
        }
        RecordedCommand::SetTempo(bpm) => timeline.set_tempo(*bpm),
        RecordedCommand::SetTimeSignature(bpb) => timeline.set_time_signature(*bpb),
        RecordedCommand::SetLoop { start, end } => timeline.set_loop(*start, *end),
        RecordedCommand::SetLoopEnabled(en) => timeline.set_loop_enabled(*en),
        RecordedCommand::SetTempoRamp {
            target,
            duration_samples,
        } => {
            timeline
                .clock_mut()
                .set_tempo_ramp(*target, *duration_samples);
        }
        RecordedCommand::SetState(state, scratch) => {
            timeline.set_state(*state, *scratch);
        }
        RecordedCommand::SetQuantize(q) => timeline.set_quantization(*q),
        RecordedCommand::Advance(samples) => {
            fired.extend(timeline.advance_block(*samples));
        }
        // Replay-only inputs: the timeline itself has no audio input or
        // listener; they are reconstructed into `ReplayOutcome` by
        // `reconstruct_inputs` and applied by the driver / spatial renderer.
        RecordedCommand::InputAudio(_) | RecordedCommand::SetListenerPosition { .. } => {}
    }
    Ok(())
}

/// Convenience: the end-state clock of a replay (position, master, tempo).
pub fn end_clock(outcome: &ReplayOutcome) -> &AudioClock {
    outcome.timeline.clock()
}
