# Phase 46 Pre-Porting Inventory — Node Parity for Graph2 (prep, pre-v3.51.0)

> Status: **pre-porting work**, produced in parallel with Phase 45 (realtime
> lowering substrate). This document enumerates the *complete* production node
> set, the control surface `Graph2ControlHandle` must mirror, and the scenario
> matrix `tests/fidelity/graph2_graph_equivalence.rs` must cover — everything
> the port itself (Phase 46 items 1–4) will consume. Per the campaign plan:
> *"Phase 46 enumerates every `GraphControlHandle`/capability consumer and
> maps it to a test scenario before porting."*
>
> Baseline: `main` @ `6a62eb7` (v3.49.0). Line counts cite this revision.

## 1. Production node set → Graph2 `NodeKind` port map

The engine's hot path is the 17-slot `GraphNode` arena
(`src/dsp/graph/mod.rs:142`, slot table at `mod.rs:98`). Phase 46 ports each
arena slot's **behavior** into a `graph2` node kind (or reuses one that
already exists), with the Phase-45 kernel seam guaranteeing one shared math
implementation. `dsp::graph` node types stay until Phase 48.

| # | Arena slot | Type (file, lines) | Graph2 target | Exists today? | Port effort notes |
|---|---|---|---|---|---|
| 0 | `MIX` | `MixBusNode` + `MixInput` (`nodes/mix/mod.rs` 1,087 / `sum.rs` 1,163 / `envelope.rs` 27) | new `NodeKind::MixBus` | partial — `NodeKind::Mix` is a bare fan-in sum (`exec.rs:869`) | **Largest port.** N per-input pre-mix chains (preamp `GainNode` + `LoudnessNode` + user-gain ramp + balance + pan×pan_law + mute + active), per-slot channel trims (gain+polarity), per-slot master/aux sends, per-slot metering, automation tracks (≤64 breakpoints, targets gain/balance/pan/trim/send), ducking (≤4 targets, aux-peak-gated envelope), TrackMixer-compatible transition state machine (`envelope.rs` — PlayingCurrent/Crossfading/Fading/PlayingNext/Silent, gapless), MAX_MIX_SLOTS=8, MAX_CHANNELS planes per slot preallocated. Sum math in `sum.rs` (stereo front-pair + multichannel lanes). |
| — | (mix chain members) | `GainNode` (preamp form), `LoudnessNode` (`gain_node.rs` 256 / `loudness_node.rs` 99) | shared kernels consumed by `MixBus` | n/a | Port as Phase-45 `ops.rs` kernels (ramped gain, loudness EQ 2-band preamp), **not** standalone kinds — they live inside the mix node's per-input chain in `dsp::graph`, and the same nesting applies in graph2. |
| 14 | `AUX` | `AuxBusNode` + `AuxSendBus` (`aux_node.rs` 504) | new `NodeKind::AuxBus` | no | Standalone plan node: consumes per-slot post-fader send taps via a shared same-thread `AuxSendBus` (UnsafeCell, single-audio-thread contract — must be preserved or replaced by an intra-plan buffer), per-send ramped gains (10 ms), optional global convolution insert (wet/dry), return into master front pair, independent metering, duck-gain return tap. Disabled = bit-absent. |
| 1 | `EQ` | `EqNode` (`eq_node.rs` 92) wrapping `dsp::equalizer` | new `NodeKind::Equalizer` | no | Thin node: parametric EQ bank (≤64 bands, `EqBandParams`), preamp, bass/treble shelves, auto-headroom, mid-side mode, 0 latency. Full math already in `dsp::equalizer` — the node port is glue. |
| 2 | `DYNAMICS` | `DynamicsNode` (`dynamics_node.rs` 61) wrapping multiband compressor | new `NodeKind::Dynamics` | no | Thin node over `dsp::multiband`: per-band threshold/ratio/attack/release/makeup + knee/detector/stereo-link features. 0 latency, tail from release envelopes. |
| 3 | `CONVOLUTION` | `ConvolutionNode` (`convolution_node.rs` 78) over `ConvolutionEngine` | new `NodeKind::Convolution` (reuse existing offline `NodeKind::Convolution`? **No — keep both, rename production one**) | `NodeKind::Convolution` exists as a 1:1 FIR toy (fixed pipeline delay = kernel len, `exec.rs` kernel) | The production node is **stereo, partitioned-FFT** (`dsp::convolution`), wet/dry mix, latency = engine block size, tail = IR length. The graph2 offline `Convolution` is a mono direct-FIR demo oracle. Port the production behavior as `NodeKind::Convolver` (distinct kind) so the offline primitive remains the aelog/reference oracle. |
| 4 | `BALANCE` | `BalanceNode` (`gain_node.rs` 111) | shared kernel / new `NodeKind::Balance` | no | Stereo balance pair-law (same family as mix balance kernel). |
| 5 | `CROSSFEED` | `CrossfeedNode` (`crossfeed_node.rs` 76) over `dsp::crossfeed` | new `NodeKind::Crossfeed` | no | Profiles (incl. custom freq/Q/delay), enabled toggle. Latency from the internal delay line, tail likewise. |
| 6 | `STEREO` | `StereoNode` (`stereo_node.rs` 67) over `StereoEnhancer` | new `NodeKind::StereoEnhancer` | no | Width control, enabled toggle. |
| 7 | `TIMESTRETCH` | `TimeStretchNode` (`timestretch_node.rs` 69) over WSOLA `dsp::timestretch` | new `NodeKind::Timestretch` | no | Speed multiplier; latency = `stretcher.latency_ms`×sr (WSOLA window), f32-only on hot path (demote/promote in Quality mode — precision contract must carry over). |
| 8 | `VOLUME` | `GainNode` volume form (`gain_node.rs` 210) | shared ramped-gain kernel (Phase-45 `ops.rs`) | partial — `NodeKind::Gain` is static scalar | Production form has a 10 ms one-pole ramp + sticky user-state mirror. The ramped kernel must be sample-accurate and shared. |
| 9 | `SEEK_FADE` | `SeekFadeNode` (`gain_node.rs` 27) | new `NodeKind::SeekFade` | no | Fade-out/fade-in envelope pair (`BeginSeekFadeout/In` commands), is_complete polling. |
| 10 | `ROUTING` | `RoutingNode` (`routing_node.rs` 93) over `dsp::routing` (ChannelTrimmer / BassManagement / matrix) | new `NodeKind::Routing` | no | Channel-layout-driven: trim to layout, bass management, down/upmix matrix; `multichannel_layout` is graph state that must become plan-visible. |
| 11 | `RESAMPLER` | `ResamplerNode` (`resampler_node.rs` 80) over Rubato adapter (`resample` feature) | new `NodeKind::RateConverter` (production) | `NodeKind::Resampler` is a fixed-ratio windowed-sinc offline demo | Production resampler is streaming, block-fed, ratio from device policy, latency `r.latency_samples()`. Keep the offline demo kind; port production behavior under a new kind. |
| 12 | `LIMITER` | `LimiterNode` (`limiter_node.rs` 114) over lookahead limiter | new `NodeKind::Limiter` | no | Lookahead latency (`lookahead_ms`×sr), true-peak mode, ceiling/attack/release/soft-clip, gain-reduction + max-true-peak telemetry atomics (control mirrors), plus the **output-domain** final-safety-limiter entry points (`graph/limiter.rs`: per-sample / block / multichannel / flush) used by the engine's output matrix. |
| 13 | `DITHER` | `DitherNode` (`dither_node.rs` 75) over `dsp::dither` | new `NodeKind::Dither` | no | Bit-depth + noise-shaping profile; output-domain (runs at endpoint depth). |
| 15 | `CORRECTION` | `CorrectionNode` (`correction_node.rs` 368) per-channel partitioned convolution bank | new `NodeKind::Correction` | no | `CorrectionIrSet` (heap-bearing — generation-swap path, never queued), live enabled/depth toggles, latency = block size when active, tail = IR length. |
| 16 | `SPATIAL` | `SpatialNode` (`spatial_node.rs` 1,161) wrapping the full `spatial` renderer | new `NodeKind::Spatial` | `NodeKind::Acoustic` is the *offline baked-scene reflection* demo; **not** the production renderer | Production: virtual-screen geometry, room reflections + late field, listener orientation, quality tiers, voice budget, program automation, `SpatialTelemetry` + `SpatialHealthSnapshot` publication, scene persistence (`engine/spatial_persistence.rs`). Port = wiring the existing renderer behind a node op; the renderer itself is untouched. |

**Channel-format nodes** (plan item: "channel-format nodes"): `RoutingNode` +
the mix node's per-slot plane staging cover it today; the graph2 port maps
them onto `PortSpec.channels` (0 = wildcard) with layout flowing as plan
metadata — no separate kinds needed beyond `Routing`/`MixBus`.

**Latency/tail capability descriptors** (plan item 3): every ported kind
reports through `graph2/latency.rs::node_latency` exactly what the arena node
reports via `DspNode::latency_samples/tail_samples` today — see the table in
§3.2. `compensate` then covers the ported nodes with zero new machinery.

### 1.1 Shared-kernel extraction list (Phase-45 seam ↔ Phase-46 consumers)

Behavior, not files: `dsp::graph` node logic is refactored into pure kernels
that both crates call. Candidate pure functions (all f32/f64 planar, no
allocation, no state ownership — state structs stay per-node):

1. `gain_ramp` block kernel (one-pole/target-ramp, sample-accurate) —
   consumed by: MixBus preamp/gain, Volume, Balance, Aux send gains.
2. `loudness_preamp` 2-band kernel — MixBus per-input loudness.
3. `pan_law_pair` / `balance_pair` coefficient kernels — MixBus, BalanceNode.
4. `envelope_step` transition kernel (TrackMixer state machine math,
   `mix/envelope.rs`) — MixBus.
5. `sum_stereo_front` / `sum_multichannel_lane` accumulate kernels — MixBus.
6. `aux_accumulate` + `aux_return` kernels — AuxBus.
7. `duck_envelope_step` kernel — MixBus (reads aux peak from the send bus).
8. Per-stage nodes (EQ/dynamics/crossfeed/stereo/limiter/dither/routing/
   convolution/correction/spatial/timestretch/resampler): the nodes are thin
   over `dsp::*` primitives that **already are** the single implementation —
   the "kernel" is the node-op wrapper signature (planes in/out + state),
   shared by both executors via Phase-45 `ops.rs`.

## 2. Control surface parity — `Graph2ControlHandle` mirror

`Graph2ControlHandle` must mirror `dsp::graph::GraphControlHandle`
(`controls.rs:577`) **one-to-one** so Phase-47 command handlers need only a
type swap. Discipline: `NodeCmd` stays `Copy`, SPSC capacity 64, drained at
block boundary, heap-bearing ops via generation swap.

### 2.1 Queued commands (`NodeCmd`, `controls.rs:42`) → graph2 routing

| `NodeCmd` variant | Target node kind (graph2) | Notes |
|---|---|---|
| `SetBitPerfect(bool)`, `SetDoPBypass(bool)`, `SetSpeed(f32)` | shell/plan-level | bit-perfect must remain **plan-absent stages** (disabled-exact); speed feeds timestretch + limiter schedule |
| `SetVolumeTarget(f32)`, `SetBalance(f32)` | `MixBus`-post / Volume node | sticky mirror `user_volume/balance` |
| `BeginSeekFadeout/In` | `SeekFade` | completion polled |
| `MixInput{input, cmd}` (→ `MixInputCmd`: gain/balance/pan/pan_law/mute/active/trim/send, `mix/mod.rs:206`) | `MixBus` per-slot | sticky per-slot atomics (gain/balance/pan/mute/active, `MAX_MIX_SLOTS`) |
| `MixTransition` (→ `MixTransitionCmd`: crossfade/fade/playing/duration, `mix/mod.rs:246`) | `MixBus` envelope | crossfade curve/enabled/duration also mirrored via `SetMixCurve/Enabled/DurationFrames` |
| `SetDuck(Option<DuckState>)` | `MixBus` duck envelope | ≤4 targets; gates on aux peak from send bus |
| `SetAux{enabled, return_gain}`, `SetAuxInsert{enabled, wet_mix}` | `AuxBus` + `MixBus` taps | sticky `aux_state`/`aux_insert_state` |
| `SetCorrectionEnabled(bool)`, `SetCorrectionDepth(f32)` | `Correction` | IR itself = generation swap (`load_correction_ir` handle method takes `Arc<CorrectionIrSet>`) |
| `SetSpatialEnabled/Screen/Room/Listener` | `Spatial` | screen=(az,hw,el,gain), room=(en,w,d,h,abs,order,rt60,late_mix,wet), listener=(yaw,pitch,roll) |
| `SetLimiter*` (enabled/mode/params/true-peak) | `Limiter` | gain-reduction + max-TP telemetry mirrors |
| `SetEq*` (enabled/auto-headroom/preamp/bass/treble/band/midside) | `Equalizer` | band params `EqBandParams` |
| `SetConvolutionWetMix(f32)` | `Convolver` | IR = generation swap |
| `SetStereoWidth`, `SetStereoEnhancerEnabled` | `StereoEnhancer` | |
| `SetCrossfeedEnabled/Profile/Custom` | `Crossfeed` | |
| `SetCompressor*` (enabled/band/band-features) | `Dynamics` | |

### 2.2 Handle surface (control thread) — full mirror checklist

From `controls.rs:577–1186` (`GraphControlHandle`) + `DspGraph` passthroughs
(`controls.rs:1513–1906`). Grouped; every row must exist on
`Graph2ControlHandle` with identical semantics before Phase 47:

- **Lifecycle/generation**: `publish_generation`, `reclaim_retired`,
  `generation`, `reclaimed_count`, `dropped_commands`.
- **Shell**: `set_volume`, `set_volume_db`, `set_balance`,
  `begin_seek_fadeout/in`, `apply_loudness_metadata_outgoing/incoming`,
  `set_bit_perfect`, `set_dop_bypass`, `set_speed`.
- **Limiter**: `set_limiter_enabled/mode/params/true_peak`,
  `limiter_true_peak_enabled`, `limiter_gain_reduction_db`,
  `limiter_max_true_peak_dbtp`.
- **EQ**: `set_eq_enabled/auto_headroom`, `set_preamp_db`,
  `set_bass_shelf`, `set_treble_shelf`, `set_eq_band`, `eq_num_bands`,
  `set_midside_eq`, `is_midside_eq`.
- **Per-stage toggles/params**: `set_convolution_wet_mix`,
  `set_stereo_width`, `set_stereo_enhancer_enabled`,
  `set_crossfeed_enabled/profile/custom_params`,
  `set_compressor_enabled/band_params/band_features`, `set_loudness_mode`.
- **Mix bus / lanes**: `set_input_gain/gain_db/balance/pan/pan_law/mute/
  active`, `set_slot_trim`, `set_slot_send`, `set_duck`,
  `set_slot_automation`, `clear_slot_automation`, `slot_meters`.
- **Transitions**: `begin_crossfade_frames`, `begin_fade_frames`,
  `begin_playing`, `set_crossfade_curve/enabled/duration_frames` (+ ms
  passthroughs on the graph object).
- **Aux**: `set_aux`, `set_aux_insert`, `aux_state`, `aux_insert_state`,
  `aux_meters`, `aux_send_peak`.
- **Correction**: `set_correction_enabled/depth`, `load_correction_ir`,
  `correction_state`.
- **Spatial**: `set_spatial_enabled/screen/room/listener`,
  `spatial_enabled` (+ richer `SpatialNode` accessors used by
  persistence/telemetry — see §2.3).
- **Mixer introspection**: `mixer_state` (`MixerState` for engine events).

**Sticky mirrors to reproduce** (audio-writes, control-reads, from
`controls.rs:203–236`): `user_volume/balance/speed/fade_ms`, per-slot
`gain/balance/pan/mute/active`, per-slot `peak_db/rms_db`, per-slot send
levels (packed master‖aux), aux insert state, correction state, spatial
enabled, limiter GR/TP. `UserState` snapshot seeding on rebuild
(`swap.rs:86`) incl. `has_live_bus_state` semantics must be preserved.

### 2.3 Non-queue engine call sites that must find a graph2 seam

Census of `self.graph.*` across `src/engine` (call counts), Phase-47 targets:

| Call site (engine file) | Graph API consumed | Graph2 port needs |
|---|---|---|
| `decode_loop/single.rs` (×5), `crossfade.rs`, `common.rs` | `process_final_limiter{,_block,_multichannel}`, `process_block{,_lanes,_crossfade_with_lanes}`, `mixer_state` | RT-plan entry points with identical signatures (Phase 47 does the swap; Phase 46 defines the ops) |
| `engine/tick.rs:406,433` | `control_handle().slot_meters(lane.slot)`, `aux_insert_state()` | telemetry mirrors above |
| `engine/tick.rs:292,318,817` | `engine_stats_with_output_format`, `bit_perfect_report_with_access`, `latency_report(resampler_ms, ring_ms, device_ms)` | `graph2` report module parity (`report.rs` equivalents) |
| `engine/spatial_persistence.rs:180–187` | `control_handle().set_spatial_enabled/screen/room/listener` | command routing + `SpatialNode::screen()/room()/listener()` getters for restore verification |
| `engine/commands/{playback,dsp,eq,lanes,output,playlist,…}.rs` | every `set_*` above + accessors `spatial()`, `spatial_mut()`, `eq_mut()`, `timestretch_mut()`, `routing_mut()`, `correction()`, `convolution_ir_needs_reload()`, `dither_mut()` | typed per-node access on the graph2 plan shell (mirror of `access.rs`) |
| FFI (`ffi.rs` via `pipeline_mut()`) | the whole surface above through `EngineHandle` | unchanged behavior (Phase 47 keeps FFI identical) |
| `engine/tests/{commands,lanes,spatial_persistence}.rs` | same surface | tests are the acceptance harness for API parity |

**Test-scenario mapping for each consumer** (the campaign mitigation): every
row of §2.1/§2.2 maps to at least one equivalence scenario in §4; every
§2.3 row maps to an engine test that must pass unchanged after the Phase-47
type swap (engine test files enumerate themselves).

## 3. Latency / tail / capability descriptors

### 3.1 Capability contract

Every ported kind extends `NodeKind::capabilities` (stateful / realtime_safe /
taps) to match `DSP_STAGE_CAPABILITIES` (the pipeline table is the single
source of truth; graph2 must not invent divergent metadata).

### 3.2 Latency/tail per node (as reported today → what `graph2::node_latency` must return)

| Node | latency_samples (active) | tail_samples |
|---|---|---|
| Convolution | `engine.block_size()` | IR length (partitioned tail) |
| Crossfeed | internal delay line | same |
| Limiter | `lookahead_ms·sr` (rounded) | release-dependent (see node) |
| TimeStretch | `stretcher.latency_ms·sr` | WSOLA window |
| Resampler | `resampler.latency_samples()` | filter tail |
| Correction | block size (when active) | IR length |
| Spatial / Acoustic | 0 (direct path) | reflections+late field |
| EQ / Dynamics / Stereo / Balance / Gain / Mix / Aux / Dither / Routing / SeekFade | 0 | EQ/dynamics filter ring-down; mix/aux envelopes |

`graph2/latency.rs::analyze/compensate` must consult these (plan item 3) so
the alignment pass covers ported nodes; the graph's
`total_latency_ms`/`latency_report` (resampler+ring+device inputs) keeps its
engine-side composition.

## 4. Scenario matrix for `tests/fidelity/graph2_graph_equivalence.rs`

Requirement (plan item 4): *for every scenario in `graph_pipeline_equivalence`
(27 cases, `tests/fidelity/graph_pipeline_equivalence.rs:660–1156`), the
equivalent Graph2 topology matches `dsp::graph` bit-exactly.* Golden vectors
unchanged.

### 4.1 The 27 existing cases → graph2 topology mapping

| # | Case | Graph2 topology to build |
|---|---|---|
| 1 | `default_stereo_f32` | default plan: Buffer(in) → MixBus(1 slot) → EQ? … exact default chain → Sink |
| 2 | `default_stereo_f64` | same, Quality precision path |
| 3 | `all_stages_stereo_f32` | full chain: MixBus → Correction → EQ → Dynamics → Convolver → Crossfeed → StereoEnhancer → TimeStretch → Volume → Routing … (canonical order from `plan.rs`) |
| 4 | `all_stages_stereo_f64` | same in f64 (timestretch demote/promote contract) |
| 5 | `all_stages_block_len_1` | block-size-1 planes (plan runner split discipline) |
| 6 | `all_stages_block_max` | `MAX_AUDIO_BLOCK_FRAMES` planes |
| 7 | `all_stages_overrun_buffer` | planes > block budget (scratch overrun path) |
| 8 | `midstream_control_changes` | drive §2.1 commands between blocks on the same graph2 control queues |
| 9 | `bit_perfect_stereo` | topology **without** DSP nodes (disabled-exact: bit-absent) |
| 10 | `bit_perfect_multichannel` | same, multichannel routing |
| 11 | `dop_bypass_stereo` | DSD bypass plan (no PCM DSP) |
| 12 | `loudness_ebu_r128` | Loudness node op with EBU R128 metadata |
| 13 | `convolution_synthetic_ir` | Convolver with the suite's synthetic stereo IR (wet/dry mix exercised) |
| 14 | `mono_via_mc_entry` | single-channel input through multichannel entry |
| 15 | `multichannel_5_1_trim` | Routing node + per-slot trims, 6ch |
| 16 | `multichannel_7_1` | 8ch full chain |
| 17 | `eq_control_surface` | EQ node + §2.1 SetEq* commands midstream |
| 18 | `crossfeed_control_surface` | Crossfeed + profile/custom commands |
| 19 | `compressor_control_surface` | Dynamics + band/feature commands |
| 20 | `limiter_control_surface` | Limiter + params/true-peak/toggles |
| 21 | `loudness_track_replaygain` | loudness mode switch + Track RG metadata |
| 22 | `crossfade_2input_f32` | MixBus 2 slots, `MixTransitionCmd::Crossfade` midstream |
| 23 | `crossfade_2input_f64` | same in Quality mode (secondary pre-mix stays f32 — S1 contract) |
| 24 | `fade_2input` | sequential fade thirds |
| 25 | `crossfade_disabled_gapless_2input` | gapless switch (no envelope math) |
| 26 | `crossfade_2input_midstream_controls` | both inputs' loudness metadata + volume moves + full transition sequence |
| 27 | `crossfade_2input_overrun_buffer` | 2-slot mix + overrun planes |

### 4.2 Additional parity scenarios (graph2-only behavior gaps the 27 don't cover)

These exercise `dsp::graph` surfaces the 27-case matrix doesn't, drawn from
§2.3 consumers — each must also match bit-exactly:

- **Aux bus**: per-slot sends pre-programmed + runtime `set_slot_send`,
  `set_aux` enable/disable midstream, aux insert on/off, aux meters
  monotonicity (aux return into master front pair).
- **Ducking**: `set_duck(Some)` with aux-peak trigger crossing the threshold
  mid-block sequence; `None` disabled-exact.
- **Per-slot automation**: `set_slot_automation` (gain/balance/pan/trim/send
  targets, ≤64 points) vs `dsp::graph` equivalents; `clear_slot_automation`.
- **Correction**: enabled/depth toggles with a fixed synthetic IR set (IR via
  generation-swap path); disabled-exact when off.
- **Spatial**: enabled + fixed screen/room/listener vs `SpatialNode` output;
  disabled-exact (plan-absent). Bit-exactness relies on the same renderer
  instance math — compare against `dsp::graph`'s node, not a re-render.
- **Lanes**: `process_block_lanes`-shaped topology (primary + N lane slots),
  lane activation/deactivation midstream (`set_input_active`).
- **SeekFade**: begin fadeout → poll complete → begin fadein, mid-chain.
- **Speed/timestretch**: `set_speed` changes midstream on the timestretch op.
- **Routing/bass management**: 5.1/7.1 layouts with bass management on.
- **Precision mode**: f32↔f64 switch on one topology (`set_precision_mode`).
- **Generation swap under load**: publish a reconfig (add lane / aux /
  spatial) mid-run; output continuity identical to `dsp::graph` swap (sticky
  state: volume/balance/speed/slots/aux/correction/spatial/duck survive).
- **Queue backpressure**: `dropped_commands` accounting when a queue fills
  (capacity 64) — parity of the drop counters.

### 4.3 Harness contract

- Reuse the deterministic generator (`xorshift` multi-tone + seeded noise,
  `graph_pipeline_equivalence.rs:165–178`) and the `Case`/`Cmd` model
  verbatim (same signals, same command timing) so a mismatch is always a
  graph2 divergence, not a fixture difference.
- Compare with `f32::to_bits` equality per sample (NaN payloads included);
  structural parity assertions: identical total latency
  (`graph2::analyze` vs `graph.total_latency_ms`) and identical active-node
  set (`graph_nodes()` vs graph2 capability report).
- Per-case `block_len` sweep {1, 256, MAX} where the original fixes one.
- Gate: `#[ignore]`-free, runs in the normal `cargo test --workspace`
  matrix; each ported node kind lands with its scenarios **in the same PR**
  (port order in §1 is also scenario-landing order).

## 5. Out of scope for Phase 46 (tracked elsewhere)

- Engine call-site swaps (`process_block*` → Graph2 RT executor) — Phase 47.
- Shadow-mode dual-run — Phase 47.
- `dsp::graph` removal + `graph2` prelude re-exports — Phase 48 (v4.0.0).
- Plugin node/ABI — Phase 49.
- `dsp::graph/nodes/spatial_node.rs` (1,161 lines) / `hrtf.rs` / etc. splits —
  Track-C opportunistic debt, cohesion check first.

## 6. Port-order proposal (for the Phase-46 implementation PR series)

1. `Equalizer`, `Dynamics`, `Crossfeed`, `StereoEnhancer`, `Convolver`,
   `Dither`, `Routing`, `Balance` (thin wrappers; land with their §4.1
   control-surface scenarios).
2. `SeekFade`, `Volume` ramp kernel, `Limiter` (+ telemetry mirrors + the
   final-safety-limiter ops), `RateConverter`.
3. `MixBus` + envelope/sum/duck/automation kernels + per-slot mirrors
   (§2.2), 2-input crossfade scenarios first (22–27), then multichannel.
4. `AuxBus` + send-bus contract + aux scenarios.
5. `Correction`, `Spatial` (+ persistence-restore parity).
6. Full-matrix run + `realtime_allocation` graph2-RT extension (Phase-45
   seam) + version bump v3.51.0 (lockstep both crates, CHANGELOG, tag).
