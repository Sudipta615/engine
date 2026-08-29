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
pub use playback_info::{PlaybackInfo, PlaybackState, SpatialTelemetry};
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
        dsp::graph2::{
            ExecutionOrder, Graph2, Graph2Error, NodeCapabilities, NodeDef, NodeId, NodeKind,
            NodeParams, OfflineExecutor, PortId, PortSpec, SignalType, SourceParams, TestSignal,
            ValidationReport,
        },
        dsp::pipeline::{DspPipeline, OutputSampleFormat},
        events::EngineEvent,
        playback_info::{PlaybackInfo, PlaybackState},
        playlist::{Playlist, RepeatMode},
        sink::{DacSink, NoopSink, SampleSink, VecSink},
        source::AudioSource,
        spatial::{
            encode_plane_wave, head_shadow_alpha, rotate_bus_frame, sh_foa, woodworth_itd_sec,
            AcousticBaker, AcousticPath, AcousticRoom, AcousticTransmission, AcousticWorld,
            AirAbsorption, AmbisonicDecoder, AmbisonicRenderer, BakePolicy, BakedObject, BakedPath,
            BakedScene, BasicPanner, BedId, BinauralRenderer, CustomDirectivity, DecoderPolicy,
            DiffractionEdge, Directivity, DistanceModel, Ear, FieldId, HeadSample, HeadShadow,
            HeadTracker, HybridBlockInputs, LayoutCalibration, Listener, MaterialKind,
            MaterialSpectrum, ObjectAudioRef, ObjectId, Occlusion, PathKind, Portal, Quat,
            RenderError, RendererKind, Room, SpatialAudioObject, SpatialBed, SpatialBedStore,
            SpatialField, SpatialFieldStore, SpatialObjectStore, SpatialRenderer, SpatialScene,
            Speaker, SpeakerId, SpeakerLayout, TrackingConfig, VbapRenderer, Vec3, Wall,
        },
    };
    pub use config::{AudioBackend, ChannelPolicy, ResamplerQuality};
}
