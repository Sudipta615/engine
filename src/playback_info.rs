pub const DEFAULT_SAMPLE_RATE: u32 = 44100;

/// Re-export DSP stats for consumers of this type.
pub use crate::dsp::pipeline::EngineStats;
#[cfg(feature = "audio-output")]
pub use crate::output::output_info::OutputInfo;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
    Buffering,
}

#[derive(Debug, Clone)]
pub struct PlaybackInfo {
    pub state: PlaybackState,
    /// Decoded/source position in seconds (the playhead at the *decoder*).
    /// This is the raw sample-clock position and takes no account of the
    /// pipeline's own latency — the audio heard at the DAC lags it by
    /// [`Self::latency_ms`].
    pub position_secs: f32,
    /// [`Self::position_secs`] minus the end-to-end graph latency, clamped at
    /// 0: the position of what is actually audible at the DAC. `None`-style
    /// semantics are avoided — it is simply 0 until playback has progressed
    /// past the pipeline latency.
    pub position_secs_compensated: f32,
    /// End-to-end graph latency (ms) at the time the position was sampled
    /// (limiter lookahead + convolution + crossfeed + timestretch +
    /// resampler + ring buffer + device buffer).
    pub latency_ms: f32,
    pub duration_secs: f32,
    pub volume: f32,
    /// Set when a hardware-endpoint volume change could not be applied to
    /// the device (e.g. no native volume backend on the current platform).
    /// `volume` is NOT updated on failure, so the displayed value always
    /// reflects what the hardware actually did. Cleared on the next
    /// successful volume change.
    pub volume_error: Option<String>,
    /// Which volume path is actually in the signal chain: `Hardware` when the
    /// endpoint owns the level (DSP at unity), `Software` when the DSP gain
    /// stage applies it (SoftwareOnly, or HardwarePreferred fallback).
    /// `None` until the first volume change or mode switch.
    pub volume_path: Option<crate::dsp::pipeline::VolumePath>,
    pub speed: f32,
    pub current_source: Option<crate::source::AudioSource>,
    pub sample_rate: u32,
    pub cpu_usage_pct: f32,
    /// Number of audio dropouts / CPU overloads detected
    pub cpu_overloads: u64,
    /// Whether the resampler has been disabled due to creation or rebuild failures.
    pub resampler_disabled: bool,
    /// Whether the resampler encountered an unrecoverable failure and playback
    /// was halted to prevent playing at wrong speed/pitch.
    pub resampler_failed_fatal: bool,
    /// Whether the convolution engine's loaded IR has a stale frequency
    /// mapping due to a sample rate change and needs to be reloaded.
    pub convolution_ir_needs_reload: bool,
    /// Latest fatal engine error that requires UI intervention or playback halt.
    pub engine_error: Option<String>,
    /// Number of samples that exceeded ±1.0 in the output callback and
    /// were hard-clamped. A non-zero value indicates the upstream DSP
    /// produced overshoots that the limiter failed to catch (e.g.
    /// limiter disabled, ceiling too high, or true-peak overshoot).
    /// Reset on read by the engine.
    pub clip_count: u64,
    /// Number of non-finite (NaN/Inf) samples encountered in the output
    /// callback. Any non-zero value is a serious numerical bug in the
    /// DSP. Reset on read by the engine.
    pub nan_count: u64,

    // ── New diagnostics fields ───────────────────────────────────────────
    /// Rich DSP diagnostic snapshot. Updated every engine tick.
    /// `None` until the engine starts playing.
    pub engine_stats: Option<EngineStats>,

    /// Actual vs. requested output backend/rate.
    /// `None` until the audio output stream is opened.
    #[cfg(feature = "audio-output")]
    pub output_info: Option<OutputInfo>,

    /// Whether the current output is believed to be fully bit-perfect.
    /// True only when: exclusive mode confirmed + bit_perfect DSP bypass +
    /// no sample-rate conversion needed.
    pub bit_perfect: bool,
    /// Whether the current track is being output as DSD-over-PCM (DoP): raw
    /// DSD packed into 24-bit frames, DSP bypassed, no decimation.
    pub dop_active: bool,
    /// Whether the current track is being output as native DSD: raw 1-bit
    /// bitstream to a DSD-capable DAC, entire f32 DSP path structurally
    /// bypassed.
    pub native_dsd_active: bool,
    /// The DSD transport actually in use for the current track.
    pub dsd_transport: crate::decode::DsdTransport,
    /// Full DSD transport negotiation report (§7, §28): requested vs actual
    /// transport, the negotiated wire format (for native DSD), the bit rate,
    /// and the ordered fallback steps. Exposes every downgrade so a UI can
    /// render the exact `DSD source → native DSD unavailable → DoP → PCM`
    /// chain instead of a single transport label.
    pub dsd_transport_report: crate::decode::DsdTransportReport,
    /// ID of the output profile currently applied to the active device
    /// (§10). `None` when no profile is active.
    pub active_output_profile: Option<String>,
    /// Current playlist entry index (`None` when the queue is empty).
    pub playlist_index: Option<usize>,
    /// Number of entries in the playback queue.
    pub playlist_length: usize,
    /// Multi-track lane telemetry (Phase 4 S6): one entry per active lane,
    /// refreshed every engine tick from the lane registry + the mix bus's
    /// per-slot meters.
    pub lanes: Vec<LaneInfo>,
    /// Multi-endpoint routing-matrix telemetry (roadmap Phase 5): one entry
    /// per started additional output endpoint, refreshed every engine tick.
    /// Empty in single-endpoint mode.
    pub endpoints: Vec<EndpointInfo>,
}

/// Telemetry for one additional output endpoint.
#[derive(Debug, Clone)]
pub struct EndpointInfo {
    /// Device name from the endpoint's config.
    pub device: String,
    /// The endpoint's negotiated sample rate (its own rate domain).
    pub sample_rate: u32,
    /// Per-endpoint level in [0, 1].
    pub gain: f32,
    /// Frames buffered ahead of the endpoint's ring (resampler/limiter
    /// tail not yet accepted by the device). A persistently large value
    /// means the endpoint cannot keep up with the master mix.
    pub pending_frames: usize,
}

/// Telemetry for one playback lane (Phase 4 S6).
#[derive(Debug, Clone)]
pub struct LaneInfo {
    /// Mix-bus slot (≥ 2).
    pub slot: u8,
    /// The source being played.
    pub source: Option<crate::source::AudioSource>,
    /// User gain in [0, 1].
    pub gain: f32,
    /// User pan in [-1, 1].
    pub pan: f32,
    /// Whether the lane is contributing audio (false once its stream ends).
    pub active: bool,
    /// Peak level (dBFS) of the lane's bus slot (max over its channels),
    /// from the graph's per-slot meters (Phase 4 S3).
    pub level_db: f32,
    /// Post-fader master-send gain in [0, 1] (Phase 5 S2).
    pub send_master_gain: f32,
    /// Post-fader aux-send gain in [0, 1] (Phase 5 S2).
    pub send_aux_gain: f32,
    /// Decoded position in seconds.
    pub position_secs: f32,
    /// Source duration in seconds.
    pub duration_secs: f32,
}

impl Default for PlaybackInfo {
    fn default() -> Self {
        Self {
            state: PlaybackState::Stopped,
            position_secs: 0.0,
            position_secs_compensated: 0.0,
            latency_ms: 0.0,
            duration_secs: 0.0,
            volume: 0.75,
            volume_error: None,
            volume_path: None,
            speed: 1.0,
            current_source: None,
            sample_rate: DEFAULT_SAMPLE_RATE,
            cpu_usage_pct: 0.0,
            cpu_overloads: 0,
            resampler_disabled: false,
            resampler_failed_fatal: false,
            convolution_ir_needs_reload: false,
            engine_error: None,
            clip_count: 0,
            nan_count: 0,
            engine_stats: None,
            #[cfg(feature = "audio-output")]
            output_info: None,
            bit_perfect: false,
            dop_active: false,
            native_dsd_active: false,
            dsd_transport: crate::decode::DsdTransport::PcmConversion,
            dsd_transport_report: crate::decode::DsdTransportReport::default(),
            active_output_profile: None,
            playlist_index: None,
            playlist_length: 0,
            lanes: Vec::new(),
            endpoints: Vec::new(),
        }
    }
}
