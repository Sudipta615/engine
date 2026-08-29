# Engine evolution: single-stream player → multi-stream graph runtime

This document records how the engine evolved — a phased history of two
completed campaigns: the transformation from a single-stream, pipeline-driven
player into a multi-stream, node-graph mixing runtime (Phases 1–7), and the
speaker-independent spatial layer (Phases 8–22, plus the v3.24 seams). Each
phase is grounded in the code as it exists on `main`; all phases are **Done**
(merged), so this is a historical record rather than a forward-looking plan.

The architecture reference is [`ARCHITECTURE.md`](ARCHITECTURE.md); the
signal path is in [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md). Every phase follows one
non-negotiable realtime discipline: **no heap allocation and no locks on the
decode/DSP hot path** (atomics + preallocated planes + SPSC control queues
only), and every new capability must be **disabled-exact** — a feature that
is off is literally absent from the executed expression, so the frozen
pipeline-vs-graph equivalence suite (`tests/fidelity/graph_pipeline_equivalence.rs`)
stays bit-exact across phases.

---

## Phase 1 — Execution plans + control surface (v3.1.0) — **Done**

**Intent.** Introduce the `DspGraph` as the production hot path without yet
changing any audio: a node arena (`GraphNode`), a compiled execution plan
(`GraphPlan` / `PlanSet`), enum-dispatch processing, and a typed control
surface — while keeping the existing `DspPipeline` as the frozen oracle.

**Key mechanisms.**
- `GraphNode` arena with typed accessors; nodes declare capability
  descriptors (`DspNode::capability`) so latency/tail/bit-perfect metadata is
  automated.
- Execution plans compiled per config; enum dispatch once per block instead
  of trait-vtable per node.
- `DspGraph` / `GraphControlHandle`: queued control commands, block-boundary
  drain.
- Golden equivalence harness: the graph vs the pipeline across 27
  scenarios, bit-exact.

**Realtime discipline.** The plan runner is zero-alloc (preallocated
scratch); node order and bypass come from the compiled plan.

**Unblocks.** The arena and plan are the substrate every later phase extends.

---

## Phase 2 — Live generation swap (v3.2.0) — **Done**

**Intent.** Make the graph reconfigurable **during playback** without a
glitch: build a fresh generation, publish it, and retire the old one at a
block boundary.

**Key mechanisms.**
- `GraphGeneration` + `UserState`: each generation owns immutable nodes and a
  plan; user state (gains, balances, mode) is mirrored onto sticky atomics on
  the control bus and **replayed** into the fresh generation.
- Per-node SPSC control queues; commands drain at block boundaries.
- Publish / swap / retire handshake with deferred reclamation — the retired
  generation is dropped only after the audio thread has left it, never mid-
  block.

**Realtime discipline.** The swap is a pointer publish (`AtomicPtr`/arc
swap), not a copy; retirement is deferred, never on the hot path.

**Unblocks.** Reconfiguration becomes the mechanism Phase 4 uses to grow the
bus and Phase 5 to carry new per-slot state across swaps.

---

## Phase 3 — Mix bus + engine migration (v3.3.0) — **Done**

**Intent.** Give the graph a real mixing node and migrate the engine's decode
loop onto the graph end-to-end.

**Key mechanisms.**
- `MixBusNode` (in `dsp/graph/nodes/mix/`): a two-input transition pair
  (outgoing/incoming) whose per-input pre-mix chains and crossfade envelope
  mirror `TrackMixer` — bit-exact by construction. The engine's single and
  crossfade decode paths now drive the graph (`process_block` /
  `process_block_inputs`) instead of `DspPipeline` + `TrackMixer`.

**Realtime discipline.** The pair envelope is embedded per-frame in the sum
loops; the post-mix chain runs once per block.

**Unblocks.** The bus is the place later phases bolt on musical behavior and
simultaneous streams.

---

## Phase 4 — Multi-track runtime (v3.4.0, v3.5.0) — **Done**

**Intent.** Turn the two-input bus into an N-slot mixing surface, give each
slot musical behavior, and let the engine play N independent tracks at once.

### S1+S2 (v3.4.0): parameterized slots + N-channel streams
- `EngineConfig::mix_slots` becomes a **generation parameter**: `MixBusNode::with_slots(N)`
  builds an N-slot bus; slots 0/1 stay the transition pair, slots ≥ 2 are
  independent lanes (`lane_preamp` / `lane_loudness` chains).
- Per-slot `SlotState` (gain / balance / mute / active) mirrored onto sticky
  per-slot atomics and replayed at swap (the Phase-2 discipline, per slot).
- Secondary slots carry **N-channel channel-major planes** with a
  channel-wise multichannel sum (`mix_multichannel`, `feed_secondary_slot_mc`);
  `nodes/mix/` split into `mod` / `envelope` / `sum`.

### S3–S6 (v3.5.0): pan, meters, ducking, automation, lanes
- **S3 — pan law + level meters.** Each slot gains `pan` (`[-1,1]`) + `PanLaw`
  (`Linear`/`EqualPower`/`Center`); `pan = 0` folds in as an exact ×1.0.
  Every slot computes per-block peak/RMS metering published to the control
  bus (`GraphControlHandle::slot_meters(slot)`).
- **S4 — program-gated ducking.** `DuckState` (source slot, threshold, depth,
  attack/release, up to 4 targets) rides the SPSC queue; the trigger is
  evaluated once per block from the source slot's peak meter and ramped over
  attack/release. Disabled = bit-exact.
- **S5 — automation tracks.** A slot carries one immutable track
  (`AutomationTarget::Gain | Pan`, up to 64 breakpoints) with sample-accurate
  linear interpolation and edge-hold; the cursor advances monotonically
  across blocks. No track = bit-exact.
- **S6 — engine lane registry.** `engine/lanes.rs`: `LaneTrack` (decoder +
  resampler + bounded FIFO) per independent stream on the first free slot ≥ 2.
  The decode loop fills each active lane every block and feeds it as a
  secondary (`process_block_lanes`; lanes ride after the incoming stream
  during crossfades); adding a lane grows the bus via the Phase-2 swap.
  Commands: `AddTrack` / `RemoveTrack` / `SetTrackGain` / `SetTrackPan` /
  `DuckTracks`. Telemetry: `PlaybackInfo.lanes: Vec<LaneInfo>` (slot, source,
  gain, pan, level, position, duration).

**Realtime discipline.** All four capabilities are per-frame or per-block
scalars folded into existing sums; disabled paths are skipped, never
reordered. Automation samples into fixed stack arrays; duck ramps are a
one-pole step; meters reuse the existing analyzer convention.

**Unblocks.** The meters, duck, automation, and lane registry are exactly the
substrate Phase 5 needs: sends are per-slot taps, the aux bus is meterable,
sends are automatable, and lanes are the send sources.

---

## Phase 5 — Per-slot trim, lane sends, aux/master busses, multi-endpoint fan-out — **Done (v3.6.0)**

**Intent.** Give the bus a real mixer topology: per-slot channel shaping,
per-slot post-fader sends, and a second (aux) bus that accumulates sends and
returns into the master — with an insert seam where global effects land in
Phase 6.

### S1 — per-slot channel trim
- Each `MixInput` gains a lightweight per-channel trim bank
  (`PerChannelTrim`: per-channel linear gain + polarity, default unity),
  applied on the slot's own channel-major planes between `loudness` and the
  sum. Slots needing a full routing matrix can opt into a per-slot
  `ChannelTrimmer` (its `process_planes(&mut [Vec<f32>], …)` signature is
  exactly the slot's `planes[..channels]`).
- `SetSlotTrim { slot, channel, gain_db, invert }` rides the SPSC queue and
  mirrors onto `SlotState` for swap survival; `EngineConfig.mix_trims`
  carries the generation config.
- **Disabled-exact:** unity trims skip the pass entirely.

### S2 — lane sends into the bus
- Per-slot `SlotSend { master_gain (default 1.0), aux_gain (default 0.0) }`:
  `master_gain` is the slot's master send level (0 = "sends-only" lane);
  `aux_gain` is a **post-fader tap** into the aux accumulator. Both fold into
  the existing per-frame product inside the sum loops — the tap reuses the
  slot contribution already computed, adding only the send multiplies.
- `AuxBus`: preallocated stereo planes, zeroed once per block, accumulated
  during the slot sums, then `master += aux * return_gain` after the sums
  (front pair on the MC path).
- `SetSend { slot, master_gain, aux_gain }` command; engine lane surface gains
  `SetTrackSend` / `SetTrackMasterGain`. **Automation on sends** is free:
  `AutomationTarget::Send` modulates `aux_gain` per frame through the
  existing S5 runner.
- **Disabled-exact:** `aux_gain = 0` skips the tap; `master_gain = 1.0` is an
  exact ×1.0; `aux_enabled = false` skips zeroing/return.

### S3 — aux/master sends as first-class topology
- Promote the accumulator into `nodes/mix/sends.rs` (house concern-split
  pattern): `AuxBus { planes, enabled, return_gain, insert: Option<…> }` —
  the **insert point** is the seam where a global convolution/reverb rides in
  during Phase 6 (ships as `None` with the wiring in place).
- **Aux metering:** the aux owns a `SlotMeters` and publishes via the
  existing meter path (`aux_meters()`), so the sub-mix is observable like any
  slot.
- **Aux ducking:** extend the `DuckState` address space with a reserved aux
  id so `source`/`targets` can gate the aux return.
- **Generation survival:** aux config + per-slot sends + trims are generation
  state via the mirror/replay discipline, so a live swap preserves the whole
  send topology.

**Realtime discipline.** One preallocated aux plane set, one memset per
block, O(sending slots × frames) accumulates, one return pass. No
allocation, no locks. Every new expression is either skipped when disabled
or an exact ×1.0, so the 27-scenario equivalence suite stays bit-exact (the
suite never enables trims/sends/aux).

### Multi-endpoint fan-out
Configured endpoints use independent bounded rings, channel-agnostic gain,
explicit drop accounting, lifecycle recovery, endpoint-specific telemetry,
and public `EngineHandle` configuration controls. The primary sink and each
secondary endpoint are independent subscribers; a short write on one never
changes the frames offered to another.

---

## Phase 5b — Multi-endpoint routing matrix — **Done (v3.7.0)**

**Intent.** Drive several output devices simultaneously, each with its own
rate domain, chain, and backend.

- `EngineConfig.endpoints: Vec<EndpointConfig>` (device, backend, enabled,
  per-endpoint gain, per-endpoint drift correction) — the primary device
  config is unchanged, so single-endpoint mode is bit-identical.
- `EndpointWorker` (`src/output/endpoint.rs`): each endpoint owns its
  lock-free ring + backend, a resampler from the master rate into its own
  rate domain (omitted when the rates match), and a rate-matched final
  safety limiter (applied to resampled frames only). The decode loop fans
  every master-domain block out to each endpoint (resample → drift trim →
  gain → ring), with partial-write preservation and a bounded pending
  queue (a stuck endpoint drops oldest frames, never grows memory).
- Lifecycle: `start()`/`stop()` open/close every endpoint; stream recovery
  reopens them against the new master rate; a failing secondary endpoint is
  logged and skipped — it can never take down the primary. Telemetry via
  `PlaybackInfo.endpoints` (device, rate, gain, pending frames, drift
  state).
- **Realtime discipline.** The fan-out runs on the decode loop (a control
  thread), never on a backend callback; each endpoint's realtime thread
  reads only its own ring. No allocation, no locks added to any hot path.
- **Clock drift (done, v3.9.0).** Independent devices drift; each endpoint's
  FFT resampler stays fixed at the nominal ratio (retuning it would hit
  rubato's fixed-sync chunk pathology at non-grid rates) and a rubato
  `Slip` — a 1:1 clutch that inserts/drops single frames behind a short
  crossfade — trims the stream to the device's actual clock. A
  proportional ring-fill controller steers the slip ratio (clamped ±500
  ppm) and converges it onto the real crystal; telemetry reports the ppm
  offset per endpoint.

---

## Phase 6 — Inserts & performance — **First cut done (v3.8.0)**

- **Aux inserts (done, v3.8.0):** the global convolution insert landed on the
  Phase 5 S3 `AuxBus.insert` seam — `AuxBusConfig.insert_*` (generation-
  carried), a runtime `set_aux_insert` / `aux_insert_state` control surface
  with the same mirror/replay discipline as the rest of the bus (a live
  toggle survives swaps; a missing IR stays bit-exact), and metering through
  the existing aux meters.
- **SIMD channel sums (done, v3.8.0):** the aux return accumulate is
  SSE2/NEON SIMD with a strict element-wise (no-FMA) bit-exact contract,
  locked by `phase6_bit_exact_simd_accumulate_matches_scalar` and the
  graph-vs-pipeline equivalence suite.
- **Aux as a plan node (done, v3.9.0):** promoted the aux out of `MixBusNode`
  into a standalone `AuxBusNode` running as its own `AUX` plan step with a
  shared interior-mutable `AuxSendBus`. Each mix slot's send is now an
  independent ramped gain (click-free per-send automation) with its own
  per-send peak meter (`aux_send_peak(slot)`); the master meter is measured
  at the post-aux / pre-post-mix-chain point so telemetry includes the aux
  return exactly as the pre-split layout did.
- **Horizon:** precompiled lane kernels, decode-ahead lane buffering — the
  meter/send/automation substrate from Phases 4–6 is the exact measurement
  and control surface those need.

---

## Phase 7 — Room & headphone correction pipeline (v3.10.0) — **Implemented S1–S5**

**Intent.** Turn the engine's existing parts — partitioned convolution,
EBU R128-grade measurement tooling, loopback capture (Windows) — into a
**measurement-to-correction pipeline**: play an exponential sine sweep through
the room or headphones, deconvolve the recording into an impulse response,
derive a regularized inverse against a target curve, render it into the
user's chosen phase mode, and run it as a first-class, disabled-exact plan
node ahead of the EQ chain. The portable path (IR file import, e.g. REW /
Dirac exports) works on every platform; the integrated live-measurement path
works wherever a capture input exists today (WASAPI loopback).

### S1 — sweep measurement kit — `dsp/correction/sweep.rs` (pure DSP, portable)

- Farina **exponential sine sweep** generator (configurable 10–60 s,
  20 Hz–24 kHz band, optional pre-emphasis) with its exact reference capture
  signal.
- **Deconvolution** of the recorded sweep (frequency-domain regularized
  inverse of the sweep spectrum) into a complex impulse response, with
  pre-delay/latency detection (peak search, sub-sample via phase slope).
- **Harmonic separation**: Farina's method places the 2nd/3rd/… harmonic
  impulses at known negative pre-delay offsets — time-gate them to report
  per-harmonic distortion and the measurement's usable SNR.
- Output: `Measurement { per-channel complex IR, sample rate, snr_db,
  harmonic_db, pre_delay }` — the raw truth before any interpretation.

### S2 — IR import & conditioning — `dsp/correction/ir.rs` (portable)

- Load WAV IRs (any channel count; per-channel extraction for multichannel
  rooms).
- Conditioning chain (control path only): DC/rumble high-pass, level
  normalization to a reference peak, **decay-tail truncation** (energy
  percentile windowing, configurable), sample-rate alignment between IR and
  session.
- The same conditioner runs on sweep-derived and imported IRs — one code
  path.

### S3 — phase machinery — `dsp/correction/phase.rs`

- **Minimum-phase** rendering via the cepstral (Hilbert of log-magnitude)
  method; **excess-phase** allpass extraction so min + excess ≡ original.
- **Linear-phase** rendering (symmetric IR, constant group delay) for
  purists who accept the latency.
- **Hybrid** rendering: minimal-phase below a crossover (bass keeps
  transient alignment), linear-phase above — implemented as the exact
  minimum-phase IR delayed by two crossover cycles, so the magnitude is
  bit-identical to the min render at every frequency while the group
  delay sits at ≈ τ₀ (two crossover cycles) where the correction is
  smooth; group-delay continuity holds exactly (GD_min + τ₀ everywhere).
- Frequency-dependent **time alignment** between channels (multiway
  speakers, distance delays folded into the IR rather than the routing
  matrix).

### S4 — correction derivation — `dsp/correction/derive.rs`

- Smooth the measured magnitude (log-domain octave-fraction smoothing),
  compare against the **target curve** (flat, tilt dB/octave, or shelf), and
  derive a **regularized inverse**: Wiener-style, weighted by the
  measurement's own SNR so boosts collapse where the measurement is
  unreliable.
- Hard safety rails: boost clamp (`max_boost_db`, default +6 dB) and the
  derived IR peak-normalized below digital full scale, so correction can
  never clip into the master limiter on its own.
- Render the result into the S3 phase mode per channel → the final
  correction IR set.

### S5 — engine integration

- **`CorrectionNode`** (`dsp/graph/nodes/correction_node.rs`): a per-channel
  bank of partitioned `ConvolutionEngine`s running as an `AllChannels`-scoped
  plan step placed **post-aux / pre-EQ** (`mix → aux → correction → eq → …`),
  so user EQ stacks on the corrected response and the node's declared latency
  flows through the Phase-1 capability metadata into
  `position_secs_compensated`.
- **Config** (`CorrectionConfig` in `crates/config/src/dsp_config.rs`,
  generation-carried like `AuxBusConfig`): `enabled`, per-channel IR paths,
  `phase_mode`, `hybrid_crossover_hz`, `target`, `max_boost_db`,
  `smoothing_octaves`, `depth` (0–1 wet/dry). Runtime commands
  `SetCorrectionEnabled` / `SetCorrectionDepth` / `LoadCorrectionIr` ride the
  SPSC control queue with the mirror/replay swap discipline (a live toggle
  and IR hot-load survive swaps; a missing IR stays bit-exact).
- **Measurement orchestration**: `MeasureRoom { seconds, pre_emphasis }`
  plays the S1 sweep on the primary endpoint while capturing (WASAPI loopback
  today; a generic input backend is Horizon), then runs S2–S4 on the control
  thread and lands the result as a correction IR. Progress/completion via
  `EngineEvent::MeasurementProgress` / `MeasurementComplete { path, snr_db }`.
- **Telemetry**: `PlaybackInfo.correction` (enabled, phase mode, IR length,
  added latency ms, per-channel max gain) — published with the existing
  ArcSwap snapshot.
- **C FFI**: `engine_set_correction_enabled`, `engine_set_correction_depth`,
  `engine_load_correction_ir`, `engine_correction_info` — status-code
  contract like the rest of `ffi.rs`.

### Acceptance tests — written before implementation

Each lands as a `[[test]]` entry under `tests/fidelity/`; the phase is not
**Done** until every threshold below is met by a committed suite (spec-first:
these names and thresholds are the contract the implementation is reviewed
against).

- **`tests/fidelity/ess_measurement.rs`** — S1 correctness:
  - a synthetic room (min-phase peaks/dips + pure delay) probed by the sweep
    is recovered within **±0.1 dB, 20 Hz–20 kHz**; delay recovered within
    **1 sample** @ 48 kHz;
  - injected 2nd/3rd-harmonic distortion is reported within **±1 dB** at the
    predicted pre-delay offsets;
  - an injected noise floor is estimated within **±2 dB** of truth.
- **`tests/fidelity/minimal_phase.rs`** — S3 correctness:
  - the min-phase render's magnitude matches its source within
    **±0.01 dB** full band; strictly causal support;
  - the excess-phase allpass is flat within **±0.001 dB**; min + excess ≡
    original (magnitude **±0.01 dB**, group delay **±1 sample**);
  - the linear-phase render has constant group delay (N−1)/2 ± 0.5 sample;
    the hybrid split keeps crossover group-delay continuity within
    **5 samples** and magnitude unchanged **±0.01 dB**.
- **`tests/fidelity/correction_inverse.rs`** — S4 correctness:
  - a synthetic ±6 dB room corrected to a flat target leaves a residual
    within **±0.5 dB, 40 Hz–16 kHz**;
  - where injected SNR < 10 dB the inverse clamps to `max_boost_db`; no
    NaN/Inf anywhere in the derived IR set;
  - tilt/shelf targets are honored within **±0.2 dB**.
- **`tests/fidelity/room_correction_pipeline.rs`** — S5 end-to-end through
  the graph:
  - pink noise through a corrected synthetic room → octave-band residual
    within **±0.5 dB, 40 Hz–16 kHz**;
  - **disabled = bit-exact**: plans without the correction step remain
    bit-identical to the frozen master (equivalence-suite discipline);
  - all three phase modes produce identical magnitude (**±0.01 dB**),
    differing only in phase/latency;
  - a live toggle and IR hot-load survive a generation swap with no NaN and
    no discontinuity beyond headroom;
  - the reported correction latency matches the IR group delay, and
    `position_secs_compensated` tracks a recorded WAV's content offset (the
    `transition_tails` method).
- **`tests/fidelity/realtime_allocation.rs`** — extended: the correction
  processing path allocates nothing after IR load (load/render are
  control-path); no locks added.
- **`benches/graph_plan_bench.rs`** — extended with correction on/off:
  disabled adds **zero** p50 cost; enabled at a 200 ms IR stays within
  **p50 < 2 ms per 1024-frame block @ 48 kHz** on the CI reference runner
  (criterion-tracked).

**Realtime discipline.** Sweep synthesis, deconvolution, phase rendering,
and inversion are control-thread DSP — they run once per measurement and are
heap-happy by design. The hot path sees only precomputed, per-channel
partitioned FFT state inside `CorrectionNode` — the same allocation-free
contract as the existing convolution node. Disabled = the plan step is
skipped, never reordered; missing IR = bit-exact passthrough.

**Unblocks (Horizon).** A generic input/capture backend brings integrated
measurement to Linux/macOS; per-device auto-load via `output_profile`;
correction-aware bass management (correcting the sub path against the
mains); AutoEQ headphone-target integration (S4's target curves are the
seam); extending the pipeline oracle with a correction stage for a
correction-vs-pipeline golden equivalence suite.

---

## Phase 8 — Spatial scene + basic panner (v3.11.0) — **Implemented**

**Intent.** Lay the speaker-independent spatial layer (the spec's central
rule: *objects describe content, channels describe the reproduction system*)
without touching the conventional PCM/DSP/output core. The spec's
multichannel foundation (semantic `ChannelId`/`ChannelLayout`, N-channel
buffers, LFE-as-effects-path, BS.775 matrices) already shipped, so this
phase adds **`crate::spatial`**: an opt-in, standalone scene model plus the
first renderer.

### Key mechanisms

- **`spatial::math`** — allocation-free `Vec3`/`Quat` with one documented
  coordinate system (`+X` right, `+Y` front, `+Z` up; azimuth 0 = front,
  +90° = right; metres/radians/linear). Deliberately dependency-light (no
  `glam`/`nalgebra`).
- **Scene model** — `SpatialScene` (listener + bounded object store),
  `Listener`/`ListenerTransform` (world-fixed objects move exactly opposite
  the listener yaw — the head-tracking seam), `SpatialAudioObject` with
  shareable `ObjectAudioRef`, and `SpatialSourceType` (Point/Extended/
  Diffuse/Bed seams). Beds/fields/room are documented future seams, so the
  scene needs no redesign when they land (§136).
- **Speaker geometry** — `Speaker`/`SpeakerLayout` (  named presets stereo / 5.1
  / 7.1 / 7.1.4 + arbitrary `custom`) and `LayoutCalibration` (per-speaker
  trim + time-align) applied separately from geometry (§19–20).
- **`BasicPanner`** — listener-space transform, azimuth-bracketing
  speaker-pair solve under the equal-power `la²+lb²=1` law, per-
  `(object,speaker)` coefficient smoothing (click-free region crossings),
  additive LFE send (never a pan target), simplified spread, and a broad
  `cos(elevation)` off-plane term. Writes into a caller-supplied interleaved
  buffer, so the steady-state hot path allocates nothing.
- **Renderer abstraction** — `SpatialRenderer` trait, `RendererKind`,
  typed `RenderError` (invalid/degenerate geometry surfaces as an error,
  never NaN). `ChannelPolicy::SpatialRender` is the opt-in flag.

**Realtime discipline.** Geometry is fully preprocessed in `prepare`;
`process_block` has no `Vec` growth and no locks (validated by
`realtime_allocation`). Disabled-exact by construction — the conventional
decode loop and the graph-vs-pipeline equivalence suite are untouched and
stay bit-exact.

**Acceptance (spec-first).** `tests/fidelity/spatial_panner.rs` covers
cardinal impulses, symmetry, continuity around a full circle (no NaN / no
jump), the narrow-band energy invariant at spread 0, listener rotation
preserving world-fixed objects, distance/elevation monotonicity, LFE
isolation, per-speaker calibration trim, custom layouts, and bounded air
absorption.

**Unblocks (Horizon).** The next spatial phases in the spec's dependency
order: 3D VBAP geometry (done — Phase 9), object behavior
(directivity/Doppler/occlusion), beds/fields, Ambisonics/HOA,
room/reflections, HRTF/SOFA/binaural, head tracking, scene file format, and
eventually a `SpatialNode` in the production graph. The scene/level/render
layer is the stable substrate those build on.

---

## Phase 9 — 3D VBAP renderer (v3.12.0) — **Implemented**

**Intent.** Give the spatial layer its first serious object-to-speaker
renderer: vector-based amplitude panning computed from actual speaker
geometry (spec Part V §25–29), with reduced-dimension handling for
coplanar layouts, a deterministic out-of-coverage fallback, and
preprocessed panning regions so coefficients are continuous under motion.
Same opt-in surface as Phase 8: hosts render a `SpatialScene` through
`VbapRenderer` into a normal interleaved multichannel buffer.

### Key mechanisms

- **`spatial::vbap::VbapRenderer`** — solves an object's listener-space
  direction as a non-negative combination of the surrounding speakers.
  `PanMode` classifies the layout at `prepare`: `ThreeDim` (≥1 valid
  triplet), `Planar` (coplanar speakers reduce to a 2D equal-power
  azimuth-pair ring, §26/§27), or `Single`.
- **Geometry preprocessing (§21)** — normalized speaker directions,
  loudspeaker-basis inverses for every valid triplet (adjugate/det,
  `|det| > ε` guards), the azimuth-pair ring, and a **Delaunay-style
  empty-triangle filter**: a triplet is a valid panning region only if no
  other speaker lies inside it or on its boundary. This is what makes the
  triangle set a proper tessellation (spec's "panning regions /
  convex-hull relationships") without pulling in a convex-hull library.
- **Coefficient solver** — among enclosing triplets, pick the most balanced
  (largest minimum coefficient; tightest-norm tie-break), then
  energy-normalize (constant power while moving). Full 3D placement: no
  `cos(elevation)` term (that was a BasicPanner azimuth-only hack).
- **Out-of-coverage fallback (§28)** — no enclosing triplet (below the
  floor, above the rig) → direction-preserving nearest-speaker fallback;
  deterministic, finite, never silent, never pollutes state.
- **Realtime discipline** — triangulation/inverse work is control-path;
  `process_block` reuses the panner's per-(object,speaker) smoothing,
  additive LFE send, and caller buffer: zero allocations (new
  `realtime_allocation` test).

**Correctness note (front-centre regression).** An over-complete triangle
enumeration places the 7.1.4 Centre speaker exactly on the FL–FR base edge
of `{FL, FR, height}`; a direction on that edge then snapped between a
phantom FL/FR pair and the real centre speaker (a knife-edge
discontinuity). The empty-triangle filter removes such triangles, so the
front axis renders through the real Centre speaker and gains move
continuously.

**Acceptance (spec-first).** `tests/fidelity/spatial_vbap.rs` covers layout
classification, energy preservation over a sphere sweep, left/right 3D
symmetry, overhead→height routing, the front-centre regression, out-of-
coverage determinism, degenerate coplanar geometry, custom asymmetric 3D
arrays, and a full-circle continuity sweep; plus the unit suite in
`src/spatial/vbap.rs`.

**Unblocks (Horizon).** Object behavior (done — Phase 10), beds/fields
hybrid mixing, Ambisonics/HOA, room/reflections, HRTF/SOFA/binaural, head
tracking, scene file format, and eventually a `SpatialNode` in the
production graph.

---

## Phase 10 — Object behavior: directivity, occlusion, spread (v3.13.0) — **Implemented**

**Intent.** Give objects the behavior of real sources: they can be
directional (a voice or a loudspeaker, spec §41), occluded by walls
(§43–44), and extended rather than points (§30). All three plug into the
renderer's level chain *before* panning, reuse the existing per-path
smoothing, and stay allocation-free. Defaults are unchanged — existing
scenes render bit-identically.

### Key mechanisms

- **`spatial::directivity`** — [`Directivity`]
  (Omnidirectional/Cardioid/Supercardioid/Custom) evaluated at the
  documented angle (0 = facing the listener, π = away) via the shared
  `listener_angle_rad` transform `q_source⁻¹ ∘ q_listener`. `CustomDirectivity`
  is a fixed 91-sample 2° curve with linear interpolation — stack-copied,
  no allocation. Objects gain `source_orientation` (+Y = facing).
- **`spatial::occlusion`** — [`Occlusion`] (amount + max attenuation +
  min cutoff) → structured [`AcousticTransmission`] (`attenuation_db` /
  `cutoff_hz` / `diffusion`, with diffusion as the §44 seam). Cutoff is
  exponential in log-frequency; the gain plus a real Butterworth biquad
  (Q = 0.707, block-rate coefficients, smoothed cutoff, per-object state)
  makes occluded sources quieter *and* duller, feeding pan paths and the
  LFE send (§43).
- **`spatial::spread`** — the spec's angular-region recipe (§30): one solve
  on the exact direction (weight `1−s`) + 3 ring samples at `s × 60°`
  (weight `s/3`), aggregated by speaker and energy-normalized (§29). Fixed
  sample count (4 solves / 12 entries) — deterministic and allocation-free.
  Replaces the nearest-speaker widening in both `BasicPanner` and
  `VbapRenderer`.

**Correctness note (latent fix).** The VBAP coplanar reduction stored
*output* channel indices in its azimuth-pair table but indexed `self.pan`
with them — out of bounds on any layout with an LFE gap (5.1/7.1). Latent
because prior VBAP tests used only stereo/custom layouts. The pair table now
stores pan-slot positions; the new 5.1 acceptance tests pin it.

**Acceptance (spec-first).** `tests/fidelity/spatial_object_behavior.rs`
— cardioid and custom-curve routing, omni-default regression guard,
occlusion attenuation + real low-pass measured on analytic sines
(10 kHz dies ≫ 100 Hz), monotonicity, bounded transmission, spread widening
(concentration index) with energy preservation, stereo symmetry of a
centered spread source, spread-sweep continuity, and composition; plus unit
suites in the three new modules and a `realtime_allocation` test running all
behaviors together.

**Unblocks (Horizon).** Beds/fields hybrid mixing (done — Phase 11),
Ambisonics/HOA, room/reflections (occlusion's `AcousticTransmission` is the
transmission seam), HRTF/SOFA/binaural, Doppler (per-object `velocity` +
`source_orientation` are the seams), head tracking, scene file format, and
eventually a `SpatialNode` in the production graph.

---

## Phase 11 — Hybrid beds & fields (v3.14.0) — **Implemented**

**Intent.** Complete the scene's three content classes (spec §13): objects
(point/extended), beds (channel-based), and fields (diffuse). A `SpatialScene`
now carries all three, and the renderers mix them through one
`process_hybrid_block` into a single interleaved buffer — the spec's spatial
mixer (§37) — while the object-only path and the trait's default hybrid
behavior keep every existing caller unchanged.

### Key mechanisms

- **`spatial::bed`** — [`SpatialBed`] is authored content with a semantic
  [`ChannelLayout`] (roles cached once at construction, control path).
  Routing is by **semantic role** onto the matching output speaker
  (`render_beds`), with calibration trim and the authored LFE channel;
  unmatched channels drop cleanly. Bounded store, stable [`BedId`]s (≤ 16).
  Full BS.775 rematrixing stays the conventional PCM path's job — the bed
  path routes by identity.
- **`spatial::field`** — [`SpatialField`] is a positionless diffuse source:
  encoded into the ambisonic bus (Phase 12) and decoded onto every pan
  speaker with the `√N` diffuse compensation — equal power (`1/√N`),
  decorrelated per speaker through a deterministic 2.0–10.25 ms delay
  (distinct for ≤ 12 speakers), LFE never involved.
  [`AmbisonicFieldMixer`] owns the preallocated per-block bus scratch and
  per-speaker rings; per-block work is a fixed stack-array plane list +
  one encode + one decode + one delayed read per speaker.
- **Hybrid mixer** — `SpatialRenderer::process_hybrid_block(scene,
  &HybridBlockInputs { objects, beds, fields }, frames, out)`; the trait
  default forwards to `process_block`, `process_block` delegates to the
  hybrid path with empty bed/field planes, and both `BasicPanner` and
  `VbapRenderer` implement the full mix (objects → beds → fields).

**Realtime discipline.** Bed routing is a linear scan of the small role
table; the field mixer never allocates (verified by the new
`realtime_allocation` hybrid test running objects + beds + fields, 10k
blocks, 0 allocs).

**Acceptance (spec-first).** `tests/fidelity/spatial_hybrid.rs` — 5.1 bed
routing by semantic role, 7.1-bed-on-5.1 dropping, field equal-power spread
with distinct decorrelation arrivals and silent LFE, deterministic finite
objects+beds+fields mixing, missing-plane tolerance, and the panner's
hybrid mix; plus unit suites in `bed.rs`/`field.rs`.

**Unblocks (Horizon).** Ambisonics/HOA (done — Phase 12), room/reflections
(occlusion's `AcousticTransmission` is the transmission seam),
HRTF/SOFA/binaural, Doppler (per-object `velocity` + `source_orientation`
are the seams), head tracking, scene file format, and eventually a
`SpatialNode` in the production graph.

## Phase 12 — Ambisonics / HOA (v3.15.0) — **Implemented**

**Intent.** Give the spatial layer a real sound-field representation (spec
Part VI §32–37, §55): a direction-independent **ambisonic bus** that any
spatial source encodes into and any speaker layout decodes from, so the same
encoded field renders to stereo, 5.1, 7.1.4, or a custom array without
re-authoring. The diffuse-field path (Phase 11) is upgraded to ride the
encode → bus → decode pipeline, and a standalone `AmbisonicRenderer`
services hosts that want to feed a FOA bus directly.

### Key mechanisms

- **FOA math core** (`spatial::ambisonic`) — documented conventions: ACN
  ordering `[W, Y, Z, X]`, SN3D normalization (`W = 1`, first order =
  `√3` × direction), real SH basis (`sh_foa`), plane-wave encoder
  (`encode_plane_wave`, defensive normalization — a zero direction encodes
  silence, never NaN), and order-1 bus rotation (`rotate_bus_frame`: `W`
  invariant, `X Y Z` rotate like direction vectors, §34).
- **Decoder** (`AmbisonicDecoder`) — `DecoderPolicy::Basic` is the sampling
  decoder `D = Y(S)ᵀ/N` (a plane wave from `d` lands on speaker `s` as
  `(1 + 3·cosθ)/N`); `DecoderPolicy::MaxRe` applies the documented FOA
  `a1 = √3/2` weights to narrow the lobe. `prepare` builds the per-speaker
  matrix and rejects empty / LFE-only layouts; `process_bus` (and the
  per-frame `decode_frame`) are allocation-free.
- **`AmbisonicRenderer`** (§23) — decodes a 4-plane `[W, Y, Z, X]` bus via
  `process_block`, applying the listener orientation per frame (world-
  encoded fields stay world-fixed, §48) and per-speaker calibration.
  `RendererKind::Ambisonic`.
- **Field-path upgrade** — `AmbisonicFieldMixer` replaces
  `DiffuseFieldMixer`: field → encoder (`W` only) → bus → decoder →
  per-speaker decorrelation rings. The `√N` **diffuse compensation** (the
  sampling decoder maps unit `W` to `1/N` per speaker, energy `1/N`; a
  diffuse field must decode at unit energy, so `W` is boosted by `√N`)
  keeps the Phase-11 equal-power `1/√N` behavior and unit energy exactly.

**Realtime discipline.** The renderer's per-frame rotation and the
decode matrix run on preallocated scratch; the field mixer's bus scratch
and rings are preallocated at `prepare` — new `realtime_allocation` test
(10k blocks, rotating listener, MaxRe, 7.1.4, 0 allocs).

**Acceptance (spec-first).** `tests/fidelity/spatial_ambisonic.rs` —
SN3D/ACN convention pins, plane-wave round-trip to stereo / 5.1 / 7.1.4
from one bus (speaker independence), max-rE lobe narrowing, world-fixed
listener rotation, W-only equal-power decode with silent LFE, a 720-step
rotation continuity sweep, and determinism / unprepared-use rejection; plus
unit suites in `ambisonic.rs` / `field.rs`.

**Unblocks (Horizon).** Higher orders (order-N SH basis + per-order decoder
weights + full rotation — the documented §34 extension), room/reflections
(done — Phase 13), HRTF/SOFA/binaural, head tracking, scene file format, and
eventually a `SpatialNode` in the production graph.

## Phase 13 — Room acoustics: reflections + late field (v3.16.0) — **Implemented**

**Intent.** Give the scene an acoustic space (spec §49): participating
objects become small acoustic events — image-source early reflections plus
a Schroeder late field whose output **encodes into the ambisonic bus** (§55)
and decodes as a diffuse source. Occlusion's `AcousticTransmission` is the
transmission seam (§43–44). Opt-in at both levels: `Room::default()` is
disabled (bit-exact) and objects participate via their `room_send` — the
seam the scene model declared at Phase 8.

### Key mechanisms

- **`spatial::room::Room`** — axis-aligned box (width/depth/height), one
  wall absorption coefficient (per-wall is a documented seam), reflection
  order (1 | 2), late RT60, late wet mix. Stored on `SpatialScene`.
- **Image sources** (`image_sources`) — BFS reflection across the six
  walls with deduplication: order 1 → 6 images, order 2 → 24 distinct
  (two crossings on one axis, one each on two). Each image carries the
  product of the crossed walls' `(1 − absorption)` coefficients.
- **Early reflections** (`EarlyReflections`) — per-object delay rings
  (≈171 ms), a per-(object, image, speaker) smoothed tap matrix, and a
  room-send accumulator. Each renderer solves every image with its **own**
  pan machinery (equal-power pairs / 3-triplet VBAP), applies distance
  attenuation and the coefficient, and delays the tap by the excess path
  `(dist_image − dist_direct)/c`. The same low-passed (occluded) sample
  that feeds the direct path feeds the reflections. LFE never participates.
- **Late field** (`RoomLateField`) — Schroeder tail (4 parallel combs with
  gains derived exactly from RT60, 2 serial allpasses, `(1 − g)`
  normalization for boundedness) fed by the accumulated room send, then
  `AmbisonicFieldMixer::render_extra`: encode → bus → `√N`-compensated
  decode → per-speaker decorrelation (§55).

**Realtime discipline.** Image enumeration is per-object per-block pure
arithmetic into fixed stack arrays; rings, tap matrix, and tail buffers are
preallocated at `prepare` — the per-frame hot path is one ring store + one
delayed read per tap. New `realtime_allocation` test: order-2 worst case
(24 images/object, occluded + directional objects, wet 0.7), 10k blocks,
0 allocs.

**Acceptance (spec-first).** `tests/fidelity/spatial_room.rs` — the
predicted 280-sample reflection arrival on both renderers (2 m excess path
÷ 343 m/s), coefficient-exact tap amplitudes, absorption monotonicity,
order-2 energy growth, room-disabled bit-exact restoration, dry-object
bit-exactness, diffuse LFE-free late field over 12 blocks, `late_mix`
monotonicity, and deterministic finite hybrid rendering; plus unit suites
in `room.rs` / `field.rs` (including an RT60 measurement of the tail within
±15%).

**Unblocks (Horizon).** Higher orders, HRTF/SOFA/binaural (done —
Phase 14), head tracking, scene file format, and eventually a `SpatialNode`
in the production graph.

## Phase 14 — Binaural rendering: head model + HRTF cues (v3.17.0) — **Implemented**

**Intent.** Render the *whole hybrid scene* — objects, beds, fields, and
room — straight to headphones (spec Part VII §47–48, §62, §136) with a
real head model instead of a speaker array: the Woodworth interaural time
difference plus a Duda-Martens head-shadow shelf. The output is always two
ears; the "pan" *is* the head.

### Key mechanisms

- **`spatial::hrtf`** — the documented head model: `woodworth_itd_sec`
  (`(a/c)(sinθ + θ)` to the ear axis, `(a/c)(π − θ + sinθ)` behind, zero
  again straight back — the front/back cone ambiguity), `head_shadow_alpha`
  (`α = 1.05 + 0.95·sinφ`, ≈ 2.0 at the ear, ≈ 0.1 shadowed), a
  first-order `HeadShadow` shelf (bilinear Duda-Martens; DC gain exactly
  1, HF asymptote exactly α, block-smoothed α), and the fractional
  `read_delayed` ring read that makes ITD changes glide as sources move.
- **`spatial::binaural::BinauralRenderer`** — every content class through
  the head model:
  - *Objects* — the shared level chain (distance · directivity ·
    occlusion) then two ear paths: contralateral delay (fractional ITD
    ring) + per-(direction, ear) shadow shelf. LFE send folds at `1/√2`.
    Spread samples the exact + ring directions, each with its own cues —
    widening blurs the interaural cues instead of moving the image.
  - *Beds* — semantic-role fold: FL → −30° cues, SL → −110°, …; the LFE
    role folds at `1/√2` to both ears.
  - *Fields + room late field* — diffuse content decodes onto a **virtual
    8-speaker ring** (ambisonic bus + `√N` compensation + the field
    mixer's per-speaker decorrelation), then each virtual speaker is
    head-modeled — surrounding ambience, not a phantom.
  - *Room* — image sources are binauralized: each ear hears a reflection
    at `excess_path + ITD(ear)` through its own (image, ear) shelf and
    smoothed tap gain.
- `RendererKind::Binaural` joins `Basic` / `Vbap` / `Ambisonic` (the
  `HrtfUnavailable` error variant was already declared); `prepare` requires
  exactly two enabled non-LFE speakers (stereo/headphone layouts).

**Realtime discipline.** All per-path state (per-object ITD rings, shadow
shelves, reflection tap gains, the virtual ring's delay lines) is
preallocated flat at `prepare`; the per-frame hot path is bounded
arithmetic. New `realtime_allocation` test: order-2 room, occluded +
cardioid/custom + spread + LFE-send objects, a bed, a field, and a
sweeping listener yaw — 10k blocks, 0 allocs.

**Acceptance (spec-first).** `tests/fidelity/spatial_binaural.rs` — the
Woodworth closed form at the public API, front-center unity and balance,
hard-right ITD + head shadow measured on impulse argmax, exact ear swap
under mirror symmetry, bed role/LFE folds, diffuse equal-ear-energy fields
with decorrelation, the 280-sample room reflection arriving `ITD` later at
the contralateral ear, listener-rotation image motion, spread's effective
ITD shrinkage, and deterministic finite full-hybrid rendering; plus unit
suites in `hrtf.rs` / `binaural.rs` (shelf DC/Nyquist/α=1 pins,
fractional-delay interpolation, spread centroid metric).

**Unblocks (Horizon).** Higher orders (done — Phase 16), full spectral
HRTFs (done — Phase 18), head tracking (done — Phase 15), scene file
format (done — Phase 19), and the `SpatialNode` in the production graph
(done — Phase 17).

## Phase 15 — Head tracking (v3.18.0) — **Implemented**

**Intent.** Close the VR/AR loop (spec §48, §136): feed the scene a live
stream of head-orientation samples and let the audio follow the head. The
renderers never change — the listener's orientation was already a
first-class scene transform — so tracking is a **control-side**
interpolation + smoothing problem.

### Key mechanisms

- **`spatial::tracking::HeadTracker`** — `push(HeadSample)` ingests
  timestamped orientations (IMU / VR rig / mock); `sample(now)` returns
  the smoothed current orientation; `apply_to(&mut listener, now)` is the
  per-block host convenience. The pipeline is documented: shortest-path
  **nlerp** across the last two samples (a pure yaw sweep glides, never
  snaps; the latest sample is held when no new one has arrived) → an
  **exponential one-pole** on the orientation error (`smoothing_ms`;
  `0` snaps exactly) → an optional **rate limit** (`max_angular_rate_deg_s`
  clamps the per-step angle, so a glitch cannot fling the soundfield).
- **`Quat` interpolation math** (`math.rs`): `nlerp` (shortest path,
  normalized), `angle_to` (`2·acos(|dot|)`), `dot`, `normalized`,
  `negated`, `length`, `is_finite`, `Add`/`Mul<f32>` — unit-tested.

**Realtime discipline.** The tracker is fixed-size state — `push` and
`sample` allocate nothing and take no locks, so a host may run them on the
audio thread's caller; the renderers themselves stay untouched and
lock-free. New `realtime_allocation` test: a 10k-sample jittery stream with
block-rate sampling, 0 allocs.

**Acceptance (spec-first).** `tests/fidelity/spatial_tracking.rs` — the
headline Woodworth consistency: a 137° tracked yaw sweep renders a
world-fixed source with the closed-form `itd(az, L) − itd(az, R)` ear lag
at *every* block; the frozen-image contrast (the same sweep without
applying the tracker leaves the image at +90° while the head has turned
90°); smoothing gliding the ear lag without zipper and converging; a 5.1
panner moving the image from the side pair to front/center as the head
turns; tracker determinism + rate-limit capping; and `apply_to` updating
the listener (pinned via the ITD). Plus unit suites in `tracking.rs` /
`math.rs`.

**Unblocks (Horizon).** All four previously-horizon phases are now
implemented: higher-order ambisonics (Phase 16), the `SpatialNode` in the
production graph (Phase 17), full spectral HRTFs (Phase 18), and the
scene-file format (Phase 19).

## Phase 16 — Higher-order ambisonics (v3.19.0) — **Implemented**

**Intent.** Extend the order-1 ambisonic bus to order-2 per the spec's §34
table: the exact order-N spherical-harmonic basis, a channel table matching
the published Furse–Malham ordering, per-order max-rE decoder weights, and
an exact order-2 bus rotation — while keeping order-1 behavior bit-for-bit
identical to the previous release.

### Key mechanisms

- **`sh_n(order, dir, out)` / `channel_count(order)`** — the exact SH basis:
  order-1 `[W, Y, Z, X]` (pinned FOA) plus order-2 `[U, V, T, R, S]` per
  the published table. `encode_plane_wave_n` and `rotate_bus_frame_n`
  generalize the FOA encoders.
- **Per-order decoder weights** — the published max-rE window: order-2
  `a1 ≈ 0.9057`, `a2 ≈ 0.6827`; order-1 stays `√3/2`. `AmbisonicDecoder`
  was already weight-parameterized, so order-2 speakers decode with the
  same normalization and `√N` compensation.
- **Exact order-2 rotation** — the WXYZ block rotates as one (as before),
  and the 5 new channels rotate by the closed-form order-2 matrices
  (W and X interleave with U/V; Y with T/S; Z with R) — a 90° yaw moves a
  plane wave to the correct column, pinned by a dedicated test.
- **`AmbisonicRenderer::with_order(policy, order)`** — renders any
  supported order (≤ `MAX_AMBISONIC_ORDER` = 2) to any speaker layout,
  zero allocations on the audio thread.

**Acceptance (spec-first).** `tests/fidelity/spatial_hoa.rs` — order-2
rendering to 7.1.4 (channel activity + energy), the exact rotation
property, per-order weight checks. Unit suite in `ambisonic.rs` (basis
orthonormality, channel table, weights, rotation, order-2 renderer). New
`realtime_allocation` test: order-2 with per-frame exact rotation, 0
allocs.

**Unblocks (Horizon).** Order 3 is implemented (v3.20.0 — the cubic
coefficient projection extends the same pattern); the next step is
higher-order *spatial* recording via the order-3 encoder, and beyond that
order 4+ (the form-tensor projection generalizes to any degree).

## Phase 17 — SpatialNode in the production graph (v3.19.0) — **Implemented**

**Intent.** Make the spatial layer a first-class part of the DspGraph hot
path: a `SpatialNode` plan step that spatializes the master's front pair
through the binaural head model (optionally with the room), controlled
through the same command/atom/swap machinery as every other graph node.

### Key mechanisms

- **`nodes/spatial_node.rs`** — a graph arena slot (with `DspNode` variant
  and plan step) that renders stereo masters binaurally; multichannel
  masters pass through untouched (documented seam). The renderer is
  preallocated flat at construction/prepare — the plan step allocates
  nothing.
- **Control surface** — `set_spatial_enabled`, `set_spatial_screen`
  (yaw/pitch/width/gain), `set_spatial_room` (enabled + dimensions +
  absorption + order + RT60 + late mix), `set_spatial_listener` (yaw /
  pitch / roll): per-node atomic control mirror, drained and applied at
  block boundaries.
- **Config section** — `SpatialConfig` + `SpatialRoomConfig` in
  `engine_config`, wired through `EngineConfig`.
- **Swap replay** — the enabled state survives generation rebuilds (the
  node's live state is replayed on swap, like the aux/convolver nodes).

**Acceptance (spec-first).** `tests/fidelity/spatial_node.rs` — bit-exact
passthrough when disabled (the equivalence suites depend on this), binaural
ITD on the graph output once enabled, room tail beyond the direct, listener
yaw moving the screen across the ears, the full control surface at block
boundaries, reconfig survival, and the MC seam. New `realtime_allocation`
test: SpatialNode + room under a block-rate listener sweep, 0 allocs.

**Unblocks (Horizon).** Spatialization of non-front channels / higher
output counts through the node, and object-metadata routing from the
decoder into the node's scene.

## Phase 18 — Spectral HRTF / elevation (v3.19.0) — **Implemented**

**Intent.** Replace (or complement) the analytic head model with measured
spectral HRTFs: a grid of per-ear impulse responses with bilinear
interpolation, so the renderer carries real direction-dependent spectral
cues — elevation included — while the analytic path gains a documented
pinna-notch model as the fallback.

### Key mechanisms

- **`HrtfDataset`** — azimuth × elevation grid of per-ear IRs, validated
  on the control path (`from_planes` rejects non-monotonic grids and
  non-finite IRs), bilinear interpolation with the azimuth wrapped
  continuously across the 360° seam, allocation-free writes into
  caller-provided scratch. `synthetic()` discretizes the analytic model on
  a regular grid so the FIR path is testable without a measured corpus.
- **`BinauralRenderer::use_dataset`** — with a dataset loaded, object
  direct paths switch from the analytic chain to FIR convolution of the
  interpolated IR (which carries both the ITD and the spectral cues),
  hoisted per block into preallocated per-(direction, ear) buffers.
- **`ElevationNotch`** — the analytic fallback: a pinna-notch biquad whose
  center rises with elevation (`6 kHz + 4 kHz·sin(el)`, depth
  `−8 dB·|sin(el)|`), an exact passthrough at 0°.

### Fixed

- **Woodworth ITD folding for wrapped azimuths** — angles past ±π folded
  by reflection (300° → 60°, not 180°) and the ear-side test uses
  `sin(azimuth)`: a 0–360° grid (the dataset convention) now renders
  correctly. The renderer's signed azimuths were unaffected; new unit
  tests pin the fold.

**Acceptance (spec-first).** `tests/fidelity/spatial_hrtf_ir.rs` — the FIR
path reproduces the dataset IR exactly, mirror symmetry holds in both
paths (a real bug caught: the dataset's wrapped-angle IRs were not
mirrors), elevation notches in both paths, determinism. Unit suites in
`hrtf.rs` / `binaural.rs`. New `realtime_allocation` test: dataset path +
worst-case order-2 room, 0 allocs.

**Unblocks (Horizon).** Measured corpus loading (done — Phase 20), and
optional minimum-phase conversion of the IRs to shrink the FIR length.

## Phase 19 — Scene file format (v3.19.0) — **Implemented**

**Intent.** A Serde-serializable, renderer-independent scene model so hosts
can persist, exchange and version spatial scenes — the spec's Part XXVI —
with the renderer and output layout staying host choices.

### Key mechanisms

- **`config::SpatialSceneConfig`** (+ `SceneListenerConfig`,
  `SpatialObjectConfig`, `SpatialBedConfig`, `SpatialFieldConfig`) — the
  scene as data: listener (position + canonical orientation quaternion),
  objects (gain / spread / room_send / lfe_send), beds by semantic role
  names (`"FL"`, `"C"`, …), fields, and the room. Every optional field
  defaults (`#[serde(default)]`) so older hosts keep reading newer files.
- **Conversions** — `SpatialScene::from_config` / `to_config` are lossless
  (the listener orientation stays the quaternion — no Euler round-trip
  drift) and enforce the engine caps with typed errors.
- **File I/O** — `save_scene_json` / `load_scene_json` with typed
  `SceneFileError`s (io / json / scene); `SpatialSceneConfig::validate`
  gives rich messages before conversion.

**Acceptance (spec-first).** `tests/fidelity/spatial_scene.rs` — a
save/load round-trip of a rich scene renders **bit-identical** through the
binaural renderer; minimal JSON defaults forward-compatibly; unknown role
names, over-capacity classes, out-of-range gains and non-finite positions
are rejected; the same file renders through both the head model and a VBAP
array; and the listener quaternion survives exactly.

**Unblocks (Horizon).** Versioned file headers, binary scene archives, and
host tooling (scene editors) on top of the model.

## Phase 20 — Measured HRTF corpus loading (v3.21.0) — **Implemented**

**Intent.** Replace the synthetic grid with measured data: load a real
HRTF corpus (SOFA-style) into `HrtfDataset` so the binaural path renders
true recorded head-related impulse responses.

### Key mechanisms

- **`HrtfCorpus` / `HrtfMeasurement`** — the SOFA data model reduced to
  pure Rust: measurement directions (unit `[x, y, z]` in the layer
  convention) each paired with left/right IRs plus the recorded sample
  rate and optional provenance. This is exactly what a `.sofa` HDF5
  export (`SourcePosition` + `Data.IR` + `Data.SamplingRate`) reduces to;
  the engine deliberately avoids HDF5 bindings (not pure Rust).
- **`HrtfDataset::from_corpus`** — validates the corpus (finite unit
  directions, non-finite IRs rejected, tap range), resamples every IR to
  the target rate by piecewise-linear interpolation, peak-normalizes
  (optional), trims/pads to the FIR tap count, and requires a **regular**
  azimuth × elevation mesh (a full Cartesian product of the present
  values) so the renderer's bilinear interpolation stays exact; an
  irregular mesh is a typed error, never a silently-wrong grid.
- **JSON corpus I/O** — `save_hrtf_corpus_json` / `load_hrtf_corpus_json`:
  a portable, pure-Rust interchange form of the SOFA data model, so hosts
  and converters can feed measured corpora without binary assets.

**Acceptance (spec-first).** `tests/fidelity/spatial_hrtf_ir.rs` — a
measured-style 96 kHz corpus (resampled, peak-normalized, 2×2 grid)
renders through the binaural FIR path sample-for-sample equal to the
loaded IR at grid points. Unit suites in `hrtf.rs` cover resampling,
peak normalization, irregular-mesh rejection, and the JSON round-trip.

**Unblocks (Horizon).** Optional minimum-phase conversion of the IRs to
shrink the FIR length, and per-corpus tap budgeting on `use_dataset`.

## Phase 21 — Scene persistence in the engine lifecycle (v3.22.0) — **Implemented**

**Intent.** The active spatial scene survives across sessions: the engine
auto-saves the graph's spatial state and restores it at construction, so
a host's spatial tuning is not lost on restart.

### Key mechanisms

- **`engine::spatial_persistence`** — a control-path concern
  (`SpatialPersistence`) that snapshots the `SpatialNode` surface
  (enable, screen, room, listener) into the existing
  `config::SpatialConfig` serde model — the same model the graph
  configures from — so no new file format is needed. Writes are atomic
  (temp + rename): a crash mid-write can never corrupt the last good
  scene.
- **Lifecycle hooks** — restore runs at engine construction (best-effort:
  a missing/corrupt file keeps the config defaults; construction never
  fails). `maybe_save` runs once per engine tick *after* queued graph
  controls are applied and writes only when the state changed (plain
  field compare on the steady path). `Drop` flushes pending controls and
  persists the final scene, so a graceful shutdown always restores
  exactly what was active.
- **`EngineConfig::spatial_autosave_path`** — hosts can point the
  auto-save at their own file (default: `<user-data>/engine/
  spatial_scene.json`).

**Acceptance (spec-first).** Unit suites in `spatial_persistence.rs` pin
the snapshot round-trip, write-on-change, and best-effort restore.
`src/engine/tests/spatial_persistence.rs` exercises the full lifecycle:
a scene survives an engine restart, drop persists without a tick, and a
missing auto-save is a no-op.

**Unblocks (Horizon).** Versioned auto-save files, per-endpoint scene
layouts, and host tooling on top of the restored scene.

## Phase 22 — Optimization (v3.23.0) — **Implemented**

**Intent.** The final phase: cut the spatial renderer's avoidable hot-path
cost. Every Phase-8–21 capability must stay exactly as verified — the
phase's non-negotiables are **bit-exactness** (the equivalence suites stay
pinned) and **zero allocation** (the realtime suites stay green).

### Key mechanisms

- **Per-block geometry hoisting in `BinauralRenderer::render_objects`** —
  the analytic ITD delay (`ear_delay_sec` → `sin` + Woodworth) and the
  room images' ITD are pure functions of the block's direction/ear pairs,
  yet were recomputed once per *frame*. They now live in per-block
  tables (`dly`, `ref_itd`); `azimuth_rad` is no longer called per frame
  in the FIR path either. Identical values, moved call sites.
- **Modulo-free FIR ring reads** — the dataset convolution read its
  `taps`-sample window with `% fir_len` per tap. The window wraps at most
  once (`taps < fir_len`), so a descending index with one wrap branch
  replaces the per-tap division; the tap order — and therefore the
  accumulation — is unchanged.
- **Increment-and-wrap ring cursors** — `(pos + frame) % len` per frame
  becomes a running cursor with a wrap check.
- **`read_delayed` one modulo** — `b` is one ring slot behind `a`, so the
  second modulo becomes a branch.

### Measured (criterion, `benches/spatial_bench.rs`, 4 objects × 1024
frames @ 48 kHz, after vs before)

| Path | Before | After | Speedup |
|---|---|---|---|
| Binaural FIR (64-tap dataset) | 4.35 ms | 1.52 ms | **2.9×** |
| Binaural analytic | 701 µs | 324 µs | **2.2×** |
| Binaural analytic + room | 1.63 ms | 964 µs | **1.7×** |
| Graph SpatialNode (512-block) | 128 µs | 68 µs | **1.9×** |
| VBAP 5.1 (untouched) | 61 µs | 53 µs | — |

**Acceptance (spec-first).** All Phase-8–21 suites unchanged and green:
the `graph_pipeline_equivalence` bit-exact suites, every spatial
acceptance suite (`spatial_binaural`, `spatial_hrtf_ir`, `spatial_scene`,
`spatial_render`, …), and `realtime_allocation` (0 allocs on the
optimized paths). `spatial_bench` pins the numbers going forward.

**Unblocks (Horizon).** SIMD (portable `std::simd` on stable) for the FIR
convolution and mix sum, and per-architecture tuning of the renderer
inner loops.

---

## Phase 23 — Acoustic world simulation (v3.25.0) — **Implemented**

**Intent.** The guide's directive for v3.25: **separate acoustic simulation
from acoustic rendering.** Everything before this phase either rendered an
audio *signal* (direct path, reflections, binaural) or dressed a scalar
parameter (the old `Room::absorption`) — but there was no *simulation* layer
that owns a space's acoustic identity and computes how sound propagates
through it. This phase adds that: a new `spatial::acoustic` module turns a
goal description of a space — walls with **frequency-dependent materials**,
**portals** (openings), and **diffraction edges** — into a concrete set of
[`AcousticPath`]s a renderer consumes. The renderers never re-derive
propagation; they place, filter and attenuate paths.

### Key mechanisms

- **`material`** — [`MaterialSpectrum`]: per-ISO-octave-band (63 Hz–16 kHz)
  absorption / specular reflection / transmission, log-frequency
  interpolation, a `broadband` reduction (geometric-mean gain + −3 dB
  low-pass corner) for the realtime renderers, and the guide's Direction-8
  presets ([`MaterialKind`]: Concrete / Wood / Glass / Fabric / Carpet /
  Metal / OpenMesh). A wall no longer has one number.
- **`geometry`** — [`AcousticRoom`] (axis-aligned box with **per-wall**
  materials — the seam the old `Room::absorption` documented), [`Portal`]
  (an opening in a wall coupling two spaces, with its own transmissive
  material), [`DiffractionEdge`] (freestanding fin/mullion), doorway jamb
  edges. Pure control-path data.
- **`path`** — [`AcousticPath`] is the simulation→render contract, exactly
  the guide's shape: `kind, direction (from the listener), distance,
  delay_samples, gain, lowpass_hz, flags, interacting wall`. [`PathKind`]
  (Direct / Reflected / Diffracted / Transmitted / Diffuse) and
  [`PathFlags`] (spectral-collapse / crosses-boundary) tell a renderer how
  to handle each path.
- **`solver`** — [`AcousticWorld`] owns the geometry; `solve(source,
  listener)` enumerates the path set: the **direct** path; **image-source
  reflections** (order 1 → 6, order 2 → 24, mirroring the renderer's
  `room::image_sources` geometry so the excess-path delays agree to half a
  sample); **wedge diffraction** around each portal jamb / freestanding
  edge via the shortest source→edge→listener bend, with an HF roll-off
  growing with the bend angle (bass bends, treble doesn't); and
  **transmission** through each portal filtered by its material. Disabled
  world = exact single direct path. Deterministic; bounded to `MAX_PATHS`.

**Realtime discipline.** The whole module runs on the control / offline
path — solving is heap-happy by design (like correction), and the realtime
renderers consume only the fixed-size resulting [`AcousticPath`]s. No new
allocation or lock is introduced anywhere on the audio path.

**Acceptance (spec-first).** `tests/fidelity/acoustic_world.rs` (8 tests)
— order-1 box → direct + 6 reflections with finite physically-placed
delays; the left-wall reflection's excess-path delay matches the renderer's
own image geometry; a fully-open portal transmits brightly (gain > 0.5)
and diffracts around its jambs; a fabric wall low-passes its reflections
well below a concrete wall's, both bounded to the audio band; the disabled
world is an exact direct path; a freestanding fin diffracts a path; solves
are deterministic, capped, and finite; and `diffract_around_edge` reports
the exact 1/r distance + delay. Unit suites in each new module (15 tests
total). `engine` and `config` stay in lockstep at 3.25.0.



**Unblocks (Horizon).** v3.26 acoustic **baking** (cache solved paths per
static source/listener — the solver's deterministic, bounded path set is
the exact input an offline baker needs), per-room coupling topology and
room graphs, and feeding solved [`AcousticPath`]s into the
binaural/panner renderers so room reflections become simulation-driven
rather than recomputed in the render loop.

---

## Phase 24 — Acoustic baking (v3.26.0) — **Implemented**

**Intent.** The guide's directive for v3.26: **turn expensive acoustic
computation into reusable render data.** Phase 23's solver enumerates every
propagation path between a source and a listener — direct, image-source
reflections, wedge diffraction, portal transmission. For a *static* scene
that enumeration is identical block after block, yet the renderers would
re-run it every frame. This phase adds the cache that makes the simulation
pay for itself: a **position-dependent response cache** (`BakedScene`)
pre-solves the world for a set of static source positions against a fixed
listener, and the binaural / panner / VBAP renderers consume the cached
responses at audio time.

### Key mechanisms

- **`bake`** — [`AcousticBaker`] (control path: owns an `AcousticWorld`,
  bakes a scene's static object positions in one `bake_scene` call),
  [`BakedScene`] (position→response map keyed by the containing 0.5 m
  cube; incremental `bake` for hosts that accumulate cells),
  [`BakedObject`] (one resolved response per cell) and [`BakedPath`] (a
  light, `Copy` path record: direction, distance, delay, gain, low-pass
  corner, path kind, interacting wall, and the **full per-band
  [`MaterialSpectrum`]** where a surface interacted). [`BakePolicy`]
  retains only the path kinds a host renders.
- **Renderer consumption** — `BasicPanner`, `VbapRenderer` and
  `BinauralRenderer` gain `set_baked(Option<BakedScene>)`. When an
  object's position falls in a baked cell, room reflections are placed
  from the cached response via [`BakedScene::listener_images`], which
  converts cached paths into the renderers' existing `ListenerImage` tap
  format with the *same excess-delay convention* as `images_for_object`;
  objects outside the bake fall back to the live solve. No bake attached
  → bit-identical to Phase 23: the bake is a cache, not a new model.
- **Frequency-domain data survives** — each baked reflection carries its
  full spectrum so offline / reference renderers can do true
  frequency-domain processing rather than the collapsed low-pass corner.

**Realtime discipline.** Baking is control / offline-path and heap-happy
by design (it is the expensive work being cached). Render-time is a read
of a `HashMap` plus a flat copy into a fixed `[ListenerImage; MAX_IMAGES]`
— no solving, no allocation, no locks on the audio path.

**Acceptance (spec-first).** `tests/fidelity/acoustic_bake.rs` (7 tests)
— baked-vs-live equivalence for all three renderers within tight
relative tolerance (identical geometry; the ±1 ulp difference comes from
`Vec3::normalized` division vs the live reciprocal multiply, not the
model); the cache is position-keyed (distinct cells distinct, same-cell
re-bakes reuse); unbaked objects fall back to the live solve
(deterministic, finite); a fabric-wall bake darkens the reflected
low-pass corner below a concrete bake while both stay in-band; and
bake+render is deterministic. Unit suites in `bake.rs` (7 tests; total
acoustic unit count 22). `engine` and `config` stay in lockstep at
3.26.0.

**Unblocks (Horizon).** v3.27 **Graph 2.0** (arbitrary topology — the
baked scene is already an explicit `position → response` dependency a
general graph could represent as nodes/edges), v3.28 timeline/scheduler
(bake invalidation on scene mutation), and multi-listener bakes (one
`BakedScene` per listener — the cache is already keyed per listener
position, so N listeners is N scenes).

---

## Phase 25 — Graph 2.0 (v3.27.0) — **Implemented**

**Intent.** The guide's directive for v3.27: **make the graph the true
center of the rendering engine.** Everything before this phase either
rendered a signal through a fixed linear chain (`DspPipeline`) or through
the fixed canonical arena of `dsp::graph` — a set of node slots whose
*order* is data but whose chain is implicit and track/bus-centered. This
phase generalizes that: a new `dsp::graph2` module is a model of **explicit
structure**. Nodes declare input/output **ports** with typed-bus metadata;
connections are **first-class edges**; the topology — not an authored
chain — defines the signal flow. Execution order is derived from the edge
set, never authored.

### Key mechanisms

- **`node`** — [`NodeId`] / [`PortId`] identity, [`PortSpec`] (direction +
  [`SignalType`] + channel count; `0` = any for fan-in/out ports),
  [`NodeKind`] (Source / Sink / Gain / Delay / Mix / Split — a
  topology-complete set: generator, consumer, 1:1 transform, fan-in,
  fan-out), per-node [`NodeCapabilities`] (stateful / realtime-safe /
  taps), and [`NodeParams`] payloads.
- **`edge`** — [`EdgeDef`]: an explicit, addressable connection from one
  typed port to another. The topology *is* its edge set.
- **`validate`** — [`ValidationReport`] with hard errors and warnings.
  Errors: unknown node/port, typed-bus (`SignalType`) mismatch,
  duplicate fan-in into one input port, and cycles. Warnings: dangling
  ports (unconnected inputs read silence; unconnected outputs are
  dropped). Cycle detection is a grey/white/black DFS that reports the
  **cycle path** (`A -> B -> A`).
- **`sort`** — deterministic Kahn's algorithm (ascending-id tie-break)
  producing the [`ExecutionOrder`]: the Graph 2.0 analogue of
  `dsp::graph::plan::ExecutionPlan`. Identical topologies always compile
  to identical orders; every node appears after all of its producers.
- **`mod`** — [`Graph2`] builder/query: `add_source` / `add_gain` /
  `add_delay` / `add_mix` / `add_split` / `add_sink`, `add_edge`
  (fail-fast on endpoint, typed-bus, and fan-in violations), `remove_*`
  with an ownership rule (a node with attached edges cannot be removed),
  `set_params`, `validate`, `compile` (cached; any mutation invalidates —
  **dynamic graph recompilation** is mutate-then-compile), serde
  serialization round-trip, and `to_dot` Graphviz inspection.
- **`exec`** — [`OfflineExecutor`]: renders a compiled topology block by
  block. Each edge owns a one-block plane (zeroed at block start, so an
  unconnected input is silence); state lives per node (delay lines,
  oscillator phase); sinks accumulate captures. Offline-first by design,
  exactly like the acoustic layer.

**Realtime discipline.** Graph 2.0 is control/offline-path and heap-happy
by design (building, validating, compiling, and rendering offline are the
expensive work an offline engine can afford). The realtime `dsp::graph`
hot path is untouched — no allocation or lock added to any audio thread. A
future milestone lowers a compiled [`ExecutionOrder`] onto a realtime
plan.

**Acceptance (spec-first).** `tests/fidelity/graph_topology.rs` (8 tests)
— a dry/wet diamond (`Split → {Gain, Delay} → Mix`) renders its dry and
wet copies at the exact expected offsets and gains; three-way fan-out sums
exactly (0.1 + 0.2 + 0.3); a cycle is rejected at compile with the path
reported; structural validation catches bad ports, duplicate fan-in, and
typed-bus mismatches as errors while dangling inputs are warnings; two
identical topologies schedule identically and every node runs after its
producers; tearing down one branch and recompiling changes the render;
JSON round-trip is render-identical; and a sine source drives a gain graph
continuously. Unit suites in each new module (9 tests total). `engine`
and `config` stay in lockstep at 3.27.0.

**Unblocks (Horizon).** v3.28 **timeline and scheduler** (a compiled
`ExecutionOrder` is a natural host for sample-accurate events and tempo
mapping), v3.29 reference rendering/determinism (the offline executor is
already a deterministic renderer — event recording and golden renders can
wrap it), and v3.30 graph-wide latency (per-node `taps` capability is
already declared; a propagation pass over the edge set is next).

---

## Phase 26 — Timeline and Scheduler (v3.28.0) — **Implemented**

**Intent.** The guide's directive for v3.28: **make time a first-class
render primitive.** Phases 23–25 built the world (acoustic simulation),
cached it (baking), and made the graph the center (Graph 2.0) — but time
was still implicit: block boundaries, not musical events, drove rendering.
This phase adds a deterministic **clock + event scheduler** on the
control/offline path and wires it to the Graph 2.0 executor, so a host
runs "this audio program over time" — BPM, bars/beats/ticks, tempo
changes and ramps, looping, quantization, transport, timeline regions —
and scheduled parameter changes land on the exact sample.

### Key mechanisms

- **`clock`** — [`AudioClock`]: Direction 3's shape in full. Two
  positions: `position` (the looped playhead — drives display and musical
  position) and `master_position` (a monotonic, never-wrapping counter
  that events are keyed against — events fire once, exactly once, even
  across loops). Transport state (Playing / Paused / Stopped), loop
  region, linear [`TempoRamp`], time signature, bars/beats/ticks (MIDI
  480 PPQ), and sample↔beat conversions.
- **`tempo`** — [`TempoMap`]: ordered tempo changes with exact
  piecewise-constant beat↔sample integration, so "at beat 4" maps to the
  right sample when the tempo changes at beat 2.
- **`event`** — [`ScheduledEvent`]: resolved to an absolute master sample
  at schedule time; [`EventTime`] (Sample | Beat), [`EventPayload`]
  (SetGain typed to a Graph 2.0 node, a Trigger, an opaque Host tag).
- **`Timeline`** (this module) — the scheduler: `schedule_at_sample` /
  `schedule_at_beat`, note-grid [`Quantize`] snapping, `advance_block`
  returning exactly the events whose master sample was crossed (each
  carrying its in-block index for sample-accurate application),
  [`TimelineRegion`] containment, and `pending()`.
- **Renderer hook** — [`OfflineExecutor::set_gain_step`] applies a gain
  change at an arbitrary in-block frame, so an event firing at master
  sample `S` lands on `S % block` exactly, then persists as the new
  block-quantized value.

**Realtime discipline.** The timeline is control/offline-path and
heap-happy by design; it drives an offline renderer. It adds no allocation
or lock to any realtime audio thread (the realtime `dsp::graph` hot path
is untouched).

**Acceptance (spec-first).** `tests/fidelity/timeline_scheduler.rs` (7
tests) — a Timeline drives the Graph 2.0 executor with a gain gate
scheduled at beat 1 (120 BPM): silent for the first 23 999 samples, then
sample-exact at 24 000; a non-block-aligned gain step lands
at the exact in-block index and persists; looping wraps the playhead
(musical position < 4 beats at master 12 800) while the event fires once;
pausing freezes the clock, leaves the event pending, and — because the
render loop gates `process_block` on `is_playing` — produces no audio; a
16th-note grid snaps beat 6.6 to sample 156 000; a tempo change retimes a
beat event across segments (beat 4 → sample 72 000); and timeline regions
resolve containment. Unit suites: clock 5, tempo 3, timeline mod 6,
graph2 `set_gain_step` 1 (15 new). `engine` and `config` stay in lockstep
at 3.28.0.

**Unblocks (Horizon).** v3.29 **reference rendering and determinism**
(the timeline's monotonic master + once-fire events are exactly the inputs
an event-recording/`aelog`-replay layer needs); v3.30 graph-wide latency;
and musical automation (tempo-mapped control curves riding this clock).

