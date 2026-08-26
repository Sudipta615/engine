use crate::source::AudioSource;

#[derive(Debug, Clone, PartialEq)]
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

    // ── Playlist / queue ─────────────────────────────────────────────────
    /// Append a source to the end of the playback queue.
    Enqueue(AudioSource),
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
}
