# Changelog

All notable changes to this project are documented in this file.

## [3.49.0] — 2026-08-31

### Added

- **Versioned configuration envelope + migration framework** (`config`).
  New `VersionedConfig` wraps an [`EngineConfig`] with a `version` schema
  ({ `version`: 1, …config } via `#[serde(flatten)]`); `load`, `save_pretty`
  and `migrate` expose an explicit, future-proof upgrade path. A legacy
  pre-versioning payload (a bare `EngineConfig` JSON) deserializes as
  already-current — the historical no-op guarantee — and `EngineConfig`
  itself is unchanged, so hosts using it today keep working. Re-exported as
  `config::{CONFIG_VERSION, migrate_step, VersionedConfig, ConfigLoadError}`.
- **Unified quality-evaluation harness (Phase 2).** New [`eval`]
  module (see [`docs/QUALITY.md`]): a versioned [`ReferenceVectorRegistry`]
  (content-addressed via the aelog `SHA-256` substrate — a changed
  expectation changes the address, so a drifting spec is always
detectable) plus objective measurement primitives (Goertzel amplitude,
  THD+N, bit-exactness, DTFT impulse-response magnitude **and phase**) and
  **nine** DSP/spatial suites: [`DspPipeline`] bit-exact + transparency,
  parametric-EQ biquad frequency **and phase** response, limiter true-peak
  ceiling, resampler in-band gain, binaural inter-aural level, **EBU R128
  loudness** (BS.1770-4 reference tone), **partitioned-FFT convolution vs
  a naive-direct reference**, **channel separation / crosstalk**, and
  **HRTF-interpolation convexity** against the measured grid.
  [`EvaluationReport`] renders a human-readable PASS/FAIL table
  (`render_text`) and machine-readable JSON (`to_json`), and
  [`eval::EvaluationReport::compare`] diffs two engine versions into a
  [`VersionComparison`] (unchanged / drift / improvement / regression) so
  regressions are detected automatically across builds.
  [`eval::run_quality`] assembles every suite, and the `quality_harness`
  fidelity test asserts all components pass, the report is deterministic +
  versioned, and cross-version drift is detectable. Everything runs off
  the audio path; measurement numbers mirror the existing
  golden/fidelity conventions. The controlled listening-test layer is
  documented in [`docs/QUALITY.md`].
- **Consolidated, versioned track metadata model** (`decode`). New
  [`TrackMetadata`] aggregates the previously scattered metadata into one
  `Clone`/`PartialEq` model: editorial [`TrackTags`] (title/artist/album /
  album-artist/genre/date/track/disc numbers/artwork reference), duration,
  technical [`AudioFormatInfo`], loudness tags
  ([`LoudnessMetadata`]), and opt-in offline measured loudness
  ([`LoudnessScanResult`]) and chapters ([`CueSheet`]).
  [`TrackMetadata::from_path`] is cheap (tags + loudness reads, no decode);
  `from_path_with_measurement` runs a full scan. Reuses the existing
  codec-routing extractors, so values match what playback reads on load.
  `LoudnessMetadata`, `GaplessInfo`, `AudioFormatInfo` now derive
  `PartialEq` so the aggregate is comparable.
- **Spatial-health diagnostics (explainable per-source status).** New
  [`spatial::SpatialHealthSnapshot`] derives *why* the spatial render
  behaves as it does — **localization quality** (measured-HRTF grid
  coverage or the analytic fallback, plus angular-spread blur),
  **direct-vs-reflected energy ratio** (direct path gain vs room send ×
  wall reflection coefficient), **occlusion severity** (applied
  attenuation + low-pass cutoff), and **phase risk** (measured
  inter-channel correlation of the master output + per-source spread /
  extreme-pan heuristics) — as a serde-serializable per-source report with
  stable `HealthLevel` codes (`inactive`/`good`/`moderate`/`poor`) and a
  human note per factor. Runs entirely on the **telemetry/control path**
  (the engine tick, from the existing meter snapshot + scene + voice
  counts); the audio path is untouched. `PlaybackInfo` gains
  `spatial_health`, the metering snapshot gains the measured
  `stereo_correlation` (cross-energy accumulation, allocation-free and
  opt-in), and the C FFI gains `engine_spatial_health`.
- **Deterministic AudioProfile perceptual layer (Phase 3).** New
  [`profile`] module fusing loudness, dynamics, spectral, transient,
  stereo, spatial, and content measurements into one versioned,
  serializable [`AudioProfile`] with documented units/ranges and
  confidence semantics (duration × sub-profile coverage). Built entirely
  on deterministic DSP — BS.1770-4 loudness via the shared
  [`LoudnessMeter`] (identical to the scanner), a Hann-windowed FFT
  power average (centroid / rolloff / flatness / slope / brightness),
  windowed onset detection, and running L/R + mid/side statistics
  (correlation / width / balance / phase risk / side fraction).
  [`AnalysisMask`] lets consumers request only the analysis they need;
  the bounded-memory streaming [`ProfileAnalyzer`] and
  `analyze_decoder`/`analyze_path` run entirely off the audio path;
  and the on-disk [`profile::cache`] persists results validated against
  file size/mtime and the schema version, optionally deduplicating
  identical content across paths via a content fingerprint
  (`analyze_path_cached_by_fingerprint` with the `fingerprint`
  feature). Content-class probabilities are normalized heuristic
  indicators with an explicit no-evidence prior — never a hidden
  learned model.
- **Typed, serializable diagnostics.** New `engine::DiagnosticKind`
  (`EngineFault` / `TrackLoad` / `Decode` / `Output` / `BitPerfect` /
  `Configuration`) and `engine::BitPerfectCause` (all stable snake_case
  codes) replace the previously string-only `EngineStats`
  `engine_error`/`bit_perfect_reason` fields with structured categories;
  the message strings remain for humans and stay exactly as before.
  [`PlaybackInfo`] exposes a serializable `engine_diagnostics: Vec<Diagnostic>`
  snapshot, and the C FFI gains `engine_diagnostics_info` (typed kind /
  bit-perfect cause codes + the human message) so hosts can query typed
  diagnostics without parsing prose. Config validation is now
  categorized too: `config::{ConfigIssue, ConfigIssueKind, ConfigSeverity}`
  ride alongside `ConfigValidation::errors`/`warnings` (same checks, same
  messages), giving hosts a stable machine-readable `kind.code()` per issue.

### Changed

- **Spatial seam reconciliation (docs/code consistency).** The binaural
  module docs no longer claim the head model is "azimuth-only": elevation is
  carried by the pinna [`ElevationNotch`] in the analytic path and by
  bilinear elevation interpolation in a loaded [`HrtfDataset`]. `object`/
  `scene` velocity docs now describe the live per-block Doppler path
  (`object.velocity − listener.velocity`) instead of "future Doppler"; the
  panner's air-absorption docs reflect that the HF roll-off is applied, and
  the `level::DistanceModel` and `SpatialSourceType` docs are corrected.
  NetCDF-4/HDF5 `nc4` SOFA is **explicitly deferred with rationale** in the
  spatial module docs: the robust readers link `libhdf5`, conflicting with
  the pure-Rust/no-FFI rule, and the typed rejection already isolates the gap
  behind `HrtfCorpus`.

## [3.48.0] — 2026-08-30

### Added

- **Per-path air absorption / distance roll-off on the spectral kernels.**
  [`BakedScene`] now carries a scene-scoped [`AirAbsorption`] model; when a
  host enables it, every non-direct spectral kernel the `Acoustic` node
  renders is additionally shaped by a **per-path, distance-dependent HF
  roll-off** `1 / √(1 + (f/f_air)²)` where `f_air =
  [`AirAbsorption::cutoff_hz`]`(path.distance)` — so a farther reflection
  genuinely darkens with travel distance, exactly as the acoustic bake
  intends, while staying equal at DC.

  - [`BakedScene::set_air_absorption`] attaches the model (default = the
    disabled [`AirAbsorption::default`], keeping every kernel — and every
    golden render — **bit-identical** to v3.47); serialized on the scene with
    `#[serde(default)]`, so older baked-scene logs load with air off.
  - New `spectral_taps_with` / `path_filter_kernel_with` thread the model
    through the kernel builder; `spectral_taps` and `path_filter_kernel`
    keep their exact signatures (disabled model, unchanged). The `Acoustic`
    node (offline `run_acoustic`) now reads the attached scene's air model.
  - With air enabled even a spectrally flat path becomes a distance-darkened
    low-pass kernel rather than a single-tap delta.

### Changed

- [`AirAbsorption`] derives `Serialize`/`Deserialize` so the baker can embed
  it verbatim.


### Added

- **Realtime room reflections are now spectrally coloured.** The per-path
  spectral model the offline `Acoustic` node renders exactly (a material's
  per-band [`MaterialSpectrum`] or a collapsed diffraction/transmission
  low-pass corner) is forked into the **production hot path**: the baked
  path responses [the `BasicPanner`, `VbapRenderer` and `BinauralRenderer`
  place from a [`BakedScene`]] now fall back on a **one-pole low-pass per
  reflected image** realised from that same corner, so a curtain-darkened
  or diffracted reflection genuinely loses its highs instead of just being
  scaled.

  - [`ListenerImage`] gains a `lowpass_hz` field (∞ = spectrally flat). The
    live scalar-`Room` solve fills ∞ (bit-identical to before);
    `BakedScene::listener_images` derives the corner per reflection — from
    the path's full per-band spectrum via `surface_lowpass_hz` when one is
    present, else its collapsed diffraction corner, with near-Nyquist
    corners collapsed back to ∞ so flat materials stay strict passthrough.
  - [`EarlyReflections`] gains per-(object, image) one-pole low-pass state
    (`set_reflection_filter` block-rate setup, `filter_reflection` for the
    binaural direct-ring reads) and applies it inside `object_frame`;
    entirely preallocated at `prepare`, allocation-free and lock-free on the
    audio thread (verified by the realtime suite). The panner/VBAP tap path
    and the binaural fractional-delay path both colour their reflections.
  - Reflection level is unchanged (`coeff`/`gain` still applies) — the
    low-pass is the spectral roll-off “from the corner”, so a damped
    wall stays as loud where it reflects bass/deps as before while its
    treble genuinely rolls off.

### Changed

- `ListenerImage` grew a field (`lowpass_hz`). It is `Copy` and carries
  only a default-∞ added member; all call sites updated.


### Added

- **Graph-based binaural branches now use real measured head-related
  responses.** [`NodeParams::HRTF`] gains a `source` field:
  [`HrtfSource::Inline`] keeps the classic hand-authored `left`/`right`
  tabs (backward compatible; reported taps stay `max(left.len,
  right.len)`), while new **`HrtfSource::Dataset { azimuth_deg,
  elevation_deg, taps }`** reads the executor's attached
  [`HrtfDataset`](crate::spatial::hrtf::HrtfDataset) via
  `OfflineExecutor::set_hrtf_dataset` and renders the **bilinearly-
  interpolated measured per-ear HRIRs** at that source direction — the
  same corpus the real `BinauralRenderer` consumes, now routable in the
  topology. New builders `Graph2::add_hrtf_dataset(name, az, el)` (renders
  at [`MAX_HRTF_TAPS`] = 128, mirroring the dataset) and
  `add_hrtf_dataset_with_taps(...)` (pass the dataset's own
  [`HrtfDataset::taps`](crate::spatial::hrtf::HrtfDataset::taps) to avoid
  zero-padding).
- The measured node **reports its taps to the latency pass like a
  `Delay`**: `node_latency` returns `taps`, its capabilities flag `taps`,
  and `compensate` aligns a merge by `Delay(taps)` on the opposing branch
  — so a dataset-driven binaural branch carrying real ITD/head-shadow
  impulse responses still lines up exactly with the dry leg.
- **Rendering.** `OfflineExecutor` gets `set_hrtf_dataset(Option)`;
  `run_hrtf` reads per-ear measured IRs with
  `HrtfDataset::bilinear_interpolate` (padded/truncated to the reported
  `taps` so reported and actual delay agree), then renders through the
  same per-ear streaming pipeline as the inline ears (both ears delayed
  together, pair aligned). A dataset node with no dataset attached falls
  back to pass-through, mirroring an unbaked `Acoustic`.

## [3.45.0] — 2026-08-30

### Added

- A **`Resampler` node** closes the last latency hook the v3.30 pass
  documented (alongside Delay, Convolution and HRTF). New
  [`Graph2::add_resampler`](`crate::dsp::graph2::Graph2::add_resampler`)
  / `add_resampler_with_quality` build a mono-in/mono-out sample-rate-
  conversion node that **reports its own taps to the latency pass**:
  `node_latency` returns the filter half-span `quality` (default
  [`RESAMPLER_DEFAULT_QUALITY`] = 32), its capabilities flag `taps`, and
  [`compensate`](`crate::dsp::graph2::latency::compensate`) aligns parallel
  branches around it exactly like a `Delay`. The node emits with exactly
  `quality` samples of pipeline delay — so its *reported* and *actual*
  taps agree, the same convention as the other tap-reporting nodes.
- **Rendering.** The executor's `run_resampler` resamples the input by a
  bandlimited Hann-windowed-sinc interpolator (`ratio ≥ 1` = output frames
  per input frame). The fixed-frame offline executor resamples onto its own
  frame grid (a rate/pitch remap), with a per-node input history ring that
  keeps interpolation continuous across blocks and a leading `quality`
  zeros to carry the reported taps; ratio 1 reproduces the input to
  interpolator accuracy, and higher ratios sample it at `1/ratio` the
  source rate.

## [3.44.0] — 2026-08-30

### Changed

- Long-kernel **`Convolution` nodes render through the realtime partitioned
  FFT engine** (`dsp::convolution`) instead of the O(N·M) direct path.
  Kernels ≥ [`CONVOLUTION_FFT_THRESHOLD`] (512 taps) route through the
  re-aimed [`OfflineExecutor`] partitioned overlap-add engine, so genuinely
  long impulse responses render fast offline; shorter kernels keep the
  exact byte-equal direct path (where partitioned FFT would both cost more
  and already exceed the front delay the node must present). The engine's
  UP-OLA latency (one partition, 512 frames) is absorbed into an extra
  front-padded delay so the node's **reported and actual contract is
  unchanged**: `output[k] = (x * h)[k - kernel.len()]`, and graph-wide
  latency/compensation still aligns convolution branches by the full kernel
  length. Falls back to the exact direct path transparently if the engine
  can't load the IR.

## [3.43.0] — 2026-08-30

### Added

- The **`aelog_replay` CLI** now hooks in the golden-render cache with a
  `--cache` flag and **hit/miss reporting**, so repeated `engine replay`
  runs of the same session skip re-rendering. Pass `--graph <graph.json>`
  (a serialized [`Graph2`] topology; `order` is recompiled on load),
  optionally `--sink <n>` to choose the capture sink (defaults to the
  graph's first Sink node), and `--cache-dir <dir>` (defaults to
  [`AelogCache::default_root`]). The first run reports `cache: MISS
  rendered & stored (<samples> B) under <sha-256 content address>`;
  repeated runs report `cache: HIT reused golden render` and splice the
  stored capture instead of rendering, including the capture size, byte
  count, and content address for reproducibility. `--verbose` also prints
  the rendered peak and end master position. Without `--cache` the CLI's
  original event-replay oracle output is unchanged. End-to-end covered by
  a new test that drives the compiled binary itself
  (`tests/fidelity/aelog_replay.rs`).

## [3.42.0] — 2026-08-30

### Added

- The aelog golden-render cache is now **size-bounded**: [`AelogCache`]
  enforces a byte budget by **LRU eviction**, so the app-data cache
  directory can't grow without bound. New `AelogCache::with_budget(root,
  bytes)` constructor (default [`DEFAULT_CACHE_BUDGET`] = 256 MiB;
  `0` = evict everything except the just-written entry), each entry stores
  a last-access `touched` stamp bumped on every lookup hit, and `insert`
  evicts the least-recently-used entries until the total sits at or below
  ​90% of the budget. A single capture larger than the budget is kept
  (never evicted by its own insert). Entries persisted before this version
  load fine (`touched` defaults to oldest).

### Changed

- Cache entries are now **content-addressed**: each golden-render file is
  named by the **SHA-256** (new dependency-free [`sha256`], pinned against
  the FIPS 180-4 vectors) of the canonical JSON of its render identity
  (log hash, graph hash, sink, sample rate, block frames) — instead of a
  machine-local FNV name. Because the render is a pure function of that
  identity, the name derives solely from semantic content: two machines
  rendering the same session through the same graph compute the *same*
  filename for the *same* stored bytes, so a synced or shared cache
  directory is valid on any host. New public `content_address` helper;
  the LRU `touched` stamp is deliberately excluded from the address so a
  local hit-touch never rewrites the shared name. The in-process memo and
  the `log_hash`/`graph_fingerprint` identity hashes are unchanged.

## [3.41.1] — 2026-08-30

### Fixed

- The aelog golden-render cache key no longer includes the log's **label**
  or `format_version`: [`log_hash`] hashes only the render-relevant
  content (sample rate, block cadence, and every recorded command), so
  semantically identical sessions — the same take re-labelled for a
  different song, say — **reuse one cached golden render** instead of
  missing and re-rendering. Sample-rate/command differences still split
  keys; the label itself is preserved in the log, it just doesn't join
  the key.

## [3.41.0] — 2026-08-30

**Musical automation** — tempo-mapped control curves on the AudioClock
drive graph parameters over time. A curve is authored in **beats** and
evaluated against a **tempo map**, so the same automation lands on the
correct samples as the tempo changes, and the graph's gain sweeps
smoothly over the session.

### Added

- `dsp::timeline::automation::CurveBeats`: a piecewise-linear control
  curve in musical time — `set(beat, value)`, `evaluate_beats(beat)`, and
  `evaluate(sample, &TempoMap, sample_rate)` which maps a sample back to a
  beat through the tempo map before interpolating (a tempo change just
  remaps where each beat lands).
- `OfflineExecutor` gain automation: `set_tempo_map` + `set_gain_automation`
  drive a Gain node from a curve, sweeping the gain with a
  **sample-accurate linear ramp** across each block (`master_sample` tracks
  the playhead); an explicit `set_gain_step` still wins for the block.
- aelog recording/replay of musical automation: `SetTempoMap` +
  `SetGainAutomation` commands (`record_tempo_map` /
  `record_gain_automation`), `ReplayOutcome::tempo_map` +
  `gain_automation`, and `replay_render` attaching them to the executor so
  a recorded session renders the exact gain sweep.

### Changed

- Beats were already scheduleable (`EventTime::Beat`, v3.28); this makes a
  *continuous tempo-mapped parameter curve* a first-class recorded input on
  top of that. The aelog format stays v3 (additive variants; old logs still
  load).

## [3.40.0] — 2026-08-30

**Per-path spectral filtering** in the `Acoustic` node — the collapsed
broadband gain for each reflection/diffraction path is replaced with a
real minimum-phase filter per path, so a room's material (and diffraction
corners) colour the sound the way they physically do.

### Added

- `BakedScene::spectral_taps` / free `spectral_taps` (+ `ACOUSTIC_IR_LEN`)
  render one `(excess_delay, FIR kernel)` per non-direct path.
- A reflection carrying its full per-band `MaterialSpectrum` shapes the
  source directly via `reflectivity_at_hz` (sampled per FFT bin and
  synthesized as a minimum-phase FIR with `dsp::correction::phase`).
- A diffraction/transmission path collapsed to a corner (a finite
  `lowpass_hz`, no spectrum — the `SPECTRAL_COLLAPSED` case) applies a
  one-pole low-pass at that corner, scaled by the broadband gain.
- A truly flat path (no spectrum, no corner) reduces to a single-tap gain
  delta, reproducing the classic broadband behavior exactly and for free.- An executor **`acoustic_epoch`** so the node recompiles its kernels when
  the world changes even if two worlds bake the same source cell.

### Changed

- **`render_cached` is now two-tier**: the free convenience entry point
  consults a **thread-local in-process memo** (keyed by the same
  `(log_hash, graph fingerprint, sink)` tuple as the file cache) before
  the persistent `AelogCache`, so a second render of an identical log in
  the same process reuses the captured golden audio instead of
  re-rendering — `replay_events` (pure, cheap) + a splice, byte-identical
  to a fresh render. `clear_memo()` empties the fast layer; the memo is
  capped (`MEMO_CAP`) so a long-lived process can't hoard render memory.




### Changed

- `run_acoustic` renders each non-direct path by convolving it against the
  raw input-history ring (delay ∘ filter, both LTI, so they commute) at
  its excess delay — kernels (re)compile on scene swap / listener drive
  while the ring keeps the room ringing; `ACOUSTIC_HISTORY`-deep ring
  never drops session history. A flat scene's output is **byte-identical**
  to the previous broadband renderer; only non-flat materials change.
- `acoustic_taps` (the broadband reduction) is now test-only.

### Fixed

- Golden oracles and the fabric dampening checks now measure per-path
  spectral filtering (and broadband gain) instead of the collapsed taps.

## [3.39.0] — 2026-08-30

Acoustic nodes support **per-listener baked scenes**: a node can name a
scene from an executor registry, so a single graph renders **distinct
room responses for several listeners** and mixes them in the topology
(instead of every node sharing one global scene).

### Added

- **`NodeParams::Acoustic { position, scene: Option<String> }`**
  (`dsp::graph2`) — `scene: Some(name)` renders from the executor's named
  scene registry; `scene: None` (the plain `add_acoustic`) keeps using the
  active global scene. `Graph2::add_acoustic_scene(name, position,
  scene_id)` builds a scene-addressed node; `describe`/`to_dot` show the
  id.
- **`OfflineExecutor::set_scene(name, scene)` / `remove_scene(name)`** — a
  per-listener bake registry keyed by id. `run_acoustic` selects per node:
  named scene if the params say so, else the active scene; an unregistered
  id (or unbaked position) falls back to pass-through, and the tapped
  delay lines keep ringing through a replacement. The v3.38 listener
  position drive and the v3.37 scene swaps compose unchanged.

### Fixed

- None.

## [3.38.0] — 2026-08-30

The replayed listener trajectory now **drives** the graph: `replay_render`
retargets every `Acoustic` node's baked lookup from the recorded
`SetListenerPosition` stream, so a spatial golden render exercises the
full baked-room path — the room response is re-dered from the position
cache *as the listener moves*. Nodes keep their `NodeParams::Acoustic`
position as the fallback when no listener is driving.

### Added

- **`OfflineExecutor::listener_position`** (`dsp::graph2`) — a live
  listener position (`set_listener_position(position)`) that overrides
  each `Acoustic` node's lookup position; `None` restores the node's own
  position. `run_acoustic` re-looks-up the cell each block, so a moving
  listener walks through baked cells and unbaked regions fall back to
  pass-through, while the tapped delay lines keep ringing.
- **`replay_render`** (`dsp::aelog`) — applies each
  `SetListenerPosition { at, position }` to the executor before the block
  it covers (sample-exact on faithful logs, alongside scene swaps), so
  listener motion is no longer a report-only input.

### Fixed

- None.

## [3.37.0] — 2026-08-30

Animated acoustic worlds become deterministic: a `BakedScene` swap is a
recorded aelog command, so the geometry timeline of a session replays
exactly. The scene embeds in the log verbatim (order-stable serde — the
response cache is a `BTreeMap`, the `f32::INFINITY` low-pass sentinel
round-trips via a `−1.0` marker, the solver world is skipped), and
`replay_render` re-attaches each swap at its master sample **without
resetting** the `Acoustic` nodes' tapped delay lines — the room keeps
ringing through the change.

### Added

- **`RecordedCommand::SetBakedScene { at, scene }`** (`dsp::aelog`) —
  stamped with the master sample at record time;
  `AelogRecorder::record_baked_scene` logs a swap. Format stays v3 (an
  additive variant — old v3 files still load).
- **`OfflineExecutor::swap_baked_scene`** (`dsp::graph2`) — replaces the
  active scene mid-session without clearing the tapped delay lines, so a
  geometry change (a door opens, a wall turns to fabric) shifts the
  response seamlessly; `set_baked_scene` remains the fresh-attach reset.
- **Deterministic scene serde** (`spatial::acoustic::bake`) — `BakedScene`
  / `BakedObject` / `BakedPath` serialize (cell cache as an ordered entry
  list, `lowpass_hz` infinity sentinel), so aelog logs and hashes are
  pure functions of their commands.
- **Replay** — `ReplayOutcome::scene_swaps` exposes the `(master sample,
  scene)` timeline; `replay_render` applies each swap before the block it
  covers (sample-exact on faithful logs).

### Fixed

- None.

## [3.36.0] — 2026-08-30

Audio inputs go **multi-channel**: `Buffer` nodes and the aelog
`InputAudio` chunks carry **channel-major planes** (`track[0]` = channel
0, …), so stereo and spatial sessions record, reconstruct, and replay
every channel exactly. A `Buffer` node exposes one mono output port per
channel (the HRTF convention), and mono clips/tracks keep working
unchanged (a mono clip is simply a one-plane clip).

### Added

- **Multi-channel buffers** (`dsp::graph2`) — [`NodeParams::Buffer`]
  `samples` is now channel-major planes; `Graph2::add_buffer_channels` /
  `add_buffer_clip_channels` build N-port sources (one mono output port
  per channel). External tracks (`OfflineExecutor::set_external_input` /
  `set_external_clip`) follow the same layout; a shared cursor advances
  all channels in lockstep, and a mono track on an N-port node reads
  silence on the missing channels (no upmix).
- **Multi-channel aelog** — `AelogRecorder::record_audio_input_channels` /
  `record_clip_audio_channels` join the mono conveniences;
  `RecordedCommand::InputAudio` chunks are channel-major planes (format
  v3 — `AELOG_VERSION` bumped).
- **Multi-channel replay** — `ReplayOutcome::audio_input` /
  `clip_tracks` reconstruct channel-major tracks (a mono session yields
  one plane); `replay_render` feeds them to the executor so stereo/
  spatial sessions render byte-exact per channel.

### Fixed

- None.

## [3.35.0] — 2026-08-30

Audio inputs become **clip-addressed**: a `Buffer` source node carries an
optional clip address, the executor registers per-clip external tracks,
and aelog records each audio-input chunk with its clip — so a recorded
session's tracks route only to the nodes bearing that address, enabling
**multi-input graphs** (one graph mixing several recorded inputs). The
unaddressed single-track path is unchanged.

### Added

- **Clip-addressed buffers** (`dsp::graph2`) — [`NodeParams::Buffer`]
  gains `clip: Option<String>`; `Graph2::add_buffer_clip(name, clip, …)`
  builds an addressed source. An addressed node plays the per-clip track
  registered for its name (`OfflineExecutor::set_external_clip`); an
  unaddressed node plays the global external track
  (`OfflineExecutor::set_external_input`); either falls back to the
  embedded clip.
- **Per-clip aelog recording** — `AelogRecorder::record_clip_audio(clip,
  chunk)` joins `record_audio_input`; `RecordedCommand::InputAudio` now
  carries the optional clip (format v2 — `AELOG_VERSION` bumped).
- **Per-clip replay** — `ReplayOutcome::clip_tracks` reconstructs one
  `(clip, track)` pair per address in first-recorded order; `replay_render`
  feeds each track only to the matching nodes, so a multi-input session
  replays byte-identically.

### Fixed

- None.

## [3.34.0] — 2026-08-29

Binaural and convolution-heavy branches join the latency pass: two new
Graph 2.0 nodes — [`NodeKind::Convolution`] (1:1 FIR convolver) and
[`NodeKind::HRTF`] (mono-in / stereo-out binaural filter) — report and
compensate **exactly like `Delay` nodes**. The convolver reports its
kernel length as taps; the HRTF node reports the longer of its two
per-ear IRs and delays both ears by that length so the pair stays
mutually aligned. The executor renders both with a streaming
**overlap-add** convolution pipeline whose delay never drifts, so the
reported taps and the rendered timing agree at any block count.

### Added

- **Convolution node** (`dsp::graph2`) — [`NodeKind::Convolution`] with
  [`NodeParams::Convolution { kernel }`](NodeParams): convolves the input
  with an embedded FIR kernel and emits with one kernel-length pipeline
  delay (the block-partitioned lookahead convention). `node_latency` =
  `kernel.len()`. `Graph2::add_convolution` builds it.
- **HRTF node** — [`NodeKind::HRTF`] with
  [`NodeParams::HRTF { left, right }`](NodeParams): mono in, left/right
  ear out (ports 0/1), per-ear IR convolution, both ears delayed by the
  longer IR. `node_latency` = `max(left.len(), right.len())`.
  `Graph2::add_hrtf` builds it.
- **Streaming convolution pipeline** (`exec.rs`) — overlap-add across
  blocks with a constant-length delay queue: `output[k] = (x * h)[k - N]`
  exactly, at any block count; the per-ear pipeline delay is shared so an
  HRTF pair stays mutually aligned.
- **Latency pass** — `analyze`/`compensate` propagate the new taps like
  any `Delay`: a dry/wet diamond with a 300-tap convolver compensates to a
  single summed sample at 300, and a binaural branch aligns a dry leg to
  its 300 taps while preserving node ids.

### Fixed

- None.

### Changed

- None.

## [3.33.0] — 2026-08-29

Golden renders become cacheable: **a render cache keyed by a
deterministic hash of the aelog session** (plus the graph fingerprint and
sink) stores captured audio on disk, so identical logs reuse the stored
render instead of re-rendering. The hash is dependency-free FNV-1a over
the canonical JSON — identical sessions hash identically, any command
difference changes the hash — and the cache is best-effort: corrupt or
missing entries are misses, never wrong renders, and writes are atomic.

### Added

- **Render cache** (`dsp::aelog::cache`) — [`AelogCache`] with
  [`AelogCache::lookup`] / [`AelogCache::insert`] /
  [`AelogCache::render_cached`] and a default root under the app data
  directory; [`log_hash`] / [`graph_fingerprint`] expose the stable
  hashes. `render_cached` returns the stored capture on a hit (the cheap
  event stream is recomputed from the log — pure — and only the audio
  comes from the cache) and renders + stores on a miss.
- **Keying contract** — the key folds in the graph fingerprint and the
  sink id because a golden render is a pure function of
  `(log, graph, sink)`: the same log through a different graph is a
  separate entry, never a wrong render.
- **Robustness** — entries carry the hashes and header back and are
  re-verified on load; a collision or corrupted file degrades to a miss.
  Writes are temp-file + rename.

### Fixed

- None.

### Changed

- None.

## [3.32.0] — 2026-08-29

Aelog now records **every render input, not just timeline commands**:
audio fed into the graph and listener motion join the event log, so
spatial sessions replay exactly. A new `Buffer` source node in Graph 2.0
plays an embedded clip (one-shot or looping) or an externally supplied
track; the recorder logs each audio chunk and every listener position as
master-sample-stamped commands; replay reconstructs the full track and
listener trajectory and feeds the track back into the executor — the
final pieces of the guide's deterministic golden-render pipeline.

### Added

- **Buffer source node** (`dsp::graph2`) — [`NodeKind::Buffer`] with
  [`NodeParams::Buffer { samples, looping }`](NodeParams): a graph input
  primitive that plays its embedded clip, or the executor's external
  track when one is attached (`OfflineExecutor::set_external_input`).
  `Graph2::add_buffer` builds it; `to_dot` labels it.
- **Audio-input recording** — [`RecordedCommand::InputAudio`]: chunk-wise
  audio input logs; replay concatenates chunks into the exact session
  track (`ReplayOutcome::audio_input`) and feeds it into the render so
  captures are byte-identical.
- **Listener-motion recording** — [`RecordedCommand::SetListenerPosition`]
  stamped with the master sample at record time; replay returns the full
  `(sample, position)` trajectory (`ReplayOutcome::listener_motion`) for
  spatial renderers to re-apply sample-exactly.
- **Recorder surface** — [`AelogRecorder::record_audio_input`] and
  [`AelogRecorder::record_listener_position`] mirror the timeline's
  mutation discipline: every input is a command, so a session is a pure
  function of its log.

### Fixed

- None.

### Changed

- `Vec3` (spatial math) gained serde derives, keeping serializable graphs
  and logs position-exact.

## [3.31.0] — 2026-08-29

The acoustic world joins the graph: **reflections and baking become
graph-routable primitives**. A new [`NodeKind::Acoustic`] in Graph 2.0
renders the baked room response of a source position from a
[`BakedScene`] attached to the executor — an impulse into the node comes
out as the direct path plus one delayed, gain-scaled copy per baked
propagation path. Wet rooms route through `Split`/`Mix`/`Gain` like any
other signal.

### Added

- **Acoustic node** (`dsp::graph2`) — [`NodeKind::Acoustic`] with
  [`NodeParams::Acoustic { position }`](NodeParams): 1-in/1-out; the
  input plane passes through (scaled by the baked direct gain) plus each
  non-direct path at its excess delay with its gain, via a per-node tapped
  delay line sized to the longest tap.
- **Executor hook** — [`OfflineExecutor::set_baked_scene`]: attach a
  v3.26 `BakedScene`; `Acoustic` nodes look up their configured position's
  cell. An unbaked position or a missing scene passes the input through
  unchanged (deterministic fallback, matching the renderers' live-solve
  fallback semantics).
- **Latency semantics** — the acoustic node reports **zero pipeline
  latency** (the direct path passes immediately; the tail is wet content,
  not alignment delay), so `analyze`/`compensate` treat a room exactly
  like a signal with no latency to align — documented in
  `dsp::graph2::latency::node_latency`.
- **Serializable** — [`Vec3`] gains serde derives, so a graph containing
  acoustic nodes round-trips through JSON position-exactly.
- **Fidelity suite** — `tests/fidelity/acoustic_graph.rs` (6 tests): an
  impulse into the node reproduces the baked response **exactly** (oracle
  built from the same paths); a wet room + dry `Gain` route through
  `Split`/`Mix` with reflections summed per-excess-delay; unbaked
  positions and missing scenes pass through; the node adds no pipeline
  latency; the graph serializes/round-trips with positions intact; and a
  fabric-wall bake changes the rendered taps (weaker reflections).

## [3.30.0] — 2026-08-29

Graph-wide latency and alignment (roadmap v3.30 — the final roadmap phase;
Direction 2, "first-class latency"): timing relationships become explicit
and correct across arbitrary Graph 2.0 topologies. A new `dsp::graph2::latency`
pass reports per-node taps and cumulative upstream latency, and
**automatically compensates** parallel branches so every path into a merge
point arrives aligned to the slowest — a convolution/delay branch and a dry
branch no longer need hand-rolled delays.

### Added

- **Per-node latency accounting** — [`node_latency`]: every node reports
  its intrinsic sample taps (only `Delay` today; a future convolution /
  HRTF / resampler / lookahead node plugs in the same way).
- **Graph-wide propagation & diagnostics** ([`analyze`],
  [`LatencyReport`]) — validates, topologically schedules, and propagates
  cumulative upstream latency along the edge set: a `Mix` reports the
  slowest of its inputs, the report gives per-node `upstream` and `taps`
  plus the graph `total_samples` / `total_ms`.
- **Automatic delay compensation** ([`compensate`]) — returns an edited
  copy of the graph with a compensating `Delay` spliced in series on every
  faster branch into a merge point, so all inputs arrive aligned to the
  slowest. **Original node ids are preserved verbatim**, so a Timeline
  event addressing a node by id (e.g. `SetGain`) keeps working on the
  compensated graph; only new `Delay` nodes are added, and the result is
  re-validated.
- **Fidelity suite** — `tests/fidelity/latency_alignment.rs` (6 tests): a
  dry/wet diamond renders unaligned (dry @0, wet @300) then, after
  compensation, as a single aligned spike at 300 summing both branches
  (0.5 + 1.0); a deep 100+200-tap chain sums to 300 with no compensation
  inserted; the report propagates (mix upstream 300, taps 300/0, total_ms
  6.25); a Timeline `SetGain` on the dry node id still lands after
  compensation (3× sine once gated, 160 Hz phase-locked to the 300-tap
  delay); a three-way fan-out aligns all branches to the slowest (200 +
  100 taps, summed at sample 200); and analyze/compensate are
  deterministic.

## [3.29.0] — 2026-08-29

Reference rendering and determinism (roadmap v3.29) — Direction 17 made
concrete: a new [`dsp::aelog`] module records a render session (every
timeline mutation and block advance) into a versioned, serializable
`recording.aelog`, and replays it deterministically to reproduce identical
events and **byte-identical captured audio** — the project's
**golden-render substrate**. A bug report becomes "replay this log", and a
regression check becomes "compare the replay against the golden capture".

### Added

- **Aelog format** (`dsp::aelog`) — [`SessionHeader`] (versioned, sample
  rate, block size, label — deliberately no wall-clock timestamps, so a
  log is a pure function of its commands), [`RecordedCommand`] (Schedule /
  SetTempo / SetTimeSignature / SetLoop / SetLoopEnabled / SetTempoRamp /
  SetState / SetQuantize / Advance), and [`Aelog`] with JSON string and
  file round-trips (`to_json` / `from_json` / `save_json` / `load_json`)
  and explicit format-version checks.
- **Recorder** ([`AelogRecorder`]) — wraps a [`Timeline`] and mirrors its
  mutation surface, appending a command for every call; the recorder is
  the only way to touch its timeline, so a session cannot silently drift
  from its log. Two identical sessions serialize to byte-equal logs.
- **Replay** ([`replay_events`], [`replay_render`], [`ReplayOutcome`]) —
  `replay_events` reproduces the identical fired-event stream and end
  clock state; `replay_render` additionally feeds blocks to a provided
  Graph 2.0 [`OfflineExecutor`] (applying `SetGain` events sample-
  accurately, exactly as a live driver would) and returns the captured
  audio — byte-identical to the recorded session.
- **CLI** — new `aelog-replay` binary: the guide's `engine replay
  recording.aelog`. Loads a log, re-executes it, and prints the command /
  fired-event counts, end transport state, and (with `--verbose`) every
  command and fired event.
- **Fidelity suite** — `tests/fidelity/aelog_replay.rs` (6 tests): a
  recorded gate session replays to byte-identical golden audio and an
  identical fired stream with the sample-accurate beat-1 gate intact; JSON
  string and file round-trips replay identically; pause/resume + looping
  are recorded faithfully (master only advances while playing, the
  playhead wraps, the trigger fires exactly once); two identical sessions
  produce byte-equal logs; and replay is a pure function of the log.

## [3.28.0] — 2026-08-29

Timeline and Scheduler (roadmap v3.28): **make time a first-class render
primitive.** A new [`dsp::timeline`] module is Direction 3's `AudioClock`
(Direction 5's event runtime) fused: a deterministic, sample-accurate clock
and event queue that drives the Graph 2.0 `OfflineExecutor` — scheduled
parameter changes land on the exact sample, and the transport owns the
render, not just the events.

### Added

- **AudioClock** (`dsp::timeline::clock`) — Direction 3's shape in full:
  sample position (a looped playhead) + a monotonic master counter,
  tempo, bars/beats/ticks (MIDI 480 PPQ), transport state (Playing /
  Paused / Stopped), a loop region (playhead wraps; events still fire
  once on the master), a linear [`TempoRamp`], time-signature, and
  sample↔beat conversions.
- **TempoMap** (`dsp::timeline::tempo`) — ordered tempo changes with exact
  piecewise-constant beat↔sample integration, so a musical position maps
  correctly across tempo changes.
- **Events** (`dsp::timeline::event`) — [`EventTime`] (Sample or Beat),
  [`EventPayload`] (SetGain typed to a Graph 2.0 node, a Trigger, and an
  opaque Host tag), and once-only sample-accurate firing.
- **Timeline scheduler** ([`Timeline`]) — `schedule_at_sample` /
  `schedule_at_beat`, `advance_block` returning exactly the events whose
  master sample was crossed (each with the in-block index for sample-
  accurate application), note-grid [`Quantize`] snapping, [`TimelineRegion`]
  containment, and mutation-free determinism. Beat events resolve to an
  absolute master sample at schedule time.
- **Renderer hook** — [`OfflineExecutor::set_gain_step`](crate::dsp::graph2::OfflineExecutor::set_gain_step)
  applies a gain change at an arbitrary in-block frame; a timeline event
  firing at master sample `S` lands on `S % block` exactly.
- **Fidelity suite** — `tests/fidelity/timeline_scheduler.rs` (7 tests): a
  Timeline drives the Graph 2.0 executor with a gate scheduled at beat 1
  opening sample-exactly at 24 000; a non-block-aligned gain step lands at
  the exact index; looping wraps the playhead while events fire once;
  pausing halts both clock and render (the transport owns rendering); a
  16th-note grid snaps a beat to the exact sample; a tempo change retimes
  a beat across segments; and timeline regions resolve containment.

## [3.27.0] — 2026-08-29

Graph 2.0 (roadmap v3.27): **make the graph the true center of the
rendering engine** by generalizing the fixed track/bus chain of `dsp::graph`
into an *arbitrary topology* runtime. A new [`dsp::graph2`] module is a
model of explicit structure: nodes declare **input/output ports** with
typed-bus metadata (signal class + channel count), every connection is a
**first-class edge**, and the topology — not an authored chain — defines the
signal flow. Validation, cycle detection, deterministic topological
scheduling, dynamic recompilation, inspection/serialization, and an offline
executor that renders any built topology are all included.

### Added

- **General-purpose topology** (`dsp::graph2`) — [`Graph2`]: a builder
  (`add_source` / `add_gain` / `add_delay` / `add_mix` / `add_split` /
  `add_sink`, plus `add_node_raw` for host-defined port shapes), explicit
  edges (`add_edge` fails fast on unknown endpoints, typed-bus mismatch,
  and duplicate fan-in), removal with an ownership rule (a node with
  attached edges cannot be removed), and parameter mutation. Deterministic
  `BTreeMap` iteration makes every artifact reproducible.
- **Typed ports** ([`PortSpec`], [`SignalType`], channel metadata) — an
  edge is only legal when both endpoints agree on signal class; Audio can
  never cross into a Control port. Built-ins carry Audio; Control ports
  are fully modeled and enforced via `add_node_raw`.
- **Validation** ([`ValidationReport`], [`Graph2Error`]) — structural
  errors (unknown node/port, typed-bus mismatch, duplicate fan-in, cycles)
  block compilation; dangling ports are warnings the executor tolerates
  (unconnected inputs read silence, unconnected outputs are dropped).
- **Cycle detection** — grey/white/black DFS reporting the **actual cycle
  path** (`A -> B -> A`) in the error.
- **Topological scheduling** ([`ExecutionOrder`], `topological_order`) —
  deterministic Kahn's algorithm (ascending-id tie-break): identical
  topologies always compile to identical orders, and every node runs after
  its producers. This is the Graph 2.0 analogue of `dsp::graph::plan`.
- **Offline executor** ([`OfflineExecutor`]) — renders a compiled topology
  block by block through the built-in ops (Source / Sink / Gain / Delay /
  Mix / Split). A dry/wet bus is just `Split → {Gain, Delay} → Mix`; the
  executor is offline-first by design, exactly like the acoustic layer.
- **Inspection & serialization** — [`Graph2::to_dot`] renders a Graphviz
  digraph; the whole topology round-trips through serde JSON to an
  identical render.
- **Dynamic recompilation** — every mutation invalidates the compiled
  order; `mutate → compile() again` is the recompilation loop.
- **Fidelity suite** — `tests/fidelity/graph_topology.rs` (8 tests):
  dry/wet diamond renders both branches at exact offsets/gains, three-way
  fan-out sums exactly, cycles rejected with path, structural validation
  (bad ports, duplicate fan-in, typed-bus mismatch, dangling-warning
  semantics), deterministic scheduling, dynamic recompilation changing the
  render, JSON round-trip identity, and a sine source driving a gain graph.

## [3.26.0] — 2026-08-29

Acoustic baking (roadmap v3.26): **turn expensive acoustic computation into
reusable render data.** The v3.25 `AcousticWorld` solver enumerates every
propagation path (direct, image-source reflections, wedge diffraction, portal
transmission) between a source and a listener — work that, for a *static*
scene, is identical block after block yet was being re-run every frame.
A new [`BakedScene`] is a **position-dependent response cache**: source
positions are quantised to cubes (default 0.5 m) and the full resolved path
set — direction, distance, delay, gain, low-pass corner, path kind, and the
per-band material spectrum where a surface interacted — is stored once per
cell, then looked up by the renderers at audio time with no solving, no
allocation, and no locks.

### Added

- **Baking layer** (`spatial::acoustic::bake`) — [`AcousticBaker`] (control
  path: owns an `AcousticWorld`, bakes a scene's static object positions in
  one call), [`BakedScene`] (the position-keyed cache with an incremental
  `bake` for hosts that accumulate cells), [`BakedObject`] (one resolved
  response per cell) and [`BakedPath`] (a light, `Copy` path record).
  [`BakePolicy`] lets a host retain only the path kinds it actually renders.
- **Renderer consumption** — `BasicPanner`, `VbapRenderer` and
  `BinauralRenderer` each gain `set_baked(Option<BakedScene>)`. When an
  object's position falls in a baked cell, room reflections are placed from
  the cached response via [`BakedScene::listener_images`] (which converts
  cached paths into the renderers' existing `ListenerImage` tap format with
  the same excess-delay convention as `images_for_object`); objects outside
  the bake fall back to the live solve. With no bake attached the renderers
  are bit-identical to v3.25 — the bake is a cache, not a new model.
- **Frequency-domain data survives the bake** — each baked reflection
  carries its full per-band [`MaterialSpectrum`] so offline/reference
  renderers can do true frequency-domain processing instead of the collapsed
  low-pass corner.
- **Fidelity suite** — `tests/fidelity/acoustic_bake.rs` (7 tests):
  baked-vs-live equivalence for all three renderers, position-keyed caching
  (distinct cells distinct, same-cell reuse), live-solve fallback for
  unbaked objects, fabric-wall darkening of the reflection low-pass, and
  deterministic bake+render.

### Changed

- `AcousticWorld::probe_reflection_spectra` exposes the per-reflection
  frequency spectra to the baker (was solver-internal).

## [3.25.0] — 2026-08-29

Acoustic world simulation (roadmap v3.25 — the guide's Direction 6/7/8/9):
the **first purely simulation-side layer**, built to *separate acoustic
simulation from acoustic rendering*. A new `spatial::acoustic` module turns a
geometric description of a space (walls with frequency-dependent materials,
openings, diffraction edges) into a concrete set of propagation paths that
any renderer — binaural, panner, or an offline baker — consumes. It ships the
**geometry**, **materials**, **portals**, **propagation-path** and
**diffraction** primitives the guide lists, and moves the room from a single
scalar absorption coefficient to per-octave-band spectra.

### Added

- **Frequency-dependent acoustic materials** (`spatial::acoustic::material`)
  — [`MaterialSpectrum`]: per-ISO-octave-band (63 Hz–16 kHz) absorption /
  specular-reflection / transmission spectra, with log-frequency
  interpolation, a `broadband` reduction (geometric-mean gain + −3 dB
  low-pass corner) for the realtime renderers, and the named presets
  Direction 8 calls for ([`MaterialKind`]: Concrete / Wood / Glass / Fabric /
  Carpet / Metal / OpenMesh), each with a documented ISO-class spectrum.
- **Acoustic geometry** (`spatial::acoustic::geometry`) — [`AcousticRoom`]
  (an axis-aligned box with **per-wall** materials — the seam the old
  [`Room::absorption`](crate::spatial::room::Room::absorption) documented),
  [`Portal`] (an opening in a wall coupling two spaces, with its own
  transmissive material), and [`DiffractionEdge`] (a freestanding
  fin/mullion sound bends around), plus the jamb edges of a doorway.
- **Propagation paths** (`spatial::acoustic::path`) — [`AcousticPath`]: the
  simulation→render contract exactly as the guide specifies
  (`kind`, `direction`, `distance`, `delay_samples`, `gain`, `lowpass_hz`,
  `flags`, interacting wall), with [`PathKind`] (Direct / Reflected /
  Diffracted / Transmitted / Diffuse) and [`PathFlags`]
  (spectral-collapse / crosses-boundary metadata).
- **World + path solver** (`spatial::acoustic::solver`) — [`AcousticWorld`]
  owns the geometry and, given a source/listener pair, enumerates the path
  set: the **direct** path, **image-source reflections** (order 1 → 6,
  order 2 → 24, mirroring the renderer's room geometry — the excess-path
  delays are pinned to match), **wedge diffraction** around each portal jamb
  (and any freestanding edge) via the shortest source→edge→listener bend
  with an HF roll-off that grows with the bend angle, and **transmission**
  through each portal filtered by its material. Disabled world = an exact
  single direct path. Deterministic, bounded to `MAX_PATHS`.
- **Separation of concerns** — nothing here runs on the audio thread:
  solving is control/offline-path (heap-happy by design, like correction);
  the realtime renderers consume only the fixed-size resulting paths.

### Tests

- Unit suites in each new module (`material.rs`, `geometry.rs`, `path.rs`,
  `solver.rs`): flat/rising/material-spectrum reduction, per-wall plane
  geometry, portal centres/jambs, direct-path delay, path flags,
  order-1/order-2 reflection enumeration, portal transmission + diffraction,
  and bend-angle geometry.
- Acceptance suite `tests/fidelity/acoustic_world.rs` (8 tests): order-1 box
  → one direct + six reflections with finite physically-placed delays; the
  left-wall reflection's excess-path delay matches the renderer's own
  image-source geometry to half a sample; a fully-open portal transmits
  brightly (gain > 0.5) and diffracts around its jambs; a fabric wall
  low-passes its reflections well below a concrete wall's; the disabled
  world is an exact direct path; a freestanding fin diffracts a path;
  solves are deterministic and capped; and `diffract_around_edge` reports
  the correct 1/r distance + delay.
- `config` and `engine` crates stay in lockstep at 3.31.0.

## [3.24.1] — 2026-08-29

### Fixed

- **FadeProcessor accumulation precision** — `FadeProcessor::advance` now computes
  the gain value from the exact `samples_processed / total_samples` ratio in `f64`
  rather than accumulating an `f32` increment per sample. This eliminates
  floating-point accumulation drift over long fades (millions of samples).
- **Loudness K-weighting precision** — `KWeightStage1` and `KWeightStage2` in
  `dsp::loudness::meter` now maintain `f64` filter states (`z1`, `z2`), aligning with
  the main biquad engine's precision model and lowering the measurement noise floor.

### Changed

- **Crossfade curve documentation** — Extended `CrossfadeCurve` doc comments to clearly
  specify midpoint energy and power characteristics (constant-power vs −3 dB dip) for
  each curve variant.
- **Loudness hop timing documentation** — Added documentation detailing hop quantization
  at non-standard sample rates and alignment with EBU R128 tolerances.

## [3.24.0] — 2026-08-29

Completed the DSP seams and added the spatial scene infrastructure layers
listed in the architecture guide (Rules 3–10).

### Added

- **Near-field correction (§40)** — `spatial::nearfield`: bounded proximity
  gain (`MAX_GAIN = 2.0`) plus an optional low-shelf LF lift, per-object and
  block-rate smoothed.
- **Applied air absorption (§39)** — `AbsorptionState` in `spatial::level`:
  the distance-dependent `AirAbsorption::cutoff_hz` is now actually applied as
  a smoothed biquad low-pass (previously it was computed and discarded).
- **Doppler (§42)** — `spatial::doppler`: smoothed, variable-ratio fractional
  resampler driven by radial relative velocity, bounded to stay clear of the
  speed-of-sound, with deterministic re-anchoring for sustained approach.
- **Quality tiers (§86)** — `spatial::quality::SpatialQuality` (Low/Medium/
  High/Reference) backing the room-reflection depth on the hybrid paths.
- **Automation (§47)** — `spatial::automation`: generic piecewise-linear
  `Curve` (scalar/Vec3/Quaternion) with block-rate and sample-accurate
  evaluation, plus a `SpatialAudioAutomationFrame` per-object override.
- **Voice budget (§88)** — `spatial::voice`: `VoiceBudget` / `VoicePriority`
  (Fixed/DistanceWeighted/GainWeighted/UserDefined) scheduler producing a per-
  slot Full/Degraded/Dropped plan.
- **Metering (§70)** — `spatial::metering::SpatialMeter`: per-speaker / bus /
  LFE peak + RMS output meters.
- **Diagnostics (§107)** — `spatial::diagnostics`: allocation-free scene
  diagnostics + host-rasterizable reflection rays from `EarlyReflections`.
- **HRTF provider (§60–61)** — `spatial::provider::HrtfProvider` trait with a
  normalized-corpus adapter, isolating data import from the runtime renderer.
- **Upmix policies** — `spatial::upmix::UpmixMode` (ForceDownmixStereo /
  MatchOutput / SpatialRender) with deterministic gain policies.
- **Declarative render knobs in `config` (§86, §76, §70, §47)** — the new
  spatial quality / voice / metering / automation knobs are now serde fields
  hosts configure in files/JSON: `SpatialConfig` gains `quality`
  (`config::SpatialQuality`), `voice` (`SpatialVoiceConfig`: capacity /
  full-quality capacity / `VoicePriority` policy), and `metering`
  (`SpatialMeterConfig` enable); `SpatialObjectConfig` gains `automation`
  (per-object position/orientation/gain/spread curves via
  `CurveVec3Config` / `CurveQuatConfig` / `CurveScalarConfig`).
  `SpatialNode::apply_config` applies quality + metering to its binaural
  renderer and stores the converted `VoiceBudget`; `SpatialScene::
  from_config`/`to_config` round-trip the automation curves losslessly
  (new `Curve*::keyframes` accessors). Every new field is
  `#[serde(default)]`, so old configs and scene files keep deserializing.
- **Native NetCDF-classic SOFA importer (§61, optional `sofa-import` feature)**
  — `spatial::sofa`: a dependency-free, pure-Rust reader for the NetCDF-3
  classic subset of the AES69 `.sofa` format (the `SOFA_NETCDF3` /
  `SOFA_NETCDF3_CLASSIC` container) that validates the `Conventions` gate and
  reduces `SourcePosition` + `Data.IR` + `Data.SamplingRate` into an
  [`HrtfCorpus`] for the existing `HrtfDataset::from_corpus` pipeline.
  Big/little-endian CDF-1 supported; directions are mapped onto the layer's
  coordinate frame (SOFA CCW-left azimuth → engine CW-right `+X`); modern
  NetCDF-4/HDF5 (`nc4`) files are refused with a typed [`SofaImportError`]
  and the format-specific guidance (documented HDF5 seam). Ships synthetic
  CDF fixtures testing endian round-trips, the L/R stride, the direction
  convention, and rejection of bad magic / non-SOFA conventions /
  truncated data.
- **Spatial knobs exposed through C FFI (Phase 17, §47, §76, §86)** —
  `engine_set_spatial_quality` (tier 0–3), `engine_set_spatial_voice`
  (enabled / capacity / full-quality capacity / `VoicePriority` 0–3), and
  `engine_set_spatial_automation` (per-program-object gain/spread keyframe
  arrays, bounded + finite-validated, cleared with 0 points) / `
  engine_set_spatial_automation_time` (scene automation clock) let
  C/C++ hosts configure the spatial master they previously could only
  read (`engine_spatial_info`). Commands are dispatched through
  `EngineCommand` (`SetSpatialQuality` / `SetSpatialVoice` /
  `SetSpatialAutomation` / `SetSpatialAutomationTime`) into the graph's
  `SpatialNode` off the audio thread; automation curves now also apply
  live in the binaural object loop (`set_automation_time` + per-object
  gain/spread overrides applied at block rate).

### Changed

- `BasicPanner`, `VbapRenderer`, and `BinauralRenderer` now run the per-object
  cascade (Doppler → occlusion → air absorption → near-field) before the pan /
  head model; each stage is an exact passthrough when disabled, so the
  conventional paths remain bit-identical.
- Renderers expose `set_quality`, `set_automation_time`, and `meters()`
  accessors; `SpatialAudioObject` gained `doppler`, `near_field`, and
  `air_absorption` runtime knobs whose defaults keep existing scenes unchanged.
- **Voice admission applied end-to-end (`SpatialNode`, spec §76)** —
  `SpatialNode` now runs its configured voice budget over the scene's
  objects and feeds the per-slot `VoiceAdmission` plan into the renderer's
  object loop. `VoiceBudget::plan_into` is a new allocation-free sibling of
  `plan` (selection-rank ranking into caller buffers) so the budget can run
  on the audio path; the `BinauralRenderer` gained `set_voice_admission` /
  `clear_voice_admission` and honours admission per object (Dropped = silence
  with level-chain smoothing still advancing; Degraded = direct path only,
  no room reflections, point spread).  No budget configured = full admission,
  so conventional paths stay bit-identical.
- **Spatial telemetry in `PlaybackInfo` (Phase 17)** — the `SpatialNode`
  now publishes its live per-ear output meters (peak/RMS dBFS) and its
  voice-admission counts (full / degraded / dropped) as
  `PlaybackInfo::spatial` (`Option<SpatialTelemetry>`, always present,
  inactive at rest) on the lock-free snapshot's telemetry cadence, plus a C
  FFI `engine_spatial_info` getter mirroring `engine_correction_info`. Hosts
  read spatial levels / dropped-voice count from the already-atomic
  `ArcSwap<PlaybackInfo>` without touching the audio thread.

## [3.23.0] — 2026-08-29

Hot-path optimization of the binaural renderer (Phase 22). Per-frame-
constant geometry — the analytic ITD delay (`sin` + Woodworth) and the
room images' ITD — is hoisted out of the per-sample loop and computed once
per block; FIR ring reads drop their per-tap modulo; ring cursors advance
by increment-and-wrap; and `read_delayed` needs one modulo instead of two.
All changes are arithmetic-identical (same samples, same order), so the
bit-exact equivalence suites are untouched — measured on `spatial_bench`
(4 objects, 1024 frames @ 48 kHz): FIR dataset path **2.9×**, analytic
**2.2×**, analytic + room **1.7×**, production graph SpatialNode (512)
**1.9×**; the VBAP path is unchanged (untouched).

### Added

- `benches/spatial_bench.rs` — criterion suites for the binaural FIR /
  analytic / room paths, VBAP on 5.1, and the graph with the SpatialNode
  enabled (regression guards for the Phase-22 wins).

### Changed

- `BinauralRenderer` object loop: analytic ITD delays and room-image ITDs
  precomputed once per block; `azimuth_rad` no longer recomputed per
  frame in the FIR path.
- FIR convolution: descending ring reads with a wrap branch instead of a
  modulo per tap (identical tap order).
- `read_delayed`: one modulo + branch instead of two modulos.

### Fixed

- None.

## [3.22.0] — 2026-08-29

The active spatial scene now persists across sessions: the engine
auto-saves the graph's spatial state (screen, room, listener, enable) and
restores it at construction, so a host's spatial tuning survives a
restart without any host-side bookkeeping.

### Added

- `engine::spatial_persistence` — control-path auto-save/restore of the
  active spatial scene. `SpatialPersistence` snapshots the `SpatialNode`
  surface into the existing `config::SpatialConfig` serde model, writes it
  atomically (temp + rename, so a crash can never corrupt the last good
  scene), and restores best-effort at engine construction.
- `EngineConfig::spatial_autosave_path` — optional explicit path for the
  auto-save file (default: `<user-data>/engine/spatial_scene.json`);
  hosts that want persistence elsewhere (or disabled) set this.
- Engine lifecycle hooks: `maybe_save` runs each tick after queued graph
  controls are applied and writes only when the state actually changed;
  `Drop` flushes pending controls and persists the final scene, so a
  graceful shutdown always restores exactly what was active.

### Fixed

- None.

## [3.21.0] — 2026-08-29

Measured HRTF corpus loading: the binaural path can now render real
recorded head-related impulse responses, not just the synthetic grid.

### Added

- `HrtfCorpus` / `HrtfMeasurement` — the SOFA data model (measurement
  directions + per-ear IRs + recorded rate) reduced to pure Rust; the
  exact reduction of a `.sofa` HDF5 export, so hosts can load real
  corpora (CIPIC, TU-Berlin, KEMAR, …) without HDF5 bindings.
- `HrtfDataset::from_corpus` — control-path corpus loading: validates
  finite unit directions and non-finite IRs, resamples every IR to the
  target rate (piecewise-linear), optional peak normalization,
  trims/pads to the FIR tap count, and requires a regular
  azimuth × elevation mesh (full Cartesian product) so bilinear
  interpolation stays exact; irregular meshes are typed errors.
- `save_hrtf_corpus_json` / `load_hrtf_corpus_json` — portable pure-Rust
  JSON interchange of the SOFA data model.
- `HrtfLoadOptions`, `HrtfNormalize`, `HrtfLoadError` — typed load
  controls and errors (empty corpus, bad taps, non-finite direction/IR,
  irregular mesh, JSON failures).

### Fixed

- Clippy `needless_range_loop` / constant-assertion warnings in the
  order-3 ambisonic test suites (`--workspace --all-targets` is now
  warning-free).

## [3.20.0] — 2026-08-28

The ambisonic layer extends from order 2 to **order 3** (Third-Order
Ambisonics, 16 channels). This is a backward-compatible capability: every
order-≤2 behavior — basis values, decode, and the exact rotation — is
pinned bit-for-bit identical to v3.19.0 by the existing unit and acceptance
tests.

### Added

- **Third-order SN3D basis** (`sh_n`, order 3): the seven ACN-9–15 cubic
  harmonics per the Furse–Malham table — `√(35/8)·x(x²−3y²)`, `√105·xyz`,
  `√(21/8)·y(5z²−1)`, `(√7/2)·z(5z²−3)`, `√(21/8)·x(5z²−1)`,
  `(√105/2)·z(x²−y²)`, `√(35/8)·y(y²−3x²)` — each validated to unit sphere
  mean-square (SN3D) and mutual orthogonality by a dedicated grid test. New
  `AMBISONIC_CHANNELS_ORDER_3 = 16` and `AMBISONIC_CHANNELS_MAX` channel
  constants.
- **Exact order-3 Wigner rotation** (`rotate_bus_frame_n`): the 7×7 block
  is the projection of the cubic triple-Kronecker action `R⊗R⊗R` onto the
  order-3 subspace, computed by coefficient linear algebra (monomial
  substitution under `R` + Gram projection), in f64 — so the defining
  property `sh_n(3, R·v) == W₃·sh_n(3, v)` holds to high precision on every
  channel. `AmbisonicRenderer` / `AmbisonicDecoder::with_order` now accept
  order 3; `process_bus` and the renderer scratch buffers grow to
  `AMBISONIC_CHANNELS_MAX` (still fully preallocated, zero allocations).
- **Third-order max-rE window**: the published Zotter–Frank weights
  `a1 ≈ 0.7660, a2 ≈ 0.6534, a3 ≈ 0.5715`, verifiable per-speaker against
  the closed-form decode.

### Tests

- Unit suite additions: `order3_basis_matches_documented_sn3d_convention`
  (cardinal-direction values + order-1/2 rows unchanged), `
  order3_norm_preserving_on_the_sphere` (mean-square 1 + orthogonality),
  `order3_rotation_is_exact_on_the_basis` and `order3_rotation_round_trips_
  to_identity`, and the rejection test now checks order 4 is unsupported.
  Total 137 spatial lib tests.
- Acceptance suite `spatial_hoa` gains three order-3 tests (conventions,
  exact rotation + world-fixed end-to-end decode, and the max-rE window vs
  the closed form + max-rE-over-basic rear-lobe narrowing). 9 tests green.
- Three implementation defects were caught while extending:
  - A test-side arithmetic error asserted `Y₃⁰(+Y)=√7` (really 0; `z=0` at
    +Y) and `Y₃¹(+Y)=−√(21/8)` (really ACN 11); corrected to pin `Y₃³(+Y)`
    and `Y₃⁻¹(+Y)`.
  - The order-3 rotate arm initially accumulated `W₃ᵀ·c` (transposed);
    corrected to the direct `W₃·c` convention (order-2's block is stored
    transposed, order-3's is direct — now documented in the code).
  - A flawed acceptance assumption (order-3 max-rE rear lobe narrower than
    order-2's) was wrong; replaced with the per-order-3 meaningful invariant
    (max-rE vs basic at order 3).

## [3.19.0] — 2026-08-28

The spatial layer's capstone: **higher-order ambisonics**, a **SpatialNode**
in the production DSP graph, **measured spectral HRTFs** with elevation
cues, and the **scene-file format**. Together they close the remaining
roadmap phases: order-2 SH rendering (roadmap Phase 16), the spatial master
output stage as a first-class graph node (Phase 17), full head-related
impulse responses replacing the analytic shelf (Phase 18), and Serde
scene persistence (Phase 19).

### Added

- **Higher-order ambisonics (Phase 16, spec §34)** — `ambisonic.rs` is
  rewritten around the exact order-N SH basis (`sh_n`, `channel_count`):
  order-1 (FOA) behavior is pinned bit-for-bit identical to the previous
  release, and order-2 adds the 5 new channels (`U`, `V`, `T`, `R`, `S`)
  per the published Furse–Malham table. The decoder weights are the
  published max-rE window (order-2 `a1 ≈ 0.9057`, `a2 ≈ 0.6827`; order-1
  stays `√3/2`), `AmbisonicRenderer::with_order` renders any supported
  order to any speaker layout, and the bus rotation is the **exact**
  order-2 rotation (WXYZ interleaved with UV) — a 90° yaw moves a plane
  wave to the correct column, pinned by a dedicated test.
- **SpatialNode in the production graph (Phase 17)** — a new `SpatialNode`
  plan node: a stereo master is spatialized through the binaural head model
  (optionally with the room); multichannel masters pass through untouched
  (documented seam). It ships with a full control surface
  (`set_spatial_enabled/screen/room/listener`), a config section
  (`SpatialConfig` + `SpatialRoomConfig` in `engine_config`), a per-node
  atomic control mirror applied at block boundaries, and live enable
  surviving generation rebuilds (swap replay). Zero allocation on the
  audio thread, pinned by `realtime_allocation`.
- **Measured spectral HRTFs (Phase 18, spec §62)** — `HrtfDataset`:
  a grid of per-ear impulse responses (azimuth × elevation) with bilinear
  interpolation (azimuth wrapped continuously across the 360° seam),
  loaded on the control path and validated (`from_planes` rejects
  non-monotonic grids and non-finite IRs). `BinauralRenderer::use_dataset`
  switches object direct paths from the analytic chain to FIR convolution
  of the interpolated IR (which carries both ITD and spectral cues); the
  analytic path gains `ElevationNotch`, a documented pinna-notch biquad
  (`f = 6 kHz + 4 kHz·sin(el)`, depth `−8 dB·|sin(el)|`, an exact
  passthrough at 0° elevation). A synthetic dataset generator discretizes
  the analytic model on a regular grid so the FIR path is testable without
  shipping a measured corpus.
- **Scene file format (Phase 19, spec Part XXVI)** — `config::
  SpatialSceneConfig` (+ `SceneListenerConfig`, `SpatialObjectConfig`,
  `SpatialBedConfig`, `SpatialFieldConfig`): a Serde-serializable,
  renderer-independent scene model (listener, objects, beds by semantic
  role names, fields, room). `SpatialScene::from_config` / `to_config`
  convert losslessly (listener orientation stays the canonical quaternion —
  no Euler drift), `save_scene_json` / `load_scene_json` are the file I/O
  with typed errors, and `validate` enforces the engine's caps with rich
  messages before anything reaches the audio thread. Optional fields
  default (`#[serde(default)]`) so older hosts keep reading newer files.

### Fixed

- **Woodworth ITD folding for wrapped azimuths** — `woodworth_itd_sec`
  folded angles past ±π with `rem_euclid(...).min(π)`, mapping 300°
  (physically −60°) to 180° (zero ITD), and `ear_delay_sec`'s left/right
  side test used `signum(azimuth)`, which is wrong for azimuths wrapped
  past ±π. Both now fold by reflection / use `sin(azimuth)` so a 0–360°
  grid (the dataset convention) is rendered correctly; the renderer's
  signed azimuths were unaffected. Pinned by new unit tests and by the
  dataset-path mirror-symmetry acceptance test.

### Tests

- Unit suites: `ambisonic.rs` (order-2 basis orthonormality, channel
  table, max-rE weights, the exact rotation property, order-2 renderer),
  `hrtf.rs` (dataset structure, bilinear exactness/wrap, synthetic IR
  pins, notch passthrough/elevation), `binaural.rs` (dataset path renders
  the IR exactly, mirror symmetry in both paths), `scene.rs` (config
  round-trip, JSON IO, validation), plus SpatialNode graph tests.
- Four acceptance suites: `tests/fidelity/spatial_hoa.rs` (order-2
  rendering to 7.1.4, rotation, per-order weights), `spatial_node.rs`
  (bit-exact passthrough when disabled, binaural ITD on the graph output,
  room tail, listener yaw, control surface, reconfig survival, MC seam),
  `spatial_hrtf_ir.rs` (dataset IR fidelity, mirror symmetry, elevation
  notch, determinism), and `spatial_scene.rs` (lossless round-trip renders
  bit-identical, forward-compatible defaults, validation rejections,
  renderer independence, quaternion fidelity).
- `realtime_allocation`: three new zero-alloc tests — order-2 ambisonics
  with per-frame exact rotation, the SpatialNode with a room under a
  block-rate listener sweep, and the HRTF dataset path with the worst-case
  order-2 room. 18/18 tests, **zero allocations**.

## [3.18.0] — 2026-08-28

Head tracking (roadmap Phase 15, spec §48, §136): the VR/AR seam. A new
`spatial::tracking` module turns a stream of timestamped orientation
samples (an IMU, a webcam, a game engine's VR rig — anything that can
produce a `Quat`) into a smooth current head orientation the host applies
to the scene listener before each render block. The renderers never
change: the listener's orientation was already a first-class transform,
so tracking is purely a control-side interpolation + smoothing problem.
The `HeadTracker` shortest-path nlerps across the last two samples, feeds
an exponential (one-pole) filter on the orientation error (τ in ms; `0`
snaps exactly), and can rate-limit the angular step (deg/s) so a violent
head jump or sensor glitch cannot fling the soundfield.

### Added

- **`spatial::tracking`** — [`HeadTracker`], [`HeadSample`] (timestamped
  orientation), [`TrackingConfig`] (`smoothing_ms`, `max_angular_rate_deg_s`):
  `push` ingests samples (host thread), `sample(now)` returns the smoothed
  current orientation, `apply_to(&mut listener, now)` is the per-block host
  convenience. Pure fixed-size state — allocation-free and lock-free, so it
  can run on the audio thread's caller.
- **Quaternion interpolation** (`math`): `Quat::nlerp` (shortest-path,
  normalized linear interpolation), `Quat::angle_to`, `Quat::dot`,
  `Quat::normalized`, `Quat::negated`, `Quat::length`, `Quat::is_finite`,
  and `Add` / `Mul<f32>` — all unit-tested (midpoint pins, shortest-arc
  wrap, `q` vs `−q`).

### Tests

- Unit suites in `tracking.rs` (segment interpolation vs the closed form,
  shortest arc, smoothing ramp + convergence, exact mode, rate-limit cap,
  reset/first-sample snap, `apply_to`) and `math.rs` (nlerp endpoints /
  midpoint / shortest arc, `angle_to` bounds, scalar ops).
- Acceptance suite `tests/fidelity/spatial_tracking.rs` (6 tests): the
  headline Woodworth consistency — a 137° tracked yaw sweep renders a
  world-fixed source with the closed-form `itd(az, L) − itd(az, R)` ear lag
  at every block; the frozen-image contrast (same sweep without applying
  the tracker); smoothing gliding the image without zipper; a 5.1 panner
  moving the image from the side pair to the front/center as the head turns;
  tracker determinism + rate-limit capping end to end; and `apply_to`
  updating the listener (plus the renderer using it, pinned via the ITD).
- `realtime_allocation`: new `realtime_head_tracker_does_not_allocate` — a
  10k-sample jittery stream with block-rate sampling, **zero allocations**.

## [3.17.0] — 2026-08-28

Binaural rendering (roadmap Phase 14, spec Part VII §47–48, §62, §136):
the spatial layer gains a **head model** — a `BinauralRenderer` that
renders the entire hybrid scene (objects, beds, fields, and the room's
reflections) to two ears using the documented Woodworth interaural time
difference and a Duda-Martens head-shadow shelf, with no speaker array.
A new `spatial::hrtf` module ships the open model (ITD formula, `α`
coefficient, a first-order shelf filter, and the fractional-delay ring read
that makes ITD changes glide); `spatial::binaural` assembles the renderer:
objects through the shared level chain then per-ear delay + shadow (spread
blurs the interaural cues instead of moving the image), beds folded by
semantic role (LFE at `1/√2`), and diffuse content (fields + the room's
late field) decoded onto a virtual 8-speaker ring before the head model —
surrounding ambience, not a phantom. `RendererKind::Binaural` joins
`Basic` / `Vbap` / `Ambisonic`; `prepare` requires exactly two enabled
non-LFE speakers (stereo/headphone layouts).

### Added

- **`spatial::hrtf`** — `woodworth_itd_sec` (front 0 → ear-axis maximum
  `(a/c)(π/2+1)` → rear 0, the documented front/back cone ambiguity),
  `head_shadow_alpha` (`1.05 + 0.95·sinφ`: ≈ 2.0 at the ear, ≈ 0.1
  shadowed), `Ear`, the `HeadShadow` first-order shelf (DC gain exactly 1,
  HF asymptote exactly α, one-pole-smoothed α), and the fractional
  `read_delayed` ring read — all pinned by unit tests.
- **`spatial::binaural::BinauralRenderer`** — full hybrid rendering to a
  stereo interleaved buffer: the object level chain (distance ·
  directivity · occlusion) then per-(direction, ear) Woodworth delay +
  shadow shelf; angular-region spread renders every sampled direction with
  its own cues; the LFE send and bed-LFE role fold at `1/√2`; beds route
  by semantic-role azimuth; fields and the room's late field decode onto a
  virtual 8-speaker ring (`√N`-compensated, decorrelated) and are
  head-modeled per virtual speaker; room image sources are binauralized at
  `excess_path + ITD(ear)` with per-(image, ear) shadow and smoothed tap
  gain.
- **`RendererKind::Binaural`** — non-exhaustive enum extension; the
  previously-declared `RenderError::HrtfUnavailable` seam is now real.
- **`EarlyReflections` binaural primitives** — `cursor_at`, `store`,
  `read_delayed` (fractional), `add_send` for the renderer's own head-model
  taps.

### Tests

- Unit suites in `hrtf.rs` (Woodworth closed form, per-ear delay rules, α
  complement, shelf DC/Nyquist/α=1 pins, fractional interpolation) and
  `binaural.rs` (stereo-layout enforcement, front unity, exact ear swap
  under mirror symmetry, LFE fold, spread's effective-ITD shrinkage).
- Acceptance suite `tests/fidelity/spatial_binaural.rs` (11 tests): the
  Woodworth closed form at the public API, front-center unity and balance,
  hard-right ITD + head shadow measured on impulse argmax, mirror
  symmetry, bed role/LFE folds, diffuse equal-ear-energy fields with
  decorrelation, the 280-sample room reflection arriving one ITD later at
  the contralateral ear, listener-rotation image motion, spread's effective
  ITD reduction, deterministic finite full-hybrid rendering, and layout
  validation.
- `realtime_allocation`: new `realtime_spatial_binaural_does_not_allocate`
  — order-2 room, occluded/directional/spread/LFE objects, a bed, a field,
  and a sweeping listener yaw; 10k blocks, **zero allocations**.

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
