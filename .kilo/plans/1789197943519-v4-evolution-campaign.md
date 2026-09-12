# Shadow Desktop — v4 Evolution Campaign (Phases 45–57)

A balanced multi-track campaign: **Graph2 full replacement of the hot path**,
**Rust-native plugin hosting**, **all four spatial horizons**, **gapless
queue/library/playback polish**, and a **WebSocket/JSON remote API** — sequenced
core-first, each phase landing as its own release per the AGENTS.md versioning
rules.

## Resolved decisions

| Decision | Choice |
|---|---|
| Flagship | Balanced campaign, core first |
| Graph2 end state | **Full replacement** of `dsp::graph` on the audio thread |
| Legacy `dsp::graph` | Frozen through one release (equivalence oracle), then **removed** → **v4.0.0** |
| Codecs/playback | Gapless queue + cache, library + playlists, playback polish. CUE/BWF out of scope |
| Plugins | **Rust-native spec** — no VST3/CLAP FFI hosting this campaign |
| Remote control | **WebSocket/JSON** API riding `EngineCommand`/`EngineEvent` |
| Spatial | All four horizons: acoustic agreement, listener motion, scene animation events, diagnostics |
| Sequencing | Track A (core) → B–E build on the new core |

## Non-negotiables (every phase)

- No heap allocation, no locks on the audio path (extend
  `tests/fidelity/realtime_allocation.rs` to cover every new hot-path executor).
- **Disabled-exact**: a feature that is off is bit-absent from the executed
  expression; `graph_pipeline_equivalence` (and its Graph2 successor) stays
  bit-exact across phases.
- Both crate versions bumped in lockstep + dated CHANGELOG + `vX.Y.Z` tag per
  phase-release; module map in AGENTS.md / ARCHITECTURE.md updated when modules
  move.
- No god files: split any file crossing two of (size > ~1000 lines, multiple
  unrelated concerns, breaks existing layout, forwarding plumbing) before merge.
- CI per phase: `cargo fmt --all -- --check`, `cargo clippy --workspace
  --all-targets -- -D warnings` (+ optional-feature clippy), `cargo test
  --workspace`.

---

## Track A — Core: Graph2 realtime full replacement (Phases 45–48)

### Phase 45 — Realtime lowering substrate (v3.50.0) — **DONE (2026-09-12)**

**Goal.** Make a compiled `ExecutionOrder` executable on the audio thread with
zero allocation, without touching the engine yet.

1. **Split `graph2/exec.rs` (2,434 lines)** into `graph2/exec/` by concern
   (`mod.rs` wiring, `offline.rs` offline executor, `ops.rs` shared node
   processing kernels, `buffers.rs` scratch/buffer management) — the
   `dsp/pipeline/` house pattern. Node *definitions* stay in `node.rs`.
2. **New `graph2/rt/` realtime executor**: preallocated per-port plane pools
   sized from the compiled plan (port specs declare channel counts), fixed
   scratch, enum-dispatch per block — never trait-object dispatch. Building the
   plan and pools is control-thread work; the audio thread receives an immutable
   `RtPlan` behind an atomic pointer publish (the Phase-2 generation-swap
   discipline, reused).
3. **Node-op sharing contract**: one set of per-node processing kernels used by
   *both* executors (offline renders through the same math as realtime), so
   offline/realtime divergence is structurally impossible.
4. **Validation**: new fidelity suite `tests/fidelity/graph2_rt_offline_equivalence.rs`
   (bit-exact across topologies incl. multi-plane, delay-compensated,
   resampler, acoustic, convolver, hrtf nodes); `realtime_allocation` extended
   with a `graph2_rt` case; existing graph2 offline tests stay green.

### Phase 46 — Node parity: port the production node set (v3.51.0)

**Goal.** Every node the engine's hot path depends on exists as a Graph2
`NodeKind` with identical math.

1. Port from `dsp::graph` into `graph2` node kinds (reusing the Phase-45 kernel
   seam so there is exactly one implementation): `MixBusNode` (incl. N-slot
   envelope/sum, per-slot trims, sends, ducking, automation), `AuxNode`,
   `SpatialNode`, `AcousticNode`, resampler node, convolver, limiter, dither,
   channel-format nodes. Port **behavior**, not files: `dsp::graph/nodes/mix/`
   logic is refactored into shared kernels that both crates call until Track-A
   completes.
2. Port the control surface: per-node SPSC control queues + sticky-atomic
   control mirrors, replayed on generation swap (the `dsp::graph` discipline).
   `Graph2ControlHandle` mirrors the existing `GraphControlHandle` API so
   command handlers need only a type swap.
3. Latency/tail/bit-perfect capability descriptors flow through
   `graph2/latency.rs` (already exists) so the alignment pass covers ported
   nodes.
4. **Validation**: new suite `tests/fidelity/graph2_graph_equivalence.rs` — for
   every scenario in `graph_pipeline_equivalence`, the equivalent Graph2
   topology matches `dsp::graph` bit-exactly (297+ scenarios). Golden vectors
   unchanged.

### Phase 47 — Engine migration + shadow equivalence (v3.52.0)

**Goal.** The decode loops drive Graph2; `dsp::graph` runs in shadow mode for
one release.

1. Swap `self.graph.process_block*` call sites (`engine/decode_loop/single.rs`,
   `crossfade.rs`, `engine/commands/playback.rs`, `engine/tick.rs`) onto the
   Graph2 RT executor. The default engine topology is **constructed to be
   behavior-identical** to today's chain; per-config generation swaps rebuild it
   (add/remove lanes, aux, spatial) exactly as today.
2. **Shadow mode**: a `config` flag runs `dsp::graph` on the same input and
   asserts bit-equality every block (control thread, results dropped on
   mismatch + diagnostic). Enabled in CI fidelity runs, off by default for
   performance.
3. All engine command/event/telemetry surfaces unchanged: same
   `EngineCommand`s, same `PlaybackInfo` (ArcSwap publication continues),
   same FFI behavior. This is a minor release because public API is unchanged.
4. **Validation**: full `cargo test --workspace`; shadow equivalence green on
   all platforms in CI; `realtime_allocation` now exercises the Graph2 path in
   the engine loops.

### Phase 48 — Legacy `dsp::graph` removal (v4.0.0)

1. Remove `dsp::graph` (arena/plan/nodes/handlers) and the shadow mode;
   `DspPipeline` **stays** as the frozen bit-exact oracle
   (`graph_pipeline_equivalence` re-points at Graph2-vs-pipeline).
2. Public API break documented in CHANGELOG `### Changed/Breaking`; `graph2`
   re-exports replace `dsp::graph` exports in `lib.rs`/prelude; FFI unchanged
   in behavior.
3. Housekeeping: `dsp::graph` god files (`controls.rs` 1,906 lines) disappear
   with the crate; `ffi.rs` (1,587 lines) split by concern per house pattern if
   the removal touches it.
4. **Version**: **v4.0.0** — major, breaking removal of `dsp::graph` public types.

---

## Track B — Rust-native plugin spec (Phase 49, v4.1.0)

**Goal.** Host external pure-Rust effect plugins on the aux/master insert seams
with the same realtime discipline.

1. **Spec crate** (workspace member `crates/plugin-abi`): versioned C-ABI vtable
   (CLAP-style: a struct of fn pointers, not Rust traits across dylib
   boundaries — Rust has no stable ABI). Plugin side gets a safe Rust facade
   (`#[export_plugin]`-style builder) so authors never touch unsafe.
   Capabilities: typed ports (reuses `PortSpec`/`SignalType`), parameter list
   with ranges/defaults, latency declaration, tail length, activate/deactivate,
   state save/restore (serde). ABI version negotiated at load; mismatch refuses
   with a typed error.
2. **Host side**: loader (control thread, `libloading`-free via `dlopen`/`LoadLibrary`
   through a small existing-dependency seam — audited unsafe), validation
   (ports/latency declared sane), and a **Graph2 plugin node** whose RT plan
   embeds a preallocated io buffer pair; parameters ride the per-node SPSC
   queue; latency feeds the Phase-28 alignment pass. Bypass = plan without the
   node (disabled-exact).
3. **Reference plugin**: `crates/plugin-test-echo` (a delay + gain) exercised
   by a fidelity suite (`plugin_host.rs`: load, insert, automate parameter,
   latency-compensate, save/restore state, bit-exact bypass, unload during
   playback without glitch). Also a "worst-case" plugin that allocates in
   process to prove the host survives misbehaving plugins (documented limit:
   allocation is a plugin contract violation, host can only watchdog).
4. CI: cross-compile check of the plugin ABI crate for the three desktop OSes.

## Track C — Spatial horizons (Phases 50–53, v4.2.0–v4.5.0)

### Phase 50 — Acoustic agreement (v4.2.0)
1. Fold per-path distance roll-off into the **realtime** reflection low-pass
   corner so production spatial path and offline `Acoustic` node agree on
   distance colour (the documented Phase-44 horizon).
2. Frequency-dependent attenuation model richer than the one-pole
   (magnitude-approximation families, DC-exact, disabled-exact default).
3. Late-field distance roll-off in `spatial/room` late field.
- **Validation**: bake-vs-realtime agreement suite; golden renders unchanged
  with model off.

### Phase 51 — Listener motion (v4.3.0)
1. Runtime-editable listener rotation/position on the realtime `SpatialNode`
   (today only IMU-driven tracking moves the listener) via SPSC scene-target
   commands, one-pole/nlerp smoothing per the tracking conventions.
2. Smooth re-bake seam: when a moving listener crosses baked-scene relevance
   bounds, the control thread re-bakes and swaps the acoustic generation
   without audio interruption (Phase-2 swap machinery).
3. Telemetry: listener pose in `SpatialTelemetry`.
- **Validation**: moving-listener aelog replay tests; no glitch at swaps
  (`realtime_allocation` + continuity checks); persistence unaffected.

### Phase 52 — Scene animation events (v4.4.0)
1. Scene-file format extension (serde, `#[serde(default)]` for
   backward compat): per-object animation curves (position/gain/spread
   keyframes, looping, hold) + trigger events (named cues with timestamps,
   whoosh/door presets as composable parameter curves).
2. Realtime evaluation on the SpatialNode control path (curve interpolation in
   existing automation style — sample-accurate, monotonic cursor).
3. Events triggerable via `EngineCommand` and from the timeline scheduler
   (`dsp::timeline`), aelog-replayable.
- **Validation**: scene round-trip tests, legacy scenes load (fields absent =
   off, bit-exact), aelog replay of animated scenes, new `spatial_events`
   fidelity suite.

### Phase 53 — Spatial diagnostics (v4.5.0)
1. Per-node render-cost meters (block-time accumulators published on the
   control bus, not the hot path), tail-budget reporting, and scene-cost
   report rendered through `spatial/diagnostics.rs` + `eval::EvaluationReport`
   conventions (PASS/WARN table + JSON).
2. Health checks extended: warn when a scene's realtime cost exceeds a
   configurable block budget on the target endpoint rate.
- **Validation**: deterministic cost reports in tests (fixed scenes), CI asserts
   budget thresholds on the golden scenes; `realtime_allocation` unaffected.

## Track D — Playback & library (Phases 54–56, v4.6.0–v4.8.0)

### Phase 54 — Gapless queue + track cache (v4.6.0)
1. **Play queue** with shuffle (seeded, deterministic, saveable), repeat modes
   (extending `RepeatMode`), queue persistence to the config store; queue
   commands added (`QueueAdd/Remove/Move/Clear/Shuffle`).
2. **Track cache**: pre-scanned metadata + loudness/profile (`profile` module)
   stored per content hash (SHA-256 via aelog substrate), so queue advancement
   never re-analyzes.
3. **Primed decoders**: next-track decoder pre-opened and pre-rolled (first
   frames decoded into a primed buffer) so track transitions are
   sample-accurate with no decode hiccup — reuses the crossfade seam for the
   handoff. Same rate/format = true gapless; rate change = documented
   micro-crossfade through the existing resampler policy.
- **Validation**: extend `crossfade_gapless`/`gapless_seek`; new
  `queue_transition` suite (sample-exact splice at boundaries; primed decode
  deterministic; cache hit/miss paths).

### Phase 55 — Library + playlists (v4.7.0)
1. Library scan (walk directories, decode tags via existing tag layer, store
   in a content-addressed JSON or SQLite-free record store consistent with the
   existing config-store approach — no new heavyweight deps), incremental
   rescan by mtime/hash, watch folders via the existing `device_monitor`
   pattern (poll or OS watcher, control thread only).
2. M3U/M3U8/PLS import+export; smart collections (genre/artist/rating/year
   filters, saved queries).
3. Playlist persistence with per-track metadata overrides; ratings/tags
   write-back optional via `tag-write` feature.
- **Validation**: parser unit tests (fixture files, no network), library
  incremental-rescan tests, smart-collection query tests, CLI surface
  (`audio-engine-cli`) extended and smoke-tested.

### Phase 56 — Playback polish (v4.8.0)
1. Sample-accurate seeking across all codecs (seek-table verification where
   the format lacks exact frames — decode-to-target with bounded error,
   documented per codec).
2. True-Peak scanner integrated into the queue/cache pipeline (uses
   `dsp/true_peak.rs`), per-track replaygain-style loudness normalize policy
   options (off/album/track) applying existing loudness state without
   re-analysis.
3. Per-track sample-rate policy: auto-switch device rate when the endpoint
   supports the track's native rate (rate-policy negotiation via existing
   `rate_policy.rs`), with user override — a config + `EngineCommand` surface.
- **Validation**: seek-accuracy suite (frame-indexed fixtures), true-peak
  against reference vectors, rate-switch glitch tests (device re-init path
  exercised on CI via wav_writer sinks).

## Track E — WebSocket/JSON remote API (Phase 57, v4.9.0)

1. New optional feature `remote-api` (pure-Rust server: `tungstenite`-family
   sync WebSocket + `serde_json` — no async runtime, consistent with the
   project's sync philosophy). New bin `engine-remote` hosting it.
2. **Protocol**: versioned envelope; commands mirror `EngineCommand` (whitelist
   — unsafe-in-remote commands like path access are opt-in via config),
   events mirror `EngineEvent`, telemetry snapshots pushed on subscription.
   Schema versioned like `VersionedConfig` with a migrate path.
3. Security: localhost bind by default, optional token auth (config), explicit
   opt-in for LAN bind; documented threat model in `docs/EMBEDDING.md`.
4. **Validation**: protocol round-trip tests over an in-process socket, event
   ordering under load, unauthorized-command tests; CLI `audio-engine-cli`
   gains `remote` subcommand using the same protocol.

---

## Cross-cutting work (folded into the phases that need them)

- **AGENTS.md / docs module map** updates at every module move (graph2 split,
  dsp::graph removal, new crates, new bins); EVOLUTION.md gets one section per
  phase in the established style.
- **CI additions**: plugin-ABI cross-checks (Phase 49), shadow-equivalence job
  (Phase 47), `remote-api` feature clippy/test (Phase 57), coverage of new
  suites in the existing matrix.
- **Versioning**: v3.50.0 → v3.52.0 additive; **v4.0.0** at Phase 48; then
  minors per phase to v4.9.0. Both crates lockstep, tags per release.
- **Pre-existing debt to clear opportunistically**: split `spatial/hrtf.rs`
  (1,577), `ambisonic.rs` (1,446), `binaural.rs` (1,356) when Tracks C touch
  them (cohesion check first — large cohesive DSP files are allowed);
  Cargo.toml comment drift (`codec-wavpack` note placement) fixed in the first
  version-touching PR.

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Graph2 lowering introduces allocation on the audio path | RtPlan immutable + preallocated pools; `realtime_allocation` covers the RT executor from Phase 45 onward; zero-alloc is a merge blocker, not a follow-up |
| Bit-exactness breaks mid-campaign (Phase 47) | Shadow mode + 297-scenario equivalence suite gate the migration; any mismatch is a release blocker |
| Rust plugin dylib ABI instability | C-ABI vtable (not Rust traits) across the boundary; versioned negotiation; safe facade crate |
| Dual-maintenance window (Phases 47–48) | Intentionally one release, not indefinite; `dsp::graph` is frozen (no new features) during the window |
| Graph2 node parity misses engine-only behavior | Phase 46 enumerates every `GraphControlHandle`/capability consumer and maps it to a test scenario before porting |
| Remote API command surface scope creep | Whitelist mirrors `EngineCommand` once; additions require a protocol minor bump |
| Realtime acoustic re-bake stalls on big scenes | Re-bake on control thread with budget guard; scene complexity diagnostics (Phase 53) land first in C ordering (50→53 order is deliberate: diagnostics before listener motion ships) |

## Validation plan (campaign exit criteria)

- All CI jobs green on all three OSes incl. optional-feature matrices.
- `realtime_allocation` proves zero allocation on: Graph2 RT executor, plugin
  node, animated spatial scenes, queue transitions.
- `graph_pipeline_equivalence` bit-exact (Graph2 vs pipeline oracle) at every
  phase boundary; golden vectors unchanged end-to-end.
- Full `cargo test --workspace` + all 60+ fidelity suites green at each release
  tag; CHANGELOG complete; AGENTS.md/docs maps accurate; versions in lockstep.

## Out of scope (this campaign)

- VST3/CLAP/LV2 hosting (the Phase-49 spec is the seam for a future adapter).
- CUE sheets, BWF/extra containers, new codecs.
- Any GUI (the remote API is the seam for one).
- Distributed/multi-host engine, cloud library sync.
