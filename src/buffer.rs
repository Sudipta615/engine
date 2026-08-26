//! Audio buffer primitives and their public compatibility façade.
//!
//! Implementations are split by responsibility under [`buffer`]: frames and
//! chunks, the PCM ring, native-DSD bytes, and the fixed frame handle each
//! have their own module. This file owns only shared limits/errors and the
//! stable `crate::buffer::*` API.

pub mod audio_frame;
pub mod dsd;
pub mod fixed_frame;
pub mod output;
pub mod pcm_ring;

pub use audio_frame::{AudioChunk, AudioFrame, AudioFrameF32, AudioFrameF64};
pub use dsd::DsdByteBuffer;
pub use fixed_frame::{FixedFrameBuffer, FixedFrameBufferF32, FixedFrameBufferF64};
pub use output::OutputBuffer;
pub use pcm_ring::PcmRingBuffer;

/// Maximum number of frames accepted by one realtime DSP operation.
///
/// Callers that receive larger decoder/device blocks must split them before
/// entering the audio path. Keeping this contract explicit lets every DSP
/// stage preallocate for the same worst-case callback size.
pub const MAX_AUDIO_BLOCK_FRAMES: usize = 4096;
/// Maximum number of frames in the decode-to-DSP buffer
pub const DECODE_BUFFER_FRAMES: usize = 16384;
/// Maximum number of frames in the DSP-to-output buffer
pub const OUTPUT_BUFFER_FRAMES: usize = 8192;
/// Default sample rate
pub const DEFAULT_SAMPLE_RATE: u32 = 44100;
/// Maximum channels we support (16, including 7.1.4 and future layouts).
/// The commonly used 7.1.4 layout occupies 12 channels; keeping headroom
/// here avoids advertising a layout that the realtime frame/scratch types
/// cannot actually carry.
pub const MAX_CHANNELS: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum AudioBlockError {
    #[error("audio block has {frames} frames; maximum is {max}")]
    TooLarge { frames: usize, max: usize },
}

#[inline]
pub fn validate_audio_block(frames: usize) -> Result<(), AudioBlockError> {
    if frames > MAX_AUDIO_BLOCK_FRAMES {
        Err(AudioBlockError::TooLarge {
            frames,
            max: MAX_AUDIO_BLOCK_FRAMES,
        })
    } else {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    #[error("FixedFrameBuffer capacity must be > 0, got {0}")]
    InvalidCapacity(usize),
    #[error("AudioFrame channel count must be between 1 and {0}")]
    InvalidChannelCount(u8),
}

pub use crate::commands::EngineCommand;
pub use crate::dsp_utils::{
    enable_flush_zero_denormals_on_current_thread, flush_denormal, flush_denormal_f64,
    DENORMAL_OFFSET,
};
pub use crate::playback_info::{PlaybackInfo, PlaybackState};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_block_limit_is_enforced() {
        assert!(validate_audio_block(MAX_AUDIO_BLOCK_FRAMES).is_ok());
        assert!(matches!(
            validate_audio_block(MAX_AUDIO_BLOCK_FRAMES + 1),
            Err(AudioBlockError::TooLarge { .. })
        ));
    }

    #[test]
    fn compatibility_reexports_remain_available() {
        let command = EngineCommand::Seek(42.5);
        assert_eq!(command, command.clone());

        let info = PlaybackInfo::default();
        assert_eq!(info.state, PlaybackState::Stopped);
        assert_eq!(info.position_secs, 0.0);
        assert!((info.volume - 0.75).abs() < 1e-6);
        assert_eq!(info.cpu_overloads, 0);
        assert!(!info.resampler_disabled);
        assert!(!info.convolution_ir_needs_reload);

        assert!((flush_denormal(0.0) - 0.0).abs() < 1e-15);
        assert!((flush_denormal(1e-40) - 0.0).abs() < 1e-45);
        assert!((flush_denormal(1e-20) - 1e-20).abs() < 1e-25);
        assert!((flush_denormal(0.5) - 0.5).abs() < 1e-15);
    }
}
