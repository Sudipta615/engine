pub mod audio_io;
pub mod buffer;
pub mod commands;
pub mod decode;
pub mod dsp;
pub mod dsp_utils;
#[cfg(feature = "audio-output")]
pub mod engine;
pub mod events;
#[cfg(feature = "c-ffi")]
pub mod ffi;
#[cfg(feature = "audio-output")]
pub mod output;
pub mod paths;
pub mod playback_info;
pub mod playlist;
pub mod sink;
pub mod source;
pub mod spatial;

// Re-exports for convenience
pub use commands::EngineCommand;
pub use events::EngineEvent;
pub use playback_info::{PlaybackInfo, PlaybackState};
pub use playlist::{Playlist, RepeatMode};
pub use source::AudioSource;

#[cfg(feature = "audio-output")]
pub use engine::{AudioEngine, EngineError, EngineHandle};

#[cfg(feature = "network-streaming")]
pub use audio_io::NetworkByteSource;
pub use audio_io::{AudioByteSource, FileByteSource, MemoryByteSource};
pub use config::{AudioBackend, EngineConfig, ResamplerQuality};
pub use decode::extract_track_metadata;
pub use sink::{DacSink, NoopSink, SampleSink, VecSink};

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
        playlist::{Playlist, RepeatMode},
        sink::{DacSink, NoopSink, SampleSink, VecSink},
        source::AudioSource,
        spatial::{
            AcousticTransmission, AirAbsorption, BasicPanner, CustomDirectivity, Directivity,
            DistanceModel, LayoutCalibration, Listener, ObjectAudioRef, ObjectId, Occlusion, Quat,
            RenderError, RendererKind, SpatialAudioObject, SpatialObjectStore, SpatialRenderer,
            SpatialScene, Speaker, SpeakerId, SpeakerLayout, VbapRenderer, Vec3,
        },
    };
    pub use config::{AudioBackend, ChannelPolicy, ResamplerQuality};
}
