# Changelog

All notable changes to this project are documented in this file.

## [3.16.0] — 2026-08-28

Room acoustics (roadmap Phase 13): the spatial scene gains an acoustic
space. A `spatial::room::Room` (axis-aligned box in world space) turns
participating objects into small acoustic events: **early reflections** via
the image-source method (each mirrored image is a virtual source with its
own pan solve, distance attenuation, reflection-coefficient amplitude, and
excess-path delay through a per-object ring) and a **late field** — a
Schroeder tail shaped by the room's RT60 that encodes into the ambisonic
bus (the Phase 12 seam, §55) and decodes as a diffuse, decorrelated source.
The room is opt-in at both levels: `Room::default()` is disabled (renders
stay bit-identical) and an object participates only through its `room_send`
— the seam the scene model declared. Occlusion's `AcousticTransmission` is
the transmission seam (§43–44): the same low-passed sample that feeds the
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

## [3.15.0] — 2026-08-28

Ambisonics / First-Order Ambisonics (roadmap Phase 12): the engine's
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
  so higher orders are a table + rotation extension (§34).
- **Decoder** (`AmbisonicDecoder`): `DecoderPolicy::Basic` — the sampling
  decoder `D = Y(S)ᵀ/N`, so a plane wave from `d` lands on speaker `s` as
  `(1 + 3·cosθ)/N` — and `DecoderPolicy::MaxRe` (documented FOA `a1 = √3/2`
  lobe narrowing). `prepare` builds the per-speaker matrix and rejects
  empty / LFE-only layouts; `process_bus` and the per-frame `decode_frame`
  are allocation-free.
- **Standalone `AmbisonicRenderer`** (§23): decodes an interleaved `[W, Y,
  Z, X]` bus (four planes via `process_block`) into the active layout,
  applying the listener's orientation per frame (so a world-encoded field
  stays world-fixed as the listener turns, §48) and per-speaker
  calibration. Registered as `RendererKind::Ambisonic`.
- **Diffuse-field upgrade** (`AmbisonicFieldMixer`, replaces
  `DiffuseFieldMixer`): the field path now goes field → encoder → FOA bus →
  decoder → per-speaker decorrelation rings. `W` is boosted by `√N` (the
  documented diffuse compensation) so a diffuse field decodes at unit
  energy with the equal-power `1/√N` spread, exactly preserving the
  previous phase's behavior; LFE never receives field energy.
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

## [3.14.0] — 2026-08-28

Hybrid beds & fields (roadmap Phase 11): the spatial scene's second and
third content classes. A `SpatialScene` now carries **beds** (channel-based
content that already has a spatial structure — 5.1 music, a 7.1 effects bed)
and **fields** (positionless diffuse environments — rain, ambience, crowds)
alongside objects, and both renderers mix all three through one
`process_hybrid_block` into a single interleaved buffer (spec §37's spatial
mixer). The object-only `process_block` and the trait's default hybrid
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

## [3.13.0] — 2026-08-28

Object behavior (roadmap Phase 10): directivity, occlusion, and a real
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
  so both renderers can never disagree on the convention (§41).
  `CustomDirectivity` is a fixed 91-sample curve (2° steps, linear
  interpolation, clamped) — stack-copied, allocation-free on the render
  path. `SpatialAudioObject` gains `source_orientation` (world-space quat;
  local +Y is the facing) and `directivity`.
- **Occlusion** (`spatial::occlusion`): [`Occlusion`] config (amount +
  max attenuation + min cutoff) mapped to a structured
  [`AcousticTransmission`] (`attenuation_db`, `cutoff_hz`, `diffusion` —
  diffusion is a declared seam, §44) with the cutoff interpolated
  exponentially in log-frequency. Applied as gain plus a real Butterworth
  low-pass (engine biquad, Q = 0.707) with per-object filter state and
  smoothed block-rate cutoff — an occluded source is quieter *and* duller,
  before panning (§43), feeding both the pan paths and the LFE send.
- **Angular-region spread** (`spatial::spread`): replaces the simplified
  nearest-speaker widening in both renderers with the spec's recipe (§30) —
  solve the exact direction (weight `1−s`) plus 3 ring samples at `s × 60°`
  (weight `s/3`), aggregate by speaker, and energy-normalize (constant power
  while the image widens, §29). Fixed sample count (4 solves, 12 entries),
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

## [3.12.0] — 2026-08-28

3D VBAP-style object-to-speaker rendering (roadmap Phase 9): the spatial
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
  actual speaker geometry (§25), with `PanMode` classification
  (`ThreeDim` / `Planar` / `Single`) resolving degenerate and reduced-dimension
  layouts (§27) at `prepare` time.
- **Geometry preprocessing** (§21): normalized speaker directions, the
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
- **Out-of-coverage fallback** (§28): no enclosing triplet (below the
  floor, above the rig) → deterministic direction-preserving nearest-speaker
  fallback; never silent, never NaN, never state-polluting.
- **Realtime discipline** (§71–75): all triangulation/inverse work happens
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

## [3.11.0] — 2026-08-28

Spatial audio foundation (roadmap Phase 8): an independent, opt-in spatial
scene layer plus the first renderer. A `crate::spatial` module brings a
speaker-independent object scene model and an equal-power `BasicPanner` that
renders the same scene to any layout — stereo, 5.1, 7.1, 7.1.4, or a custom
array — into a normal interleaved multichannel buffer the existing output
core delivers. The conventional PCM/DSP path is untouched; spatial rendering
is opted into via `ChannelPolicy::SpatialRender`. The spec's multichannel
foundation (semantic channels, N-channel buffers, LFE-as-effects-path already
normalized in prior phases) means this phase adds the scene/level/render
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

## [3.10.0] — 2026-08-28

Room/headphone correction pipeline (Phase 7 S1–S5): measurement through
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

## [3.9.0] — 2026-08-27

Endpoint & aux-insert control surfaces, per-endpoint clock drift correction,
and the aux bus promoted to its own graph plan node.

### Added

- **C FFI endpoint routing surface** (`src/ffi.rs`): `engine_upsert_endpoint`
  (device, backend, gain, enabled, drift correction), `engine_remove_endpoint`,
  `engine_clear_endpoints`, `engine_endpoint_count`, `engine_endpoint_id`, and
  `engine_endpoint_info` (rate, gain, pending frames, drift offset) so
  C/C++ hosts can drive the multi-endpoint routing matrix.
- **C FFI aux-insert surface**: `engine_set_aux_insert` and
  `engine_aux_insert_state` expose the Phase 6 aux-bus convolution insert.
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

## [3.8.0] — 2026-08-27

Phase 6 (roadmap) first cut: the aux-bus insert seam and a bit-exact SIMD
pass on the mix bus. The aux bus (Phase 5) gains an optional global insert —
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

## [3.7.0] — 2026-08-27

Multi-endpoint routing matrix (roadmap Phase 5): the engine can now drive
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

## [3.6.1] — 2026-08-27

Phase 5 hardening: the v3.6.0 mix-bus surface had real defects that could
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

## [3.6.0] — 2026-08-27

Phase 5 is complete: per-slot mixer controls and robust multi-endpoint
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


Phase 5 S1–S3 of the player → graph-runtime roadmap: the mix bus becomes a
real mixing surface. Per-slot channel trim banks (S1) shape each slot's
planes per channel (gain in dB + polarity) between its pre-mix chains and
its sum. Post-fader sends (S2) split every slot's contribution between the
master sum (`master_gain`) and a new aux-bus accumulator (`aux_gain`),
with `Send` automation tracks modulating the tap sample-accurately. The aux
bus (S3) is a first-class stereo accumulator with its own per-block
peak/RMS metering, a duck target id (`AUX_BUS_ID`), a return gain into the
master before the post-mix chain, a Phase-6 insert seam, and full
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
  Phase-6 `insert` seam.
- Engine commands `SetTrackMasterGain` / `SetTrackSend`; `LaneInfo` now
  carries `send_master_gain` / `send_aux_gain`.
- Graph tests for trim/send/aux end-to-end; engine lane test covers the
  send-only path (aux tapped, master silent at zero return).

## [3.5.0] — 2026-08-26

Phase 4 S3–S6 of the player → graph-runtime roadmap: the mix bus gains
musical behavior and the engine gains a multi-track lane registry. Per-slot
pan laws and level meters (S3), program-gated ducking (S4), and
sample-accurate automation tracks (S5) live entirely in the bus; the engine
now plays N independent lanes on bus slots ≥ 2 alongside the primary stream,
with `AddTrack` / `RemoveTrack` / `SetTrackGain` / `SetTrackPan` /
`DuckTracks` commands and per-lane telemetry in `PlaybackInfo` (S6).

### Added

- **Per-slot pan law** (S3): each `MixInput` gains `pan` (`[-1, 1]`) and a
  `PanLaw` (`Linear` / `EqualPower` / `Center`, default `Linear`); the pan
  pair is folded into the front-L/R gain product so `pan = 0` stays
  bit-exact. `SetPan` / `SetPanLaw` commands; the existing `SetBalance`
  behavior is untouched.
- **Per-slot level meters** (S3): every slot accumulates per-block
  peak/RMS metering (dBFS) and publishes it to the control bus;
  `GraphControlHandle::slot_meters(slot)` reads `(peak_db, rms_db)`.
- **Program-gated ducking** (S4): `DuckState` (source slot, threshold,
  depth, attack/release frames, up to 4 target slots) rides the control
  queue; the audio side evaluates the trigger once per block from the
  source slot's peak meter and ramps the duck gain over attack/release.
  Disabled (`None`) is bit-exact. Exposed as `set_duck` and the engine's
  `DuckTracks` command.
- **Automation tracks** (S5): a slot may carry one immutable track
  (`AutomationTarget::Gain | Pan`) of up to 64 breakpoints; values are
  linearly interpolated sample-accurately on the audio path, with the edge
  values holding. Tracks are replaced wholesale via
  `set_slot_automation` / `clear_slot_automation` and reset per generation.
  A slot with no track is bit-exact.
- **Engine lane registry** (S6): `LaneTrack` (decoder + resampler + bounded
  FIFO) per independent stream on the first free bus slot ≥ 2; the decode
  loop fills each active lane's planes every block and feeds them as
  secondaries (`process_block_lanes` for the single path, lanes riding
  after the incoming stream during crossfades). Commands:
  `AddTrack(Source)`, `RemoveTrack(slot)`, `SetTrackGain { slot, gain }`,
  `SetTrackPan { slot, pan }`, `DuckTracks { … }`. Adding a lane grows the
  bus on demand via the glitch-free generation swap.
- **Lane telemetry** (S6): `PlaybackInfo.lanes: Vec<LaneInfo>` (slot,
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

## [3.4.0] — 2026-08-27

Phase 4 S1+S2 of the player → graph-runtime roadmap: the mix bus slot count
becomes a first-class generation parameter and secondary streams go
N-channel. From here the graph is `EngineConfig::mix_slots` lanes (slots 0/1
are the transition pair, slots ≥ 2 are independent simultaneous streams), and
multichannel output can be fed N simultaneous N-channel `Tracks`.

### Added

- **Slot-count generation parameter** (`EngineConfig::mix_slots`,
  default 2): the mix bus is built with `config.mix_slots` inputs (clamped
  to `MAX_MIX_SLOTS`), so a graph carrying N simultaneous streams is a
  plain generation rebuild ride the Phase-2 publish/swap/retire handshake.
  `MixBusNode::with_slots` constructs an N-slot bus; slots ≥ 2 carry
  `lane_preamp` / `lane_loudness` chains.
- **Per-slot user state** (`SlotState`): gain / balance / mute / active are
  mirrored onto sticky per-slot atomics at drain and replayed into fresh
  generations on `reconfigure`, so a reconfig never snaps a lane's settings
  (gains replay as one-pole targets; slot 0 is never detached).
- **N-channel secondary planes** (Phase 4 S2): `MixInput` planes are now
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
  path since Phase 3.

## [3.3.0] — 2026-08-27

Phase 3 of the player → graph-runtime roadmap: the mix bus and the engine
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
  their chains do not advance (Phase-3 S2 stream slots).
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

## [3.2.0] — 2026-08-26

Phase 2 of the player → graph-runtime roadmap: live graph swap. The graph is
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
- **Phase-2 gates**: `graph_*` unit tests for the defer/swap/coalesce/
  reclamation discipline and a two-thread control-vs-audio stress test;
  `realtime_graph_swap_does_not_allocate_on_audio_thread` pins the
  zero-allocation swap contract; `graph_live_reconfig` bench group reports
  the per-block cost of a reconfig cadence.

### Fixed

- **1 ms dead weight in the public generation builder**: the Phase-2
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

## [3.1.0] — 2026-08-26

Phase 1 of the player → graph-runtime roadmap: `DspGraph` gains a compiled
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

## [3.0.0] — 2026-08-26

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
