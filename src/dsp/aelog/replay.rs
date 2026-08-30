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
use crate::dsp::timeline::{
    AudioClock, CurveBeats, EventPayload, ScheduledEvent, TempoMap, Timeline,
};
use crate::spatial::acoustic::bake::BakedScene;
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
    /// The reconstructed unaddressed audio-input track (all `clip: None`
    /// `InputAudio` chunks concatenated in order) as **channel-major
    /// planes** — `track[0]` is channel 0 (a mono session yields one
    /// plane).
    pub audio_input: Vec<Vec<f32>>,
    /// The reconstructed per-clip audio tracks: `(clip address, channel-
    /// major planes)` in first-recorded order. Each addresses the `Buffer`
    /// nodes bearing that clip name — the multi-input companion to
    /// `audio_input`.
    pub clip_tracks: Vec<(String, Vec<Vec<f32>>)>,
    /// The reconstructed listener trajectory: `(master sample, position)`
    /// pairs in recorded order — the listener motion a spatial renderer
    /// re-applies sample-exactly.
    pub listener_motion: Vec<(u64, Vec3)>,
    /// The reconstructed **acoustic world timeline**: `(master sample,
    /// baked scene)` swaps in recorded order. `replay_render` re-attaches
    /// each to the executor's `Acoustic` nodes at its sample, so an
    /// animated world replays its geometry changes exactly.
    pub scene_swaps: Vec<(u64, BakedScene)>,
    /// The recorded **tempo map** musical automation evaluates against
    /// (beat → sample across tempo changes). `None` if never recorded.
    pub tempo_map: Option<TempoMap>,
    /// The reconstructed **tempo-mapped gain automation**: `(node, curve)`
    /// pairs in recorded order. `replay_render` registers each (plus the
    /// tempo map) on the executor, so the gain sweeps match the recording
    /// byte-for-byte.
    pub gain_automation: Vec<(u32, CurveBeats)>,
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
    let inputs = reconstruct_inputs(log);
    Ok(ReplayOutcome {
        timeline,
        fired,
        captured: Vec::new(),
        audio_input: inputs.audio,
        listener_motion: inputs.motion,
        clip_tracks: inputs.clips,
        scene_swaps: inputs.scenes,
        tempo_map: inputs.tempo_map,
        gain_automation: inputs.gain_auto,
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
    let inputs = reconstruct_inputs(log);
    let mut ex = OfflineExecutor::new(graph, order, block, log.header.sample_rate)
        .map_err(|_| ReplayError::BadOrder)?;
    // Feed the recorded audio input into the graph's Buffer sources so the
    // render uses the exact session audio: the unaddressed track drives
    // unaddressed nodes, and each clip track drives only the nodes bearing
    // that clip address.
    if !inputs.audio.is_empty() {
        ex.set_external_input(Some(inputs.audio.clone()));
    }
    for (clip, track) in &inputs.clips {
        ex.set_external_clip(clip, Some(track.clone()));
    }
    // Musical automation: attach the recorded tempo map and each
    // tempo-mapped gain curve before the first block, so the render sweeps
    // the gains exactly as recorded.
    ex.set_tempo_map(inputs.tempo_map.clone());
    for (node, curve) in &inputs.gain_auto {
        ex.set_gain_automation(NodeId(*node), Some(curve.clone()));
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
            // Acoustic render inputs are applied here, before the next
            // block: a faithful log records each at the current master,
            // which is exactly the next block's start, so they land
            // sample-exactly on the following process_block without
            // resetting the Acoustic nodes' tapped delay lines.
            match command {
                // Re-attach the scene (an animated-world snapshot)…
                RecordedCommand::SetBakedScene { scene, .. } => {
                    ex.swap_baked_scene(scene.clone());
                }
                // …and drive the Acoustic nodes' lookup positions from the
                // replayed listener trajectory, so a spatial golden render
                // exercises the full baked-room path (the response
                // re-looked-up per moving listener).
                RecordedCommand::SetListenerPosition { position, .. } => {
                    ex.set_listener_position(Some(*position));
                }
                _ => {}
            }
        }
    }

    let captured = ex.capture(sink).map(|v| v.to_vec()).unwrap_or_default();
    Ok(ReplayOutcome {
        timeline,
        fired,
        captured,
        audio_input: inputs.audio,
        listener_motion: inputs.motion,
        clip_tracks: inputs.clips,
        scene_swaps: inputs.scenes,
        tempo_map: inputs.tempo_map,
        gain_automation: inputs.gain_auto,
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

/// The render inputs reassembled from a log's commands: the unaddressed
/// audio track (channel-major planes), one track per clip (first-recorded
/// order), the listener trajectory, the acoustic scene-swap timeline, and
/// the musical automation (tempo map + tempo-mapped gain curves).
struct ReconstructedInputs {
    audio: Vec<Vec<f32>>,
    clips: Vec<(String, Vec<Vec<f32>>)>,
    motion: Vec<(u64, Vec3)>,
    scenes: Vec<(u64, BakedScene)>,
    tempo_map: Option<TempoMap>,
    gain_auto: Vec<(u32, CurveBeats)>,
}

/// Reassemble the recorded audio-input tracks (the unaddressed track and
/// one per clip, chunks in order), the listener trajectory, the acoustic
/// scene swaps, and the musical automation from the log's commands. Clip
/// tracks and automation appear in first-recorded order.
fn reconstruct_inputs(log: &Aelog) -> ReconstructedInputs {
    let mut audio: Vec<Vec<f32>> = Vec::new();
    let mut motion = Vec::new();
    let mut clips: Vec<(String, Vec<Vec<f32>>)> = Vec::new();
    let mut scenes: Vec<(u64, BakedScene)> = Vec::new();
    let mut tempo_map: Option<TempoMap> = None;
    let mut gain_auto: Vec<(u32, CurveBeats)> = Vec::new();
    for c in &log.commands {
        match c {
            // Concatenate per channel, so `audio[ch]` is that channel's
            // full track. Channel counts are stable per stream (all chunks
            // of one input carry the same number of planes); a shorter
            // chunk is padded by the missing-channel silence rule below.
            RecordedCommand::InputAudio { clip: None, chunk } => {
                for (ch, plane) in chunk.iter().enumerate() {
                    if ch >= audio.len() {
                        audio.push(Vec::new());
                    }
                    audio[ch].extend_from_slice(plane);
                }
            }
            RecordedCommand::InputAudio {
                clip: Some(name),
                chunk,
            } => match clips.iter_mut().find(|(n, _)| n == name) {
                Some((_, buf)) => {
                    for (ch, plane) in chunk.iter().enumerate() {
                        if ch >= buf.len() {
                            buf.push(Vec::new());
                        }
                        buf[ch].extend_from_slice(plane);
                    }
                }
                None => clips.push((name.clone(), chunk.clone())),
            },
            RecordedCommand::SetListenerPosition { at, position } => {
                motion.push((*at, *position));
            }
            RecordedCommand::SetBakedScene { at, scene } => scenes.push((*at, scene.clone())),
            RecordedCommand::SetTempoMap(map) => tempo_map = Some(map.clone()),
            RecordedCommand::SetGainAutomation { node, curve } => {
                gain_auto.push((*node, curve.clone()));
            }
            _ => {}
        }
    }
    ReconstructedInputs {
        audio,
        clips,
        motion,
        scenes,
        tempo_map,
        gain_auto,
    }
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
        // Replay-only inputs: the timeline itself has no audio input,
        // listener, acoustic world, or musical automation; they are
        // reconstructed into `ReplayOutcome` by `reconstruct_inputs` and
        // applied by the driver / spatial renderer / executor.
        RecordedCommand::InputAudio { .. }
        | RecordedCommand::SetListenerPosition { .. }
        | RecordedCommand::SetBakedScene { .. }
        | RecordedCommand::SetTempoMap(_)
        | RecordedCommand::SetGainAutomation { .. } => {}
    }
    Ok(())
}

/// Convenience: the end-state clock of a replay (position, master, tempo).
pub fn end_clock(outcome: &ReplayOutcome) -> &AudioClock {
    outcome.timeline.clock()
}
