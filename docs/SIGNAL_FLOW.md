# Signal Flow

This document traces one block of audio samples from file to speaker, plus
the analysis and capture side-paths. See [`ARCHITECTURE.md`](ARCHITECTURE.md)
for the module-level view.

## Playback path

```
file / URI / memory
        │
        ▼
┌─────────────────┐   ┌──────────────────┐
│ Format scanner  │──▶│ Decoder::open    │  probe by extension + magic bytes,
│ (scanner.rs)    │   │ (decoder.rs)     │  route to Symphonia / DSD / Opus,
└─────────────────┘   └────────┬─────────┘  TTA / WavPack / APE
└─────────────────┘   └────────┬─────────┘
                               │  AudioFrame<f32> blocks (native rate,
                               │  native channel layout, up to 7.1.4)
                               ▼
┌──────────────────────────────────────────────┐
│            Decode & process loop             │
│  (engine/decode_loop/)                       │
│                                              │
│  ┌─────────────────────────────────┐         │
│  │ Channel trim / routing /        │  multichannel path only: per-channel
│  │ bass mgmt / LFE                 │  gain, delay, polarity, crossover
│  └────────────────┬────────────────┘         │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Mix bus (MixBusNode)          │  N per-input chains: preamp + loudness
│  │ per-input pre-mix + sum       │  + user gain/balance/mute, summed under
│  │                               │  a TrackMixer-compatible envelope
│  │                               │  (gapless / crossfade / fade; the
│  │                               │  pre→post mixing boundary)
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Parametric EQ (+ graphic EQ)  │  up to 64 bands + AutoEQ presets,
│  │ (post-mix)                    │  10/15/31 ISO graphic layer, preamp
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Multiband compressor          │  3-band
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Convolution reverb            │  FFT partitioned, IR loader
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Balance → crossfeed → stereo  │  balance, Bauer / Chu Moy / J. Meier
│  │ enhancer                      │  crossfeed, mid-side width
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Timestretch / pitch           │  WSOLA (varispeed, time-stretch,
│  └────────────────┬──────────────┘  pitch-shift)
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Volume + seek fade            │  software gain w/ ramps
│  └───────────────────────────────┘           │
│                                              │
│              analyzer tap ──────────────────▶│  RMS / peak / spectrum /
│  (fed from the decode loop)                  │  dominant frequency → PlaybackInfo
│                                              │
│  Post-mix block then goes to the output      │
│  domain (not shown here; see below).         │
└──────────────────────────────────────────────┘

Then, in the **output domain** (after the process loop, at the output rate):

```
post-mix block (f32 or f64)
        │
        ▼
┌─────────────────────┐
│ Resampler           │  Rubato sinc → output rate, or bit-perfect
│ (or passthrough)    │  / DoP passthrough
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ Safety limiter      │  4× true-peak FIR, ceiling, lookahead
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ TPDF dither         │  applied at int conversion boundary only
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ FixedFrameBuffer    │  lock-free SPSC ring, interleaved f32
│ (primary output)   │
└──────────┬──────────┘
           ▼
┌──────────────────────────────────────────────────────────┐
│ Independent endpoint fan-out                            │
│ Each enabled endpoint has its own bounded ring, gain,    │
│ lifecycle, drop counters, and transport error state.    │
└──────────┬───────────────────────────────────────────────┘
           ▼
┌─────────────────────┼─────────────────────┐
▼                     ▼                     ▼
cpal shared      native exclusive    ASIO / WASAPI / CoreAudio hog
mode callbacks   (ALSA hw:, WASAPI    (native DSD transport)
                 exclusive, CoreAudio)
```

### Precision modes

The whole chain runs in `f32` by default (Performance). In `f64` Quality
mode each sample is promoted to `f64` once at the start of the process loop
and every pre- and post-mix stage runs in double precision until the result
is demoted back to `f32` for the ring. Two hard bypass modes skip the entire
chain: **bit-perfect** (only volume ramps and seek fades survive) and **DoP
bypass** (a pure passthrough so 24-bit DSD-over-PCM words reach the DAC
unmodified). The safety limiter and dither run in the output domain in f32.

## Side paths

### System-audio capture (WASAPI loopback)

```
system mixer (all apps)
        │  IAudioCaptureClient packets
        ▼
loopback thread ──▶ capture ring (FixedFrameBuffer)
                          │  drained every tick
                          ▼
              WAV file (f32) ──▶ finalized header on stop
```

Capture is independent of playback state — you can record system audio while
the engine is idle or playing through a different endpoint.

### Additional endpoints (multi-endpoint routing matrix, v3.7.0)

```
master stereo block (output domain, primary rate)
        │  fan-out on the decode loop, once per flushed block
        ▼
per endpoint: SPSC ring (decode loop pushes, backend drains)
        │  resampler master → endpoint rate (None when rates match)
        │  endpoint-rate final limiter (resampled frames only)
        │  per-endpoint gain
        ▼
endpoint backend's realtime callback ──▶ device
```

Each `EngineConfig.endpoints` entry drives one extra output device
independently: its own lock-free ring, its own rate domain, its own final
limiter sized for that rate, and its own level. A stuck endpoint buffers at
most `MAX_ENDPOINT_PENDING_FRAMES` ahead of its ring (oldest frames dropped
first) and can never take down the primary device. The per-endpoint FFT
resampler runs at the fixed nominal ratio; when drift correction is enabled
it is followed by a rubato `Slip` (a 1:1 clutch that occasionally
inserts/drops a single frame behind a short crossfade) whose ratio is
steered by a proportional ring-fill controller, so the stream tracks the
device's actual crystal (offset reported in ppm) instead of its nominal
clock. Same-rate endpoints reuse the master's already-limited block
untouched.

### Acoustic world simulation (opt-in, v3.25.0)

A **simulation-side** layer that computes how sound propagates through a
space, separate from rendering. Controlling/acoustic geometry is described
with frequency-dependent materials:

```
AcousticRoom (per-wall MaterialSpectrum) + Portal(s) + DiffractionEdge(s)
        │  AcousticWorld::solve(source, listener)
        ▼
[AcousticPath; N]  — Direct / Reflected / Diffracted / Transmitted
        │  each: direction, distance, delay_samples, gain, lowpass_hz, flags
        ▼
renders (binaural / pan / offline baker) consume the paths
```

The solver runs on the control / offline path (heap-happy by design, like
correction); the realtime renderers consume only the fixed-size paths. An
order-1 box yields one direct + six image-source reflections whose
`delay_samples` match the renderer's own room-image geometry; portals add a
material-filtered transmission path plus wedge diffraction around their
jambs.

### Acoustic baking (opt-in, v3.26.0)

For **static** scenes the solve above is identical block after block — so it
is baked once and cached, then consumed at audio time:

```
AcousticBaker (control path)
        │  bake_scene(object positions, listener)
        ▼
BakedScene — position → response cache (0.5 m cells)
        │  listener_images(BakedObject) → [ListenerImage; MAX_IMAGES]
        ▼
BasicPanner / VbapRenderer / BinauralRenderer (set_baked)
        │  object in baked cell?  ──yes──▶ cached taps (no solve)
        └──no──▶ live AcousticWorld::solve fallback (Phase 23)
```

Baking is control/offline-path; render-time is a `HashMap` read plus a flat
copy into the fixed `ListenerImage` buffer — no solving, allocation, or
locks. With no bake attached the renderers are bit-identical to v3.25; each
baked reflection also keeps its full per-band material spectrum for offline
frequency-domain renderers.

### Graph 2.0 — general-purpose topology (offline, v3.27.0)

Alongside the fixed-chain realtime graph, a new topology model makes the
graph itself the center of rendering: nodes declare **explicit typed ports**
(signal class + channels) and connections are **first-class edges**; the
edge set *defines* the signal flow, and execution order is derived:

```
Build        Graph2::add_source/add_gain/add_delay/add_mix/add_split/add_sink
             └─ add_edge (fail-fast: endpoints, typed buses, fan-in)
Validate     Graph2::validate → ValidationReport
             (errors: bad ports, SignalType mismatch, fan-in, cycles
              with the cycle path; warnings: dangling ports)
Compile      Graph2::compile → ExecutionOrder (deterministic Kahn topo sort)
             └─ every mutation invalidates → dynamic recompilation
Execute      OfflineExecutor::process_block  (offline, block-by-block)
             └─ per-edge planes; Source/Sink/Gain/Delay/Mix/Split ops
```

Example — a dry/wet bus is arbitrary topology, not a special case:

```
Source ──▶ Split(2) ──▶ Gain(0.5) ──▶ Mix ──▶ Sink
                   └──▶ Delay(100) ──▶┘
```

The whole topology serializes to JSON and back to an identical render, and
exports a Graphviz `digraph` for inspection. Offline-first by design (like
the acoustic layer); the realtime `dsp::graph` hot path is untouched.

### Spatial rendering (opt-in, v3.11.0 → v3.19.0)

```
SpatialScene (world space: listener + objects + beds + fields)
        │  per-block object / bed / field audio planes + scene
        ▼
renderer::process_hybrid_block  (BasicPanner — equal-power pair pans, or
        │                         VbapRenderer — 3-triplet VBAP on 3D
        │                         layouts, 2D pair reduction on coplanar,
        │                         nearest-speaker out-of-coverage fallback)
        │   objects: level chain (distance · directivity · occlusion) →
        │            pan solves (spread-aware) → smoothing → LFE send
        │            + room: image-source early reflections (per-object
        │              delay rings, per-image pan solves + coeff·dist) and
        │              room send → Schroeder tail → ambisonic W encode
        │   beds:    semantic-role routing onto matching speakers
        │   fields:  ambisonic encode (W) → bus → decode → decorrelation
        ▼
interleaved multichannel PCM (frames × layout channels)
        │
        ▼
output domain ──▶ existing ring / endpoint path

Ambisonic bus path (opt-in, v3.15.0 → order-3 in v3.20.0)
        │  bus planes [W, Y, Z, X] or order-2 [W,Y,Z,X,U,V,T,R,S] or
        │  order-3 +ACN 9–15 (world orientation, exact SN3D SH basis)
        ▼
AmbisonicRenderer (order ≤ 3): per-frame listener rotation (exact
        │   order-1/2/3 Wigner matrices) → decode matrix (Basic sampling
        │   or per-order max-rE weights) → calibration
        ▼
interleaved multichannel PCM

Binaural path (opt-in, v3.17.0 → spectral HRTFs in v3.19.0 → measured corpus loading in v3.21.0)
        │  full hybrid scene (objects + beds + fields + room)
        ▼
BinauralRenderer (stereo/headphone layout, exactly 2 ears)
        │   objects: level chain → per-(direction, ear) cues — analytic
        │            Woodworth ITD (fractional delay) + Duda-Martens
        │            shadow shelf + pinna ElevationNotch, or FIR
        │            convolution of an interpolated spectral IR when a
        │            measured HrtfDataset is loaded (carries ITD +
        │            elevation cues); spread samples each ring direction
        │            with its own cues; LFE send folds at 1/√2
        │   beds:    semantic-role azimuth fold (FL → −30°, SL → −110°, …;
        │            LFE role folds at 1/√2)
        │   room:    image reflections at excess_path + ITD(ear), per-image
        │            shadow; late field → Schroeder tail → virtual ring
        │   fields:  ambisonic W encode → √N compensation → virtual 8-
        │            speaker ring (decorrelated) → per-speaker ITD + shadow
        ▼
interleaved stereo PCM (L, R ears)

Head tracking (opt-in, v3.18.0) — the VR/AR seam, control-side only
        │  IMU / VR rig ── HeadSample(time, quat) ──> HeadTracker
        │        nlerp across the last two samples → one-pole smoothing
        │        (smoothing_ms) → optional rate limit (deg/s)
        ▼
listener.orientation ──> scene ──> any renderer (unchanged)
        │  world-fixed sources keep their world position as the head turns;
        │  the host calls tracker.sample(now) once per render block

SpatialNode in the production graph (opt-in, v3.19.0)
        │  stereo master planes [L, R] + node controls (enable / screen /
        │  room / listener) drained at the block boundary
        ▼
SpatialNode plan step: binaural head model (+ room) on the front pair
        │  MC masters pass through untouched (documented seam); live
        │  enable survives generation swaps
        ▼
master planes ──> limiter / output

Scene files (opt-in, v3.19.0) — content only, renderer-independent
        │  SpatialScene ──to_config──> config::SpatialSceneConfig
        │     (listener quaternion, objects, beds by role names, fields,
        │      room) ──save_scene_json──> JSON on disk
        │  JSON ──load_scene_json──> config ──from_config──> SpatialScene
        │  validate() enforces engine caps before conversion
```

Spatial rendering is **opt-in**: the conventional decode loop and DSP graph are
untouched. A host builds a `SpatialScene` of objects (world positions, gains,
spread, LFE send), prepares a renderer — `BasicPanner` (equal-power pairs),
`VbapRenderer` (3-triplet VBAP for 3D layouts, 2D pair reduction for coplanar
ones, nearest-speaker out-of-coverage fallback), or `AmbisonicRenderer` (a
FOA bus decoded to any layout) — on a `SpeakerLayout` (stereo / 5.1 / 7.1 /
7.1.4 / custom), and renders decoded object planes into a caller-supplied
interleaved multichannel buffer that can be pushed through the existing
output core (`SampleSink::push_frames_interleaved`). The object pipeline
order is: `listener-space transform → distance model → directivity (source
facing vs. listener angle) → occlusion (attenuation + low-pass) → pan
coefficients (BasicPanner equal-power or VBAP basis solve with energy
normalization; angular-region spread samples the direction cap) →
coefficient smoothing → LFE send → channel calibration trim`. When a `Room`
is enabled (and the object's `room_send` is non-zero), the same filtered
sample additionally drives the room: image-source early reflections (each
image solved by the renderer's own pan machinery, scaled by the wall
reflection coefficients and image distance, delayed by the excess path via
a per-object ring) and a room-send accumulator whose Schroeder tail encodes
into the ambisonic bus and decodes as a diffuse source. Beds route by
semantic role onto matching speakers; fields encode into the ambisonic bus
(`W` only — perfectly diffuse — with the `√N` diffuse compensation) and
decode onto every pan speaker with per-speaker decorrelation; all three
classes sum into the same buffer via `process_hybrid_block` (the spatial
mixer). Alternatively the same hybrid scene renders **binaurally**
(`BinauralRenderer`, stereo/headphone layout): the head model replaces the
speaker array — every object/bed path becomes a contralateral-ear Woodworth
delay plus a Duda-Martens shadow shelf (per direction/ear), the LFE folds
at `1/√2`, and diffuse content (fields + the room's late field) decodes
onto a virtual 8-speaker ring (ambisonic bus + `√N` compensation + the
field mixer's decorrelation) before each virtual speaker is head-modeled.
Mirror symmetry is the exact invariant; the head diffracts (no
constant-power law). With a measured `HrtfDataset` loaded, object direct
paths replace the analytic chain with FIR convolution of the bilinearly
interpolated spectral IR (which carries both the ITD and the elevation
cues); the analytic path keeps the `ElevationNotch` pinna model as its
elevation cue. The same hybrid scene is now also reachable as a real plan
step: the `SpatialNode` spatializes the graph's stereo master through the
head model (optionally with the room) with a full control surface, while
multichannel masters pass through untouched. A host that already has an
ambisonic bus can skip the scene and feed `AmbisonicRenderer` directly
(now up to order 2): the renderer rotates each frame by the listener
orientation (world-fixed fields) and applies the decode matrix (`Basic`
sampling or per-order `MaxRe` weights) plus calibration. Scenes persist
through the scene-file format: `save_scene_json` / `load_scene_json`
round-trip a `SpatialScene` losslessly (the listener orientation stays the
canonical quaternion) into a renderer-independent JSON document. The
scene/level model and renderers live in `crate::spatial` (see
[`ARCHITECTURE.md`](ARCHITECTURE.md)); `tests/fidelity/spatial_panner.rs`,
`tests/fidelity/spatial_vbap.rs`, `tests/fidelity/spatial_object_behavior.rs`,
`tests/fidelity/spatial_hybrid.rs`, `tests/fidelity/spatial_ambisonic.rs`,
`tests/fidelity/spatial_room.rs`, `spatial_hoa.rs`, `spatial_node.rs`,
`spatial_hrtf_ir.rs` and `spatial_scene.rs` pin the contracts and
`realtime_allocation` verifies the render paths allocate nothing in steady
state.

### Loudness analysis

```
decode (offline, background thread)
        │  EBU R128 meter (ITU-R BS.1770 / EBU Tech 3342: 400 ms momentary
        │  blocks on a 100 ms hop, 3 s short-term window, gated)
        ▼
LoudnessScanResult { LUFS, dBTP, LRA, RG gain/peak }
        │
        ├─▶ applied to playback pipeline (loudness normalization)
        ├─▶ merged into metadata tags (`tag-write`)
        └─▶ emitted as LoudnessScanComplete event
```

### Fingerprinting (AcoustID)

```
decode (offline) ──▶ mono downmix ──▶ 16-bit PCM ──▶ Chromaprint
        ──▶ compact fingerprint + duration ──▶ submit to AcoustID API
```

### Multi-endpoint fan-out

The processed output block is offered independently to every enabled configured
endpoint. Each endpoint has its own bounded SPSC ring and gain multiplier;
backpressure or drops on one endpoint do not alter primary-sink delivery or
another endpoint. `PlaybackInfo::endpoints` reports per-endpoint written,
dropped, available, and transport-error counters, while `OutputEvent::EndpointError`
reports transport failures asynchronously.

### Telemetry

Every tick publishes a `PlaybackInfo` snapshot into an `ArcSwap`:
position, state, volume, format, DSP status, analyzer levels, queue state,
and u64 counters (clips, NaNs, underruns, CPU overloads, deadlock misses).
Hosts read it lock-free from any thread.
