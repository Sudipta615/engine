use crate::source::AudioSource;

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum EngineCommand {
    Play,
    Pause,
    Stop,
    /// Seek to position in seconds. Must be finite and >= 0; invalid values are ignored.
    Seek(f32),
    SetVolume(f32),
    /// Set volume directly in dB. Range: [-60.0, 0.0]. Values below
    /// -60 dB are treated as mute. This is the perceptually-correct API
    /// — UI percentages should be converted to dB via a logarithmic curve
    /// (see `DspPipeline::volume_percent_to_db`) before being sent here.
    /// The legacy `SetVolume(f32)` command takes a linear 0.0–1.0 gain;
    /// prefer `SetVolumeDb` for new code.
    SetVolumeDb(f32),
    SetSpeed(f32),
    /// Open an explicit audio source for playback.
    Open(AudioSource),
    /// Prepare the next audio source for gapless / crossfade transition.
    PrepareNext(AudioSource),

    // ── Multi-track lanes ───────────────────────────────────
    /// Add a track as an independent lane on the first free mix-bus slot
    /// ≥ 2, playing alongside the primary stream.
    AddTrack(AudioSource),
    /// Remove the lane on the given mix-bus slot (if any) and silence it.
    RemoveTrack(u8),
    /// Set a lane's linear gain in [0, 1].
    SetTrackGain {
        slot: u8,
        gain: f32,
    },
    /// Set a lane's pan in [-1, 1].
    SetTrackPan {
        slot: u8,
        pan: f32,
    },
    /// Set a lane's post-fader master-send gain in [0, 1]:
    /// scales the lane's contribution to the master sum. Independent of the
    /// user gain.
    SetTrackMasterGain {
        slot: u8,
        gain: f32,
    },
    /// Set a lane's post-fader aux-send gain in [0, 1]: taps
    /// the lane's signal into the aux bus accumulator.
    SetTrackSend {
        slot: u8,
        gain: f32,
    },
    /// Configure program-gated ducking across lanes (derivation): when the
    /// `source_slot`'s peak rises above `threshold_db`, the `targets` slots
    /// are attenuated by `depth_db`. `None`-style disabling is done with an
    /// empty `targets` list.
    DuckTracks {
        source_slot: u8,
        targets: Vec<u8>,
        threshold_db: f32,
        depth_db: f32,
        attack_ms: f32,
        release_ms: f32,
    },

    // ── Playlist / queue ─────────────────────────────────────────────────
    /// Append a source to the end of the playback queue.
    Enqueue(AudioSource),
    /// Remove and discard the next track from the playback queue.
    Dequeue,
    /// Remove the entry at `index` from the playback queue.
    RemoveFromPlaylist(usize),
    /// Clear the playback queue (does not stop the current track).
    ClearPlaylist,
    /// Jump directly to the entry at `index` and start playing it.
    PlayIndex(usize),
    /// Skip to the next queue entry (manual Next).
    Next,
    /// Skip to the previous queue entry (manual Previous).
    Previous,
    /// Set the repeat mode (Off / All / One).
    SetRepeatMode(crate::playlist::RepeatMode),
    /// Enable or disable shuffle.
    SetShuffle(bool),

    Shutdown,
    SetOutputBackend(config::AudioBackend),
    SetOutputDevice(Option<String>),
    #[cfg(feature = "audio-output")]
    SetEndpoints(Vec<config::EndpointConfig>),
    #[cfg(feature = "audio-output")]
    UpsertEndpoint(config::EndpointConfig),
    #[cfg(feature = "audio-output")]
    RemoveEndpoint(String),
    /// Runtime toggle of the Aux insert (the global convolution on
    /// the aux bus): `enabled` + `wet_mix` only — the impulse response stays
    /// as configured. No-op when no IR engine exists yet.
    SetAuxInsert {
        enabled: bool,
        wet_mix: f32,
    },
    /// Plugin host: live enable toggle for the whole plugin
    /// insert (all slots). Disabled = the plan step is skipped,
    /// bit-exact; attached plugin instances stay loaded.
    SetPluginEnabled(bool),
    /// Plugin host: live parameter batch. Indices are the
    /// plugin's declared parameter indices; the batch is plain data and
    /// is applied atomically at the next block boundary.
    SetPluginParams(Vec<(u32, f32)>),
    /// Spatial master: renderer quality tier (spec §86).
    SetSpatialQuality(config::SpatialQuality),
    /// Spatial master: voice budget (spec §76). `enabled == false`
    /// clears it (full admission).
    SetSpatialVoice {
        enabled: bool,
        capacity: usize,
        full_quality_capacity: usize,
        policy: config::VoicePriority,
    },
    /// Spatial master: attach/clear a scalar automation curve
    /// (gain or spread) to a program object (0 = L, 1 = R), and set the
    /// scene automation clock to `time_secs`. The curve is built off the
    /// audio thread and evaluated allocation-free at block rate (spec §47).
    SetSpatialAutomation {
        object: u8,
        kind: u8,
        curve: Option<std::sync::Arc<crate::spatial::automation::CurveScalar>>,
        time_secs: f32,
    },
    /// Spatial master: drive program-object automation at `seconds`.
    SetSpatialAutomationTime(f32),
    /// Listener motion (v4.3.0): set the target listener pose —
    /// world-space orientation (quaternion) + position (metres). The
    /// spatial master glides toward it every processed block (shortest-arc
    /// nlerp on orientation, one-pole on position) per its tracking
    /// policy, so a moving listener sweeps the world-fixed image smoothly.
    SetSpatialListenerPose {
        orientation: crate::spatial::math::Quat,
        position: crate::spatial::math::Vec3,
    },
    /// Listener motion: the glide's smoothing policy (one-pole
    /// time constant in ms — `0` snaps; optional angular rate limit in
    /// deg/s — `0` unlimited).
    SetSpatialListenerTracking {
        smoothing_ms: f32,
        max_angular_rate_deg_s: f32,
    },
    /// Scene animation (v4.4.0): replace the spatial master's
    /// cue bank from the scene-file model (the `whoosh` / `door` presets
    /// live on `config::SpatialCueConfig`). Control path; the bank is
    /// rebuilt off the audio thread and then only read.
    SetSpatialCues(Vec<config::SpatialCueConfig>),
    /// Scene animation: fire the named cue at the block
    /// boundary (evaluated relative to the firing instant).
    TriggerSpatialCue(String),
    /// Scene animation: stop the active cue on `target`
    /// (program object 0 = L, 1 = R).
    StopSpatialCue(usize),
    /// Scene animation: stop every active cue.
    StopAllSpatialCues,
    /// Room/headphone correction: live enabled toggle (the loaded IR stays;
    /// disabled = the plan step is skipped, bit-exact).
    SetCorrectionEnabled(bool),
    /// Room/headphone correction: live wet/dry depth in [0, 1] (1.0 = fully
    /// corrected).
    SetCorrectionDepth(f32),
    /// Room/headphone correction: load a measured IR file and derive the
    /// Correction from it (IR conditioning → derivation regularized inverse → phase
    /// render, using the config's target / boost clamp / smoothing / phase
    /// mode), then enable it. A missing or unreadable file keeps the
    /// previous correction (or none) — never a failure state.
    LoadCorrectionIr(std::path::PathBuf),
    /// Room measurement orchestration: generate the exponential
    /// sine sweep, play it on the primary stream, capture it (WASAPI
    /// Loopback on Windows), then run sweep deconvolution → IR conditioning to derivation and land
    /// the result as the live correction. Progress / completion surface as
    /// `MeasurementProgress` / `MeasurementComplete { path, snr_db }`;
    /// without a capture backend the sweep still plays and
    /// `MeasurementFailed` fires.
    MeasureRoom {
        /// Sweep duration in seconds (0.25–120).
        seconds: f32,
        /// Pre-emphasis in dB; > 0 flattens the sweep's spectral energy
        /// density for HF SNR.
        pre_emphasis: f32,
    },
    SetEqEnabled(bool),
    /// Enable or disable automatic EQ headroom. When enabled, the engine
    /// reserves the curve's own peak boost as pre-EQ attenuation and keeps it
    /// updated as bands change; disabling restores the manual headroom.
    SetEqAutoHeadroom(bool),
    SetEqBand {
        index: usize,
        frequency: f32,
        gain_db: f32,
        q: f32,
        enabled: bool,
    },
    SetEqBandParams {
        index: usize,
        frequency: f32,
        gain_db: f32,
        q: f32,
        filter_type: crate::dsp::equalizer::EqFilterType,
        enabled: bool,
    },
    SetResamplerQuality(config::types::enums::ResamplerQuality),
    /// Apply a complete EQ preset (e.g. an AutoEQ result). Replaces the
    /// pipeline's bands and preamp with the preset's; enables the EQ.
    SetEqPreset(config::EqPreset),
    /// Activate the graphic EQ layer with a new band layout (all sliders
    /// reset to 0 dB) and sync it into the pipeline.
    SetGraphicEqLayout(config::GraphicEqLayout),
    /// Set one graphic EQ slider in dB (activating the layer).
    SetGraphicEqSlider {
        band: usize,
        gain_db: f32,
    },
    /// Set the graphic EQ preamp in dB.
    SetGraphicEqPreamp(f32),
    /// Enable or disable the graphic EQ layer.
    SetGraphicEqEnabled(bool),
    /// Install a per-output profile and apply it to the active device. The
    /// profile's backend preference is honored at stream (re)creation.
    #[cfg(feature = "audio-output")]
    SetOutputProfile(crate::output::OutputProfile),
    /// Remove the explicit output profile (auto-selection resumes).
    ClearOutputProfile,
    SetBassShelf(f32),
    SetTrebleShelf(f32),
    SetPreamp(f32),
    SetStereoWidth(f32),
    SetBalance(f32),
    SetDitherEnabled(bool),
    SetMidsideEq(bool),
    SetCrossfeedEnabled(bool),
    SetCrossfeedProfile(config::types::enums::CrossfeedProfile),
    SetCrossfeedCustomParams {
        frequency_hz: f32,
        q: f32,
        delay_ms: f32,
    },
    SetCompressorEnabled(bool),
    SetCompressorBandParams {
        band: usize, // 0=Low, 1=Mid, 2=High
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    },
    /// Request stream recovery after a device disconnection or error.
    RecoverStream,
    /// Automatically triggered stream recovery from the background monitor thread.
    AutoRecoverStream,
    /// Result of a background loudness scan for a loaded track. The engine
    /// applies the measured metadata only if `path` still matches the
    /// currently loaded track.
    LoudnessScanComplete {
        path: std::path::PathBuf,
        result: Option<crate::decode::LoudnessScanResult>,
    },

    /// Scan a file for EBU R128 / ReplayGain loudness and write the result
    /// into the file's tags (requires the `tag-write` feature). Emits a
    /// `LoudnessScanComplete` event when done.
    WriteLoudnessTags(std::path::PathBuf),

    // ── New Poweramp-class commands ─────────────────────────────────────
    /// Set the output sample rate policy.
    /// The engine will apply this on the next stream restart.
    SetSampleRatePolicy(config::SampleRatePolicy),

    /// Set the DSP processing precision (f32 Performance or f64 Quality).
    SetPrecisionMode(crate::dsp::pipeline::PrecisionMode),

    /// Enable or disable bit-perfect mode.
    /// When enabled, all DSP stages are bypassed; only volume and seek fades
    /// are preserved.
    SetBitPerfect(bool),

    /// Set the limiter post-gain mode (Transparent or Saturate).
    SetLimiterMode(crate::dsp::limiter::LimiterMode),

    /// Enable or disable true-peak FIR oversampling on the limiter.
    SetLimiterTruePeak(bool),

    /// Set volume control mode (Software DSP vs Hardware Endpoint).
    SetVolumeMode(config::VolumeMode),

    /// Set fallback policy for exclusive mode (Strict vs Allow).
    SetFallbackPolicy(config::FallbackPolicy),

    /// Set crossfade configuration.
    SetCrossfadeConfig(config::CrossfadeConfig),

    /// Set crossfade curve shape.
    SetCrossfadeCurve(config::CrossfadeCurve),

    /// Set transition mode (Gapless, Crossfade, Fade, Stop).
    SetTransitionMode(config::TransitionMode),

    /// Set playback speed mode (Varispeed, TimeStretch, PitchShift).
    SetSpeedMode(config::SpeedMode),

    /// Set pitch shift in semitones (effective in TimeStretch/PitchShift mode).
    SetPitch(f32),

    // ── Multichannel & Spatial DSP commands ─────────────────────────────
    /// Set explicit upmix/downmix channel mix template or custom matrix.
    SetChannelMix(config::ChannelMixConfig),

    /// Set multichannel preservation policy (StereoDownmix, PassThrough, MaxChannels).
    SetChannelPolicy(config::ChannelPolicy),

    /// Set per-channel trim (gain, delay, polarity) for multichannel output.
    SetChannelTrim(config::ChannelTrimConfig),

    /// Set multichannel source->destination routing matrix.
    SetChannelRouting(config::ChannelRoutingConfig),

    /// Set per-channel parametric EQ for multichannel output.
    SetChannelEq(config::ChannelEqConfig),

    /// Set LFE management configuration (subwoofer gain and low-pass crossover).
    SetLfeConfig(config::LfeConfig),

    /// Set bass management configuration (mains high-pass and shared crossover).
    SetBassManagement(config::BassManagementConfig),

    /// Open the active ASIO driver's manufacturer settings dialog
    /// (`IASIO::controlPanel`). No-op when the current output backend is not
    /// ASIO or the `asio-native` feature is not compiled in.
    OpenAsioControlPanel,

    // ── System audio capture ───────────────────────────────────────────
    /// Start capturing the system mix (WASAPI loopback) to a WAV file.
    /// `path` defaults to `capture.wav`. No-op unless the `wasapi-native`
    /// feature is compiled in on Windows.
    CaptureStart {
        /// Output WAV path (`None` → `capture.wav` in the current directory).
        path: Option<std::path::PathBuf>,
        /// Render endpoint to capture (`None` → system default).
        device: Option<String>,
    },
    /// Stop the active system-audio capture and finalize its WAV file.
    CaptureStop,

    // ── Runtime DSP & Mix Controls (Punch List P1 Items 20 & 21) ───────
    /// Spatial master: live enabled toggle.
    SetSpatialEnabled(bool),
    /// Spatial master: virtual screen geometry and gain.
    SetSpatialScreen {
        center_azimuth_deg: f32,
        half_width_deg: f32,
        elevation_deg: f32,
        gain: f32,
    },
    /// Spatial master: acoustic room reflections and reverberation.
    SetSpatialRoom {
        enabled: bool,
        width: f32,
        depth: f32,
        height: f32,
        absorption: f32,
        reflection_order: u8,
        rt60_ms: f32,
        late_mix: f32,
        late_distance: bool,
        wet: f32,
    },
    /// Spatial master: atmospheric air absorption simulation.
    SetSpatialAir(crate::spatial::level::AirAbsorption),
    /// Spatial master: listener head orientation in degrees.
    SetSpatialListener {
        yaw_deg: f32,
        pitch_deg: f32,
        roll_deg: f32,
    },
    /// Active HRTF profile selection (Item 21).
    SetHrtfProfile(String),

    /// Limiter: live enabled toggle.
    SetLimiterEnabled(bool),
    /// Limiter: full parameter set.
    SetLimiterParams {
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool,
    },
    /// Multiband compressor: advanced band features (knee, detector mode, stereo link).
    SetCompressorBandFeatures {
        band: usize,
        knee_db: f32,
        detector: config::CompressorDetector,
        stereo_link: bool,
    },

    /// Stereo enhancer: live enabled toggle.
    SetStereoEnhancerEnabled(bool),

    /// Loudness normalization mode.
    SetLoudnessMode(config::LoudnessMode),

    /// Mix-lane / bus: per-input channel trim gain and polarity.
    SetSlotTrim {
        slot: u8,
        channel: usize,
        gain_db: f32,
        invert_polarity: bool,
    },
    /// Mix-lane / bus: aux bus enabled and return gain.
    SetAux {
        enabled: bool,
        return_gain: f32,
    },
    /// Mix-lane / bus: mute state for an input lane.
    SetInputMute {
        slot: u8,
        muted: bool,
    },
    /// Mix-lane / bus: active / detached state for an input lane.
    SetInputActive {
        slot: u8,
        active: bool,
    },
    /// Mix-lane / bus: parameter automation curve for a mix slot.
    SetSlotAutomation {
        slot: u8,
        kind: u8,
        curve: Option<std::sync::Arc<crate::spatial::automation::CurveScalar>>,
        time_secs: f32,
    },
}
