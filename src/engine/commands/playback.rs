//! Playback lifecycle command handlers — play, pause, stop, seek, speed, pitch.

use log::{info, warn};

use super::super::{AudioEngine, PlaybackStream};
use crate::buffer::PlaybackState;

impl AudioEngine {
    pub(super) fn handle_play(&mut self) {
        if self.stream.is_some() && !self.stream_ended {
            if let Some(ref output) = self.audio_output {
                output.resume();
            }
            self.update_playback_state(PlaybackState::Playing);
            info!("Playback started");
        } else if self.stream_ended {
            log::warn!("Play command ignored: stream has ended. Reload the track to play again.");
        } else {
            log::warn!("Play command ignored: no track loaded");
            self.update_playback_state(PlaybackState::Stopped);
        }
    }

    pub(super) fn handle_pause(&mut self) {
        if self.stream.is_some() {
            if let Some(ref output) = self.audio_output {
                output.pause();
            }
            self.update_playback_state(PlaybackState::Paused);
            info!("Playback paused");
        }
    }

    pub(super) fn handle_stop(&mut self) {
        if let Some(ref output) = self.audio_output {
            output.reset_buffer();
        } else {
            self.sample_sink.reset();
        }
        self.scratch.pending_output_frames.clear();
        self.clock.set_source_frames(0);
        self.graph.reset();
        self.stream = None;
        self.stream_ended = false;
        self.scratch.crossfade_triggered = false;
        self.loudness_scan.next_track_path = None;
        self.scratch.cached_incoming_decoder = None;
        self.loudness_scan.current_track_path = None;
        self.loudness_scan.pending_loudness_metadata = None;
        self.loudness_scan.incoming_track_path = None;
        self.loudness_scan.pending_incoming_loudness_metadata = None;
        self.scratch.pending_chunk = None;
        self.scratch.pending_incoming_chunk = None;
        self.recovery.consecutive_decode_errors = 0;
        self.write_playback_info(|pb| pb.position_secs = 0.0);
        self.update_playback_state(PlaybackState::Stopped);
        info!("Playback stopped");
    }

    pub(super) fn handle_seek(&mut self, pos_secs: f32) {
        if !pos_secs.is_finite() || pos_secs < 0.0 {
            warn!("Seek ignored: invalid position {}", pos_secs);
            return;
        }
        let seek_in_incoming = self.stream.as_ref().is_some_and(|s| s.is_crossfading());
        if seek_in_incoming {
            if let Some(PlaybackStream::Transitioning {
                incoming_decoder,
                incoming_resampler,
                ..
            }) = self.stream.take()
            {
                self.clock.source_sample_rate = incoming_decoder.info().sample_rate;
                self.duration_secs = incoming_decoder.duration_secs();
                self.scratch.crossfade_triggered = false;
                self.recovery.consecutive_decode_errors = 0;
                self.loudness_scan.current_track_path =
                    self.loudness_scan.incoming_track_path.take();
                let incoming_meta = self.loudness_scan.pending_incoming_loudness_metadata.take();
                self.loudness_scan.pending_loudness_metadata = incoming_meta;
                if let Some(meta) = incoming_meta {
                    self.graph.apply_loudness_metadata_outgoing(Some(meta));
                }
                self.stream = Some(PlaybackStream::Single {
                    decoder: incoming_decoder,
                    resampler: incoming_resampler,
                });
                self.graph.begin_playing();
            }
        }

        if let Some(PlaybackStream::Single {
            ref mut decoder,
            ref mut resampler,
        }) = self.stream
        {
            let clamped_pos = if self.duration_secs > 0.0 {
                pos_secs.min(self.duration_secs - 0.05).max(0.0)
            } else {
                pos_secs.min(86400.0)
            };

            self.scratch.pending_output_frames.clear();
            if let Some(ref output) = self.audio_output {
                output.reset_buffer();
            } else {
                self.sample_sink.reset();
            }

            self.graph.begin_seek_fadeout();

            if !self.dsd.dop_active {
                for _ in 0..128 {
                    if self.scratch.pending_output_frames.len()
                        >= super::super::MAX_PENDING_OUTPUT_FRAMES
                    {
                        break;
                    }
                    // Advance the seek-fade envelope on a silence frame (the
                    // per-frame pipeline entry became the graph's block path).
                    let mut l = [0.0f32];
                    let mut r = [0.0f32];
                    self.graph.process_block(&mut l, &mut r);
                    super::super::decode_loop::push_pending_back_bounded(
                        &mut self.scratch.pending_output_frames,
                        (l[0], r[0]),
                    );
                }
            }
            match decoder.seek(clamped_pos) {
                Ok(()) => {
                    self.clock.set_source_frames(
                        (clamped_pos * self.clock.source_sample_rate as f32).round() as u64,
                    );
                    #[cfg(feature = "resample")]
                    if let Some(ref mut r) = resampler {
                        r.reset();
                    }
                    #[cfg(not(feature = "resample"))]
                    let _ = resampler;
                    self.graph.reset_filters_only();
                    self.graph.begin_seek_fadein();
                    self.scratch.crossfade_triggered = false;
                    self.scratch.pending_chunk = None;
                    self.scratch.pending_incoming_chunk = None;
                    self.write_playback_info(|pb| pb.position_secs = clamped_pos);
                    self.emit_event(crate::events::EngineEvent::SeekCompleted {
                        position_secs: clamped_pos,
                    });
                    info!("Seeked to {:.1}s", clamped_pos);
                }
                Err(e) => {
                    self.graph.begin_seek_fadein();
                    self.clock.reset_track(self.clock.source_sample_rate);
                    self.write_playback_info(|pb| pb.position_secs = 0.0);
                    self.emit_event(crate::events::EngineEvent::Error(format!(
                        "Seek failed: {}",
                        e
                    )));
                    warn!("Seek failed: {}", e);
                }
            }
        }
    }

    pub(super) fn handle_set_speed(&mut self, speed: f32) {
        if !speed.is_finite() {
            warn!("SetSpeed ignored: non-finite value {}", speed);
            return;
        }
        let clamped = speed.clamp(0.25, 4.0);
        if self.dsd.dop_active {
            warn!("SetSpeed ignored while DSD DoP is active (DoP is fixed at 1.0×)");
            return;
        }
        self.speed = clamped;

        match self.config.speed_mode {
            config::SpeedMode::TimeStretch => {
                self.graph.timestretch_mut().stretcher.set_speed(clamped);
                #[cfg(feature = "resample")]
                self.resampler_set_speed_all(1.0);
            }
            config::SpeedMode::PitchShift => {
                self.graph
                    .timestretch_mut()
                    .stretcher
                    .set_pitch_ratio(clamped);
                #[cfg(feature = "resample")]
                self.resampler_set_speed_all(1.0);
            }
            config::SpeedMode::Varispeed => {
                self.graph.timestretch_mut().stretcher.set_speed(1.0);
                #[cfg(feature = "resample")]
                self.resampler_set_speed_all(clamped);
            }
        }

        self.write_playback_info(|pb| pb.speed = clamped);
        info!(
            "Playback speed set to {:.2}x ({:?})",
            clamped, self.config.speed_mode
        );
    }

    /// Set speed on all active resamplers. No-op without the `resample` feature.
    #[cfg(feature = "resample")]
    fn resampler_set_speed_all(&mut self, speed: f32) {
        match &mut self.stream {
            Some(PlaybackStream::Single {
                resampler: Some(ref mut r),
                ..
            }) => {
                r.set_speed(speed);
            }
            Some(PlaybackStream::Transitioning {
                outgoing_resampler,
                incoming_resampler,
                ..
            }) => {
                if let Some(ref mut r) = outgoing_resampler {
                    r.set_speed(speed);
                }
                if let Some(ref mut r) = incoming_resampler {
                    r.set_speed(speed);
                }
            }
            _ => {}
        }
    }

    #[cfg(not(feature = "resample"))]
    fn resampler_set_speed_all(&mut self, _speed: f32) {}

    pub(super) fn handle_set_pitch(&mut self, semitones: f32) {
        if !semitones.is_finite() {
            warn!("SetPitch ignored: non-finite value {}", semitones);
            return;
        }
        let clamped = semitones.clamp(-24.0, 24.0);
        info!("Pitch shift set to {:.2} semitones", clamped);
        self.graph
            .timestretch_mut()
            .stretcher
            .set_pitch_semitones(clamped);
    }

    pub(super) fn handle_shutdown(&mut self) {
        self.stop();
    }
}
