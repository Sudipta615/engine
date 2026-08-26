# Changelog

All notable changes to this project are documented in this file.

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
