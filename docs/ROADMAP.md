# Roadmap: single-stream player → multi-stream graph runtime

This document captures the phased evolution of the engine from a
single-stream, pipeline-driven player into a multi-stream, node-graph mixing
runtime. Each phase is grounded in the code as it exists on `main`; status is
one of **Done** (merged), **Designed** (specified, not yet implemented), or
**Horizon** (planned).

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
order: 3D VBAP geometry (speaker-pair/triplet solver + out-of-coverage
fallback), object behavior (directivity/Doppler/occlusion), beds/fields,
Ambisonics/HOA, room/reflections, HRTF/SOFA/binaural, head tracking, scene
file format, and eventually a `SpatialNode` in the production graph. The
scene/level/render layer is the stable substrate those build on.

