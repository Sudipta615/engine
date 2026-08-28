# Architecture

This document describes the overall structure of the engine. For the sample
flow through the DSP chain, see [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md). For
runnable embedding examples (Rust `EngineHandle` + C FFI), see
[`EMBEDDING.md`](EMBEDDING.md). For the phased evolution from the
single-stream player to the multi-stream graph runtime, see
[`ROADMAP.md`](ROADMAP.md).

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
├── ffi.rs                    # C FFI surface (engine_create/destroy, controls)
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
│   ├── volume.rs             # Volume control modes (software / hardware)
│   ├── output_setup.rs       # Output backend creation & device selection
│   ├── helpers.rs            # Shared helpers (event emission, playback info writes)
│   ├── decode_loop/          # Decode-and-process hot loop (single / transition)
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
│       └── capture.rs        # WASAPI loopback capture start/stop
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
│   ├── pipeline/             # DspPipeline — reference chain; bit-exact
│   │                         #   oracle for the graph equivalence suite
│   │                         #   (mod.rs + controls/process/format/tests)
│   ├── correction/           # Room & headphone correction (Phase 7):
│   │                         #   sweep/ (ESS measurement + deconvolution),
│   │                         #   ir/ (WAV import + conditioning), phase/
│   │                         #   (min/linear/hybrid rendering), derive/
│   │                         #   (smoothed regularized inverse) — all
│   │                         #   control-thread f64 DSP
│   ├── equalizer/            # Parametric EQ (RBJ) + shared types
│   ├── graphic_eq.rs         # Graphic EQ model (10/15/31 ISO bands) → compiled into EQ
│   ├── loudness/             # EBU R128 loudness meter/normalizer
│   ├── resampler/            # Rubato-based sinc resampler (+ resampler_handle.rs)
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
│                             #   report / plan / swap / nodes/mix/
│                             #   (MixBusNode split into mod/envelope/sum:
│                             #   N-slot + N-channel bus with post-fader
│                             #   lane sends), and nodes/aux_node.rs
│                             #   (AuxBusNode + shared AuxSendBus: per-send
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
│                             #   convolution bank, post-aux/pre-EQ)
│
├── spatial/                  # ── Spatial audio (Phases 8–13, opt-in) ──
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
│   ├── ambisonic.rs          # Ambisonics/HOA core: ACN/SN3D conventions,
│   │                         #   sh_foa SH basis, encode_plane_wave, order-1
│   │                         #   rotate_bus_frame, DecoderPolicy (Basic /
│   │                         #   MaxRe), AmbisonicDecoder (per-speaker
│   │                         #   matrix), AmbisonicRenderer (listener-
│   │                         #   rotated FOA-bus decode to any layout)
│   ├── room.rs               # Room acoustics: Room (box + absorption +
│   │                         #   order + RT60), image-source enumeration,
│   │                         #   EarlyReflections (per-object delay rings +
│   │                         #   tap smoothing), RoomLateField (Schroeder
│   │                         #   tail encoding into the ambisonic bus)
│   ├── render.rs             # SpatialRenderer trait (incl. HybridBlockInputs /
│   │                         #   process_hybrid_block), RendererKind, RenderError
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
│                             #   Beds/fields/room/HRTF/binaural are declared
│                             #   seams for later phases (spec Part XXVI).
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
│   │                           rubato Slip drift trim + final limiter, plus
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
└── buffer/                   # ── Buffers ──
    ├── pcm_ring.rs           # Lock-free SPSC ring (cache-padded atomics)
    ├── fixed_frame.rs        # FixedFrameBuffer — interleaved f32 frame ring
    ├── audio_frame.rs        # AudioFrame — typed sample frame
    ├── dsd.rs                # DSD byte ring
    └── output.rs             # Output ring helpers
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
