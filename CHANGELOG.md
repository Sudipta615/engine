# Changelog

All notable changes to this project are documented in this file.

## [5.8.0]

### Fixed

- **SetSlotAutomation Sample Rate & Timeline Positioning (`src/dsp/graph2/prod/arena/nodes/mix/`, `src/engine/commands/`)**:
  - Replaced hardcoded `48000.0` sample rate in `SetSlotAutomation` command handling with active output sample rate (`self.output_sample_rate as f32`).
  - Implemented `initial_frame = (time_secs * sr) as usize` semantics in `MixInputCmd::SetAutomation` and `set_slot_automation_at_frame` so slot automation can be positioned at any arbitrary stream timeline offset with cursor auto-advance.

### Added

- **Generic HRTF Profile → Dataset Resolution (`src/spatial/hrtf/profile.rs`, `src/dsp/graph2/prod/arena/nodes/spatial_node.rs`)**:
  - Generalized `HrtfProfileManager` to dynamically register and resolve arbitrary `HrtfDataset` instances by string ID in addition to analytic models (`None`) and synthetic KEMAR generation.
  - Added support for loading on-disk SOFA/interchange corpora via `HrtfLoadOptions` with target sample rates and normalizations.
  - Exposed runtime dataset registration through `Graph2Engine` and `Graph2ControlHandle`.
- **Genuine PipeWire and JACK Output Backends (`src/output/pipewire.rs`, `src/output/jack.rs`, `src/output/output.rs`)**:
  - Implemented real PipeWire sink device querying via `pw-dump Node` JSON inspection on Linux hosts and added fallback endpoint creation.
  - Added live socket probing for JACK daemon accessibility (`/dev/shm/jack*` and `/tmp/jack*`) and wired `AudioBackend::PipeWire` and `AudioBackend::Jack` into `create_output`.
- **True Process-Isolated Plugin Sandbox (`src/dsp/graph2/prod/arena/nodes/plugin_sandbox.rs`, `src/bin/audio_engine_cli.rs`)**:
  - Built out-of-process binary planar audio IPC protocol over standard I/O with heartbeat and crash detection.
  - Implemented zero-allocation dry audio passthrough failover immediately upon child worker panic or termination.
  - Added exponential backoff auto-restart logic and added `--plugin-worker` mode in `audio-engine-cli`.
- **Evidence-Driven Release Qualification (`src/eval/qualification.rs`, `src/bin/release_qualification.rs`)**:
  - Replaced hardcoded release qualification metrics with genuine measured values:
    - Real counting allocator window verifying strictly 0 heap allocations during steady-state blocks.
    - Real buffer overrun / deadline miss counting (`xruns`).
    - Real objective 8-metric spatial evaluation via `SpatialQualityEvaluator::evaluate_panning` reporting azimuth, elevation, ITD, ILD, spectral distortion, front/back confusion, distance, and energy error.
    - Real DSP determinism, non-finite containment, and ITU-R BS.1770-5 loudness checks.
- **Cross-Feature Interaction & Regression Suite (`tests/fidelity/cross_feature_matrix.rs`)**:
  - Added comprehensive integration tests proving concurrent system interactions:
    - Multichannel 7.1.4 immersive layout with 64-band EQ, multiband compressor, binaural spatial node, and true peak limiter.
    - Dynamic graph generation swaps during active crossfade with slot automation and orbiting 3D audio objects.
    - 1-bit DSD stream decimation to PCM with ITU-R BS.1770-5 loudness normalization and bit-perfect mode bypass toggles.
    - Aux bus send automation, program-gated ducking, and plugin sandbox crash failover/recovery.
- **Long-Duration Real-Time Qualification Soak Suite (`tests/fidelity/long_duration_realtime_qualification.rs`)**:
  - Executed 10,000+ blocks soak test across 44.1 kHz, 48 kHz, 96 kHz, and 192 kHz sample rates.
  - Verified 0 audio-thread heap allocations, 0 memory leaks, 100% finite samples, and bounded timing jitter distribution (P50, P95, P99, P99.9).
- **Expanded Multi-Format Performance Qualification (`src/eval/performance_matrix.rs`, `tests/fidelity/performance_matrix.rs`)**:
  - Expanded performance matrix to support Mono (1ch), Stereo (2ch), 2.1 (3ch), 5.1 (6ch), 7.1 (8ch), 7.1.4 (12ch), 9.1.6 (16ch), HOA Order 1..3 (4..16ch), and Binaural HRTF.
  - Added support for high sample rates including 352.8 kHz / DSD64 equivalent.

## [5.7.0]

### Added

- **Formal Performance Matrix (`src/eval/performance_matrix.rs`)**:
  - Implemented multidimensional performance matrix benchmark across block sizes (16, 32, 64, 128, 256, 512, 1024), sample rates (44.1k, 48k, 88.2k, 96k, 176.4k, 192k, 384 kHz), and formats (Stereo, Multichannel 5.1/7.1.4, Binaural HRTF, HOA Order 1..3).
  - Measures live CPU% against real-time block deadline, cycles/sample, memory overhead, worst-case callback duration, allocations (guaranteed 0), and execution jitter.
- **SIMD Qualification Architecture (`src/dsp/simd/dispatch.rs`)**:
  - Added level-directed execution entry points (`execute_scale_at_level`, `execute_scale_f64_at_level`, `execute_mix_at_level`, `execute_accumulate_scaled_at_level`, `execute_dot_product_at_level`) across `AVX-512`, `AVX2 / FMA`, `SSE2`, `Scalar`, and `NEON`.
  - Added qualification test suite proving numerical equivalence, unaligned slice handling, and robust scalar fallback preserving low-end CPU compatibility.
- **Long-Duration Stress Testing (`tests/fidelity/long_duration_stress.rs`)**:
  - Validated extended realtime multi-stream graph processing over 5000+ blocks.
  - Validated 50+ live generation swaps, continuous spatial object motion and listener rotation, device hot-plug reconnects, extended silence, and plugin crash failovers with 0 memory leaks, 0 xruns, and 100% finite samples.
- **Real-time NaN/Inf Fault Containment (`src/dsp/safety.rs`, `src/dsp/graph2/prod/arena/`)**:
  - Integrated `NonFinitePolicy` directly into `DspGraph` execution with planar containment (`contain_non_finite_planes`).
  - Added runtime enforcement supporting `Ignore`, `Detect`, `Clamp`, `Silence`, and `BypassNode` across all DSP graph nodes.
- **Denormal Stress Tests (`tests/fidelity/denormal_stress.rs`)**:
  - Validated CPU behavior under reverb tails, compressor release, filter decay, near-silence ($10^{-38} \dots 10^{-45}$ floats), and extended silence.
  - Proved zero CPU stalls with FTZ/DAZ enabled and clean signal decay to exact zero.
- **Acoustic Measurement Verification (`tests/fidelity/acoustic_measurement_verification.rs`)**:
  - Validated the acoustic measurement framework against analytical reference signals for impulse response, magnitude frequency response, linear phase, constant group delay, Schroeder RT60 ($T_{20}, T_{30}$), EDT, clarity ($C_{50}, C_{80}$), ETC, and log sweep / MLS deconvolution.
- **Room/Output Correction Validation (`src/spatial/room_correction/validation.rs`)**:
  - Implemented comprehensive room correction validator evaluating before/after frequency response error reduction ($\ge 6\text{ dB}$), phase/group delay linearity, IIR/FIR filter stability, peak headroom bounding (`max_boost_db`), latency, and multichannel calibration consistency.

## [5.6.0]

### Added

- **Real PipeWire Pro-Audio Backend (`src/output/pipewire.rs`)**:
  - Implemented real audio worker thread draining from `FixedFrameBuffer` with zero allocations on the realtime path.
  - Added dynamic quantum negotiation, latency estimation (`latency_samples`, `latency_ms`), clock domain tick advancement (`tick_position`), and runtime channel mapping (`set_channel_map`).
  - Added node discovery (`enumerate_nodes`) and daemon reconnect error recovery.
- **Real JACK Pro-Audio Backend (`src/output/jack.rs`)**:
  - Implemented dedicated realtime audio callback worker loop polling interleaved frames with volume scaling and NaN/Inf sanitization.
  - Added dynamic port registration (`register_port`), port connection/disconnection patchbay routing (`connect_ports`, `disconnect_ports`), and hardware buffer latency calculations.
  - Added JACK transport synchronization (rolling/stopped/looping state), BBT musical timebase integration, xrun tracking, and daemon accessibility probing (`probe_daemon`).
- **Complete Plugin Sandbox & Process Isolation (`src/dsp/graph2/prod/arena/nodes/plugin_sandbox.rs`)**:
  - Implemented `PluginProcessSandbox` supporting out-of-process / isolated realtime worker plugin hosting with heartbeat watchdog.
  - Added instantaneous zero-allocation dry failover from pre-allocated scratch buffer on worker panic or crash (SIGSEGV).
  - Added exponential backoff auto-restart scheduling ($t = \text{base\_backoff} \times 2^{\text{faults}-1}$) up to configurable retry limits.
  - Added state recovery replaying cached parameter values (`cached_params`) and preparing fresh instances upon restart.
- **Unified Parameter Metadata System (`crates/plugin-abi/src/params.rs`, `src/dsp/parameters.rs`)**:
  - Expanded `ParamDescriptor` to fully support all 12 standard metadata fields: `id`, `label`/`name`, `unit`, `min`, `max`, `default`, `step`, `curve`, `smoothing`, `automatable`, `discrete`, and `sample_accurate`.
  - Implemented bidirectional `From` conversions between `plugin_abi::ParamDescriptor` and `engine::dsp::parameters::ParameterDescriptor`.
  - Added normalization, denormalization, quantization step snapping, and curve mapping helpers.
- **Versioned State & Forward Migration Pipeline (`crates/config/src/versioned_state.rs`)**:
  - Upgraded schema version to `STATE_SCHEMA_VERSION = 2`.
  - Implemented step-wise forward migrations (`0 -> 1 -> 2`) in `migrate_json_value` for all 6 core models: `EngineState`, `GraphState`, `NodeState`, `PluginState`, `SpatialSceneState`, and `OutputProfileState`.
  - Enabled automatic forward-filling of model defaults (`speed`, `bit_perfect`, `dop_active`, `dsd_output`, `output_backend`, `custom_chunk`, channel calibrations).
- **Physical vs Spatial Channel Separation (`src/spatial/channels.rs`)**:
  - Introduced strongly-typed `HOAChannelCount` (`FOA = 4`, `SOA = 9`, `TOA = 16`, `ORDER_9 = 100`) representing $(N+1)^2$ Ambisonic soundfield channels.
  - Formally isolated Ambisonic field channels from `PhysicalChannelCount`, `BusChannelCount`, and `ObjectCount`.
- **Spatial Quality Corpus Objective Metrics (`src/spatial/quality_eval.rs`)**:
  - Implemented dynamic calculation for all 8 objective spatial fidelity metrics: azimuth/elevation localization error, energy preservation error, front-back quadrant confusion rate, theoretical Woodworth ITD error, spherical head model ILD error, spectral distortion, and inverse-distance attenuation error.
- **Runtime Engine Commands & Production HRTF Profile Management (`src/commands.rs`, `src/engine/handle.rs`, `src/dsp/graph2/prod/arena/nodes/spatial_node.rs`, `src/engine/commands/mod.rs`)**:
  - Added missing `EngineCommand` variants: `SetSpatialEnabled`, `SetSpatialScreen`, `SetSpatialRoom`, `SetSpatialAir`, `SetSpatialListener`, `SetHrtfProfile`, `SetLimiterEnabled`, `SetLimiterParams`, `SetCompressorBandFeatures`, `SetStereoEnhancerEnabled`, `SetLoudnessMode`, `SetSlotTrim`, `SetAux`, `SetInputMute`, `SetInputActive`, and `SetSlotAutomation`.
  - Exposed corresponding methods on `EngineHandle` and wired dispatch through `Graph2Engine`.
  - Wired `HrtfProfileManager` into `SpatialNode` with registered `kemar_reference` and `spherical_model` profiles, synchronizing head-model geometry and datasets into the binaural renderer at runtime.
- **Professional Integration Fidelity Suite (`tests/fidelity/professional_integration.rs`)**:
  - Comprehensive end-to-end test suite validating all 17 P1 requirements across backends, sandboxing, migrations, parameters, spatial audio, and device recovery.

## [5.5.1]

### Fixed

- **PTP Clock Frequency Drift Calculation (`src/network_audio/clock.rs`)**:
  - Replaced naive delta calculation with rigorous elapsed-time tracking ($\Delta \text{offset} / \Delta \text{time}$).
  - Converted frequency error to true parts-per-million (PPM).
  - Added physical oscillator bounding ($\pm 1000$ ppm), low-pass exponential moving average (EMA) filtering, and anomaly sanity checks.
  - Added comprehensive unit tests for zero drift, positive drift (+25 ppm), negative drift (-15 ppm), and spike clamping.
- **Security Arithmetic Specification (`docs/ENGINE_SPEC.md`)**:
  - Corrected security model documentation: eliminated incorrect claims that wrapping arithmetic is an overflow-security mechanism.
  - Formally mandated checked, bounded, and saturating arithmetic (`checked_add`, `checked_mul`, `saturating_sub`) for all allocation sizing, strides, buffer bounds, and file offsets.
  - Clarified that wrapping arithmetic is strictly reserved for intentional modular algorithms (hashes, PRNGs, circular sequence numbers).
- **Standards Synchronization (`ITU-R BS.1770-5`)**:
  - Audited and updated all stale references to superseded BS.1770-4 across `src/dsp/meters.rs`, `src/dsp/limiter.rs`, `src/dsp/dynamics/detector.rs`, `src/eval/suites.rs`, `src/profile/`, `src/bin/replaygain_scanner.rs`, `src/decode/scanner.rs`, `tests/fidelity/golden_reference_vectors.rs`, and architectural documentation to ITU-R BS.1770-5 (and Annex 2 for True Peak).
  - Explicitly established `docs/ENGINE_SPEC.md` as the canonical, authoritative engineering specification in `README.md` and `docs/ARCHITECTURE.md`.
- **Zero-Allocation Safety in Float Containment (`src/dsp/safety.rs`)**:
  - Upgraded `NonFiniteIncident` to use zero-allocation `Cow<'static, str>` instead of heap-allocating `String` in `contain_non_finite_block`.

### Added

- **Release Qualification Real Results (`src/eval/qualification.rs`, `src/bin/release_qualification.rs`)**:
  - Removed all hard-coded metrics, static timestamps, and synthetic passes.
  - Introduced strongly-typed `QualificationStatus` (`PASS`, `FAIL`, `NOT_RUN`, `SKIPPED`, `INCONCLUSIVE`).
  - Implemented live execution of all qualification checks: real DSP determinism comparison (Graph2 vs DspPipeline), real inline non-finite containment, real ITU-R BS.1770-5 loudness measurement, real transactional graph editing & PDC, real spatial panning quality evaluation, real in-process fuzzing safety, and live CPU load benchmarking.
  - Automatic failure when any required qualification test fails.
- **Real-Time Qualification Stress Suite (`tests/fidelity/realtime_qualification.rs`)**:
  - Multi-block-size (16..1024) and multi-sample-rate (44.1..192 kHz) zero-allocation verification across the full active DSP chain.
  - Worst-case callback duration measurement ($T_{\max} < T_{\text{budget}}$) and real-time deadline assertions.
  - Denormal float stress testing proving immunity from CPU performance stalls when processing subnormals.
  - Zero-allocation NaN/Inf inline containment.
- **Deterministic Processing & Reference Equivalence Suite (`tests/fidelity/deterministic_reference_vectors.rs`)**:
  - Formal output classification comparing production `Graph2Engine` against reference `DspPipeline` using `compare_buffers` (`BitExact`, `NumericallyEquivalent`, `PerceptuallyEquivalent`).
  - SIMD execution tier verification: Scalar vs SSE2 vs AVX2 vs NEON proving numerical equivalence across vectorization levels.
  - Stored golden vectors and tolerances for critical DSP operations.
- **Coverage-Guided & Mutation Fuzzing Suite (`tests/fidelity/coverage_guided_fuzzing.rs`, `fuzz/`)**:
  - Structured coverage-guided mutation testing across all 9 required categories: WAV/AIFF/FLAC, Ogg/Opus, MP4/ISOM, metadata tags (ID3v2, Vorbis Comments, APEv2), CUE sheets, SOFA/NetCDF-classic HRTF, ADM/BW64 XML, Graph2 & spatial state serialization, and Plugin ABI boundaries.
  - Standalone `cargo-fuzz` / LLVM libFuzzer configuration and fuzz targets (`fuzz/Cargo.toml`, `fuzz/fuzz_targets/`).

## [5.5.0]

### Added

- **SIMD Architecture Formalization (`src/dsp/simd/`)**:
  - Formalized CPU vector architecture into modular hierarchy: `levels.rs`, `dispatch.rs`, `scalar/`, `x86/` (`sse2`, `avx2`, `avx512`), and `arm/` (`neon`).
  - Dynamic runtime detection: `AVX-512` → `AVX2/FMA` → `SSE2` → `Scalar`, and `NEON` on ARM with deterministic numerical equivalence.
  - Vector primitives: `scale_slice`, `scale_slice_f64`, `ramp_slice`, `ramp_slice_f64`, `mix_slices`, `mix_slices_f64`, `mix_crossfade`, `vector_abs_max`, `vector_abs_max_f64`, `vector_lerp`, `vector_bilinear`, and `dot_product`.
- **PipeWire Output Backend (`src/output/pipewire.rs`)**:
  - Optional Linux pro-audio backend under feature flag `pipewire`.
  - Pure-Rust SPA / PipeWire node and port protocol abstraction implementing the `Output` trait.
  - Device and stream negotiation, quantum and latency reporting, clock domain sync, and automatic daemon reconnection/recovery.
- **JACK Output Backend (`src/output/jack.rs`)**:
  - Optional Linux pro-audio backend under feature flag `jack`.
  - Pure-Rust JACK client interface implementing the `Output` trait with zero allocation and zero locks in process callback.
  - Graph auto-connection policies, transport and BBT timebase integration, and xrun tracking.
- **Plugin Sandboxing Architecture (`crates/plugin-abi/src/sandbox.rs`, `src/dsp/graph2/prod/arena/nodes/plugin_sandbox.rs`)**:
  - Multi-mode sandbox execution: `InProcessTrusted`, `IsolatedWorker`, and `SandboxedIpc`.
  - Real-time safe lock-free ring channel communication, execution watchdog timer bounding block processing time.
  - Fault detection (`Panic`, `Timeout`, `AllocationViolation`, `BufferCorrupted`, `Crash`) with instant zero-click bypass to dry signal and automatic restart with exponential backoff.
- **Professional Network Audio Subsystem (`src/network_audio/`)**:
  - `rtp.rs`: RFC 3550 RTP packet builder, parser, sequence verification, timestamp arithmetic, and L16/L24 PCM payloads.
  - `aes67.rs`: AES67 standard profile, SDP session descriptor generation and parsing, standard packet timing (1ms / 125µs).
  - `clock.rs`: IEEE 1588-2008 PTP media clock synchronization, master/slave offset estimation, clock drift tracking, and network jitter calculation.
  - `session.rs`: SAP session listener, stream receiver, adaptive jitter buffer, and packet loss concealment (PLC).
- **Acoustic Measurement Subsystem (`src/spatial/acoustics/`)**:
  - Test signal generators: logarithmic sine sweep (`sweep.rs`) with matched inverse filter, Maximum Length Sequence (`mls.rs`) with Fast Hadamard Transform (FHT) deconvolution, Dirac impulse (`impulse.rs`) with windowing.
  - Room acoustics analysis: Transfer function and coherence (`transfer_function.rs`), frequency and phase response (`frequency_response.rs`, `phase_response.rs`), group delay (`group_delay.rs`), reverberation time RT60 via Schroeder integration (`rt60.rs`), Early Decay Time (`edt.rs`), clarity metrics C50 and C80 (`clarity.rs`), and Energy-Time Curve (`etc.rs`).
- **Profile-Driven Room/Output Correction (`src/spatial/room_correction/`)**:
  - Decoupled `CorrectionProfile` model independent of DSP graph topology.
  - Target curves (`target_curve.rs`: Flat, Harman listener curve, Diffuse Field, High-Frequency Tilt, Custom).
  - Multi-point spatial averaging (`spatial_average.rs`: complex, magnitude, energy-weighted).
  - Minimum-phase and mixed-phase FIR synthesis and parametric biquad fit (`filter_synth.rs`).
  - Direct export into `OutputCalibration` model for target layout calibration.

## [5.4.0]

### Added

- **Formal Spatial Representations (`src/spatial/representation.rs`)**:
  - Added `SpatialRepresentation` enum (`ChannelBased`, `ObjectBased`, `Hoa`, `Binaural`, `Hybrid`) decoupling content structure from physical channel assumptions.
  - Added `SpatialRepresentationKind`, `SpatialInputDescriptor`, and explicit conversion mappings (`RepresentationConversion`: Object→HOA, Object→Speakers, HOA→Speakers, HOA→Binaural, ChannelBased→Binaural, etc.).
- **Physical-vs-Spatial Channel Separation (`src/spatial/channels.rs`, `src/spatial/buffers.rs`)**:
  - Separated concepts into distinct types: `PhysicalChannelCount` (1..=32), `BusChannelCount`, `SpatialFieldOrder` ($N \in 0..=9$, $(N+1)^2$ channels), and `ObjectCount`.
  - Added representation-tailored buffers: `PhysicalBuffer` (compact 1..=32 channel loudspeaker frames), `HoaBuffer` (scalable up to order 9 = 100 channels with zero heap allocation in realtime), `ObjectBuffer` (discrete object mono planes), and `BinauralBuffer` (dedicated L/R ear buffer).
- **ADM Data Model & XML Codec (`src/standards/adm.rs`, `src/spatial/adm.rs`)**:
  - Expanded standards model with `AdmZoneExclusion`, `ScreenReferenceAdm`, and polar/Cartesian coordinate conversions.
  - Pure-Rust ITU-R BS.2076 ADM XML parser (`parse_adm_xml`) and serializer (`to_adm_xml`).
  - Bidirectional mapping between `AdmDocument` and `SpatialScene` (`AdmSceneConverter::to_spatial_scene` and `AdmSceneConverter::from_spatial_scene`).
- **BWF/BW64 Container Interoperability (`src/spatial/bw64.rs`)**:
  - Added complete support for EBU Tech 3285 / ITU-R BS.2088 containers: `Bw64File`, `Bw64Header`, `Ds64Chunk` (64-bit RF64/BW64 sizes), `BextChunk` (loudness, true peak, timecode, originator, UMID), `ChnaChunk` (track-to-ADM UID mappings), `AxmlChunk` (embedded ADM XML), and `IxmlChunk`.
- **Object Metadata Expansion (`src/spatial/object.rs`)**:
  - Added explicit metadata: `head_locked`, `screen_relative`, `screen_ref` (`ScreenReference`), `divergence` (`[0.0, 1.0]`), `extent` (`ObjectExtent`: width, height, depth), `diffuseness` (`[0.0, 1.0]`), `absolute_distance`, and `zone_exclusion` (`ZoneExclusion`).
  - Added builder methods (`with_head_locked`, `with_screen_relative`, `with_divergence`, `with_extent`, `with_diffuseness`, `with_absolute_distance`, `with_zone_exclusion`).
- **First-Class HRTF Profile Architecture (`src/spatial/hrtf/profile.rs`, `src/spatial/hrtf/mod.rs`)**:
  - Added `HrtfProfile` bundling `dataset`, `subject` (`HrtfSubjectInfo`), `sampling_rate`, `anthropometry` (`AnthropometricMetadata`), `interpolation_method` (`HrtfInterpolationMethod`), `latency_alignment` (`HrtfLatencyAlignment`), `phase_mode` (`HrtfPhaseMode`), and `personalization` (`HrtfPersonalization`).
  - Added `HrtfProfileManager` with profile registry and glitch-safe switching support.
  - Added `compute_quality_metrics` calculating mean lateral ITD, spectral energy, front-to-back contrast, and interaural balance.
- **Spatial Quality Evaluation Framework (`src/spatial/quality_eval.rs`)**:
  - Added `SpatialQualityEvaluator` and `SpatialQualityReport` computing objective localization metrics: `azimuth_error_deg`, `elevation_error_deg`, `itd_error_sec`, `ild_error_db`, `spectral_distortion_db`, `front_back_confusion_rate`, `distance_error_m`, `energy_error_db`, `phase_error_deg`, and `room_decay_error_sec`.
  - Human-readable report formatting and JSON export.
- **Output-Profile Calibration (`src/output/calibration.rs`, `src/output/output_profile.rs`, `src/output/mod.rs`)**:
  - Added `OutputCalibration` with `TargetLayoutKind` (Stereo, Headphones, 5.1, 7.1, 7.1.4, 9.1.6, Binaural, CustomArray), per-channel `speaker_positions`, `delays`, `gains`, `polarity`, `calibration_offsets`, `sample_rate`, `latency`, and `correction_ir`.
  - Integrated `calibration: Option<OutputCalibration>` into `OutputProfile`.
  - Built-in calibrated monitoring presets for Stereo, Headphones, 5.1, 7.1, 7.1.4, 9.1.6, and Binaural.
  - Updated `crates/config/src/versioned_state.rs` to persist calibration state.

## [5.3.0]

### Added

- **Real-time Health Monitor (`src/diagnostics/`)**:
  - Added `RealtimeHealthMonitor` tracking per-callback duration (µs), rolling average and worst-case duration, CPU load estimate, buffer fill %, XRun / underrun / overrun counters, clock drift (ppm), resampler ratio, graph generation sequence, and output latency — all via lock-free atomics with zero allocations on the audio thread.
  - Added `HealthSnapshot` — an immutable, copy-safe telemetry snapshot readable from any thread at any time.
  - Added `RealtimeDiagnosticQueue` — a lock-free SPSC ring for pushing `RawDiagnosticEvent`s from the audio thread and draining them on the control/UI thread without allocation.
  - Added `RawDiagnosticEvent` and `DiagnosticEvent` with `DiagnosticSeverity` (`Info`, `Warning`, `Error`, `Critical`) and expanded `DiagnosticKind` (`Dsp`, `Clock`, `Plugin`, `Graph`, `Security`).
- **Node-Level Diagnostics (`src/dsp/graph2/diagnostics.rs`)**:
  - Added `NodeDiagnostics` carrying per-node: `node_id`, `name`, `active`, `latency_samples`, `latency_ms`, `tail_samples`, `tail_ms`, `peak_in_db`, `peak_out_db`, `gain_reduction_db`, `error_count`, `non_finite_count`, and `cpu_cost_us`.
  - Builder API (`with_latency_and_tail`, `with_metering`, `with_errors`, `with_cpu_cost`) for zero-cost construction in hot paths.
  - Full `serde` round-trip support for telemetry serialization.
- **Unified Parameter Metadata System (`src/dsp/parameters.rs`)**:
  - Added `ParameterId`, `ParameterDescriptor` with `min`/`max`/`default`, `ParameterUnit` (LinearGain, Decibels, Hertz, Milliseconds, Percent, Semitones, Ratio, Boolean, Integer, Enum), `ParameterCurve` (Linear, Logarithmic, Exponential, Decibel, SCurve), and `ParameterSmoothing` (None, OnePole, LinearRamp, SlewRateLimit).
  - `ParameterDescriptor` methods: `normalize`, `denormalize`, `clamp`, `snap_step`, `format_value`.
  - `ParameterRegistry` — centralized catalog seeded with standard engine parameters (master volume, balance, speed, EQ band frequency/gain/Q, dynamics threshold/ratio, spatial master gain).
- **Versioned State & Preset Serialization (`crates/config/src/versioned_state.rs`, `src/state/mod.rs`)**:
  - Added generic `VersionedEnvelope<T>` carrying `schema_version`, `engine_version`, `component_version`, and typed `state`.
  - Added state models: `EngineState`, `GraphState`, `NodeState`, `PluginState`, `SpatialSceneState`, `OutputProfileState`.
  - Added `StateMigrationError` with structured variants including `UnsupportedSchema { found, max_supported }` for forward-incompatible rejection.
  - Added `save_versioned_state` / `load_versioned_state` helpers and `STATE_SCHEMA_VERSION` / `CURRENT_ENGINE_VERSION` constants.
- **Seamless Graph Transitions (`src/dsp/graph2/transitions.rs`)**:
  - Added `TransitionCrossfader` performing dual-path equal-power, linear, or S-curve crossfades over a configurable window (default 10 ms).
  - Added `TransitionConfig` and `TransitionCurve`.
  - Added `ContinuousParameterSmoother` (one-pole exponential) for zipper-free EQ and dynamics parameter changes.
  - Added `measure_max_discontinuity` analytical helper for fidelity verification.
  - Added `blend_stereo_into` variant for alias-free stereo blending into a separate output buffer.
- **Seamless generation-swap click elimination (`src/dsp/graph2/prod/arena/process.rs`, `controls.rs`)**:
  - `control_tick` now arms `transition_fader` on every generation swap.
  - `process_block` runs both the retiring and incoming generations on disjoint pre-allocated scratch buffers (`scratch_trans_l/r`, `scratch_trans_new_l/r`) and blends them via `blend_stereo_into` — eliminating the instantaneous cut click that previously occurred on graph reconfigurations, EQ changes, and plugin reloads.
  - `GraphScratch` extended with `scratch_trans_new_l/r` (pre-allocated, zero hot-path allocation).
  - Added `retire_generation_to_bus` audio-side helper on `DspGraph` to encapsulate atomic handback of retiring generations.
- **Device/Output Recovery Hardening (`src/output/recovery.rs`)**:
  - Formalized 7-phase recovery state machine: `DeviceDisappeared` → `PreserveEngineState` → `ReopenDevice` → `ReconfigureFormat` → `RestoreClock` → `RestoreOutputProfile` → `Idle`.
  - Added `OutputRecoveryController` with exponential-backoff retry counting and `PreservedPlaybackSnapshot` carrying sample-accurate playhead position, volume, output profile ID, and playback state.
  - Added `rescale_clock_frames` for sample-accurate frame count rescaling across sample-rate changes (e.g. 48 kHz → 96 kHz device reconnect).

### Changed

- Diagnostics & state fidelity test suite formalized as
  [`tests/fidelity/production_diagnostics_fidelity.rs`](tests/fidelity/production_diagnostics_fidelity.rs)
  (`[[test]] name` in `Cargo.toml` updated accordingly). Module doc rewritten with
  domain-precise descriptions of each pillar.

## [5.2.0]

### Added

- **Dedicated Standards & Version Framework (`src/standards/`)**:
  - Created formal, typed standards subsystem containing `standards::loudness` (ITU-R BS.1770-5, EBU R128, ReplayGain 2.0), `standards::true_peak` (ITU-R BS.1770-5 Annex 2, EBU Tech 3341), `standards::channel_layout` (ITU-R BS.775, ITU-R BS.2051-3, SMPTE ST 2036-2), `standards::spatial` (Right-Handed Cartesian, Polar, ACN/SN3D/N3D/Max-rE), `standards::adm` (ITU-R BS.2076-1/2 Audio Definition Model complete data structures), and `standards::metadata` (BWF, BW64, iXML, ID3v2.4, VorbisComment).
  - Added `StandardizedComponent` trait for declaring standards adherence and machine-readable version queries.
- **Complete Loudness Analysis & Compliance Subsystem (`dsp::loudness::analysis`)**:
  - Implemented `LoudnessAnalyzer` computing integrated LUFS, momentary LUFS, short-term LUFS, Loudness Range (LRA per EBU Tech 3342), maximum inter-sample true peak (dBTP per BS.1770-5 Annex 2), maximum discrete sample peak (dBFS), loudness timelines, peak timelines, gating diagnostics (% gated frames, ungated mean), and per-channel energy contributions.
  - Added `LoudnessComplianceProfile` and `LoudnessComplianceResult` evaluating broadcast (EBU R128), streaming (-14 LUFS), and ReplayGain compliance.
- **Unified Latency & Plugin Delay Compensation (PDC) Architecture (`dsp::graph2::latency`)**:
  - Added `NodeLatencyBreakdown` decomposing latency into intrinsic, lookahead, plugin, resampler, HRTF, convolution, device, and output transport samples.
  - Added `LatencyMeasurementKind` (`Reported`, `Actual`, `Estimated`, `Measured`) and `UnifiedLatencyReport` providing deterministic per-node and graph-wide latency introspection.
- **Transactional Graph Editing Runtime (`dsp::graph2::transaction`)**:
  - Implemented `GraphTransaction` enforcing atomic transaction pipeline: modify &rarr; validate &rarr; PDC delay compensation &rarr; preallocate runtime state (`RtPlan`) &rarr; warm up &rarr; publish generation.
  - Added fail-safe isolation ensuring failed validations or allocations abort gracefully without interrupting playback on the active audio graph.
- **Deterministic Processing Mode & Numerical Equivalence (`dsp::deterministic`)**:
  - Implemented `EquivalenceClass` (`BitExact`, `NumericallyEquivalent`, `PerceptuallyEquivalent`, `Divergent`) and `DeterministicMode` (`StrictBitExact`, `Numerical`, `Perceptual`) with automated buffer comparison and SNR evaluation.
- **Realtime Float Safety & Non-Finite Containment (`dsp::safety`, `diagnostics`)**:
  - Implemented `FloatSafetyMode` (`FlushToZero`, `DenormalSuppress`, `StrictIEEE`) with hardware FTZ/DAZ thread enforcement.
  - Implemented `NonFinitePolicy` (`Ignore`, `Detect`, `Clamp`, `Silence`, `BypassNode`) and zero-allocation inline block sanitizer `contain_non_finite_block`.
  - Added `NonFiniteIncident` structured diagnostics identifying offending node ID, channel, sample offset, and graph generation.
  - Expanded `DiagnosticKind` with `Dsp`, `Clock`, `Plugin`, `Graph`, and `Security` categories.
- **Independent Reference Oracle Test Suite (`tests/fidelity/independent_references.rs`)**:
  - Independent analytical mathematical oracles for synthetic BS.1770-5 loudness, direct O(N²) DFT vs FFT, analytical Z-plane biquad transfer functions, brickwall limiter ceiling guarantees, and spherical harmonics basis functions.
- **Expanded Mutation Fuzzing Suite (`tests/fidelity/fuzz_expanded.rs`)**:
  - Multi-target mutation fuzzing testing CUE sheet parsing, Graph2 JSON deserialization, SpatialScene deserialization, AdmDocument deserialization, TTA headers, and DSD/DSF container readers.
- **Formal Performance Budget Benchmarks (`benches/performance_budget.rs`)**:
  - Comprehensive Criterion benchmarks scaling across block sizes (16..1024), sample rates (44.1k..384k), and multichannel speaker layouts (stereo, 5.1, 7.1, 7.1.4, 9.1.6).
- **Release Qualification Pipeline Engine & CLI (`eval::qualification`, `src/bin/release_qualification.rs`)**:
  - Automated release qualification pipeline generating machine-readable JSON status summaries (`qualification_report.json`).
- **Canonical Engineering Specification (`docs/ENGINE_SPEC.md`)**:
  - Authoritative 21-section engineering contract defining the audio model, thread model, realtime guarantees, buffer semantics, precision, graph semantics, latency/PDC, tails, automation, plugin ABI, spatial coordinates, channel layouts, loudness standards, output matrix, diagnostics, errors, serialization, determinism, compatibility, security, and verification requirements.

### Changed

- Upgraded loudness metering and true peak documentation across `dsp::loudness` and `dsp::true_peak` to strictly reference **ITU-R BS.1770-5** (superseding superseded BS.1770-4 references).
- Upgraded `LoudnessMeasurement` to include `standard: LoudnessStandard` defaulting to `ItuBs1770_5`.

## [5.1.0]

### Added

- **Robust Adaptive Endpoint Clock Correction / ASRC (`output::drift`, `output::endpoint`)**:
  - Implemented `DriftController` with dual-mode proportional-integral (PI) loop filter, dynamic gain scheduling (`FAST_KP/KI` vs `STEADY_KP/KI`), and conditional integration anti-windup clamping.
  - Added cascaded 2-pole IIR low-pass jitter filter rejecting high-frequency scheduling jitter without introducing phase lag.
  - Added slew-rate limiting bounding ratio updates to $\pm 2.0$ ppm per block to prevent audible pitch modulations.
  - Added loss-of-clock detector tracking persistent ring buffer stall counts and safely falling back to nominal 1.0 ratio upon buffer freeze.
  - Decomposed `endpoint.rs` down to 866 lines, preserving modularity guidelines and eliminating god-file tendencies.
- **Production-Grade Plugin Host Abstraction (`crates/plugin-abi`, `dsp::graph2::prod::arena::nodes::plugin_host_node`)**:
  - Multi-bus audio routing: added `AudioBusses`, `AudioBusConfig`, and `BusLayout` supporting primary, auxiliary, and sidechain buses with up to 16 channels.
  - Full MIDI & MPE event dispatch: added `MidiEvent`, `MidiEventType` (NoteOn/Off, CC, PitchBend, ChannelPressure, PolyPressure, ProgramChange), `MidiBuffer`, and `MpeProfile`.
  - Musical transport synchronization: added `TransportInfo`, `MusicalTime`, `TimeSignature`, and playback state tracking tempo (BPM), beat position, bar start, and play state.
  - Sample-offset automation: added `SampleOffsetAutomation` dispatching block-relative parameter changes with sub-block sample accuracy.
  - Dynamic bypass & error isolation: integrated click-free crossfaded plugin bypass and host error isolation in `PluginHostNode`.
- **Tail-Length & Flush Semantics in DSP Graph (`dsp::graph2::latency`, `dsp::graph2::exec`)**:
  - Tail analysis: implemented `node_tail_at`, `node_tail`, `TailReport`, `analyze_tail`, and `graph_tail_samples` traversing graph topologies to calculate sample-accurate decay tail requirements for delays, reverbs, and acoustic simulation nodes.
  - Graph flush: implemented `OfflineExecutor::flush()` zeroing out all delay lines, acoustic room states, partitioned convolution IR partitions, and inter-node audio edges.
  - Offline tail rendering: added `OfflineExecutor::render_tail(tail_samples)` to drain lingering acoustic and algorithmic tails into output buffers.
- **First-Class Sample-Accurate Automation (`dsp::timeline::curve`)**:
  - Implemented `InterpolationMode` supporting `Step`, `Linear`, `Exponential`, and smooth cubic `SCurve` transitions.
  - Added `AutomationKeyframe` and `AutomationTrack::render_block` generating sample-accurate continuous parameter modulation buffers with zero allocations on the audio thread.
- **Unified Modulation System (`dsp::modulation`)**:
  - `Lfo`: multi-waveform low-frequency oscillator (`Sine`, `Triangle`, `Sawtooth`, `Square`, `SampleAndHold`) supporting free-running frequency and tempo-synchronized musical subdivisions with unipolar/bipolar modes.
  - `AdsrEnvelope`: 4-stage attack-decay-sustain-release envelope with configurable sample-rate timing and retrigger support.
  - `EnvelopeFollower`: peak and RMS follower with dual-mode attack and release time constants for dynamic tracking.
  - `ModulationMatrix`: flexible routing matrix connecting modulation sources (`Lfo`, `Adsr`, `Follower`, `PitchBend`, `ModWheel`, `Aftertouch`, `Velocity`) to target parameter destinations with bipolar scaling and modulation summing.
- **Dedicated Creative Sound-Design DSP Layer (`fx`)**:
  - Delay & spatial processing: `CombFilter` (feedback & feedforward comb filtering with damping) and `PingPongDelay` (stereo cross-feedback delay with tempo sync and feedback filtering).
  - Modulation effects: `Chorus` (multi-voice delay modulation with quadrature LFOs), `Flanger` (short comb-delay modulation with feedback inversion), `Phaser` (cascaded allpass filter stages with feedback), and `RingModulator` (sine oscillator carrier with AM and four-quadrant multiplier).
  - Nonlinear processing: `Saturator` providing tape, tube, soft clipping, hard clipping, and asymmetric wavefolding distortion modes with wet/dry blending.
- **Spectral / Psychoacoustic Analysis Layer (`dsp::analysis`)**:
  - Spectral features: `spectral_centroid`, `spectral_spread`, `spectral_flux`, `spectral_rolloff`, `spectral_flatness`, and `sub_bass_energy_ratio`.
  - Temporal dynamics: `crest_factor`, `dynamic_range_db`, `transient_density`, and `true_peak_density`.
  - Harmonic content: `harmonicity` and `tonality_estimate` with autocorrelation harmonic energy detection.
  - Analysis engine: `AnalysisEngine` computing comprehensive `AnalysisSnapshot` telemetry in real time without audio-thread heap allocations.
- **Perceptual, Numerical & Realtime QA Framework (`eval::measure`, `tests/fidelity/realtime_allocation.rs`)**:
  - Numerical metrics: implemented `snr_db`, `intermod_distortion_smpte`, `ir_phase_rad`, `group_delay_samples`, `itd_error_samples`, and `ild_error_db` in `eval::measure`.
  - Realtime allocation tests: added comprehensive test cases covering `DriftController`, `AutomationTrack`, creative FX, modulation processors, and `AnalysisEngine`, passing all 38 test suites with 0 heap allocations.

## [5.0.0]

### Added

- **Constant-Spread Spatial Panning (`spatial::spread`, `spatial::panner`)**:
  - Implemented `SpreadMode` (Point, Focused, Narrow, Medium, Wide, Enveloping, Diffuse) with ring count scaling (1, 3, 5, 8, 12, 16).
  - Added `constant_spread_half_angle` dynamically bounded by physical speaker layout span.
  - Added `constant_power_spread_gains` with energy-normalized gain distribution across discrete speaker positions.
  - Integrated constant-spread algorithm into `BasicPanner::solve_spread` with energy preservation.
- **Higher-Order Ambisonics Beyond 3rd Order to Order 9 (`spatial::ambisonic`)**:
  - Extended spherical harmonic basis (`sh_n`) up to order 9 (100 channels) using normalized associated Legendre polynomial recurrence.
  - Implemented `max_re_window` and `in_phase_window` order-weighting tables up to order 9.
  - Added `HoaConfig`, `HoaDecoding`, `HoaEncoder`, `HoaDecoder`, `NearFieldCompensation`, and `PerSpeakerDelay`.
  - Eliminated the 1710-line `ambisonic.rs` god file, modularizing into `basis`, `encode`, `rotation`, `decoder`, `hoa`, and `mod` submodules.
- **Spherical & Irregular-Mesh HRTF Interpolation (`spatial::hrtf::interpolate`, `spatial::hrtf::dataset`)**:
  - Implemented `SphericalHrtfInterpolator` using 3D convex hull spherical triangulation on $S^2$.
  - Implemented `barycentric_sphere` solving exact spherical barycentric coordinates with normalized weighting.
  - Added irregular mesh fallback with nearest-neighbor inverse-distance weighting.
- **Decomposed HRIR Representation (`spatial::hrtf::decompose`)**:
  - Implemented `HrirComponents` separating impulse responses into fractional onset ITD, minimum-phase spectrum, and excess-phase allpass.
  - Added threshold-based `detect_onset_samples` and `extract_itd`.
  - Added `minimum_phase_from_ir` and `excess_phase_from_ir` using cepstral Hilbert transform reconstruction.
  - Added `decompose_corpus` and `reconstruct_ir`.
- **Multi-Tap HRTF Quality Tiers & Strategies (`spatial::hrtf::quality`, `spatial::binaural`)**:
  - Implemented `HrtfQualityMode` supporting `Low64`, `Medium128`, `High512`, and `Ultra2048` taps with up to 2048 taps ceiling (`MAX_HRTF_TAPS`).
  - Added `HrtfConvStrategy` selecting between direct time-domain FIR convolution (<= 256 taps) and partitioned overlap-add frequency domain convolution (> 256 taps).
  - Added `BinauralRenderer::hrtf_quality_mode` mapping spatial quality tiers to HRTF quality modes.
- **SOFA Irregular Mesh Compatibility (`spatial::sofa`)**:
  - Added `SofaImportMode` (`Auto`, `ForceRegular`, `ForceIrregular`) and `import_sofa_with_mode`.
  - Added auto-detection of non-Cartesian coordinate meshes setting `mesh_hint` on imported `HrtfCorpus`.
- **HRTF Subsystem Modularization (`spatial::hrtf`)**:
  - Eliminated the 1574-line `hrtf.rs` god file, decomposing into `quality`, `corpus`, `interpolate`, `decompose`, `dataset`, and `mod` submodules.
- **Near-Field Wavefront Curvature (`spatial::nearfield`)**:
  - Implemented `WavefrontCurvatureState` with distance-dependent spherical wavefront curvature (`1 + r/d`) providing additional near-field ILD boost.
  - Added `NearFieldModel` enum (`ProximityGainAndShelf`, `WavefrontCurvature`, `HoaDistanceEncoding`) and `hoa_distance_encode_filter`.
- **Frequency-Dependent Diffraction & Material Transmission (`spatial::occlusion`)**:
  - Implemented `MaterialTransmission` with presets (`TRANSPARENT`, `DRYWALL`, `CONCRETE`, `GLASS`, `WOOD`) and `DiffractionOcclusion`.
  - Added 3-band crossover filtering (`OcclusionBandCoeffs`, `OcclusionBandState`) with sub-block cutoff smoothing and energy conservation.
- **Room Correction Measurement & Processing Infrastructure (`spatial::room_correction`)**:
  - Added `measurement` module: log-sine sweep generation, matching inverse filter generation, and deconvolution via spectral division.
  - Added `analysis` module: frequency response, Schroeder backward-integrated energy-time curve (ETC), RT60 estimation, phase, and group delay.
  - Added `correction` module: FIR correction filter derivation with boost-clamping, smoothing, and target curve compliance, wrapped in `RoomCorrectionProcessor`.
- **Psychoacoustically Adaptive Bass Enhancement (`spatial::bass::psychoacoustic`, `config::spatial_render`)**:
  - Added `SpatialBassMode::Psychoacoustic` and `PsychoacousticBassConfig` to `config` crate.
  - Implemented `FundamentalDetector` with 4:1 decimation and linear autocorrelation peak tracking.
  - Implemented `SpeakerCapabilityModel` (f3 cutoff, high-pass roll-off), `MaskingModel` (simultaneous masking threshold curve), and `HarmonicSelector` (2nd, 3rd, 4th synthetic harmonic generation).
  - Added `PsychoacousticBassProcessor` integrated into left and right channel processing paths in `SpatialBassEngine`.
- **Realtime Safety Guarantees**:
  - Added 6 realtime zero-allocation tests in `tests/fidelity/realtime_allocation.rs` verifying 0 heap allocations across all spatial subsystems on the audio hot path.

## [4.9.0]

### Added

- **Hybrid Time-Stretching & Pitch-Shifting Engine (`dsp::timestretch`)**:
  - Added `PhaseVocoder` using `realfft` with Laroche-Dolson spectral peak detection, rigid phase locking, and inter-channel stereo coherence.
  - Added `TransientDetector` based on short-time/long-time energy onset ratios with attack hold timing.
  - Added `TimeStretchMode` (`Wsola`, `PhaseVocoder`, `Hybrid`) and optional formant preservation.
  - Modularized `timestretch.rs` into cohesive submodules (`config_types`, `transient`, `phase_vocoder`, `stretcher`, `tests`) adhering strictly to the "No God Files" modularity standard.
- **Dynamic EQ (`dsp::equalizer::dynamic`)**:
  - Implemented `DynamicEq` and `DynamicEqBand` with dynamic boost and cut modes, threshold, ratio, attack/release ballistics, and sidechain envelope tracking.
  - Added sub-block parameter smoothing (32 frames) to eliminate zipper noise and avoid per-sample trigonometric coefficient recomputation.
  - Added `DynamicEqConfig` and `DynamicEqBandConfig` to `config` crate.
- **Unified Dynamics Detector Architecture (`dsp::dynamics`)**:
  - Implemented `DynamicsDetector`, `BallisticEnvelope`, `DetectionMode` (Peak, Rms, TruePeak), and `ChannelLinkMode` (Independent, LinkedAverage, LinkedMax).
  - Integrated sidechain filtering (`SidechainFilter`: Flat, HighPass, LowPass, BandPass, Bell, HighShelf) with attack, release, and hold ballistic stages.
- **High-Quality Transparent Mastering Limiter (`dsp::limiter`)**:
  - Expanded `LimiterMode` with `Safety`, `Mastering`, and `ClipperLimiter`.
  - Added program-dependent dual-stage release (crest factor over RMS history), adaptive recovery, variable stereo link controls, and pre-limiter soft clipping for peak shaving.
- **Expanded Dither and Noise Shaping (`dsp::dither`)**:
  - Added `DitherType::NoiseShaped16`, `DitherType::NoiseShaped20`, and `DitherType::NoiseShaped24`.
  - Implemented Wannamaker 4-tap F-weighting noise shaping for 16-bit, 3-tap for 20-bit, and 2-tap for 24-bit with error feedback filtering across stereo and mono f32/f64 paths.
- **Selectable Crossover Architectures (`dsp::crossover`)**:
  - Implemented `Crossover2Way`, `Crossover3Way`, `CrossoverArchitecture` (Linkwitz-Riley 12/24/48 dB/oct, Linear-Phase FIR with windowed-sinc, and Mixed-Phase).
  - Verified perfect reconstruction across crossover bands and exact linear phase.
- **Late-Reverberation Engine Upgrade (`spatial::room`)**:
  - Upgraded late field to an 8-line Feedback Delay Network (FDN) with mutually prime coprime delay lengths.
  - Integrated orthogonal Householder feedback matrix ($A = I - \frac{2}{N}\mathbf{1}\mathbf{1}^T$) ensuring energy conservation and maximal cross-channel diffusion.
  - Added frequency-dependent one-pole absorption damping and dual series allpass input diffusers.
  - Modularized `room.rs` into `src/spatial/room/` submodules (`mod`, `early`, `late`, `tests`), fully eliminating god files and preserving 100% zero-allocation guarantees on the audio path.

## [4.8.0]

### Added

- **Resampler Latency Provider Trait (`dsp::LatencyProvider`)**: Added `LatencyProvider` trait exposing exact resampler latency (filter group delay + buffering delay) across all sample rate ratios and quality tiers, implemented on `AudioResampler<T>`, `GenericResampler`, and `ResamplerNode`.
- **Convolution Engine Latency Distinction**: Added explicit accessors distinguishing `intrinsic_latency_samples()`, `partition_latency_samples()`, `algorithmic_latency_samples()`, `ir_length_samples()`, and `tail_length_samples()` in `ConvolutionEngine`.
- **Multichannel Professional Metering Beyond 8 Channels**: Extended `KWeightStage1`, `KWeightStage2`, `bs1770_weights_for_layout`, `LoudnessMeter`, and `ProfessionalMeters` to 16 channels (`MAX_CHANNELS`), adding native support for Mono, Stereo, 5.1, 7.1, 7.1.4 (12 ch), and 9.1.6 (16 ch).
- **Lock-Free Triple-Buffered Professional Metering Telemetry**: Replaced `Mutex<MeterState>` in `ProfessionalMeters` with an atomic triple-buffered snapshot exchange and CAS processing guard, guaranteeing zero mutex acquisition and zero heap allocations on the audio processing hot path.

### Fixed

- **Hybrid Spatial Zero-Allocation Hot Path**: Eliminated dynamic vector allocations in `HybridSpatialRenderer::process_hybrid_block` via fixed-size stack arrays; verified zero allocations across 2, 8, 12, 16 channels and all standard block sizes.
- **Hybrid Spatial Bass Configuration Preservation**: Ensured `HybridSpatialRenderer::prepare` preserves active custom `SpatialBassConfig` across reconfigurations, and correctly prepares child binaural renderers with stereo master layout.
- **Graph 2.0 Terminal Latency Accounting**: Updated `dsp::graph2::latency::analyze` to evaluate total graph latency across all sink/terminal branches using `max(upstream + taps)`, ensuring intrinsic taps of terminal limiters and resamplers are included in graph latency.
- **Limiter Lookahead Sample Calculation**: Corrected `lookahead_samples` calculation from `.ceil()` to `.round()` in `LookaheadLimiter`, ensuring reported delay matches impulse peak emergence across 44.1, 48, 88.2, 96, 176.4, and 192 kHz sample-peak and true-peak modes.
- **EBU Tech 3342 LRA Startup Window**: Fixed `LoudnessMeter::commit_hop` to require a completely populated 3.0 s sliding window (30 hops of 100 ms) before recording short-term values for Loudness Range calculation.

## [4.7.0]

### Added

- **SIMD-Accelerated Hot DSP Kernels (`dsp::simd`)**:
  - Specialized vectorization kernels for gain scaling, channel mixing, biquad IIR filtering, true-peak limiting, and bilinear interpolation using explicit x86 SSE2 / aarch64 NEON with exact scalar fallback.
  - Integration into `dsp::gain` and `spatial::hrtf::vector_bilinear`.
- **Graph 2.0 Realtime Buffer Zeroing Optimization**:
  - Epoch-based plane validity tracking in `RtPlan`, eliminating redundant O(planes * block_size) zero-fill operations per audio block while preserving bit-exact silence on unassigned planes.
- **Hot Path Zero-Allocation Polish**:
  - Removed redundant `Arc::clone` invocations in `dsp::convolution` frequency-domain workspace processing via direct borrowed references.
- **Adaptive Spatial Voice Scaling & CPU Budgeting (`spatial::voice`)**:
  - Added `VoicePriority::AdaptiveAudibility` policy ranking voices via psychoacoustic audibility `(gain * importance) / distance.max(0.1)`.
  - Added `importance` factor to `BudgetCandidate` and `SpatialAudioObject`.
- **Legacy Low-Power Performance Profile**:
  - Added `PerformanceMode::LegacyLowPower` and `EnginePreset::LegacyLowPower` for power-constrained environments and vintage hardware.
- **Dedicated Spatial Bass Management Layer (`spatial::bass`)**:
  - `BassManager` supporting multi-slope crossovers (12, 24, 48 dB/oct) with Linkwitz-Riley (LR2, LR4, LR8) and Butterworth topologies.
  - Subwoofer delay compensation (0..50 ms) and continuous phase alignment (0..180° with polarity inversion).
  - Mains high-pass and bass redirection to subwoofer / LFE.
- **Spatial Bass Engine (`spatial::bass::SpatialBassEngine`)**:
  - Three operational modes: `Pure` (bit-perfect passthrough), `BassManaged` (active crossover steering), and `BassImmersion` (psychoacoustic missing fundamental 2nd/3rd harmonic synthesis and dynamic low-shelf EQ).
- **Per-Object Bass Intent**:
  - `BassIntent` enum supporting `FullRangeManaged`, `DirectLfe`, `SubBassOnly`, and `Bypass` modes, serialized in `SpatialObjectConfig`.
- **Hybrid Spatial Renderer (`spatial::hybrid_renderer`)**:
  - Unified pipeline integrating Objects, Beds, Fields, Room Modal Acoustics, Spatial Bass Management, and flexible output panning (VBAP, Ambisonics HOA, Binaural HRTF).
- **Higher-Order Ambisonics (HOA) Quality Tiers**:
  - Explicit mapping of `SpatialQuality` tiers to ambisonic orders: Tier 1 (Low/Medium, Order 1 / 4 ch), Tier 2 (High, Order 2 / 9 ch), Tier 3 (Ultra, Order 3 / 16 ch).
- **Bass-Aware Room Modeling (`spatial::acoustic::bass_room`)**:
  - `ModalBassRoom` modeling rectangular room standing waves below the Schroeder cutoff frequency ($f_s \approx 2000\sqrt{RT_{60}/V}$) with spatial source-listener eigenfunction coupling and resonant peaking biquad filters.
- **Licensing & Algorithmic Attribution Audit**:
  - Published `docs/LICENSES_AND_ATTRIBUTION.md` documenting 100% pure Rust design, absence of proprietary Dolby/DTS blobs, Apache-2.0 compliance, and academic citations.

## [4.6.0]

### Added

- **Track Preloader & Gapless Queue Handoff**:
  - `engine::preload`: background decoder & loudness preloading for upcoming playlist queue tracks (`PreloadManager`).
  - Seamless, gapless track handoff when transitioning across queued tracks (`swap_to_prepared_track`), eliminating decode startup latency and preserving sample continuity.
  - Integration fidelity suite `tests/gapless_queue.rs`.
- **Bounded LRU Track Cache (`engine::track_cache`)**:
  - `TrackCache` and `CachedTrackInfo` providing bounded LRU metadata caching with automatic cache invalidation upon file size or modification time (`mtime`) mutation.
  - Comprehensive hit/miss metrics and eviction testing in `tests/track_cache.rs`.
- **Unified Professional Audio Metering (`dsp::meters`)**:
  - `ProfessionalMeters` calculating real-time sample peaks, 4x-oversampled true-peak (dBTP), windowed RMS (dBFS), dynamic range / crest factor, DC offset, and clipping counter.
  - Comprehensive metering fidelity suite `tests/fidelity/meters.rs`.
- **Deterministic Offline Audio Rendering (`engine::offline`)**:
  - `OfflineRenderer` and `OfflineRenderResult` for headless, non-realtime batch rendering and offline testing of audio sources through the DSP pipeline.
  - Integration suite in `tests/offline_render.rs`.
- **Streaming Buffer Resiliency & Dynamic Eviction**:
  - Dynamic stream buffer management in `src/audio_io.rs` with automatic eviction of historical buffered bytes past the read window (`BACK_MARGIN`), preventing unbounded memory growth during HTTP Range streaming.
- **Low-Memory Build Profile & Bundled LLD Linker Optimization**:
  - `.cargo/config.toml` configuring `jobs = 2`, `lld` linking via Rust's bundled toolchain (`-C link-arg=-fuse-ld=lld`), and `debug = 1` for dev and test profiles, cutting linker RAM usage by >70% and preventing system thrashing/crashes on memory-constrained systems.

### Fixed

- **Plugin ABI serde compatibility**:
  - Implemented custom non-allocating `Serialize` and `Deserialize` for `PluginParams` and derived serde on `ParamValue` to satisfy trait bounds on fixed-size arrays (`[ParamValue; 64]`) when `serde-types` feature is enabled.
- **FFI spatial metrics lifetime**:
  - Fixed temporary value lifetime in `engine_get_spatial_cost_metrics` in `src/ffi.rs`.
- **Clippy lints & warnings**:
  - Addressed all lints across workspace targets, including needless borrows in playlist management, redundant option closures, range checks, and collapsible conditionals in `src/audio_io.rs`.

## [4.5.1]

### Changed

- **Documentation & internal artifact consolidation**:
  - Removed internal planning documents (`docs/EVOLUTION.md`,
    `docs/PHASE46_NODE_PARITY_INVENTORY.md`, `docs/QUALITY.md`, and `.kilo/plans/`).
  - Standardized terminology across codebase comments, module documentation,
    and test suites, replacing historical phase/milestone numbers with canonical
    domain concepts (room correction, multi-track lanes, plugin host insert,
    listener motion, scene animation, and diagnostics).
  - Updated system documentation (`README.md`, `AGENTS.md`,
    `docs/ARCHITECTURE.md`, `docs/OWNERS_GUIDE.md`) and `.gitignore` to reflect
    the consolidated documentation tree.
- **Workspace crate versions**:
  - Synchronized `plugin-abi` and `plugin-test-echo` versions to `4.5.1` in
    lockstep with `engine` and `config`.

## [4.5.0]

### Added

- **Spatial diagnostics.** The spatial master gains a
  **deterministic render-cost model** and per-block cost telemetry,
  independent of wall-clock timing: a pure function of the scene, so a
  cost report is reproducible, comparable across runs, and usable as a
  regression gate.
  - `spatial::diagnostics` grows `build_scene_cost_report` — per-object
    cost rows (base = 1.0 + spread + room-send per object, scaled by the
    quality tier: Low/Medium 1.0×, High 1.5×, Ultra 2.5×), the block
    budget (the voice budget's capacity, or the 4.0-unit default), the
    total, and the utilization fraction (`> 1.0` = over budget).
  - **Telemetry**: `SpatialTelemetry` gains `render_cost_units`,
    `cost_utilization`, and `tail_blocks_remaining` (the headroom in
    equivalent blocks; `∞` when idle, 0 at/over budget), refreshed on
    the control path (`SpatialNode::refresh_cost_diagnostics` /
    `Graph2Engine::refresh_spatial_cost`) and read via the new C FFI
    `engine_spatial_render_cost`.
  - **Eval harness**: new `spatial_render_cost_units` /
    `spatial_cost_utilization` metric kinds + the `spatial_cost`
    reference vector (`spatial_cost@1`) — the default stereo program
    gates at exactly 2.0 units / ≤ 0.6 utilization, so cost regressions
    fail CI deterministically (no timers).
  - `SpatialHealthSnapshot` now reports the cue bank size and the active
    cue count (`cue_count` / `active_cue_count`).

## [4.4.0]

### Added

- **Scene animation events.** The spatial layer gains
  **named trigger cues** and automation **playback modes** — the event
  half of scene animation, sample-accurate at the block boundary and
  allocation-free on the audio path (verified by a new
  `realtime_allocation` case).
  - **Cue bank**: scenes carry named `cues` (scene file +
    `SpatialConfig.cues`); each cue targets a program object (0 = L,
    1 = R) and drives gain / spread / position curves **relative to the
    firing instant**, with `looping` (wrap the clock at the cue
    duration) and `hold` (persist the final keyframe instead of
    releasing) modes. The `whoosh` / `door` presets ship on
    `config::SpatialCueConfig`.
  - **Automation modes**: `SpatialAutomationConfig` (and the runtime
    twin) gain `looping` / `hold` — object automation can repeat
    forever, and a finished automation can release its parameters back
    to the authored values (`hold = false`) or keep driving them.
  - **Realtime semantics**: while a cue is active its curves override
    the target's parameters (snapshot/restore around the render —
    bit-exact when idle); last-wins per target; the cue clock advances
    per block on the node.
  - **Surface**: `EngineCommand::SetSpatialCues` / `TriggerSpatialCue` /
    `StopSpatialCue` / `StopAllSpatialCues` (+ `EngineHandle`
    equivalents + `Graph2Engine::set_spatial_cues` /
    `trigger_spatial_cue` / stops, the queued
    `NodeCmd::TriggerSpatialCue` / `StopSpatialCue` /
    `StopAllSpatialCues`, and the C FFI `engine_trigger_spatial_cue` /
    `engine_stop_spatial_cue` / `engine_stop_all_spatial_cues`). The
    cue bank and active triggers mirror onto the sticky user state, so
    a live cue survives a generation swap (re-fired at the new clock
    origin, documented).
  - **Timeline + aelog**: `EventPayload::SpatialCue { cue }` schedules
    cue triggers on the timeline; the recorder gains
    `record_spatial_cues` / `record_spatial_cue_trigger` and replay
    reconstructs the bank + trigger timeline
    (`ReplayOutcome::cue_bank` / `cue_triggers`).
  - **Acceptance suite**: `tests/fidelity/spatial_events.rs` — bit-exact
    idle bank, overlay-and-release, looping/hold, last-wins, unknown
    name rejection, scene-file round-trip, and generation-swap
    survival.

## [4.3.0]

### Added

- **Listener motion.** The spatial master's listener
  is now **runtime-movable** on the audio path: a new
  `SetSpatialListenerPose` control command (queued per-node SPSC,
  block-boundary applied — never locks, never allocates) sets a target
  pose — world-space orientation (quaternion) + position (metres) — and
  the node glides toward it every processed block with the head-tracking
  conventions: shortest-arc **nlerp** on orientation, linear **one-pole**
  on position, a configurable smoothing time constant (`0 ms` snaps
  exactly) and an optional angular rate limit (deg/s). The glide runs
  inside the plan step (allocation-free, verified by a new
  `realtime_allocation` case) so a world-fixed image sweeps smoothly as
  the listener rotates/moves — the VR seam, host-driven.
  - Surface: `EngineHandle::set_spatial_listener_pose` /
    `set_spatial_listener_tracking`, `EngineCommand::SetSpatialListenerPose` /
    `SetSpatialListenerTracking`, `GraphControlHandle::set_spatial_listener_pose`
    (+ `NodeCmd::SetSpatialListenerPose` / `SetSpatialListenerTracking`,
    `DspGraph`/`Graph2ControlHandle` forwards), and the C FFI pair
    `engine_set_spatial_listener_pose` /
    `engine_set_spatial_listener_tracking`.
  - **Smooth re-bake seam**: `SpatialNode::listener_rebake_due(cell_m,
    last_baked_at)` — the control thread's decision function; when a
    moving listener crosses a full bake cell from the position the scene
    was baked at, the host re-bakes on the control thread and publishes
    through the generation-swap machinery (audio never interrupted; the
    tail-reset is the documented generation-rebuild behavior, identical
    without motion).
  - **Listener pose telemetry**: `SpatialTelemetry` gains
    `listener_yaw_deg` / `listener_pitch_deg` / `listener_roll_deg` /
    `listener_position` (the post-glide live pose), mirrored on the
    telemetry cadence and readable via the new FFI
    `engine_spatial_listener_pose`.
  - **Tracking surface**: `HeadSample` grows a `position` field (with the
    backward-compatible `new` constructor; `with_position` for full
    poses) and `HeadTracker` gains `sample_pose` / `apply_pose_to` /
    `current_position` — the same nlerp + one-pole + rate-limit
    discipline extended to position (segment-linear interpolation, held
    past the latest sample, first pose snaps).
  - **Math**: `Quat::to_euler_rad` — the verified inverse of
    `from_euler_rad` (ZXY extraction with gimbal-lock fold handling),
    and `Vec3::lerp`.
  - Acceptance: `spatial_node` suite motion cases (glide boundedness,
    zero-smoothing snap, rate-limit cap, generation-swap continuity,
    re-bake bound), tracking pose tests, FFI round-trip/rejection tests,
    and a moving-listener zero-allocation proof.

## [4.2.0]

### Added

- **Acoustic agreement — realtime distance colour .**
  With a baked scene's air-absorption model enabled, every reflection
  tap's realtime low-pass corner (what the per-image biquad runs at) is
  the **composition** of the surface corner with the air model's
  distance corner: `BakedScene::listener_images` folds
  [`AirAbsorption::compose_corner_hz`] so the production renderers
  darken a farther reflection exactly at the offline spectral kernel's
  −3 dB point. The renderers (panner / VBAP / binaural) also fold the
  scene-wide model onto **live-solve** reflections (previously
  spectrally flat), so live and baked paths agree on distance colour.
  The `BinauralRenderer` gains the dormant `set_air_absorption`
  control the other renderers already carried. The production
  `SpatialNode` exposes it through `set_air_absorption` /
  `air_absorption`, wired as `GraphControlHandle::set_spatial_air`
  (+ `NodeCmd::SetSpatialAir`).
- **Richer frequency-dependent attenuation families .**
  `AirAbsorption` grows a `rolloff_model` field:
  `one_pole` (the v3.48 default, bit-exact), `two_pole`
  (−12 dB/oct), and `exponential` (softest skirt). Every family is
  DC-exact (magnitude 1.0 at f = 0), monotonically non-increasing, and
  *disabled-exact* (`enabled: false` is exactly ×1.0 at every
  frequency). The offline kernels (`spectral_taps_with`) sample the
  family's own magnitude — no longer a hardcoded one-pole — and the new
  `AirAbsorption::corner_hz` maps each family to its −3 dB equivalent
  realtime corner.
- **Late-field distance roll-off .** New `Room`
  knob `late_distance` (config: `SpatialRoomConfig.late_distance`,
  `set_spatial_room`'s new parameter): when enabled, each object's
  room-send to the Schroeder tail is attenuated by the object's own
  distance model at its direct distance, so the tail rolls off with
  source distance like the direct and early-reflection paths. Default
  `false` keeps every legacy render bit-identical; the flag round-trips
  through the scene file format and spatial auto-save
  (`#[serde(default)]` — old files load with it off).
- **Fidelity suite `acoustic_agreement`**: realtime corner ≡ offline
  kernel −3 dB point (per family, DFT-probed), monotonic darkening with
  distance, disabled = bit-exact golden renders, live-path fold, and
  the late-field roll-off ratio.
- **Rust-native plugin ABI.** A versioned `#[repr(C)]`
  C-ABI plugin specification for pure-Rust effect plugins, hosted in
  the production graph at the master insert seam: the `plugin-abi`
  spec crate (vtable + `plugin_abi_v1()` handshake + safe host facade +
  static registry + optional dlopen loader), the `plugin-test-echo`
  reference plugin (`worst-case` feature allocates on purpose), the
  `ProdStage::PluginHost` arena slot (post-volume / pre-spatial;
  bit-exact pass-through when unconfigured, failed slots skipped),
  live `SetPluginEnabled` / `SetPluginParams` control with swap-survival
  mirrors, `EngineConfig.plugins` config + `ConfigIssueKind::Plugin`
  validation, the `plugin-dylib` engine feature, and the
  `tests/fidelity/plugin_host.rs` suite (10 tests) plus a plugin case
  in `realtime_allocation.rs`. The production topology is now 18
  nodes / 15 edges.

### Fixed

- The v3.48 `set_air_absorption` on `BasicPanner`/`VbapRenderer` was
  stored but never applied; the reflection path now consumes it (and
  the binaural renderer gained the field).

## [4.1.0]

### Added

- **Rust-native plugin ABI.** A versioned, `#[repr(C)]`
  C-ABI plugin specification for pure-Rust effect plugins, hosted in the
  production graph at the master insert seam (post-volume / seek-fade,
  pre-limiter). New workspace crates ship the whole surface:
  - `plugin-abi` — the spec crate: the `PluginVTable` vtable
    (descriptor / instantiate / prepare / set_param / process /
    save_state / load_state / reset / drop_instance), the
    `plugin_abi_v1()` handshake symbol gated by `PLUGIN_ABI_VERSION`,
    plain-data crossing types (`PluginDescriptor`, `AudioBlockMut`,
    bounded `PluginParams`, `StateBuffer`), a safe host facade
    (`PluginHost` / `PluginInstance`), a bounded static-plugin
    registry, and (behind the `host` feature) the `dlopen` /
    `LoadLibrary` loader.
  - `plugin-test-echo` — the reference plugin (delay + gain + wet/dry,
    `cdylib` + `rlib`): demonstrates every ABI obligation including a
    realtime-safe `process`; its `worst-case` feature deliberately
    allocates in `process` so the realtime suite can prove it catches
    offenders.
- **Plugin host insert in the production graph.** A new arena slot
  (`node_id::PLUGIN`, `ProdStage::PluginHost`) lowers into the MC chain
  between seek-fade and spatial. With no plugins configured the step is
  a bit-exact pass-through (pinned by the `graph_pipeline_equivalence`
  matrix). Plugins are attached per configured slot (library path or
  `static:<uid>` registry source) at generation build; a slot that fails
  to load is skipped — a broken plugin never interrupts playback.
  Live control (`SetPluginEnabled` / `SetPluginParams` commands,
  `EngineHandle::set_plugin_enabled` / `set_plugin_params`) travels as
  plain data over the per-node SPSC queue and survives generation swaps
  via sticky mirrors.
- **Engine feature `plugin-dylib`** for dynamic library loading; the
  static-registry path works without it.
- **Config: `EngineConfig.plugins` (`PluginHostConfig` /
  `PluginSlotConfig`)** with per-slot source / enabled / params / base64
  state, validated into the new typed `ConfigIssueKind::Plugin`
  ("plugin") issue kind.
- **Fidelity suite `tests/fidelity/plugin_host.rs`** (10 tests): ABI
  conformance, exact-sample delay semantics, in-graph processing with
  runtime bit-exact toggle + swap survival, live params over the queue,
  broken-source tolerance, zero-allocation plan step, worst-case
  offender detection, config validation, f64-domain demote/promote, and
  quality-precision processing. Plus a plugin case in
  `tests/fidelity/realtime_allocation.rs`.

### Fixed

- None (additive release; the default chain remains bit-exact).

### Changed

- `engine` + `config` bumped to 4.1.0 in lockstep (SemVer minor: purely
  additive). The production topology is now 18 nodes / 15 edges.

## [4.0.0]

### Changed / Breaking

- **Legacy `dsp::graph` module removed.** The public module
  `engine::dsp::graph` no longer exists. Its node arena, compiled-plan
  execution, `GraphGeneration` swaps, per-node SPSC control queues, and node
  implementations moved to `dsp::graph2::prod::arena` as a crate-private
  internal of the Graph 2.0 production engine. Hosts reach the surface
  through the `dsp::graph2::prod` re-exports (previously the `dsp::graph`
  exports): `DspGraph`, `DspNode`, `GraphControlHandle`, `GraphGeneration`,
  `GraphScratch`, and the node types (`MixBusNode`, `AuxBusNode`,
  `SpatialNode`, `DuckState`, `PanLaw`, `AutomationTarget`, …), also
  re-exported via the crate prelude.
- **The hand-authored plan source is gone.** `PlanSet::compile()` is
  removed; the Graph2 topology lowering (`dsp::graph2::prod::lowering`) is
  the single plan source for every generation (construction, reconfigure,
  and generation builds all lower).
- **Shadow verification mode removed** along with the `graph2_shadow_verify`
  config flag (a breaking `EngineConfig` field removal): the legacy-plan
  twin, its control fan-out, and the per-block bit-compare are no more.
  The `Graph2Engine::with_both` accessor seam is renamed `with_graph`.
- **`graph_pipeline_equivalence` re-pointed**: the fidelity gate now
  bit-compares the **Graph2 engine** (Graph2-lowered plans over the arena)
  against the frozen `DspPipeline` oracle — the same 27-scenario matrix,
  same `to_bits` comparisons, same structural parity assertions
  (node-set + latency). The `graph2_graph_equivalence` suite
  (Graph2-vs-legacy `DspGraph` A/B) is deleted with the legacy plan source
  it pinned.
- **FFI unchanged in behavior**: the C-FFI surface, `EngineCommand`s,
  `EngineEvent`s, and `PlaybackInfo` telemetry are byte-identical; only the
  Rust-internal plan provenance and module paths changed.
- `DspPipeline` stays as the frozen bit-exact oracle (unchanged).

## [3.52.0]

### Added

- **Graph 2.0 production node kinds** (`dsp::graph2::node`): the
  full production stage set — mix bus, aux bus, correction, EQ, dynamics,
  convolution, balance, crossfeed, stereo, timestretch, volume, seek-fade,
  routing, resampler, limiter, dither, spatial — exists as
  `NodeKind::Prod(ProdStage)` with per-stage capability descriptors
  (stateful / realtime-safe / latency taps) and the arena-slot mapping, so
  the engine chain can be **described as a topology** (typed ports, edges,
  validation, topological compile).
- **Graph2 production topology + plan lowering** (`dsp::graph2::prod`): the canonical signal chain as a real `Graph2` graph, compiled
  by the deterministic topological sort and **lowered onto the production
  `PlanSet`** — the execution plans every generation carries are now
  topology-derived instead of hand-authored, while the node arena, config
  application, user-state replay, control queues, and swap machinery stay
  the single `dsp::graph` implementation (exactly one node implementation:
  bit-exactness by construction, pinned by the
  `lowered_plans_match_handauthored` unit test).
- **`Graph2Engine`** (`dsp::graph2::prod`): the production engine shell on
  Graph 2.0 — a drop-in replacement for `DspGraph` mirroring the full
  public surface (9 block entry points, the complete queued-control
  mutator set, typed node accessors, lifecycle, telemetry reports) with
  `with_both` as the fan-out seam for accessor-style mutations. Exported
  via the prelude alongside `Graph2ControlHandle` (the cloneable
  cross-thread surface, method-for-method mirror of
  `GraphControlHandle`).
- **Graph2 parity suite** (`tests/fidelity/graph2_graph_equivalence.rs`): 47 tests — the 27 `graph_pipeline_equivalence` scenarios plus
  the 14 control-surface extensions (aux bus, ducking, slot
  automation, correction, spatial, lanes, seek-fade, speed, routing,
  precision, generation swap, queue backpressure), each bit-comparing
  `Graph2Engine` (lowered plans) against `DspGraph` (hand-authored plans)
  sample-for-sample (`f32::to_bits`).
- **Shadow verification mode** (`graph2_shadow_verify` config flag): with the flag on, the engine keeps a legacy-plan `DspGraph`
  twin driven in lock-step (every control mutator fans out; accessor
  mutations go through `with_both`) and **bit-compares every processed
  block** (`f32::to_bits`) — the A/B that proves the lowered plans drive
  the same nodes in the same order. A mismatch is a diagnostic (counter +
  first-difference), never an output change; the active engine's output
  always stands. Default **off** (the comparison deliberately allocates:
  it is a CI/diagnostic instrument, not a realtime stage) — pinned by a
  dedicated realtime test.
- **Engine migration onto Graph2Engine**: `AudioEngine.graph`
  is a `Graph2Engine`; the decode loops, tick thread, command handlers,
  and spatial persistence all drive the Graph2-lowered production path
  with the command/event/telemetry/FFI surfaces unchanged.
- **Graph2 realtime zero-allocation cases** in
  `tests/fidelity/realtime_allocation.rs`: the production stereo path,
  the multichannel path (lowered `NormalMc` plan), and the generation-swap
  adopt all prove zero allocation on the audio thread — identical contract
  to `DspGraph`; plus the shadow-mode default-off/diagnostic-only pin.

## [3.51.0]

### Added

- **Production node parity substrate**: production `NodeKind::Prod` kinds,
  the `graph2::prod` topology/lowering, the `Graph2Engine` shell, the
  `Graph2ControlHandle` mirror, and the parity-suite harness landed
  together with the Graph2Engine migration (see 3.52.0 above for the unified
  description).

## [3.50.0]

### Added

- **Graph 2.0 realtime lowering substrate** (`dsp::graph2::rt`): a
  compiled `ExecutionOrder` is now executable on the audio thread with **zero
  allocation**. [`RtPlan`] snapshots a topology into an immutable,
  fully-preallocated form — per-edge plane pools sized from the plan, fixed
  scratch, precomputed input/output adjacency, per-node state (delay rings,
  convolution overlap-add queues, HRTF per-ear pipelines, ratio-1 resampler
  windows, acoustic spectral kernels + raw-history rings) — and
  [`RtExecutor`] renders it **enum-dispatched per block** (never trait
  objects), adopting new plans at block boundaries through an atomic-pointer
  publish / swap / retire handshake (the generation-swap discipline, reused). Control-side state (HRTF dataset IRs, baked acoustic scenes,
  listener drives) is resolved at plan build into fixed buffers, so a scene
  swap is a rebuild + publish, never a live mutation. Deliberate initial scope boundaries (documented in `rt`'s module docs and pinned by tests):
  resampler nodes are ratio-1 only, sinks sum into the caller's output, and
  node state starts fresh on a plan swap (state carry lands with the control-surface migration).
- **Shared node-processing kernel seam** (`dsp::graph2::exec::ops`): one set of per-node processing kernels (`kernel_gain`, `kernel_delay`,
  `kernel_source`, `direct_convolve_into`, `to_ir_taps_into`, …) used by
  **both** executors, so offline and realtime renders go through the exact
  same arithmetic — divergence is structurally impossible.
- **Graph2 RT fidelity suite** (`tests/fidelity/graph2_rt_offline_equivalence.rs`):
  17 bit-exact cases (`f32::to_bits`) across the topology matrix — gain
  chains, sine/impulse sources, multi-plane Split/Mix, delay across block
  boundaries, convolution overlap-add, HRTF per-ear alignment, Buffer
  one-shot/loop/multi-channel replay, ratio-1 resampler, baked acoustic room
  responses, dry/wet buses, conv/delay parallel-branch latency alignment,
  the plan publish/swap/adopt handshake, and multi-sink summing — each swept
  over block sizes {1, 16, 256, 1024}.
- **Zero-allocation case for the Graph2 RT executor** in
  `tests/fidelity/realtime_allocation.rs`: a full-topology RT render (source,
  split, gain, delay, convolution, HRTF, resampler, mix) with a mid-loop plan
  publish must perform zero heap allocations on the audio path.

### Fixed

- **`Graph2` topological order with multi-edge node pairs** (`dsp::graph2::sort`):
  Kahn's decrement ran once per *producer pair* instead of once per *edge*, so
  two channels of one `Buffer`/`Split` feeding the same `Mix` left the mix
  with residual in-degree and compilation failed with a spurious `Cycle`.
  The decrement now counts edges from the ready node exactly.

### Changed

- **`dsp::graph2::exec.rs` (2,434 lines) split into `dsp::graph2::exec/`**
  per the `dsp/pipeline/` house pattern: `mod.rs` (wiring, `OfflineExecutor`
  struct, control surface, `process_block` dispatch), `offline.rs` (per-node
  `run_*` ops), `ops.rs` (the shared kernels), `buffers.rs` (per-node
  pipeline-state types + overlap-add / windowed-sinc math, with new
  allocation-free `*_into` forms), `tests.rs` (the offline battery, moved
  verbatim). Behavior is unchanged; the offline executor's public API is
  identical.
- `Kernel lengths ≥ 512 route a Convolution node through the partitioned-FFT
  engine` — [`CONVOLUTION_FFT_THRESHOLD`] moved from `exec.rs` to
  `exec::ops` and re-exported from `dsp::graph2` unchanged.

## [3.49.0]

### Added

- **Versioned configuration envelope + migration framework** (`config`).
  New `VersionedConfig` wraps an [`EngineConfig`] with a `version` schema
  ({ `version`: 1, …config } via `#[serde(flatten)]`); `load`, `save_pretty`
  and `migrate` expose an explicit, future-proof upgrade path. A legacy
  pre-versioning payload (a bare `EngineConfig` JSON) deserializes as
  already-current — the historical no-op guarantee — and `EngineConfig`
  itself is unchanged, so hosts using it today keep working. Re-exported as
  `config::{CONFIG_VERSION, migrate_step, VersionedConfig, ConfigLoadError}`.
- **Unified quality-evaluation harness.** New [`eval`]
  module: a versioned [`ReferenceVectorRegistry`]
  (content-addressed via the aelog `SHA-256` substrate — a changed
  expectation changes the address, so a drifting spec is always
detectable) plus objective measurement primitives (Goertzel amplitude,
  THD+N, bit-exactness, DTFT impulse-response magnitude **and phase**) and
  **nine** DSP/spatial suites: [`DspPipeline`] bit-exact + transparency,
  parametric-EQ biquad frequency **and phase** response, limiter true-peak
  ceiling, resampler in-band gain, binaural inter-aural level, **EBU R128
  loudness** (BS.1770-4 reference tone), **partitioned-FFT convolution vs
  a naive-direct reference**, **channel separation / crosstalk**, and
  **HRTF-interpolation convexity** against the measured grid.
  [`EvaluationReport`] renders a human-readable PASS/FAIL table
  (`render_text`) and machine-readable JSON (`to_json`), and
  [`eval::EvaluationReport::compare`] diffs two engine versions into a
  [`VersionComparison`] (unchanged / drift / improvement / regression) so
  regressions are detected automatically across builds.
  [`eval::run_quality`] assembles every suite, and the `quality_harness`
  fidelity test asserts all components pass, the report is deterministic +
  versioned, and cross-version drift is detectable. Everything runs off
  the audio path; measurement numbers mirror the existing
  golden/fidelity conventions.
- **Consolidated, versioned track metadata model** (`decode`). New
  [`TrackMetadata`] aggregates the previously scattered metadata into one
  `Clone`/`PartialEq` model: editorial [`TrackTags`] (title/artist/album /
  album-artist/genre/date/track/disc numbers/artwork reference), duration,
  technical [`AudioFormatInfo`], loudness tags
  ([`LoudnessMetadata`]), and opt-in offline measured loudness
  ([`LoudnessScanResult`]) and chapters ([`CueSheet`]).
  [`TrackMetadata::from_path`] is cheap (tags + loudness reads, no decode);
  `from_path_with_measurement` runs a full scan. Reuses the existing
  codec-routing extractors, so values match what playback reads on load.
  `LoudnessMetadata`, `GaplessInfo`, `AudioFormatInfo` now derive
  `PartialEq` so the aggregate is comparable.
- **Spatial-health diagnostics (explainable per-source status).** New
  [`spatial::SpatialHealthSnapshot`] derives *why* the spatial render
  behaves as it does — **localization quality** (measured-HRTF grid
  coverage or the analytic fallback, plus angular-spread blur),
  **direct-vs-reflected energy ratio** (direct path gain vs room send ×
  wall reflection coefficient), **occlusion severity** (applied
  attenuation + low-pass cutoff), and **phase risk** (measured
  inter-channel correlation of the master output + per-source spread /
  extreme-pan heuristics) — as a serde-serializable per-source report with
  stable `HealthLevel` codes (`inactive`/`good`/`moderate`/`poor`) and a
  human note per factor. Runs entirely on the **telemetry/control path**
  (the engine tick, from the existing meter snapshot + scene + voice
  counts); the audio path is untouched. `PlaybackInfo` gains
  `spatial_health`, the metering snapshot gains the measured
  `stereo_correlation` (cross-energy accumulation, allocation-free and
  opt-in), and the C FFI gains `engine_spatial_health`.
- **Deterministic AudioProfile perceptual layer.** New
  [`profile`] module fusing loudness, dynamics, spectral, transient,
  stereo, spatial, and content measurements into one versioned,
  serializable [`AudioProfile`] with documented units/ranges and
  confidence semantics (duration × sub-profile coverage). Built entirely
  on deterministic DSP — BS.1770-4 loudness via the shared
  [`LoudnessMeter`] (identical to the scanner), a Hann-windowed FFT
  power average (centroid / rolloff / flatness / slope / brightness),
  windowed onset detection, and running L/R + mid/side statistics
  (correlation / width / balance / phase risk / side fraction).
  [`AnalysisMask`] lets consumers request only the analysis they need;
  the bounded-memory streaming [`ProfileAnalyzer`] and
  `analyze_decoder`/`analyze_path` run entirely off the audio path;
  and the on-disk [`profile::cache`] persists results validated against
  file size/mtime and the schema version, optionally deduplicating
  identical content across paths via a content fingerprint
  (`analyze_path_cached_by_fingerprint` with the `fingerprint`
  feature). Content-class probabilities are normalized heuristic
  indicators with an explicit no-evidence prior — never a hidden
  learned model.
- **Typed, serializable diagnostics.** New `engine::DiagnosticKind`
  (`EngineFault` / `TrackLoad` / `Decode` / `Output` / `BitPerfect` /
  `Configuration`) and `engine::BitPerfectCause` (all stable snake_case
  codes) replace the previously string-only `EngineStats`
  `engine_error`/`bit_perfect_reason` fields with structured categories;
  the message strings remain for humans and stay exactly as before.
  [`PlaybackInfo`] exposes a serializable `engine_diagnostics: Vec<Diagnostic>`
  snapshot, and the C FFI gains `engine_diagnostics_info` (typed kind /
  bit-perfect cause codes + the human message) so hosts can query typed
  diagnostics without parsing prose. Config validation is now
  categorized too: `config::{ConfigIssue, ConfigIssueKind, ConfigSeverity}`
  ride alongside `ConfigValidation::errors`/`warnings` (same checks, same
  messages), giving hosts a stable machine-readable `kind.code()` per issue.

### Changed

- **Spatial seam reconciliation (docs/code consistency).** The binaural
  module docs no longer claim the head model is "azimuth-only": elevation is
  carried by the pinna [`ElevationNotch`] in the analytic path and by
  bilinear elevation interpolation in a loaded [`HrtfDataset`]. `object`/
  `scene` velocity docs now describe the live per-block Doppler path
  (`object.velocity − listener.velocity`) instead of "future Doppler"; the
  panner's air-absorption docs reflect that the HF roll-off is applied, and
  the `level::DistanceModel` and `SpatialSourceType` docs are corrected.
  NetCDF-4/HDF5 `nc4` SOFA is **explicitly deferred with rationale** in the
  spatial module docs: the robust readers link `libhdf5`, conflicting with
  the pure-Rust/no-FFI rule, and the typed rejection already isolates the gap
  behind `HrtfCorpus`.

## [3.48.0]

### Added

- **Per-path air absorption / distance roll-off on the spectral kernels.**
  [`BakedScene`] now carries a scene-scoped [`AirAbsorption`] model; when a
  host enables it, every non-direct spectral kernel the `Acoustic` node
  renders is additionally shaped by a **per-path, distance-dependent HF
  roll-off** `1 / √(1 + (f/f_air)²)` where `f_air =
  [`AirAbsorption::cutoff_hz`]`(path.distance)` — so a farther reflection
  genuinely darkens with travel distance, exactly as the acoustic bake
  intends, while staying equal at DC.

  - [`BakedScene::set_air_absorption`] attaches the model (default = the
    disabled [`AirAbsorption::default`], keeping every kernel — and every
    golden render — **bit-identical** to v3.47); serialized on the scene with
    `#[serde(default)]`, so older baked-scene logs load with air off.
  - New `spectral_taps_with` / `path_filter_kernel_with` thread the model
    through the kernel builder; `spectral_taps` and `path_filter_kernel`
    keep their exact signatures (disabled model, unchanged). The `Acoustic`
    node (offline `run_acoustic`) now reads the attached scene's air model.
  - With air enabled even a spectrally flat path becomes a distance-darkened
    low-pass kernel rather than a single-tap delta.

### Changed

- [`AirAbsorption`] derives `Serialize`/`Deserialize` so the baker can embed
  it verbatim.


### Added

- **Realtime room reflections are now spectrally coloured.** The per-path
  spectral model the offline `Acoustic` node renders exactly (a material's
  per-band [`MaterialSpectrum`] or a collapsed diffraction/transmission
  low-pass corner) is forked into the **production hot path**: the baked
  path responses [the `BasicPanner`, `VbapRenderer` and `BinauralRenderer`
  place from a [`BakedScene`]] now fall back on a **one-pole low-pass per
  reflected image** realised from that same corner, so a curtain-darkened
  or diffracted reflection genuinely loses its highs instead of just being
  scaled.

  - [`ListenerImage`] gains a `lowpass_hz` field (∞ = spectrally flat). The
    live scalar-`Room` solve fills ∞ (bit-identical to before);
    `BakedScene::listener_images` derives the corner per reflection — from
    the path's full per-band spectrum via `surface_lowpass_hz` when one is
    present, else its collapsed diffraction corner, with near-Nyquist
    corners collapsed back to ∞ so flat materials stay strict passthrough.
  - [`EarlyReflections`] gains per-(object, image) one-pole low-pass state
    (`set_reflection_filter` block-rate setup, `filter_reflection` for the
    binaural direct-ring reads) and applies it inside `object_frame`;
    entirely preallocated at `prepare`, allocation-free and lock-free on the
    audio thread (verified by the realtime suite). The panner/VBAP tap path
    and the binaural fractional-delay path both colour their reflections.
  - Reflection level is unchanged (`coeff`/`gain` still applies) — the
    low-pass is the spectral roll-off “from the corner”, so a damped
    wall stays as loud where it reflects bass/deps as before while its
    treble genuinely rolls off.

### Changed

- `ListenerImage` grew a field (`lowpass_hz`). It is `Copy` and carries
  only a default-∞ added member; all call sites updated.


### Added

- **Graph-based binaural branches now use real measured head-related
  responses.** [`NodeParams::HRTF`] gains a `source` field:
  [`HrtfSource::Inline`] keeps the classic hand-authored `left`/`right`
  tabs (backward compatible; reported taps stay `max(left.len,
  right.len)`), while new **`HrtfSource::Dataset { azimuth_deg,
  elevation_deg, taps }`** reads the executor's attached
  [`HrtfDataset`](crate::spatial::hrtf::HrtfDataset) via
  `OfflineExecutor::set_hrtf_dataset` and renders the **bilinearly-
  interpolated measured per-ear HRIRs** at that source direction — the
  same corpus the real `BinauralRenderer` consumes, now routable in the
  topology. New builders `Graph2::add_hrtf_dataset(name, az, el)` (renders
  at [`MAX_HRTF_TAPS`] = 128, mirroring the dataset) and
  `add_hrtf_dataset_with_taps(...)` (pass the dataset's own
  [`HrtfDataset::taps`](crate::spatial::hrtf::HrtfDataset::taps) to avoid
  zero-padding).
- The measured node **reports its taps to the latency pass like a
  `Delay`**: `node_latency` returns `taps`, its capabilities flag `taps`,
  and `compensate` aligns a merge by `Delay(taps)` on the opposing branch
  — so a dataset-driven binaural branch carrying real ITD/head-shadow
  impulse responses still lines up exactly with the dry leg.
- **Rendering.** `OfflineExecutor` gets `set_hrtf_dataset(Option)`;
  `run_hrtf` reads per-ear measured IRs with
  `HrtfDataset::bilinear_interpolate` (padded/truncated to the reported
  `taps` so reported and actual delay agree), then renders through the
  same per-ear streaming pipeline as the inline ears (both ears delayed
  together, pair aligned). A dataset node with no dataset attached falls
  back to pass-through, mirroring an unbaked `Acoustic`.

## [3.45.0]

### Added

- A **`Resampler` node** closes the last latency hook the v3.30 pass
  documented (alongside Delay, Convolution and HRTF). New
  [`Graph2::add_resampler`](`crate::dsp::graph2::Graph2::add_resampler`)
  / `add_resampler_with_quality` build a mono-in/mono-out sample-rate-
  conversion node that **reports its own taps to the latency pass**:
  `node_latency` returns the filter half-span `quality` (default
  [`RESAMPLER_DEFAULT_QUALITY`] = 32), its capabilities flag `taps`, and
  [`compensate`](`crate::dsp::graph2::latency::compensate`) aligns parallel
  branches around it exactly like a `Delay`. The node emits with exactly
  `quality` samples of pipeline delay — so its *reported* and *actual*
  taps agree, the same convention as the other tap-reporting nodes.
- **Rendering.** The executor's `run_resampler` resamples the input by a
  bandlimited Hann-windowed-sinc interpolator (`ratio ≥ 1` = output frames
  per input frame). The fixed-frame offline executor resamples onto its own
  frame grid (a rate/pitch remap), with a per-node input history ring that
  keeps interpolation continuous across blocks and a leading `quality`
  zeros to carry the reported taps; ratio 1 reproduces the input to
  interpolator accuracy, and higher ratios sample it at `1/ratio` the
  source rate.

## [3.44.0]

### Changed

- Long-kernel **`Convolution` nodes render through the realtime partitioned
  FFT engine** (`dsp::convolution`) instead of the O(N·M) direct path.
  Kernels ≥ [`CONVOLUTION_FFT_THRESHOLD`] (512 taps) route through the
  re-aimed [`OfflineExecutor`] partitioned overlap-add engine, so genuinely
  long impulse responses render fast offline; shorter kernels keep the
  exact byte-equal direct path (where partitioned FFT would both cost more
  and already exceed the front delay the node must present). The engine's
  UP-OLA latency (one partition, 512 frames) is absorbed into an extra
  front-padded delay so the node's **reported and actual contract is
  unchanged**: `output[k] = (x * h)[k - kernel.len()]`, and graph-wide
  latency/compensation still aligns convolution branches by the full kernel
  length. Falls back to the exact direct path transparently if the engine
  can't load the IR.

## [3.43.0]

### Added

- The **`aelog_replay` CLI** now hooks in the golden-render cache with a
  `--cache` flag and **hit/miss reporting**, so repeated `engine replay`
  runs of the same session skip re-rendering. Pass `--graph <graph.json>`
  (a serialized [`Graph2`] topology; `order` is recompiled on load),
  optionally `--sink <n>` to choose the capture sink (defaults to the
  graph's first Sink node), and `--cache-dir <dir>` (defaults to
  [`AelogCache::default_root`]). The first run reports `cache: MISS
  rendered & stored (<samples> B) under <sha-256 content address>`;
  repeated runs report `cache: HIT reused golden render` and splice the
  stored capture instead of rendering, including the capture size, byte
  count, and content address for reproducibility. `--verbose` also prints
  the rendered peak and end master position. Without `--cache` the CLI's
  original event-replay oracle output is unchanged. End-to-end covered by
  a new test that drives the compiled binary itself
  (`tests/fidelity/aelog_replay.rs`).

## [3.42.0]

### Added

- The aelog golden-render cache is now **size-bounded**: [`AelogCache`]
  enforces a byte budget by **LRU eviction**, so the app-data cache
  directory can't grow without bound. New `AelogCache::with_budget(root,
  bytes)` constructor (default [`DEFAULT_CACHE_BUDGET`] = 256 MiB;
  `0` = evict everything except the just-written entry), each entry stores
  a last-access `touched` stamp bumped on every lookup hit, and `insert`
  evicts the least-recently-used entries until the total sits at or below
  ​90% of the budget. A single capture larger than the budget is kept
  (never evicted by its own insert). Entries persisted before this version
  load fine (`touched` defaults to oldest).

### Changed

- Cache entries are now **content-addressed**: each golden-render file is
  named by the **SHA-256** (new dependency-free [`sha256`], pinned against
  the FIPS 180-4 vectors) of the canonical JSON of its render identity
  (log hash, graph hash, sink, sample rate, block frames) — instead of a
  machine-local FNV name. Because the render is a pure function of that
  identity, the name derives solely from semantic content: two machines
  rendering the same session through the same graph compute the *same*
  filename for the *same* stored bytes, so a synced or shared cache
  directory is valid on any host. New public `content_address` helper;
  the LRU `touched` stamp is deliberately excluded from the address so a
  local hit-touch never rewrites the shared name. The in-process memo and
  the `log_hash`/`graph_fingerprint` identity hashes are unchanged.

## [3.41.1]

### Fixed

- The aelog golden-render cache key no longer includes the log's **label**
  or `format_version`: [`log_hash`] hashes only the render-relevant
  content (sample rate, block cadence, and every recorded command), so
  semantically identical sessions — the same take re-labelled for a
  different song, say — **reuse one cached golden render** instead of
  missing and re-rendering. Sample-rate/command differences still split
  keys; the label itself is preserved in the log, it just doesn't join
  the key.

## [3.41.0]

**Musical automation** — tempo-mapped control curves on the AudioClock
drive graph parameters over time. A curve is authored in **beats** and
evaluated against a **tempo map**, so the same automation lands on the
correct samples as the tempo changes, and the graph's gain sweeps
smoothly over the session.

### Added

- `dsp::timeline::automation::CurveBeats`: a piecewise-linear control
  curve in musical time — `set(beat, value)`, `evaluate_beats(beat)`, and
  `evaluate(sample, &TempoMap, sample_rate)` which maps a sample back to a
  beat through the tempo map before interpolating (a tempo change just
  remaps where each beat lands).
- `OfflineExecutor` gain automation: `set_tempo_map` + `set_gain_automation`
  drive a Gain node from a curve, sweeping the gain with a
  **sample-accurate linear ramp** across each block (`master_sample` tracks
  the playhead); an explicit `set_gain_step` still wins for the block.
- aelog recording/replay of musical automation: `SetTempoMap` +
  `SetGainAutomation` commands (`record_tempo_map` /
  `record_gain_automation`), `ReplayOutcome::tempo_map` +
  `gain_automation`, and `replay_render` attaching them to the executor so
  a recorded session renders the exact gain sweep.

### Changed

- Beats were already scheduleable (`EventTime::Beat`, v3.28); this makes a
  *continuous tempo-mapped parameter curve* a first-class recorded input on
  top of that. The aelog format stays v3 (additive variants; old logs still
  load).

## [3.40.0]

**Per-path spectral filtering** in the `Acoustic` node — the collapsed
broadband gain for each reflection/diffraction path is replaced with a
real minimum-phase filter per path, so a room's material (and diffraction
corners) colour the sound the way they physically do.

### Added

- `BakedScene::spectral_taps` / free `spectral_taps` (+ `ACOUSTIC_IR_LEN`)
  render one `(excess_delay, FIR kernel)` per non-direct path.
- A reflection carrying its full per-band `MaterialSpectrum` shapes the
  source directly via `reflectivity_at_hz` (sampled per FFT bin and
  synthesized as a minimum-phase FIR with `dsp::correction::phase`).
- A diffraction/transmission path collapsed to a corner (a finite
  `lowpass_hz`, no spectrum — the `SPECTRAL_COLLAPSED` case) applies a
  one-pole low-pass at that corner, scaled by the broadband gain.
- A truly flat path (no spectrum, no corner) reduces to a single-tap gain
  delta, reproducing the classic broadband behavior exactly and for free.- An executor **`acoustic_epoch`** so the node recompiles its kernels when
  the world changes even if two worlds bake the same source cell.

### Changed

- **`render_cached` is now two-tier**: the free convenience entry point
  consults a **thread-local in-process memo** (keyed by the same
  `(log_hash, graph fingerprint, sink)` tuple as the file cache) before
  the persistent `AelogCache`, so a second render of an identical log in
  the same process reuses the captured golden audio instead of
  re-rendering — `replay_events` (pure, cheap) + a splice, byte-identical
  to a fresh render. `clear_memo()` empties the fast layer; the memo is
  capped (`MEMO_CAP`) so a long-lived process can't hoard render memory.




### Changed

- `run_acoustic` renders each non-direct path by convolving it against the
  raw input-history ring (delay ∘ filter, both LTI, so they commute) at
  its excess delay — kernels (re)compile on scene swap / listener drive
  while the ring keeps the room ringing; `ACOUSTIC_HISTORY`-deep ring
  never drops session history. A flat scene's output is **byte-identical**
  to the previous broadband renderer; only non-flat materials change.
- `acoustic_taps` (the broadband reduction) is now test-only.

### Fixed

- Golden oracles and the fabric dampening checks now measure per-path
  spectral filtering (and broadband gain) instead of the collapsed taps.

## [3.39.0]

Acoustic nodes support **per-listener baked scenes**: a node can name a
scene from an executor registry, so a single graph renders **distinct
room responses for several listeners** and mixes them in the topology
(instead of every node sharing one global scene).

### Added

- **`NodeParams::Acoustic { position, scene: Option<String> }`**
  (`dsp::graph2`) — `scene: Some(name)` renders from the executor's named
  scene registry; `scene: None` (the plain `add_acoustic`) keeps using the
  active global scene. `Graph2::add_acoustic_scene(name, position,
  scene_id)` builds a scene-addressed node; `describe`/`to_dot` show the
  id.
- **`OfflineExecutor::set_scene(name, scene)` / `remove_scene(name)`** — a
  per-listener bake registry keyed by id. `run_acoustic` selects per node:
  named scene if the params say so, else the active scene; an unregistered
  id (or unbaked position) falls back to pass-through, and the tapped
  delay lines keep ringing through a replacement. The v3.38 listener
  position drive and the v3.37 scene swaps compose unchanged.

### Fixed

- None.

## [3.38.0]

The replayed listener trajectory now **drives** the graph: `replay_render`
retargets every `Acoustic` node's baked lookup from the recorded
`SetListenerPosition` stream, so a spatial golden render exercises the
full baked-room path — the room response is re-dered from the position
cache *as the listener moves*. Nodes keep their `NodeParams::Acoustic`
position as the fallback when no listener is driving.

### Added

- **`OfflineExecutor::listener_position`** (`dsp::graph2`) — a live
  listener position (`set_listener_position(position)`) that overrides
  each `Acoustic` node's lookup position; `None` restores the node's own
  position. `run_acoustic` re-looks-up the cell each block, so a moving
  listener walks through baked cells and unbaked regions fall back to
  pass-through, while the tapped delay lines keep ringing.
- **`replay_render`** (`dsp::aelog`) — applies each
  `SetListenerPosition { at, position }` to the executor before the block
  it covers (sample-exact on faithful logs, alongside scene swaps), so
  listener motion is no longer a report-only input.

### Fixed

- None.

## [3.37.0]

Animated acoustic worlds become deterministic: a `BakedScene` swap is a
recorded aelog command, so the geometry timeline of a session replays
exactly. The scene embeds in the log verbatim (order-stable serde — the
response cache is a `BTreeMap`, the `f32::INFINITY` low-pass sentinel
round-trips via a `−1.0` marker, the solver world is skipped), and
`replay_render` re-attaches each swap at its master sample **without
resetting** the `Acoustic` nodes' tapped delay lines — the room keeps
ringing through the change.

### Added

- **`RecordedCommand::SetBakedScene { at, scene }`** (`dsp::aelog`) —
  stamped with the master sample at record time;
  `AelogRecorder::record_baked_scene` logs a swap. Format stays v3 (an
  additive variant — old v3 files still load).
- **`OfflineExecutor::swap_baked_scene`** (`dsp::graph2`) — replaces the
  active scene mid-session without clearing the tapped delay lines, so a
  geometry change (a door opens, a wall turns to fabric) shifts the
  response seamlessly; `set_baked_scene` remains the fresh-attach reset.
- **Deterministic scene serde** (`spatial::acoustic::bake`) — `BakedScene`
  / `BakedObject` / `BakedPath` serialize (cell cache as an ordered entry
  list, `lowpass_hz` infinity sentinel), so aelog logs and hashes are
  pure functions of their commands.
- **Replay** — `ReplayOutcome::scene_swaps` exposes the `(master sample,
  scene)` timeline; `replay_render` applies each swap before the block it
  covers (sample-exact on faithful logs).

### Fixed

- None.

## [3.36.0]

Audio inputs go **multi-channel**: `Buffer` nodes and the aelog
`InputAudio` chunks carry **channel-major planes** (`track[0]` = channel
0, …), so stereo and spatial sessions record, reconstruct, and replay
every channel exactly. A `Buffer` node exposes one mono output port per
channel (the HRTF convention), and mono clips/tracks keep working
unchanged (a mono clip is simply a one-plane clip).

### Added

- **Multi-channel buffers** (`dsp::graph2`) — [`NodeParams::Buffer`]
  `samples` is now channel-major planes; `Graph2::add_buffer_channels` /
  `add_buffer_clip_channels` build N-port sources (one mono output port
  per channel). External tracks (`OfflineExecutor::set_external_input` /
  `set_external_clip`) follow the same layout; a shared cursor advances
  all channels in lockstep, and a mono track on an N-port node reads
  silence on the missing channels (no upmix).
- **Multi-channel aelog** — `AelogRecorder::record_audio_input_channels` /
  `record_clip_audio_channels` join the mono conveniences;
  `RecordedCommand::InputAudio` chunks are channel-major planes (format
  v3 — `AELOG_VERSION` bumped).
- **Multi-channel replay** — `ReplayOutcome::audio_input` /
  `clip_tracks` reconstruct channel-major tracks (a mono session yields
  one plane); `replay_render` feeds them to the executor so stereo/
  spatial sessions render byte-exact per channel.

### Fixed

- None.

## [3.35.0]

Audio inputs become **clip-addressed**: a `Buffer` source node carries an
optional clip address, the executor registers per-clip external tracks,
and aelog records each audio-input chunk with its clip — so a recorded
session's tracks route only to the nodes bearing that address, enabling
**multi-input graphs** (one graph mixing several recorded inputs). The
unaddressed single-track path is unchanged.

### Added

- **Clip-addressed buffers** (`dsp::graph2`) — [`NodeParams::Buffer`]
  gains `clip: Option<String>`; `Graph2::add_buffer_clip(name, clip, …)`
  builds an addressed source. An addressed node plays the per-clip track
  registered for its name (`OfflineExecutor::set_external_clip`); an
  unaddressed node plays the global external track
  (`OfflineExecutor::set_external_input`); either falls back to the
  embedded clip.
- **Per-clip aelog recording** — `AelogRecorder::record_clip_audio(clip,
  chunk)` joins `record_audio_input`; `RecordedCommand::InputAudio` now
  carries the optional clip (format v2 — `AELOG_VERSION` bumped).
- **Per-clip replay** — `ReplayOutcome::clip_tracks` reconstructs one
  `(clip, track)` pair per address in first-recorded order; `replay_render`
  feeds each track only to the matching nodes, so a multi-input session
  replays byte-identically.

### Fixed

- None.

## [3.34.0]

Binaural and convolution-heavy branches join the latency pass: two new
Graph 2.0 nodes — [`NodeKind::Convolution`] (1:1 FIR convolver) and
[`NodeKind::HRTF`] (mono-in / stereo-out binaural filter) — report and
compensate **exactly like `Delay` nodes**. The convolver reports its
kernel length as taps; the HRTF node reports the longer of its two
per-ear IRs and delays both ears by that length so the pair stays
mutually aligned. The executor renders both with a streaming
**overlap-add** convolution pipeline whose delay never drifts, so the
reported taps and the rendered timing agree at any block count.

### Added

- **Convolution node** (`dsp::graph2`) — [`NodeKind::Convolution`] with
  [`NodeParams::Convolution { kernel }`](NodeParams): convolves the input
  with an embedded FIR kernel and emits with one kernel-length pipeline
  delay (the block-partitioned lookahead convention). `node_latency` =
  `kernel.len()`. `Graph2::add_convolution` builds it.
- **HRTF node** — [`NodeKind::HRTF`] with
  [`NodeParams::HRTF { left, right }`](NodeParams): mono in, left/right
  ear out (ports 0/1), per-ear IR convolution, both ears delayed by the
  longer IR. `node_latency` = `max(left.len(), right.len())`.
  `Graph2::add_hrtf` builds it.
- **Streaming convolution pipeline** (`exec.rs`) — overlap-add across
  blocks with a constant-length delay queue: `output[k] = (x * h)[k - N]`
  exactly, at any block count; the per-ear pipeline delay is shared so an
  HRTF pair stays mutually aligned.
- **Latency pass** — `analyze`/`compensate` propagate the new taps like
  any `Delay`: a dry/wet diamond with a 300-tap convolver compensates to a
  single summed sample at 300, and a binaural branch aligns a dry leg to
  its 300 taps while preserving node ids.

### Fixed

- None.

### Changed

- None.

## [3.33.0]

Golden renders become cacheable: **a render cache keyed by a
deterministic hash of the aelog session** (plus the graph fingerprint and
sink) stores captured audio on disk, so identical logs reuse the stored
render instead of re-rendering. The hash is dependency-free FNV-1a over
the canonical JSON — identical sessions hash identically, any command
difference changes the hash — and the cache is best-effort: corrupt or
missing entries are misses, never wrong renders, and writes are atomic.

### Added

- **Render cache** (`dsp::aelog::cache`) — [`AelogCache`] with
  [`AelogCache::lookup`] / [`AelogCache::insert`] /
  [`AelogCache::render_cached`] and a default root under the app data
  directory; [`log_hash`] / [`graph_fingerprint`] expose the stable
  hashes. `render_cached` returns the stored capture on a hit (the cheap
  event stream is recomputed from the log — pure — and only the audio
  comes from the cache) and renders + stores on a miss.
- **Keying contract** — the key folds in the graph fingerprint and the
  sink id because a golden render is a pure function of
  `(log, graph, sink)`: the same log through a different graph is a
  separate entry, never a wrong render.
- **Robustness** — entries carry the hashes and header back and are
  re-verified on load; a collision or corrupted file degrades to a miss.
  Writes are temp-file + rename.

### Fixed

- None.

### Changed

- None.

## [3.32.0]

Aelog now records **every render input, not just timeline commands**:
audio fed into the graph and listener motion join the event log, so
spatial sessions replay exactly. A new `Buffer` source node in Graph 2.0
plays an embedded clip (one-shot or looping) or an externally supplied
track; the recorder logs each audio chunk and every listener position as
master-sample-stamped commands; replay reconstructs the full track and
listener trajectory and feeds the track back into the executor — the
final pieces of the guide's deterministic golden-render pipeline.

### Added

- **Buffer source node** (`dsp::graph2`) — [`NodeKind::Buffer`] with
  [`NodeParams::Buffer { samples, looping }`](NodeParams): a graph input
  primitive that plays its embedded clip, or the executor's external
  track when one is attached (`OfflineExecutor::set_external_input`).
  `Graph2::add_buffer` builds it; `to_dot` labels it.
- **Audio-input recording** — [`RecordedCommand::InputAudio`]: chunk-wise
  audio input logs; replay concatenates chunks into the exact session
  track (`ReplayOutcome::audio_input`) and feeds it into the render so
  captures are byte-identical.
- **Listener-motion recording** — [`RecordedCommand::SetListenerPosition`]
  stamped with the master sample at record time; replay returns the full
  `(sample, position)` trajectory (`ReplayOutcome::listener_motion`) for
  spatial renderers to re-apply sample-exactly.
- **Recorder surface** — [`AelogRecorder::record_audio_input`] and
  [`AelogRecorder::record_listener_position`] mirror the timeline's
  mutation discipline: every input is a command, so a session is a pure
  function of its log.

### Fixed

- None.

### Changed

- `Vec3` (spatial math) gained serde derives, keeping serializable graphs
  and logs position-exact.

## [3.31.0]

The acoustic world joins the graph: **reflections and baking become
graph-routable primitives**. A new [`NodeKind::Acoustic`] in Graph 2.0
renders the baked room response of a source position from a
[`BakedScene`] attached to the executor — an impulse into the node comes
out as the direct path plus one delayed, gain-scaled copy per baked
propagation path. Wet rooms route through `Split`/`Mix`/`Gain` like any
other signal.

### Added

- **Acoustic node** (`dsp::graph2`) — [`NodeKind::Acoustic`] with
  [`NodeParams::Acoustic { position }`](NodeParams): 1-in/1-out; the
  input plane passes through (scaled by the baked direct gain) plus each
  non-direct path at its excess delay with its gain, via a per-node tapped
  delay line sized to the longest tap.
- **Executor hook** — [`OfflineExecutor::set_baked_scene`]: attach a
  v3.26 `BakedScene`; `Acoustic` nodes look up their configured position's
  cell. An unbaked position or a missing scene passes the input through
  unchanged (deterministic fallback, matching the renderers' live-solve
  fallback semantics).
- **Latency semantics** — the acoustic node reports **zero pipeline
  latency** (the direct path passes immediately; the tail is wet content,
  not alignment delay), so `analyze`/`compensate` treat a room exactly
  like a signal with no latency to align — documented in
  `dsp::graph2::latency::node_latency`.
- **Serializable** — [`Vec3`] gains serde derives, so a graph containing
  acoustic nodes round-trips through JSON position-exactly.
- **Fidelity suite** — `tests/fidelity/acoustic_graph.rs` (6 tests): an
  impulse into the node reproduces the baked response **exactly** (oracle
  built from the same paths); a wet room + dry `Gain` route through
  `Split`/`Mix` with reflections summed per-excess-delay; unbaked
  positions and missing scenes pass through; the node adds no pipeline
  latency; the graph serializes/round-trips with positions intact; and a
  fabric-wall bake changes the rendered taps (weaker reflections).

## [3.30.0]

Graph-wide latency and alignment: timing relationships become explicit
and correct across arbitrary Graph 2.0 topologies. A new `dsp::graph2::latency`
pass reports per-node taps and cumulative upstream latency, and
**automatically compensates** parallel branches so every path into a merge
point arrives aligned to the slowest — a convolution/delay branch and a dry
branch no longer need hand-rolled delays.

### Added

- **Per-node latency accounting** — [`node_latency`]: every node reports
  its intrinsic sample taps (only `Delay` today; a future convolution /
  HRTF / resampler / lookahead node plugs in the same way).
- **Graph-wide propagation & diagnostics** ([`analyze`],
  [`LatencyReport`]) — validates, topologically schedules, and propagates
  cumulative upstream latency along the edge set: a `Mix` reports the
  slowest of its inputs, the report gives per-node `upstream` and `taps`
  plus the graph `total_samples` / `total_ms`.
- **Automatic delay compensation** ([`compensate`]) — returns an edited
  copy of the graph with a compensating `Delay` spliced in series on every
  faster branch into a merge point, so all inputs arrive aligned to the
  slowest. **Original node ids are preserved verbatim**, so a Timeline
  event addressing a node by id (e.g. `SetGain`) keeps working on the
  compensated graph; only new `Delay` nodes are added, and the result is
  re-validated.
- **Fidelity suite** — `tests/fidelity/latency_alignment.rs` (6 tests): a
  dry/wet diamond renders unaligned (dry @0, wet @300) then, after
  compensation, as a single aligned spike at 300 summing both branches
  (0.5 + 1.0); a deep 100+200-tap chain sums to 300 with no compensation
  inserted; the report propagates (mix upstream 300, taps 300/0, total_ms
  6.25); a Timeline `SetGain` on the dry node id still lands after
  compensation (3× sine once gated, 160 Hz phase-locked to the 300-tap
  delay); a three-way fan-out aligns all branches to the slowest (200 +
  100 taps, summed at sample 200); and analyze/compensate are
  deterministic.

## [3.29.0]

Reference rendering and determinism: a new [`dsp::aelog`] module records a render session (every
timeline mutation and block advance) into a versioned, serializable
`recording.aelog`, and replays it deterministically to reproduce identical
events and **byte-identical captured audio** — the project's
**golden-render substrate**. A bug report becomes "replay this log", and a
regression check becomes "compare the replay against the golden capture".

### Added

- **Aelog format** (`dsp::aelog`) — [`SessionHeader`] (versioned, sample
  rate, block size, label — deliberately no wall-clock timestamps, so a
  log is a pure function of its commands), [`RecordedCommand`] (Schedule /
  SetTempo / SetTimeSignature / SetLoop / SetLoopEnabled / SetTempoRamp /
  SetState / SetQuantize / Advance), and [`Aelog`] with JSON string and
  file round-trips (`to_json` / `from_json` / `save_json` / `load_json`)
  and explicit format-version checks.
- **Recorder** ([`AelogRecorder`]) — wraps a [`Timeline`] and mirrors its
  mutation surface, appending a command for every call; the recorder is
  the only way to touch its timeline, so a session cannot silently drift
  from its log. Two identical sessions serialize to byte-equal logs.
- **Replay** ([`replay_events`], [`replay_render`], [`ReplayOutcome`]) —
  `replay_events` reproduces the identical fired-event stream and end
  clock state; `replay_render` additionally feeds blocks to a provided
  Graph 2.0 [`OfflineExecutor`] (applying `SetGain` events sample-
  accurately, exactly as a live driver would) and returns the captured
  audio — byte-identical to the recorded session.
- **CLI** — new `aelog-replay` binary: the guide's `engine replay
  recording.aelog`. Loads a log, re-executes it, and prints the command /
  fired-event counts, end transport state, and (with `--verbose`) every
  command and fired event.
- **Fidelity suite** — `tests/fidelity/aelog_replay.rs` (6 tests): a
  recorded gate session replays to byte-identical golden audio and an
  identical fired stream with the sample-accurate beat-1 gate intact; JSON
  string and file round-trips replay identically; pause/resume + looping
  are recorded faithfully (master only advances while playing, the
  playhead wraps, the trigger fires exactly once); two identical sessions
  produce byte-equal logs; and replay is a pure function of the log.

## [3.28.0]

Timeline and Scheduler: **make time a first-class render
primitive.** A new [`dsp::timeline`] module fuses an `AudioClock` and
event runtime: a deterministic, sample-accurate clock
and event queue that drives the Graph 2.0 `OfflineExecutor` — scheduled
parameter changes land on the exact sample, and the transport owns the
render, not just the events.

### Added

- **AudioClock** (`dsp::timeline::clock`):
  sample position (a looped playhead) + a monotonic master counter,
  tempo, bars/beats/ticks (MIDI 480 PPQ), transport state (Playing /
  Paused / Stopped), a loop region (playhead wraps; events still fire
  once on the master), a linear [`TempoRamp`], time-signature, and
  sample↔beat conversions.
- **TempoMap** (`dsp::timeline::tempo`) — ordered tempo changes with exact
  piecewise-constant beat↔sample integration, so a musical position maps
  correctly across tempo changes.
- **Events** (`dsp::timeline::event`) — [`EventTime`] (Sample or Beat),
  [`EventPayload`] (SetGain typed to a Graph 2.0 node, a Trigger, and an
  opaque Host tag), and once-only sample-accurate firing.
- **Timeline scheduler** ([`Timeline`]) — `schedule_at_sample` /
  `schedule_at_beat`, `advance_block` returning exactly the events whose
  master sample was crossed (each with the in-block index for sample-
  accurate application), note-grid [`Quantize`] snapping, [`TimelineRegion`]
  containment, and mutation-free determinism. Beat events resolve to an
  absolute master sample at schedule time.
- **Renderer hook** — [`OfflineExecutor::set_gain_step`](crate::dsp::graph2::OfflineExecutor::set_gain_step)
  applies a gain change at an arbitrary in-block frame; a timeline event
  firing at master sample `S` lands on `S % block` exactly.
- **Fidelity suite** — `tests/fidelity/timeline_scheduler.rs` (7 tests): a
  Timeline drives the Graph 2.0 executor with a gate scheduled at beat 1
  opening sample-exactly at 24 000; a non-block-aligned gain step lands at
  the exact index; looping wraps the playhead while events fire once;
  pausing halts both clock and render (the transport owns rendering); a
  16th-note grid snaps a beat to the exact sample; a tempo change retimes
  a beat across segments; and timeline regions resolve containment.

## [3.27.0]

Graph 2.0: **make the graph the true center of the
rendering engine** by generalizing the fixed track/bus chain of `dsp::graph`
into an *arbitrary topology* runtime. A new [`dsp::graph2`] module is a
model of explicit structure: nodes declare **input/output ports** with
typed-bus metadata (signal class + channel count), every connection is a
**first-class edge**, and the topology — not an authored chain — defines the
signal flow. Validation, cycle detection, deterministic topological
scheduling, dynamic recompilation, inspection/serialization, and an offline
executor that renders any built topology are all included.

### Added

- **General-purpose topology** (`dsp::graph2`) — [`Graph2`]: a builder
  (`add_source` / `add_gain` / `add_delay` / `add_mix` / `add_split` /
  `add_sink`, plus `add_node_raw` for host-defined port shapes), explicit
  edges (`add_edge` fails fast on unknown endpoints, typed-bus mismatch,
  and duplicate fan-in), removal with an ownership rule (a node with
  attached edges cannot be removed), and parameter mutation. Deterministic
  `BTreeMap` iteration makes every artifact reproducible.
- **Typed ports** ([`PortSpec`], [`SignalType`], channel metadata) — an
  edge is only legal when both endpoints agree on signal class; Audio can
  never cross into a Control port. Built-ins carry Audio; Control ports
  are fully modeled and enforced via `add_node_raw`.
- **Validation** ([`ValidationReport`], [`Graph2Error`]) — structural
  errors (unknown node/port, typed-bus mismatch, duplicate fan-in, cycles)
  block compilation; dangling ports are warnings the executor tolerates
  (unconnected inputs read silence, unconnected outputs are dropped).
- **Cycle detection** — grey/white/black DFS reporting the **actual cycle
  path** (`A -> B -> A`) in the error.
- **Topological scheduling** ([`ExecutionOrder`], `topological_order`) —
  deterministic Kahn's algorithm (ascending-id tie-break): identical
  topologies always compile to identical orders, and every node runs after
  its producers. This is the Graph 2.0 analogue of `dsp::graph::plan`.
- **Offline executor** ([`OfflineExecutor`]) — renders a compiled topology
  block by block through the built-in ops (Source / Sink / Gain / Delay /
  Mix / Split). A dry/wet bus is just `Split → {Gain, Delay} → Mix`; the
  executor is offline-first by design, exactly like the acoustic layer.
- **Inspection & serialization** — [`Graph2::to_dot`] renders a Graphviz
  digraph; the whole topology round-trips through serde JSON to an
  identical render.
- **Dynamic recompilation** — every mutation invalidates the compiled
  order; `mutate → compile() again` is the recompilation loop.
- **Fidelity suite** — `tests/fidelity/graph_topology.rs` (8 tests):
  dry/wet diamond renders both branches at exact offsets/gains, three-way
  fan-out sums exactly, cycles rejected with path, structural validation
  (bad ports, duplicate fan-in, typed-bus mismatch, dangling-warning
  semantics), deterministic scheduling, dynamic recompilation changing the
  render, JSON round-trip identity, and a sine source driving a gain graph.

## [3.26.0]

Acoustic baking: **turn expensive acoustic computation into
reusable render data.** The v3.25 `AcousticWorld` solver enumerates every
propagation path (direct, image-source reflections, wedge diffraction, portal
transmission) between a source and a listener — work that, for a *static*
scene, is identical block after block yet was being re-run every frame.
A new [`BakedScene`] is a **position-dependent response cache**: source
positions are quantised to cubes (default 0.5 m) and the full resolved path
set — direction, distance, delay, gain, low-pass corner, path kind, and the
per-band material spectrum where a surface interacted — is stored once per
cell, then looked up by the renderers at audio time with no solving, no
allocation, and no locks.

### Added

- **Baking layer** (`spatial::acoustic::bake`) — [`AcousticBaker`] (control
  path: owns an `AcousticWorld`, bakes a scene's static object positions in
  one call), [`BakedScene`] (the position-keyed cache with an incremental
  `bake` for hosts that accumulate cells), [`BakedObject`] (one resolved
  response per cell) and [`BakedPath`] (a light, `Copy` path record).
  [`BakePolicy`] lets a host retain only the path kinds it actually renders.
- **Renderer consumption** — `BasicPanner`, `VbapRenderer` and
  `BinauralRenderer` each gain `set_baked(Option<BakedScene>)`. When an
  object's position falls in a baked cell, room reflections are placed from
  the cached response via [`BakedScene::listener_images`] (which converts
  cached paths into the renderers' existing `ListenerImage` tap format with
  the same excess-delay convention as `images_for_object`); objects outside
  the bake fall back to the live solve. With no bake attached the renderers
  are bit-identical to v3.25 — the bake is a cache, not a new model.
- **Frequency-domain data survives the bake** — each baked reflection
  carries its full per-band [`MaterialSpectrum`] so offline/reference
  renderers can do true frequency-domain processing instead of the collapsed
  low-pass corner.
- **Fidelity suite** — `tests/fidelity/acoustic_bake.rs` (7 tests):
  baked-vs-live equivalence for all three renderers, position-keyed caching
  (distinct cells distinct, same-cell reuse), live-solve fallback for
  unbaked objects, fabric-wall darkening of the reflection low-pass, and
  deterministic bake+render.

### Changed

- `AcousticWorld::probe_reflection_spectra` exposes the per-reflection
  frequency spectra to the baker (was solver-internal).

## [3.25.0]

Acoustic world simulation: the **first purely simulation-side layer**, built to *separate acoustic
simulation from acoustic rendering*. A new `spatial::acoustic` module turns a
geometric description of a space (walls with frequency-dependent materials,
openings, diffraction edges) into a concrete set of propagation paths that
any renderer — binaural, panner, or an offline baker — consumes. It ships the
**geometry**, **materials**, **portals**, **propagation-path** and
**diffraction** primitives the guide lists, and moves the room from a single
scalar absorption coefficient to per-octave-band spectra.

### Added

- **Frequency-dependent acoustic materials** (`spatial::acoustic::material`)
  — [`MaterialSpectrum`]: per-ISO-octave-band (63 Hz–16 kHz) absorption /
  specular-reflection / transmission spectra, with log-frequency
  interpolation, a `broadband` reduction (geometric-mean gain + −3 dB
  low-pass corner) for the realtime renderers, and the named presets
  Direction 8 calls for ([`MaterialKind`]: Concrete / Wood / Glass / Fabric /
  Carpet / Metal / OpenMesh), each with a documented ISO-class spectrum.
- **Acoustic geometry** (`spatial::acoustic::geometry`) — [`AcousticRoom`]
  (an axis-aligned box with **per-wall** materials — the seam the old
  [`Room::absorption`](crate::spatial::room::Room::absorption) documented),
  [`Portal`] (an opening in a wall coupling two spaces, with its own
  transmissive material), and [`DiffractionEdge`] (a freestanding
  fin/mullion sound bends around), plus the jamb edges of a doorway.
- **Propagation paths** (`spatial::acoustic::path`) — [`AcousticPath`]: the
  simulation→render contract exactly as the guide specifies
  (`kind`, `direction`, `distance`, `delay_samples`, `gain`, `lowpass_hz`,
  `flags`, interacting wall), with [`PathKind`] (Direct / Reflected /
  Diffracted / Transmitted / Diffuse) and [`PathFlags`]
  (spectral-collapse / crosses-boundary metadata).
- **World + path solver** (`spatial::acoustic::solver`) — [`AcousticWorld`]
  owns the geometry and, given a source/listener pair, enumerates the path
  set: the **direct** path, **image-source reflections** (order 1 → 6,
  order 2 → 24, mirroring the renderer's room geometry — the excess-path
  delays are pinned to match), **wedge diffraction** around each portal jamb
  (and any freestanding edge) via the shortest source→edge→listener bend
  with an HF roll-off that grows with the bend angle, and **transmission**
  through each portal filtered by its material. Disabled world = an exact
  single direct path. Deterministic, bounded to `MAX_PATHS`.
- **Separation of concerns** — nothing here runs on the audio thread:
  solving is control/offline-path (heap-happy by design, like correction);
  the realtime renderers consume only the fixed-size resulting paths.

### Tests

- Unit suites in each new module (`material.rs`, `geometry.rs`, `path.rs`,
  `solver.rs`): flat/rising/material-spectrum reduction, per-wall plane
  geometry, portal centres/jambs, direct-path delay, path flags,
  order-1/order-2 reflection enumeration, portal transmission + diffraction,
  and bend-angle geometry.
- Acceptance suite `tests/fidelity/acoustic_world.rs` (8 tests): order-1 box
  → one direct + six reflections with finite physically-placed delays; the
  left-wall reflection's excess-path delay matches the renderer's own
  image-source geometry to half a sample; a fully-open portal transmits
  brightly (gain > 0.5) and diffracts around its jambs; a fabric wall
  low-passes its reflections well below a concrete wall's; the disabled
  world is an exact direct path; a freestanding fin diffracts a path;
  solves are deterministic and capped; and `diffract_around_edge` reports
  the correct 1/r distance + delay.
- `config` and `engine` crates stay in lockstep at 3.31.0.

## [3.24.1]

### Fixed

- **FadeProcessor accumulation precision** — `FadeProcessor::advance` now computes
  the gain value from the exact `samples_processed / total_samples` ratio in `f64`
  rather than accumulating an `f32` increment per sample. This eliminates
  floating-point accumulation drift over long fades (millions of samples).
- **Loudness K-weighting precision** — `KWeightStage1` and `KWeightStage2` in
  `dsp::loudness::meter` now maintain `f64` filter states (`z1`, `z2`), aligning with
  the main biquad engine's precision model and lowering the measurement noise floor.

### Changed

- **Crossfade curve documentation** — Extended `CrossfadeCurve` doc comments to clearly
  specify midpoint energy and power characteristics (constant-power vs −3 dB dip) for
  each curve variant.
- **Loudness hop timing documentation** — Added documentation detailing hop quantization
  at non-standard sample rates and alignment with EBU R128 tolerances.

## [3.24.0]

Completed the DSP seams and added the spatial scene infrastructure layers
listed in the architecture guide (Rules 3–10).

### Added

- **Near-field correction** — `spatial::nearfield`: bounded proximity
  gain (`MAX_GAIN = 2.0`) plus an optional low-shelf LF lift, per-object and
  block-rate smoothed.
- **Applied air absorption** — `AbsorptionState` in `spatial::level`:
  the distance-dependent `AirAbsorption::cutoff_hz` is now actually applied as
  a smoothed biquad low-pass (previously it was computed and discarded).
- **Doppler** — `spatial::doppler`: smoothed, variable-ratio fractional
  resampler driven by radial relative velocity, bounded to stay clear of the
  speed-of-sound, with deterministic re-anchoring for sustained approach.
- **Quality tiers** — `spatial::quality::SpatialQuality` (Low/Medium/
  High/Reference) backing the room-reflection depth on the hybrid paths.
- **Automation** — `spatial::automation`: generic piecewise-linear
  `Curve` (scalar/Vec3/Quaternion) with block-rate and sample-accurate
  evaluation, plus a `SpatialAudioAutomationFrame` per-object override.
- **Voice budget** — `spatial::voice`: `VoiceBudget` / `VoicePriority`
  (Fixed/DistanceWeighted/GainWeighted/UserDefined) scheduler producing a per-
  slot Full/Degraded/Dropped plan.
- **Metering** — `spatial::metering::SpatialMeter`: per-speaker / bus /
  LFE peak + RMS output meters.
- **Diagnostics** — `spatial::diagnostics`: allocation-free scene
  diagnostics + host-rasterizable reflection rays from `EarlyReflections`.
- **HRTF provider** — `spatial::provider::HrtfProvider` trait with a
  normalized-corpus adapter, isolating data import from the runtime renderer.
- **Upmix policies** — `spatial::upmix::UpmixMode` (ForceDownmixStereo /
  MatchOutput / SpatialRender) with deterministic gain policies.
- **Declarative render knobs in `config`** — the new
  spatial quality / voice / metering / automation knobs are now serde fields
  hosts configure in files/JSON: `SpatialConfig` gains `quality`
  (`config::SpatialQuality`), `voice` (`SpatialVoiceConfig`: capacity /
  full-quality capacity / `VoicePriority` policy), and `metering`
  (`SpatialMeterConfig` enable); `SpatialObjectConfig` gains `automation`
  (per-object position/orientation/gain/spread curves via
  `CurveVec3Config` / `CurveQuatConfig` / `CurveScalarConfig`).
  `SpatialNode::apply_config` applies quality + metering to its binaural
  renderer and stores the converted `VoiceBudget`; `SpatialScene::
  from_config`/`to_config` round-trip the automation curves losslessly
  (new `Curve*::keyframes` accessors). Every new field is
  `#[serde(default)]`, so old configs and scene files keep deserializing.
- **Native NetCDF-classic SOFA importer (optional `sofa-import` feature)**
  — `spatial::sofa`: a dependency-free, pure-Rust reader for the NetCDF-3
  classic subset of the AES69 `.sofa` format (the `SOFA_NETCDF3` /
  `SOFA_NETCDF3_CLASSIC` container) that validates the `Conventions` gate and
  reduces `SourcePosition` + `Data.IR` + `Data.SamplingRate` into an
  [`HrtfCorpus`] for the existing `HrtfDataset::from_corpus` pipeline.
  Big/little-endian CDF-1 supported; directions are mapped onto the layer's
  coordinate frame (SOFA CCW-left azimuth → engine CW-right `+X`); modern
  NetCDF-4/HDF5 (`nc4`) files are refused with a typed [`SofaImportError`]
  and the format-specific guidance (documented HDF5 seam). Ships synthetic
  CDF fixtures testing endian round-trips, the L/R stride, the direction
  convention, and rejection of bad magic / non-SOFA conventions /
  truncated data.
- **Spatial knobs exposed through C FFI** —
  `engine_set_spatial_quality` (tier 0–3), `engine_set_spatial_voice`
  (enabled / capacity / full-quality capacity / `VoicePriority` 0–3), and
  `engine_set_spatial_automation` (per-program-object gain/spread keyframe
  arrays, bounded + finite-validated, cleared with 0 points) / `
  engine_set_spatial_automation_time` (scene automation clock) let
  C/C++ hosts configure the spatial master they previously could only
  read (`engine_spatial_info`). Commands are dispatched through
  `EngineCommand` (`SetSpatialQuality` / `SetSpatialVoice` /
  `SetSpatialAutomation` / `SetSpatialAutomationTime`) into the graph's
  `SpatialNode` off the audio thread; automation curves now also apply
  live in the binaural object loop (`set_automation_time` + per-object
  gain/spread overrides applied at block rate).

### Changed

- `BasicPanner`, `VbapRenderer`, and `BinauralRenderer` now run the per-object
  cascade (Doppler → occlusion → air absorption → near-field) before the pan /
  head model; each stage is an exact passthrough when disabled, so the
  conventional paths remain bit-identical.
- Renderers expose `set_quality`, `set_automation_time`, and `meters()`
  accessors; `SpatialAudioObject` gained `doppler`, `near_field`, and
  `air_absorption` runtime knobs whose defaults keep existing scenes unchanged.
- **Voice admission applied end-to-end (`SpatialNode`)** —
  `SpatialNode` now runs its configured voice budget over the scene's
  objects and feeds the per-slot `VoiceAdmission` plan into the renderer's
  object loop. `VoiceBudget::plan_into` is a new allocation-free sibling of
  `plan` (selection-rank ranking into caller buffers) so the budget can run
  on the audio path; the `BinauralRenderer` gained `set_voice_admission` /
  `clear_voice_admission` and honours admission per object (Dropped = silence
  with level-chain smoothing still advancing; Degraded = direct path only,
  no room reflections, point spread).  No budget configured = full admission,
  so conventional paths stay bit-identical.
- **Spatial telemetry in `PlaybackInfo`** — the `SpatialNode`
  now publishes its live per-ear output meters (peak/RMS dBFS) and its
  voice-admission counts (full / degraded / dropped) as
  `PlaybackInfo::spatial` (`Option<SpatialTelemetry>`, always present,
  inactive at rest) on the lock-free snapshot's telemetry cadence, plus a C
  FFI `engine_spatial_info` getter mirroring `engine_correction_info`. Hosts
  read spatial levels / dropped-voice count from the already-atomic
  `ArcSwap<PlaybackInfo>` without touching the audio thread.

## [3.23.0]

Hot-path optimization of the binaural renderer. Per-frame-
constant geometry — the analytic ITD delay (`sin` + Woodworth) and the
room images' ITD — is hoisted out of the per-sample loop and computed once
per block; FIR ring reads drop their per-tap modulo; ring cursors advance
by increment-and-wrap; and `read_delayed` needs one modulo instead of two.
All changes are arithmetic-identical (same samples, same order), so the
bit-exact equivalence suites are untouched — measured on `spatial_bench`
(4 objects, 1024 frames @ 48 kHz): FIR dataset path **2.9×**, analytic
**2.2×**, analytic + room **1.7×**, production graph SpatialNode (512)
**1.9×**; the VBAP path is unchanged (untouched).

### Added

- `benches/spatial_bench.rs` — criterion suites for the binaural FIR /
  analytic / room paths, VBAP on 5.1, and the graph with the SpatialNode
  enabled (regression guards for the optimization wins).

### Changed

- `BinauralRenderer` object loop: analytic ITD delays and room-image ITDs
  precomputed once per block; `azimuth_rad` no longer recomputed per
  frame in the FIR path.
- FIR convolution: descending ring reads with a wrap branch instead of a
  modulo per tap (identical tap order).
- `read_delayed`: one modulo + branch instead of two modulos.

### Fixed

- None.

## [3.22.0]

The active spatial scene now persists across sessions: the engine
auto-saves the graph's spatial state (screen, room, listener, enable) and
restores it at construction, so a host's spatial tuning survives a
restart without any host-side bookkeeping.

### Added

- `engine::spatial_persistence` — control-path auto-save/restore of the
  active spatial scene. `SpatialPersistence` snapshots the `SpatialNode`
  surface into the existing `config::SpatialConfig` serde model, writes it
  atomically (temp + rename, so a crash can never corrupt the last good
  scene), and restores best-effort at engine construction.
- `EngineConfig::spatial_autosave_path` — optional explicit path for the
  auto-save file (default: `<user-data>/engine/spatial_scene.json`);
  hosts that want persistence elsewhere (or disabled) set this.
- Engine lifecycle hooks: `maybe_save` runs each tick after queued graph
  controls are applied and writes only when the state actually changed;
  `Drop` flushes pending controls and persists the final scene, so a
  graceful shutdown always restores exactly what was active.

### Fixed

- None.

## [3.21.0]

Measured HRTF corpus loading: the binaural path can now render real
recorded head-related impulse responses, not just the synthetic grid.

### Added

- `HrtfCorpus` / `HrtfMeasurement` — the SOFA data model (measurement
  directions + per-ear IRs + recorded rate) reduced to pure Rust; the
  exact reduction of a `.sofa` HDF5 export, so hosts can load real
  corpora (CIPIC, TU-Berlin, KEMAR, …) without HDF5 bindings.
- `HrtfDataset::from_corpus` — control-path corpus loading: validates
  finite unit directions and non-finite IRs, resamples every IR to the
  target rate (piecewise-linear), optional peak normalization,
  trims/pads to the FIR tap count, and requires a regular
  azimuth × elevation mesh (full Cartesian product) so bilinear
  interpolation stays exact; irregular meshes are typed errors.
- `save_hrtf_corpus_json` / `load_hrtf_corpus_json` — portable pure-Rust
  JSON interchange of the SOFA data model.
- `HrtfLoadOptions`, `HrtfNormalize`, `HrtfLoadError` — typed load
  controls and errors (empty corpus, bad taps, non-finite direction/IR,
  irregular mesh, JSON failures).

### Fixed

- Clippy `needless_range_loop` / constant-assertion warnings in the
  order-3 ambisonic test suites (`--workspace --all-targets` is now
  warning-free).

## [3.20.0]

The ambisonic layer extends from order 2 to **order 3** (Third-Order
Ambisonics, 16 channels). This is a backward-compatible capability: every
order-≤2 behavior — basis values, decode, and the exact rotation — is
pinned bit-for-bit identical to v3.19.0 by the existing unit and acceptance
tests.

### Added

- **Third-order SN3D basis** (`sh_n`, order 3): the seven ACN-9–15 cubic
  harmonics per the Furse–Malham table — `√(35/8)·x(x²−3y²)`, `√105·xyz`,
  `√(21/8)·y(5z²−1)`, `(√7/2)·z(5z²−3)`, `√(21/8)·x(5z²−1)`,
  `(√105/2)·z(x²−y²)`, `√(35/8)·y(y²−3x²)` — each validated to unit sphere
  mean-square (SN3D) and mutual orthogonality by a dedicated grid test. New
  `AMBISONIC_CHANNELS_ORDER_3 = 16` and `AMBISONIC_CHANNELS_MAX` channel
  constants.
- **Exact order-3 Wigner rotation** (`rotate_bus_frame_n`): the 7×7 block
  is the projection of the cubic triple-Kronecker action `R⊗R⊗R` onto the
  order-3 subspace, computed by coefficient linear algebra (monomial
  substitution under `R` + Gram projection), in f64 — so the defining
  property `sh_n(3, R·v) == W₃·sh_n(3, v)` holds to high precision on every
  channel. `AmbisonicRenderer` / `AmbisonicDecoder::with_order` now accept
  order 3; `process_bus` and the renderer scratch buffers grow to
  `AMBISONIC_CHANNELS_MAX` (still fully preallocated, zero allocations).
- **Third-order max-rE window**: the published Zotter–Frank weights
  `a1 ≈ 0.7660, a2 ≈ 0.6534, a3 ≈ 0.5715`, verifiable per-speaker against
  the closed-form decode.

### Tests

- Unit suite additions: `order3_basis_matches_documented_sn3d_convention`
  (cardinal-direction values + order-1/2 rows unchanged), `
  order3_norm_preserving_on_the_sphere` (mean-square 1 + orthogonality),
  `order3_rotation_is_exact_on_the_basis` and `order3_rotation_round_trips_
  to_identity`, and the rejection test now checks order 4 is unsupported.
  Total 137 spatial lib tests.
- Acceptance suite `spatial_hoa` gains three order-3 tests (conventions,
  exact rotation + world-fixed end-to-end decode, and the max-rE window vs
  the closed form + max-rE-over-basic rear-lobe narrowing). 9 tests green.
- Three implementation defects were caught while extending:
  - A test-side arithmetic error asserted `Y₃⁰(+Y)=√7` (really 0; `z=0` at
    +Y) and `Y₃¹(+Y)=−√(21/8)` (really ACN 11); corrected to pin `Y₃³(+Y)`
    and `Y₃⁻¹(+Y)`.
  - The order-3 rotate arm initially accumulated `W₃ᵀ·c` (transposed);
    corrected to the direct `W₃·c` convention (order-2's block is stored
    transposed, order-3's is direct — now documented in the code).
  - A flawed acceptance assumption (order-3 max-rE rear lobe narrower than
    order-2's) was wrong; replaced with the per-order-3 meaningful invariant
    (max-rE vs basic at order 3).

## [3.19.0]

The spatial layer's capstone: **higher-order ambisonics**, a **SpatialNode**
in the production DSP graph, **measured spectral HRTFs** with elevation
cues, and the **scene-file format**. Together they close the remaining
spatial audio enhancements: order-2 SH rendering, the spatial master
output stage as a first-class graph node, full head-related
impulse responses replacing the analytic shelf, and Serde
scene persistence.

### Added

- **Higher-order ambisonics** — `ambisonic.rs` is
  rewritten around the exact order-N SH basis (`sh_n`, `channel_count`):
  order-1 (FOA) behavior is pinned bit-for-bit identical to the previous
  release, and order-2 adds the 5 new channels (`U`, `V`, `T`, `R`, `S`)
  per the published Furse–Malham table. The decoder weights are the
  published max-rE window (order-2 `a1 ≈ 0.9057`, `a2 ≈ 0.6827`; order-1
  stays `√3/2`), `AmbisonicRenderer::with_order` renders any supported
  order to any speaker layout, and the bus rotation is the **exact**
  order-2 rotation (WXYZ interleaved with UV) — a 90° yaw moves a plane
  wave to the correct column, pinned by a dedicated test.
- **SpatialNode in the production graph** — a new `SpatialNode`
  plan node: a stereo master is spatialized through the binaural head model
  (optionally with the room); multichannel masters pass through untouched
  (documented seam). It ships with a full control surface
  (`set_spatial_enabled/screen/room/listener`), a config section
  (`SpatialConfig` + `SpatialRoomConfig` in `engine_config`), a per-node
  atomic control mirror applied at block boundaries, and live enable
  surviving generation rebuilds (swap replay). Zero allocation on the
  audio thread, pinned by `realtime_allocation`.
- **Measured spectral HRTFs** — `HrtfDataset`:
  a grid of per-ear impulse responses (azimuth × elevation) with bilinear
  interpolation (azimuth wrapped continuously across the 360° seam),
  loaded on the control path and validated (`from_planes` rejects
  non-monotonic grids and non-finite IRs). `BinauralRenderer::use_dataset`
  switches object direct paths from the analytic chain to FIR convolution
  of the interpolated IR (which carries both ITD and spectral cues); the
  analytic path gains `ElevationNotch`, a documented pinna-notch biquad
  (`f = 6 kHz + 4 kHz·sin(el)`, depth `−8 dB·|sin(el)|`, an exact
  passthrough at 0° elevation). A synthetic dataset generator discretizes
  the analytic model on a regular grid so the FIR path is testable without
  shipping a measured corpus.
- **Scene file format** — `config::
  SpatialSceneConfig` (+ `SceneListenerConfig`, `SpatialObjectConfig`,
  `SpatialBedConfig`, `SpatialFieldConfig`): a Serde-serializable,
  renderer-independent scene model (listener, objects, beds by semantic
  role names, fields, room). `SpatialScene::from_config` / `to_config`
  convert losslessly (listener orientation stays the canonical quaternion —
  no Euler drift), `save_scene_json` / `load_scene_json` are the file I/O
  with typed errors, and `validate` enforces the engine's caps with rich
  messages before anything reaches the audio thread. Optional fields
  default (`#[serde(default)]`) so older hosts keep reading newer files.

### Fixed

- **Woodworth ITD folding for wrapped azimuths** — `woodworth_itd_sec`
  folded angles past ±π with `rem_euclid(...).min(π)`, mapping 300°
  (physically −60°) to 180° (zero ITD), and `ear_delay_sec`'s left/right
  side test used `signum(azimuth)`, which is wrong for azimuths wrapped
  past ±π. Both now fold by reflection / use `sin(azimuth)` so a 0–360°
  grid (the dataset convention) is rendered correctly; the renderer's
  signed azimuths were unaffected. Pinned by new unit tests and by the
  dataset-path mirror-symmetry acceptance test.

### Tests

- Unit suites: `ambisonic.rs` (order-2 basis orthonormality, channel
  table, max-rE weights, the exact rotation property, order-2 renderer),
  `hrtf.rs` (dataset structure, bilinear exactness/wrap, synthetic IR
  pins, notch passthrough/elevation), `binaural.rs` (dataset path renders
  the IR exactly, mirror symmetry in both paths), `scene.rs` (config
  round-trip, JSON IO, validation), plus SpatialNode graph tests.
- Four acceptance suites: `tests/fidelity/spatial_hoa.rs` (order-2
  rendering to 7.1.4, rotation, per-order weights), `spatial_node.rs`
  (bit-exact passthrough when disabled, binaural ITD on the graph output,
  room tail, listener yaw, control surface, reconfig survival, MC seam),
  `spatial_hrtf_ir.rs` (dataset IR fidelity, mirror symmetry, elevation
  notch, determinism), and `spatial_scene.rs` (lossless round-trip renders
  bit-identical, forward-compatible defaults, validation rejections,
  renderer independence, quaternion fidelity).
- `realtime_allocation`: three new zero-alloc tests — order-2 ambisonics
  with per-frame exact rotation, the SpatialNode with a room under a
  block-rate listener sweep, and the HRTF dataset path with the worst-case
  order-2 room. 18/18 tests, **zero allocations**.

## [3.18.0]

Head tracking: the VR/AR seam. A new
`spatial::tracking` module turns a stream of timestamped orientation
samples (an IMU, a webcam, a game engine's VR rig — anything that can
produce a `Quat`) into a smooth current head orientation the host applies
to the scene listener before each render block. The renderers never
change: the listener's orientation was already a first-class transform,
so tracking is purely a control-side interpolation + smoothing problem.
The `HeadTracker` shortest-path nlerps across the last two samples, feeds
an exponential (one-pole) filter on the orientation error (τ in ms; `0`
snaps exactly), and can rate-limit the angular step (deg/s) so a violent
head jump or sensor glitch cannot fling the soundfield.

### Added

- **`spatial::tracking`** — [`HeadTracker`], [`HeadSample`] (timestamped
  orientation), [`TrackingConfig`] (`smoothing_ms`, `max_angular_rate_deg_s`):
  `push` ingests samples (host thread), `sample(now)` returns the smoothed
  current orientation, `apply_to(&mut listener, now)` is the per-block host
  convenience. Pure fixed-size state — allocation-free and lock-free, so it
  can run on the audio thread's caller.
- **Quaternion interpolation** (`math`): `Quat::nlerp` (shortest-path,
  normalized linear interpolation), `Quat::angle_to`, `Quat::dot`,
  `Quat::normalized`, `Quat::negated`, `Quat::length`, `Quat::is_finite`,
  and `Add` / `Mul<f32>` — all unit-tested (midpoint pins, shortest-arc
  wrap, `q` vs `−q`).

### Tests

- Unit suites in `tracking.rs` (segment interpolation vs the closed form,
  shortest arc, smoothing ramp + convergence, exact mode, rate-limit cap,
  reset/first-sample snap, `apply_to`) and `math.rs` (nlerp endpoints /
  midpoint / shortest arc, `angle_to` bounds, scalar ops).
- Acceptance suite `tests/fidelity/spatial_tracking.rs` (6 tests): the
  headline Woodworth consistency — a 137° tracked yaw sweep renders a
  world-fixed source with the closed-form `itd(az, L) − itd(az, R)` ear lag
  at every block; the frozen-image contrast (same sweep without applying
  the tracker); smoothing gliding the image without zipper; a 5.1 panner
  moving the image from the side pair to the front/center as the head turns;
  tracker determinism + rate-limit capping end to end; and `apply_to`
  updating the listener (plus the renderer using it, pinned via the ITD).
- `realtime_allocation`: new `realtime_head_tracker_does_not_allocate` — a
  10k-sample jittery stream with block-rate sampling, **zero allocations**.

## [3.17.0]

Binaural rendering:
the spatial layer gains a **head model** — a `BinauralRenderer` that
renders the entire hybrid scene (objects, beds, fields, and the room's
reflections) to two ears using the documented Woodworth interaural time
difference and a Duda-Martens head-shadow shelf, with no speaker array.
A new `spatial::hrtf` module ships the open model (ITD formula, `α`
coefficient, a first-order shelf filter, and the fractional-delay ring read
that makes ITD changes glide); `spatial::binaural` assembles the renderer:
objects through the shared level chain then per-ear delay + shadow (spread
blurs the interaural cues instead of moving the image), beds folded by
semantic role (LFE at `1/√2`), and diffuse content (fields + the room's
late field) decoded onto a virtual 8-speaker ring before the head model —
surrounding ambience, not a phantom. `RendererKind::Binaural` joins
`Basic` / `Vbap` / `Ambisonic`; `prepare` requires exactly two enabled
non-LFE speakers (stereo/headphone layouts).

### Added

- **`spatial::hrtf`** — `woodworth_itd_sec` (front 0 → ear-axis maximum
  `(a/c)(π/2+1)` → rear 0, the documented front/back cone ambiguity),
  `head_shadow_alpha` (`1.05 + 0.95·sinφ`: ≈ 2.0 at the ear, ≈ 0.1
  shadowed), `Ear`, the `HeadShadow` first-order shelf (DC gain exactly 1,
  HF asymptote exactly α, one-pole-smoothed α), and the fractional
  `read_delayed` ring read — all pinned by unit tests.
- **`spatial::binaural::BinauralRenderer`** — full hybrid rendering to a
  stereo interleaved buffer: the object level chain (distance ·
  directivity · occlusion) then per-(direction, ear) Woodworth delay +
  shadow shelf; angular-region spread renders every sampled direction with
  its own cues; the LFE send and bed-LFE role fold at `1/√2`; beds route
  by semantic-role azimuth; fields and the room's late field decode onto a
  virtual 8-speaker ring (`√N`-compensated, decorrelated) and are
  head-modeled per virtual speaker; room image sources are binauralized at
  `excess_path + ITD(ear)` with per-(image, ear) shadow and smoothed tap
  gain.
- **`RendererKind::Binaural`** — non-exhaustive enum extension; the
  previously-declared `RenderError::HrtfUnavailable` seam is now real.
- **`EarlyReflections` binaural primitives** — `cursor_at`, `store`,
  `read_delayed` (fractional), `add_send` for the renderer's own head-model
  taps.

### Tests

- Unit suites in `hrtf.rs` (Woodworth closed form, per-ear delay rules, α
  complement, shelf DC/Nyquist/α=1 pins, fractional interpolation) and
  `binaural.rs` (stereo-layout enforcement, front unity, exact ear swap
  under mirror symmetry, LFE fold, spread's effective-ITD shrinkage).
- Acceptance suite `tests/fidelity/spatial_binaural.rs` (11 tests): the
  Woodworth closed form at the public API, front-center unity and balance,
  hard-right ITD + head shadow measured on impulse argmax, mirror
  symmetry, bed role/LFE folds, diffuse equal-ear-energy fields with
  decorrelation, the 280-sample room reflection arriving one ITD later at
  the contralateral ear, listener-rotation image motion, spread's effective
  ITD reduction, deterministic finite full-hybrid rendering, and layout
  validation.
- `realtime_allocation`: new `realtime_spatial_binaural_does_not_allocate`
  — order-2 room, occluded/directional/spread/LFE objects, a bed, a field,
  and a sweeping listener yaw; 10k blocks, **zero allocations**.

## [3.16.0]

Room acoustics: the spatial scene gains an acoustic
space. A `spatial::room::Room` (axis-aligned box in world space) turns
participating objects into small acoustic events: **early reflections** via
the image-source method (each mirrored image is a virtual source with its
own pan solve, distance attenuation, reflection-coefficient amplitude, and
excess-path delay through a per-object ring) and a **late field** — a
Schroeder tail shaped by the room's RT60 that encodes into the ambisonic
bus and decodes as a diffuse, decorrelated source.
The room is opt-in at both levels: `Room::default()` is disabled (renders
stay bit-identical) and an object participates only through its `room_send`
— the seam the scene model declared. Occlusion's `AcousticTransmission` is
the transmission seam: the same low-passed sample that feeds the
direct path feeds the reflections.

### Added

- **`Room` model** (`spatial::room`): dimensions (width/depth/height),
  wall absorption (one coefficient; per-wall is a documented seam),
  early-reflection order (1 = 6 images, 2 = 24 distinct), late RT60, and
  late wet mix. Added to `SpatialScene`; `Room::default()` is disabled.
- **Image-source geometry** (`image_sources`): breadth-first reflection of
  the source across the six walls with deduplication — order 1 → 6 images,
  order 2 → 24 (two crossings on one axis, or one on each of two). Each
  image carries the product of the crossed walls' reflection coefficients
  (`1 − absorption`). Pure arithmetic, unit-tested against closed forms.
- **Early reflections** (`EarlyReflections`, renderer-owned): per-object
  delay rings (≈171 ms @ 48 kHz, preallocated), a per-(object, image,
  speaker) smoothed tap-gain matrix, and a room-send accumulator. The
  renderers solve each image with their own pan machinery (equal-power
  pairs for `BasicPanner`, full 3-triplet VBAP for `VbapRenderer`), so
  reflections obey the same geometry as the direct path; delays are the
  excess path `(dist_image − dist_direct)/c`, clamped to the ring.
- **Late field** (`RoomLateField`): a Schroeder tail — 4 parallel feedback
  combs whose gains are derived exactly from `rt60_ms` (verified by
  measuring the output decay), 2 serial allpasses for density, `(1 − g)`
  per-comb normalization so the tail stays bounded — whose output is fed
  through `AmbisonicFieldMixer::render_extra`: encode into the ambisonic
  bus (`W` only) → decode with the `√N` diffuse compensation → per-speaker
  decorrelation. LFE never receives room energy.
- **Realtime**: image enumeration is pure arithmetic into fixed stack
  arrays; rings/tap matrix/tail buffers are preallocated — new
  `realtime_allocation` test running the order-2 worst case (24 images per
  object, occluded + directional objects, late field at 0.7 wet) over
  10k blocks, **0 allocations**.
- **Public surface**: `Room`, `EarlyReflections`, `RoomLateField`,
  `image_sources`, `ReflectionImage`, `ListenerImage` exported from
  `spatial`; `Room` in the `prelude`. Objects already carried the
  `room_send` participation seam.
- **Acceptance suite**: `tests/fidelity/spatial_room.rs` — the predicted
  280-sample reflection arrival (excess path 2 m ÷ 343 m/s) on both
  renderers, coefficient-exact tap amplitudes, absorption monotonicity,
  order-2 energy growth, room-disabled bit-exact restoration (no state
  pollution), dry-object bit-exactness, diffuse LFE-free late field over a
  12-block run, `late_mix` monotonicity (0 removes the late field), and
  deterministic finite hybrid rendering.

### Fixed

- n/a

### Changed

- `AmbisonicFieldMixer` gains `render_extra` for derived diffuse planes
  (the encode/decode/decorrelation loop is shared with `render`).
- `engine` and `config` versions remain synchronized at `3.16.0`.

## [3.15.0]

Ambisonics / First-Order Ambisonics: the engine's
sound-field representation. A `spatial::ambisonic` module brings a
documented FOA core — ACN ordering `[W, Y, Z, X]`, SN3D normalization, real
spherical-harmonic basis, plane-wave encoder, order-1 bus rotation, and a
sampling ("basic") decoder with a max-rE policy — plus a standalone
`AmbisonicRenderer` that decodes a 4-plane FOA bus onto *any* speaker
layout, so the same encoded bus renders to stereo, 5.1, 7.1.4, or a custom
array without re-authoring. The diffuse-field path now genuinely rides the
pipeline: `AmbisonicFieldMixer` encodes fields into a per-block FOA bus
(perfectly diffuse → `W` only), decodes through the real matrix, and
keeps the per-speaker decorrelation delays — the `√N` diffuse compensation
restores unit energy for diffuse content.

### Added

- **FOA math core** (`spatial::ambisonic`): `sh_foa` real-SH basis and
  `encode_plane_wave` (defensive normalization; a zero direction encodes
  silence, never NaN), `rotate_bus_frame` (order-1 rotation: `W` invariant,
  `X Y Z` rotate like direction vectors), `channel_count` / `AMBISONIC_ORDER`
  so higher orders are a table + rotation extension.
- **Decoder** (`AmbisonicDecoder`): `DecoderPolicy::Basic` — the sampling
  decoder `D = Y(S)ᵀ/N`, so a plane wave from `d` lands on speaker `s` as
  `(1 + 3·cosθ)/N` — and `DecoderPolicy::MaxRe` (documented FOA `a1 = √3/2`
  lobe narrowing). `prepare` builds the per-speaker matrix and rejects
  empty / LFE-only layouts; `process_bus` and the per-frame `decode_frame`
  are allocation-free.
- **Standalone `AmbisonicRenderer`**: decodes an interleaved `[W, Y,
  Z, X]` bus (four planes via `process_block`) into the active layout,
  applying the listener's orientation per frame (so a world-encoded field
  stays world-fixed as the listener turns) and per-speaker
  calibration. Registered as `RendererKind::Ambisonic`.
- **Diffuse-field upgrade** (`AmbisonicFieldMixer`, replaces
  `DiffuseFieldMixer`): the field path now goes field → encoder → FOA bus →
  decoder → per-speaker decorrelation rings. `W` is boosted by `√N` (the
  documented diffuse compensation) so a diffuse field decodes at unit
  energy with the equal-power `1/√N` spread, exactly preserving the
  previous baseline behavior; LFE never receives field energy.
- **Realtime**: bus encode/decode and listener rotation run on preallocated
  scratch — new `realtime_allocation` test (10k blocks, rotating listener,
  `MaxRe`, 7.1.4, 0 allocs); the field mixer's bus path is exercised by the
  existing hybrid zero-alloc test.
- **Public surface**: `AmbisonicDecoder`, `AmbisonicRenderer`,
  `DecoderPolicy`, `sh_foa`, `encode_plane_wave`, `rotate_bus_frame`
  exported from `spatial` and the `prelude`; `RendererKind::Ambisonic`.
- **Acceptance suite**: `tests/fidelity/spatial_ambisonic.rs` — SN3D/ACN
  convention pins, plane-wave round-trip to stereo / 5.1 / 7.1.4 from one
  bus (speaker independence), max-rE lobe narrowing, world-fixed listener
  rotation, W-only equal-power decode with silent LFE, a 720-step rotation
  continuity sweep, and determinism / unprepared-use rejection.

### Fixed

- n/a

### Changed

- `DiffuseFieldMixer` → `AmbisonicFieldMixer`; `prepare` now returns
  `Result` (a degenerate layout surfaces as an error instead of silently
  disabling fields). Field behavior is unchanged (equal power, distinct
  deterministic delays, silent LFE, unit energy).
- `engine` and `config` versions remain synchronized at `3.15.0`.

## [3.14.0]

Hybrid beds & fields: the spatial scene's second and
third content classes. A `SpatialScene` now carries **beds** (channel-based
content that already has a spatial structure — 5.1 music, a 7.1 effects bed)
and **fields** (positionless diffuse environments — rain, ambience, crowds)
alongside objects, and both renderers mix all three through one
`process_hybrid_block` into a single interleaved buffer (spatial mixer). The object-only `process_block` and the trait's default hybrid
behavior keep every existing caller working unchanged.

### Added

- **Beds** (`spatial::bed`): [`SpatialBed`] authored with a semantic
  [`ChannelLayout`](crate::decode::ChannelLayout) (roles cached at
  construction, control path) and routed by **semantic role** onto the
  matching output speaker — never by numeric position — with calibration
  trim and authored LFE included; channels with no matching output speaker
  drop cleanly (full BS.775 rematrixing remains the conventional PCM path's
  job). Bounded [`SpatialBedStore`] with stable [`BedId`]s (≤ 16).
- **Fields** (`spatial::field`): [`SpatialField`] — a diffuse source spread
  with equal power (`1/√N`) across every pan speaker and **decorrelated per
  speaker** through a deterministic delay line (2.0–10.25 ms, all distinct
  for ≤ 12 speakers), so it reads as surrounding ambience rather than a
  phantom image. LFE never receives field energy. Bounded
  [`SpatialFieldStore`] with stable [`FieldId`]s (≤ 16).
- **Hybrid mixer**: `SpatialRenderer::process_hybrid_block` takes a
  [`HybridBlockInputs`] struct (object planes + bed-major bed planes + field
  planes) and sums objects → beds → fields into the caller's interleaved
  buffer; the trait default forwards to the object path so third-party
  renderers keep working, and `process_block` now delegates to the hybrid
  path with empty bed/field planes. Both `BasicPanner` and `VbapRenderer`
  implement the full hybrid path.
- **Realtime**: bed routing is a role-table scan; the field mixer reads/writes
  preallocated per-speaker delay rings with a fixed stack-array plane list —
  `process_hybrid_block` is allocation-free (new `realtime_allocation` test
  running objects + two beds + two fields together, 10k blocks, 0 allocs).
- **Public surface**: `SpatialBed`, `SpatialBedStore`, `SpatialField`,
  `SpatialFieldStore`, `BedId`, `FieldId`, `HybridBlockInputs` exported from
  `spatial` and the `prelude`; scene helpers `create_bed` / `create_field`.
- **Acceptance suite**: `tests/fidelity/spatial_hybrid.rs` — 5.1 bed routing
  by semantic role, 7.1-bed-on-5.1 channel dropping, field equal-power
  spread with distinct decorrelation arrivals and silent LFE, deterministic
  finite objects+beds+fields mixing, missing-plane tolerance, and the
  panner's hybrid mix.

### Fixed

- n/a

### Changed

- `engine` and `config` versions remain synchronized at `3.14.0`.

## [3.13.0]

Object behavior: directivity, occlusion, and a real
angular-region spread model for the spatial layer. Sources can now be
directional (omni / cardioid / supercardioid / arbitrary sampled curve),
occluded (broadband attenuation + a genuine low-pass through the engine's
biquad), and extended (an angular region sampled by a fixed ring of pan
solves instead of the old nearest-speaker widening) — all evaluated in the
renderer's level chain with the existing per-path smoothing, and all
allocation-free on the hot path. The renderer change is backward-compatible:
defaults (omnidirectional, no occlusion, spread 0) render bit-identically to
3.12.0.

### Added

- **Directivity** (`spatial::directivity`): [`Directivity`] enum
  (Omnidirectional / Cardioid / Supercardioid / Custom) evaluated at the
  documented angle — 0 = the source faces the listener, π = facing away —
  via the shared `listener_angle_rad` transform (`q_source⁻¹ ∘ q_listener`),
  so both renderers can never disagree on the convention.
  `CustomDirectivity` is a fixed 91-sample curve (2° steps, linear
  interpolation, clamped) — stack-copied, allocation-free on the render
  path. `SpatialAudioObject` gains `source_orientation` (world-space quat;
  local +Y is the facing) and `directivity`.
- **Occlusion** (`spatial::occlusion`): [`Occlusion`] config (amount +
  max attenuation + min cutoff) mapped to a structured
  [`AcousticTransmission`] (`attenuation_db`, `cutoff_hz`, `diffusion` —
  diffusion is a declared seam) with the cutoff interpolated
  exponentially in log-frequency. Applied as gain plus a real Butterworth
  low-pass (engine biquad, Q = 0.707) with per-object filter state and
  smoothed block-rate cutoff — an occluded source is quieter *and* duller,
  before panning, feeding both the pan paths and the LFE send.
- **Angular-region spread** (`spatial::spread`): replaces the simplified
  nearest-speaker widening in both renderers —
  solve the exact direction (weight `1−s`) plus 3 ring samples at `s × 60°`
  (weight `s/3`), aggregate by speaker, and energy-normalize (constant power
  while the image widens). Fixed sample count (4 solves, 12 entries),
  deterministic, allocation-free.
- **Realtime**: all three behaviors stay inside `process_block` with no
  allocation — new `realtime_allocation` test exercising cardioid + custom
  curves, occlusion filters, and wide spread together (10k blocks, 0 allocs).
- **Public surface**: `Directivity`, `CustomDirectivity`, `Occlusion`,
  `AcousticTransmission` exported from `spatial` and the `prelude`.
- **Acceptance suite**: `tests/fidelity/spatial_object_behavior.rs` —
  cardioid routing (facing vs. away), custom-curve routing, omni-default
  regression guard, occlusion attenuation + low-pass measured on analytic
  sines (10 kHz dies ≫ 100 Hz), monotonicity, bounded transmission,
  spread widening (concentration index) with energy preservation, stereo
  symmetry of a centered spread source, spread-sweep continuity, and
  directivity+occlusion+spread composition.

### Fixed

- **VBAP planar-pair indexing**: the coplanar reduction stored *output*
  channel indices in its azimuth-pair table but indexed `self.pan` with
  them, panning out of bounds on any layout with an LFE gap (5.1, 7.1). The
  pair table now stores pan-slot positions; the bug was latent because
  earlier VBAP tests only used stereo/custom layouts where the indices
  coincided.

### Changed

- The simplified spread (energy blended onto the nearest speaker) is
  replaced by the angular-region model in both `BasicPanner` and
  `VbapRenderer`.
- `engine` and `config` versions remain synchronized at `3.13.0`.

## [3.12.0]

3D VBAP-style object-to-speaker rendering: the spatial
layer's first serious object renderer. A `spatial::vbap::VbapRenderer`
solves an object's listener-space direction as a non-negative combination of
its geometrically surrounding speakers — a full 3-triplet solve on 3D
layouts (7.1.4, custom arrays), a 2D equal-power azimuth-pair reduction for
coplanar layouts (stereo, 5.1, 7.1), and a deterministic nearest-speaker
fallback out of coverage. The triangulation is precomputed at `prepare`
(including the Delaunay empty-triangle filter so no speaker lies on another
triangle's edge and moving objects never snap between region solutions), the
render path is allocation-free, and the front-centre direction in 7.1.4 is
rendered by the real Center speaker — not a phantom stereo pair.

### Added

- **`VbapRenderer`** (`spatial::vbap`): computes panning coefficients from
  actual speaker geometry, with `PanMode` classification
  (`ThreeDim` / `Planar` / `Single`) resolving degenerate and reduced-dimension
  layouts at `prepare` time.
- **Geometry preprocessing**: normalized speaker directions, the
  loudspeaker-basis inverses for every valid triplet, the horizontal
  azimuth-pair ring for planar layouts, and a Delaunay-style empty-triangle
  filter that rejects triangles containing another speaker — eliminating
  knife-edge discontinuities (e.g. the front-centre speaker lying on the
  FL–FR base edge of `{FL, FR, height}`) without a full convex-hull
  library.
- **Coefficient solver**: max-min-gain triplet selection (most-balanced
  enclosing triangle, tightest-norm tie-break), energy normalization
  (constant power across movement), and 3D placement with no off-plane
  `cos(elevation)` hack — distance and level chain identical to the
  `BasicPanner`.
- **Out-of-coverage fallback**: no enclosing triplet (below the
  floor, above the rig) → deterministic direction-preserving nearest-speaker
  fallback; never silent, never NaN, never state-polluting.
- **Realtime discipline**: all triangulation/inverse work happens
  in `prepare`; `process_block` reuses the same per-(object,speaker) one-pole
  smoothing, additive LFE send, and caller-supplied interleaved output as
  the panner — zero allocations in steady state (new `realtime_allocation`
  test).
- **Public surface**: `VbapRenderer` re-exported from `spatial` and the
  `prelude`, `RendererKind::Vbap`, and public `PanMode` introspection.
- **Acceptance suites**: `tests/fidelity/spatial_vbap.rs` — layout
  classification, energy preservation over a sphere sweep, left/right
  symmetry in 3D, overhead-to-height routing, the front-centre regression
  (real Center speaker, no phantom pair), out-of-coverage determinism,
  degenerate coplanar geometry, custom asymmetric 3D layouts, and a
  full-circle continuity sweep.

### Fixed

- The spatial layer's over-complete triangle enumeration could place a
  speaker exactly on another triangle's boundary, so a direction on that
  edge flipped between wildly different coefficient vectors (front-centre
  phantom-pair snap). The empty-triangle filter restores continuous,
  uniquely-tessellated panning regions.

### Changed

- `engine` and `config` versions remain synchronized at `3.12.0`.

## [3.11.0]

Spatial audio foundation: an independent, opt-in spatial
scene layer plus the first renderer. A `crate::spatial` module brings a
speaker-independent object scene model and an equal-power `BasicPanner` that
renders the same scene to any layout — stereo, 5.1, 7.1, 7.1.4, or a custom
array — into a normal interleaved multichannel buffer the existing output
core delivers. The conventional PCM/DSP path is untouched; spatial rendering
is opted into via `ChannelPolicy::SpatialRender`. The spec's multichannel
foundation (semantic channels, N-channel buffers, LFE-as-effects-path already
normalized in prior work) means this release adds the scene/level/render
layer without touching conventional playback.

### Added

- **Spatial math** (`spatial::math`): allocation-free `Vec3` / `Quat` with a
  single documented coordinate system (`+X` right, `+Y` front, `+Z` up;
  azimuth from front toward right; metres / radians / linear gain) — the
  engine stays dependency-light (no `glam`/`nalgebra`).
- **Scene model**: `SpatialScene` (listener + object store), `Listener` and
  the `ListenerTransform` (world-fixed objects move exactly opposite the
  listener's yaw — the head-tracking/VR seam), `SpatialAudioObject` with
  `ObjectAudioRef` (one shareable [`AudioSource`] serving many instances),
  `SpatialObjectStore` (bounded, stable handles), and `SpatialSourceType`
  (Point/Extended/Diffuse/Bed seams).
- **Speaker geometry**: `Speaker`, `SpeakerLayout` with named presets
  (`stereo` / `five_point_one` / `seven_point_one` / `seven_point_one_four`)
  plus arbitrary `custom` arrays, and `LayoutCalibration` (per-speaker level
  trim + time-alignment seams) applied separately from geometry.
- **Level laws**: `DistanceModel` (`Linear` / `Inverse` / `InverseSquare` /
  `InverseReference`) and a bounded `AirAbsorption` HF model.
- **Equal-power `BasicPanner`** (`spatial::panner`): listener-space
  transform, azimuth-bracketing speaker-pair solve with the `la²+lb²=1`
  equal-power law, per-(object,speaker) coefficient smoothing (click-free
  region transitions), additive LFE send (LFE is never a pan target),
  simplified spread, and `cos(elevation)` off-plane attenuation — writing
  into a caller-supplied interleaved buffer so the steady-state hot path
  allocates nothing.
- **Renderer abstraction**: `SpatialRenderer` trait, `RendererKind`, and
  typed `RenderError` (invalid/degenerate geometry surfaces as errors, never
  NaN).
- **Opt-in policy**: `ChannelPolicy::SpatialRender` (config crate) — the
  conventional decode loop is unchanged; hosts drive the renderer
  programmatically via the `prelude` (aspatial types re-exported).
- **Acceptance suites**: `tests/fidelity/spatial_panner.rs` (cardinal
  impulses, symmetry, continuity around a full circle, energy invariant at
  spread 0, listener rotation keeping world-fixed objects stable, distance /
  elevation monotonicity, LFE isolation, calibration trim, custom layouts,
  bounded air absorption) and a `realtime_allocation` test proving
  `BasicPanner::process_block` performs zero allocations in steady state.

### Fixed

- n/a

### Changed

- `engine` and `config` versions remain synchronized at `3.11.0`.

## [3.10.0]

Room/headphone correction pipeline: measurement through
real-time graph application.

### Added

- **ESS measurement kit** (`dsp::correction::sweep`): Farina exponential
  sine-sweep generation, regularized frequency-domain deconvolution,
  sub-sample pre-delay estimation, harmonic-offset reporting, and noise/SNR
  measurement.
- **IR import and conditioning** (`dsp::correction::ir`): pure-Rust WAV
  parsing for PCM and IEEE-float formats, multichannel extraction, rumble
  high-pass filtering, onset/tail conditioning, and peak normalization.
- **Phase machinery** (`dsp::correction::phase`): cepstral minimum-phase,
  excess-phase allpass extraction, linear-phase rendering, hybrid rendering
  (minimum-phase response delayed by two crossover cycles — magnitude
  bit-identical to the min render), and group-delay utilities.
- **Correction derivation** (`dsp::correction::derive`): octave smoothing,
  flat/tilt/shelf targets, SNR-weighted regularized inversion, boost clamps,
  safety normalization, and per-channel rendered correction IRs.
- **Real-time graph integration** (`dsp::graph::nodes::correction_node`): a
  per-channel partitioned-convolution bank with depth control and hot IR
  swaps, wired into the plan post-aux/pre-EQ with live enable toggles,
  sticky state across generation swaps, and latency/bit-perfect reporting.
- **Engine surface**: `EngineCommand` variants and handlers, `EngineHandle`
  methods, correction lifecycle events, and `CorrectionInfo` telemetry in
  the lock-free playback snapshot; C FFI (`engine_set_correction_enabled`,
  `engine_set_correction_depth`, `engine_load_correction_ir`,
  `engine_correction_info`) and `audio-engine-cli` flags.
- Acceptance suites for ESS measurement, phase rendering, correction
  inversion, and the graph end-to-end room-correction pipeline under
  `tests/fidelity/`.

### Fixed

- Added explicit finite-value and parameter validation throughout the
  measurement-to-correction control path.
- Hybrid-phase rendering no longer re-integrates per-bin group delays into
  a blended phase (which wrapped negative-delay content around the IR
  window and rippled the response); it delays the exact minimum-phase IR
  instead, keeping the magnitude bit-identical to the min render at every
  frequency.

### Changed

- `engine` and `config` versions remain synchronized at `3.10.0`.

## [3.9.0]

Endpoint & aux-insert control surfaces, per-endpoint clock drift correction,
and the aux bus promoted to its own graph plan node.

### Added

- **C FFI endpoint routing surface** (`src/ffi.rs`): `engine_upsert_endpoint`
  (device, backend, gain, enabled, drift correction), `engine_remove_endpoint`,
  `engine_clear_endpoints`, `engine_endpoint_count`, `engine_endpoint_id`, and
  `engine_endpoint_info` (rate, gain, pending frames, drift offset) so
  C/C++ hosts can drive the multi-endpoint routing matrix.
- **C FFI aux-insert surface**: `engine_set_aux_insert` and
  `engine_aux_insert_state` expose the aux-bus convolution insert.
  Rust side: `EngineCommand::SetAuxInsert` + `EngineHandle::set_aux_insert`;
  `PlaybackInfo` publishes the live insert state.
- **Per-endpoint clock drift correction** (`EndpointConfig.drift_correction`,
  default on) for rate-mismatched endpoints. Each endpoint's FFT resampler
  stays fixed at the nominal ratio and a rubato `Slip` (a 1:1 clutch that
  inserts/drops single frames behind a short crossfade) trims the stream to
  the device's actual crystal; a proportional ring-fill controller steers
  the slip ratio (clamped ±500 ppm) and converges it onto the real clock.
  `PlaybackInfo.endpoints[]` reports `drift_active` / `drift_ppm` per
  endpoint.
- **Aux bus as a first-class plan node** (`src/dsp/graph/nodes/aux_node.rs`):
  the send accumulator, return, and optional insert move out of `MixBusNode`
  into a standalone `AuxBusNode` that runs as its own `AUX` plan step right
  after the mix step. The mix node and aux node share one interior-mutable
  `AuxSendBus`; the sum loops write each slot's post-fader front-pair signal
  there and the aux node consumes it. Disabled = bit-exact.
- **Per-send gain automation**: each mix slot's aux send is an independent
  ramped gain (10 ms glide on target change, snap on first engagement), so
  a send can be automated without clicks and without disturbing other
  slots' sends.
- **Independent per-send metering**: `ControlHandle::aux_send_peak(slot)`
  reports each slot's own aux-send peak (dBFS), alongside the existing
  aggregate `aux_meters()`, published once per audio block.

### Changed

- **Endpoint path unified**: the superseded `EngineConfig.additional_endpoints`
  / `EndpointTransport` fan-out (a parallel routing matrix that survived an
  earlier merge) was removed in favor of the `output::EndpointWorker` path,
  fixing a double-output risk on stream recovery and stale config references.
  `EngineHandle`'s public endpoint accessors are re-pointed at the unified
  registry. Single-endpoint behavior is unchanged.
- **Drift correction no longer retunes the FFT resampler**: integer-Hz rate
  changes hit rubato's fixed-sync chunk pathology at non-grid rates (e.g.
  47 999 Hz collapses the Fast tier to ~6 800-frame chunks), so the nominal
  ratio is now left fixed and the slip handles the trim.
- **Master (slot 0) meter point**: the plan runner recomputes slot 0's meter
  immediately after the AUX step — the return has landed but the post-mix
  chain has not run — so telemetry includes the aux return (restoring
  pre-split semantics) instead of reporting the post-EQ/dynamics level.

## [3.8.0]

Aux-bus insert seam and bit-exact SIMD pass on the mix bus: the aux bus gains an optional global insert —
a convolution (reverb / cabinet) between the send accumulator and the return
into the master — configured via `AuxBusConfig` (`insert_enabled` /
`insert_wet_mix` / `insert_ir_path`), toggleable at runtime with a control
surface that mirrors the other bus state (a live toggle survives generation
swaps, off always wins over config, and an unloaded/missing IR leaves the
bus bit-exact). The aux return accumulate is now SIMD-accelerated
(SSE2/NEON with a scalar fallback) with a strict element-wise contract:
mul-then-add, no FMA, no reordering — bit-for-bit identical to the scalar
path, enforced by a dedicated bit-exactness test and the existing
graph-vs-pipeline equivalence suite. The f64 (quality) return keeps its
allocation-free scalar loop.

### Added

- `AuxBusConfig.insert_enabled` / `insert_wet_mix` / `insert_ir_path` and the
  `mix.apply_aux_insert(...)` wiring in graph construction (generation-
  carried; a missing IR logs a warning and stays bit-exact).
- `DspGraph::set_aux_insert(enabled, wet_mix)` / `GraphControlHandle`
  `set_aux_insert` + `aux_insert_state()` runtime control surface; the
  toggled state is mirrored on the control bus and replayed across
  generation rebuilds (live snapshots only, matching `set_aux` semantics).
- `dsp_utils::accumulate_scaled` / `accumulate_scaled_f64`: element-wise
  SIMD `dst += src * g` (SSE2 / NEON, scalar fallback) used by the aux
  return; `phase6_bit_exact_simd_accumulate_matches_scalar` locks the
  bit-exact contract across odd lengths and tails.

### Fixed

- The aux return on the f32 hot path now uses the SIMD accumulate; the
  quality (f64) path is unchanged (still allocation-free).

## [3.7.0]

Multi-endpoint routing matrix: the engine can now drive
several output devices simultaneously. The master mix (already output-domain
at the primary rate) is fanned out from the decode loop to every additional
endpoint; each endpoint owns its lock-free ring, a resampler into its own
rate domain, a rate-matched final safety limiter, and a per-endpoint level.
The primary-device path is untouched (single-endpoint mode is bit-identical),
and a failing secondary endpoint is logged and skipped — it can never take
down the primary. Clock drift between independent devices is deliberately
not corrected (each endpoint resamples against its own nominal clock);
drift correction is a documented follow-up.

### Added

- `EngineConfig.additional_endpoints: Vec<EndpointConfig>` (device, backend,
  enabled, per-endpoint gain), re-exported from the `config` crate.
- `EndpointTransport` (`src/engine/endpoints.rs`): per-endpoint ring +
  backend, resampler (master → endpoint rate, `None` when they match),
  endpoint-rate final limiter (applied to resampled frames only), bounded
  pending queue (a stuck endpoint drops oldest frames, never grows memory).
- Fan-out in the decode loop (single-stream bypass, resampled, and
  crossfade flush paths) with partial-write preservation per endpoint.
- Lifecycle: `start()`/`stop()` open/close every endpoint; stream recovery
  reopens them against the new master rate; `set_config` applies changes at
  the next start.
- Telemetry: `PlaybackInfo.endpoints: Vec<EndpointInfo>` (device, rate,
  gain, pending frames) refreshed on the telemetry cadence; engine accessors
  `additional_endpoint_count()` / `additional_endpoint_sample_rates()`.
- Unit tests (resample pitch/peak/finiteness, gain passthrough, partial
  writes, bounded pending) and end-to-end decode-loop fan-out tests with a
  fake 48 kHz endpoint beside a 44.1 kHz master.

## [3.6.1]

Mix-bus hardening: the v3.6.0 mix-bus surface had real defects that could
panic the engine or silently misroute audio. The crossfade flush built its
lane array with one iterator pull too many and panicked whenever a
crossfade ran with lanes registered; lane audio was fed to the graph by
*lane index* while every control command addressed the lane's *slot*, so a
removal-then-readd left audio and controls disagreeing (a lane feeding a
detached slot went silent, gains/ducks hit the wrong stream). The duck
envelope advanced twice per block whenever the bus carried independent
slots (attack/release ran at 2× the configured rate), the `mix_trims` /
`mix_sends` / `aux` config surface was declared but never applied (and its
entry types were not nameable by Rust hosts), commands enqueued before a
reconfig were lost on the fresh generation, and ducking + automation did
not survive generation swaps. The pair slots (0/1) were the only slots
without an aux tap, and the f64 path never published mix meters.

### Fixed

- Crossfade-with-lanes panic: a dedicated `process_block_crossfade_with_lanes`
  entry feeds the incoming stream and lane slots without assembling a
  contiguous array on the hot path.
- Lane placement is slot-addressed end to end: `fill_lane_scratch` fills the
  slot's planes (index `k` ↔ bus slot `k + 2`), and unused slots are zeroed,
  so a lane's audio always reaches its own bus slot after removals/re-adds.
- Duck envelope advances exactly once per block on every path (the
  independent-slot tail no longer ticks it a second time).
- `EngineConfig.mix_trims` / `mix_sends` / `aux` are applied at graph
  construction and `apply_config`; the config crate now re-exports
  `SlotTrimEntry`, `SlotSendConfig`, and `AuxBusConfig` so hosts can name
  them. Construction keeps the config values authoritative (pristine user
  state no longer clobbers them); live reconfigs keep the sticky
  command-applied values.
- `DspGraph::reconfigure` drains queued control commands before snapshotting
  so commands followed by a bus-growing reconfig survive, and carries duck +
  automation tracks across the rebuild.
- The pair slots (0/1) now tap the aux accumulator like every other slot
  (post-fader, pre master-send); slot 0's tap was also missing on the
  multichannel path.
- The public f64 processing path publishes per-slot and aux meters.
- `MixBusNode::is_active` reflects trim/send/automation/aux/duck state, and
  engine `set_config` routes bus-topology changes through the glitch-free
  rebuild instead of an in-place apply.

## [3.6.0]

Per-slot mixer controls and robust multi-endpoint fan-out:
fan-out are now public, configurable, observable, and tested.

### Added

- Serializable endpoint configurations with validated unique IDs, bounded
  gain, enable/disable state, independent channel-agnostic rings, lifecycle
  recovery, and explicit frame-drop telemetry.
- Endpoint transport errors are surfaced through `PlaybackInfo::engine_error`
  and `OutputEvent::EndpointError`; endpoint configuration can be replaced
  through `EngineHandle::set_endpoints`.
- Endpoint routing remains allocation-free in the steady-state engine path,
  including multichannel output scaling.

### Fixed

- Endpoint reconfiguration now rolls back its configuration if reopening
  fails, and endpoint telemetry is reset consistently after replacement.


Mix-bus transition to graph-runtime: the mix bus becomes a
real mixing surface. Per-slot channel trim banks shape each slot's
planes per channel (gain in dB + polarity) between its pre-mix chains and
its sum. Post-fader sends split every slot's contribution between the
master sum (`master_gain`) and an aux-bus accumulator (`aux_gain`),
with `Send` automation tracks modulating the tap sample-accurately. The aux
bus is a first-class stereo accumulator with its own per-block
peak/RMS metering, a duck target id (`AUX_BUS_ID`), a return gain into the
master before the post-mix chain, an aux insert seam, and full
survival across generation swaps via the mirrored `SlotState`/`UserState`.
The engine exposes `SetTrackMasterGain` / `SetTrackSend`, and
`PlaybackInfo::LaneInfo` reports each lane's sends. All additions are
disabled-exact — the 27-scenario graph-vs-pipeline equivalence suite stays
bit-identical.

### Added

- Per-slot `PerChannelTrim` banks: `SetSlotTrim` command, applied on the
  slot's own planes (all-unity = inactive = bit-exact), mirrored/replayed
  through generation swaps.
- Post-fader sends: `SetSend { master_gain, aux_gain }` on every slot; the
  slot's contribution is captured once and scaled into both destinations.
- `AutomationTarget::Send`: a send track shapes the aux tap per frame.
- The aux bus: `SetAux { enabled, return_gain }`, `AuxBus` accumulator in
  `nodes/mix/sends.rs`, per-block aux meters published to the control bus
  (`GraphControlHandle::aux_meters`), `AUX_BUS_ID` duck source, and the
  aux `insert` seam.
- Engine commands `SetTrackMasterGain` / `SetTrackSend`; `LaneInfo` now
  carries `send_master_gain` / `send_aux_gain`.
- Graph tests for trim/send/aux end-to-end; engine lane test covers the
  send-only path (aux tapped, master silent at zero return).

## [3.5.0]

Mix bus musical behavior and multi-track lane registry: the mix bus gains
musical behavior and the engine gains a multi-track lane registry. Per-slot
pan laws and level meters, program-gated ducking, and
sample-accurate automation tracks live entirely in the bus; the engine
now plays N independent lanes on bus slots ≥ 2 alongside the primary stream,
with `AddTrack` / `RemoveTrack` / `SetTrackGain` / `SetTrackPan` /
`DuckTracks` commands and per-lane telemetry in `PlaybackInfo`.

### Added

- **Per-slot pan law**: each `MixInput` gains `pan` (`[-1, 1]`) and a
  `PanLaw` (`Linear` / `EqualPower` / `Center`, default `Linear`); the pan
  pair is folded into the front-L/R gain product so `pan = 0` stays
  bit-exact. `SetPan` / `SetPanLaw` commands; the existing `SetBalance`
  behavior is untouched.
- **Per-slot level meters**: every slot accumulates per-block
  peak/RMS metering (dBFS) and publishes it to the control bus;
  `GraphControlHandle::slot_meters(slot)` reads `(peak_db, rms_db)`.
- **Program-gated ducking**: `DuckState` (source slot, threshold,
  depth, attack/release frames, up to 4 target slots) rides the control
  queue; the audio side evaluates the trigger once per block from the
  source slot's peak meter and ramps the duck gain over attack/release.
  Disabled (`None`) is bit-exact. Exposed as `set_duck` and the engine's
  `DuckTracks` command.
- **Automation tracks**: a slot may carry one immutable track
  (`AutomationTarget::Gain | Pan`) of up to 64 breakpoints; values are
  linearly interpolated sample-accurately on the audio path, with the edge
  values holding. Tracks are replaced wholesale via
  `set_slot_automation` / `clear_slot_automation` and reset per generation.
  A slot with no track is bit-exact.
- **Engine lane registry**: `LaneTrack` (decoder + resampler + bounded
  FIFO) per independent stream on the first free bus slot ≥ 2; the decode
  loop fills each active lane's planes every block and feeds them as
  secondaries (`process_block_lanes` for the single path, lanes riding
  after the incoming stream during crossfades). Commands:
  `AddTrack(Source)`, `RemoveTrack(slot)`, `SetTrackGain { slot, gain }`,
  `SetTrackPan { slot, pan }`, `DuckTracks { … }`. Adding a lane grows the
  bus on demand via the glitch-free generation swap.
- **Lane telemetry**: `PlaybackInfo.lanes: Vec<LaneInfo>` (slot,
  source, gain, pan, active, peak level, position, duration), refreshed on
  the engine's telemetry window from the lane registry and the graph's
  per-slot meters.

### Changed

- `MixBusNode` control commands are large-variant by design (`SetAutomation`
  carries a fixed 64-point breakpoint array); the same `PlaybackStream`
  precedent applies.

### Fixed

- The `AutomationPoint` frame cursor advances monotonically across blocks;
  caller-fed master planes are scaled in place, so tests re-feed fresh
  buffers per block.

## [3.4.0]

Mix bus slot count and multichannel secondary streams: the mix bus slot count
becomes a first-class generation parameter and secondary streams go
N-channel. From here the graph is `EngineConfig::mix_slots` lanes (slots 0/1
are the transition pair, slots ≥ 2 are independent simultaneous streams), and
multichannel output can be fed N simultaneous N-channel `Tracks`.

### Added

- **Slot-count generation parameter** (`EngineConfig::mix_slots`,
  default 2): the mix bus is built with `config.mix_slots` inputs (clamped
  to `MAX_MIX_SLOTS`), so a graph carrying N simultaneous streams is a
  plain generation rebuild ride the publish/swap/retire handshake.
  `MixBusNode::with_slots` constructs an N-slot bus; slots ≥ 2 carry
  `lane_preamp` / `lane_loudness` chains.
- **Per-slot user state** (`SlotState`): gain / balance / mute / active are
  mirrored onto sticky per-slot atomics at drain and replayed into fresh
  generations on `reconfigure`, so a reconfig never snaps a lane's settings
  (gains replay as one-pole targets; slot 0 is never detached).
- **N-channel secondary planes**: `MixInput` planes are now
  channel-major and preallocated to `MAX_CHANNELS × MAX_AUDIO_BLOCK_FRAMES`;
  `DspGraph::process_block_multichannel_streams` feeds each secondary slot
  from an N-channel source (interleaved + channel count) and the multichannel
  mix step (`normal_mc` plan) sums every secondary slot channel-wise into
  the master planes — front L/R shaped by per-slot balance, extra channels
  at per-input gain. Stereo streams feed slots 0/1 as before via
  `process_block_inputs` / `process_block_streams`.
- **Modular split**: `mix_node.rs` (701 lines) is split by concern into
  `nodes/mix/{mod,envelope,sum}.rs`, following the house split-by-concern
  pattern (`dsp/pipeline/`, `dsp/graph/` precedent). The split is pure code
  motion — the equivalence suite pins the bit-exact contract unchanged.

### Fixed

- The background loudness-scan tests' 15 s wall-clock deadline flaked under
  parallel test-suite load; the bound is now a generous 120 s (still fails on
  a genuine hang, tolerates CPU starvation).

### Changed

- `MixInput` field `planes_l` / `planes_r` → channel-major `planes`;
  secondary pre-mix and sums run over every active channel with zero
  allocation (fixed stack array sized to `MAX_CHANNELS`).
- `dsp/graph` module docs updated to reflect the graph as the production hot
  path.

## [3.3.0]

Mix bus and engine migration: the mix bus and the engine
migration. The DSP graph is now the production hot path — the engine drives
`DspGraph` end-to-end (single stream, crossfade, and the new multi-stream
slots), and the decode loop no longer owns DSP. The hardcoded dual-decoder
crossfade hack is replaced by a first-class N-input mix bus whose per-input
chains (preamp, loudness, user gain/balance/mute) sum under a
`TrackMixer`-compatible transition envelope.

### Added

- **Mix bus node** (`dsp::graph::nodes::mix_node`): the graph arena absorbs
  the four global OUT/IN preamp+loudness nodes into a `MixBusNode` whose
  per-input `MixInput` chains carry preamp, EBU R128/ReplayGain loudness,
  user gain (one-pole ramp), balance, and mute. The transition envelope
  (`PlayingCurrent` / `Crossfading` / `Fading` / `PlayingNext` / `Silent`)
  reuses `TrackMixer`'s exact curve math, so a 2-input bus reproduces the
  crossfade path bit-for-bit (pinned by the equivalence suite).
- **Multi-stream entry points**: `DspGraph::process_block_inputs` (primary +
  secondary stream) and `DspGraph::process_block_streams` (primary + N
  slots), plus the `MixInputCmd` / `MixTransitionCmd` control surface
  (`set_input_gain`, `set_input_balance`, `set_input_mute`,
  `set_input_active`, `begin_crossfade`, `begin_fade`, `begin_playing`,
  crossfade curve/duration config). Inactive slots contribute nothing and
  their chains do not advance (secondary stream slots).
- **Engine migration onto the graph**: `AudioEngine` now owns a `DspGraph`
  (the `pipeline()` accessor delegates). The crossfade decode path feeds
  both streams into `process_block_inputs` — per-input pre-mix happens
  inside the bus instead of the decode loop; the single-stream path runs
  through the graph's plan; output profiles, EQ/limiter delegates, telemetry
  reports, sample-rate changes, and filter resets all route through the
  graph. `reset()` now tears the transition envelope down to `Silent`
  (mirroring `DspPipeline::reset`), while `reset_filters_only()` preserves
  an active transition across seeks.

### Fixed

- Stop/track-change now leaves the mixer in `Silent` exactly like the
  pipeline's `reset()` did, so a subsequent `begin_playing` starts from a
  clean envelope.
- The `output_profiles` fidelity test and remaining engine tests that still
  addressed pipeline internals were migrated to the graph node surface.

### Changed

- The graph is now the production hot path (`docs/SIGNAL_FLOW.md` updated);
  `DspPipeline` remains as the reference implementation and the oracle for
  the equivalence suite.

## [3.2.0]

Live graph swap: The graph is
now a host that can be reconfigured underneath itself while playing — control
commands travel through per-node SPSC queues and apply deterministically at
block boundaries, and full configuration changes swap in a freshly built
generation with zero allocation and no locks on the audio thread.

### Added

- **Queued control surface** (`dsp::graph::controls`): every `DspGraph`
  control method now enqueues a plain-data `NodeCmd` (strictly `Copy`, no
  heap) into a bounded per-node SPSC queue instead of mutating nodes
  directly; the audio thread drains all queues once per caller block, so
  commands apply deterministically at the next block boundary and the
  methods are callable as `&self` from any thread holding a
  `GraphControlHandle` (via `DspGraph::control_handle()`).
- **Swappable graph generations** (`dsp::graph::swap`): a `GraphGeneration`
  (node arena + compiled `PlanSet` + stable `NodeId` identities) is an
  immutable, ownable configuration. Build with `GraphGeneration::from_config`
  on the control side and publish via `GraphControlHandle::publish_generation`;
  the audio thread swaps it in at the next block boundary (publish/swap/retire
  handshake with deferred reclamation — the audio thread never allocates or
  frees). Pending generations coalesce ("latest wins") and live memory is
  bounded to 2 generations + ≤1 in flight.
- **`UserState` snapshot** (`dsp::graph::swap`): listener-facing volume /
  balance / speed / fade state is mirrored onto the control bus at each
  drain, so a fresh generation inherits it (`DspGraph::reconfigure` replays
  the snapshot) and a reconfig never snaps the listener's settings.
- **`DspGraph::reconfigure`** (`dsp::graph::construction`): live
  same-thread reconfiguration — build + publish + swap at the next block
  boundary; safe to call while audio is playing.
- **Live-swap gates**: `graph_*` unit tests for the defer/swap/coalesce/
  reclamation discipline and a two-thread control-vs-audio stress test;
  `realtime_graph_swap_does_not_allocate_on_audio_thread` pins the
  zero-allocation swap contract; `graph_live_reconfig` bench group reports
  the per-block cost of a reconfig cadence.

### Fixed

- **1 ms dead weight in the public generation builder**: the live-swap
  `GraphGeneration` build path no longer constructs a throwaway control bus
  (the builder takes a `UserState` snapshot instead), making
  `GraphGeneration::from_config` ~6× cheaper (~0.24 ms in release).

### Changed

- `DspGraph` control methods are now deferred (applied at the next block
  boundary) rather than immediate; `&mut self` callers keep working
  unchanged. Control-queue depth is fixed at 64 commands per node; overflow
  drops and counts (`GraphControlHandle::dropped_commands`).
- `docs/ARCHITECTURE.md` module map updated for the `swap.rs` / `controls.rs`
  split and the queued control surface.

## [3.1.0]

Compiled execution-plan architecture: `DspGraph` gains a compiled
execution-plan architecture, a full symmetric control surface mirroring
`DspPipeline`, and a bit-exact equivalence gate against the pipeline.

### Added

- **`DspGraph` control surface** (`dsp::graph::controls`): symmetric with
  `DspPipeline::controls` — `set_volume` / `set_volume_db` / `set_balance` /
  `set_preamp` / `set_eq_*` / `set_midside_eq` / `set_crossfeed_*` /
  `set_stereo_width` / `set_compressor_*` / `set_limiter_*` /
  `set_loudness_*` / `begin_seek_fadeout` / `begin_seek_fadein` / `cancel_fade`.
- **Compiled execution plans** (`dsp::graph::plan`): stage order is now a
  data-driven `PlanSet` (`Normal` stereo, `NormalMc` multichannel) built at
  construction instead of hardcoded call sequences in `process.rs`; the
  `DspNode` enum dispatch (`GraphNode`) keeps the hot path monomorphized —
  no `Box<dyn DspNode>`.
- **Bit-exact equivalence suite** (`tests/fidelity/graph_pipeline_equivalence`):
  21 scenarios (stereo / f64 / per-frame / max-block / overrun / mid-stream
  control changes / bit-perfect / DoP / loudness / convolution / 5.1 & 7.1
  multichannel / full control-surface coverage) assert `DspGraph` ≡
  `DspPipeline` sample-for-sample via `to_bits`, plus structural parity of
  the node active-set and latency. The pipeline is the frozen oracle and is
  never modified by the suite.
- **Graph plan executor in the realtime-allocation gate**
  (`tests/fidelity/realtime_allocation`): the plan hot path — f32, f64
  quality promotion, and the multichannel `NormalMc` path with channel trim
  — is verified zero-allocation in steady state.
- **`benches/graph_plan_bench.rs`**: Criterion coverage for the plan
  executor (block APIs, quality mode, 6-channel multichannel) plus a
  `graph_vs_pipeline` head-to-head group (measured ≈1.0× at 4096 frames).

### Changed

- **`DspGraph` storage migrated to a node arena**: the 17 previously-`pub`
  typed node fields (`out_preamp`, `eq`, `volume`, …) are replaced by a
  private `Vec<GraphNode>` arena indexed by a `node_id` slot table, with
  typed accessors (`volume()`, `eq_mut()`, …). The graph module is
  explicitly experimental and `DspGraph` has no consumers outside the
  module, so this ships as a minor with the accessor migration documented;
  downstream users should move off direct field access. (An arena-order
  `debug_assert!` contract pins every slot to its declared node kind.)
- **`DspGraph::reset` no longer resets the volume processor** — volume is
  user state (matching `DspPipeline::reset`); previously the graph reset it
  with the filters, which would snap a listener's volume to unity on a
  track change. Pinned by the equivalence suite.

### Fixed

- **Multichannel plan overran the scratch length**: the >2-channel entry
  point built plane views over the full `MAX_AUDIO_BLOCK_FRAMES` scratch
  planes instead of the block's `n` frames, so stateful stages (volume
  ramp, seek fade, loudness, convolution tails) advanced 4096 samples per
  block regardless of block size — outputs matched for one block, then
  diverged. Views are now truncated to `n` (the same `[..n]` discipline the
  stereo path and the pipeline use). Caught by the new equivalence suite.

## [3.0.0]

Release-ready polish pass: bug fixes, new features, and documentation.

### Added

- **Playback queue** (`Playlist`): enqueue / remove / clear / play-index,
  next / previous with history, shuffle (every entry exactly once per
  cycle), repeat modes (Off / All / One), and `PlaylistChanged` events.
  Auto-advances at EndOfStream.
- **Real-time analyzer** (`dsp::analyzer`): lock-free RMS / peak /
  dominant-frequency taps fed from the decode loop and published in every
  `PlaybackInfo` snapshot.
- **WASAPI loopback capture** (`output::wasapi_loopback`, Windows,
  `wasapi-native`): record the system mix to a float32 WAV from the engine
  (`capture start` / `capture stop`, `CaptureStarted` / `CaptureStopped`
  events).
- **Loudness tag write-back** (`tag-write` feature): EBU R128 /
  ReplayGain 2.0 values written into FLAC / MP3 / MP4 / WAV / AIFF / APE /
  WavPack tags via `lofty`, interoperable with Picard / foobar2000.
- **AcoustID fingerprinting** (`fingerprint` feature): bit-identical
  Chromaprint fingerprints via pure-Rust `chromaprint`, plus a
  `fingerprint` CLI command.
- **`replaygain-scanner` CLI binary**: batch EBU R128 / ReplayGain scanning
  with optional `--write` tag write-back.
- **ASIO channel mapping** (Windows): source→output remap applied lock-free
  in the render callback, surviving DSD mode switches.
- **C FFI**: background tick thread inside `engine_create`, URI open, dB
  volume, and playlist control exports.
- **CLI polish**: `env_logger` init, `--backend` / `--device` / `--log-level`
  flags, event-driven `tick_blocking` loop, and `queue`, `levels`, `scan`,
  `fingerprint`, `capture` commands.
- **Docs**: `docs/ARCHITECTURE.md`, `docs/SIGNAL_FLOW.md`, CI workflow
  (fmt / clippy / test matrix across Linux, macOS, Windows, plus a Windows
  cross-target check of the native backends).

### Fixed

- **C FFI `engine_create` never ticked** — commands were queued but never
  processed; a background tick thread now drives the engine.
- **`DacSink` partial-frame alignment**: a frame count not divisible by the
  channel count no longer leaves the ring misaligned.
- Removed the committed `libaudio_io.rlib` build artifact; `*.rlib` is now
  gitignored.
- Cleaned up dead code (`replace_client`, unused builder field) and all
  warnings across default and optional feature builds.

### Changed

- Telemetry counters widened to `u64` (clips / NaNs / underruns /
  overloads).
- CLI tick loop switched from busy-polling to `tick_blocking`.
- Limiter true-peak oversampling decision documented: the detector remains a
  fixed spec-compliant 4× FIR shared by limiter, loudness meter, and
  scanner to keep measurements reproducible.

## [2.1.0]

Prior release. See the git history for details.
