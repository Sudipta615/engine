pub mod buffer;
pub mod commands;
pub mod decode;
pub mod dsp;
pub mod dsp_utils;
#[cfg(feature = "audio-output")]
pub mod engine;
#[cfg(feature = "audio-output")]
pub mod output;
pub mod paths;
pub mod playback_info;

// prelude, so `engine::AudioEngine` and `engine::EngineHandle` work directly (not just via prelude).
#[cfg(feature = "audio-output")]
pub use engine::{AudioEngine, EngineHandle};

pub use config::ResamplerQuality;
pub use decode::{extract_cover_art_to_cache, extract_track_metadata};

pub mod prelude {
    #[cfg(feature = "audio-output")]
    pub use crate::engine::{AudioEngine, EngineHandle, PlaybackStream};
    pub use crate::{
        buffer::{
            validate_audio_block, AudioBlockError, AudioChunk, AudioFrame, BufferError,
            EngineCommand, FixedFrameBuffer, PlaybackInfo, PlaybackState, DEFAULT_SAMPLE_RATE,
            MAX_AUDIO_BLOCK_FRAMES,
        },
        decode::{extract_cover_art_to_cache, extract_track_metadata},
        dsp::pipeline::{DspPipeline, OutputSampleFormat},
    };
    pub use config::ResamplerQuality;
}
