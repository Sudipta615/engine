<div align="center">

# Shadow Desktop — Independent Core Audio Engine

[![Crate Version](https://img.shields.io/badge/version-5.8.2-blue.svg?style=flat-square)](Cargo.toml)
[![License](https://img.shields.io/badge/license-Apache--2.0-green.svg?style=flat-square)](LICENSE-APACHE)
[![Rust Edition](https://img.shields.io/badge/rustc-1.85%2B%20%7C%202021-orange.svg?style=flat-square)](Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-lightgrey.svg?style=flat-square)](#-output-backends--os-integration)
[![Realtime Safety](https://img.shields.io/badge/realtime-0%20allocations%20hot%20path-brightgreen.svg?style=flat-square)](#-real-time-safety--concurrency)
[![Test Matrix](https://img.shields.io/badge/tests-1000%2B%20passing%20%7C%2058%20suites-success.svg?style=flat-square)](#-testing--quality-gates)

**A reference-grade, bit-perfect, headless audio playback and DSP engine written in 100% pure Rust.**  
Built for audiophile listening, pro-audio workstations, low-latency monitoring, and glitch-free realtime playback on modern and legacy hardware.

---

[Key Capabilities](#-key-capabilities) •
[Architecture](#-architecture-at-a-glance) •
[Signal Flow](#-dsp-signal-chain) •
[Quick Start](#-quick-start) •
[CLI Player](#3-reference-cli) •
[C-FFI](#4-c-ffi-c-c-python-c-nodejs) •
[Configuration](#-configuration-model) •
[Codecs & DSD](#-decoders-dsd--formats) •
[Testing](#-testing--quality-gates) •
[Documentation](#-documentation-index)

</div>

---

The engine is completely independent: **zero UI dependencies, zero database/library ties, zero playlist policy, and zero OS-specific application assumptions**. It embeds cleanly into CLI players, desktop GUIs (Slint, Iced, Qt, GTK, egui), streaming daemons, test harnesses, or pro-audio suites — and ships with a stable **C FFI** so it can be driven from C, C++, Python, C#, Node.js, and any language with C interoperability.

## 📚 Documentation Index

> - **[Canonical Specification](docs/ENGINE_SPEC.md)** — The authoritative engineering contract and architectural specification.
> - **[Owner's Guide](docs/OWNERS_GUIDE.md)** — Plain-English, comprehensive full-system map and subsystem guide.
> - **[Architecture](docs/ARCHITECTURE.md)** — Module map, concurrency model, and realtime-safety contracts.
> - **[Signal Flow](docs/SIGNAL_FLOW.md)** — Exact sample-level signal path, precision tiers, and bypass modes.
> - **[Embedding Guide](docs/EMBEDDING.md)** — End-to-end integration manual for Rust applications and C-ABI hosts.
> - **[Contributing & Versioning](AGENTS.md)** — Development rules, lockstep SemVer policies, and quality checklists.

---

## ✨ Key Capabilities

| Capability | Engineering Significance |
|---|---|
| **100% Pure Rust** | No C/C++ codec SDKs, no `unsafe` on the DSP hot path, and no FFI dependencies required for decoding. Fully auditable, memory-safe, and effortlessly cross-compilable. |
| **Graph 2.0 Runtime DSP Core** | A node-based arena graph with **compiled execution plans lowered from a typed-port Graph 2.0 topology** serves as the production hot path. Stage order is data, not code. Full reconfigurations swap live at block boundaries with **zero allocation and zero locks** on the audio thread. |
| **N-Input Mix Bus** | The primary stream, the crossfade partner, and **independent lane tracks** each ride dedicated bus slots with per-slot trim, post-fader sends, pan, mute, program-gated ducking, and sample-accurate automation tracks. |
| **Dedicated Aux Bus Node** | Per-slot aux sends are independently automatable (ramped, click-free), metered per send, and returned into the master before the post-mix chain — featuring an optional convolution **insert** (reverb / cabinet simulation) directly on the send accumulator. |
| **Multi-Endpoint Routing Matrix** | Fan out the master mix to **multiple physical output devices simultaneously**. Each endpoint runs its own realtime worker thread, independent resampler, private SPSC ring, and **per-endpoint clock-drift correction** (Rubato `Slip` trimmed to the device crystal to prevent ring buffer overflow/underflow). |
| **Bit-Perfect Direct Endpoints** | Native OS-level exclusive backends: ALSA direct `hw:` / `plughw:`, WASAPI Exclusive (`IAudioClient`), Steinberg ASIO (`IASIO`, pure Rust with no C++ SDK), and CoreAudio Hog-Mode — each verified against the OS before claiming the device, providing honest bit-perfect telemetry. |
| **Mastering-Grade Dual Precision** | Every DSP stage runs in fast single-precision **f32** (Performance) or double-precision **f64** (Quality), selectable per session. |
| **Real-Time Zero-Allocation Hot Path** | **Zero heap allocations** during steady-state decode and DSP processing (verified by 38 tests in `tests/fidelity/realtime_allocation.rs`). Cache-padded lock-free SPSC ring buffers; strictly no locks on the audio path. |
| **Gapless & Crossfade Transitions** | Sample-accurate **gapless transitions**, customizable **crossfade** (constant-power, linear, exponential, logarithmic, S-curve), transition fades, and clean seek-fade operations. |
| **Audiophile Codecs & 1-Bit DSD** | FLAC, ALAC, WAV, AIFF, APE, WavPack, TTA, Opus, Ogg Vorbis, AAC, MP3 — plus native **DSD (DSF/DFF)** up to DSD512 over Native wire and DoP (DSD-over-PCM). |
| **Immersive Multichannel** | Mono up to 7.1.4 (12 channels) and custom arrays up to 16 channels, featuring active bass management, per-channel distance delay alignment, routing matrices, and per-channel parametric EQ. |
| **Spatial 3D Audio (Opt-In)** | World-space **objects** (directivity, occlusion, spread), channel-based **beds**, diffuse **fields**, and **room acoustics** (image-source early reflections + Schroeder late field) rendered via equal-power `BasicPanner`, 3D **VBAP**, **Ambisonics** (FOA & HOA Order 1..3 with exact rotation), or **Binaural HRTF** (Woodworth ITD + Duda-Martens shadow + pinna notch + measured spectral SOFA datasets). Includes **head tracking** with smooth interpolation. |
| **Plugin Host & Process Sandbox** | Versioned C-ABI plugin interface (`crates/plugin-abi`) supporting in-process effects as well as a true **out-of-process crash-isolated sandbox** over planar binary IPC with automatic zero-allocation dry-audio failover on worker fault. |
| **Real-Time Telemetry & Analyzer** | Lock-free peak / RMS / dominant-frequency analysis, FFT spectrum taps, CPU load, and u64 hardware clip/underrun/overload counters published lock-free via `ArcSwap<PlaybackInfo>`. |
| **Loudness & Tag Write-Back** | Integrated ITU-R BS.1770-5 / EBU R128 and ReplayGain 2.0 measurement, volume normalization, and metadata tag write-back (`tag-write` via `lofty`) across major containers. |
| **Stable C FFI** | Exposes the complete engine surface — transport, DSP graph, multi-track lanes, playlist, multi-endpoint matrix, and aux inserts — through an opaque C-compatible ABI. |

---

## 🏗 Architecture at a Glance

```text
                     Host Application (GUI / CLI / FFI / Daemon)
                          │  EngineCommand (one-way control)
                          │  EngineEvent / OutputEvent (discrete lifecycle)
                          ▼
                     EngineHandle ──────────────────────────────┐
                          │                                     │ lock-free
                          ▼                                     ▼
                   Command Channel                      ArcSwap<PlaybackInfo>
                          │                               (atomic snapshot)
                          ▼
 ┌────────────────────────── AUDIO ENGINE CORE ───────────────────────────────────┐
 │                                                                               │
 │  ┌─────────────────┐       ┌─────────────────┐       ┌──────────────────────┐  │
 │  │   Decode Loop   │ ────▶ │  N-Slot Mix Bus │ ────▶ │  Graph 2.0 DSP Core  │  │
 │  │ (Decoders, SPSC,│       │ (Primary track, │       │ (Compiled plan: mix, │  │
 │  │  resamplers)    │       │  lanes, ducking)│       │  aux, EQ, dynamics,  │  │
 │  └─────────────────┘       └─────────────────┘       │  spatial, limiter)   │  │
 │                                                      └──────────────────────┘  │
 │                                                                  │             │
 │                                                                  ▼             │
 │  ┌──────────────────────────────────────────────────────────────────────────┐  │
 │  │                        Multi-Endpoint Routing Matrix                     │  │
 │  │   Each endpoint: SPSC ring ──▶ Rate Resampler ──▶ Slip Drift Correction  │  │
 │  └──────────────────────────────────────────────────────────────────────────┘  │
 └──────────────────────────────────────┬─────────────────────────────────────────┘
                                        │ independent fan-out
                                        ▼
                  Primary DAC & Secondary Physical Endpoints
      (ALSA Direct ─ WASAPI Exclusive ─ ASIO Native ─ CoreAudio Hog ─ CPAL)
                                        │
                                        ▼
                              Physical Audio Output
```

### Key Architectural Invariants

- **Dedicated Engine Worker Thread**: A single worker thread drives `AudioEngine::tick_blocking(timeout)`. It sleeps on the crossbeam command channel when idle, eliminating busy-polling.
- **Lock-Free Hot Path**: Audio samples flow exclusively through cache-line padded single-producer single-consumer (`PcmRingBuffer`) queues. No mutexes or heap allocations exist anywhere on the audio path.
- **Glitch-Free Atomic Swaps**: Graph reconfigurations and topology updates build a fresh `GraphGeneration` on the control thread and publish it using an atomic pointer swap at block boundaries.
- **Decoupled Endpoints**: Secondary output endpoints each own an independent thread, SPSC ring, resampler, and Rubato `Slip` drift controller, preventing a stalled device from interrupting the master stream.

---

## 🎛 DSP Signal Chain

The production engine executes a pre-allocated, compiled execution plan lowered from the Graph 2.0 topology. All stages operate in-place with zero allocations during steady-state processing:

```text
Decoded Audio Frames
  │
  ├── Multichannel Routing & Bass Management (LFE crossover, channel delay, trim)
  │
  ├── Mix Bus Stage
  │    ├── Per-slot gain trim, pan, and mute
  │    ├── Per-slot loudness normalizer (EBU R128 / ReplayGain)
  │    ├── Program-gated ducking & sample-accurate automation curves
  │    └── Post-fader sends (Master Send & Aux Send)
  │
  ├── Aux Bus Node
  │    ├── Summation of all slot aux sends
  │    ├── Per-send automation & ducking
  │    └── Optional Convolution Insert (Reverb / Cabinet IR) returned to Master
  │
  ├── Acoustic Room & Headphone Correction Node (Decoupled FIR/IIR calibration)
  ├── 64-Band Parametric Equalizer (+ AutoEQ headphone database presets)
  ├── Graphic Equalizer Layer (10, 15, or 31 ISO standard bands)
  ├── 3-Band Multiband Compressor (Independent crossover thresholds, attack, release)
  ├── Partitioned FFT Convolution (Impulse response reverb / cabinet modeling)
  ├── Headphone Crossfeed (Bauer, Chu Moy, Jan Meier, or custom profiles)
  ├── Mid-Side Stereo Enhancer & Channel Balance
  ├── WSOLA Time-Stretch & Pitch-Shift (Varispeed, TimeStretch, PitchShift)
  ├── Perceptual Logarithmic Volume (dB curve with click-free sample smoothing)
  ├── Seek & Track Transition Fader (Micro-fade suppression of discontinuities)
  │
  ├── Spatial Master Output Stage (Opt-in binaural HRTF head model & 3D room)
  │
  └── Output Domain Processing
       ├── High-Performance Sinc Resampling (Rubato FFT / Polynomial)
       ├── 4× Oversampling True-Peak Lookahead Limiter (Inter-sample peak protection)
       ├── Triangular Probability Density Function (TPDF) Dither
       └── Master SPSC Ring Buffer ──▶ Dispatched to DAC & Output Matrix Endpoints
```

### Hardware Bypass Modes

- **Bit-Perfect Direct**: Bypasses all DSP processing stages. Only unity-gain seek fades and essential volume smoothing are applied.
- **DSD-over-PCM (DoP) Bypass**: Total bit-transparent passthrough. Raw 24-bit DoP frames pass directly to the DAC without DSP or volume alterations.

---

## 🚀 Quick Start

### 1. Add Cargo Dependencies

Add `engine` and `config` to your `Cargo.toml`:

```toml
[dependencies]
engine = { path = "path/to/engine" }
config = { path = "path/to/engine/crates/config" }
```

### 2. Basic Playback in Rust

```rust
use std::time::Duration;
use engine::{AudioEngine, EngineConfig, EngineHandle, EngineEvent};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize engine with default configuration
    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle: EngineHandle = engine.handle();

    // 2. Drive the engine on a dedicated tick thread
    std::thread::spawn(move || {
        while engine.is_running() {
            // Sleeps when idle, wakes immediately on new commands
            engine.tick_blocking(Duration::from_millis(10));
        }
        engine.stop();
    });

    // 3. Monitor engine lifecycle events
    let events = handle.clone_event_receiver();
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match event {
                EngineEvent::PlaybackStarted => println!("▶ Playback started"),
                EngineEvent::PlaybackPaused  => println!("⏸ Playback paused"),
                EngineEvent::PlaybackStopped => println!("⏹ Playback stopped"),
                EngineEvent::Error(err)      => eprintln!("❌ Engine error: {err}"),
                _ => {}
            }
        }
    });

    // 4. Open audio, adjust volume, and start playback
    handle.open_file("music/sample.flac");
    handle.set_volume_db(-6.0); // Perceptual volume in dB (-60.0 .. 0.0 dB)
    handle.play();

    // 5. Inspect lock-free telemetry snapshot
    let info = handle.playback_info();
    println!(
        "State: {:?} | Playhead: {:.2}s / {:.2}s | Sample Rate: {} Hz",
        info.state, info.position_secs_compensated, info.duration_secs, info.sample_rate
    );

    // Keep main thread alive for demonstration
    std::thread::sleep(Duration::from_secs(5));
    Ok(())
}
```

---

### 3. Reference CLI

The repository includes a feature-packed reference CLI player:

```bash
# Launch interactive REPL (supports files, directories, or URIs)
cargo run --bin audio-engine-cli -- [options] [path_or_uri]

# Examples:
cargo run --bin audio-engine-cli -- -b alsa -d "hw:0,0" /home/user/Music
cargo run --bin audio-engine-cli -- --backend wasapi -d "default" https://stream.example.com/live.opus
```

#### Interactive Commands

| Command | Description |
|---|---|
| `open <path\|dir\|uri>` | Open and play a file, directory (auto-scanned), or HTTP(S) stream |
| `queue <path\|dir\|uri>` | Append a file or directory of tracks to the playback queue |
| `play` / `pause` / `stop` | Primary transport playback controls |
| `seek <seconds>` | Precise seek to time in seconds (e.g. `seek 45.2`) |
| `volume <0..1 \| xdB>` | Set linear gain (`volume 0.8`) or perceptual dB (`volume -12db`) |
| `speed <multiplier>` | Set playback speed (e.g. `speed 1.25`) |
| `next` / `prev` | Skip to next or previous track in the playlist |
| `shuffle [on\|off]` | Toggle playlist shuffle mode |
| `repeat [off\|all\|one]` | Configure repeat mode |
| `eq on\|off\|<preset>` | Toggle EQ or load AutoEQ preset |
| `eq-band <n> <f> <g> <q>` | Configure parametric band: index, frequency, gain dB, Q factor |
| `levels` | Live peak (dBFS), RMS, and dominant frequency readout |
| `scan <file>` | Perform EBU R128 integrated loudness & true-peak scan |
| `devices` / `device <name>` | List audio output endpoints or switch active device |
| `info` / `events` | Print lock-free telemetry snapshot or drain event log |
| `quit` / `exit` | Graceful shutdown and exit |

---

### 4. C-FFI (C, C++, Python, C#, Node.js)

Enable the `c-ffi` feature in `Cargo.toml` to access the stable C-ABI surface:

```c
#include <stdio.h>
#include "engine_ffi.h" // C bindings generated from src/ffi.rs

int main() {
    // 1. Initialize engine on dedicated background worker thread
    EngineHandleFFI* engine = engine_create(ENGINE_BACKEND_DEFAULT);
    if (!engine) {
        fprintf(stderr, "Failed to initialize audio engine\n");
        return 1;
    }

    // 2. Open file and start playback
    engine_open_file(engine, "/music/track.flac");
    engine_set_volume_db(engine, -6.0f);
    engine_play(engine);

    // 3. Multi-endpoint routing: Add secondary USB DAC output
    engine_upsert_endpoint(engine, "endpoint-usb", "USB Audio DAC", ENGINE_BACKEND_ALSA, 1.0f, 1, 1);

    // 4. Query playhead position
    float pos = engine_position_secs(engine);
    printf("Current playhead: %.2f seconds\n", pos);

    // 5. Cleanup and release resources
    engine_destroy(engine);
    return 0;
}
```

---

## 🔌 Configuration Model

[`EngineConfig`](crates/config/src/engine_config.rs) is fully Serde-serializable, enabling straightforward JSON/TOML configuration storage:

```rust
use config::{AudioBackend, EngineConfig, EnginePreset, PrecisionMode, VolumeMode};

let mut config = EngineConfig::default();
config.output_backend = AudioBackend::ExclusiveAlsa;
config.precision_mode = PrecisionMode::Quality;         // Double-precision f64 DSP path
config.volume_mode    = VolumeMode::HardwarePreferred;  // Favor hardware volume with software fallback
config.mix_slots      = 4;                              // Primary + crossfade + 2 multi-track lanes

// Validate configuration consistency before engine startup
let issues = config.validate();
assert!(issues.is_valid());

// Or initialize directly from a curated preset
let audiophile_config = EngineConfig::from_preset(EnginePreset::Fidelity);
```

### Curated Configuration Presets

- **`EnginePreset::Default`**: Balanced stereo everyday listening (f32, follow track sample rate, safety limiter active).
- **`EnginePreset::Fidelity`**: Audiophile bit-perfect configuration (f64 quality, exclusive output backend, dither enabled).
- **`EnginePreset::LowLatency`**: Pro-audio live monitoring configuration (minimum buffer sizing, zero lookahead).
- **`EnginePreset::Broadcast`**: EBU R128 loudness normalization, true peak limiting, and program-gated ducking.

---

## 🎧 Decoders, DSD & Formats

| Format / Codec | Engine Implementation | Capability & Specifications |
|---|---|---|
| **FLAC** | Symphonia Bundle | Lossless 16/24/32-bit integer PCM, multichannel up to 7.1.4 |
| **ALAC** | Symphonia Codec | Apple Lossless 16/24-bit in M4A/CAF containers |
| **WAV / AIFF** | Symphonia Riff / Aiff | Integer PCM (8/16/24/32-bit), IEEE float (32/64-bit), RF64, BWF |
| **DSD (DSF / DFF)** | Pure Rust Native (`src/decode/dsd/`) | Native wire packing, DoP (DSD64–DSD512), 1-bit multistage decimation |
| **Ogg Opus** | RFC 8251 Pure Rust (`crates/opus-decoder`) | 48 kHz float decoding, packet-loss concealment, gapless metadata |
| **True Audio (TTA)** | Pure Rust Native (`src/decode/tta.rs`) | Lossless v1/v2 integer decoding, sample-accurate CRC32 verification |
| **WavPack** | Pure Rust (`wavicle`) | Lossless v5 integer & 32-bit float decoding, fast seeking |
| **Monkey's Audio (APE)** | Pure Rust (`ape-decoder`) | APEv2 metadata tags and lossless audio decompression |
| **MP3 / AAC / Vorbis** | Symphonia Bundles | Lossy psychoacoustic decoding with gapless delay/padding trimming |

---

## 🔊 Output Backends & OS Integration

| Output Backend | Target Operating System | Exclusivity & Hardware Verification |
|---|---|---|
| **ALSA Direct** | Linux (`alsa`) | Direct kernel access (`hw:`, `plughw:`), hardware MMAP, bypasses Pulse/PipeWire |
| **WASAPI Exclusive** | Windows (`wasapi-native`) | `IAudioClient` exclusive event-driven mode, true bit-perfect bypass, system loopback |
| **Steinberg ASIO** | Windows (`asio-native`) | Pure-Rust `IASIO` COM interface (no Steinberg C++ SDK required), native DSD transport |
| **CoreAudio Hog** | macOS (`objc2-core-audio`) | Hardware Hog-Mode, direct HAL IO render procedures, hardware volume synchronization |
| **PipeWire Pro** | Linux (`pipewire`) | Direct low-latency PipeWire sink discovery, dynamic quantum negotiation, zero-alloc worker |
| **JACK Pro-Audio** | Linux / Unix (`jack`) | Low-latency synchronous audio server callbacks, BBT transport sync, patchbay routing |
| **CPAL Universal** | All Platforms | Universal cross-platform shared-mode audio output fallback |

---

## 🧪 Testing & Quality Gates

The engine repository enforces rigorous fidelity and quality assurance across **58 dedicated test suites comprising over 1,000 unit, integration, and fidelity tests**:

```bash
# Run all workspace unit and integration tests
cargo test --workspace

# Validate zero heap allocations on the DSP hot path
cargo test --test realtime_allocation

# Verify bit-exact equivalence between Graph 2.0 and reference pipeline
cargo test --test graph_pipeline_equivalence

# Validate lookahead true-peak limiter and inter-sample overshoot containment
cargo test --test limiter_correctness

# Execute multi-stage procedural musical composition evaluation render
cargo test --test song_evaluation

# Run full code-quality and clippy audits
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

### Automated CI Safeguards

- **Multi-OS Matrix**: GitHub Actions verifies Linux, macOS, and Windows on every commit.
- **Zero Allocations**: Dedicated custom counting allocator asserts 0 allocations during steady-state audio rendering.
- **Fuzzing & Robustness**: Malformed, truncated, and corrupted streams are tested against decoders to ensure clean error propagation without panics.

---

## 📁 Repository Layout

```text
├── Cargo.toml                       # Workspace root and engine crate manifest
├── CHANGELOG.md                     # Semantic version history and release notes
├── crates/
│   ├── config/                      # Serde-serializable engine & DSP configuration models
│   ├── plugin-abi/                  # C-ABI plugin specification, vtables, and host loader
│   ├── plugin-test-echo/            # Reference delay + gain audio plugin implementation
│   └── opus-decoder/                # Pure-Rust RFC 8251 Opus audio decoder
├── src/
│   ├── lib.rs                       # Crate root, feature gates, and prelude re-exports
│   ├── commands.rs                  # EngineCommand — complete host control enumeration
│   ├── events.rs                    # EngineEvent & OutputEvent lifecycle definitions
│   ├── playback_info.rs             # Atomic telemetry snapshot models (ArcSwap)
│   ├── ffi.rs                       # C Foreign Function Interface implementation
│   ├── track_cache.rs               # Bounded in-memory metadata and analysis cache
│   ├── buffer/                      # Lock-free SPSC PCM ring buffers and audio frames
│   ├── decode/                      # Decoders, format scanners, channel mix, tags, loudness
│   ├── dsp/                         # DSP filters, Graph 2.0 topology, arena, and limiter
│   │   ├── graph2/                  # Typed-port graph engine, compilation, and realtime execution
│   │   ├── resampler/               # Rubato-based high-performance sinc resampler
│   │   └── safety.rs                # NaN/Inf containment and FTZ/DAZ denormal mitigation
│   ├── spatial/                     # 3D spatial layer: VBAP, Ambisonics (HOA), Room, Binaural HRTF
│   ├── output/                      # Hardware backends (ALSA, WASAPI, ASIO, CoreAudio, CPAL)
│   └── bin/                         # audio-engine-cli, replaygain-scanner, release-qualification
├── benches/                         # Criterion benchmarks (DSP, pipeline, graph, spatial)
├── docs/                            # ENGINE_SPEC.md, OWNERS_GUIDE.md, ARCHITECTURE.md, SIGNAL_FLOW.md
└── tests/                           # 58 test files (fidelity suites, robustness fuzzing, real-time tests)
```

---

## 🤝 Contributing & Standards

We welcome contributions! Please review **[`AGENTS.md`](AGENTS.md)** before opening PRs:

1. **Strict Semantic Versioning**: `engine`, `config`, `plugin-abi`, and `plugin-test-echo` version numbers must always remain in lockstep.
2. **Modular Architecture**: Strictly avoid god files or oversized structs. New features follow the established house pattern (e.g. concern-scoped implementation modules in `src/engine/commands/` or `src/dsp/graph2/prod/arena/`).
3. **Audio-Path Realtime Safety**: No heap allocations (`Vec::push`, `Box::new`, `format!`), no system locks (mutexes), and no blocking filesystem/network I/O on the audio thread.
4. **Clean Quality Verification**: Every PR must pass `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace`.

---

## 📄 License

Licensed under the **[Apache License, Version 2.0](LICENSE-APACHE)**.
