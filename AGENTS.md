# AGENTS.md

Guidance for AI coding agents (and humans) working in this repository.

## Project snapshot

**Freebuff Desktop** is a headless, high-performance, bit-perfect audiophile audio
playback & DSP engine written in 100% pure Rust. It is a Cargo workspace:

```
├── Cargo.toml                  # workspace + `engine` crate (the library/bins)
├── crates/config/              # `config` crate — Serde-serializable engine & DSP config models
├── src/                        # `engine` crate
│   ├── lib.rs                  # crate root + prelude re-exports
│   ├── source.rs, playlist.rs, commands.rs, events.rs, playback_info.rs,
│   │   audio_io.rs, sink.rs, ffi.rs
│   ├── engine/                 # core state machine (tick loop, handle, stream, commands/, decode_loop/)
│   ├── decode/                 # decoders + channel layout/mix + tags + fingerprint
│   ├── dsp/                    # DSP primitives + `pipeline/` (production chain) + `graph/` (experimental)
│   ├── output/                 # per-OS backends (alsa/wasapi/asio/coreaudio/cpal) + capture
│   ├── buffer/                 # lock-free SPSC ring buffers
│   └── bin/                    # `audio-engine-cli`, `replaygain-scanner`
├── docs/                       # ARCHITECTURE.md, SIGNAL_FLOW.md
├── benches/                    # dsp_bench, pipeline_bench
└── tests/                      # headless + `tests/fidelity/` DSP/decoder suites
```

Two crates ship versions that **must stay in lockstep** (see Versioning):
`engine` (workspace root) and `config` (`crates/config`).

## Versioning — Semantic Versioning (`x.y.z`)

Adopt strict [Semantic Versioning](https://semver.org) with the form
`MAJOR.MINOR.PATCH`:

- **`x` (major)** — incompatible, breaking public API / C-FFI surface, a behavior
  change that is not backward-compatible, or a semantic redefinition of a public
  type/feature-gate. Examples: renaming/removing a public type, reordering an FFI
  struct field, changing `EngineEvent` variants, dropping a default feature.
- **`y` (minor)** — a backward-compatible addition: new public API, new optional
  module/feature-gate, new codec, new DSP stage, new command/event (added, not
  changed).
- **`z` (patch)** — backward-compatible bug fixes, doc updates, and performance
  work that does not change behavior or API.

### Rules — every version bump MUST do all of this in the same commit/PR

1. Bump **both** crate versions in lockstep:
   - `Cargo.toml` → `[package] version` for `engine`
   - `crates/config/Cargo.toml` → `[package] version` for `config`
2. Add a dated `## [X.Y.Z] — <ISO date>` section at the **top** of `CHANGELOG.md`,
   with `### Added`, `### Fixed`, `### Changed` subsections as applicable. Keep the
   existing entry format; pre-release segments are discouraged for this project.
3. Tag the release commit with a `vX.Y.Z` git tag.
4. Update any version references in `README.md` / `docs/` if they cite the version.

### When to bump

- Any user-visible or API change → at least a **patch**.
- Any new backward-compatible capability → a **minor**.
- Any breaking change → a **major**.

**Example.** Adding a new public `EngineHandle::set_gain` method = minor → `3.1.0`.
Fixing a limiter off-by-one bug = patch → `3.0.1`. Removing the `CpalOutput` type
= major → `4.0.0`.

## Modularity — no god files

This codebase is deliberately modular. Do **not** create god files (a.k.a. god
objects / God modules): oversized files or single types/impls that own multiple
unrelated responsibilities.

### What counts as a god file

A file is a god file if it exhibits **two or more** of these signals:

1. **Size** — over ~800–1000 lines in a single `.rs` file.
2. **Multiple unrelated concerns** — a `struct` + `impl` that both configures,
   processes, mutates lifecycle, reports diagnostics, and owns internal state of
   many subsystems (a "supervisor" that does everything).
3. **Breaks the existing layout** — code exists where a sibling-concern module
   already provides a natural home.
4. **Many small "plumbing" methods** that merely forward to fields, signaling the
   type should be decomposed.

Large files are **fine** when they are cohesive: a single-purpose DSP algorithm
(`dsp/limiter.rs`, `dsp/convolution.rs`, `dsp/timestretch.rs`), a single
self-contained OS backend (`output/*_output/`), or a test file packed with cases.

### House pattern — split large impls by concern

The canonical precedent is **`src/dsp/pipeline/`**: the `DspPipeline` struct and
its wiring live in `mod.rs`, while its behavior is split across concern-scoped
impl-block files that each declare `mod x;` in `mod.rs` and open with
`impl DspPipeline { … }`. `src/dsp/graph/` follows the same layout
(`construction.rs`, `lifecycle.rs`, `process.rs`, `limiter.rs`, `report.rs`).

When a struct/impl grows, prefer splitting like this:
- `mod.rs` — module docs, wiring, `pub struct`, `mod` declarations
- One concern-scoped file per responsibility (e.g. `construction.rs`, `process.rs`,
  `lifecycle.rs`, `report.rs`)

Keep submodule `use super::…` imports explicit and local to the file that needs
them. Keep node/primitive definitions next to their modules; only move **impl
blocks**, never the data definitions, unless the split is purely additive.

### Enforcing this during review

- If a PR adds a file that trips two or more god-file signals, **stop and split it**
  before merging.
- When adding a method to an already-large type, place it in the concern file that
  matches its job rather than growing a different concern file.
- A reviewer MUST check for the signals above on every PR touching `src/`, and
  run the affected module's tests (e.g. `cargo test --lib dsp::graph`).

## Completeness checklist

Before considering a change "complete", verify:

- [ ] **Versions in sync**: `engine` and `config` Cargo.toml versions match the new
      CHANGELOG entry (see Versioning).
- [ ] **CHANGELOG updated** at the top with a dated section for every user-visible
      change.
- [ ] **Crate metadata in sync**: for both the `engine` and `config` crates, the
      `[package]` metadata in `Cargo.toml` must match the README and each other:
      - `license` is `Apache-2.0` in both manifests and the README declares the
        Apache-2.0 license.
      - `repository` points to the current git remote and is identical in both
        manifests (compare with `git remote get-url origin`).
      - `homepage` / `documentation` / `readme` fields, when present, point to real,
        reachable URLs and are linked consistently in the README.
      - Verify with `cargo metadata --no-deps --format-version 1` and confirm the
        `license`, `repository`, and `homepage` values for both packages match the
        README's claims.
- [ ] **CI green**: `cargo fmt --all -- --check`, `cargo clippy --workspace
      --all-targets -- -D warnings`, and `cargo test --workspace` pass. Optional
      feature builds (`tag-write`, `fingerprint`, `wasapi-native`, `asio-native`)
      compile when the change touches those paths.
- [ ] **Docs consistent**: `README.md`, `docs/ARCHITECTURE.md`, `docs/SIGNAL_FLOW.md`,
      and `docs/EMBEDDING.md` still describe the real layout and behavior; update the
      module map when you add/move/remove a module.
- [ ] **License file present**: `LICENSE-APACHE` exists at the repo root (do not
      remove it), the Cargo.toml `license` field is `Apache-2.0`, and the README
      declares the Apache-2.0 license — all three must stay in sync.
- [ ] **No god files introduced** and the modular layout is maintained (see above).
- [ ] **Realtime rules honored**: no heap allocation on the decode/DSP hot path, no
      locks (atomics + SPSC ring only); add/adjust `tests/fidelity/realtime_allocation.rs`
      when the hot path changes.

## Testing

- Run unit + headless tests with `cargo test`.
- DSP fidelity/measurement suites live under `tests/fidelity/` and are named in
  `Cargo.toml` `[[test]]` entries (e.g. `--test limiter_correctness`,
  `--test golden_reference_vectors`).
- Realtime zero-allocation: `cargo test --test realtime_allocation`.
- Always re-run the relevant module tests after a modularization/split change
  (e.g. `cargo test --lib dsp::graph`).