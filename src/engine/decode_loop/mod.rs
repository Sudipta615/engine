//! Core decode-and-process loop for single and crossfade playback modes.

mod common;
mod crossfade;
mod single;

use log::info;

use super::{AudioEngine, PlaybackStream};

// Re-export constants so sibling modules can use `super::MAX_PENDING_OUTPUT_FRAMES` etc.
pub(super) use super::{MAX_PENDING_OUTPUT_FRAMES, MIX_BLOCK_FRAMES};

impl AudioEngine {
    /// Core decode-and-process loop. Handles both Single and Transitioning
    /// (crossfade) playback modes, feeding distinct sample streams into
    /// the DSP pipeline and TrackMixer.
    pub(super) fn decode_and_process(&mut self) {
        self.debug_assert_realtime_buffers();
        // Check if we need to finalize a completed crossfade transition.
        // We do this by taking the stream, checking the state, and
        // either completing the transition or putting it back.
        let needs_completion = match &self.stream {
            Some(PlaybackStream::Transitioning {
                crossfade_frames_remaining,
                ..
            }) => *crossfade_frames_remaining == 0,
            _ => false,
        };

        if needs_completion {
            if let Some(PlaybackStream::Transitioning {
                incoming_decoder,
                incoming_resampler,
                ..
            }) = self.stream.take()
            {
                info!("Crossfade transition complete; incoming track is now active");
                self.clock.reset_track(incoming_decoder.info().sample_rate);
                self.duration_secs = incoming_decoder.duration_secs();
                self.scratch.crossfade_triggered = false;
                self.recovery.consecutive_decode_errors = 0;
                // The incoming track becomes the current track: promote its
                // path and loudness metadata, and feed the merged metadata to
                // the outgoing chain that single-track playback now uses.
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
                self.scratch.pending_chunk = None;
                self.scratch.pending_incoming_chunk = None;
                self.scratch.rs_out_buf.clear();
                self.scratch.rs_in_buf.clear();
                // Re-arm a scan in case the incoming track still lacks EBU
                // R128 metadata (e.g. its scan never ran).
                self.start_loudness_scan();
            }
        }

        // Take the stream out of self to avoid double-&mut-self borrow
        // conflict: the decode methods need &mut self, but the stream
        // references (decoder, resampler) also come from self.stream.
        // By moving the stream to a local, self and stream are disjoint.
        let mut stream = match self.stream.take() {
            Some(s) => s,
            None => return,
        };

        match &mut stream {
            PlaybackStream::Single { decoder, resampler } => {
                self.decode_single_stream(
                    decoder,
                    #[cfg(feature = "resample")]
                    resampler,
                    #[cfg(not(feature = "resample"))]
                    resampler,
                );
            }
            PlaybackStream::Transitioning {
                outgoing_decoder,
                outgoing_resampler,
                incoming_decoder,
                incoming_resampler,
                crossfade_frames_remaining,
                crossfade_total_frames,
            } => {
                self.decode_transitioning_stream(
                    outgoing_decoder,
                    #[cfg(feature = "resample")]
                    outgoing_resampler,
                    #[cfg(not(feature = "resample"))]
                    outgoing_resampler,
                    incoming_decoder,
                    #[cfg(feature = "resample")]
                    incoming_resampler,
                    #[cfg(not(feature = "resample"))]
                    incoming_resampler,
                    crossfade_frames_remaining,
                    *crossfade_total_frames,
                );
            }
        }
        if self.stream.is_none() {
            self.stream = Some(stream);
        } else {
            // A new stream was loaded during decode_single_stream
            // (gapless transition). Discard the old stream; its decoder
            // has hit EndOfStream and is no longer needed.
            log::debug!("Gapless transition: replacing EOS stream with freshly loaded track");
        }
    }
}
// Re-export the free function used by commands/playback.rs
pub(super) use common::push_pending_back_bounded;
