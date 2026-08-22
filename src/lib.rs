pub mod buffer;
pub mod commands;
pub mod decode;
pub mod dsp;
pub mod dsp_utils;
#[cfg(feature = "audio-output")]
pub mod engine;
pub mod events;
#[cfg(feature = "audio-output")]
pub mod output;
pub mod paths;
pub mod playback_info;
pub mod source;

// Re-exports for convenience
pub use commands::EngineCommand;
pub use events::EngineEvent;
pub use playback_info::{PlaybackInfo, PlaybackState};
pub use source::AudioSource;

#[cfg(feature = "audio-output")]
pub use engine::{AudioEngine, EngineError, EngineHandle};

pub use config::{AudioBackend, EngineConfig, ResamplerQuality};
pub use decode::extract_track_metadata;

pub mod prelude {
    #[cfg(feature = "audio-output")]
    pub use crate::engine::{AudioEngine, EngineError, EngineHandle, PlaybackStream};
    pub use crate::{
        buffer::{
            validate_audio_block, AudioBlockError, AudioChunk, AudioFrame, BufferError,
            FixedFrameBuffer, DEFAULT_SAMPLE_RATE, MAX_AUDIO_BLOCK_FRAMES,
        },
        commands::EngineCommand,
        decode::extract_track_metadata,
        dsp::pipeline::{DspPipeline, OutputSampleFormat},
        events::EngineEvent,
        playback_info::{PlaybackInfo, PlaybackState},
        source::AudioSource,
    };
    pub use config::{AudioBackend, ResamplerQuality};
}
