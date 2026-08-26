# Embedding the engine

This guide shows real, runnable ways to embed **Freebuff Desktop**'s audio engine into a
host application. It covers the two embedding models, then walks through playback,
telemetry, DSP control, gapless/crossfade, headless analysis, sample capture, and the C
FFI. See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the module map and concurrency model,
and [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md) for the DSP chain.

> **Golden rule:** never call into the engine from a realtime/DSP callback. All host
> interaction goes through `EngineCommand` messages, discrete events, and atomic
> telemetry — the engine's tick thread owns all mutable state.

---

## 1. Add the dependency

The engine is a workspace crate (`engine`) with a companion configuration crate (`config`).

```toml
[dependencies]
engine = { path = "path/to/engine" }
config = { path = "path/to/engine/crates/config" }
```

Enable the features you need (see [Cargo features](../README.md#-cargo-features)). The
default set covers everyday playback:

```toml
engine = { path = "path/to/engine", features = ["audio-output", "resample", "codec-flac"] }
```

For an **output-less / headless host** that only decodes, runs DSP, and reads telemetry
(loudness scanner, visualizer, batch analyzer), you can drop `audio-output`:

```toml
engine = { path = "path/to/engine", default-features = false,
           features = ["resample", "codec-flac", "codec-wav"] }
```

---

## 2. The two embedding models

| Model | Constructor | Output | Use when |
|---|---|---|---|
| **Hardware playback** | `AudioEngine::new(config)` | DAC via ALSA / WASAPI / ASIO / CoreAudio / cpal | A UI/player needs audible output |
| **Headless / sink-driven** | `AudioEngine::with_sink(config, sink)` | Your `SampleSink` (`NoopSink`, `VecSink`, custom) | Analysis, capture-to-buffer, loudness, tests — no DAC |

Both models share the exact same lifecycle below.

---

## 3. Core lifecycle pattern

Every embed follows the same shape:

1. **Construct** the engine (`new` or `with_sink`).
2. **Obtain** a cloneable, thread-safe `EngineHandle`.
3. **Drive** the engine on one background thread via `tick_blocking`.
4. **Listen** for discrete `EngineEvent`s on another thread.
5. **Control** playback and **read** lock-free telemetry from any thread.
6. **Shut down** cleanly: set the tick loop's stop flag, `shutdown()` the handle, join.

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engine::{AudioEngine, EngineConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle = engine.handle();

    // Step 3 — the engine worker. `tick_blocking` sleeps on the command channel,
    // so this thread uses ~0% CPU while idle and wakes instantly on a command.
    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();
    let worker = std::thread::Builder::new()
        .name("engine-worker".into())
        .spawn(move || {
            while engine_running.load(Ordering::Relaxed) {
                engine.tick_blocking(Duration::from_millis(5));
            }
            // Drop: the engine's Drop impl stops the output backend (when present).
        })?;

    // Step 4 — (optional) background event listener.
    // (covered in Example 1 below)

    // Step 5 — control + telemetry from your main/UI thread.
    handle.open_file("/path/to/song.flac");
    handle.play();

    // ... run your app here ...

    // Step 6 — graceful shutdown.
    running.store(false, Ordering::Relaxed);
    let _ = worker.join();
    handle.shutdown();
    Ok(())
}
```

The `running` / worker-thread pattern is also exactly what the built-in C FFI
(`engine_create`) does internally, just moved off into a helper.

---

## 4. Example 1 — Playback + telemetry + events

A complete minimal player that opens a file, plays it, prints telemetry every second, and
logs discrete events.

```rust
use std::time::{Duration, Instant};

use engine::{AudioEngine, EngineConfig, EngineEvent};
use engine::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: player <audio-file>");

    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle = engine.handle();

    // Engine worker thread.
    let worker = std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(Duration::from_millis(5));
        }
    });

    // Event listener thread.
    let events = handle.clone_event_receiver();
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match &event {
                EngineEvent::SourceOpened { source, sample_rate, channels, duration_secs } => {
                    println!("opened {source:?}: {sample_rate} Hz, {channels} ch, {duration_secs:.2}s");
                }
                EngineEvent::PlaybackStarted => println!("[event] playing"),
                EngineEvent::PlaybackPaused => println!("[event] paused"),
                EngineEvent::PlaybackStopped => println!("[event] stopped"),
                EngineEvent::SourceFinished { source } => println!("[event] finished {source:?}"),
                EngineEvent::SeekCompleted { position_secs } => println!("[event] seek -> {position_secs:.2}s"),
                EngineEvent::Error(msg) => eprintln!("[event] error: {msg}"),
                _ => {}
            }
        }
    });

    // Play.
    handle.open_file(path);
    handle.play();

    // Poll lock-free telemetry.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        let info = handle.playback_info();
        println!(
            "t={info.position_secs_compensated:6.2}s / {info.duration_secs:6.2}s  \
             state={:?}  {} Hz  vol={:.2}  latency={:.1} ms  bit-perfect={}",
            info.state, info.sample_rate, info.volume, info.latency_ms, info.bit_perfect,
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    handle.stop();
    handle.shutdown();
    let _ = worker.join();
    Ok(())
}
```

**Notes**
- `engine.is_running()` / `engine.tick_blocking()` are the driving calls; the FFI tick
  thread and the reference CLI use the identical loop.
- `info.position_secs` is the decoder position; `position_secs_compensated` is what the
  DAC is currently outputting (already latency-adjusted). Prefer the compensated value
  for UI/clocks.

---

## 5. Example 2 — Headless analysis without a DAC (`with_sink`)

If your host only needs decode + DSP + telemetry (loudness, levels, position) and must not
grab an audio device — embed with a `NoopSink`.

```rust
use std::time::Duration;

use engine::{AudioEngine, EngineConfig};
use engine::sink::NoopSink;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: analyze <audio-file>");

    // `audio-output` is not needed here; `with_sink(NoopSink)` runs the whole
    // decode → DSP → limiter path but discards the samples.
    let mut engine = AudioEngine::with_sink(EngineConfig::default(), Box::new(NoopSink))?;
    let handle = engine.handle();

    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(Duration::from_millis(5));
        }
    });

    handle.open_file(path);
    handle.play();

    // Read analyzer levels + telemetry without ever opening a device.
    let analyzer = handle.analyzer();
    while handle.is_playing() {
        let snap = analyzer.snapshot();
        println!(
            "pos={:.2}s  peak L/R = {:.1}/{:.1} dBFS  dom.freq = {}",
            handle.position_secs(),
            snap.peak_db_l, snap.peak_db_r,
            snap.dominant_frequency_hz().map(|f| format!("{f:.0} Hz")).unwrap_or_else(|| "-".into()),
        );
        if let Some(stats) = handle.playback_info().engine_stats {
            println!("  codec: {}  backend: {}", stats.decoder_format, stats.output_backend);
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    handle.shutdown();
    Ok(())
}
```

> Use `VecSink` instead of `NoopSink` when you need the decoded samples themselves —
> see Example 5.

---

## 6. Example 3 — DSP control: EQ, balance, speed, pitch

`EngineHandle` exposes typed setters that do not block the audio path. The EQ is a
64-band parametric plus a 10/15/31-band graphic layer; volume is perceptual (dB).

```rust
use engine::{AudioEngine, EngineConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle = engine.handle();
    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(std::time::Duration::from_millis(5));
        }
    });

    handle.open_file("/path/to/song.flac");
    handle.play();

    handle.set_volume_db(-8.0);          // perceptual, -60..0 dB
    handle.set_balance(0.2);             // -1.0 (L) .. 1.0 (R)
    handle.set_preamp(2.0);              // dB

    // A gentle low-shelf at 100 Hz, +3 dB, Q 0.7.
    handle.set_eq_enabled(true);
    handle.set_eq_band(0, 100.0, 3.0, 0.7, true);

    // Varispeed 1.25x (pitch follows speed). For pitch-constant time-stretch:
    //   handle.set_speed_mode(config::SpeedMode::TimeStretch);
    handle.set_speed(1.25);

    // Delay changes take one command — no lock, no allocation on the audio path.
    std::thread::sleep(std::time::Duration::from_millis(3000));
    handle.set_speed(1.0);
    handle.set_volume_db(0.0);
    handle.stop();
    handle.shutdown();
    Ok(())
}
```

For a graphic-EQ host UI:

```rust
handle.set_graphic_eq_layout(config::GraphicEqLayout::ThirtyOneBand); // one-time
handle.set_graphic_eq_slider(12, -4.0);   // band 12, -4 dB
handle.set_graphic_eq_enabled(true);
// The graphic-EQ preamp has no dedicated handle setter yet — use the raw command:
let _ = handle.send_command(EngineCommand::SetGraphicEqPreamp(-1.0));
```

---

## 7. Example 4 — Gapless & crossfade transitions

The engine pre-loads a **next** decoder so the track boundary is seamless. Set the
`TransitionMode` (default `Gapless`) and, for a crossfade, the curve + duration.

Transitions are configured either at construction (via `EngineConfig`) or at runtime via
a raw `EngineCommand` (there isn't a dedicated convenience setter on `EngineHandle`):

```rust
use config::{EngineConfig, CrossfadeConfig, CrossfadeCurve, TransitionMode};
use engine::{AudioEngine, EngineCommand};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Option A — configure at construction:
    let mut config = EngineConfig::default();
    config.transition_mode = TransitionMode::Crossfade;
    config.crossfade = CrossfadeConfig {
        enabled: true,
        duration_ms: 3000,            // 3 s overlap
        curve: CrossfadeCurve::ConstantPower,
    };
    let mut engine = AudioEngine::new(config)?;
    let handle = engine.handle();
    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(std::time::Duration::from_millis(5));
        }
    });

    // Option B — change at runtime (transition to gapless):
    let _ = handle.send_command(EngineCommand::SetTransitionMode(TransitionMode::Gapless));

    // Queue two tracks. `prepare_next_file` pre-opens the next decoder so the
    // playback → second-track handoff is sample-accurate.
    handle.open_file("/playlist/01.flac");
    handle.prepare_next_file("/playlist/02.flac");
    handle.play();
    // At EndOfStream the engine auto-advances the queue per `RepeatMode`
    // (Off / All / One) and reacts to `TransitionMode`.

    std::thread::sleep(std::time::Duration::from_millis(5000));
    handle.stop();
    handle.shutdown();
    Ok(())
}
```

**Notes**
- `TransitionMode::Gapless` hands off at the logical EOS with zero silence or overlap.
- `TransitionMode::Crossfade` blends over `CrossfadeConfig::duration_ms`.
- `RepeatMode::One` restarts the current track at EOS; `RepeatMode::All` wraps.
- `EngineEvent::PlaylistChanged { current_index, length }` fires on every queue change.

---

## 8. Example 5 — Capture processed samples with a custom `SampleSink`

`AudioEngine::with_sink` takes **ownership** of your sink, so to pull the decoded samples
from another thread you wrap the sink's buffer in an `Arc<Mutex>`, keep a clone of the
`Arc`, and drain it in your host thread. This receives the interleaved f32 stream after
the resampler and safety limiter, at the output channel count.

```rust
use std::sync::{Arc, Mutex};
use std::time::{Duration};

use engine::{AudioEngine, EngineConfig};
use engine::sink::SampleSink;

/// Allocation happens on the first pushes only; the steady-state path stays
/// allocation-free (we keep one Vec and `extend_from_slice` into it, which
/// reuses capacity). Good enough for capture; flag a realtime contract if you
/// ship a sink on a hard RT path.
#[derive(Clone, Default)]
struct RingSink {
    buf: Arc<Mutex<Vec<f32>>>,
}

impl SampleSink for RingSink {
    fn push_interleaved(&self, samples: &[f32], channels: usize) -> usize {
        self.buf.lock().unwrap().extend_from_slice(samples);
        samples.len() / channels.max(1) // frames accepted = all of them
    }
    fn reset(&self) {
        self.buf.lock().unwrap().clear();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sink = RingSink::default();
    let ring = sink.clone(); // host-side handle to the shared buffer

    let mut engine = AudioEngine::with_sink(EngineConfig::default(), Box::new(sink))?;
    let handle = engine.handle();
    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(Duration::from_millis(5));
        }
    });

    handle.open_file("/path/to/song.flac");
    handle.play();
    std::thread::sleep(Duration::from_secs(2)); // let the engine decode into the sink
    handle.pause();

    // Pull the captured interleaved samples from the host thread.
    let captured: Vec<f32> = ring.buf.lock().unwrap().clone();
    println!("captured {} interleaved samples", captured.len());

    handle.shutdown();
    Ok(())
}
```

> The built-in `VecSink` (in `engine::sink`) is the same idea with a pre-built
> `Mutex<Vec<f32>>` inside; its `take()` clears and returns the buffer, `clone_samples()`
> copies without clearing. It's owned by the engine, so for cross-thread capture prefer a
> sink backed by your own `Arc` (above).

---

## 9. C FFI

Enable the `c-ffi` feature to expose a stable `extern "C"` API for C, C++, Python
(ctypes), C#, Node.js FFI, etc. Handles are opaque, every call returns a status code, and
no panics ever cross the boundary.

```c
#include <stdint.h>
#include <stdio.h>

typedef struct EngineHandleFFI EngineHandleFFI;

/* lifecycle */
EngineHandleFFI* engine_create(uint32_t backend); /* 0 Auto, 4 Default */
void             engine_destroy(EngineHandleFFI* h);

/* transport */
int32_t engine_play(EngineHandleFFI* h);
int32_t engine_pause(EngineHandleFFI* h);
int32_t engine_stop(EngineHandleFFI* h);
int32_t engine_seek(EngineHandleFFI* h, float position_secs);
int32_t engine_set_volume(EngineHandleFFI* h, float linear);
int32_t engine_set_volume_db(EngineHandleFFI* h, float db);
int32_t engine_set_speed(EngineHandleFFI* h, float speed);

/* sources & queue */
int32_t engine_open_file(EngineHandleFFI* h, const char* path);
int32_t engine_open_uri(EngineHandleFFI* h, const char* uri);
int32_t engine_enqueue_file(EngineHandleFFI* h, const char* path);
int32_t engine_next(EngineHandleFFI* h);
int32_t engine_previous(EngineHandleFFI* h);
int32_t engine_clear_playlist(EngineHandleFFI* h);

/* queries */
float   engine_position_secs(EngineHandleFFI* h);  /* -1.0 on error */
float   engine_duration_secs(EngineHandleFFI* h);  /* -1.0 on error */
int32_t engine_playback_state(EngineHandleFFI* h); /* 0 stopped,1 playing,2 paused,3 buffering */
int64_t engine_playlist_len(EngineHandleFFI* h);   /* -1 on error */
```

Minimal C program:

```c
#include <stdio.h>
#include <unistd.h>

int main(int argc, char** argv) {
    EngineHandleFFI* h = engine_create(0);          /* ENGINE_BACKEND_AUTO */
    if (!h) { fprintf(stderr, "engine_create failed\n"); return 1; }

    engine_open_file(h, argv[1]);
    engine_play(h);
    engine_set_volume_db(h, -6.0f);

    for (int i = 0; i < 20; i++) {
        printf("pos=%.2fs / %.2fs (state=%d)\n",
               engine_position_secs(h), engine_duration_secs(h), engine_playback_state(h));
        usleep(250000);
    }

    engine_stop(h);
    engine_destroy(h);
    return 0;
}
```

**Status codes** (`i32`): `0=Ok`, `-1=Error`, `-2=InvalidHandle`, `-3=InvalidArgument`,
`-4=EngineNotRunning`.

> **Current FFI surface.** The C API covers lifecycle, transport, source open, queue
> next/previous, and the position/state/duration queries. DSP controls (EQ bands,
> crossfade config, channel routing) and event subscription are **not yet exported**
> over C — if your host needs them over FFI, add `#[no_mangle] extern "C"` wrappers in
> [`src/ffi.rs`](../src/ffi.rs) following the existing opaque-handle + status-code
> pattern, or drive the engine's public `EngineCommand` type from Rust instead. The Rust
> `EngineHandle` is the complete API; the FFI is a subset.

---

## 10. Reference cheat-sheet

### `EngineHandle` — telemetry readers (lock-free, callable from any thread)

| Method | Returns |
|---|---|
| `playback_info()` | Full `PlaybackInfo` snapshot (`Clone`) |
| `state()` / `is_playing()` | `PlaybackState` / focus boolean |
| `current_source()` | `Option<AudioSource>` |
| `position_secs()` / `position_secs_compensated()` | decoder / DAC position (s) |
| `duration_secs()` | track duration (s) |
| `volume()` / `speed()` / `latency_ms()` | live values |
| `playlist_len()` / `playlist_index()` | queue info |
| `analyzer()` | `Arc<AudioAnalyzer>` for `snapshot()` (levels + spectrum) |
| `clone_event_receiver()` | `Receiver<EngineEvent>` |
| `clone_output_event_receiver()` | `Receiver<OutputEvent>` (`audio-output` only) |

### `EngineEvent` (discrete, async)

`PlaylistChanged`, `PlaybackStarted/Paused/Stopped`, `SourceOpened {source, sample_rate,
channels, duration_secs}`, `SourceFinished`, `FormatChanged`, `SeekCompleted`,
`LoudnessScanComplete`, `CaptureStarted/Stopped`, `CaptureError`, `Error(String)`.

### `OutputEvent` (device hotplug; `audio-output` only)

`OutputDeviceChanged`, `DeviceListChanged`, `DeviceConnected`, `DeviceDisconnected`.

### Realtime-safety contract (for `SampleSink` implementors)

- Called from the engine's tick thread (not a hardware callback) — tiny blocking is OK.
- **No allocation in steady state**; no panics on valid samples; `channels ≥ 1` and
  `samples.len()` is a multiple of `channels`.
- `push_interleaved` returns frames accepted (`samples.len() / channels`); return less to
  throttle; the engine preserves and retries the unwritten tail.

### Error handling

- `AudioEngine::new` / `with_sink` / `new_default` return
  `Result<Self, engine::EngineError>`.
- Non-fatal decode/output problems surface as `EngineEvent::Error(String)` and
  `OutputEvent` rather than panics; the engine has a recovery path for device
  disconnects/hotplug in exclusive mode.
- Call `config.validate()` before construction to surface contradictory settings early
  (e.g. bit-perfect intent vs. dither enabled).

---

## See also

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, concurrency model, realtime rules.
- [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md) — sample path, precision/bypass modes, side paths.
- [`src/ffi.rs`](../src/ffi.rs) — the complete C export list and the type mapping table.
- [`src/sink.rs`](../src/sink.rs), [`src/engine/handle.rs`](../src/engine/handle.rs) —
  the sink trait and every host-facing method.
- [`AGENTS.md`](../AGENTS.md) — contributing, versioning, and the completeness checklist.