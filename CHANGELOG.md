# Changelog

All notable changes to this project are documented in this file.

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
