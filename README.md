# Freebuff Desktop — Independent Core Audio Engine

A reference-grade, bit-perfect, **headless** audio playback and DSP engine written in
**100% pure Rust**. Built for audiophile listening, pro-audio workstations, low-latency
monitoring, and glitch-free real-time playback on modern and legacy hardware.

The engine is fully independent: **zero UI dependencies, zero database/library ties,
zero playlist policy, zero OS-specific application assumptions**. It embeds cleanly into
CLI players, desktop GUIs (Slint, Iced, Qt, GTK, egui), streaming daemons, test
harnesses, or pro-audio suites — and it ships with a stable **C FFI** so it can be driven
from C, C++, Python, C#, Node.js, and any language that can call C.

> **Documentation:** [Architecture](docs/ARCHITECTURE.md) · [Signal Flow](docs/SIGNAL_FLOW.md) ·
> [Embedding guide](docs/EMBEDDING.md) · [Contributing & versioning](AGENTS.md)

---

## ✨ Highlights

| Capability | What it means |
|---|---|
| **100% pure Rust** | No C/C++ codec SDKs, no `unsafe` on the DSP hot path, no FFI dependency for decoding. Fully auditable and cross-compilable. |
| **Bit-perfect direct endpoints** | Native OS-level exclusive backends: ALSA `hw:`/`plughw:`, WASAPI Exclusive (`IAudioClient`), Steinberg ASIO (`IASIO`, no C++ SDK), CoreAudio Hog-Mode — each verified against the OS before claiming the device, with honest "bit-perfect cannot be proven" reporting rather than guesses. |
| **Mastering-grade dual precision** | Every DSP stage runs in fast **f32** (Performance) or double-precision **f64** (Quality), selectable per session. |
| **Real-time safety** | **Zero heap allocations** on the decode/DSP hot path (verified by `tests/fidelity/realtime_allocation.rs`), cache-padded lock-free SPSC ring buffers, no locks anywhere on the audio path. |
| **Dual-decoder transitions** | Sample-accurate **gapless**, customizable **crossfade** (constant-power / linear / exponential / logarithmic / S-curve), fade, and stop transitions between tracks. |
| **Audiophile codecs + 1-bit DSD** | FLAC, ALAC, WAV, AIFF, APE, WavPack, TTA, Opus, Ogg Vorbis, AAC, MP3 — plus native **DSD (DSF/DFF)** up to DSD512 over Native wire and DoP. |
| **Immersive multichannel** | Mono → 7.1.4 (12 ch) and custom layouts up to 16 channels, with active bass management, per-channel distance delays, routing matrices, and per-channel EQ. |
| **Isolated client handle** | `EngineHandle` is a `Clone + Send` bridge over a lock-free command channel and atomic telemetry — the realtime thread is never blocked by the host. |
| **Real-time analyzer** | Lock-free peak / RMS / dominant-frequency and FFT spectrum taps published in every telemetry snapshot. |
| **Loudness & tags** | EBU R128 / ReplayGain measurement, normalization, and **tag write-back** (`tag-write`) in FLAC/MP3/M4A/WAV/AIFF/APE/WavPack; AcoustID **fingerprinting** (`fingerprint`). |
| **System-audio capture** | WASAPI loopback recording of the system mix straight to a float32 WAV (Windows). |

---

## 🏗 Architecture at a glance

```
         Host application (GUI / CLI / FFI)
           │  EngineCommand (control)
           │  EngineEvent / OutputEvent (discrete lifecycle)
           ▼
      EngineHandle ───────────────────────────────┐
           │                                      │ lock-free
           ▼                                      ▼
   Command channel                        ArcSwap<PlaybackInfo>
           │
           ▼
   ┌──────────────────────────── AUDIO ENGINE CORE ─────────────────────────────┐
   │  Decoders ─▶ Dual-decoder mixer ─▶ DSP pipeline ─▶ Safety limiter           │
   │  (Symphonia + native DSD/Opus/TTA/WavPack)  (f32 or f64, multichannel)      │
   │  ──────────────────────────────────────────────▶ FixedFrameBuffer ring      │
   └────────────────────────────────────────────────────────────────────────────┘
                             │  output thread(s)
                             ▼
        ALSA ─ WASAPI Exclusive ─ ASIO ─ CoreAudio Hog ─ CPAL fallback
                             ▼
                      Hardware DAC
```

- **One tick thread** owns all engine state. The host calls `tick_blocking` (or uses the
  built-in FFI tick thread); `tick_blocking` sleeps on the command channel so it never
  busy-polls.
- **Commands** are one-way `EngineCommand`s on a bounded crossbeam channel.
- **Telemetry** (`PlaybackInfo`, incl. `EngineStats`, analyzer levels, queue state, u64
  clip/underrun/overload counters) is published lock-free via `ArcSwap` — hosts read it
  from any thread.
- **Audio** flows into a cache-padded SPSC `FixedFrameBuffer`; each output backend drains
  it from its own realtime thread/callback.

See [ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full module map, concurrency model,
and realtime-safety rules, and [SIGNAL_FLOW.md](docs/SIGNAL_FLOW.md) for the exact sample
path and precision/bypass modes.

### DSP signal chain

The production chain (`dsp/pipeline`) runs in a fixed order with pre-allocated scratch —
no allocation on the hot path:

```
Source frames
  ├─ Channel trim / routing / bass management / LFE   (multichannel path)
  ├─ Input & output preamp + loudness normalizer      (EBU R128 / ReplayGain)
  ├─ Track mixer                                       (gapless / crossfade blend)
  ├─ 64-band parametric EQ (+ AutoEQ presets)          (post-mix)
  ├─ Graphic EQ layer (10 / 15 / 31 ISO bands)
  ├─ 3-band multiband compressor
  ├─ FFT partitioned convolution (HRTF / reverb IRs)
  ├─ Headphone crossfeed (Bauer / Chu Moy / J. Meier / custom)
  ├─ Mid-side stereo enhancer & balance
  ├─ WSOLA time-stretch / pitch-shift                  (varispeed, TimeStretch, PitchShift)
  ├─ Perceptual logarithmic volume (dB, ramped)
  ├─ Seek / transition fade
  ├─ Resampler (Rubato sinc) → output domain
  └─ 4× true-peak lookahead limiter + TPDF dither
       └─▶ FixedFrameBuffer → DAC
```

Two **hard bypass modes** bypass the entire graph:
- **Bit-perfect** — only volume ramps and seek fades survive; every DSP stage is skipped.
- **DoP bypass** — pure passthrough for DSD-over-PCM bitstreams (24-bit DoP words must reach
  the DAC unmodified; not even volume is applied).

A parallel, experimental **node-based DSP graph** (`dsp/graph`) exists for capability
introspection and future reorderable-chain features; the production hot path routes
through `DspPipeline`.

---

## 🚀 Quick start

### 1. Add as a dependency

```toml
[dependencies]
engine = { path = "path/to/engine" }
config = { path = "path/to/engine/crates/config" }
```

### 2. Embed and play

```rust
use engine::{AudioEngine, EngineHandle, EngineConfig, EngineEvent};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle: EngineHandle = engine.handle();

    // Drive the engine on a background thread — tick_blocking sleeps between commands.
    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(std::time::Duration::from_millis(10));
        }
    });

    // Listen for discrete lifecycle events on a second thread.
    let events = handle.clone_event_receiver();
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match event {
                EngineEvent::SourceOpened { source, sample_rate, channels, .. } => {
                    println!("Opened {source:?}: {sample_rate} Hz, {channels} ch");
                }
                EngineEvent::PlaybackStarted => println!("Playing"),
                EngineEvent::SourceFinished { source } => println!("Finished: {source:?}"),
                _ => {}
            }
        }
    });

    // Control playback from anywhere (the handle is Clone + Send).
    handle.open_file("/path/to/song.flac");
    handle.play();
    handle.set_volume_db(-6.0);               // perceptual, -60..0 dB

    // Read lock-free telemetry from any thread.
    let info = handle.playback_info();
    println!(
        "{} | {} / {} s | {} Hz | bit-perfect: {}",
        info.state, info.position_secs_compensated, info.duration_secs,
        info.sample_rate, info.bit_perfect,
    );
    Ok(())
}
```

### 3. Reference CLI

```bash
# Interactive REPL (file/URI positional arg optional)
cargo run --bin audio-engine-cli [options] [file_or_uri]

# Options
--backend, -b <auto|wasapi|alsa|coreaudio|asio>
--device,  -d <device_name>
--log-level <error|warn|info|debug|trace>
```

Inside the REPL:

```
open <path|uri>      open & play a source
queue <path|uri>     append to the playback queue
next | prev          skip forward / backward
shuffle [on|off]     toggle shuffle
repeat [off|all|one] set repeat mode
play | pause | stop  transport
seek <seconds>       seek to position
volume <0..1 | xdb>  set linear or dB volume
speed <multiplier>   set playback speed
eq on|off|<preset>   enable / disable / load an EQ preset
eq-band <n> <freq> <gain> <q> [on|off]
levels               live peak / RMS / dominant-frequency
scan <file>          EBU R128 loudness scan
capture start [f]    record system audio (Windows)
capture stop
devices | device <n> list / switch output endpoints
fingerprint <file>   AcoustID/Chromaprint fingerprint (feature `fingerprint`)
info | events        telemetry snapshot / drain events
quit                 graceful shutdown
```

```bash
# Batch loudness scan + tag write-back
cargo run --features tag-write --bin replaygain-scanner -- --write /music/album/*.flac
```

### 4. C FFI (any C-compatible language)

Enable `c-ffi` and call the opaque-handle API:

```c
EngineHandleFFI* h = engine_create(ENGINE_BACKEND_DEFAULT);
engine_open_file(h, "/path/to/song.flac");
engine_play(h);
engine_set_volume_db(h, -6.0f);
float pos = engine_position_secs(h);
engine_destroy(h);
```

Every function returns a status code (`0 = Ok`) and is safe with invalid/NULL handles; no
panics cross the boundary. See [`src/ffi.rs`](src/ffi.rs) for the full export list and C
type mapping.

For complete, runnable examples of both the Rust `EngineHandle` API and the C FFI,
including playback, DSP control, **gapless/crossfade**, headless analysis, and sample
capture, see the [`docs/EMBEDDING.md`](docs/EMBEDDING.md) guide.

---

## 🔌 Configuration model

[`EngineConfig`](crates/config/src/engine_config.rs) (in the `config` crate) is fully
Serde-serializable and controls every aspect of the engine:

```rust
use config::{EngineConfig, EnginePreset};

let mut config = EngineConfig::default();
config.output_backend = config::AudioBackend::ExclusiveAsio;
config.precision_mode = config::PrecisionMode::Quality;   // f64 DSP path
config.volume_mode = config::VolumeMode::HardwarePreferred;
config.sample_rate_policy = config::SampleRatePolicy::FollowTrack;

let issues = config.validate();          // surface contradictions early
assert!(issues.is_valid());

// Or start from a preset
let fidelity = EngineConfig::from_preset(EnginePreset::Fidelity);
```

Key configuration groups (all in `config`):

- **Output / transport** — `output_backend`, `output_device`, `sample_rate_policy`,
  `fallback_policy`, `volume_mode`, `volume_fade_ms`, `seek_fade_ms`
- **Format / precision** — `precision_mode`, `resampler_quality`, `dither_enabled`
- **DSP stages** — `eq` (+ `graphic_eq` layer), `loudness` (EBU R128 / ReplayGain),
  `limiter`, `multiband_compressor`, `convolution`, `crossfeed`, `stereo_enhancer`
- **Transitions** — `crossfade`, `transition_mode` (Gapless / Crossfade / Fade / Stop)
- **Speed / pitch** — `speed_mode` (Varispeed / TimeStretch / PitchShift), `timestretch_quality`
- **Multichannel** — `channel_policy`, `channel_trim`, `channel_eq`, `channel_routing`,
  `lfe`, `bass_management`, `channel_mix`
- **DSD** — `dsd_output` (Native / DoP / PCM)

The full per-stage tunables (band counts/frequencies/Q, limiter ceiling/attack/release,
crossfeed profiles, multiband band params, LFE crossover, …) live in
[`crates/config/src/dsp_config.rs`](crates/config/src/dsp_config.rs).

---

## 🎛 Host-control API (`EngineHandle`)

`EngineHandle` is `Clone + Send`, fully isolated from the realtime thread. Everything goes
through non-blocking message passing + atomic telemetry.

| Group | Methods |
|---|---|
| **Transport** | `play`, `pause`, `stop`, `seek(secs)`, `shutdown` |
| **Sources** | `open`, `open_file`, `open_uri`, `open_memory`, `prepare_next(File/Memory)`, `prepare_next_file` |
| **Playlist** | `enqueue`, `enqueue_file`, `play_index`, `next`, `previous`, `remove_from_playlist`, `clear_playlist`, `set_repeat_mode`, `set_shuffle`, `playlist_len`, `playlist_index` |
| **Volume / gain** | `set_volume`, `set_volume_db`, `set_volume_mode`, `set_balance`, `set_preamp` |
| **Speed / pitch** | `set_speed`, `set_speed_mode`, `set_pitch` |
| **EQ / shaping** | `set_eq_enabled`, `set_eq_preset`, `set_eq_band`, `set_graphic_eq_layout`, `set_graphic_eq_slider`, `set_graphic_eq_enabled`, `set_stereo_width` |
| **Spatial** | `set_crossfeed_enabled`, `set_crossfeed_profile`, `set_crossfeed_custom_params` |
| **Multichannel** | `set_channel_mix`, `set_channel_policy`, `set_channel_trim`, `set_channel_routing`, `set_channel_eq`, `set_lfe_config`, `set_bass_management` |
| **Output / audiophile** | `set_output_backend`, `set_output_device`, `available_devices`*, `set_sample_rate_policy`, `set_bit_perfect`, `set_dither_enabled`, `set_resampler_quality`, `set_limiter_mode`, `set_limiter_true_peak`, `open_asio_control_panel`† |
| **Capture** | `start_capture`, `stop_capture` |
| **Telemetry** | `playback_info`, `state`, `is_playing`, `current_source`, `position_secs`, `position_secs_compensated`, `duration_secs`, `volume`, `speed`, `latency_ms`, `analyzer`, `events`, `clone_event_receiver`, `clone_output_event_receiver` |

`EngineCommand` (raw), `EngineEvent`, `OutputEvent`, and `PlaybackInfo` are all public so
hosts can drive the engine over their own channels or persist command streams.

(* = requires the `audio-output` feature · † = no-op unless the active backend is ASIO and the `asio-native` feature is compiled in.)

**Telemetry** (`PlaybackInfo`) is published every tick: state, decoded + latency-compensated
position, duration, format, volume, speed, latency, bit-perfect status, analyzer levels and
dominant frequency, playlist index/length, and u64 counters (clips, NaNs, underruns, CPU
overloads, deadline misses). Read it lock-free from any thread via `handle.playback_info()`.

**Events** (`EngineEvent`): `SourceOpened`, `PlaybackStarted/Paused/Stopped`,
`SourceFinished`, `FormatChanged`, `SeekCompleted`, `PlaylistChanged`, `LoudnessScanComplete`,
`CaptureStarted/Stopped`, `CaptureError`, `Error`. Device hotplug uses a separate
`OutputEvent` channel (`OutputDeviceChanged`, `DeviceListChanged`, `DeviceConnected`,
`DeviceDisconnected`).

---

## 🎧 Decoders, DSD & outputs

**Decoders** — Symphonia (FLAC, ALAC, WAV, AIFF, MP3, AAC, Vorbis, PCM, Ogg, MP4/MKA
containers) plus pure-Rust native backends: **DSD (DSF/DFF)** up to DSD512 with native
wire packing and DoP, **Ogg Opus**, **True Audio (TTA)**, and **WavPack**. Unsupported
multichannel/hybrid/DSD WavPack is rejected explicitly at open — never silently downmixed.
A vectorized format scanner routes files by extension + magic bytes; source abstraction
covers **file, URI, and in-memory** payloads (Gapless/crossfade and `AudioSource::Memory`
included).

**Output backends** (feature-gated, each verifies exclusivity before claiming the device):

| Backend | Platform | Notes |
|---|---|---|
| ALSA native | Linux | Direct `hw:` / `plughw:` exclusive-mode |
| WASAPI native | Windows (`wasapi-native`) | `IAudioClient` exclusive-mode + loopback capture |
| ASIO native | Windows (`asio-native`) | Pure-Rust `IASIO` via COM, native DSD transport, drivers’ control panel |
| CoreAudio | macOS | Hog-mode with direct HAL IO procs + hardware endpoint volume |
| CPAL | All | Shared-mode fallback |

Plus `format_converter`, `output_profile` (per-device profiles), `device_monitor` (hotplug),
and `rate_policy` (track-native / device / fixed rate handling).

---

## ⚙️ Cargo features

Everything is opt-in; the default set covers everyday playback.

| Feature | Adds |
|---|---|
| `audio-output` (default) | Output backends, device monitor, hardware volume |
| `resample` (default) | Rubato sinc resampler |
| `all-codecs` (default) | Every codec below |
| `codec-mp3/flac/ogg/wav/aac/alac/pcm/aiff/isomp4/mkv` | Symphonia-backed codecs/containers |
| `codec-opus` | Pure-Rust Ogg Opus decode |
| `codec-tta` | Native True Audio decode |
| `codec-wavpack` | Pure-Rust WavPack v5 decode |
| `codec-ape` | Pure-Rust Monkey’s Audio decode |
| `codec-dsd` | DSD decode/decimation/DoP (accepted no-op; compiled unconditionally) |
| `asio-native` | Native Steinberg ASIO backend (Windows) |
| `wasapi-native` | Native WASAPI exclusive output + system-loopback capture (Windows) |
| `network-streaming` | Range-request HTTP(S) streaming via `ureq` |
| `tag-write` | EBU R128 / ReplayGain tag write-back via `lofty` |
| `fingerprint` | AcoustID/Chromaprint fingerprinting via `chromaprint` |
| `c-ffi` | Stable C FFI surface |

---

## 🧪 Testing & quality gates

The repository ships **44 test files** — unit tests co-located with modules plus dedicated
integration and fidelity suites under [`tests/fidelity/`](tests/fidelity/): EQ frequency
response, lookahead-limiter correctness and measurement, dither measurement, resampler
quality/measurement, EBU R128, golden reference vectors, decoder robustness + fuzz
mutation, multichannel graph, gapless/crossfade/seamless-seek, timestretch fidelity,
concurrent ring-buffer stress, realtime zero-allocation validation, and headless lifecycle
tests. Benchmarks live in [`benches/`](benches/) (Criterion).

```bash
cargo test                                  # unit + headless integration
cargo test --features tag-write,fingerprint # optional-feature coverage
cargo test --test headless_playback         # embedding lifecycle
cargo test --test realtime_allocation       # zero-allocation on the hot path
cargo test --test eq_frequency_response     # DSP fidelity measurement
cargo test --test ring_buffer_stress        # concurrent SPSC stress
cargo bench                                 # DSP / pipeline benchmarks
```

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) enforces `cargo fmt`,
`cargo clippy -D warnings`, and the test matrix across **Linux, macOS, and Windows**, plus a
cross-target compile check of the native WASAPI/ASIO backends. Always re-run `cargo fmt`,
`cargo clippy`, and `cargo test` before submitting changes.

---

## 📁 Project layout

```
├── Cargo.toml                 # workspace root + `engine` crate
├── crates/config/             # `config` crate — Serde config models, presets, validation
├── src/
│   ├── lib.rs                 # crate root + prelude
│   ├── engine/                # core state machine: tick loop, handle, stream,
│   │                          #   decode_loop/, commands/, clock, recovery, DSD transport
│   ├── decode/                # Symphonia + native DSD/Opus/TTA/WavPack, channel
│   │                          #   layout/mix, tags, fingerprint, loudness
│   ├── dsp/                   # DSP primitives + pipeline/ (production chain)
│   │                          #   + graph/ (experimental node graph)
│   ├── output/                # ALSA / WASAPI / ASIO / CoreAudio / CPAL, profiles,
│   │                          #   device monitor, WAV writer, loopback
│   ├── buffer/                # cache-padded lock-free SPSC rings
│   ├── audio_io.rs · sink.rs · source.rs · playlist.rs · events.rs
│   ├── commands.rs · playback_info.rs · ffi.rs
│   └── bin/                   # audio-engine-cli, replaygain-scanner
├── benches/                   # dsp_bench, pipeline_bench
├── docs/                      # ARCHITECTURE.md, SIGNAL_FLOW.md, EMBEDDING.md
└── tests/                     # headless_playback.rs, fidelity/
```

---

## 🤝 Contributing & versioning

This project follows **Semantic Versioning `x.y.z`** (major.minor.patch) with the engine
and `config` crate kept in **lockstep**, a dated `CHANGELOG.md` entry, and a `vX.Y.Z` git
tag on every release. The codebase is deliberately modular and enforces **no god files**:
large single-purpose DSP algorithms are fine, but oversized structs/impls that mix
unrelated concerns must be split by concern (see the `dsp/pipeline/` impl-split pattern).

Full details — version-bump rules, god-file detection signals, the completeness checklist,
and testing guidance for agents and humans — are in **[`AGENTS.md`](AGENTS.md)**.

---

## 📄 License

Licensed under the [Apache License, Version 2.0](LICENSE-APACHE).