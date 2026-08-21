# High-Performance Audiophile Core Audio Engine

A reference-grade, bit-perfect, modular audio playback and DSP engine written in 100% pure Rust. Engineered for audiophile listening, pro-audio workstations, low-latency performance, and glitch-free real-time audio playback on modern as well as legacy hardware.

---

## 🌟 Key Highlights

* **Mastering-Grade Dual Precision**: Dual-path processing architecture supporting both ultra-fast `f32` (performance mode) and `f64` double-precision (mastering grade).
* **Real-Time Safety**: Zero heap allocations on the audio playback path (`realtime_allocation.rs`), guarded by cache-padded lock-free SPSC ring buffers (`FixedFrameBuffer`).
* **Bit-Perfect Hardware Endpoints**: Native OS-level exclusive backends:
  * **Linux**: Direct ALSA (`hw:X` and `plughw:X`) bypassing PulseAudio/PipeWire mixers.
  * **Windows**: Native WASAPI Exclusive (`IAudioClient`) & Native Steinberg ASIO (`IASIO`) without external C++ SDK dependencies.
  * **macOS**: Native CoreAudio Hog-Mode with direct HAL IO procs.
* **Audiophile Codec & DSD Support**: Lossless playback for FLAC, ALAC, WAV, AIFF, APE, WavPack, TTA, Opus, Ogg Vorbis, AAC, MP3, and 1-bit native DSD (DSF / DFF) up to DSD512 (Native wire & DoP).
* **True Gapless & Crossfading**: Dual-decoder state machine for sample-accurate gapless transitions and customizable crossfading curves (Linear, Equal Power, Exponential, S-Curve).
* **Multichannel & Immersive 3D Audio**: Support for mono up to 12-channel 7.1.4 (with 4 height speakers) and custom layouts up to 16 channels, complete with active bass management and per-channel distance delays.
* **Isolated Client Handle (`EngineHandle`)**: A thread-safe, decoupled client API layer designed for modular UI and controller architectures.

---

## 📊 Core Audio Engine Architecture Graph

```text
                               ┌──────────────────────────────────────────────┐
                               │             External UI / Controller         │
                               └──────────────────────┬───────────────────────┘
                                                      │ Commands / Telemetry
                                                      ▼
                               ┌──────────────────────────────────────────────┐
                               │                 EngineHandle                 │
                               │  (Thread-Safe Client & State Snapshot API)   │
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
│   ├── buffer.rs             # Cache-padded, lock-free ring buffers (FixedFrameBuffer, DsdByteBuffer)
│   ├── commands.rs           # EngineCommand enum defining the complete control protocol
│   ├── playback_info.rs      # Real-time PlaybackInfo & EngineStats telemetry models
│   ├── decode/               # Decoders (FLAC, DSD DSF/DFF, WAV, ALAC, APE, Opus, WavPack, TTA, etc.)
│   ├── dsp/                  # Mastering DSP algorithms (EQ, Limiter, Convolution, Crossfeed, etc.)
│   │   ├── pipeline/         # Orchestrated DspPipeline signal graph
│   │   ├── channel_trim.rs   # Multichannel speaker calibration & routing matrix
│   │   ├── crossfeed.rs      # Binaural headphone spatialization
│   │   ├── convolution.rs    # Real FFT partitioned convolution engine
│   │   ├── limiter.rs        # True-peak 4x oversampled lookahead limiter
│   │   ├── multiband_compressor.rs # 3-band Linkwitz-Riley crossover compressor
│   │   └── equalizer.rs      # 64-band RBJ biquad equalizer & AutoEQ preset parser
│   ├── output/               # Hardware audio backends
│   │   ├── alsa_output/      # Native Linux ALSA direct hw: backend
│   │   ├── wasapi_output/    # Native Windows WASAPI exclusive IAudioClient backend
│   │   ├── asio_output/      # Native Windows Steinberg ASIO COM backend
│   │   ├── coreaudio_output/ # Native macOS CoreAudio Hog Mode HAL backend
│   │   └── cpal_output/      # Cross-platform shared-mode fallback
│   ├── engine/               # Dual-decoder state machine, clock, and worker thread
│   │   ├── handle.rs         # Safe, decoupled EngineHandle client interface
│   │   ├── commands.rs       # Lock-free command dispatching
│   │   └── decode_loop.rs    # Real-time decoding, resampling & multichannel routing loop
│   └── lib.rs                # Crate root and prelude exports
└── tests/
    └── fidelity/             # Comprehensive DSP fidelity, measurement & stress tests
```

---

## 🎛️ Feature & Module Breakdown

### 1. Decoding & Format Demuxing (`src/decode/`)
* **Uncompressed / Lossless**: FLAC, WAV, AIFF, ALAC, Monkey's Audio (APE), WavPack (v5 lossless), True Audio (TTA).
* **Lossy**: Ogg Opus (RFC 8251 pure-Rust), AAC, MP3, Ogg Vorbis, Matroska Audio (MKA/MKV).
* **Direct Stream Digital (DSD)**: DSF and DFF file readers with 1-bit native wire packing and DSD-over-PCM (DoP v1.1) formatting (DSD64 to DSD512).
* **CUE Sheet & Gapless Metadata**: Accurate index parsing, lead-in/lead-out pruning, and gapless sample count extraction.

### 2. Audio DSP & Mastering Suite (`src/dsp/`)
* **64-Band Parametric EQ**: RBJ filter types (Peaking, Low/High Shelf, Low/High Pass, Band Pass, Notch) with instant AutoEQ preset import.
* **31-Band ISO Graphic EQ**: Standard 1/3-octave ISO center frequencies with auto-makeup headroom.
* **Headphone Crossfeed**: Removes "inside-the-head" lateralization via Bauer, ChuMoy, Jan Meier, or custom interaural time delay (ITD) models.
* **Partitioned Convolution**: Fast, low-latency partitioned FFT convolution for Room Correction and Head-Related Transfer Function (HRTF) binaural 3D audio.
* **3-Band Multiband Compressor**: 4th-order Linkwitz-Riley (LR4) phase-compensated crossover filters with per-band attack, release, threshold, and makeup gain.
* **4x True-Peak Lookahead Limiter**: Polyphase FIR oversampling detecting inter-sample peaks with zero distortion.
* **Loudness Normalization**: EBU R128 and ITU-R BS.1770-4 loudness measurement and normalization with background thread scanning.
* **High-Order Sinc Resampler**: Band-limited sinc interpolation with sub-sample phase continuity powered by Rubato.
* **Audiophile Dithering**: Triangular Probability Density Function (TPDF) and noise-shaped dithering when quantizing to 16-bit or 24-bit PCM.

### 3. Multichannel & Spatial Audio
* **Supported Layouts**: Mono, Stereo, 2.1, 3.0, 3.1, 4.0, 4.1, 5.0, 5.1, 6.1, 7.0, 7.1, and **7.1.4 Immersive** (with 4 height speakers).
* **Bass Management**: Second-order high-pass filtering on main speakers paired with dedicated subwoofer low-pass crossovers.
* **Speaker Alignment**: Per-channel fractional millisecond delay (0–100 ms), gain trim, and polarity inversion.
* **Upmix & Downmix Matrix**: ITU-R BS.775 stereo downmixing, stereo $\to$ 5.1/7.1 upmixing templates, and custom $[N \times M]$ channel routing matrices.

### 4. Bit-Perfect Hardware Endpoints (`src/output/`)
* **ALSA Direct (`hw:`)**: Linux direct hardware streaming with software mixer bypass.
* **WASAPI Exclusive (`IAudioClient`)**: Windows bit-perfect streaming with verified OS exclusivity and hardware volume event hooks.
* **Steinberg ASIO (`IASIO`)**: 100% pure-Rust native ASIO COM driver integration with registry driver enumeration and native 1-bit DSD transport.
* **CoreAudio Hog Mode**: macOS direct device acquisition with exclusive hardware clock locking.

---

## 🚀 Quick Start & Usage

### 1. Basic Playback with `EngineHandle`

```rust
use engine::prelude::*;
use config::EngineConfig;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize engine with default audiophile configuration
    let config = EngineConfig::default();
    let mut engine = AudioEngine::new(config)?;
    
    // 2. Obtain the decoupled, cloneable handle for UI / Controller
    let handle: EngineHandle = engine.handle();
    
    // 3. Start audio worker thread
    engine.start()?;

    // 4. Send non-blocking commands through the handle
    handle.open_uri("file:///music/audiophile_track.flac");
    handle.play();
    handle.set_volume_db(-6.0); // Perceptually-correct logarithmic volume

    // 5. Inspect atomic telemetry snapshot
    let info = handle.playback_info();
    println!("State: {:?}, Position: {:.2}s / {:.2}s (Latency: {:.1}ms)",
        info.state, info.position_secs_compensated, info.duration_secs, info.latency_ms);

    Ok(())
}
```

### 2. Enabling Headphone Crossfeed & Parametric EQ

```rust
use config::CrossfeedProfile;

// Activate Bauer binaural headphone spatial crossfeed
handle.set_crossfeed_enabled(true);
handle.set_crossfeed_profile(CrossfeedProfile::Bauer);

// Enable 64-band Parametric EQ
handle.set_eq_enabled(true);
handle.set_eq_band(0, 100.0, 3.5, 0.7071, true); // +3.5 dB bass boost at 100 Hz
```

### 3. Configuring Multichannel & Spatial Audio

```rust
use config::{ChannelMixConfig, ChannelMixTemplate, ChannelPolicy};

// Downmix a 5.1/7.1 track to stereo for 2-channel headphones:
handle.set_channel_policy(ChannelPolicy::StereoDownmix);
handle.set_channel_mix(ChannelMixConfig {
    enabled: true,
    template: ChannelMixTemplate::FiveOneToStereo,
});
```

---

## 🧪 Verification & Automated Testing

The engine includes a full test suite with 25 specialized fidelity test harnesses, stress tests, and mathematical verifications:

```bash
# Run all unit tests and fidelity measurement suites
cargo test

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
