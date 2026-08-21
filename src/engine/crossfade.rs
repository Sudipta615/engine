//! Crossfade trigger detection and transition management.

use log::{info, warn};

#[cfg(feature = "resample")]
use super::recovery;
use super::{AudioEngine, EngineError, PlaybackStream};
use crate::decode::{DecodeInfo, Decoder};
use std::path::Path;

impl AudioEngine {
    /// Prepare the next track for crossfading by pre-opening its decoder.
    /// The incoming decoder is created ahead of time so that when the
    /// current track reaches its final N seconds, the crossfade can begin
    /// immediately without any I/O delay.
    pub fn prepare_next_track(&mut self, path: &Path) -> Result<DecodeInfo, EngineError> {
        // If crossfade is disabled or there is no current stream, just
        // remember the path for a regular track transition later.
        self.loudness_scan.next_track_path = Some(path.to_path_buf());
        let decoder = Decoder::open(path)?;
        let info = decoder.info().clone();
        self.scratch.cached_incoming_decoder = Some(decoder);

        // Prepare loudness metadata for the incoming track: tags now, plus a
        // cached scan result if the file is unchanged, and a background scan
        // only when EBU R128 is still missing — so normalization is active
        // from the first sample of the fade-in without re-decoding tracks.
        let mut loudness_meta = crate::decode::extract_loudness_metadata(path);
        if loudness_meta.ebu_r128_loudness.is_none() {
            if let Some(cached) = crate::decode::loudness_cache::lookup(path) {
                loudness_meta.ebu_r128_loudness = cached.ebu_r128_loudness;
                loudness_meta.ebu_r128_peak = cached.ebu_r128_peak_dbtp;
                info!("Loaded cached loudness metadata for {}", path.display());
            }
        }
        self.loudness_scan.pending_incoming_loudness_metadata = Some(loudness_meta);
        self.start_incoming_loudness_scan();

        if self.config.crossfade.enabled {
            info!("Next track prepared for crossfade: {}", path.display());
        }

        Ok(info)
    }

    /// Check if the active track has entered its final N seconds and
    /// trigger the crossfade transition if so. The threshold is computed
    /// from the crossfade duration in the config, converted to sample
    /// counts for sample-accurate timing (not wall-clock time).
    pub(super) fn check_crossfade_trigger(&mut self) {
        let is_crossfade_or_fade = match self.config.transition_mode {
            config::TransitionMode::Crossfade => true,
            config::TransitionMode::Fade => true,
            config::TransitionMode::Gapless => false,
            config::TransitionMode::Stop => false,
        } || self.config.crossfade.enabled;

        if self.scratch.crossfade_triggered || !is_crossfade_or_fade {
            return;
        }
        // Bit-Perfect mode forbids overlap, fades, and track mixing because
        // the mixer changes the sample sequence even when both envelopes are
        // nominally at unity. Let the normal gapless EOS handoff handle the
        // next track instead.
        if self.pipeline.is_bit_perfect() {
            return;
        }
        if self.loudness_scan.next_track_path.is_none() {
            return;
        }

        // DSD tracks never crossfade: overlapping/mixing DoP, decimated DSD,
        // or native DSD streams is meaningless, and DoP framing / the native
        // bitstream must stay continuous. A DSD track playing now, or a DSD
        // incoming track when DoP is requested, transitions gaplessly
        // through load_track at EOS instead.
        if self.dsd.dop_active || self.dsd.native_dsd_active {
            return;
        }
        if self.config.dsd_output == config::DsdOutput::DoP {
            if let Some(ref p) = self.loudness_scan.next_track_path {
                let is_dsd = p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    e.eq_ignore_ascii_case("dsf") || e.eq_ignore_ascii_case("dff")
                });
                if is_dsd {
                    return;
                }
            }
        }

        // Determine the remaining time in the current track.
        // Calculate based on exact frame counts to avoid floating point drift.
        let source_rate = self.clock.source_sample_rate;
        if source_rate == 0 {
            return;
        }
        let total_frames = (self.duration_secs * source_rate as f32).round() as u64;
        let remaining_frames = total_frames.saturating_sub(self.clock.source_frames);
        let remaining_secs = remaining_frames as f32 / source_rate as f32;

        let crossfade_duration_secs = self.config.crossfade.duration_ms as f32 / 1000.0;

        // Add a small lead time (0.5s) so the incoming decoder has time
        // to start producing samples before the crossfade begins.
        let trigger_threshold = crossfade_duration_secs + 0.5;

        if remaining_secs <= trigger_threshold && remaining_secs > 0.0 {
            self.scratch.crossfade_triggered = true;
            self.begin_crossfade_transition();
        }
    }

    /// Transition from Single to Transitioning state by spawning the
    /// incoming decoder and initializing the crossfade parameters.
    fn begin_crossfade_transition(&mut self) {
        let next_path = match self.loudness_scan.next_track_path.take() {
            Some(p) => p,
            None => return,
        };
        // Remember the incoming path separately: `next_track_path` is now
        // consumed, but scan results for this track must still be matched.
        self.loudness_scan.incoming_track_path = Some(next_path.clone());

        let incoming_decoder = match self.scratch.cached_incoming_decoder.take() {
            Some(d) => {
                info!("Using cached incoming decoder for crossfade");
                d
            }
            None => match Decoder::open(&next_path) {
                Ok(d) => d,
                Err(e) => {
                    warn!("Failed to open incoming track for crossfade: {}", e);
                    self.scratch.crossfade_triggered = false;
                    return;
                }
            },
        };

        let incoming_info = incoming_decoder.info().clone();
        let incoming_sample_rate = incoming_info.sample_rate;

        // Create resampler for the incoming track.
        #[cfg(feature = "resample")]
        let incoming_resampler = recovery::build_resampler(
            self.config.resampler_quality,
            incoming_sample_rate as f32,
            self.output_sample_rate as f32,
            self.speed,
            self.config.precision_mode,
        );

        #[cfg(not(feature = "resample"))]
        let incoming_resampler: Option<()> = None;

        // Calculate crossfade frame count based on output sample rate.
        let crossfade_total_frames = (self.config.crossfade.duration_ms as f32
            * 0.001
            * self.output_sample_rate as f32) as usize;

        // Extract the current decoder and resampler from the stream.
        let current_stream = self.stream.take();
        match current_stream {
            Some(PlaybackStream::Single { decoder, resampler }) => {
                // `TransitionMode::Fade` is a SEQUENTIAL fade (fade-out →
                // silence gap → fade-in) — the two tracks are never mixed at
                // the same time, unlike `Crossfade`. The mixer's Fading state
                // applies that envelope and the engine gates the incoming
                // decoder until the fade-in phase, so the next track starts
                // from its own sample 0. Both modes share the transition
                // machinery and the configured duration/curve.
                let is_fade = self.config.transition_mode == config::TransitionMode::Fade;
                info!(
                    "{} transition starting: {} frames ({:.1}s), incoming: {} Hz",
                    if is_fade { "Fade" } else { "Crossfade" },
                    crossfade_total_frames,
                    self.config.crossfade.duration_ms as f32 / 1000.0,
                    incoming_sample_rate
                );

                // Tell the pipeline mixer to start the transition with the
                // configured curve.
                self.pipeline
                    .mixer_mut()
                    .set_curve(self.config.crossfade.curve.into());
                if is_fade {
                    self.pipeline.mixer_mut().start_fade();
                } else {
                    self.pipeline.mixer_mut().start_crossfade();
                }

                self.stream = Some(PlaybackStream::Transitioning {
                    outgoing_decoder: decoder,
                    outgoing_resampler: resampler,
                    incoming_decoder,
                    incoming_resampler,
                    crossfade_frames_remaining: crossfade_total_frames,
                    crossfade_total_frames,
                });

                self.scratch.pending_chunk = None;
                self.scratch.pending_incoming_chunk = None;
                // Start each crossfade with clean output-domain scratch FIFOs
                // (a seek or earlier transition may have left leftovers).
                self.scratch.rs_out_buf.clear();
                self.scratch.rs_in_buf.clear();

                // Apply the incoming track's loudness metadata (tags, plus any
                // background scan results that already arrived) to the incoming
                // chain so EBU R128 normalization is active from the first
                // sample of the fade-in. If the metadata was never prepared,
                // extract it now.
                let incoming_meta = match self.loudness_scan.pending_incoming_loudness_metadata {
                    Some(m) => m,
                    None => {
                        let m = crate::decode::extract_loudness_metadata(&next_path);
                        self.loudness_scan.pending_incoming_loudness_metadata = Some(m);
                        m
                    }
                };
                self.pipeline
                    .apply_loudness_metadata_incoming(Some(incoming_meta));
                // Kick a scan for the incoming track if its tags lack EBU R128.
                self.start_incoming_loudness_scan();
            }
            Some(PlaybackStream::Transitioning { .. }) => {
                warn!("Crossfade triggered while already transitioning; ignoring");
            }
            None => {
                warn!("Crossfade triggered but no active stream");
            }
        }
    }
}
