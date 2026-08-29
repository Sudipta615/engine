# Shadow Desktop — Independent Core Audio Engine

A reference-grade, bit-perfect, **headless** audio playback and DSP engine written in
**100% pure Rust**. Built for audiophile listening, pro-audio workstations, low-latency
monitoring, and glitch-free real-time playback on modern and legacy hardware.

The engine is fully independent: **zero UI dependencies, zero database/library ties,
zero playlist policy, zero OS-specific application assumptions**. It embeds cleanly into
CLI players, desktop GUIs (Slint, Iced, Qt, GTK, egui), streaming daemons, test
harnesses, or pro-audio suites — and it ships with a stable **C FFI** so it can be driven
from C, C++, Python, C#, Node.js, and any language that can call C.

> **Documentation:** [Architecture](docs/ARCHITECTURE.md) · [Signal Flow](docs/SIGNAL_FLOW.md) ·
> [Embedding guide](docs/EMBEDDING.md) · [Evolution](docs/EVOLUTION.md) · [Contributing & versioning](AGENTS.md)

---

## ✨ Highlights

| Capability | What it means |
|---|---|
| **100% pure Rust** | No C/C++ codec SDKs, no `unsafe` on the DSP hot path, no FFI dependency for decoding. Fully auditable and cross-compilable. |
| **Graph-runtime DSP core** | A node-based `DspGraph` with **compiled execution plans** is the production hot path: stage order is data, not code, and full reconfigurations are swapped in live at block boundaries — zero allocation, zero locks on the audio thread. |
| **N-input mix bus** | The primary stream, the crossfade partner, and **independent lane tracks** each ride their own bus slot with per-slot trim, post-fader sends, pan, mute, program-gated ducking, and sample-accurate automation tracks. |
| **Aux bus as its own plan node** | Per-slot aux sends are independently automatable (ramped, click-free), metered per send, and returned into the master before the post-mix chain — with an optional convolution **insert** (reverb / cabinet) on the send accumulator. |
| **Multi-endpoint routing matrix** | Fan the master out to **several output devices at once**, each with its own realtime thread, rate resampler, level, and **per-endpoint clock-drift correction** (a rubato `Slip` trims the stream to the device's actual crystal — independent devices can't drift the ring full or empty). |
| **Bit-perfect direct endpoints** | Native OS-level exclusive backends: ALSA `hw:`/`plughw:`, WASAPI Exclusive (`IAudioClient`), Steinberg ASIO (`IASIO`, no C++ SDK), CoreAudio Hog-Mode — each verified against the OS before claiming the device, with honest "bit-perfect cannot be proven" reporting rather than guesses. |
| **Mastering-grade dual precision** | Every DSP stage runs in fast **f32** (Performance) or double-precision **f64** (Quality), selectable per session. |
| **Real-time safety** | **Zero heap allocations** on the decode/DSP hot path (verified by `tests/fidelity/realtime_allocation.rs`), cache-padded lock-free SPSC ring buffers, no locks anywhere on the audio path. |
| **Gapless + crossfade transitions** | Sample-accurate **gapless**, customizable **crossfade** (constant-power / linear / exponential / logarithmic / S-curve), fade, and stop transitions between tracks — all as mix-bus envelopes. |
| **Audiophile codecs + 1-bit DSD** | FLAC, ALAC, WAV, AIFF, APE, WavPack, TTA, Opus, Ogg Vorbis, AAC, MP3 — plus native **DSD (DSF/DFF)** up to DSD512 over Native wire and DoP. |
| **Immersive multichannel** | Mono → 7.1.4 (12 ch) and custom layouts up to 16 channels, with active bass management, per-channel distance delays, routing matrices, and per-channel EQ. || **Spatial audio (opt-in)** | An independent spatial scene layer (`spatial/`): world-space **objects** (with directivity, occlusion, angular-region spread), channel-based **beds**, diffuse **fields**, and a **room** (image-source early reflections + a Schroeder late field on the ambisonic bus), mixed through one hybrid renderer — equal-power `BasicPanner`, 3D **VBAP** (triplet solves, 2D reduction, out-of-coverage fallback), the **ambisonic** path (FOA bus encode → decode to any layout, Basic/Max-rE policies), or the **binaural** path (a Woodworth-ITD + Duda-Martens head model rendering the whole scene to headphones, optionally with **measured spectral HRTFs** — loaded from real SOFA-style corpora via `HrtfCorpus`/`from_corpus`, resampled and validated — and elevation cues) — so the *same* scene renders to stereo, 5.1, 7.1, 7.1.4, any custom array, or two ears; **head tracking** (a `HeadTracker` that interpolates and smooths IMU/VR orientation samples into the listener, the VR/AR seam) applies to every renderer unchanged; **higher-order ambisonics** (up to order-3 with exact rotation) and a **scene-file format** (Serde save/load of renderer-independent scenes) round out the layer, and a **SpatialNode** spatializes the production graph's stereo master — whose screen/room/listener state **auto-saves across sessions** (`EngineConfig::spatial_autosave_path`); conventional PCM/DSP untouched (opt in via `ChannelPolicy::SpatialRender`). | **Isolated client handle** | `EngineHandle` is a `Clone + Send` bridge over a lock-free command channel and atomic telemetry — the realtime thread is never blocked by the host. |
| **Real-time analyzer** | Lock-free peak / RMS / dominant-frequency and FFT spectrum taps published in every telemetry snapshot. |
| **Loudness & tags** | EBU R128 / ReplayGain measurement, normalization, and **tag write-back** (`tag-write`) in FLAC/MP3/M4A/WAV/AIFF/APE/WavPack; AcoustID **fingerprinting** (`fingerprint`). |
| **System-audio capture** | WASAPI loopback recording of the system mix straight to a float32 WAV (Windows). |
| **Stable C FFI** | Drive the whole surface — transport, DSP, playlist, **endpoint routing**, and the **aux insert** — from C/C++ or any C-callable language. |

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
   │  Decode loop ─▶ Mix bus ─▶ DSP graph ─▶ Output domain ─▶ safety limiter     │
   │  (per-stream decoders +  (N slots: primary,  (compiled plan:              │
   │   resamplers, SPSC rings) crossfade, lanes)  mix → aux → correction →   │
   │                                              eq → … → limiter)          │
   │  ──────────────────────────────────────────────────────────────────────────│
   │  Each endpoint: SPSC ring → rate resampler → Slip drift correction         │
   └────────────────────────────────────────────────────────────────────────────┘
                             │  independent fan-out rings
                             ▼
        Primary DAC + configured secondary endpoints
        (ALSA ─ WASAPI Exclusive ─ ASIO ─ CoreAudio Hog ─ CPAL fallback)
                             ▼
                      Hardware DAC
```

- **One tick thread** owns all engine state. The host calls `tick_blocking` (or uses the
  built-in FFI tick thread); `tick_blocking` sleeps on the command channel so it never
  busy-polls.
- **Commands** are one-way `EngineCommand`s on a bounded crossbeam channel, split into
  per-domain handlers under `src/engine/commands/` and applied at block boundaries via
  per-node SPSC control queues.
- **Telemetry** (`PlaybackInfo`, incl. `EngineStats`, analyzer levels, queue state, lane
  and endpoint state, u64 clip/underrun/overload counters) is published lock-free via
  `ArcSwap` — hosts read it from any thread.
- **Audio** flows into cache-padded SPSC rings; the graph shell pushes the mixed block to
  the primary ring and every endpoint's ring. Each output backend drains its own ring from
  its own realtime thread/callback and drift-corrects against its device clock.
- **Reconfiguration** is glitch-free: a fresh graph generation (arena + compiled plans) is
  built on the control thread and published with a single atomic pointer swap at the next
  block boundary; the old generation is reclaimed on the control thread.

See [ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full module map, concurrency model,
and realtime-safety rules, and [SIGNAL_FLOW.md](docs/SIGNAL_FLOW.md) for the exact sample
path and precision/bypass modes.

### DSP signal chain

The production chain (`dsp/graph`) runs a **compiled plan** in a fixed order with
pre-allocated scratch — no allocation on the hot path:

```
Source frames
  ├─ Channel trim / routing / bass management / LFE   (multichannel path)
  ├─ Mix bus: per-slot preamp + loudness normalizer  (EBU R128 / ReplayGain)
  │  + per-slot trim, pan, mute, ducking, automation
  │  + per-slot post-fader sends (master-send + aux-send)
  ├─ Aux bus node: accumulate sends → per-send automation
  │  → optional convolution insert → return into master
  ├─ 64-band parametric EQ (+ AutoEQ presets)          (post-mix)
  ├─ Graphic EQ layer (10 / 15 / 31 ISO bands)
  ├─ 3-band multiband compressor
  ├─ FFT partitioned convolution (HRTF / reverb IRs)
  ├─ Headphone crossfeed (Bauer / Chu Moy / J. Meier / custom)
  ├─ Mid-side stereo enhancer & balance
  ├─ WSOLA time-stretch / pitch-shift                  (varispeed, TimeStretch, PitchShift)
  ├─ Perceptual logarithmic volume (dB, ramped)
  ├─ Seek / transition fade
  └─ Plan done → output domain:
      resampler (Rubato sinc) → 4× true-peak lookahead limiter → TPDF dither
       └─▶ master ring → primary DAC + every endpoint
           (each endpoint resamples to its own rate and trims
            with a Slip drift corrector against its real clock)
```

The plan is data: `mix → aux → correction → eq → dynamics → convolution → balance →
crossfeed → stereo → timestretch → volume → seek_fade` (`routing` is prepended on
the multichannel plan; the correction step is the Phase 7 room/headphone
correction node, skipped when disabled), compiled per mode (stereo f32 / f64,
multichannel). Any stage can be selectively disabled; disabled paths are bit-exact.
The output-domain resampler, final safety limiter, and dither run downstream of the
graph.

Two **hard bypass modes** bypass the entire graph:
- **Bit-perfect** — only volume ramps and seek fades survive; every DSP stage is skipped.
- **DoP bypass** — pure passthrough for DSD-over-PCM bitstreams (24-bit DoP words must reach
  the DAC unmodified; not even volume is applied).

`DspPipeline` (`dsp/pipeline`) remains as the reference implementation and the
bit-exact oracle for the graph equivalence suite; the production hot path routes
through `DspGraph`.

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
engine_upsert_endpoint(h, "dac2", "USB DAC", ENGINE_BACKEND_WASAPI, 1.0f, 1, 1); // multi-endpoint
engine_set_aux_insert(h, 1, 0.3f);                                          // aux insert
float pos = engine_position_secs(h);
engine_destroy(h);
```

Every function returns a status code (`0 = Ok`) and is safe with invalid/NULL handles; no
panics cross the boundary. The endpoint routing matrix (`engine_upsert_endpoint` /
`engine_remove_endpoint` / `engine_clear_endpoints` / `engine_endpoint_count` /
`engine_endpoint_id` / `engine_endpoint_info`) and the aux insert
(`engine_set_aux_insert` / `engine_aux_insert_state`) are part of the stable surface. See
[`src/ffi.rs`](src/ffi.rs) for the full export list and C type mapping.

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
config.mix_slots = 4;                                     // N-slot mix bus (lanes ≥ 2)

let issues = config.validate();          // surface contradictions early
assert!(issues.is_valid());

// Or start from a preset
let fidelity = EngineConfig::from_preset(EnginePreset::Fidelity);
```

Key configuration groups (all in `config`):

- **Output / transport** — `output_backend`, `output_device`, `sample_rate_policy`,
  `fallback_policy`, `volume_mode`, `volume_fade_ms`, `seek_fade_ms`
- **Mix bus** — `mix_slots` (N-slot bus; independent lanes ride slots ≥ 2), `mix_trims`
  (per-slot channel trims), `mix_sends` (per-slot master/aux send gains), `aux`
  (`AuxBusConfig`: enabled, return gain, insert enabled / wet mix / IR path)
- **Format / precision** — `precision_mode`, `resampler_quality`, `dither_enabled`
- **DSP stages** — `eq` (+ `graphic_eq` layer), `loudness` (EBU R128 / ReplayGain),
  `limiter`, `multiband_compressor`, `convolution`, `crossfeed`, `stereo_enhancer`
- **Transitions** — `crossfade`, `transition_mode` (Gapless / Crossfade / Fade / Stop)
- **Speed / pitch** — `speed_mode` (Varispeed / TimeStretch / PitchShift), `timestretch_quality`
- **Multichannel** — `channel_policy`, `channel_trim`, `channel_eq`, `channel_routing`,
  `lfe`, `bass_management`, `channel_mix`
- **DSD** — `dsd_output` (Native / DoP / PCM)
- **Endpoints** — `endpoints: Vec<EndpointConfig>` configures stable IDs, backend/device
  targets, per-endpoint gain, enabled state, and **`drift_correction`** (default on): each
  endpoint's nominal-rate resampler is trimmed by a rubato `Slip` so the ring tracks the
  device's real clock. Endpoint rings are independent subscribers; drops and transport
  errors are observable in telemetry.

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
| **Lane tracks** | `EngineCommand::{AddTrack, RemoveTrack, SetTrackGain, SetTrackPan, SetTrackMasterGain, SetTrackSend, DuckTracks}` via `send_command` — independent streams on bus slots ≥ 2 |
| **Volume / gain** | `set_volume`, `set_volume_db`, `set_volume_mode`, `set_balance`, `set_preamp` |
| **Speed / pitch** | `set_speed`, `set_speed_mode`, `set_pitch` |
| **EQ / shaping** | `set_eq_enabled`, `set_eq_preset`, `set_eq_band`, `set_graphic_eq_layout`, `set_graphic_eq_slider`, `set_graphic_eq_enabled`, `set_stereo_width` |
| **Spatial** | `set_crossfeed_enabled`, `set_crossfeed_profile`, `set_crossfeed_custom_params` |
| **Aux bus** | `set_aux_insert(enabled, wet_mix)` (runtime convolution insert toggle); aux enable/return and per-slot sends via the graph control surface (`set_aux`, `set_slot_send`) |
| **Multichannel** | `set_channel_mix`, `set_channel_policy`, `set_channel_trim`, `set_channel_routing`, `set_channel_eq`, `set_lfe_config`, `set_bass_management` |
| **Output / audiophile** | `set_output_backend`, `set_output_device`, `set_endpoints`, `set_endpoint`, `remove_endpoint`, `clear_endpoints`, `available_devices`*, `set_sample_rate_policy`, `set_bit_perfect`, `set_dither_enabled`, `set_resampler_quality`, `set_limiter_mode`, `set_limiter_true_peak`, `open_asio_control_panel`† |
| **Capture** | `start_capture`, `stop_capture` |
| **Telemetry** | `playback_info`, `state`, `is_playing`, `current_source`, `position_secs`, `position_secs_compensated`, `duration_secs`, `volume`, `speed`, `latency_ms`, `analyzer`, `events`, `clone_event_receiver`, `clone_output_event_receiver` |

`EngineCommand` (raw), `EngineEvent`, `OutputEvent`, and `PlaybackInfo` are all public so
hosts can drive the engine over their own channels or persist command streams.

(* = requires the `audio-output` feature · † = no-op unless the active backend is ASIO and the `asio-native` feature is compiled in.)

**Telemetry** (`PlaybackInfo`) is published every tick: state, decoded + latency-compensated
position, duration, format, volume, speed, latency, bit-perfect status, analyzer levels and
dominant frequency, playlist index/length, per-lane state (`lanes: Vec<LaneInfo>` with slot,
gain, pan, level), per-endpoint state (`endpoints: Vec<EndpointInfo>` with rate, gain,
pending frames, **drift_active / drift_ppm**), and u64 counters (clips, NaNs, underruns, CPU
overloads, deadline misses). Read it lock-free from any thread via `handle.playback_info()`.

**Events** (`EngineEvent`): `SourceOpened`, `PlaybackStarted/Paused/Stopped`,
`SourceFinished`, `FormatChanged`, `SeekCompleted`, `PlaylistChanged`, `LoudnessScanComplete`,
`CaptureStarted/Stopped`, `CaptureError`, `Error`. Device hotplug and endpoint failures use a separate
`OutputEvent` channel (`OutputDeviceChanged`, `DeviceListChanged`, `DeviceConnected`,
`DeviceDisconnected`, `EndpointError`). Endpoint state and dropped frames are also
available through `PlaybackInfo::endpoints` and `endpoint_dropped_frames`.

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

On top of the backends: `endpoint.rs` is the **per-endpoint worker** — one SPSC ring,
rate resampler, `Slip` drift corrector, and realtime thread per configured endpoint, with
start/stop/recovery lifecycle and drift telemetry. Plus `format_converter`,
`output_profile` (per-device profiles), `device_monitor` (hotplug), and `rate_policy`
(track-native / device / fixed rate handling).

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

The repository ships **36 integration/fidelity test files** plus in-crate unit suites —
over 810 tests. Dedicated suites under [`tests/fidelity/`](tests/fidelity/) cover EQ
frequency response, lookahead-limiter correctness and measurement, dither measurement,
resampler quality/measurement, EBU R128, golden reference vectors, decoder robustness +
fuzz mutation, multichannel graph, gapless/crossfade/seamless-seek, timestretch fidelity,
the acoustic world simulation layer, its **acoustic baking** cache, the **Graph 2.0**
general-purpose topology runtime, the **timeline and scheduler** (sample-accurate
events driving the graph), the **aelog deterministic recording/replay** golden-render
pipeline, **graph-wide latency and automatic delay compensation**, **graph-vs-pipeline bit-exact equivalence**,
concurrent ring-buffer stress, and realtime zero-allocation validation. Benchmarks live
in [`benches/`](benches/) (Criterion).

```bash
cargo test                                  # unit + headless integration
cargo test --features tag-write,fingerprint # optional-feature coverage
cargo test --test headless_playback         # embedding lifecycle
cargo test --test realtime_allocation       # zero-allocation on the hot path
cargo test --test graph_pipeline_equivalence# graph ≡ pipeline bit-exact oracle
cargo test --test ring_buffer_stress        # concurrent SPSC stress
cargo bench                                 # DSP / pipeline / graph benchmarks
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
│   ├── commands.rs            # EngineCommand — the host-control surface
│   ├── events.rs · playback_info.rs · playlist.rs · source.rs · sink.rs
│   ├── audio_io.rs · ffi.rs · paths.rs · dsp_utils.rs
│   ├── buffer/                # frames/chunks, lock-free SPSC rings, DSD bytes
│   ├── engine/                # tick loop, handle, stream, construction, output_setup,
│   │                          #   lanes, track_loading, crossfade, recovery, telemetry,
│   │                          #   commands/ (per-domain handlers), decode_loop/, tests/
│   ├── decode/                # Symphonia + native DSD/Opus/TTA/WavPack, channel
│   │                          #   layout/mix, tags, fingerprint, loudness
│   ├── dsp/                   # DSP primitives + pipeline/ (reference oracle)
│   │   │                      #   + graph/ (production: arena + compiled plans,
│   │   │                      #   split into construction/plan/swap/access/controls/
│   │   │                      #   lifecycle/process/limiter/report + nodes/)
│   │   └── resampler/         # Rubato-based resampling
│   ├── spatial/               # Speaker-independent spatial layer (Phases 8–19):
│   │                          #   math/ (Vec3+Quat+coords), scene/object/speaker/
│   │                          #   level/render + panner/ (BasicPanner) +
│   │                          #   vbap/ (3-triplet VBAP) + directivity/,
│   │                          #   occlusion/, spread/ (object behavior) +
│   │                          #   bed/ (channel-based), field/ (diffuse) +
│   │                          #   ambisonic/ (order-1 FOA pinned + order-2/3
│   │                          #   HOA basis, exact rotation, max-rE decoder) +
│   │                          #   room/ (reflections + late field) +
│   │                          #   hrtf/ (Woodworth ITD + Duda-Martens head
│   │                          #   shadow + pinna notch + measured spectral
│   │                          #   HrtfDataset with bilinear interpolation) +
│   │                          #   binaural/ (head-model renderer) +
│   │                          #   tracking/ (head tracking: nlerp + one-pole
│   │                          #   smoothing of IMU/VR orientation samples) +
│   │                          #   scene-file format (Serde save/load) and a
│   │                          #   SpatialNode in the production DSP graph
│   ├── output/                # ALSA / WASAPI / ASIO / CoreAudio / CPAL + endpoint.rs
│   │                          #   (per-endpoint worker + drift correction), device
│   │                          #   monitor, output profiles, WAV writer, loopback
│   └── bin/                   # audio-engine-cli, replaygain-scanner
├── benches/                   # dsp_bench, pipeline_bench, graph_plan_bench, spatial_bench
├── docs/                      # ARCHITECTURE.md, SIGNAL_FLOW.md, EMBEDDING.md, EVOLUTION.md
└── tests/                     # headless_playback.rs, fidelity/ (26 suites)
```

---

## 🤝 Contributing & versioning

This project follows **Semantic Versioning `x.y.z`** (major.minor.patch) with the engine
and `config` crate kept in **lockstep**, a dated `CHANGELOG.md` entry, and a `vX.Y.Z` git
tag on every release. The codebase is deliberately modular and enforces **no god files**:
large single-purpose DSP algorithms are fine, but oversized structs/impls that mix
unrelated concerns must be split by concern (see the `dsp/graph/` impl-split pattern).

Full details — version-bump rules, god-file detection signals, the completeness checklist,
realtime/concurrency rules, and testing guidance for agents and humans — are in
**[`AGENTS.md`](AGENTS.md)**.

---

## 📄 License

Licensed under the [Apache License, Version 2.0](LICENSE-APACHE).
