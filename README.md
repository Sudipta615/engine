# High-Performance Audiophile Independent Core Audio Engine

A reference-grade, bit-perfect, modular, **headless** audio playback and DSP engine written in 100% pure Rust. Engineered for audiophile listening, pro-audio workstations, low-latency performance, and glitch-free real-time audio playback on modern as well as legacy hardware.

The engine is completely independent and headless: it contains zero UI framework dependencies, zero database/library ties, zero playlist policy, and zero OS-specific application assumptions. It is designed to be cleanly embedded into CLI players, desktop GUIs (Slint, Iced, Qt, GTK, egui), streaming daemons, test harnesses, or pro-audio suites.

---

## 🌟 Key Highlights

* **Headless & Policy-Free**: Pure audio engine providing playback infrastructure, DSP, and hardware interfacing. Playlist management, queue logic, shuffle/repeat policy, track databases, and MPRIS metadata are left cleanly to the host application.
* **Explicit Audio Source Abstraction**: First-class [`AudioSource`] enum (`AudioSource::File`, `AudioSource::Uri`, `AudioSource::Memory`) across all loading and crossfading APIs.
* **Separation of Concerns**:
  * **Commands (`EngineCommand`)**: One-way asynchronous control protocol from host to engine.
  * **Telemetry (`PlaybackInfo`)**: Lock-free, atomic, high-frequency state snapshots for UI/monitoring.
  * **Events (`EngineEvent`)**: Discrete lifecycle notifications (`SourceOpened`, `PlaybackStarted`, `PlaybackPaused`, `PlaybackStopped`, `SourceFinished`, `SeekCompleted`, `OutputDeviceChanged`, `FormatChanged`, `Error`).
* **Mastering-Grade Dual Precision**: Dual-path processing architecture supporting both ultra-fast `f32` (performance mode) and `f64` double-precision (mastering grade).
* **Real-Time Safety**: Zero heap allocations on the audio playback path (`realtime_allocation.rs`), guarded by cache-padded lock-free SPSC ring buffers (`FixedFrameBuffer`).
* **Bit-Perfect Hardware Endpoints**: Native OS-level exclusive backends:
  * **Linux**: Direct ALSA (`hw:X` and `plughw:X`) bypassing PulseAudio/PipeWire mixers.
  * **Windows**: Native WASAPI Exclusive (`IAudioClient`) & Native Steinberg ASIO (`IASIO`) without external C++ SDK dependencies.
  * **macOS**: Native CoreAudio Hog-Mode with direct HAL IO procs.
* **Audiophile Codec & DSD Support**: Lossless playback for FLAC, ALAC, WAV, AIFF, APE, WavPack, TTA, Opus, Ogg Vorbis, AAC, MP3, and 1-bit native DSD (DSF / DFF) up to DSD512 (Native wire & DoP).
* **True Gapless & Crossfading**: Dual-decoder state machine for sample-accurate gapless transitions and customizable crossfading curves (Linear, Equal Power, Exponential, S-Curve).
* **Multichannel & Immersive 3D Audio**: Support for mono up to 12-channel 7.1.4 (with 4 height speakers) and custom layouts up to 16 channels, complete with active bass management and per-channel distance delays.
* **Isolated Client Handle (`EngineHandle`)**: A thread-safe, cloneable client API bridge designed for host applications.

---

## 📊 Core Audio Engine Architecture Graph

```text
                               ┌──────────────────────────────────────────────┐
                               │           Host Application (GUI/CLI)         │
                               └──────────────────────┬───────────────────────┘
                                                      │ Commands / Telemetry / Events
                                                      ▼
                               ┌──────────────────────────────────────────────┐
                               │                 EngineHandle                 │
                               │  (Thread-Safe Client, Event Rx, Telemetry)   │
                               └──────────────────────┬───────────────────────┘
                                                      │ EngineCommand Channel
                                                      ▼
┌─────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│                                             AUDIO ENGINE CORE                                               │
│                                                                                                             │
│  ┌──────────────────────┐      ┌───────────────────────────┐      ┌──────────────────────────────────────┐  │
│  │   Decoders (Disk)    │      │  Dual-Decoder Track Mixer │      │         Sinc Resampler               │  │
│  │ FLAC, DSD, WAV, APE, ├─────▶│   (Sample-Accurate Gapless├─────▶│  (Rubato Band-Limited Interpolator   │  │
│  │ Opus, WavPack, etc.  │      │     & Curve Crossfader)   │      │      or Bit-Perfect Passthrough)     │  │
│  └──────────────────────┘      └───────────────────────────┘      └──────────────────┬───────────────────┘  │
│                                                                                      │                      │
│   ┌──────────────────────────────────────────────────────────────────────────────────┴──────────────────┐   │
│   │                                       DSP PROCESSING PIPELINE                                       │   │
│   │                                                                                                     │   │
│   │   [Stereo / Downmix Path]                                [Multichannel Passthrough Path]            │   │
│   │   • Input Preamp Gain                                    • Per-Channel Gain / Polarity Trim         │   │
│   │   • EBU R128 Loudness Normalizer                         • Fractional Distance Delay (0–100 ms)     │   │
│   │   • 64-Band Parametric EQ (AutoEQ / RBJ)                 • Source→Destination Routing Matrix        │   │
│   │   • 31-Band ISO Graphic EQ Layer                         • Independent Per-Channel Parametric EQ    │   │
│   │   • 3-Band Multiband Compressor                          • Active Bass Management High-Pass Filter  │   │
│   │   • FFT Partitioned Convolution (HRTF / Reverb)          • LFE Subwoofer Low-Pass & Gain Crossover  │   │
│   │   • Binaural Crossfeed (Bauer, ChuMoy, JMeier)           • Multichannel Lookahead Peak Limiter      │   │
│   │   • Mid-Side Stereo Enhancer & Balance                                                              │   │
│   │   • WSOLA Timestretcher & Pitch Shifter                                                             │   │
│   │   • 4x True-Peak Oversampled Lookahead Limiter                                                      │   │
│   │   • Perceptual Logarithmic Volume (dB)                                                              │   │
│   └──────────────────────────────────────────────────┬──────────────────────────────────────────────────┘   │
│                                                      │                                                      │
│                                                      ▼                                                      │
│                                      ┌───────────────────────────────┐                                      │
│                                      │  FixedFrameBuffer Ring Buffer │                                      │
│                                      │ (Cache-Padded Lock-Free SPSC) │                                      │
│                                      └───────────────┬───────────────┘                                      │
└──────────────────────────────────────────────────────┼──────────────────────────────────────────────────────┘
                                                       │ Audio Callback / Render Thread
                                                       ▼
┌─────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│                                       BIT-PERFECT OUTPUT BACKENDS                                           │
│                                                                                                             │
│   ┌──────────────────┐   ┌──────────────────┐   ┌──────────────────┐   ┌─────────────────┐   ┌──────────┐   │
│   │ Linux ALSA       │   │ Windows WASAPI   │   │ Windows ASIO     │   │ macOS CoreAudio │   │ CPAL     │   │
│   │ Direct (hw:X)    │   │ Exclusive Mode   │   │ Native (IASIO)   │   │ Hog Mode HAL    │   │ Fallback │   │
│   └────────┬─────────┘   └────────┬─────────┘   └────────┬─────────┘   └────────┬────────┘   └────┬─────┘   │
└────────────┼──────────────────────┼──────────────────────┼──────────────────────┼─────────────────┼─────────┘
             ▼                      ▼                      ▼                      ▼                 ▼
     ┌───────────────────────────────────────────────────────────────────────────────────────────────────┐
     │                                Hardware DAC / Sound Interface                                     │
     └───────────────────────────────────────────────────────────────────────────────────────────────────┘
```

---

## 📂 Modular Structure

```text
engine/
├── crates/
│   └── config/               # Standalone, Serde-serializable engine & DSP configuration models
├── src/
│   ├── source.rs             # AudioSource abstraction (File, Uri, Memory)
│   ├── events.rs             # EngineEvent definitions for discrete lifecycle notifications
│   ├── commands.rs           # EngineCommand enum defining the control protocol
│   ├── playback_info.rs      # Real-time PlaybackInfo & EngineStats telemetry models
│   ├── buffer.rs             # Cache-padded, lock-free ring buffers (FixedFrameBuffer, DsdByteBuffer)
│   ├── decode/               # Decoders (FLAC, DSD DSF/DFF, WAV, ALAC, APE, Opus, WavPack, TTA, etc.)
│   ├── dsp/                  # Mastering DSP algorithms (EQ, Limiter, Convolution, Crossfeed, etc.)
│   ├── output/               # Hardware audio backends (ALSA, WASAPI, ASIO, CoreAudio, CPAL)
│   ├── engine/               # Dual-decoder state machine, clock, and worker thread
│   │   ├── handle.rs         # Safe, decoupled EngineHandle client interface
│   │   ├── commands.rs       # Lock-free command dispatching
│   │   └── decode_loop.rs    # Real-time decoding, resampling & multichannel routing loop
│   ├── bin/
│   │   └── audio_engine_cli.rs # Standalone headless reference player CLI
│   └── lib.rs                # Crate root and prelude exports
└── tests/
    ├── headless_playback.rs  # Headless embedding lifecycle and event integration tests
    └── fidelity/             # Comprehensive DSP fidelity, measurement & stress tests
```

---

## 🚀 Quick Start & Usage

### 1. Basic Playback with `EngineHandle`

```rust
use engine::prelude::*;
use engine::{AudioEngine, EngineConfig, EngineHandle, AudioSource, EngineEvent};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize engine
    let config = EngineConfig::default();
    let mut engine = AudioEngine::new(config)?;
    
    // 2. Obtain decoupled, cloneable handle
    let handle: EngineHandle = engine.handle();
    
    // 3. Start audio worker thread / tick loop
    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });

    // 4. Open audio source and command playback
    handle.open_file("/path/to/song.flac");
    handle.play();
    handle.set_volume_db(-6.0); // Perceptually-correct logarithmic volume

    // 5. Inspect atomic telemetry snapshot
    let info = handle.playback_info();
    println!("Source: {:?}, State: {:?}, Position: {:.2}s / {:.2}s",
        info.current_source, info.state, info.position_secs_compensated, info.duration_secs);

    // 6. Receive discrete engine events
    let events = handle.clone_event_receiver();
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match event {
                EngineEvent::SourceOpened { source, sample_rate, .. } => {
                    println!("Opened: {} at {} Hz", source, sample_rate);
                }
                EngineEvent::PlaybackStarted => println!("Playing!"),
                EngineEvent::PlaybackStopped => println!("Stopped!"),
                EngineEvent::SourceFinished { source } => println!("Finished: {}", source),
                _ => {}
            }
        }
    });

    Ok(())
}
```

### 2. Running the Headless CLI Player

The crate includes an interactive reference command-line player `audio-engine-cli`:

```bash
# Launch interactive REPL
cargo run --bin audio-engine-cli

# Or directly play an audio file or URI
cargo run --bin audio-engine-cli -- /path/to/song.flac
```

---

## 🧪 Verification & Automated Testing

The engine includes a full test suite with 25+ specialized fidelity test harnesses, headless lifecycle tests, stress tests, and mathematical verifications:

```bash
# Run all unit tests and headless integration tests
cargo test

# Run headless playback integration test specifically
cargo test --test headless_playback

# Run real-time zero-allocation validation suite
cargo test --test realtime_allocation

# Run DSP frequency response and true-peak limiter measurements
cargo test --test eq_frequency_response
cargo test --test limiter_measurement

# Run lock-free ring buffer concurrent stress benchmarks
cargo test --test ring_buffer_stress
```

---

## 📄 License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
