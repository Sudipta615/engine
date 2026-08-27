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
- **Aux as a plan node (done, v3.10.0):** promoted the aux out of `MixBusNode`
  into a standalone `AuxBusNode` running as its own `AUX` plan step with a
  shared interior-mutable `AuxSendBus`. Each mix slot's send is now an
  independent ramped gain (click-free per-send automation) with its own
  per-send peak meter (`aux_send_peak(slot)`); the master meter is measured
  at the post-aux / pre-post-mix-chain point so telemetry includes the aux
  return exactly as the pre-split layout did.
- **Horizon:** precompiled lane kernels, decode-ahead lane buffering — the
  meter/send/automation substrate from Phases 4–6 is the exact measurement
  and control surface those need.
