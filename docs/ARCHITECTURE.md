# Architecture

This document describes the overall structure of the engine. For the sample
flow through the DSP chain, see [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md). For
runnable embedding examples (Rust `EngineHandle` + C FFI), see
[`EMBEDDING.md`](EMBEDDING.md). For the phased evolution from the
single-stream player to the multi-stream graph runtime, see
[`EVOLUTION.md`](EVOLUTION.md).

## Module map

```
src/
├── lib.rs                    # Crate root: module wiring + public re-exports
├── commands.rs               # EngineCommand — one-way control protocol
├── events.rs                 # EngineEvent / OutputEvent — discrete lifecycle notifications
├── playback_info.rs          # PlaybackInfo — lock-free telemetry snapshot
├── source.rs                 # AudioSource — File / Uri / Memory abstraction
├── playlist.rs               # Playback queue: shuffle, repeat, history
├── sink.rs                   # SampleSink trait — where processed audio goes
├── audio_io.rs               # Async file/URI I/O helpers (memory-mapped + async)
├── diagnostics.rs            # Typed diagnostics: DiagnosticKind (EngineFault /
│                             #   TrackLoad / Decode / Output / BitPerfect /
│                             #   Configuration) + BitPerfectCause + Diagnostic
├── paths.rs                  # App-data directory resolution (etcetera)
├── dsp_utils.rs              # Small shared DSP helpers
├── buffer.rs                 # The `buffer` module façade: declares the
│                             #   buffer/ submodules, re-exports, shared
│                             #   limits/errors (MAX_AUDIO_BLOCK_FRAMES =
│                             #   4096, MAX_CHANNELS = 16, …)
├── ffi.rs                    # C FFI surface (engine_create/destroy, controls)
├── eval/                     # Phase 2 quality-evaluation harness (see
│                             #   docs/QUALITY.md): versioned reference-vector
│                             #   registry (registry.rs — content-addressed
│                             #   via aelog::cache SHA-256, Expect::Equal /
│                             #   AtMost/AtLeast specs, ReferenceVector
│                             #   {id,version,engine,checks,address});
│                             #   objective measurement primitives (measure.rs
│                             #   — Goertzel amplitude, THD+N, bit-exactness,
│                             #   DTFT IR magnitude/phase); 9 DSP/spatial
│                             #   suites (suites.rs — pipeline bit-exact+THD,
│                             #   parametric-EQ FR+phase, limiter true-peak
│                             #   ceiling, resampler in-band gain, binaural
│                             #   inter-aural level, EBU R128 loudness,
│                             #   convolution vs naive-direct, channel
│                             #   separation, HRTF interpolation convexity);
│                             #   report types + render_text/to_json +
│                             #   cross-version compare (mod.rs — CheckResult /
│                             #   ComponentReport / EvaluationReport /
│                             #   VersionComparison; run_quality() entry)
├── profile/                  # Phase 3 deterministic AudioProfile layer
│                             #   (perceptual analysis, off the audio path):
│                             #   mod.rs — versioned AudioProfile + 7
│                             #   sub-profiles (Loudness/Dynamics/Spectral /
│                             #   Transient/Stereo/Spatial/Content) with
│                             #   documented units/ranges + AnalysisMask
│                             #   (consumers request only what they need) +
│                             #   confidence semantics; analysis.rs —
│                             #   bounded-memory streaming ProfileAnalyzer
│                             #   (BS.1770-4 via the shared LoudnessMeter,
│                             #   Hann-windowed FFT power averaging, onset
│                             #   deltas, running L/R + mid/side stats) +
│                             #   analyze_decoder/analyze_path + cached
│                             #   variants; cache.rs — on-disk ProfileCache
│                             #   (size/mtime + optional content-fingerprint
│                             #   keys, version-validated)
│
├── engine/                   # ── The core state machine ──
│   ├── mod.rs                # AudioEngine struct + public API
│   ├── construction.rs       # Constructors (AudioEngine::new / with_config)
│   ├── tick.rs               # The tick loop: commands, decode, telemetry, capture drain
│   ├── handle.rs             # EngineHandle — thread-safe cloneable client bridge
│   ├── stream.rs             # PlaybackStream — dual-decoder state machine
│   ├── track_loading.rs      # Decoder open/swap, gapless & crossfade handoff
│   ├── crossfade.rs          # Crossfade/gap decision logic
│   ├── clock.rs              # AudioClock — sample-accurate playhead
│   ├── recovery.rs           # Stream recovery (device hotplug, exclusive-mode falls)
│   ├── dsd_state.rs          # DSD transport state (native / DoP / PCM fallback)
│   ├── loudness_state.rs     # Background EBU R128 scan state
│   ├── spatial_persistence.rs # Phase 21: auto-save/restore of the active
│   │                         #   spatial scene (SpatialNode surface →
│   │                         #   SpatialConfig, atomic writes, lifecycle hooks)
│   ├── volume.rs             # Volume control modes (software / hardware)
│   ├── output_setup.rs       # Output backend creation & device selection
│   ├── helpers.rs            # Shared helpers (event emission, playback info writes)
│   ├── telemetry.rs          # EngineTelemetry — PlaybackInfo publication cadence
│   ├── buffers.rs            # EngineScratch — preallocated hot-path buffers
│   ├── decode_loop/          # Decode-and-process hot loop (common.rs +
│   │                         #   single.rs + crossfade.rs + mod.rs)
│   ├── lanes.rs              # Multi-track lane registry (Phase 4 S6): an
│   │                         #   independent decoder+resampler per bus slot
│   │                         #   ≥ 2, fed as secondaries each block
│   └── commands/             # Command handlers, organized by concern
│       ├── mod.rs            # Dispatch table
│       ├── playback.rs       # play / pause / stop / seek / speed / pitch
│       ├── lifecycle.rs      # open / prepare-next / recover / tag write-back
│       ├── playlist.rs       # enqueue / next / previous / shuffle / repeat
│       ├── lanes.rs          # add/remove track, track gain/pan, duck tracks
│       ├── eq.rs             # parametric + graphic EQ, shelves, preamp
│       ├── dsp.rs            # dither, crossfeed, compressor, limiter, bit-perfect
│       ├── output.rs         # backend / device / profiles / volume modes
│       ├── multichannel.rs   # channel mix / trim / routing / LFE / bass mgmt
│       ├── capture.rs        # WASAPI loopback capture start/stop
│       └── correction.rs     # Phase-7 correction: enable/depth/IR load/MeasureRoom
│
├── decode/                   # ── Decoding ──
│   ├── mod.rs                # Format routing, metadata/loudness extractors
│   ├── decoder.rs            # Decoder facade: probe, open, decode blocks
│   ├── codecs.rs             # Codec registry & capability records
│   ├── scanner.rs            # Format scanner (extension + magic probing)
│   ├── channel_layout.rs     # Channel layout descriptors (2.0 → 7.1.4, custom to 16 ch)
│   ├── channel_mix.rs        # Upmix/downmix templates & custom matrices
│   ├── format_descriptors.rs # Requested→actual format downgrade reporting
│   ├── cue.rs                # Cue-sheet parsing
│   ├── metadata.rs           # TrackMetadata — versioned, consolidated track
│   │                         #   model (TrackTags + AudioFormatInfo + loudness
│   │                         #   + optional measured loudness + chapters)
│   ├── loudness_cache.rs     # On-disk loudness scan cache
│   ├── symphonia_decoder/    # Symphonia-backed decoders (FLAC, WAV, AAC, MP3, …)
│   ├── dsd/                  # Native DSD decoders (DSF/DFF) + wire packing/decimation
│   ├── opus.rs               # Ogg Opus (pure-Rust ogg + opus-decoder)
│   ├── tta/                  # True Audio decrypt + decode (pure-Rust)
│   ├── wavpack.rs            # WavPack v5 (pure-Rust wavicle)
│   ├── ape.rs                # Monkey's Audio (pure-Rust ape-decoder)
│   ├── tags.rs               # Loudness tag write-back (`tag-write` feature)
│   └── fingerprint.rs        # Chromaprint/AcoustID (`fingerprint` feature)
│
├── dsp/                      # ── Signal processing ──
│   ├── aelog/                # Deterministic recording & replay (Phase 27,
│   │                         #   v3.29; Phase 30, v3.32: render inputs):
│   │                         #   versioned .aelog render sessions — mod.rs
│   │                         #   (SessionHeader / RecordedCommand —
│   │                         #   timeline mutations + InputAudio chunks
│   │                         #   (clip-addressed v3.35; multi-channel
│   │                         #   channel-major planes v3.36) +
│   │                         #   master-stamped SetListenerPosition / Aelog
│   │                         #   + JSON string/file round-trips), record.rs
│   │                         #   (AelogRecorder — logs every timeline
│   │                         #   mutation + record_audio_input /
│   │                         #   record_clip_audio (Phase 33, v3.35) /
│   │                         #   record_audio_input_channels /
│   │                         #   record_clip_audio_channels (Phase 34,
│   │                         #   v3.36) / record_listener_position /
│   │                         #   record_baked_scene (Phase 35, v3.37),
│   │                         #   replay.rs (replay_events — identical fired
│   │                         #   stream; replay_render — byte-identical
│   │                         #   golden capture against a Graph 2.0
│   │                         #   executor, re-feeding the recorded audio
│   │                         #   tracks, re-attaching baked-scene swaps,
│   │                         #   and driving acoustic nodes from the
│   │                         #   listener trajectory (Phase 36, v3.38);
│   │                         #   ReplayOutcome exposes audio_input +
│   │                         #   clip_tracks (per-clip tracks, Phase 33,
│   │                         #   channel-major v3.36) + listener_motion +
│   │                         #   scene_swaps (Phase 35)). cache.rs
│   │                         #   (Phase 31,
│   │                         #   v3.33: AelogCache — golden captures keyed
│   │                         #   by a deterministic hash (log_hash ×
│   │                         #   graph_fingerprint × sink); v3.42.0 names
│   │                         #   each entry by its **content address** —
│   │                         #   SHA-256 of the canonical render-identity
│   │                         #   JSON — so a synced cache directory is
│   │                         #   valid on any machine, and bounds the dir
│   │                         #   by **LRU eviction** (with_budget, touched
│   │                         #   stamp bumped on each hit);
│   │                         #   lookup/insert/render_cached, atomic
│   │                         #   temp-file writes, corrupt entries degrade
│   │                         #   to misses; log_hash (v3.41.1) covers only
│   │                         #   render-relevant content — sample rate,
│   │                         #   block cadence, commands — so the label
│   │                         #   and format version never split a key and
│   │                         #   re-labelled sessions reuse the golden
│   │                         #   render). CLI: bin/aelog_replay (engine
│   │                         #   replay recording.aelog) gained a --cache
│   │                         #   flag (v3.43.0): with --graph graph.json it
│   │                         #   renders through the content-addressed cache
│   │                         #   and reports cache: HIT/MISS, so repeated
│   │                         #   runs of the same session skip re-rendering
│   ├── pipeline/             # DspPipeline — reference chain; bit-exact
│   │                         #   oracle for the graph equivalence suite
│   │                         #   (mod.rs + controls/process/format/tests)
│   ├── correction/           # Room & headphone correction (Phase 7):
│   │                         #   sweep.rs (ESS measurement + deconvolution),
│   │                         #   ir.rs (WAV import + conditioning), phase.rs
│   │                         #   (min/linear/hybrid rendering), derive.rs
│   │                         #   (smoothed regularized inverse) — all
│   │                         #   control-thread f64 DSP
│   ├── equalizer/            # Parametric EQ (RBJ) + shared types
│   ├── graphic_eq.rs         # Graphic EQ model (10/15/31 ISO bands) → compiled into EQ
│   ├── loudness/             # EBU R128 loudness meter/normalizer
│   ├── resampler/            # Rubato-based sinc resampler
│   ├── resampler_handle.rs   # ResamplerHandle — per-stream resampler facade
│   │                         #   (quality tiers, fallback state)
│   ├── limiter.rs            # Lookahead limiter with true-peak detection
│   ├── true_peak.rs          # Spec-compliant 4× oversampled FIR detector
│   ├── dither.rs             # TPDF dither
│   ├── biquad.rs             # Shared RBJ biquad filters
│   ├── crossfeed.rs          # Bauer / Chu Moy / J. Meier crossfeed
│   ├── multiband_compressor.rs # 3-band multiband compressor
│   ├── convolution.rs        # FFT partitioned convolution engine
│   ├── crossfade.rs          # Track mixer (gapless / crossfade blend)
│   ├── timestretch.rs        # WSOLA time-stretch / pitch-shift
│   ├── gain.rs               # Ramped gain / fade processors
│   ├── stereo.rs             # Mid-side stereo enhancer
│   ├── channel_trim.rs       # Per-channel trim / routing / bass mgmt / LFE
│   ├── autoeq.rs             # AutoEQ preset pipeline
│   ├── device_profile.rs     # Per-device DSP defaults
│   ├── analyzer.rs           # Real-time peak/RMS/spectrum analyzer
│   ├── float.rs              # AudioFloat numeric helpers
│   └── graph/                # Node-based DSP graph (DspNode trait); the
│                             #   production hot path since Phase 3 — split
│                             #   by concern into construction / access /
│                             #   controls / lifecycle / process / limiter /
│                             #   report / plan / swap; nodes/ holds one file
│                             #   per stage (aux, correction, eq, dynamics,
│                             #   convolution, crossfeed, stereo, timestretch,
│                             #   gain/volume, spatial, routing, limiter,
│                             #   dither, resampler, loudness + mix/)
│                             #   — nodes/mix/ (MixBusNode split into
│                             #   mod/envelope/sum: N-slot + N-channel bus
│                             #   with post-fader lane sends), and
│                             #   nodes/aux_node.rs (AuxBusNode + shared
│                             #   AuxSendBus: per-send
│                             #   automation + accumulator + insert + return;
│                             #   Phase 2: per-node
│                             #   SPSC control queues + publish/swap/retire
│                             #   live generation swap; Phase 3: engine
│                             #   drives the graph end-to-end; Phase 4 S1+S2:
│                             #   mix_slots generation parameter + N-channel
│                             #   secondary planes with channel-wise MC sum;
│                             #   S3: pan laws + slot level meters; S4:
│                             #   program-gated ducking; S5: automation
│                             #   tracks; S6: engine lane registry feeding
│                             #   slots ≥ 2 via process_block_lanes;
│                             #   Phase 5: per-slot PerChannelTrim banks
│                             #   and post-fader SlotSend taps; Phase 6:
│                             #   aux promoted to its own AUX plan step with
│                             #   per-send automation + independent
│                             #   metering; Phase 7: nodes/correction_node.rs
│                             #   (CorrectionNode — per-channel partitioned
│                             #   convolution bank, post-aux/pre-EQ);
│                             #   Phase 11: nodes/spatial_node.rs
│                             #   (SpatialNode — binaural master spatial-
│                             #   ization on the front pair, per-node
│                             #   atomic control mirror, live-enable replay
│                             #   on generation swap; MC masters pass
│                             #   through untouched)
│   ├── graph2/               # Graph 2.0 (Phase 25, v3.27): general-purpose
│   │                         #   audio graph topology — nodes with explicit
│   │                         #   typed ports (node.rs: PortSpec/SignalType/
│   │                         #   NodeKind/NodeCapabilities), first-class
│   │                         #   edges (edge.rs), validation + cycle
│   │                         #   detection with cycle-path reporting
│   │                         #   (validate.rs), deterministic topological
│   │                         #   scheduling (sort.rs: Kahn's, ascending-id
│   │                         #   tie-break), builder/query/compile + serde
│   │                         #   round-trip + to_dot inspection (mod.rs),
│   │                         #   and an offline executor rendering any
│   │                         #   topology block-by-block (exec.rs:
│   │                         #   Source/Sink/Gain/Delay/Mix/Split;
│   │                         #   set_gain_step for sample-accurate parameter
│   │                         #   changes; latency.rs (Phase 28, v3.30:
│   │                         #   node_latency taps + LatencyReport upstream
│   │                         #   propagation + compensate — automatic delay
│   │                         #   alignment splicing Delay nodes onto faster
│   │                         #   branches while preserving node ids). Phase
│   │                         #   29 (v3.31): NodeKind::Acoustic renders a
│   │                         #   BakedScene room response from a source
│   │                         #   position (add_acoustic builder;
│   │                         #   OfflineExecutor::set_baked_scene; direct
│   │                         #   pass-through + per-path excess-delay taps;
│   │                         #   zero pipeline latency; Vec3 now serde;
│   │                         #   Phase 36 v3.38: set_listener_position
│   │                         #   overrides the lookup so the replayed
│   │                         #   listener trajectory drives the node;
│   │                         #   Phase 37 v3.39: scene: Option<String> on
│   │                         #   the node selects a named scene from
│   │                         #   set_scene/remove_scene — per-listener
│   │                         #   bakes rendered and mixed in one graph;
│   │                         #   Phase 38 v3.40: per-path spectral filtering
│   │                         #   — each non-direct path is a min-phase FIR
│   │                         #   (material spectrum / diffraction corner)
│   │                         #   convolved against a fixed raw history ring;
│   │                         #   kernels recompile on acoustic_epoch bump
│   │                         #   while the room keeps ringing). Phase
│   │                         #   30 (v3.32): NodeKind::Buffer — audio-input
│   │                         #   source (embedded clip one-shot/looping, or
│   │                         #   OfflineExecutor::set_external_input track).
│   │                         #   Phase 33 (v3.35): NodeParams::Buffer gains
│   │                         #   clip: Option<String> — add_buffer_clip /
│   │                         #   OfflineExecutor::set_external_clip route
│   │                         #   per-clip tracks only to the nodes bearing
│   │                         #   that address (multi-input graphs). Phase
│   │                         #   34 (v3.36): Buffer samples are channel-
│   │                         #   major planes — add_buffer_channels /
│   │                         #   add_buffer_clip_channels expose one mono
│   │                         #   output port per channel (lockstep cursor,
│   │                         #   no upmix); external tracks multi-channel.
│   │                         #   Phase 32 (v3.34): NodeKind::Convolution —
│   │                         #   FIR convolver reporting kernel.len() taps,
│   │                         #   and NodeKind::HRTF — mono-in/stereo-out
│   │                         #   binaural filter reporting the longer
│   │                         #   per-ear IR (both ears share that pipeline
│   │                         #   delay); streaming overlap-add pipeline in
│   │                         #   exec.rs so the delay never drifts — both
│   │                         #   compensate exactly like Delay. Phase 40
│   │                         #   (v3.44.0): Convolution kernels ≥ 512 taps
│   │                         #   render through the realtime
│   │                         #   dsp::convolution partitioned-FFT engine
│   │                         #   (FftConvState — fast long IRs), with an
│   │                         #   extra N−B+1 front delay absorbing the
│   │                         #   engine's partition latency so the reported
│   │                         #   kernel.len() offset and compensation hold;
│   │                         #   short kernels keep the exact direct path.
│   │                         #   Phase 41 (v3.45.0): NodeKind::Resampler —
│   │                         #   mono rate-conversion node reporting
│   │                         #   quality taps (add_resampler /
│   │                         #   add_resampler_with_quality,
│   │                         #   RESAMPLER_DEFAULT_QUALITY=32), the last
│   │                         #   hook the v3.30 latency pass documented:
│   │                         #   node_latency = quality, capabilities.taps,
│   │                         #   compensate aligns it like Delay;
│   │                         #   exec renders a bandlimited windowed-sinc
│   │                         #   interpolator (ratio ≥ 1 onto the fixed
│   │                         #   frame grid) with a quality-zero pipe so
│   │                         #   reported == actual delay. Phase 42
│   │                         #   (v3.46.0): HRTF nodes get a source seam —
│   │                         #   HrtfSource::Inline (classic tabs) or
│   │                         #   Dataset{az,el,taps} reading measured
│   │                         #   per-ear HRIRs from an executor-attached
│   │                         #   HrtfDataset (set_hrtf_dataset;
│   │                         #   add_hrtf_dataset[_with_taps];
│   │                         #   bilinear_interpolate in run_hrtf,
│   │                         #   padded to reported taps) so graph
│   │                         #   binaural branches carry real
│   │                         #   head-related responses and compensate
│   │                         #   like Delay(taps). The
│   │                         #   topology, not an authored chain, defines
│   │                         #   the signal flow — realtime dsp::graph is
│   │                         #   untouched
│   ├── timeline/              # Timeline & scheduler (Phase 26, v3.28):
│   │                         #   clock.rs (AudioClock — playhead + monotonic
│   │                         #   master, transport state, loop region, tempo
│   │                         #   ramp, bars/beats/ticks + conversions),
│   │                         #   tempo.rs (TempoMap — piecewise-constant
│   │                         #   beat↔sample integration across tempo
│   │                         #   changes), event.rs (ScheduledEvent / EventTime
│   │                         #   Sample|Beat / EventPayload SetGain|Trigger|
│   │                         #   Host), automation.rs (CurveBeats — a
│   │                         #   tempo-mapped piecewise-linear control curve
│   │                         #   in beats, evaluate(sample, &TempoMap) for
│   │                         #   musical automation; Phase 39 v3.41),
│   │                         #   mod.rs (Timeline scheduler —
│   │                         #   advance_block fires sample-accurate once-
│   │                         #   events per block, note-grid quantization,
│   │                         #   timeline regions). Drives a compiled Graph
│   │                         #   2.0 graph: the transport owns rendering
│
├── spatial/                  # ── Spatial audio (Phases 8–24, opt-in) ──
│   ├── acoustic/             # Acoustic world simulation + baking (Phases
│   │                         #   23–24, v3.25–v3.26): material.rs
│   │                         #   (per-octave-band MaterialSpectrum
│   │                         #   absorption/reflection/transmission + material
│   │                         #   presets), geometry.rs (AcousticRoom with per-
│   │                         #   wall materials, Portal openings, DiffractionEdge
│   │                         #   fins + doorway jambs), path.rs (AcousticPath /
│   │                         #   PathKind / PathFlags — the sim→render
│   │                         #   contract), solver.rs (AcousticWorld::solve —
│   │                         #   direct + image-source reflections + wedge
│   │                         #   diffraction + portal transmission paths),
│   │                         #   bake.rs (BakedScene position-dependent
│   │                         #   response cache + AcousticBaker; renderers
│   │                         #   consume via set_baked / listener_images —
│   │                         #   cache, not a new model; Phase 35, v3.37:
│   │                         #   deterministic serde — BTreeMap cache as
│   │                         #   ordered entries, −1.0 low-pass-infinity
│   │                         #   sentinel, solver world skipped — for
│   │                         #   aelog scene-swap logs; Phase 38, v3.40:
│   │                         #   spectral_taps(obj, ir_len) renders one
│   │                         #   (excess, min-phase FIR kernel) per
│   │                         #   non-direct path — material spectrum or
│   │                         #   diffraction corner → FIR via the correction
│   │                         #   magnitude→IR synthesizer; flat → single-tap;
│   │                         #   Phase 42, v3.48: AirAbsorption model shapes
│   │                         #   kernels per path distance when enabled)

│   ├── math.rs               # Vec3 / Quat + the single documented coordinate
│   │                         #   system (+X right, +Y front, +Z up; metres /
│   │                         #   radians / linear gain) — no linear-algebra dep
│   ├── scene.rs              # SpatialScene (listener + object store),
│   │                         #   Listener, ListenerTransform (world-fixed
│   │                         #   objects move opposite the listener yaw)
│   ├── object.rs             # SpatialAudioObject, ObjectAudioRef (shareable
│   │                         #   AudioSource), SpatialObjectStore (bounded /
│   │                         #   stable handles), SpatialSourceType
│   ├── speaker.rs            # Speaker, SpeakerLayout (stereo / 5.1 / 7.1 /
│   │                         #   7.1.4 / custom), LayoutCalibration
│   ├── level.rs              # DistanceModel (Linear/Inverse/InverseSquare/
│   │                         #   InverseReference), AirAbsorption
│   ├── directivity.rs        # Directivity (omni/cardioid/supercardioid/
│   │                         #   custom 2° curve) + the shared listener-angle
│   │                         #   transform (source orientation → curve)
│   ├── occlusion.rs          # Occlusion → AcousticTransmission (attenuation +
│   │                         #   cutoff + diffusion seam); per-object biquad
│   │                         #   low-pass with smoothed block-rate cutoff
│   ├── spread.rs             # Angular-region spread: fixed 3-ring sample
│   │                         #   directions + energy-normalized aggregation
│   ├── bed.rs                # SpatialBed (channel-based content): semantic-
│   │                         #   role routing onto matching output speakers,
│   │                         #   bounded store, allocation-free render
│   ├── field.rs              # SpatialField (diffuse content): encoded into
│   │                         #   the ambisonic bus (W only) + decoded onto
│   │                         #   every pan speaker (√N diffuse compensation),
│   │                         #   decorrelated per speaker via delay rings
│   │                         #   (AmbisonicFieldMixer)
│   ├── ambisonic.rs          # Ambisonics/HOA core (Phase 16 → order 3):
│   │                         #   exact order-N SH basis (sh_n, channel_count
│   │                         #   — order-1 FOA pinned + order-2 U/V/T/R/S +
│   │                         #   order-3 ACN 9–15 per the Furse–Malham table,
│   │                         #   all SN3D mean-square 1), encode_plane_wave_n,
│   │                         #   exact order-2/order-3 rotate_bus_frame_n
│   │                         #   (Wigner blocks by form/tensor projection),
│   │                         #   DecoderPolicy (Basic / MaxRe with per-order
│   │                         #   max-rE weights), AmbisonicDecoder
│   │                         #   (per-speaker matrix), AmbisonicRenderer::
│   │                         #   with_order (any supported order → any layout)
│   ├── room.rs               # Room acoustics: Room (box + absorption +
│   │                         #   order + RT60), image-source enumeration,
│   │                         #   EarlyReflections (per-object delay rings +
│   │                         #   tap smoothing + the binaural ring
│   │                         #   primitives + v3.47 per-(object,image)
│   │                         #   spectral reflection low-pass), RoomLateField
│   │                         #   (Schroeder tail encoding into the
│   │                         #   ambisonic bus)
│   ├── hrtf.rs               # Binaural head model: Woodworth ITD (reflective
│   │                         #   fold — correct for 0–360° azimuths),
│   │                         #   Duda-Martens head-shadow shelf (α = 1.05 +
│   │                         #   0.95·sinφ, first-order, DC=1), fractional-
│   │                         #   delay ring read, ElevationNotch (pinna
│   │                         #   notch biquad, exact passthrough at 0°),
│   │                         #   HrtfDataset (Phase 18: azimuth × elevation
│   │                         #   IR grid + bilinear interpolation with 360°
│   │                         #   wrap + synthetic generator for testing;
│   │                         #   Phase 20: from_corpus loads measured
│   │                         #   SOFA-style corpora — resample, normalize,
│   │                         #   JSON I/O)
│   ├── binaural.rs           # BinauralRenderer — the whole hybrid scene
│   │                         #   through the head model: objects (per-ear
│   │                         #   ITD + shadow, spread blurs cues; FIR
│   │                         #   convolution of interpolated spectral IRs
│   │                         #   when a dataset is loaded), beds
│   │                         #   (semantic-role fold, LFE at 1/√2), fields
│   │                         #   + late field via a virtual 8-speaker ring
│   ├── tracking.rs           # Head tracking (VR/AR seam): HeadTracker,
│   │                         #   HeadSample, TrackingConfig — nlerp
│   │                         #   interpolation + one-pole smoothing +
│   │                         #   optional rate limit; host applies the
│   │                         #   result to the listener per block
│   ├── automation.rs         # Spatial automation: CurveScalar / CurveVec3 /
│   │                         #   CurveQuat positional-seconds curves + a
│   │                         #   SpatialAutomation evaluated allocation-free
│   │                         #   at block rate (spec §47)
│   ├── diagnostics.rs        # SpatialDebugView — per-object / per-speaker /
│   │                         #   per-reflection debug info for hosts
│   ├── doppler.rs            # Doppler — live per-block pitch from
│   │                         #   (object.velocity − listener.velocity)
│   ├── health.rs             # SpatialHealthSnapshot — explainable per-source
│   │                         #   status (localization quality, direct-vs-
│   │                         #   reflected ratio, occlusion severity, phase
│   │                         #   risk) on the telemetry path
│   ├── metering.rs           # SpatialMeterState / SpatialMeters — per-speaker /
│   │                         #   bus / LFE peak + RMS accumulators (spec §70)
│   ├── nearfield.rs          # Near-field model (spec §40): bounded proximity
│   │                         #   gain + LF low-shelf boost, smoothed per block
│   ├── provider.rs           # HrtfProvider / HrtfCorpusProvider /
│   │                         #   HrtfDatasetProvider — HRTF loading seams
│   ├── quality.rs            # SpatialQuality tiers (Low/Medium/High/Ultra) —
│   │                         #   render refinement, never correctness
│   ├── upmix.rs              # UpmixMode / UpmixTrims — stereo→surround
│   │                         #   compatibility policies (spec §87–88)
│   ├── voice.rs              # VoiceBudget — per-scene voice admission
│   │                         #   (capacity / full-quality sub-capacity /
│   │                         #   priorities), per-block plan (spec §76)
│   ├── sofa.rs               # (feature `sofa-import`) NetCDF-3 classic SOFA
│   │                         #   import → HrtfCorpus; nc4/HDF5 refused (typed)
│   ├── render.rs             # SpatialRenderer trait (incl. HybridBlockInputs /
│   │                         #   process_hybrid_block), RendererKind (Basic /
│   │                         #   Vbap / Ambisonic / Binaural), RenderError
│   ├── panner.rs             # BasicPanner — equal-power pair pans, per-path
│   │                         #   coefficient smoothing, additive LFE send
│   │                         #   (LFE is not a pan target), simplified spread,
│   │                         #   cos(elevation) off-plane term; writes into a
│   │                         #   caller-supplied interleaved buffer so the
│   │                         #   steady-state hot path allocates nothing.
│   └── vbap.rs               # VbapRenderer — 3-triplet VBAP (3D layouts),
│                             #   2D azimuth-pair reduction (coplanar), and a
│                             #   deterministic nearest-speaker out-of-
│                             #   coverage fallback; geometry preprocessed at
│                             #   prepare (per-triplet inverses + Delaunay
│                             #   empty-triangle region filter), allocation-
│                             #   free render path with max-min-gain triplet
│                             #   selection and energy normalization.
│
├── output/                   # ── Output backends ──
│   ├── mod.rs                # Module wiring + re-exports
│   ├── output.rs             # Output trait + factory (backend selection/fallback)
│   ├── capabilities.rs       # Per-backend capability records & validation
│   ├── output_info.rs        # Negotiated format/access/latency info
│   ├── cpal_callbacks.rs     # Buffer-size/format negotiation helpers
│   ├── cpal_devices.rs       # cpal device enumeration
│   ├── device_match.rs       # Device-name matching heuristics
│   ├── format_converter.rs   # Sample-format conversion (f32 → i16/i24/i32/u16)
│   ├── rate_policy.rs        # Output sample-rate policy helpers
│   ├── endpoint.rs           # Multi-endpoint routing matrix (Phase 5b): per-
│   │                         #   endpoint ring + nominal-ratio resampler +
│   │                         #   rubato Slip drift trim + final limiter, plus
│   │                         #   the drift controller and virtual endpoint
│   ├── cpal_output/          # cpal shared-mode fallback (all platforms)
│   ├── alsa_output/          # Native ALSA exclusive (`hw:`/`plughw:`)
│   ├── wasapi_output/        # Native WASAPI exclusive (IAudioClient)
│   ├── wasapi_loopback.rs    # WASAPI loopback capture (system mix)
│   ├── asio_output/          # Native ASIO (COM vtable, native DSD)
│   ├── coreaudio_output/     # Native CoreAudio hog-mode
│   ├── wav_writer.rs         # Streaming float32 WAV file writer (capture)
│   ├── device_monitor.rs     # Hotplug monitoring
│   └── output_profile.rs     # Per-device output profiles
│
├── buffer/                   # ── Buffers (submodules of `buffer.rs`) ──
│   ├── pcm_ring.rs           # Lock-free SPSC ring (cache-padded atomics)
│   ├── fixed_frame.rs        # FixedFrameBuffer — interleaved f32 frame ring
│   ├── audio_frame.rs        # AudioFrame — typed sample frame
│   ├── dsd.rs                # DSD byte ring
│   └── output.rs             # Output ring helpers
└── bin/                      # ── Reference binaries ──
    ├── audio_engine_cli.rs   # Interactive REPL player
    ├── replaygain_scanner.rs # EBU R128 / ReplayGain scan + tag write-back
    └── aelog_replay.rs       # Deterministic aelog replay (--graph / --cache)
```

## Concurrency model

The engine is driven by a **single tick thread** (owned by the host — either
your own loop calling `tick_blocking`, the built-in FFI tick thread, or the
reference CLI). All engine state lives on that thread; there are no locks on
the audio path.

```
host ──EngineCommand──▶ cmd channel ──▶ tick loop ──▶ decode → DSP → ring
host ◀──EngineEvent──── event channel ◀─┘              │
host ◀──ArcSwap<PlaybackInfo> ── lock-free telemetry    ▼
                                                     output thread(s)
```

- **Commands**: a bounded crossbeam channel; `tick_blocking` sleeps on
  `recv_timeout` so the host never busy-polls.
- **Telemetry**: `PlaybackInfo` lives in an `ArcSwap`; writers publish whole
  snapshots with `rcu()`, readers `load()` — wait-free on the read side.
- **Audio**: the DSP output goes into a `FixedFrameBuffer` (SPSC ring with
  cache-padded atomics); the output backend drains it from its own thread
  (cpal callback, ALSA worker, WASAPI render thread, ASIO `bufferSwitch`,
  CoreAudio IO proc).
- **Capture** (WASAPI loopback): the loopback thread *fills* a separate ring;
  the tick thread drains it into a WAV file, so disk I/O never touches a
  realtime callback.

## Dual-decoder transitions

`PlaybackStream` holds up to two decoders (current + prepared-next). When the
current stream reaches EndOfStream the engine chooses, per `TransitionMode`:

- **Gapless** — swap to the next decoder with sample-accurate alignment.
- **Crossfade** — run both decoders, blend over the configured curve
  (constant-power / linear / exponential / logarithmic / S-curve).
- **Fade** — fade the current track out, then start the next.
- **Stop** — end playback.

The playlist auto-advances on EOS: `RepeatMode::One` restarts the current
track, `RepeatMode::All` wraps, shuffle cycles play every entry exactly once
before repeating.

## Realtime-safety rules

1. No allocation on the decode/DSP hot path (preallocated scratch, verified
   by `tests/fidelity/realtime_allocation.rs`).
2. Denormal flushing at DSP stage boundaries.
3. No locks — only atomics + the SPSC ring.
4. Output backends verify exclusivity against the OS before claiming it
   (ALSA `hw:` open, WASAPI exclusive `Initialize`, CoreAudio hog mode,
   ASIO `create_buffers`).

## Optional features

| Feature | What it adds |
|---|---|
| `wasapi-native` | Native WASAPI exclusive output **and** loopback capture (Windows) |
| `asio-native` | Native ASIO output with native-DSD transport (Windows) |
| `tag-write` | EBU R128 / ReplayGain tag write-back via `lofty` |
| `fingerprint` | Chromaprint/AcoustID fingerprinting |
| `resample` | Rubato sinc resampler |
| `codec-*` | Per-codec Symphonia/pure-Rust decoders |
| `audio-output` | Output backends (on by default) |
| `network-streaming` | HTTP(S) Range-request streaming via `ureq` |
| `c-ffi` | Stable C FFI surface |
| `sofa-import` | NetCDF-3 classic SOFA → `HrtfCorpus` (nc4/HDF5 refused) |
| `codec-dsd` | Accepted no-op for API compatibility (DSD compiled unconditionally) |
