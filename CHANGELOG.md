# Changelog

All notable changes to this project are documented in this file.

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
