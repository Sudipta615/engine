//! Pre-allocated scratch buffers for zero-allocation decode, crossfade, and output paths.

use crate::decode::{DecodedChunk, Decoder};
use std::collections::VecDeque;

pub const MAX_PENDING_OUTPUT_FRAMES: usize = 16384;
/// Maximum samples retained when a multichannel batch hits a full output ring.
pub const MAX_PENDING_MULTICHANNEL_SAMPLES: usize = 128 * crate::buffer::MAX_CHANNELS;
/// Maximum mixed frames collected before the post-mix chain runs.
pub const MIX_BLOCK_FRAMES: usize = 128;

/// Capacity (in output frames) of the crossfade resampling scratch FIFOs.
#[cfg(feature = "resample")]
pub const CROSSFADE_SCRATCH_FRAMES: usize = crate::dsp::resampler::MAX_OUTPUT_BUFFER_FRAMES * 2;
#[cfg(not(feature = "resample"))]
pub const CROSSFADE_SCRATCH_FRAMES: usize = crate::buffer::MAX_AUDIO_BLOCK_FRAMES * 2;

pub(crate) struct EngineScratch {
    /// Cached partial decoded chunk when the output ring-buffer was full.
    pub(crate) pending_chunk: Option<(DecodedChunk, usize)>,
    /// Cached partial decoded chunk for the incoming decoder during crossfade.
    pub(crate) pending_incoming_chunk: Option<(DecodedChunk, usize)>,
    /// Whether we have already triggered the crossfade for the current track.
    pub(crate) crossfade_triggered: bool,
    pub(crate) cached_incoming_decoder: Option<Decoder>,
    /// Output-domain FIFOs holding resampled frames that have been drained
    /// from the outgoing/incoming resamplers but not yet mixed together.
    pub(crate) rs_out_buf: VecDeque<(f32, f32)>,
    pub(crate) rs_in_buf: VecDeque<(f32, f32)>,
    /// FIFO buffer to hold fully processed, resampled frames that are waiting
    /// to be written to the output ring buffer.
    pub(crate) pending_output_frames: VecDeque<(f32, f32)>,
    /// Processed >2-channel frames waiting to be written to the output ring
    /// buffer (the multichannel analogue of `pending_output_frames`).
    pub(crate) pending_multichannel: Vec<f32>,
    /// Channel count of the frames buffered in `pending_multichannel` (0 when empty).
    pub(crate) pending_multichannel_channels: usize,
    /// Accumulated mixed frames during a crossfade transition: the primary
    /// (outgoing) stream and the secondary (incoming) stream, accumulated in
    /// lockstep and handed to the graph's `process_block_inputs` (Phase 3 S3).
    pub(crate) mix_l: Vec<f32>,
    pub(crate) mix_r: Vec<f32>,
    pub(crate) mix_in_l: Vec<f32>,
    pub(crate) mix_in_r: Vec<f32>,
}

impl Default for EngineScratch {
    fn default() -> Self {
        Self {
            pending_chunk: None,
            pending_incoming_chunk: None,
            crossfade_triggered: false,
            cached_incoming_decoder: None,
            rs_out_buf: VecDeque::with_capacity(CROSSFADE_SCRATCH_FRAMES),
            rs_in_buf: VecDeque::with_capacity(CROSSFADE_SCRATCH_FRAMES),
            pending_output_frames: VecDeque::with_capacity(MAX_PENDING_OUTPUT_FRAMES),
            pending_multichannel: Vec::with_capacity(MAX_PENDING_MULTICHANNEL_SAMPLES),
            pending_multichannel_channels: 0,
            mix_l: Vec::with_capacity(MIX_BLOCK_FRAMES),
            mix_r: Vec::with_capacity(MIX_BLOCK_FRAMES),
            mix_in_l: Vec::with_capacity(MIX_BLOCK_FRAMES),
            mix_in_r: Vec::with_capacity(MIX_BLOCK_FRAMES),
        }
    }
}
