# Shadow Desktop Audio Engine — Owner's Guide & Architectural Map

**Version:** 3.49.0 (engine + config in lockstep)
**License:** Apache-2.0
**Language:** 100% pure Rust (no C/C++ components)
**Audience:** the project owner and directors — people who need a reliable mental
model of the entire engine, without needing to read Rust code.

This guide was written by reading the actual source code, not just the
marketing-facing documents. Where the README or other docs disagree with the
code, the **code wins**, and the discrepancy is called out explicitly.

---

## How to read this guide

| Section | What you get |
|---|---|
| 1. Executive Overview | What this engine is, in plain English |
| 2. Capability Map | A complete inventory of what it can do |
| 3. The Audio Journey | Exactly what happens to sound, start to finish |
| 4. Architecture for a Non-Developer | The repository as a map of responsibilities |
| 5. Subsystem Encyclopedia | One detailed entry per major subsystem |
| 6. Spatial Audio Deep Dive | Objects, rooms, HRTFs, ambisonics, binaural — explained |
| 7. DSP & Processing Graph | How the signal-processing chain is built and executed |
| 8. Real-Time Safety | What must never happen on the audio thread |
| 9. Output & Bit-Perfect Operation | Devices, exclusive mode, DSD, when audio is preserved exactly |
| 10. Analysis & Audio Intelligence | Everything the engine already "knows" about audio |
| 11. Testing, Fidelity, Determinism | How the engine proves it works |
| 12. Performance Architecture | Where CPU and memory go |
| 13. Configuration & Feature Interaction | The settings that matter and how they interact |
| 14. What It Does Especially Well | Its genuine strengths, with reasons |
| 15. Remaining Gaps & Limitations | Honest list of what is missing or partial |
| 16. Recommended Stopping Point | Should you keep building the engine, or build products on it? |
| 17. Owner's Quick Reference | The one-page summary to return to |
| 18. Glossary | Every technical term, explained in plain language |
| 19. Developer/AI Handoff | The technical appendix for future engineers and AI agents |

**Status vocabulary used throughout:**

- **Fully implemented** — the feature exists in the code, is wired in, and is
  covered by tests.
- **Partially implemented** — it exists but is limited to certain platforms,
  formats, or conditions; the limitation is documented.
- **Offline-only** — fully implemented but runs outside the real-time audio
  path (rendering, analysis, simulation), not on live playback.
- **Documented seam / future** — the design explicitly reserves a place for
  it, but the code does not implement it yet.
- **Tests-only / scaffolding** — exists so tests can exercise something, but
  is not part of the product surface.

---

# 1. Executive Overview

## 1.1 What this engine is

**Shadow Desktop** is a complete, professional-grade *audio playback and
signal-processing engine* — the part of a music player or studio application
that actually turns a file on disk into sound coming out of your speakers or
headphones. It is written entirely in the Rust programming language, with no
borrowed third-party C/C++ components.

"Engine" is the key word: it has **no user interface of its own**. It is a
library that another program (a desktop app, a command-line player, a
streaming daemon, a test harness, or a C/C++ program via a compatibility
layer) embeds and controls. Think of it as the engine block of a car — you
still need a chassis, wheels, and a dashboard, but the part that generates
the power is complete and self-contained.

## 1.2 What problem it solves

Building high-quality audio software is famously hard for three reasons:

1. **Glitches are unacceptable.** An audio stream must deliver sample after
   sample with millisecond-level timing for hours on end. Any hiccup — a
   moment where the program pauses to allocate memory, wait for a lock, or
   read a disk — is heard as a pop, click, dropout, or stutter.
2. **Audio quality is measurable and people care.** Enthusiasts can hear (and
   measure) tiny differences in loudness handling, resampling quality,
   dithering, and whether the signal was altered at all.
3. **The audio world is fragmented.** Files come in dozens of formats; devices
   come in shared and exclusive modes; the three major operating systems each
   have completely different audio APIs; and every DAC (digital-to-analog
   converter) has its own clock that drifts from the computer's.

This engine solves all three: it is engineered so the real-time audio path
never stutters (no memory allocation, no locking — verified by automated
tests), it implements the DSP (digital signal processing) chain with
measurement-grade fidelity, and it abstracts away the platform/device
fragmentation behind a single clean control interface.

## 1.3 What type of audio engine it has become

Over its evolution it has grown through several identities, all of which are
still present:

- **A bit-perfect audiophile player** — can send the exact original bits of a
  file to a DAC with nothing altered, when the hardware and mode allow it.
- **A full studio-grade DSP processor** — a chain of effects (EQ, compression,
  convolution reverb, crossfeed, stereo enhancement, time-stretch,
  room/headphone correction, loudness normalization) that can be switched on
  and off without disturbing the audio.
- **A multi-stream mixer** — one main track, a gapless/crossfading transition
  partner, and several independent "lane" tracks playing simultaneously, each
  with its own gain, pan, sends, and automation — mixed through an
  N-input bus with a separate "aux" (effects send) bus.
- **A multi-output routing matrix** — one mix can be sent to several physical
  devices at once (e.g. main DAC + studio monitors + a second room), each
  device's clock drift corrected independently.
- **A spatial audio renderer** — an optional, self-contained layer that places
  sound *objects* in 3D world space and renders them to any speaker layout
  (stereo, 5.1, 7.1, 7.1.4, custom arrays) or to headphones binaurally, with
  room acoustics, head tracking, and measured HRTF data.
- **A deterministic render/test laboratory** — the same session can be
  recorded and replayed bit-for-bit, making regressions detectable
  automatically. This is unusual and valuable.
- **An offline analysis toolkit** — loudness measurement (EBU R128 /
  ReplayGain), fingerprinting, and a full perceptual "audio profile" of any
  file.

## 1.4 What it can do (the 30-second version)

- Plays **FLAC, ALAC, WAV, AIFF, MP3, AAC, Ogg Vorbis, Opus, TTA, WavPack,
  APE, PCM** and **DSD** (DSF/DFF, up to DSD512 on native transports), from
  files, HTTP(S) URLs, or memory buffers.
- Processes audio through a **64-band parametric EQ** (+ a 10/15/31-band
  graphic EQ layer + AutoEQ presets), a **3-band compressor**, **FFT
  convolution** (reverb/impulse responses), **headphone crossfeed**, a
  **mid-side stereo enhancer**, **time-stretch/pitch-shift**, a **true-peak
  lookahead limiter**, **dither**, and **room/headphone correction**.
- Measures **loudness** (EBU R128 / ReplayGain) and can write the results back
  into file tags.
- **Fingerprints** audio (Chromaprint/AcoustID).
- Handles **gapless, crossfade, fade, and stop** transitions between tracks.
- Outputs to **ALSA, WASAPI, ASIO, CoreAudio, or a portable fallback**, in
  exclusive/direct modes when available, to **several devices at once**, with
  per-device **clock-drift correction**.
- Renders **spatial scenes** (objects, beds, fields) to any layout or
  binaurally, with **room simulation, ambisonics (up to order 3), HRTF
  datasets, head tracking, and scene persistence**.
- Can be driven from **Rust**, or from **C/C++ and any C-callable language**
  through a stable C FFI (foreign function interface).
- Records the **system audio mix** (Windows) to a WAV file.
- Replays recorded sessions **bit-for-bit identically** for regression testing.

## 1.5 What makes it technically sophisticated

1. **A compiled DSP graph as the production path.** The effects chain is not a
   hand-written list of function calls; it is a *graph of nodes* that is
   *compiled into an execution plan* at startup. Stage order is data, not
   code — which means the whole chain can be **reconfigured live during
   playback** by building a fresh plan and atomically swapping it in at a
   block boundary, with zero glitch.
2. **Zero-allocation, zero-lock real-time path.** The audio thread never
   allocates memory and never takes a lock — verified by a dedicated automated
   test that runs 10,000+ blocks and asserts zero heap allocations.
3. **Bit-exact determinism.** The engine keeps a "reference" DSP pipeline that
   is bit-for-bit identical to the production graph, plus golden-reference
   vectors, so a change that alters audio by even one least-significant bit is
   caught automatically.
4. **A real head-and-room acoustic model.** Not a canned "reverb effect": an
   actual geometric model of a room (walls with frequency-dependent
   materials, openings, diffraction edges), a solver that enumerates sound
   paths, and a binaural head model (interaural time difference + head
   shadow + measured HRTF data).
5. **Drift-corrected multi-device output.** Each output device's clock runs at
   a slightly different speed; the engine trims each stream to its device's
   actual clock and reports the offset in parts-per-million.
6. **Deterministic session recording/replay.** Every control change and audio
   input of a session can be logged and replayed to produce byte-identical
   audio — a golden-render substrate for regression testing and bug reports.
7. **Double precision on demand.** Every DSP stage can run in fast single
   precision (f32) or mastering-grade double precision (f64), selectable per
   session.

## 1.6 Major design philosophies

- **Bit-perfect by default; processed only on request.** When you ask for
  untouched output, the engine will literally bypass every DSP stage (and
  tell you honestly whether it *can* prove the path is bit-perfect — it will
  not pretend).
- **"Disabled = literally absent."** A feature that is turned off is skipped
  so completely that its absence is bit-exact — it doesn't even multiply by
  one. This is what makes the whole system so stable: toggling features on
  and off cannot change the audio by accident.
- **Real-time safety is non-negotiable.** No allocations, no locks, no I/O on
  the audio thread. Expensive work happens on worker threads; the audio
  thread only reads precomputed state.
- **The realtime path and the analysis path are separate.** Measurement,
  correction derivation, loudness scanning, spatial solving, and profile
  analysis run off the audio thread, so they can be slow and thorough without
  ever causing a glitch.
- **Channels describe the reproduction system; objects describe the content.**
  A spatial scene is authored in world space and rendered to *whatever*
  speakers are present — the same scene serves stereo, 5.1, 7.1.4, or
  headphones.
- **Honest partial support.** When a format feature is not supported (e.g.
  multichannel WavPack, NetCDF-4 SOFA files), the engine rejects it with a
  typed, explicit error — it never silently degrades or misrepresents.
- **Modularity.** The codebase is deliberately split into small,
  single-responsibility modules. There is a written rule against "god files"
  (oversized files owning many unrelated concerns), enforced in review.

## 1.7 What it is NOT intended to be

- **Not a music application.** No UI, no library management, no metadata
  browsing, no artwork viewer, no remote control of its own. It is the
  engine *under* such an app.
- **Not a DAW.** It is a playback/DSP engine, not a multitrack recording
  workstation. It has no recording timeline (though it has a *rendering*
  timeline used offline), no MIDI sequencing instruments, and no editing.
- **Not a streaming service.** It can fetch audio over HTTP(S), but it has no
  catalog, no accounts, no DRM.
- **Not a general-purpose audio I/O toolkit.** It has one input path of note
  (WASAPI system-loopback capture on Windows) and no general microphone/line
  input backend.
- **Not a spatial format decoder.** It is an *independent* spatial
  implementation — no Dolby/DTS bitstream decoding, no proprietary metadata.
- **Not a "smart" engine.** The analysis layer is deterministic DSP with
  explicit, documented heuristics — deliberately *not* a trained neural
  network. (See Section 10 for what it knows and doesn't.)

## 1.8 Current maturity level

- Version 3.49.0, semantic versioning (API-stable, backward-compatible
  additions since 3.0).
- ~52,000 lines of Rust in the engine crate, plus the config crate.
- **58 test files** (55 dedicated fidelity/measurement suites + 3 headless
  integration tests) containing roughly **1,300 test functions**, plus 4
  benchmark suites.
- All phases in the design history are marked **Done**; the feature set is
  broad and internally consistent.
- Mature enough that the honest recommendation in Section 16 is: **yes — this
  is ready to be a stable foundation for products.** The engine phase of
  development should wind down; the product phase should begin.

## 1.9 One-page mental model of the entire system

```
                 ┌────────────────────────────────────────────────┐
                 │              HOST APPLICATION                  │
                 │   (your UI / CLI / C program — not part of     │
                 │    this repo, but it drives everything)        │
                 └───────┬───────────────────────────┬────────────┘
                         │  sends commands           │  reads telemetry
                         │  (play, seek, EQ, ...)    │  (position, meters, ...)
                         ▼                           ▼
                 ┌──────────────────────────────────────────────────┐
                 │           THE ENGINE (single tick thread)       │
                 │                                                  │
                 │  ┌────────────┐   ┌───────────────────────────┐  │
                 │  │  DECODE    │──▶│   DSP GRAPH (the chain)   │  │
                 │  │  layer     │   │  mix bus → aux → EQ → ... │  │
                 │  │  (formats) │   │  → volume → spatial       │  │
                 │  └────────────┘   └────────────┬──────────────┘  │
                 │                                │                 │
                 │  ┌─────────────┐   ┌───────────▼──────────────┐  │
                 │  │  ANALYSIS   │   │  OUTPUT DOMAIN           │  │
                 │  │  (loudness, │   │  resample → limiter →    │  │
                 │  │  profile,   │   │  dither → ring buffer    │  │
                 │  │  fingerprint│   └───────────┬──────────────┘  │
                 │  └─────────────┘               │  fan-out        │
                 └────────────────────────────────┼─────────────────┘
                                                  ▼
                 ┌──────────────────────────────────────────────────┐
                 │   OUTPUT WORKERS (one per device, real-time)     │
                 │   each: drain its ring → drift-correct → DAC     │
                 └──────────────────────────────────────────────────┘
```

Read it like this: the **host** tells the engine what to do; the **engine** (a
single worker thread, the "tick thread") owns all state — it decodes files,
pushes audio through the DSP graph, publishes status, and hands finished
audio to **output worker threads**, one per physical device, which are the
only parts that talk directly to hardware in real time. Everything the host
does is asynchronous (a message, not a blocking call), so the host can never
freeze the audio.

---

# 2. Complete Capability Map

Every capability below was verified against the source. Status legend:
**Full** = fully implemented and wired in · **Partial** = works but limited
(platform/format/condition, noted) · **Offline** = implemented, runs outside
the real-time audio path · **Seam** = designed for, not yet implemented.

## 2.1 Decoding and input

| Capability | Status | Details |
|---|---|---|
| File sources | Full | Local filesystem paths (`AudioSource::File`). |
| URI sources | Full | `file://` and `http(s)://` identifiers; HTTP(S) streaming via the `network-streaming` feature (Range-request HTTP client, no async runtime). |
| In-memory sources | Full | Raw bytes + an extension hint (`AudioSource::Memory`), used for embedded/bundled audio and tests. |
| Format scanner | Full | Routes files by extension **and** magic bytes; a vectorized scanner (`decode/scanner.rs`). |
| FLAC | Full | Via Symphonia. |
| ALAC | Full | Via Symphonia. |
| WAV / AIFF | Full | Via Symphonia. |
| MP3 | Full | Via Symphonia. |
| AAC (M4A/MP4) | Full | Via Symphonia (isomp4 container). |
| Ogg Vorbis | Full | Via Symphonia. |
| Matroska (MKA/MKV) | Full | Container-only feature (`codec-mkv`); inner codec decoded by the codec features above. |
| Opus (Ogg) | Full | Pure-Rust: `ogg` demuxer + RFC 8251 `opus-decoder`, always 48 kHz. |
| True Audio (TTA) | Full | Native pure-Rust decoder (`decode/tta`). |
| WavPack (`.wv`) | Partial | Pure-Rust `wavicle`: lossless v5, 16/24/32-bit int and 32-bit float, mono/stereo. Multichannel / DSD / hybrid WavPack is **rejected explicitly at open** with a typed error — never silently downmixed. |
| Monkey's Audio (APE) | Full | Pure-Rust `ape-decoder` (`codec-ape` feature). |
| DSD (DSF/DFF) | Full | Native 1-bit decoding (compiled unconditionally; the `codec-dsd` feature is an accepted no-op kept for API compatibility). Reads DSD64–DSD1024 rates (2.82–45.16 MHz); exports native wire packing, DoP packing, or decimation to PCM. |
| Cue sheets | Full | Cue-sheet parsing into chapters (`decode/cue.rs`), surfaced in `TrackMetadata`. |
| Network streaming | Full (feature) | HTTP(S) Range-request streaming (`ureq`); the engine decodes from URLs without downloading the whole file first. |

## 2.2 Metadata

| Capability | Status | Details |
|---|---|---|
| Editorial tags | Full | `TrackTags`: title, artist, album, album artist, genre, date, track/disc numbers, artwork reference — extracted per codec (Symphonia tags; Opus via its own backend). |
| Consolidated track model | Full | `TrackMetadata` aggregates tags + duration + technical format info + loudness tags + optional measured loudness + chapters into one versioned, comparable model (`decode/metadata.rs`). |
| Technical format info | Full | `AudioFormatInfo`: codec, sample rate, bit depth, channels, layout, bitrate — plus "requested vs actual" downgrade reporting (`format_descriptors.rs`). |
| Gapless metadata | Full | `GaplessInfo` (start/end trim samples) for gapless albums. |
| Loudness tags | Full | ReplayGain / EBU R128 values read from tags (`LoudnessMetadata`). |
| Tag write-back | Full (feature `tag-write`) | Writes measured EBU R128 / ReplayGain back into FLAC/MP3/M4A/WAV/AIFF/APE/WavPack tags via `lofty`. Powers the `replaygain-scanner` binary. |
| Loudness scan cache | Full | On-disk cache of scan results (`decode/loudness_cache.rs`). |

## 2.3 Streaming / transport

| Capability | Status | Details |
|---|---|---|
| Playback queue | Full | Ordered playlist with shuffle, repeat modes (Off/All/One), history for Previous; auto-advance at end-of-stream. |
| Gapless transition | Full | Sample-accurate handoff at the logical end of a track — zero silence, zero overlap. |
| Crossfade transition | Full | Overlapping blend; curves: constant-power, linear, exponential, logarithmic, S-curve; duration configurable. |
| Fade / Stop transitions | Full | Fade-out then fade-in next; or stop. |
| Prepare-next (dual decoder) | Full | Second decoder pre-loaded for gapless/crossfade; the transition pair occupies bus slots 0/1. |
| Seek | Full | Sample-accurate, with configurable seek fade; position compensation for DSP latency. |
| Independent lane tracks | Full | Additional streams on mix-bus slots ≥ 2, each with its own decoder + resampler, gain/pan/master-send/aux-send, program-gated ducking, telemetry. |
| Speed/pitch control | Full | Varispeed (speed changes pitch), TimeStretch (WSOLA; pitch constant), PitchShift (tempo constant) — per-track pitch in semitones. |

## 2.4 DSP — equalization

| Capability | Status | Details |
|---|---|---|
| Parametric EQ | Full | Up to 64 bands, RBJ biquads; filter types: peaking, low/high shelf, low/high pass, notch, bandpass, allpass; per-band enable; preamp; bass/treble shelves; mid-side EQ option. |
| EQ presets | Full | Named presets; `SetEqPreset` replaces bands + preamp (the AutoEQ pipeline seam). |
| AutoEQ presets | Full | `autoeq.rs` — preset pipeline applying measured headphone EQ curves. |
| Graphic EQ layer | Full | 10 / 15 / 31 ISO band layouts, slider dB values + preamp, compiled into the parametric EQ. |
| EQ auto-headroom | Full | Reserves the curve's own peak boost as pre-EQ attenuation to avoid clipping; updates as bands change. |

## 2.5 DSP — dynamics, loudness, shaping

| Capability | Status | Details |
|---|---|---|
| 3-band multiband compressor | Full | Per-band threshold/ratio/attack/release/makeup; peak or RMS detector; enable toggle. |
| Lookahead limiter | Full | Final safety limiter: configurable ceiling, attack/release, lookahead (default 5 ms), true-peak FIR oversampling (4×) on/off, Transparent/Saturate modes; runs in the output domain in f32. |
| TPDF dither | Full | Triangular dither at the integer conversion boundary; global toggle + per-device force overrides. |
| Loudness normalization | Full | EBU R128 / ReplayGain modes (track or album), applied as per-slot preamp in the mix bus; measurement per BS.1770-4 (momentary 400 ms / 100 ms hop, short-term 3 s, gated). |
| Stereo enhancer | Full | Mid-side width control + balance. |
| Headphone crossfeed | Full | Bauer, Chu Moy, J. Meier, and custom (frequency/Q/delay); simulates speaker listening on headphones. |
| Convolution engine | Full | FFT partitioned convolution (`realfft`), used for reverb/IRs, aux insert, correction node, and long-kernel graph nodes; real-time safe. |
| Time-stretch / pitch | Full | WSOLA with 3 quality tiers (Low/Balanced/High — concrete window/hop/search parameters, not labels). |
| Channel trim | Full | Per-channel gain, polarity, delay for multichannel output. |
| Bass management / LFE | Full | Sub crossover + gain; mains high-pass with shared crossover; LFE as an effects path. |
| Channel routing matrix | Full | Source→destination channel routing for multichannel. |
| Channel EQ | Full | Per-channel parametric EQ. |
| Room/headphone correction | Full | Phase 7: ESS sweep measurement, IR import, min/linear/hybrid phase rendering, regularized inverse derivation, live `CorrectionNode` in the graph. |

## 2.6 Resampling

| Capability | Status | Details |
|---|---|---|
| Sinc resampler | Full | Rubato-based; quality tiers Fast/Balanced/HighQuality/Ultra with measured stopband/ripple/latency characteristics (≈320–2240-tap effective filters for 44.1↔48 kHz). |
| Precision modes | Full | f32 Performance vs f64 Quality; documented exceptions: WSOLA core stays f32, rubato uses its own internal precision, final limiter stays f32. |
| Rate policy | Full | FollowTrack / device / fixed rates; fallback policies (Nearest, PreferHigher, PreferLower, SameFamilyFirst). |
| Clock drift correction | Full | Per-endpoint rubato `Slip` (1:1 frame insert/drop behind a short crossfade) steered by ring-fill feedback, clamped ±500 ppm, reported in ppm. |

## 2.7 Multichannel processing

| Capability | Status | Details |
|---|---|---|
| Channel layouts | Full | Mono → 7.1.4 (12 ch) and custom layouts up to 16 channels; semantic `ChannelId` roles. |
| Downmix/upmix | Full | ITU-R BS.775 templates and custom matrices (`channel_mix.rs`); upmix modes (`spatial/upmix.rs`). |
| Channel policies | Full | ForceDownmixStereo (default) / PassThrough / MaxChannels(N) / SpatialRender (opt-in). PassThrough is *conditional*: preserved only when source, output, DSP channel limit, and resampler all agree; otherwise documented downmix — never silent channel loss. |
| Channel-state preservation | Partial | Multichannel preservation requires source layout = output layout = DSP support and no active resampler. |

## 2.8 Routing / output / devices

| Capability | Status | Details |
|---|---|---|
| Multi-endpoint routing | Full | Configured `endpoints: Vec<EndpointConfig>` — stable IDs, backend/device, per-endpoint gain, enabled state, drift correction on/off. Each endpoint: independent SPSC ring, nominal-ratio resampler, rate-matched final limiter, own realtime thread. |
| Backends — ALSA | Full | Native exclusive `hw:` / `plughw:` (Linux). |
| Backends — WASAPI | Full (feature `wasapi-native`) | Native exclusive `IAudioClient` stream with OS-level exclusivity verification (Windows). |
| Backends — ASIO | Full (feature `asio-native`) | Pure-Rust Steinberg ASIO via COM (`IASIO`), no C++ SDK, driver control panel, native DSD transport hook (Windows). |
| Backends — CoreAudio | Full | Native hog-mode with direct HAL IO procs + hardware endpoint volume (macOS). |
| Backends — CPAL fallback | Full | Portable shared-mode fallback (all platforms). |
| Bit-perfect verification | Full | Exclusive mode is verified against the OS before claiming the device; `bit_perfect` is true only when exclusive confirmed + DSP bypass + no sample-rate conversion. "Bit-perfect cannot be proven" is reported honestly rather than guessed. |
| Device monitor | Full | Hotplug monitoring (connect/disconnect/list changes) via `OutputEvent`; automatic stream recovery. |
| Output profiles | Full | Per-device profiles (backend preference, dither policy, rate handling) auto-selected by device name, overrideable. |
| Hardware volume | Partial | Native hardware endpoint volume implemented on macOS (CoreAudio HAL); `VolumeMode` HardwarePreferred/HardwareOnly fall back or report `volume_error` where unsupported. |
| System-audio capture | Full (Windows feature) | WASAPI loopback recording of the system mix to float32 WAV. |

## 2.9 DSD / DoP

| Capability | Status | Details |
|---|---|---|
| Native DSD output | Partial | Native wire packing supported on the native ALSA backend (`hw:` node only; `plughw:` rejected because its conversion plugin is not bit-perfect). Not possible on WASAPI (no format), not implemented for ASIO vendor extensions, not yet on CoreAudio. |
| DoP (DSD-over-PCM) | Partial | Works on WASAPI exclusive (I32 container at bit_rate/16) and ASIO; CoreAudio DoP documented as target, not implemented. |
| PCM conversion | Full | Decimation of DSD to PCM (safe default, `DsdOutput::PcmConvert`). |
| Transport negotiation reporting | Full | `DsdTransportReport`: requested vs actual, wire format, bit rate, and the ordered fallback chain (Native → DoP → PCM) — every downgrade is explicit, never silent. |

## 2.10 Spatial audio (opt-in layer — see Section 6 for depth)

| Capability | Status | Details |
|---|---|---|
| Scene model | Full | `SpatialScene`: listener + objects + beds + fields + room in world space (metres, +X right / +Y front / +Z up). |
| Basic panner | Full | Equal-power pair pans, per-path smoothing, LFE as effects path, spread. |
| VBAP renderer | Full | 3-triplet VBAP, 2D coplanar reduction, Delaunay empty-triangle region filter, deterministic out-of-coverage nearest-speaker fallback. |
| Ambisonics | Full | Order-1 (FOA) pinned; order-2 and order-3 implemented (exact SH basis, exact rotation, per-order max-rE weights). Order-4+ future. |
| Binaural renderer | Full | Whole hybrid scene to two ears: Woodworth ITD + Duda-Martens head shadow + pinna elevation notch; virtual 8-speaker ring for diffuse content. |
| Measured HRTFs | Full | `HrtfDataset` (azimuth × elevation grid, bilinear interpolation, 360° wrap), corpus load/save (`from_corpus`), synthetic generator for testing. |
| SOFA import | Partial (feature `sofa-import`) | Native NetCDF-3 classic (.sofa) import; **NetCDF-4/HDF5 is refused with a typed error** (deliberate: robust HDF5 readers link libhdf5, violating the pure-Rust rule). |
| Room model | Full | Axis-aligned box, image-source reflections (order 1 → 6 images, order 2 → 24), Schroeder late field encoded into the ambisonic bus. |
| Acoustic world simulation | Full | Per-wall frequency-dependent materials, portals (openings), diffraction edges; a solver enumerates direct/reflected/diffracted/transmitted paths. |
| Acoustic baking | Full | Position-dependent response cache (`BakedScene`, 0.5 m cells) so static rooms cost nothing at audio time. |
| Head tracking | Full | `HeadTracker`: nlerp + one-pole smoothing + optional rate limit over IMU/VR orientation samples; control-side. |
| Spatial automation | Full | Positional-seconds curves (`CurveScalar`) on object gain/spread, evaluated allocation-free at block rate; spatial clock. |
| Spatial persistence | Full | Active scene auto-saves across sessions (JSON, atomic writes; `spatial_autosave_path` configurable). |
| Spatial health diagnostics | Full | Per-source explainable status (localization quality, direct-vs-reflected ratio, occlusion severity, phase risk) on the telemetry path. |
| Voice budget | Full | Per-scene voice capacity with full-quality sub-capacity, priorities (fixed/distance/gain/user), admission counts in telemetry. |
| Quality tiers | Full | Low/Medium/High/Ultra scale spread samples, room order, HRTF taps, voice budget. |
| SpatialNode in the live graph | Partial | Spatializes the **stereo front pair** through the head model + room; **multichannel masters pass through untouched** (documented seam). |
| Scene files | Full | Serde save/load of renderer-independent scenes (JSON), validated against engine caps. |

## 2.11 Offline systems (Graph 2.0, timeline, aelog, eval, profile)

| Capability | Status | Details |
|---|---|---|
| Graph 2.0 topology | Full (offline) | General-purpose audio graph: typed ports, first-class edges, validation + cycle detection, deterministic topological scheduling, serde round-trip, Graphviz export, offline executor. |
| Timeline & scheduler | Full (offline) | Sample-accurate clock (playhead + master), tempo map, bars/beats/ticks (MIDI 480 PPQ), tempo ramps, loops, transport, quantize, regions, scheduled events. |
| Musical automation | Full (offline) | Beat-authored control curves (`CurveBeats`) evaluated against the tempo map; sample-accurate ramps. |
| Latency analysis & compensation | Full (offline) | Per-node taps (Delay, Convolution, HRTF, Resampler, Acoustic), upstream latency propagation, automatic delay compensation splicing `Delay` nodes onto faster branches while preserving node ids. |
| Aelog recording/replay | Full (offline) | Versioned session logs (every timeline mutation + audio inputs + listener motion + scene swaps) that replay to **byte-identical** audio. |
| Golden-render cache | Full (offline) | Content-addressed (SHA-256) LRU-bounded cache of golden captures keyed by (log, graph, sink) — valid across machines. |
| Quality harness (`eval`) | Full (offline) | 9 DSP/spatial suites with versioned, content-addressed reference vectors; PASS/FAIL reports (text + JSON); cross-version regression detection. |
| AudioProfile analysis | Full (offline) | Deterministic perceptual profile: loudness, dynamics, spectral, transient, stereo, spatial, content sub-profiles with confidence semantics; on-disk cache. |

## 2.12 Analysis / diagnostics / telemetry

| Capability | Status | Details |
|---|---|---|
| Real-time analyzer | Full | Peak / RMS / dominant frequency / FFT spectrum taps published in every telemetry snapshot (lock-free). |
| Telemetry | Full | `PlaybackInfo` via `ArcSwap`: state, positions, volume, latency, CPU%, counters (clips, NaNs, underruns, overloads, deadline misses), lane state, endpoint state, DSD transport report, correction state, spatial telemetry. |
| Typed diagnostics | Full | `DiagnosticKind` (EngineFault / TrackLoad / Decode / Output / BitPerfect / Configuration) + `BitPerfectCause` codes; serializable; FFI surface. |
| Events | Full | Discrete `EngineEvent` (playback lifecycle) + `OutputEvent` (device hotplug, endpoint errors) on separate channels. |
| Loudness scanning | Full | Offline background scans with `LoudnessScanComplete` events; results applied to the active pipeline when the path still matches. |
| Fingerprinting | Full (feature `fingerprint`) | Chromaprint/AcoustID: mono downmix → 16-bit PCM → fingerprint → AcoustID submission data. |

## 2.13 Integration / persistence

| Capability | Status | Details |
|---|---|---|
| Rust API | Full | `AudioEngine` + `EngineHandle` (Clone + Send), `EngineCommand` surface, `SampleSink` pluggable destinations. |
| C FFI | Full (feature `c-ffi`) | Opaque-handle C API (engine_create/destroy + ~50 control/query functions incl. endpoints, aux insert, spatial health, diagnostics); status-code contract; NULL-safe. |
| Sink abstraction | Full | `DacSink` (ring → hardware), `NoopSink` (discard), `VecSink` (capture to memory), custom sinks for hosts. |
| Config model | Full | Serde-serializable `EngineConfig` in the `config` crate with validation (typed issues), presets (Consumer/Fidelity), and a versioned envelope + migration framework (`VersionedConfig`). |
| Scene/config persistence | Full | JSON save/load for spatial scenes; auto-save of the active scene; versioned config files. |
| Spatial debug view | Full | `SpatialDebugView` — per-object/per-speaker debug info for visualization. |

## 2.14 Capability matrix (condensed)

| Capability | Status | Purpose | Major Dependencies | Important Limitations |
|---|---|---|---|---|
| 15+ lossless/lossy codecs | Full | Play any source | Symphonia; pure-Rust native decoders | WavPack multichannel/DSD/hybrid rejected (typed error) |
| DSD DSF/DFF | Full | 1-bit playback | Native module | Native wire only on ALSA hw:; DoP on WASAPI/ASIO; CoreAudio DoP future |
| Network streaming | Full | Stream URLs | ureq (feature) | HTTP(S) only, no auth/session layers |
| Gapless/crossfade | Full | Seamless albums | Dual decoder + mix-bus envelopes | — |
| Lane tracks | Full | Simultaneous streams | Mix bus slots ≥ 2 | Max slots 8 |
| Parametric EQ 64-band | Full | Tone shaping | RBJ biquads | — |
| Graphic EQ | Full | Tone shaping | compiled into EQ | 10/15/31 ISO layouts |
| Multiband compressor | Full | Dynamics | 3 bands | — |
| True-peak limiter | Full | Safety ceiling | 4× FIR oversampler | f32 in both precision modes |
| Convolution | Full | Reverb/correction/IRs | realfft partitioned FFT | IR reload needed on rate change |
| Crossfeed | Full | Headphone imaging | 4 profiles | — |
| Time-stretch/pitch | Full | Varispeed/WSOLA | WSOLA core | Core is f32 in both modes; latency grows with quality |
| Loudness (R128/RG) | Full | Consistent level | BS.1770-4 meter | — |
| Tag write-back | Full | Persist scans | lofty (feature) | Feature-gated |
| Fingerprinting | Full | Identify tracks | chromaprint (feature) | Feature-gated |
| Resampler | Full | Rate conversion | Rubato | Quality tier costs CPU/latency |
| Drift correction | Full | Multi-device sync | rubato Slip | ±500 ppm clamp |
| Exclusive-mode backends | Full | Bit-perfect | OS-native APIs | Windows/macOS features opt-in |
| Multi-endpoint | Full | Several devices | endpoint workers | Stuck endpoint drops oldest frames |
| Spatial scene+renderers | Full | 3D placement | internal math | Opt-in; SpatialNode = stereo front pair only |
| Ambisonics ≤ order 3 | Full | Speaker-independent field | exact SH basis | Order-4+ and spatial recording future |
| Binaural + HRTF | Full | Headphone spatial | head model + datasets | Elevation from notch/dataset; SOFA nc4 deferred |
| Acoustic world + bake | Full | Room simulation | image-source solver | Reflection order ≤ 2 live; heavy solve is baked |
| Head tracking | Full | VR/AR seam | nlerp + one-pole | Control-side; no bundled IMU driver |
| Graph 2.0 + timeline + aelog | Full (offline) | Deterministic render lab | internal | Offline-only; realtime graph untouched |
| Quality harness | Full (offline) | Regression detection | aelog SHA-256 | 9 suites today |
| AudioProfile | Full (offline) | Content intelligence | deterministic DSP | Heuristics, not ML |
| System capture | Full (Windows) | Record system mix | WASAPI loopback | Windows only |
| C FFI | Full | Any-language hosts | feature `c-ffi` | Feature-gated |

---

# 3. The Complete Audio Journey

## 3.1 The conceptual pipeline (what every audio engine does)

```
Input ─▶ Decoder ─▶ PCM ─▶ Analysis ─▶ DSP graph ─▶ Spatial ─▶ Acoustic
  (file)   (bits →   (raw     (meters,    (EQ, mix,   (objects   (room
            samples)  levels)   compressor)  in 3D)    reflections)
                                                                    │
        ┌───────────────────────────────────────────────────────────┘
        ▼
   Master/output processing ─▶ Resampling ─▶ Device backend ─▶ Output
   (limiter, dither)          (rate match)   (exclusive/shared)   (DAC)
```

## 3.2 The actual implementation

Here is the same journey as the code actually performs it, in two levels:

### Level 1 — decode & analysis (the tick thread, block by block)

```
file / URI / memory / URL
        │
        ▼
Format scanner (extension + magic bytes) ──▶ codec routing
        │
        ▼
Decoder::open ──▶ per-codec decoder (Symphonia / DSD / Opus / TTA / WavPack / APE)
        │  produces blocks of interleaved f32 samples at the file's native
        │  rate and native channel layout (up to 7.1.4 / 16 ch)
        ▼
Channel stage (multichannel path only): per-channel trim, routing,
        bass management, LFE crossover
        │
        ▼
Mix bus: each bus input runs its own pre-mix (loudness normalizer +
        preamp + user gain/pan/mute + automation) then all inputs are
        summed into the master planes
        │
        ▼
DSP graph plan (compiled once, executed every block):
        aux → correction → EQ → compressor → convolution → balance →
        crossfeed → stereo → timestretch → volume → seek fade → spatial
        │
        ▼  (analyzer taps live levels/spectrum off the block as it flows)
Output domain (at the output sample rate):
        resampler → final safety limiter → TPDF dither
        │
        ▼
Master ring buffer + one ring per configured endpoint (fan-out)
```

### Level 2 — delivery (per-endpoint realtime worker threads)

```
primary ring ──▶ primary backend (ALSA / WASAPI / ASIO / CoreAudio / cpal)
                    │  its own realtime thread/callback drains the ring
                    ▼
                DAC #1

endpoint ring #1 ──▶ endpoint worker: nominal-ratio resampler →
                        Slip drift trim (per-device clock) →
                        rate-matched limiter → gain → backend thread
                        ▼
                     DAC #2 (different device, different clock)
```

The key architectural fact: **the engine's tick thread does all the heavy
work, and each output device has its own realtime thread that only reads its
own ring buffer.** A slow device can never stall the others, and the tick
thread can never be blocked by hardware.

## 3.3 What runs where

| Stage | Thread | Real-time? | Notes |
|---|---|---|---|
| Format probing, decoding | Tick thread (decode loop) | Soft (must keep up, but can pre-buffer) | SPSC rings decouple decode from playback; resampler consumes at rate |
| DSP graph execution | Tick thread | Soft | Must finish each block before the ring starves; zero allocation by design |
| Analyzer taps | Tick thread | Soft | Lock-free accumulation into telemetry |
| Final limiter / dither | Tick thread (output domain) | Soft | Downstream of the graph |
| Ring drain + device write | Per-endpoint worker | **Hard** | The OS callback; must never block or allocate |
| Drift correction | Per-endpoint worker | **Hard** | Reads ring-fill, nudges slip ratio between blocks |
| Device monitor / hotplug | Background | Async | Spawns `AutoRecoverStream` |
| Loudness scan | Background worker | Async | Result applied only if the path still matches |
| Correction IR derivation | Tick thread control path | Async/offline | Sweep → deconvolution → inversion (heap-happy, fine) |
| Acoustic solving / baking | Control/offline | Async/offline | Baked result consumed by realtime renderers |
| Graph 2.0, timeline, aelog | Offline (host-driven) | Offline | Deterministic render lab; never touches live audio |
| Profile analysis | Background / offline | Async | Bounded-memory streaming pass |
| WASAPI loopback capture | Loopback thread fills ring; tick drains to WAV | Async | Disk I/O never in a realtime callback |

## 3.4 Which stages are optional / bypassable

- **Every DSP stage** can be selectively disabled; disabled stages are bit-exact
  (literally absent from the executed expression).
- **Bit-perfect mode** bypasses the entire graph — only volume ramps and seek
  fades survive (and even those are the *only* modifications allowed).
- **DoP bypass** is a pure passthrough: 24-bit DSD-over-PCM words must reach
  the DAC unmodified, so not even volume is applied.
- **Spatial processing** is opt-in at three levels: the whole scene layer, the
  `ChannelPolicy::SpatialRender` flag, and the `SpatialNode` plan step.
- **Room/headphone correction** is a plan step that is skipped when disabled
  or IR-less.
- **Loudness normalization** can be Off / Track ReplayGain / Album ReplayGain /
  EBU R128.
- **Dither** is configurable, and per-device profiles can force it on/off.

## 3.5 Where latency comes from

Every stage that must *look ahead* or *filter over time* adds latency. The
engine reports total end-to-end latency in `PlaybackInfo::latency_ms`, and
`position_secs_compensated` subtracts it so the displayed position matches
what you actually hear at the DAC.

| Stage | Latency source | Typical magnitude | Tunable? |
|---|---|---|---|
| Limiter lookahead | Delay line before gain reduction | Default 5 ms | Yes (`limiter.lookahead_ms`) |
| Convolution / correction IRs | Partition size + IR length | ms–tens of ms | By IR choice |
| Crossfeed | Inter-channel delay | ~0.1–1 ms | Profile/custom params |
| Time-stretch (WSOLA) | Analysis/synthesis window | 512–2048 samples by tier | Quality tier |
| Resampler | Anti-aliasing filter length | ~3–23 ms by tier | Quality tier |
| Ring buffer fill | Decode-ahead buffering | Bounded by design | — |
| Device buffer | Hardware/driver buffering | Driver-dependent | Backend/driver |
| Drift correction | None (1-frame inserts/drops) | 0 | — |

## 3.6 Determinism — what is reproducible

- **The DSP graph is deterministic**: the same inputs through the same plan
  produce bit-identical output (pinned by the graph-vs-pipeline equivalence
  suite and golden vectors).
- **Offline Graph 2.0 rendering is deterministic end-to-end**: the aelog
  session log (commands + audio inputs + listener motion + scene swaps) is a
  pure function of its contents; replaying it reproduces byte-identical
  captured audio, cacheable by content address.
- **Analysis is deterministic**: loudness, profile, and eval suites use fixed
  deterministic DSP with no wall-clock dependence (eval reports stamp a
  generation time *outside* the identity hash).
- **Not deterministic / adaptive by design**: per-endpoint clock-drift
  correction adapts to the physical device's crystal — the number of frames
  inserted/dropped depends on real hardware, so live multi-device output is
  not bit-reproducible (it is still glitch-free and drift-free).
- **Wall-clock-free logs**: aelog deliberately records no timestamps, so a log
  is a pure function of its commands.

## 3.7 A concrete example: one stereo FLAC track

1. Host calls `open_file("song.flac")`; the command rides a bounded channel to
   the tick thread.
2. The format scanner probes magic bytes → Symphonia FLAC decoder opens; the
   file's native rate (e.g. 44.1 kHz) and 2 channels are reported
   (`SourceOpened` event).
3. The tick loop decodes 44.1 kHz stereo blocks, feeding the mix bus slot 0.
4. Each block runs the compiled plan: mix (×1.0) → aux (disabled → skipped)
   → correction (disabled → skipped) → EQ (if enabled) → … → volume (e.g.
   −6 dB ramp) → seek fade (none) → spatial (disabled → skipped).
5. The output domain resamples 44.1 kHz → the DAC's 48 kHz (if the policy and
   device require), passes the final limiter (ceiling −1 dBTP, true-peak),
   dithers to the device bit depth, and pushes frames into the ring.
6. The output worker's realtime callback drains the ring and writes to the
   DAC. If `bit_perfect` mode were on, steps 4–5 would instead be pure
   passthrough with only the volume/fade ramps, and no resampling.
7. Every tick, telemetry (`PlaybackInfo`) is published: position compensated
   for latency, volume, analyzer levels, clip/underrun counters.
8. At end-of-stream the engine auto-advances the playlist, using the prepared
   next decoder for a gapless handoff (sample-aligned, zero silence).

---

# 4. Architecture for a Non-Developer

This is the repository drawn as a map of responsibilities. Each entry says
*why it exists*, *what it owns*, and *what flows through it*.

```
Repository (Cargo workspace: two crates, one project)
│
├── crates/config/        THE CONFIGURATION CRATE
│   └── EngineConfig + presets + validation + versioned persistence
│       (one Serde-serializable description of every knob)
│
└── src/  (the `engine` crate — the whole engine)
    │
    ├── lib.rs            Front door: what the world can see (public API)
    ├── commands.rs       The control protocol (EngineCommand: play, seek, EQ…)
    ├── events.rs         The notification protocol (EngineEvent / OutputEvent)
    ├── playback_info.rs  The status dashboard (PlaybackInfo telemetry)
    ├── source.rs         What audio is (File / Uri / Memory)
    ├── playlist.rs       The queue (shuffle, repeat, history)
    ├── sink.rs           Where processed audio goes (DAC / discard / capture)
    ├── audio_io.rs       File & URL byte access
    ├── ffi.rs            The C-language door (feature `c-ffi`)
    ├── paths.rs          Where app data lives on disk
    ├── diagnostics.rs    Typed error categories (BitPerfectCause, DiagnosticKind)
    │
    ├── engine/           THE BRAIN — the core state machine
    │   │                 one tick thread owns ALL mutable state
    │   ├── tick.rs        the main loop: commands → decode → DSP → rings
    │   ├── handle.rs      EngineHandle: the thread-safe remote control
    │   ├── stream.rs      PlaybackStream: the dual-decoder state machine
    │   ├── track_loading.rs  open/swap decoders; gapless & crossfade handoff
    │   ├── crossfade.rs   transition decision logic
    │   ├── clock.rs       AudioClock: the sample-accurate playhead
    │   ├── recovery.rs    device loss / exclusive-mode failure recovery
    │   ├── lanes.rs       the extra simultaneous tracks
    │   ├── output_setup.rs  pick backend + device
    │   ├── volume.rs      software vs hardware volume modes
    │   ├── dsd_state.rs   DSD transport negotiation (native / DoP / PCM)
    │   ├── loudness_state.rs  background loudness scans
    │   ├── spatial_persistence.rs  auto-save/restore the active spatial scene
    │   ├── decode_loop/   the block-by-block decode-and-process loop
    │   └── commands/      command handlers split by concern (playback, dsp,
    │                      eq, lanes, output, playlist, capture, multichannel)
    │
    ├── decode/           THE READERS — turning bytes into samples
    │   ├── scanner.rs     format sniffing
    │   ├── decoder.rs     the Decoder facade
    │   ├── codecs.rs      codec registry & capability records
    │   ├── symphonia_decoder/  Symphonia-based codecs (FLAC, WAV, MP3, AAC…)
    │   ├── dsd/           native DSD (DSF/DFF) + wire packing + decimation
    │   ├── opus.rs / tta/ / wavpack.rs / ape.rs   native pure-Rust codecs
    │   ├── channel_layout.rs / channel_mix.rs      layouts & down/upmix
    │   ├── tags.rs        loudness tag write-back (feature)
    │   ├── fingerprint.rs Chromaprint/AcoustID (feature)
    │   ├── metadata.rs    the consolidated TrackMetadata model
    │   └── cue.rs         cue sheets
    │
    ├── dsp/              THE PROCESSORS — turning samples into better samples
    │   ├── graph/         the PRODUCTION signal chain (node arena + compiled
    │   │                  plans + live generation swap + per-node control
    │   │                  queues; nodes/: mix, aux, correction, eq, dynamics,
    │   │                  convolution, crossfeed, stereo, timestretch, volume,
    │   │                  spatial, routing, limiter, dither…)
    │   ├── pipeline/      the REFERENCE chain (bit-exact oracle for tests)
    │   ├── equalizer/ biquad.rs graphic_eq.rs autoeq.rs   EQ family
    │   ├── limiter.rs true_peak.rs dither.rs              output safety
    │   ├── convolution.rs partitioned-FFT engine
    │   ├── multiband_compressor.rs crossfeed.rs stereo.rs timestretch.rs gain.rs
    │   ├── loudness/      EBU R128 meter + normalizer
    │   ├── resampler/     Rubato sinc resampler
    │   ├── analyzer.rs    realtime peak/RMS/spectrum
    │   ├── correction/    room/headphone correction pipeline (sweep, IR,
    │   │                  phase, derive)
    │   ├── graph2/        Graph 2.0 — general-purpose topology (offline)
    │   ├── timeline/      AudioClock + tempo map + events + automation (offline)
    │   └── aelog/         deterministic session recording/replay + cache (offline)
    │
    ├── spatial/          THE 3D AUDIO LAYER (opt-in)
    │   ├── math.rs        Vec3/Quat + one documented coordinate system
    │   ├── scene.rs object.rs speaker.rs level.rs    world model
    │   ├── panner.rs vbap.rs ambisonic.rs binaural.rs hrtf.rs   renderers
    │   ├── bed.rs field.rs room.rs acoustic/         content + room + bake
    │   ├── directivity.rs occlusion.rs spread.rs doppler.rs   object behavior
    │   ├── tracking.rs    head tracking
    │   ├── automation.rs health.rs metering.rs voice.rs quality.rs
    │   └── sofa.rs        NetCDF-3 SOFA import (feature)
    │
    ├── output/           THE HARDWARE DOOR
    │   ├── endpoint.rs    per-device worker: ring + resampler + drift slip
    │   ├── alsa_output/ wasapi_output/ asio_output/ coreaudio_output/ cpal_output/
    │   ├── output_profile.rs device_monitor.rs rate_policy.rs capabilities.rs
    │   └── wav_writer.rs wasapi_loopback.rs    capture
    │
    ├── buffer/           THE PIPES — lock-free SPSC rings, frame buffers, DSD bytes
    │
    ├── eval/             the objective quality harness (offline)
    ├── profile/          the deterministic AudioProfile analyzer (offline)
    │
    └── bin/              reference programs
        ├── audio_engine_cli.rs   interactive REPL player
        ├── replaygain_scanner.rs loudness scan + tag write
        └── aelog_replay.rs       deterministic replay tool

benches/   Criterion benchmarks (dsp, pipeline, graph plan, spatial)
tests/     headless integration tests + 55 fidelity suites
```

## 4.1 The two crates

- **`engine`** (workspace root) — the whole engine: `src/` plus tests/benches.
- **`config`** (`crates/config/`) — a small companion crate holding every
  configuration *type*: `EngineConfig` and its ~30 sub-configs (EQ, limiter,
  aux, spatial, endpoints…), validation, presets, and the versioned envelope.
  It has almost no dependencies (only serde + serde_json), so hosts can parse
  config files without linking the whole engine.

The two crates **must stay at the same version** (a release rule in
`AGENTS.md`), because the engine's public API exposes config types directly.

## 4.2 Dependencies (who relies on whom)

```
config  (tiny, dependency-free-ish)
   ▲
engine — everything depends on config's types
   ├── decode uses buffer, source, dsp (analyzer/loudness)
   ├── engine core uses decode, dsp::graph, output, buffer, playlist, sink
   ├── output uses buffer (rings)
   ├── spatial uses buffer, dsp::convolution (nothing from output)
   ├── dsp::graph2/timeline/aelog use spatial::math (Vec3 serde) and each other
   ├── eval uses dsp::aelog (SHA-256 substrate) + the DSP under test
   └── profile uses dsp::loudness (shared meter) + decode
```

External crates of note: Symphonia (most codecs), Rubato (resampling),
`realfft`/`rustfft` (FFT), `crossbeam` (channels), `arc-swap` (lock-free
telemetry), `serde`/`serde_json` (persistence), plus optional ones behind
features (ureq, lofty, chromaprint-next, wavicle, ape-decoder, ogg,
opus-decoder, windows/objc2 bindings for native backends). Everything is
Rust; the native OS backends use the OS's own audio APIs via safe bindings.

---

# 5. Subsystem Encyclopedia

Each major subsystem gets the same seven-part treatment. Spatial subsystems
are summarized here and detailed in Section 6.

## 5.1 Source & I/O abstraction

**Purpose.** Decouple the engine from host-specific identifiers and storage.
**Mental model.** A universal "where is the audio?" answer — file path, URL,
or a blob of bytes in memory.
**Inputs.** Paths/URIs/byte buffers from the host. **Processing.** Access to
bytes: memory-mapped/async file reads, HTTP Range requests (feature).
**Outputs.** `AudioSource` values + a byte-reader abstraction (`AudioByteSource`)
that decoders consume. **Depends on.** nothing core. **Used by.** every open/
queue/prepare command. **Runtime.** control/worker threads. **Performance.**
memory-mapped files mean decoders never copy the whole file. **User effect.**
You can feed the engine anything; formats stay identical. **Limitations.**
HTTP has no auth/session layer. **Location.** `src/source.rs`, `src/audio_io.rs`.

## 5.2 Format scanner & codec registry

**Purpose.** Identify what a file is and which decoder owns it. **Mental
model.** A receptionist reading the file's name *and* its first bytes, then
directing it to the right specialist. **Inputs.** Byte stream. **Processing.**
Extension + magic-byte probing, capability lookups. **Outputs.** A `Codec`
record (decoder type, channels, rate, DSD/DoP capability). **Depends on.**
codec modules. **Used by.** `Decoder::open`, metadata extraction. **Runtime.**
open-time. **Limitations.** Probing is heuristic; magic bytes are
authoritative over extensions. **Location.** `src/decode/scanner.rs`,
`src/decode/codecs.rs`.

## 5.3 Decoders

**Purpose.** Convert compressed/encoded bytes into raw PCM samples. **Mental
model.** Translators from many languages into one common tongue (f32
samples). **Inputs.** Encoded bytes. **Processing.** Symphonia decoders
(FLAC/ALAC/WAV/AIFF/MP3/AAC/Vorbis/PCM/Ogg/MP4/MKA), native pure-Rust
decoders (DSD DSF/DFF, Ogg Opus, TTA, WavPack, APE). **Outputs.**
`DecodedChunk` blocks of interleaved f32 at native rate/layout. **Depends
on.** buffer, source. **Used by.** decode loop, scanners, tests. **Runtime.**
tick thread (decode loop) and offline (metadata/loudness). **Performance.**
Per-codec; decode is off the hard-realtime path (rings decouple).
**Limitations.** WavPack multichannel/DSD/hybrid rejected explicitly; DSD
rates DSD64–DSD1024 readable, native wire depends on backend. **Location.**
`src/decode/` (per-codec files), `decode/dsd/`.

## 5.4 Channel layout & mixing

**Purpose.** Map any source's channel count onto any output's channel count
without losing (or inventing) channels silently. **Mental model.** A seating
chart: the source has seats labeled by *role* (Left, Right, Center, LFE…);
the output has its own seats; the chart maps roles to seats or downmixes
them by the standard BS.775 recipe. **Inputs.** Source layout + output
layout. **Processing.** ITU-R BS.775 templates, custom matrices, upmix
modes. **Outputs.** Channel-mapped planes. **Depends on.** decode. **Used
by.** decode loop, output. **Limitations.** `ChannelPolicy::PassThrough` is
conditional (see 2.7). **Location.** `src/decode/channel_layout.rs`,
`channel_mix.rs`, `src/spatial/upmix.rs`.

## 5.5 PlaybackStream & the decode loop

**Purpose.** Own the "now playing" state: current decoder, prepared-next
decoder, position, and the block-by-block fill of the mix bus. **Mental
model.** The conductor: reads the score (decoder), hands each bar to the
orchestra (mix bus/DSP), and knows exactly where they are. **Inputs.**
Commands, decoded blocks. **Processing.** Dual-decoder handoff (gapless /
crossfade / fade / stop), seek with fades, lane feeding, analyzer taps,
endpoint fan-out. **Outputs.** Processed blocks into rings. **Depends on.**
decode, dsp::graph, output, playlist. **Used by.** tick. **Runtime.** tick
thread. **Performance.** The single most CPU-heavy loop; must stay within
real-time budget per block. **User effect.** The position you see, the
seamlessness of track changes. **Limitations.** — **Location.**
`src/engine/stream.rs`, `src/engine/decode_loop/`, `src/engine/track_loading.rs`.

## 5.6 AudioClock

**Purpose.** The single sample-accurate source of truth for the playhead.
**Mental model.** A digital odometer counting samples since playback began.
**Inputs.** Seek/speed changes, decoded frames. **Processing.** Integer
sample counting, speed scaling, position reporting. **Outputs.**
`position_secs` (+ compensated), duration. **Used by.** telemetry, decode
loop, crossfade logic. **Runtime.** tick thread. **Limitations.** Distinct
from the *offline* `dsp::timeline::AudioClock` (which adds tempo/beats/loops
for the render lab). **Location.** `src/engine/clock.rs`.

## 5.7 Mix bus (MixBusNode)

**Purpose.** Sum N simultaneous inputs into one master mix, each with its own
musical behavior. **Mental model.** A mixing console: each channel strip
(slot) has preamp/loudness, gain, pan, mute, automation, and sends; the
master sums them. **Inputs.** N planes (primary, crossfade partner, lanes).
**Processing.** Per-slot pre-mix (loudness normalizer + preamp + gain/pan /
mute + automation tracks + ducking), per-slot trim, per-slot post-fader
sends (master + aux), N-channel multichannel sum, per-slot meters.
**Outputs.** Summed master planes. **Depends on.** graph infra, loudness,
gain. **Used by.** decode loop (every block). **Runtime.** tick thread, zero
alloc. **Performance.** O(slots × frames); disabled slots skip entirely.
**User effect.** Track levels, panning, ducking, automation, sends.
**Limitations.** Slot count 2–8 (default 2; lanes need ≥ 2). **Location.**
`src/dsp/graph/nodes/mix/` (mod/envelope/sum).

## 5.8 Aux bus (AuxBusNode) + insert

**Purpose.** A second bus that accumulates per-slot "effects sends" and
returns into the master — the standard place for a shared reverb/effect.
**Mental model.** The "FX return" fader on a console, with its own
insert point where a processor sits in the send path. **Inputs.** Per-slot
post-fader aux taps. **Processing.** Accumulate sends (per-send ramped,
automatable), optional convolution insert (reverb IR), return into master
before the post-mix chain; per-send meters; duckable. **Outputs.** Aux
planes returned into the master. **Depends on.** mix bus, convolution.
**Used by.** decode loop. **Runtime.** tick thread. **User effect.**
Headphone reverb buses, shared room effects, sends-only mixing.
**Limitations.** Insert = one convolution; disabled = bit-exact. **Location.**
`src/dsp/graph/nodes/aux_node.rs`, `nodes/mix/sends.rs`.

## 5.9 Equalization family

**Purpose.** Tone shaping. **Mental model.** Precise tone knobs with exact
frequencies. **Inputs.** Master planes. **Processing.** 64-band parametric
(RBJ biquads: peaking/shelves/pass/notch/allpass), graphic EQ layer compiled
into it, AutoEQ presets, shelves, mid-side option, auto-headroom.
**Outputs.** Shaped planes. **Depends on.** dsp core. **Used by.** graph
plan (EQ step). **Runtime.** tick thread. **Performance.** Cheap per band;
64 bands is the max. **User effect.** Everything tonal. **Limitations.**
Graphic EQ is a layer over parametric, not a separate filter bank.
**Location.** `src/dsp/equalizer/`, `graphic_eq.rs`, `autoeq.rs`,
`nodes/eq_node.rs`.

## 5.10 Dynamics (multiband compressor)

**Purpose.** Level control: tame loud parts, glue mixes. **Mental model.**
An automatic volume rider working in three frequency bands independently.
**Inputs.** Master planes. **Processing.** 3-band split, per-band
threshold/ratio/attack/release/makeup, peak or RMS detector. **Outputs.**
Compressed planes. **Depends on.** dsp core. **Used by.** graph plan
(DYNAMICS step). **Runtime.** tick thread. **User effect.** Punch, glue,
consistency. **Limitations.** Fixed 3 bands. **Location.**
`src/dsp/multiband_compressor.rs`, `nodes/dynamics_node.rs`.

## 5.11 Convolution engine

**Purpose.** Apply any impulse response (reverb, cab IR, correction filters)
by filtering. **Mental model.** "Play the sound through a model of a room /
box / filter." **Inputs.** Audio + an IR (kernel). **Processing.** FFT
partitioned overlap-add convolution (realfft); kernels ≥ 512 taps route
through the partitioned engine, shorter keep an exact direct path.
**Outputs.** Filtered audio. **Depends on.** realfft. **Used by.** aux
insert, correction node, graph Convolution node, graph2 offline rendering.
**Runtime.** tick thread (realtime) + offline. **Performance.**
O(P·2B·log2B) per partition — fast on long IRs; memory scales with kernel
length. **User effect.** Reverb, cab simulation, impulse-based processing.
**Limitations.** IRs must be reloaded if the session rate changes
(`convolution_ir_needs_reload`). **Location.** `src/dsp/convolution.rs`,
`nodes/convolution_node.rs`.

## 5.12 Timestretch / pitch (WSOLA)

**Purpose.** Change speed without changing pitch (or pitch without speed).
**Mental model.** A tape machine with a smarter algorithm that keeps voices
natural. **Inputs.** Planes + speed/pitch. **Processing.** WSOLA: analysis
window, similarity search, overlap-add synthesis; 3 tiers (window 512 /
1024 / 2048). **Outputs.** Time/pitch-modified planes. **Depends on.** dsp
core. **Used by.** graph plan (TIMESTRETCH step); varispeed is resampling-
based instead. **Runtime.** tick thread. **Performance.** Search cost
scales with tier; algorithmic latency scales with window. **User effect.**
Podcast speed, DJ varispeed, vocal pitch. **Limitations.** Core runs f32 in
both precision modes (documented trade-off). **Location.**
`src/dsp/timestretch.rs`, `nodes/timestretch_node.rs`.

---

## 5.13 Volume & fades

**Purpose.** Perceptual level control without zipper noise. **Mental model.**
A motorized fader that glides instead of jumping. **Inputs.** dB/linear
requests, hardware volume support. **Processing.** dB→linear conversion
(-60…0 dB), ramped gain over `volume_fade_ms`, seek fades, volume-path
selection (software DSP vs hardware endpoint; four `VolumeMode`s).
**Outputs.** Ramped planes. **Used by.** graph plan (VOLUME, SEEK_FADE
steps). **Runtime.** tick thread. **User effect.** Smooth level changes;
`volume_path`/`volume_error` report whether hardware actually owns the
level. **Limitations.** Hardware volume only on macOS among native
backends. **Location.** `src/engine/volume.rs`, `src/dsp/gain.rs`,
`nodes/gain_node.rs`.

## 5.14 Room & headphone correction (Phase 7)

**Purpose.** Correct for what a room or headphones do to sound — measured,
not guessed. **Mental model.** Play a test sweep, record what comes back,
compute the exact opposite filter, apply it. **Inputs.** A captured sweep
(WASAPI loopback on Windows) or an imported IR file (REW/Dirac exports,
all platforms). **Processing.** S1 ESS sweep + deconvolution (with harmonic
separation) → S2 IR conditioning → S3 phase rendering (minimum / linear /
hybrid) → S4 smoothed regularized inverse vs target curve (boost clamped,
SNR-weighted) → S5 live `CorrectionNode` (per-channel partitioned
convolution, post-aux / pre-EQ). **Outputs.** Correction IRs running in the
graph; telemetry (phase mode, IR length, latency, max gain). **Depends on.**
convolution, capture, dsp/fft. **Used by.** graph plan (CORRECTION step).
**Runtime.** measurement/derivation = control path (heap-happy); execution =
tick thread (zero alloc). **Performance.** IR length determines cost;
partitioned FFT keeps it bounded. **User effect.** Flat response in your
room or on your headphones. **Limitations.** Integrated live measurement
needs a capture backend — WASAPI loopback today (Windows); other platforms
import IRs. **Location.** `src/dsp/correction/` (sweep/ir/phase/derive),
`nodes/correction_node.rs`, `engine/commands/` (MeasureRoom).

## 5.15 Output domain (resampler → limiter → dither)

**Purpose.** Deliver the final signal at the exact rate, bounded below
clipping, and quantized cleanly for the device. **Mental model.** The
mastering suite before the pressing plant. **Inputs.** Post-graph planes.
**Processing.** Rubato sinc resample to output rate (or passthrough /
bit-perfect/DoP bypass), 4× true-peak lookahead limiter (default 5 ms
lookahead, ceiling, Transparent/Saturate), TPDF dither at the integer
boundary. **Outputs.** f32 frames into rings. **Depends on.** resampler,
limiter, dither. **Used by.** decode loop, endpoint workers (each endpoint
has its own rate-matched limiter for resampled frames). **Runtime.** tick
thread + per-endpoint workers. **Performance.** Resampler quality tier is
the big knob (see 12). **User effect.** No clipping, no distortion,
correct rate, clean low-level detail. **Limitations.** Limiter and dither
are f32 in both precision modes (deliberate). **Location.**
`src/dsp/graph/limiter.rs`, `resampler/`, `true_peak.rs`, `dither.rs`,
`nodes/resampler_node.rs`/`limiter_node.rs`/`dither_node.rs`.

## 5.16 Output backends & endpoint workers

**Purpose.** Talk to physical hardware on every OS, in shared or exclusive
mode, with per-device clock drift correction. **Mental model.** Each device
gets a dedicated courier who only ever delivers to that device, watches the
device's clock, and adjusts the pace slightly so the stream never runs out
or overflows. **Inputs.** Rings of processed frames. **Processing.** Backend
factory + exclusivity verification (ALSA `hw:`, WASAPI exclusive
`IAudioClient`, ASIO `IASIO` via COM, CoreAudio hog mode, cpal fallback),
per-endpoint nominal resampler + rubato `Slip` drift trim + rate-matched
limiter + gain. **Outputs.** Samples to devices. **Depends on.** buffer,
OS APIs (feature-gated). **Used by.** the whole engine at output.
**Runtime.** per-endpoint realtime threads (hard real-time).
**Performance.** Drain-rate = device clock; worker does minimal work per
callback. **User effect.** Glitch-free multi-device playback; ppm drift
reported per endpoint. **Limitations.** Exclusive modes are feature-gated;
bit-perfect is only *claimed* when verified. **Location.** `src/output/`
(alsa_output/, wasapi_output/, asio_output/, coreaudio_output/,
cpal_output/, endpoint.rs).

## 5.17 Device monitor, output profiles, rate policy

**Purpose.** Adapt to the physical world: devices come and go; devices have
personalities. **Mental model.** A facility manager watching the building.
**Inputs.** OS device events. **Processing.** Hotplug monitoring
(OutputEvent: connected/disconnected/list-changed), auto stream recovery,
per-device profile auto-selection (backend preference, dither policy, rate
handling), rate-policy helpers (follow track / device / fixed;
nearest/higher/lower/same-family fallbacks). **Outputs.** Events +
reconfiguration commands. **Used by.** engine lifecycle. **Runtime.**
background thread. **User effect.** Plugging in a USB DAC just works;
profiles remember how each device likes to be driven. **Limitations.**
Hotplug support quality depends on OS APIs. **Location.**
`src/output/device_monitor.rs`, `output_profile.rs`, `rate_policy.rs`,
`src/engine/recovery.rs`.

## 5.18 Real-time analyzer

**Purpose.** Tell the host (and users) what is happening level- and
spectrum-wise right now, without disturbing audio. **Mental model.** A
clip-on tuner/level meter. **Inputs.** Blocks as they flow. **Processing.**
Peak, RMS, dominant frequency, FFT spectrum taps (lock-free). **Outputs.**
Levels in every `PlaybackInfo` snapshot + per-slot meters. **Used by.**
hosts, UI, ducking trigger (per-slot peak), spatial meters. **Runtime.**
tick thread; accumulation is allocation-free. **User effect.** Level
meters, visualizers, ducking behavior. **Limitations.** Snapshot cadence =
tick rate. **Location.** `src/dsp/analyzer.rs`.

## 5.19 Loudness (EBU R128 / ReplayGain)

**Purpose.** Measure perceived loudness the way broadcast standards define
it, and normalize playback / write tags. **Mental model.** A calibrated
ear that averages loudness over time with a gating curve. **Inputs.**
PCM. **Processing.** BS.1770-4: 400 ms momentary blocks on 100 ms hops,
3 s short-term window, gating → LUFS, dBTP (true peak), LRA, ReplayGain
gain/peak. **Outputs.** `LoudnessScanResult`; applied normalization;
tags written; events. **Depends on.** true_peak, dither-era math. **Used
by.** mix bus (per-slot loudness), scanner binary, profile layer (shared
meter). **Runtime.** background/offline scan; normalization runs in the
mix bus. **User effect.** Consistent level across tracks/albums.
**Limitations.** Album vs track gating is a choice; scans run once per
file (cached). **Location.** `src/dsp/loudness/`, `src/decode/scanner.rs`,
`src/decode/loudness_cache.rs`, `src/bin/replaygain_scanner.rs`.

## 5.20 Playlist

**Purpose.** Queue management with shuffle/repeat/history. **Mental model.**
A deck of cards with rules. **Inputs.** Enqueue/remove/clear/play-index/
next/previous/repeat/shuffle. **Processing.** Ordered queue, shuffle cycle
(every entry exactly once before repeating), repeat modes, history for
Previous. **Outputs.** `PlaylistChanged` events + `PlaybackInfo`
index/length. **Used by.** tick. **Limitations.** No cross-session
persistence of queues (that's the host's job). **Location.**
`src/playlist.rs`.

## 5.21 Lane registry

**Purpose.** Play extra independent streams alongside the main track.
**Mental model.** Extra turntables plugged into spare channels. **Inputs.**
AddTrack/RemoveTrack/SetTrackGain/Pan/MasterGain/Send/DuckTracks.
**Processing.** Per-lane decoder + resampler + FIFO on bus slots ≥ 2, fed
as secondaries each block; ducking between lanes; telemetry per lane.
**Outputs.** Lane planes into the mix bus. **Depends on.** decode, mix
bus. **Used by.** hosts wanting simultaneous tracks. **Limitations.**
Capacity = free slots (max 8 total). **Location.** `src/engine/lanes.rs`.

## 5.22 Telemetry

**Purpose.** Let hosts read the engine's state from any thread without ever
blocking audio. **Mental model.** A bulletin board updated every tick;
anyone can glance at it. **Inputs.** Everything the tick knows. **Processing.**
`PlaybackInfo` snapshot published via `ArcSwap` (atomic pointer swap,
wait-free reads). **Outputs.** Position, state, volume, latency, CPU%,
counters (clips/NaNs/overloads/deadline misses), lanes, endpoints (with
drift ppm), DSD report, correction state, spatial telemetry/health,
diagnostics. **Used by.** every host. **Limitations.** Snapshot granularity
= tick rate. **Location.** `src/playback_info.rs`, `src/engine/telemetry.rs`.

## 5.23 Events

**Purpose.** Discrete lifecycle notifications. **Mental model.** Notifications,
not a bulletin board: you subscribe and get pinged on track end, errors,
device plug/unplug. **Inputs.** Internal state transitions. **Outputs.**
`EngineEvent` (lifecycle) and `OutputEvent` (devices) on separate channels.
**Used by.** hosts. **Limitations.** Bounded channels; hosts should drain.
**Location.** `src/events.rs`.

## 5.24 Command dispatcher

**Purpose.** Turn host messages into engine actions, organized by domain.
**Mental model.** A well-sorted inbox. **Inputs.** `EngineCommand` variants.
**Processing.** Per-domain handlers (playback, lifecycle, playlist, lanes,
eq, dsp, output, multichannel, capture) that mutate tick-thread state and
enqueue graph control messages. **Outputs.** State changes + graph control
queues. **Location.** `src/commands.rs`, `src/engine/commands/`.

## 5.25 C FFI

**Purpose.** Let C, C++, Python, C#, Node.js, and anything C-callable drive
the whole engine. **Mental model.** A translated control panel: every Rust
function gets a C twin. **Inputs.** C calls through an opaque handle.
**Processing.** `engine_create/destroy` + ~50 functions (transport, DSP,
playlist, endpoints, aux insert, spatial health, diagnostics, capture).
**Outputs.** Status codes (0 = OK); NULL/invalid handles are safe; no
panics cross the boundary. **Used by.** non-Rust hosts. **Limitations.**
Feature-gated (`c-ffi`); the surface is stable but additions need version
bumps. **Location.** `src/ffi.rs`.

## 5.26 Graph 2.0 (general-purpose topology, offline)

**Purpose.** Let any audio network — not just the fixed playback chain — be
built, validated, executed deterministically, and serialized. **Mental
model.** LEGO for signal flow: nodes with typed sockets, edges as the
wiring, a checker, and an offline player. **Inputs.** Builder calls
(`add_source/gain/delay/mix/split/sink/acoustic/buffer/convolution/hrtf/
resampler`) + edges. **Processing.** Typed-port validation, cycle
detection (with the offending cycle path), deterministic topological
sort (Kahn's, stable tie-break), latency analysis + automatic delay
compensation, serde round-trip, Graphviz export. **Outputs.**
`OfflineExecutor` rendering blocks; `LatencyReport`; compensated graphs.
**Depends on.** spatial::math (Vec3 serde), convolution. **Used by.** aelog
replay, latency suite, tests. **Runtime.** offline only — the realtime
`dsp::graph` is untouched. **Location.** `src/dsp/graph2/`.

## 5.27 Timeline, tempo, automation (offline)

**Purpose.** Make time a first-class render primitive: sample-accurate
scheduling in musical time. **Mental model.** A conductor's score with a
metronome and cue list. **Inputs.** Tempo/time-signature/loops/transport,
scheduled events (sample or beat), control curves. **Processing.**
`AudioClock` (playhead + monotonic master + tempo map + ramps, 480 PPQ),
piecewise-exact beat↔sample mapping across tempo changes, once-only
sample-accurate event firing, note-grid quantization, beat-authored gain
curves (`CurveBeats`). **Outputs.** Events with in-block sample indices;
drives the Graph 2.0 executor. **Used by.** aelog, musical automation
tests. **Runtime.** offline. **Location.** `src/dsp/timeline/`.

## 5.28 Aelog (deterministic session recording/replay)

**Purpose.** Turn any render session into a pure function of its log, so
bug reports and regressions become "replay this file." **Mental model.**
A flight recorder: every action is logged; the flight can be re-flown
exactly. **Inputs.** Every timeline mutation + audio input chunks
(clip-addressed, channel-major) + listener motion + baked-scene swaps.
**Processing.** Versioned JSON logs (`recording.aelog`), deterministic
replay (`replay_events` = identical event stream; `replay_render` =
byte-identical captured audio), content-addressed SHA-256 golden-render
cache with LRU budget (256 MiB default) and in-process memo. **Outputs.**
`ReplayOutcome` (audio tracks, listener trajectory, scene swaps),
captured audio. **Used by.** `aelog_replay` binary, quality harness,
fidelity suites. **Runtime.** offline. **Location.** `src/dsp/aelog/`.

## 5.29 Eval (quality harness)

**Purpose.** Measure whether components are technically correct, in a
reportable, regression-comparable way. **Mental model.** An automated lab
that issues PASS/FAIL certificates per component. **Inputs.**
Deterministic stimuli. **Processing.** 9 suites (pipeline bit-exactness +
THD+N, parametric EQ magnitude+phase, limiter true-peak ceiling,
resampler in-band gain, binaural inter-aural level, EBU R128 loudness,
convolution vs naive-direct, channel separation, HRTF interpolation
convexity), versioned content-addressed reference vectors, JSON/text
reports, cross-version `compare()` (regression detection). **Outputs.**
`EvaluationReport`. **Used by.** CI (`quality_harness` test), release
gates. **Runtime.** offline. **Location.** `src/eval/`.

## 5.30 Profile (AudioProfile analyzer)

**Purpose.** Give hosts a deterministic, serializable description of a
file's character (loudness, dynamics, spectrum, transients, stereo,
spatial, content class) for smart defaults. **Mental model.** A lab report
on the audio's personality. **Inputs.** PCM (streaming pass). **Processing.**
BS.1770-4 loudness (shared meter), Hann-windowed FFT averages, onset
detection, running L/R + mid/side stats; `AnalysisMask` for selective
analysis; confidence semantics; on-disk cache (size/mtime or content-
fingerprint keyed). **Outputs.** `AudioProfile`. **Used by.** hosts
(future smart defaults), tests. **Limitations.** Explicitly heuristic
content probabilities — deliberately not a trained model. **Location.**
`src/profile/`.

## 5.31 Capture (WASAPI loopback)

**Purpose.** Record what the system is playing. **Mental model.** A tape
deck wired to the system mixer. **Inputs.** System mix packets.
**Processing.** Loopback thread fills a ring; tick drains to a streaming
float32 WAV writer; header finalized on stop. **Outputs.** WAV files +
CaptureStarted/Stopped/Error events. **Limitations.** Windows + feature
only; independent of playback state. **Location.**
`src/output/wasapi_loopback.rs`, `wav_writer.rs`, `engine/commands/capture.rs`.

## 5.32 Spatial layer (summary — full treatment in Section 6)

**Purpose.** Author audio in 3D world space; render to any speakers or to
headphones. **Subsystems.** scene/object/bed/field (content), speaker
(geometry), panner/vbap/ambisonic/binaural (renderers), hrtf (head model +
datasets), room/acoustic (simulation + baking), directivity/occlusion/
spread/doppler (behavior), tracking (head), automation/health/metering/
voice/quality (control), sofa (import), upmix/nearfield (extras),
SpatialNode (live-graph integration). **Runtime.** renderers are real-time
safe (zero alloc); solving/baking/analysis are control/offline.
**Location.** `src/spatial/` (whole tree).

---

# 6. Spatial Audio: Dedicated Deep Explanation

The spatial layer follows one governing principle, stated twice in the code:

> **Channels describe the output reproduction system; spatial objects and
> fields describe the content.**

In plain terms: you place sound *sources* in a 3D world, and the engine
figures out how they should sound on *whatever* speakers (or headphones) you
have. You never re-author the content for 5.1 vs stereo vs 7.1.4.

## 6.1 Concepts: what each piece is, and how this engine does it

**World coordinates & the listener.** A shared 3D frame: `+X` right, `+Y`
front, `+Z` up; positions in metres, angles in degrees at the API, radians
inside. The listener is a position + orientation (yaw/pitch/roll) in that
world. World-fixed sources stay put as the listener turns — that's the
foundation of head tracking.

**Spatial sources.** Three kinds of content, all mixed by one renderer:

| Class | Concept | This engine's implementation |
|---|---|---|
| **Object** | A point (or extended) sound at a world position | `SpatialAudioObject`: position, gain, spread, LFE send, velocity, orientation, room send; shareable audio via `ObjectAudioRef` |
| **Bed** | Channel-based content (e.g. a 5.1 stem) | `SpatialBed`: semantic-role routing onto matching output speakers; bounded store (≤ 16); unmatched channels drop cleanly |
| **Field** | Positionless diffuse ambience (rain, room tone) | `SpatialField`: encoded into the ambisonic bus (W only = perfectly diffuse) with √N compensation, decoded onto every pan speaker with per-speaker decorrelation (2–10.25 ms delay rings) |

**Distance attenuation.** Sound gets quieter with distance. Implemented as
`DistanceModel`: Linear / Inverse / Inverse Square / Inverse Reference, plus
`AirAbsorption` (a distance-dependent high-frequency roll-off — farther
sound darkens).

**Directivity.** A source can be directional (a voice, a speaker).
Implemented as `Directivity`: omnidirectional / cardioid / supercardioid /
custom 2°-step curve, evaluated at the angle between the source's facing
direction and the listener.

**Spread.** How wide an extended source is. Implemented per the "angular
region" recipe: one solve on the exact direction (weight 1−s) + 3 ring
samples at s×60° (weight s/3), aggregated by speaker and energy-normalized
— a fixed 4-solve pattern, deterministic and allocation-free.

**Doppler.** Pitch shift from motion. Implemented live per block from
`object.velocity − listener.velocity` (the docs corrected an old "future
Doppler" claim — the code now describes the live per-block path).

**Occlusion.** A wall between source and listener muffles sound.
Implemented as `Occlusion` (amount + max attenuation + min cutoff) →
`AcousticTransmission` (attenuation dB + cutoff Hz + diffusion seam): a
per-object low-pass biquad with smoothed block-rate cutoff, so occluded
sources are quieter *and* duller.

**Diffraction.** Sound bends around edges. Implemented in the acoustic world
solver: `DiffractionEdge` fins/mullions and doorway jambs produce
wedge-diffraction paths with their own low-pass corners.

**Transmission.** Sound through a wall/portal. Implemented as material
per-band transmission spectra; portals add a material-filtered transmitted
path.

**Air absorption.** HF roll-off over distance — see `AirAbsorption` above;
recently extended into the baked acoustic kernels (per-path, distance-
dependent) so far reflections genuinely darken.

**Reflections (early).** The first echoes off walls. Implemented with the
image-source method: a box room's walls are mirrored to produce virtual
"image" sources; each image contributes a delayed, gain-scaled, spectrally-
colored copy. Order 1 → 6 images, order 2 → 24 distinct. Per-object delay
rings (≈171 ms) hold the taps.

**Late reverberation.** The dense tail after early reflections. Implemented
as a Schroeder tail (4 parallel comb filters + 2 serial allpasses, gains
derived exactly from RT60) fed by the room send, encoded into the
ambisonic bus and decoded as a diffuse source.

**Acoustic paths.** The sim→render contract: `AcousticPath` (kind,
direction, distance, delay_samples, gain, lowpass corner, flags). The
solver enumerates Direct / Reflected / Diffracted / Transmitted / Diffuse
paths between a source and a listener.

**Rooms.** Two generations exist: the simple `Room` (axis-aligned box, one
wall absorption coefficient, order, RT60 — used by the renderers' live
path) and the richer `AcousticRoom` (per-wall frequency-dependent
materials, portals, diffraction edges — used by the acoustic world
solver). The per-wall seam is explicitly documented.

**Materials.** Per-ISO-octave-band (63 Hz–16 kHz) absorption, specular-
reflection, and transmission spectra, with named presets: Concrete, Wood,
Glass, Fabric, Carpet, Metal, OpenMesh. `MaterialSpectrum` also reduces to
a broadband gain + −3 dB low-pass corner for the realtime renderers.

**Acoustic baking.** For static scenes, the solve is identical every block,
so it's computed once and cached: `AcousticBaker` bakes a `BakedScene`
(position → response cache, 0.5 m cells) that renderers look up at audio
time — no solving, no allocation. A bake is a cache, not a new model:
unbaked positions fall back to the live solve.

**VBAP.** Vector-Based Amplitude Panning: place a sound by solving which
speakers surround its direction. Implemented as `VbapRenderer`: 3-triplet
solves on 3D layouts, 2D azimuth-pair reduction for coplanar layouts, a
Delaunay-style empty-triangle filter so triangles tile the sphere cleanly,
energy-normalized gains, and a deterministic nearest-speaker fallback for
directions outside the array's coverage (under the floor, above the rig).

**Ambisonics / HOA.** A speaker-independent field representation: instead of
"send this much to speaker 3," you encode the *sound field* as a small set
of basis signals (W, X, Y, Z for order 1; 9 signals for order 2; 16 for
order 3) and any layout can decode it. Implemented with the exact
spherical-harmonic basis (ACN ordering, SN3D normalization), exact bus
rotation (the field stays world-fixed as the listener turns), and Basic /
max-rE decoder policies with per-order weights. Order-1 FOA is pinned;
orders 2 and 3 are implemented; order-4+ is future work.

**Binaural rendering.** Rendering to two ears as if the sound came from 3D
space around your head — no speakers needed. Implemented as
`BinauralRenderer`: every object/bed path becomes a per-ear interaural
time difference (ITD) + a head-shadow spectral filter; diffuse content
decodes onto a virtual 8-speaker ring first, then each virtual speaker is
head-modeled. Mirror symmetry (left/right) is the exact invariant.

**HRTFs.** Head-Related Transfer Functions: the measurable fact that a sound
from a given direction is altered by your head and ears (delay, shadowing,
pinna filtering) before reaching each eardrum. This engine implements the
**analytic** head model (Woodworth ITD formula + Duda-Martens head-shadow
shelf + a pinna `ElevationNotch`) and can additionally load **measured**
`HrtfDataset`s (azimuth × elevation grids of actual impulse responses,
bilinearly interpolated, 360°-wrapped) so the binaural path uses real
measured cues, including elevation.

**SOFA.** A file standard (.sofa) for storing measured HRTF data. The engine
imports the NetCDF-3 classic subset natively (pure-Rust, feature
`sofa-import`) into its `HrtfCorpus`/`HrtfDataset` pipeline. **NetCDF-4/HDF5
files are refused with a typed error** — deliberate, because robust HDF5
readers link the C library `libhdf5`, which violates the project's
pure-Rust, no-FFI rule. The gap is isolated behind `HrtfCorpus` so an
optional future feature could close it.

**Head tracking.** The VR/AR seam: feed a stream of IMU/VR orientation
samples and the soundfield follows your head. Implemented as `HeadTracker`:
shortest-path nlerp across the last two samples (glides, never snaps),
one-pole smoothing (`smoothing_ms`), optional rate limiting (a glitch can't
fling the field). It's control-side: the host samples it and applies the
orientation to the listener before each render block; the renderers never
change.

**Speaker layouts.** `SpeakerLayout`: stereo / 5.1 / 7.1 / 7.1.4 / custom
arrays with per-speaker `LayoutCalibration` (trim + time-align) applied
separately from geometry.

**Spatial quality tiers.** Low / Medium (default) / High / Ultra: advisory
knobs that scale how *refined* the (always-correct) render is — spread
samples, room reflection order, HRTF tap length, voice budget. Never
essential correctness or the real-time rules.

**Voice budgeting.** A hard cap on concurrent spatial voices (default 48)
with a full-quality sub-capacity (default 24): beyond it, voices are
*degraded* (cheaper rendering) or *dropped* (silenced), ranked by priority
(fixed order / distance / gain / user). Admission counts appear in
`SpatialTelemetry`.

**Spatial automation.** Positional-seconds control curves (`CurveScalar`)
on object gain/spread, evaluated allocation-free at block rate against the
spatial clock (`SetSpatialAutomation` / `SetSpatialAutomationTime`). The
musical counterpart (`CurveBeats`) lives in the offline timeline.

**Spatial persistence.** The active scene auto-saves across sessions to
`<data_local_dir>/engine/spatial_scene.json` (customizable via
`EngineConfig::spatial_autosave_path`, atomic writes, lifecycle hooks).

## 6.2 Diagrams

### 1. Simple spatial pipeline

```
scene (listener + objects + beds + fields + room)
        │  per-block audio planes + scene state
        ▼
renderer: BasicPanner | VbapRenderer | AmbisonicRenderer | BinauralRenderer
        │
        ▼
interleaved multichannel PCM (or stereo ears) ─▶ existing output core
```

### 2. Detailed spatial pipeline (one hybrid block)

```
objects:  world-space transform ─▶ distance model ─▶ air absorption
            ─▶ directivity (facing vs listener angle)
            ─▶ occlusion (attenuation + low-pass)
            ─▶ Doppler (velocity difference)
            ─▶ spread sampling (1 + 3 ring directions)
            ─▶ pan solves (equal-power pair or VBAP triplet)
            ─▶ coefficient smoothing ─▶ LFE send ─▶ calibration trim
              │                       (LFE is never a pan target)
              └─ room_send ──▶ EarlyReflections (image sources:
                               per-image pan solve + coeff·dist + spectral
                               low-pass, delayed via per-object rings)
                               └─▶ RoomLateField (Schroeder tail) ─┐
beds:     semantic-role routing onto matching speakers (trim + LFE)   │
fields:   ambisonic W encode ─▶ bus ─▶ √N decode ─▶ decorrelation ─┘│
        │                                                           │
        ▼                                                           │
   hybrid sum into the interleaved output buffer ◀──────────────────┘
```

### 3. Source-to-speaker / binaural flow

```
                  ┌─▶ speaker array: pan solves ─▶ per-speaker taps ─▶ out
source audio ──▶   │
   (per-object,    ├─▶ ambisonic bus: encode ─▶ rotate (listener) ─▶ decode ─▶ out
    per-bed,       │
    per-field)     └─▶ binaural: per-ear ITD + head shadow (+ measured
                         HRIR convolution when a dataset is loaded) ─▶ L/R ears

    The same scene flows through all three; you choose one renderer.
```

### 4. Room / acoustic path diagram

```
AcousticRoom (per-wall MaterialSpectrum) + Portal(s) + DiffractionEdge(s)
        │  AcousticWorld::solve(source, listener)     [control/offline]
        ▼
[AcousticPath; N]  Direct · Reflected · Diffracted · Transmitted · Diffuse
        │  each carries: direction, distance, delay_samples, gain, lowpass,
        │                material spectrum
        ▼
   live renderers (binaural/panner/vbap)   OR   AcousticBaker ─▶ BakedScene
        │                                        (0.5 m cells, cached)   │
        ▼                                        renderers: set_baked     ▼
   direct + one delayed copy per path         cell hit? ─▶ cached taps (no solve)
                                              miss ─▶ live solve fallback
```

### 5. When to use which spatial feature

| Situation | Use | Why |
|---|---|---|
| Stereo speakers, simple placement | BasicPanner | Equal-power pair pans, cheapest |
| 5.1/7.1/7.1.4/custom arrays, 3D placement | VbapRenderer | Real speaker-geometry solving |
| Fixed speaker layout, want field recording/compat | AmbisonicRenderer | Layout-independent encoded field |
| Headphones, immersive scene | BinauralRenderer | The head IS the pan |
| Headphones + measured data | BinauralRenderer + HrtfDataset | Real per-person cues incl. elevation |
| VR/AR | + HeadTracker | Field follows the head |
| A room's sound | Room (live) or AcousticWorld + bake (static scenes) | Reflections + tail; bake for zero runtime cost |
| A moving sound world | AcousticWorld + BakedScene with listener drive / scene swaps | Geometry timeline replays deterministically |
| Background ambience | SpatialField | Diffuse, positionless |
| Channel-based stems (dialogue, FX) | SpatialBed | Role routing to matching speakers |
| Occluded sources | Occlusion | Muffle + attenuate |
| Directional sources | Directivity | Cardioid etc. |
| Widened sources | Spread | Angular-region sampling |
| Live-graph stereo master binaural | SpatialNode (plan step) | One toggle on the production chain |
| Limited CPU | SpatialQuality::Low + voice budget | Cheaper samples/reflections, degraded voices |

---

# 7. DSP and Processing Graph

## 7.1 Conceptually

Think of the DSP chain as a **factory assembly line** whose layout is chosen
at startup and can be re-plumbed while products are moving through it,
without a single product being damaged:

- **Nodes** are workstations (mix, EQ, compressor, reverb, volume…).
- **Connections** are conveyors between them.
- **The execution plan** is the paper order that says exactly which
  workstation each batch visits, in which order.
- **Reconfiguration** is the factory switching to a new paper order at a
  batch boundary: the old order finishes its current batch, then the new one
  takes over — no partially-processed batch is ever dropped.

## 7.2 Technically (the production graph, `dsp::graph`)

- **Node arena.** A fixed array of typed graph nodes (`GraphNode`), each with
  capability metadata (latency/tail/bit-perfect behavior).
- **Compiled plans.** `PlanSet::compile()` produces the canonical stage
  order as an ordered list of `(node, channel scope)` steps. The plan is the
  single source of truth for the chain; the audio path only *reads* it.
- **Two plans.** `Normal` (stereo) and `NormalMc` (multichannel, prepends the
  routing/trim step). Bit-perfect/DoP bypass isn't even a plan — the entry
  points return before any stage runs.
- **Generation swap.** Reconfiguration builds a fresh `GraphGeneration`
  (nodes + plan) on the control thread and publishes it with an atomic
  pointer swap at a block boundary; the retired generation is reclaimed only
  after the audio thread has left it. Per-slot/per-node user state (gains,
  toggles, sends) is mirrored onto sticky atomics and **replayed** into the
  fresh generation, so nothing resets during a live swap.
- **Control queues.** Each node has an SPSC control queue; commands (e.g.
  `SetEqBand`, `SetAuxInsert`) drain at block boundaries and apply for the
  next block — sample-accurate boundaries, zero locks.
- **Validation & ordering.** Stage order is fixed by the plan (not derived
  per run); disabled stages are skipped, never reordered. In the *offline*
  Graph 2.0 world, ordering is derived by topological sort and validated
  (cycle detection etc.) — different regime, same discipline.

### The canonical plan order (from `plan.rs` — code wins over README)

```
(stereo)      mix → aux → correction → eq → dynamics → convolution →
              balance → crossfeed → stereo → timestretch → volume →
              seek_fade → spatial
(multichannel) routing (channel trim) prepended before mix
```

- `mix` runs every bus input's pre-mix (loudness + preamp) and sums them.
- `aux` consumes the mix node's send taps, returns into the master **before**
  the post-mix chain (so EQ etc. process the aux return too).
- `correction` runs post-aux / pre-EQ (user EQ stacks on the corrected
  response); skipped when disabled or IR-less.
- `spatial` (the `SpatialNode`) is the final step — it spatializes the
  fully-processed stereo master.
- **Downstream of the graph** (output domain, at the output rate):
  resampler → final safety limiter → dither.

> **Doc discrepancy note (resolved):** the README's chain summary previously
> omitted the trailing `spatial` step and listed the resampler/limiter/dither
> as if in the same chain; the README has been corrected. The code places
> `spatial` last in the plan and the safety chain in the output domain — the
> code is authoritative.

## 7.3 Latency & compensation in the graph

- Each stage's intrinsic delay (limiter lookahead, convolution IR length,
  crossfeed delay, timestretch window, resampler filter) is accounted and
  reported as `latency_ms`; `position_secs_compensated` subtracts it.
- In the offline Graph 2.0 world, per-node taps (Delay, Convolution, HRTF,
  Resampler) propagate as cumulative upstream latency, and `compensate()`
  splices `Delay` nodes onto faster branches so every path into a merge
  arrives aligned — while preserving original node ids so scheduled events
  still address them.

## 7.4 Example: how a normal stereo track flows

```
Slot 0 (primary)  ─┐  preamp + loudness + gain/pan/automation
Slot 1 (next)     ─┼─▶ MIX ──▶ (sends) ──▶ AUX ──▶ (return) ─┐
Lane slot 2       ─┘                        (optional IR insert)│
                                                                ▼
CORRECTION (optional IRs) ─▶ EQ (64 bands + graphic) ─▶ COMPRESSOR
   ─▶ CONVOLUTION (reverb IR) ─▶ BALANCE ─▶ CROSSFEED ─▶ STEREO width
   ─▶ TIMESTRETCH (if speed ≠ 1) ─▶ VOLUME (ramped dB) ─▶ SEEK FADE
   ─▶ SPATIAL (optional binaural master) ─▶ RESAMPLE ─▶ LIMITER ─▶ DITHER
   ─▶ ring ─▶ device
```

Any stage can be disabled; disabled stages contribute nothing (bit-exact).

---

# 8. Real-Time Audio Safety

## 8.1 The guarantee

The engine's core promise: **the real-time audio path performs no heap
allocation and takes no locks.** This is verified by
`tests/fidelity/realtime_allocation.rs`, which runs many blocks through the
hot paths (including the spatial and correction paths) and asserts zero
allocations, and by `ring_buffer_stress.rs` for concurrent ring behavior.

## 8.2 Things That Must Never Be Done on the Real-Time Audio Thread

Derived from the code's documented invariants (`AGENTS.md`, `ARCHITECTURE.md`,
the `SampleSink` contract, and the hot-path modules):

1. **Never allocate memory** — no `Vec` growth, no `Box`/`String`, no
   `HashMap` inserts, no format/`write!` (formatting allocates). All buffers
   are preallocated; steady state reuses them.
2. **Never take a lock** — no `Mutex`, no `RwLock`; only atomics and
   cache-padded SPSC rings. The `VecSink` is explicitly documented as
   using a `Mutex` precisely because it is *not* the realtime path.
3. **Never do I/O** — no disk reads, no network, no file writing. (The
   tick thread may touch I/O, but never a backend callback.)
4. **Never touch shared mutable state** — each endpoint thread reads only
   its own ring, its own resampler/slip, and the shared graph read-only.
   Shared audio-thread state (e.g. the `AuxSendBus`) is safe only *by
   contract* — both sides run on the same audio thread; the contract is
   documented next to the `unsafe impl Sync`.
5. **Never call into the host or the engine's control API** — no blocking
   calls, no `EngineHandle` use from a callback. Host interaction is via
   `EngineCommand` messages, events, and atomic telemetry only.
6. **Never let a single sample path take unbounded time** — no
   conditionals that grow with input length; no recursion; no syscalls.
7. **Never leave denormals to crawl** — denormal flushing at DSP stage
   boundaries (a performance trap that silently costs 100× on some CPUs).
8. **Never read a partially-published configuration** — reconfiguration
   arrives only as a whole new generation at a block boundary.

## 8.3 How expensive work is moved away from the audio callback

| Expensive work | Where it actually runs |
|---|---|
| Decoding & DSP | The tick thread (soft real-time), decoupled by rings |
| Loudness scanning | Background worker; result applied only if still current |
| Correction IR derivation | Control path after capture |
| Acoustic solving & baking | Control/offline; renderers consume fixed-size paths |
| Graph 2.0 / timeline / aelog | Offline, host-driven |
| Profile analysis | Background/offline streaming pass |
| WASAPI loopback disk writes | Loopback thread fills ring; tick drains to WAV |
| Device hotplug handling | Monitor thread → `AutoRecoverStream` command |
| Generation builds / IR loads | Control thread; audio thread only reads the result |

## 8.4 Thread communication summary

```
host ──EngineCommand (bounded crossbeam channel)──▶ tick loop
host ◀──EngineEvent / OutputEvent (channels)────────┘
host ◀──ArcSwap<PlaybackInfo> (wait-free read)──────┘
tick ──▶ per-node SPSC control queues ──▶ applied at block boundary
tick ──▶ rings (FixedFrameBuffer, cache-padded atomics) ──▶ endpoint threads
tick ──▶ per-lane/per-endpoint rings (independent subscribers)
```

## 8.5 Failure handling on the audio path

- **Underruns / CPU overloads** are counted (u64 counters in telemetry) and
  reported — the engine detects and surfaces them rather than silently
  degrading.
- **Clips and NaNs** in the output callback are counted and reset on read —
  a nonzero NaN count is treated as a serious numerical bug to fix.
- **Fatal resampler failure** halts playback rather than playing at the
  wrong speed/pitch (`resampler_failed_fatal`).
- **A stuck secondary endpoint** drops oldest frames (bounded pending queue)
  and can never take down the primary device.
- **Device loss** triggers recovery (reopen against the same/fallback
  device), driven by monitor-thread commands — never by the audio thread.

---

# 9. Output, Devices, and Bit-Perfect Operation

## 9.1 The bit-perfect path vs the processed path — in plain words

**Processed path (default).** The engine treats your audio like a recording
in a studio: it may EQ it, compress it, resample it, and always run it
through a safety limiter and dither before it reaches the device. The sound
is *optimized*, not preserved.

**Bit-perfect path.** The engine promises: *the exact bits that are on the
disk arrive at the DAC unchanged.* For that to be true, **three things must
all hold**:

1. The DSP bypass is on (bit-perfect mode — every stage skipped; only
   volume ramps and seek fades survive), or the track is DSD-over-PCM (DoP)
   where even volume is skipped.
2. No sample-rate conversion happens (the file rate equals the device rate).
3. The output stream is genuinely exclusive/direct — verified against the
   OS, not assumed (ALSA `hw:`, WASAPI exclusive, ASIO, CoreAudio hog).

If all three hold, `PlaybackInfo::bit_perfect` is `true`. If the engine
cannot prove any one of them (e.g. shared mode because the device was busy,
fallback policy allowed), it says so — `bit_perfect = false` with a typed
`BitPerfectCause` and human message. **It never guesses.**

Under the hood the verdict is even finer: `EngineStats::bit_perfect_report`
(`BitPerfectReport`) evaluates every condition separately — volume unity,
per-stage DSP bypass (EQ/compressor/convolution/correction/crossfeed/
stereo/limiter/loudness), no SRC, lossless source codec, bit-depth not
truncated by the output container, no dither, no active crossfade, and
exclusive/verified transport — and separates "**samples** untouched" from
"**transport** provably direct" so a host can show exactly which condition
failed and why (e.g. `BitPerfectCause::DitherActive`,
`CrossfadeActive`, `SoftwareVolume`).

```
file ──▶ [DSP chain] ──▶ resample ──▶ limiter ──▶ dither ──▶ device
         processed path: all stages active (may alter the signal)

file ──▶ bypass ──▶ (volume/fade only) ──▶ no resample ──▶ device
         bit-perfect path: original bits, rate-matched, exclusive mode

DSD ──▶ DoP bypass: 24-bit DoP words passthrough (no volume, no DSP)
```

## 9.2 Supported device backends

| Backend | Platform | Feature | Mode | Notes |
|---|---|---|---|---|
| ALSA native | Linux | default | `hw:` / `plughw:` exclusive | Native DSD works on exact `hw:` nodes |
| WASAPI native | Windows | `wasapi-native` | `IAudioClient` exclusive + loopback capture | OS-level exclusivity verification |
| ASIO native | Windows | `asio-native` | Pure-Rust `IASIO` via COM, native DSD transport hook, driver control panel | No C++ SDK needed |
| CoreAudio | macOS | default | Hog-mode, direct HAL IO procs | Hardware endpoint volume |
| CPAL | All | default | Shared-mode fallback | Everything else |

Each backend must verify exclusivity with the OS before claiming a device;
requests are honored under the `FallbackPolicy` (Strict vs Allow).

## 9.3 Endpoint abstraction & multiple outputs

`endpoint.rs` is the per-device worker: **one bounded SPSC ring, one nominal-
ratio resampler, one `Slip` drift corrector, one rate-matched final limiter,
one gain, and one realtime thread per configured endpoint.** The decoded
master block is fanned out to every endpoint's ring; each endpoint resamples
to its own device rate and trims to its device's actual clock. Telemetry
reports per-endpoint written/dropped/pending frames, transport errors, and
**drift in ppm**. A failing endpoint is logged and skipped — it can never
take down the primary.

**Drift correction explained.** Two devices with independent crystals drift
apart by a few parts per million — over minutes that's enough to empty or
overflow a buffer. Each endpoint's resampler runs at the fixed nominal
ratio; a rubato `Slip` (a 1:1 "clutch" that inserts or drops a single frame
behind a short crossfade) is steered by a proportional ring-fill controller
(clamped ±500 ppm) so the ring tracks the device's *real* clock. You hear
nothing (a single-frame crossfade every so often) and the buffer never
starves or overflows.

## 9.4 Device capability detection & recovery

- Capability records per backend (`capabilities.rs`); negotiated
  format/access/latency info reported via `OutputInfo` in telemetry
  (`OutputAccessMode`: Shared / Exclusive / DirectHw / BitstreamPassthrough;
  `is_bit_perfect` = actual direct + verified).
- Hotplug via `device_monitor.rs` → `OutputEvent::{DeviceConnected,
  DeviceDisconnected, DeviceListChanged, OutputDeviceChanged}` →
  `RecoverStream`/`AutoRecoverStream`.
- Output profiles auto-select per device (backend preference, dither policy,
  rate handling).

## 9.5 DSD / DoP transport matrix

| Transport | ALSA `hw:` | WASAPI excl. | ASIO | CoreAudio |
|---|---|---|---|---|
| Native DSD | ✅ (exact hw node) | ❌ (no format) | ❌ (vendor extension not implemented) | ❌ (no native format) |
| DoP | ✅ | ✅ (I32 @ bit_rate/16) | ✅ | ⚠ documented target, not implemented |
| PCM conversion | ✅ (default) | ✅ | ✅ | ✅ |

`DsdTransportReport` records the full negotiation: requested vs actual,
wire format, bit rate, and the ordered fallback steps — so a UI can render
"DSD source → native DSD unavailable → DoP → PCM" instead of a single label.
Never a silent fallback.

## 9.6 Channel mapping

Sources map to outputs by semantic role (BS.775) with per-channel trim,
routing matrices, channel EQ, LFE, and bass management; `ChannelPolicy`
controls preservation vs downmix (see 2.7). Output channel count is what the
sink receives; the engine downmixes/preserves *before* samples reach the
sink.

---

# 10. Analysis and Audio Intelligence Already Present

## 10.1 The layers of "knowing" audio

| Layer | What it is | This engine's tools |
|---|---|---|
| **Raw signal measurements** | Physics of the samples, no perception | Real-time analyzer (peak/RMS/dominant frequency/FFT spectrum); clip & NaN counters; channel correlation meters |
| **Perceptual measurements** | What a calibrated listener would hear | EBU R128 loudness (LUFS, dBTP, LRA, ReplayGain) — broadcast-standard, gated |
| **Metadata** | What the file says about itself | Editorial tags, technical format info, gapless info, cue sheets, loudness tags |
| **Fingerprinting** | A content-based ID | Chromaprint/AcoustID (feature) |
| **Spatial analysis** | Where the sound is / how wide | Mid/side side-fraction + ambience dB; per-source spatial health (localization quality, direct-vs-reflected ratio, occlusion severity, phase risk); measured stereo correlation |
| **Quality diagnostics** | Is the chain healthy | Typed diagnostics (fault/track/decode/output/bit-perfect/config), volume-path reports, drift ppm, underrun/overload counters, spatial debug view |
| **Acoustic analysis** | What a room does | ESS sweep measurement with harmonic separation and SNR, per-channel IRs, derived corrections |

## 10.2 What the engine already "knows" about audio

For any file it can produce (offline, via `AudioProfile`):

- **Loudness**: integrated/short-term LUFS, true peak dBTP, loudness range,
  loudness stability (0–1).
- **Dynamics**: crest factor, dynamic range, a coarse character class
  (Compressed / Moderate / Dynamic), a compression heuristic (0–1).
- **Spectral balance**: centroid Hz, rolloff Hz, tilt dB/octave, flatness,
  brightness dB.
- **Transients**: onset density (events/sec), onset strength.
- **Stereo image**: channel correlation, width heuristic, L/R balance, phase
  risk (out-of-phase fraction).
- **Spatial density**: side-energy fraction, ambience dB (from mid/side).
- **Content class**: normalized heuristic probabilities (speech / music /
  ambient) with an explicit "no evidence" prior and a confidence score
  (duration × coverage).

All of it is deterministic DSP with documented units and ranges; `None`
means "not computable," and confidence below ~0.5 means "provisional." The
same underlying meter drives the loudness scanner and the profile, so the
numbers agree.

## 10.3 What it does NOT understand

- **Semantic content**: no speech recognition, no transcription, no
  lyrics/title inference.
- **Genre, mood, energy for recommendation** beyond the crude content-class
  heuristics.
- **Instrument recognition**, harmony, or structure (verse/chorus).
- **Source separation** (vocals from music).
- **Room analysis from arbitrary recordings** (it measures *its own*
  sweeps through *its own* capture — it does not infer acoustics from
  ordinary audio).
- **Head-related personalization**: it uses measured datasets but has no
  per-listener HRTF fitting/optimization.
- **Learned models of any kind** — by design (see below).

## 10.4 Would an intelligent/ML subsystem add value?

Yes, but the boundaries are now clear and favorable:

- The `AudioProfile` interface is **deliberately shaped** so a future tiny
  ML feature-supplier could fill the same fields without changing consumers
  (its module docs say exactly this). The heavy lifting — loudness,
  spectral, stereo measurements — is already done deterministically; ML
  would refine *classification* (content class, genre, quality scoring),
  not the measurements.
- The content-class probabilities are explicitly "normalized heuristic
  indicators with an explicit no-evidence prior — never a hidden learned
  model." That is an honest invitation: replace the heuristic classifier
  with a real one behind the same fields.
- Candidate high-value ML uses: content classification (the existing
  `ContentProfile`), perceptual quality scoring (the eval harness's
  tolerance framework is already there), headphone/room-correction target
  selection, smart default suggestion from profiles (spatial defaults,
  AutoEQ aggressiveness, upmix selection — the profile module names these
  consumers explicitly).
- The engine itself should stay deterministic and ML-free on the audio
  path; intelligence belongs in offline analysis and host-side decisions.

---

# 11. Testing, Fidelity, and Determinism

## 11.1 Why a professional audio engine tests like this

Audio bugs are insidious: a change that shifts a filter by one coefficient
can pass every "does it sound okay?" check yet alter measured behavior;
memory bugs can be silent until a glitch; and regressions can hide in a
single least-significant bit. This engine treats testing as a *hard
measurement discipline*, not a checkbox:

- **Bit-exactness** is tested, not hoped for: reference vectors and a
  graph-vs-pipeline oracle catch any accidental change.
- **Zero-allocation** is tested, not assumed: the realtime suites run real
  hot paths and fail on any heap allocation.
- **Fidelity thresholds** are numeric and committed (e.g. "residual within
  ±0.5 dB, 40 Hz–16 kHz"), so "good enough" is defined in the repo.
- **Determinism** is tested: identical sessions produce identical logs and
  byte-identical audio.

## 11.2 The test stack

| Layer | What it covers | Where |
|---|---|---|
| Unit tests | Individual modules (filters, math, config, codecs, spatial math) | `#[cfg(test)]` inside `src/` (~1,300 test functions total across src+tests) |
| Headless integration | Engine lifecycle end-to-end without hardware | `tests/headless_playback.rs`, `decoder_from_memory.rs`, `memory_and_hotplug.rs` |
| Fidelity suites (55) | DSP/spatial/acoustic correctness with committed thresholds | `tests/fidelity/` (each a `[[test]]` entry in Cargo.toml) |
| Golden reference vectors | Frozen input→output captures | `golden_reference_vectors.rs`, `golden_corpus_expansion.rs` |
| Graph-vs-pipeline oracle | Production graph ≡ reference pipeline, bit-exact | `graph_pipeline_equivalence.rs` |
| Realtime allocation | Zero allocations on hot paths (10k+ blocks) | `realtime_allocation.rs` |
| Concurrency stress | SPSC ring correctness under load | `ring_buffer_stress.rs` |
| Decoder robustness / fuzzing | Corrupted/truncated/mutated inputs never crash | `decoder_robustness.rs`, `fuzz_mutation.rs` |
| Deterministic replay | aelog logs replay to byte-identical audio | `aelog_replay.rs`, `aelog_inputs.rs`, `aelog_automation.rs`, `aelog_scene.rs`, `aelog_cache.rs` |
| Quality harness | 9 objective suites + cross-version regression detection | `quality_harness.rs` + `src/eval/` |
| Measurement suites | ESS sweep, min-phase, correction inverse, room correction pipeline | `ess_measurement.rs`, `minimal_phase.rs`, `correction_inverse.rs`, `room_correction_pipeline.rs` |
| Spatial suites | panner, VBAP, objects, hybrid, ambisonic, HOA, room, binaural, tracking, node, HRTF IR, scene, acoustic world/bake | `spatial_*.rs`, `acoustic_*.rs` |
| Graph 2.0 / timeline / latency | topology, scheduling, compensation, resampler/HRTF/convolver taps | `graph_topology.rs`, `timeline_scheduler.rs`, `latency_alignment.rs`, `convolver_taps.rs` |
| Output profiles / format | device profiles, EQ response, resampler quality | `output_profiles.rs`, `eq_*.rs`, `resampler_*.rs`, `dither_measurement.rs`, `limiter_*.rs` |
| Benchmarks | Criterion: DSP, pipeline, graph plan, spatial | `benches/` (4 harnesses) |

> **Doc discrepancy note (resolved):** the README previously claimed "42
> integration/fidelity test files… over 860 tests"; it has been corrected to
> match the verified counts — 58 test files (55 fidelity suites + 3 headless)
> containing roughly 1,300 test functions.

## 11.3 Deterministic replay & reproducibility

The crown jewel is the **aelog golden-render pipeline**: a session (every
control change, every audio input chunk — clip-addressed and channel-major —
every listener position, every baked-scene swap) is recorded into a
versioned, wall-clock-free JSON log. Replaying it reproduces:

- the identical fired-event stream, and
- **byte-identical captured audio** through Graph 2.0,

with captures cached by **content address** (SHA-256 of the render identity:
log hash × graph fingerprint × sink), under an LRU byte budget (default
256 MiB) plus an in-process memo — so a synced cache directory is valid on
any machine and repeated renders of the same session are free. A bug report
becomes "replay this log"; a regression check becomes "compare against the
golden capture."

The `eval` harness extends the same discipline to *quality expectations*:
reference vectors are versioned and content-addressed, so a changed
expectation changes the address and a drifting spec is detectable; and
`EvaluationReport::compare` diffs two engine versions and flags
regressions (a clean release must report zero).

## 11.4 Where testing is weaker or less explicit

- **Perceptual (listening) results** are not in CI — `docs/QUALITY.md`
  defines a rigorous controlled-listening procedure (ABX/anchored,
  blind, documented conditions) but it is a human-run protocol, not an
  automated gate.
- **Hardware-dependent behavior** (exclusive-mode verification, hotplug,
  drift correction against real crystals, native DSD wire formats) is
  tested where the OS allows (CI compiles the native backends cross-
target; real-device validation is inherently manual).
- **Windows/macOS-only paths** (WASAPI exclusive, ASIO, CoreAudio hog,
  loopback capture) have compile-time coverage in CI but limited
  runtime coverage on non-Windows/non-macOS runners.
- **Long-horizon robustness** (hours-long drift, thermal behavior,
  memory over days) has no soak-test suite — though the bounded-memory
  design and allocation tests make leaks unlikely.
- **Fuzz coverage** targets decoders; there is no property-based fuzzing
  of the DSP graph or spatial scene parameters.
- **Benchmarks** exist but are not asserted in CI with hard budgets
  (criterion tracks, humans review).

---

# 12. Performance Architecture

## 12.1 Where the CPU goes

| Operation | Cost class | Notes |
|---|---|---|
| Per-sample DSP (gains, fades, biquads) | Cheap | Vectorizable; biquad cascades promote to f64 internally where it matters |
| Mix sums | Cheap | O(slots × frames); SIMD aux accumulate with a strict bit-exact contract |
| Parametric EQ (≤64 bands) | Moderate | Per-band biquad per sample |
| Multiband compressor | Moderate | 3 bands of envelope + gain |
| **Convolution / correction IRs** | **Expensive** | Partitioned FFT: O(P·2B·log2B) per block; scales with kernel length and partition count |
| **Resampling** | **Expensive** | Rubato sinc; the *quality tier* is the biggest single knob: ≈320 / 640 / 1120 / 2240 effective taps → roughly Low/Medium/High/Very-High CPU |
| Time-stretch (WSOLA) | Expensive | Similarity search over ±window/4 per synthesis frame; tier-dependent (512/1024/2048 windows) |
| True-peak limiter | Moderate | 4× oversampled FIR detection |
| DSD→PCM decimation | Expensive when active | 1-bit → multi-bit conversion; off the hard-realtime path |
| Spatial render (objects/room/binaural) | Moderate→High | Scales with voice count, reflection order, spread samples, HRTF tap length; quality tier + voice budget govern it |
| Acoustic solving / baking | Heavy | **Offline/control path only** — renderers consume fixed paths |
| Analysis (loudness/profile/eval) | Heavy but off-path | Background/offline; never touches audio |

## 12.2 Where memory goes

- **Preallocated buffers everywhere on the hot path**: ring buffers (fixed
  capacity), scratch planes, delay lines (mix, aux, correction, spatial
  rings), partitioned FFT state — allocated at construction/prepare, reused
  forever. Steady-state allocation is zero (tested).
- **State scales with**: mix slots × frames (scratch), convolution kernel
  lengths (FFT partitions + history), spatial voice count (per-voice
  rings/filters), resampler quality (filter tables), drift-corrected
  endpoints (per-endpoint rings).
- **Bounded by design**: endpoint pending queues cap (`MAX_ENDPOINT_PENDING_
  FRAMES`), lane FIFOs bounded, spatial stores bounded (objects/beds/fields),
  aelog cache LRU-bounded (256 MiB default), profile cache validated by
  size/mtime.

## 12.3 Allocation-sensitive paths

Only the hot paths matter, and they are all zero-alloc by construction:
`DspGraph` process, the mix/aux sums, the spatial renderers (`prepare` does
all allocation; `process_block` reuses caller buffers), the endpoint
workers, the analyzer. Anything that allocates (IR loads, generation
builds, solving, analysis) lives on control/background/offline threads.

## 12.4 Scaling factors

The knobs that change cost by more than a constant factor:

- **Resampler quality**: CPU and latency grow substantially tier by tier
  (see table above).
- **Convolution/IR length**: partitioned FFT cost grows ~linearly in
  partitions; memory grows with kernel length.
- **Spatial**: voice count × (spread samples × pan solves + room taps);
  reflection order 2 → 24 images/object; HRTF tap length (≤ 128); quality
  tier (Low/Medium/High/Ultra) and the voice budget (default 48 voices /
  24 full-quality) bound the worst case.
- **Multichannel**: cost scales with channel count up to 16; the front
  pair carries the stereo-linked stages, others carry routing/trim/volume.
- **Lanes/endpoints**: each lane is a decoder+resampler; each endpoint a
  resampler+slip+limiter — linear in count.

## 12.5 Low → High → Ultra progression (where it exists)

- **Resampler** (Fast→Balanced→HighQuality→Ultra): longer anti-aliasing
  filters, deeper stopband (150→180 dB), more latency (≈3→23 ms).
- **Time-stretch** (Low→Balanced→High): 512→1024→2048 windows, finer
  alignment, better transient/tonal fidelity, more CPU + latency.
- **Spatial quality** (Low→Medium→High→Ultra): minimal spread samples /
  first-order room → full spread refinement / second-order room → longer
  HRTF convolution / higher voice budget. Always-correct render; only the
  refinement changes.
- **Precision mode** (Performance→Quality): f32→f64 everywhere except the
  documented exceptions (WSOLA core, rubato internals, final limiter) —
  roughly 2× cost on the processing chain, no audible difference for most
  material but mastering-grade for the rest.

## 12.6 Real-time budget reasoning

The tick thread must finish (decode + DSP + output-domain + fan-out) per
block within the block's real-time budget (e.g. 1024 frames @ 48 kHz ≈
21.3 ms). Rings decouple bursts; the hard deadline belongs to the endpoint
threads, which only drain preprocessed frames. `cpu_usage_pct`,
`cpu_overloads`, and deadline-miss counters in telemetry make overload
visible instead of silent.

---

# 13. Configuration and Feature Interaction

All configuration lives in `EngineConfig` (the `config` crate): one
Serde-serializable object with ~30 sub-configs, a `validate()` that returns
typed issues, presets (Consumer / Fidelity), and a versioned envelope
(`VersionedConfig`) with a migration framework for future schema changes.
The engine exposes the same knobs at runtime through `EngineCommand`s.

## 13.1 The settings that matter most

| Setting | Meaning | Audible Effect | CPU Cost | Latency Cost | Interactions |
|---|---|---|---|---|---|
| `precision_mode` (Performance/Quality) | f32 vs f64 DSP | None for most material; mastering-grade for the rest | ~2× chain | none | WSOLA core, rubato internals, final limiter stay f32 (documented) |
| `resampler_quality` (Fast…Ultra) | Anti-aliasing filter length | Stopband depth 150→180 dB | Low→Very High | ≈3→23 ms | Only relevant when rates differ; interacts with `sample_rate_policy` |
| `sample_rate_policy` (FollowTrack / device / fixed) | Which rate wins | Rate-matching artifacts or their absence | — | — | FollowTrack may force resampling on every track change; conflicts with bit-perfect (which requires no SRC) |
| `volume_mode` (SoftwareOnly / HardwarePreferred / HardwareOnly / SoftwareAllowed) | Who owns the level | Hardware attenuation vs DSP gain | none | none | HardwareOnly + no hardware volume ⇒ signal untouched + `volume_error`; bit-perfect requests should use HardwareOnly |
| `fallback_policy` (Strict/Allow) | May an exclusive request fall back to shared? | Shared mode = OS mixer can alter the signal | — | — | Strict + busy device ⇒ open failure; Allow ⇒ shared fallback (bit-perfect false) |
| `dither_enabled` (+ per-device overrides) | TPDF at integer boundary | Low-level noise floor shaping | negligible | none | Should be off with all DSP disabled (validation warns); per-device profiles can force it |
| `limiter` (ceiling/attack/release/lookahead, true-peak, mode) | Final safety ceiling | Clipping vs transparency | Moderate | Default 5 ms lookahead | True-peak on adds oversampling cost; ceiling > −0 dBFS risks intersample clips |
| `transition_mode` (Gapless/Crossfade/Fade/Stop) + `crossfade` | Track handoff | Overlap vs seam | — | — | Gapless needs prepared-next; lanes ride after the incoming stream during crossfades |
| `speed_mode` (Varispeed/TimeStretch/PitchShift) + `timestretch_quality` | Speed/pitch semantics | Pitch follows speed or not | WSOLA expensive | Tier-dependent | Varispeed is resampling-based (no WSOLA); pitch semitones only meaningful in TimeStretch/PitchShift |
| `mix_slots` (2–8) | Bus width | Lane capacity | linear in slots | none | Lanes need slots ≥ 2; > 8 is clamped with a warning |
| `aux` (enabled/return/insert IR/wet) | FX send bus | Reverb/room on sends | insert = convolution cost | IR length | Insert needs the IR file; disabled = bit-exact |
| `correction` (enabled/phase/IR/depth) | Room/headphone correction | Flat response | convolution | phase-mode group delay | Runs post-aux/pre-EQ; disabled/IR-less = bit-exact; IR must reload on rate change |
| `eq` / `graphic_eq` / `autoeq` | Tone shaping | Everything tonal | 64 bands max | none | Auto-headroom reserves boost; graphic layer compiles into parametric |
| `loudness.mode` (Off/Track RG/Album RG/R128) | Normalization | Consistent level | negligible | none | Per-slot preamp in the mix bus; scans run once per file (cached) |
| `channel_policy` (ForceDownmixStereo/PassThrough/MaxChannels/SpatialRender) | Multichannel fate | Channel preservation vs downmix | channel count | none | PassThrough is conditional (see 2.7); SpatialRender is opt-in and routes through the spatial layer |
| `dsd_output` (PcmConvert/DoP/NativeDsd) | DSD transport | Whether 1-bit survives | decimation when active | — | Native only on ALSA hw:; DoP needs I32 container; downgrades are explicit (`DsdTransportReport`) |
| `spatial.*` (enabled/screen/room/listener/quality/voice/metering) | Master spatialization | Headphone 3D + room | voice-dependent | HRTF taps | SpatialNode affects the stereo front pair only (MC passes through); disabled = bit-exact |
| `endpoints[].drift_correction` | Per-device clock trim | Long-session stability | per-endpoint slip | none | Irrelevant when rates match; ±500 ppm clamp |
| `performance_mode` (Normal/LowPower) | Global cost hint | — | — | — | Host-advisory; low-power consumers pick cheaper defaults |

## 13.2 Interactions that surprise

1. **Bit-perfect + sample-rate policy**: bit-perfect requires *no*
   resampling — if the track rate ≠ device rate, `bit_perfect` is false
   even with DSP bypass on. Pick a device-rate-following policy.
2. **Hardware volume + bit-perfect**: to keep a truly untouched path, use
   `VolumeMode::HardwareOnly`; otherwise the DSP gain stage silently
   becomes the level owner (software attenuation = not bit-perfect).
3. **Loudness normalization + bit-perfect**: any normalization gain is a
   modification; bit-perfect mode bypasses the whole mix, so the two never
   coexist.
4. **Dither with all DSP off**: validation warns — dithering a
   bit-exact stream is a pointless (and audible in the noise floor)
   modification.
5. **Time-stretch in Quality mode**: the WSOLA core runs f32 in both
   precision modes — don't expect f64-grade changes from pitch/speed.
6. **Convolution IR + rate change**: the IR's frequency mapping goes stale
   when the session rate changes (`convolution_ir_needs_reload`); reload
   before trusting corrected/reverbed output.
7. **Drift correction + exact alignment**: drift trims by inserting/dropping
   frames per the device's real clock — the price of multi-device sync is
   that output is no longer bit-reproducible (still glitch-free).
8. **Voice budget + ultra quality**: raising quality without raising the
   full-quality sub-capacity will degrade voices instead of dropping them —
   both knobs are part of the same policy.
9. **Lane sends + aux**: lanes ride slots ≥ 2 and their aux sends are
   post-fader taps; `master_gain = 0` makes a sends-only lane, which is
   silent unless the aux return is enabled.
10. **Fallback policy + exclusivity**: the honest "cannot prove bit-perfect"
    reporting depends on `Allow`; under `Strict`, an unavailable exclusive
    device is an open *failure*, not a silent downgrade.

---

# 14. What the Engine Does Especially Well

Grounded in the implementation, not marketing:

1. **Determinism as a first-class property.** A graph-vs-pipeline bit-exact
   oracle, golden vectors, deterministic aelog replay, content-addressed
   caches, wall-clock-free logs, and a versioned quality harness with
   automatic regression detection. Few audio engines of any kind can prove
   "the output is byte-identical to last release" — this one can.
2. **"Disabled = absent" discipline.** Every feature skip is bit-exact,
   which is what makes the huge feature set safe to toggle at runtime and
   the equivalence suites stable across a decade of phases.
3. **Real-time safety enforced by tests, not promises.** Zero-allocation
   suites over the actual hot paths (DSP, spatial, correction, tracking),
   lock-free SPSC rings, generation swaps with deferred reclamation — the
   guarantees are machine-checked.
4. **DSP breadth with honest quality tiers.** 64-band EQ + graphic layer +
   AutoEQ, multiband compression, partitioned convolution, crossfeed,
   WSOLA, true-peak limiting, dither, R128 loudness, correction — and every
   tier is described by concrete parameters (taps, windows, dB) that tests
   verify.
5. **The spatial layer's separation of content from reproduction.** The
   same scene renders to stereo, 5.1, 7.1.4, custom arrays, or two ears;
   simulation (acoustic world) is separated from rendering (paths →
   renderers); heavy computation is baked (position-cached) so the
   realtime cost of a static room is a hash lookup. This is an
   architecturally clean way to build spatial audio.
6. **A real head-and-room model.** Woodworth ITD + Duda-Martens shadow +
   pinna notch + measured HRTF datasets; image-source reflections with
   spectral materials; Schroeder late fields; diffraction and transmission
   paths. It models physics instead of faking reverb.
7. **Multi-device output with drift correction.** Independent rings,
   nominal resampling, and a slip corrector steered to each device's real
   crystal, with ppm telemetry — a genuinely hard problem solved
   end-to-end.
8. **Modularity discipline.** Concern-split modules (graph split into
   construction/plan/swap/process/…; mix into mod/envelope/sum; command
   handlers by domain), explicit god-file rules in review, and small
   config crate — the codebase stays navigable at 52k lines.
9. **Honest capability reporting.** Bit-perfect is only claimed when
   verified; WavPack subsets are rejected with typed errors; SOFA nc4 is
   refused explicitly; DSD downgrades are reported step by step. The
   engine would rather tell you "cannot prove" than guess.
10. **A stable, complete host surface.** `EngineHandle` (Clone + Send,
    non-blocking), `PlaybackInfo` telemetry, `EngineEvent`/`OutputEvent`,
    `SampleSink` pluggability, and a NULL-safe C FFI covering even the
    advanced features (endpoints, aux insert, spatial health,
    diagnostics).
11. **The offline render laboratory.** Graph 2.0 + timeline + aelog turn
    the engine into a deterministic test bed for arbitrary signal
    networks — an unusual and powerful asset for R&D.
12. **Pure Rust, no FFI on the audio path.** Every codec and the DSP are
    auditable, cross-compilable, and free of native-library fragility;
    native backends use OS APIs through safe bindings only.

---

# 15. Remaining Gaps and Limitations

Classified honestly. Nothing below is manufactured — each item is grounded
in the code, its docs, or the platform constraints.

## Critical (should genuinely be fixed)

Honestly: **no critical defects were found in this review.** The engine is
internally consistent, heavily tested, and its documented invariants hold
in the code. The closest things to "must-fix" are process issues, not code
issues:

1. **Documentation drift (mostly resolved).** The README's test inventory
   ("42 test files, over 860 tests"), project-layout line ("fidelity/
   (26 suites)"), and chain summary (missing the final `spatial` plan step)
   have been corrected to match the verified numbers and the `plan.rs`
   order. Re-check other docs (e.g. the module map in `ARCHITECTURE.md`)
   on the same cadence as the tree changes.
2. **Uncommitted work-in-progress in the checkout** (42 modified files at
   the time of writing, including CHANGELOG and source) — an ownership/
   hygiene question, not a code defect.

## Important (materially improve robustness/completeness)

1. **Integrated room measurement is Windows-only in practice.** The
   portable IR-import path works everywhere, but the integrated sweep
   capture needs an input backend — today only WASAPI loopback exists. A
   generic input/capture backend would bring live measurement to
   Linux/macOS (the code explicitly marks this "Horizon").
2. **Native DSD transport coverage.** Native wire works on ALSA `hw:` only;
   WASAPI has no format, ASIO vendor extensions are unimplemented, and
   CoreAudio DoP is a documented target, not code. If DSD is a product
   priority, close the ASIO/CoreAudio seams.
3. **SpatialNode covers the stereo front pair only.** Multichannel masters
   pass through untouched (documented seam). Spatializing all channels or
   routing decoder object metadata into the node's scene is future work.
4. **Hardware volume coverage.** Native hardware endpoint volume exists on
   macOS; other backends rely on software gain or report `volume_error`.
   Hardware volume on Windows/Linux (where the OS offers it) would
   complete the VolumeMode contract.
5. **NetCDF-4/HDF5 SOFA** is refused (by design). If the market demands
   nc4 corpora, an optional feature with a contained C dependency (or a
   pure-Rust HDF5 subset) would close the gap without touching renderers.
6. **No soak/robustness tests** for hours-long drift, memory stability over
   days, or device-plug storms.
7. **Benchmark budgets are not CI-enforced** (criterion reports are
   reviewed manually).

## Optional (useful, not necessary)

1. **Order-4+ ambisonics** and **spatial recording** (a higher-order
   encoder for captured content). The math generalizes (documented);
   demand is niche.
2. **Per-wall materials in the live `Room`** (the old room uses one
   absorption coefficient; per-wall spectra exist in the acoustic world
   — a convergence seam).
3. **Generic input backend** beyond loopback (mic/line-in) — enables
   measurement everywhere and live monitoring.
4. **Loopback capture on macOS/Linux** (Windows-only today).
5. **More crossfeed profiles / limiter modes / graphic-EQ layouts** —
   the seams exist; add data, not architecture.
6. **`codec-dsd` feature cleanup**: it's an accepted no-op for API
   compatibility; a future major version could drop it.
7. **Property-based fuzzing** of graph/spatial parameters (decoders
   already fuzzed).

## Future research (do NOT treat as required features)

1. **A real content-classifier** (ML) behind the existing `ContentProfile`
   fields — the interface is already shaped for it; the engine should stay
   deterministic on the audio path.
2. **Per-listener HRTF personalization** (fitting datasets to individuals)
   — needs a measurement method first.
3. **Ray-traced / general-geometry acoustics** beyond axis-aligned
   image-source rooms.
4. **Cross-room portal networks in real time** (multi-room scenes) — the
   solver models portals; the renderers currently use one room per scene
   at a time.
5. **Head-tracking hardware drivers / sensor fusion** bundled with the
   engine (today the host supplies samples).
6. **Psychoacoustic masking-based EQ/compression** (uses the masking
   heuristics already computed in profiles).

---

# 16. Recommended Stopping Point

**Has the engine reached a reasonable point to transition from engine
development to product development? Yes — clearly.** The evidence:

- The feature surface is broad, coherent, and internally consistent; every
  design phase is marked Done.
- The real-time guarantees are machine-verified; the fidelity surface is
  pinned by ~1,300 tests and golden vectors; the API is versioned and
  stable (semver discipline with lockstep config crate, CHANGELOG, tags).
- The risk profile of *adding features* now exceeds the risk of *building
  products on what exists*: each new capability carries realtime-safety,
  determinism, and equivalence obligations.

## 16.1 What should still be fixed (before or during productization)

- The documentation drift (test counts, plan order, layout lines).
- Decide the WIP-commit question in the working tree.
- If DSD, spatial-on-multichannel, or integrated room measurement are
  product commitments, close those specific seams (Section 15.Important).
  Otherwise, leave them — they are honest, documented partials.

## 16.2 What should be left alone

- The realtime invariants: zero allocation/locks on the audio path,
  generation swaps, SPSC rings, atomic telemetry. Any change here is a
  regression risk to the engine's core promise.
- The "disabled = absent" discipline and the equivalence suites that pin
  it.
- The bit-exact vs processed distinction and its honest reporting.
- The two-crate layout and lockstep versioning.
- The no-god-files modularity rules.
- The offline analysis/render laboratory (Graph 2.0, timeline, aelog,
  eval, profile): it costs nothing at runtime and is a unique asset.

## 16.3 What features should NOT be added

- **UI, database, library management, streaming service logic** — that's
  the product layer; putting it in the engine recreates the god-object
  problem at product scale.
- **New renderers or new codecs** beyond the existing, unless a product
  requirement demonstrably demands them (each carries full fidelity/
  realtime obligations).
- **ML on the audio path** — keep intelligence offline and host-side.
- **MPMC queues, mutexes, or heap allocation on hot paths** — ever.
- **Dolby/DTS or other proprietary spatial formats** — the independent
  implementation is a deliberate legal/technical stance.
- **A general input backend** unless measurement/monitoring becomes a
  product feature (it's a big surface: a new subsystem, not a knob).

## 16.4 Risks of continuing to expand the core engine

- Each new DSP/spatial/codec feature must re-prove determinism,
  zero-allocation, and bit-exact disabling — the test surface grows
  superlinearly.
- The API grows; every public addition is a semver commitment (minor
  bump) and a C FFI obligation.
- More features mean more interactions (Section 13.2) — config validation
  must keep pace.
- Feature creep blurs the product boundary and the story: "the engine
  does everything" is a liability, not a pitch.

## 16.5 Where the engine/product boundary should be

**Engine owns:** playback, DSP, decoding, output, spatial rendering,
acoustics, measurement, analysis, deterministic rendering, telemetry,
configuration, FFI.

**Product owns:** UI, playlists/libraries/collections, artwork, browsing,
search, recommendation, accounts, DRM, device management UX, update
systems, session persistence of queues, ML-driven UX decisions (consuming
the engine's profiles, not running inside it).

## 16.6 What should become stable/public API

- The current `EngineHandle` surface, `EngineCommand`, `PlaybackInfo`,
  events, config types, and the C FFI (already semver-managed). Freeze
  them as the **contract**: changes become major-version events.
- The `SampleSink` trait (hosts build on it).
- The scene-file format and aelog format (already versioned; document
  them as stable interchange formats).
- `AudioProfile` as the analysis contract (versioned already).

## 16.7 What should remain internal

- Graph internals (arena, plans, generations, control queues) — behavior
  only, no types.
- Decoder internals and codec registry.
- Output backend implementations.
- Internal telemetry details not already exposed.

## 16.8 What types of future changes are justified

- **Patch/minor**: bug fixes, performance work, doc fixes, new commands/
  events/options that don't break anything, optional features behind
  gates, new profile sub-measures, new fidelity suites.
- **Major** (rare): breaking API cleanup (e.g. dropping the `codec-dsd`
  no-op), a new versioning milestone, boundary clarifications.
- **Product-side**: everything in 16.5.

**Bottom line:** stop growing the engine's feature surface; freeze the API;
polish the docs and the honest seams; and start building products on the
stable foundation.

---

# 17. Owner's Quick Reference

## What the engine is

Shadow Desktop is a headless, 100%-pure-Rust audio playback and DSP engine:
the complete, product-ready core of a music player or studio tool, with no
UI. It decodes nearly every common format, processes audio through a
studio-grade DSP chain, mixes multiple simultaneous streams, and outputs to
several devices at once with per-device clock-drift correction — all with
zero allocations and zero locks on the real-time audio path, verified by
automated tests.

It is also a serious spatial audio engine: objects, beds, and diffuse
fields in 3D world space rendered to any speaker layout (stereo → 7.1.4 →
custom) or binaurally to headphones, with a real head model, optional
measured HRTF datasets, room simulation (image-source reflections +
Schroeder late field), acoustic baking, and head tracking. And it is a
deterministic render laboratory: sessions can be recorded and replayed
bit-for-bit, so regressions are detected automatically.

It is **not** a UI, a library manager, a DAW, or a streaming service —
those are the product layers a host builds on top, via a clean Rust API or
the stable C FFI.

## What it can do (categorized)

- **Play**: FLAC, ALAC, WAV, AIFF, MP3, AAC, Vorbis, Opus, TTA, WavPack,
  APE, PCM, DSD (DSF/DFF, DSD64–DSD1024; native/DoP/PCM transports), from
  files, URLs (HTTP Range), or memory; gapless/crossfade/fade/stop
  transitions; queue with shuffle/repeat; independent lane tracks; seek;
  speed/pitch.
- **Process**: 64-band parametric EQ (+ graphic layer, AutoEQ, shelves,
  mid-side), 3-band compressor, convolution (reverb/IRs), crossfeed,
  stereo enhancement, WSOLA time-stretch/pitch, true-peak limiter, TPDF
  dither, EBU R128/ReplayGain normalization, room/headphone correction,
  f32/f64 precision modes.
- **Output**: ALSA / WASAPI exclusive / ASIO / CoreAudio hog / cpal;
  multiple endpoints with drift correction; bit-perfect (verified) and DoP
  bypass modes; DSD transport negotiation with explicit downgrades;
  hardware volume (macOS); device hotplug + recovery; per-device
  profiles.
- **Analyze**: realtime levels/spectrum; loudness (LUFS/dBTP/LRA); tag
  write-back; fingerprinting; full deterministic AudioProfile (loudness,
  dynamics, spectral, transient, stereo, spatial, content).
- **Spatial**: panner / VBAP / ambisonics (≤ order 3) / binaural
  renderers; HRTF datasets + SOFA (NetCDF-3); room simulation + acoustic
  baking; head tracking; spatial automation; scene persistence; voice
  budgeting; spatial health diagnostics.
- **Extend**: Graph 2.0 arbitrary topologies (offline), timeline/automation,
  aelog deterministic replay + cache, quality harness with regression
  detection.
- **Integrate**: Rust API, C FFI, pluggable sinks, serde config with
  validation + versioned migration.

## How audio flows through it

```
file/URL/memory ─▶ scanner ─▶ decoder ─▶ channel stage ─▶ mix bus ─▶
DSP plan (aux→correction→eq→dynamics→convolution→balance→crossfeed→
stereo→timestretch→volume→seek_fade→spatial) ─▶ resample ─▶ limiter ─▶
dither ─▶ rings ─▶ endpoint workers (drift-corrected) ─▶ DACs
```

## Major subsystems (one hierarchy)

```
Engine
├── Control: EngineHandle · commands · events · telemetry · FFI
├── Decode: scanner · codecs (Symphonia + native) · DSD · channel mix
├── Core DSP: graph (production) · pipeline (oracle) · EQ · dynamics ·
│            convolution · crossfeed · stereo · timestretch · correction
├── Output domain: resampler · limiter · dither
├── Output: backends (ALSA/WASAPI/ASIO/CoreAudio/cpal) · endpoints ·
│            drift · profiles · device monitor · capture
├── Mixing: mix bus (N slots) · aux bus · lanes · playlist
├── Spatial: scene/object/bed/field · renderers (panner/VBAP/ambisonic/
│            binaural) · HRTF · room/acoustic · tracking · node
├── Analysis: analyzer · loudness · profile · fingerprint · eval
├── Offline lab: graph2 · timeline · aelog · cache
├── Persistence: config (versioned) · scene files · caches
└── Testing: 58 test files (~1,300 tests) · 4 benches · CI matrix
```

## What makes it unusual (the top characteristics)

1. Zero-allocation / zero-lock real-time path, verified by tests.
2. Compiled DSP graph with glitch-free live reconfiguration (generation
   swap).
3. "Disabled = bit-exact absent" discipline everywhere.
4. Graph-vs-pipeline bit-exact oracle + golden vectors.
5. Deterministic session recording/replay (aelog) with content-addressed
   golden-render cache.
6. Versioned, content-addressed quality harness with automatic
   cross-version regression detection.
7. Bit-perfect only claimed when verified against the OS; honest
   downgrade reporting everywhere.
8. Multi-device output with per-endpoint clock-drift correction (ppm).
9. Independent spatial layer: content vs reproduction separated; same
   scene → any layout or binaural.
10. Real head model (Woodworth ITD + Duda-Martens + pinna) + measured
    HRTF datasets + SOFA import (NetCDF-3).
11. Acoustic simulation separated from rendering, with position-cached
    baking.
12. 100% pure Rust (no native codec SDKs; safe OS bindings only).
13. Double-precision Quality mode with documented exceptions.
14. Honest partial support (typed rejections, no silent downmixes).
15. Stable C FFI covering advanced features.

## Known limitations (genuine ones only)

- Integrated room measurement capture: WASAPI loopback (Windows) only;
  other platforms import IRs.
- Native DSD wire: ALSA `hw:` only; DoP on WASAPI/ASIO; CoreAudio DoP
  unimplemented.
- SpatialNode spatializes the stereo front pair only (MC passthrough).
- SOFA nc4/HDF5 refused (deliberate, pure-Rust rule).
- WavPack multichannel/DSD/hybrid rejected (typed error).
- Hardware volume: macOS only among native backends.
- System capture: Windows only.
- WSOLA core and final limiter are f32 in both precision modes.
- Bit-perfect is conditional (exclusive + no SRC + bypass).
- Graph 2.0/timeline/aelog are offline-only.
- Content classification is heuristic, not ML.

## What should not be changed casually (architectural invariants)

1. No allocation / no locks on the real-time audio path.
2. SPSC rings + atomics only for hot-path communication.
3. Generation-swap reconfiguration with deferred reclamation.
4. Disabled stages are literally absent (bit-exact).
5. The plan order in `plan.rs` is the single source of truth for the
   chain.
6. Bit-perfect/DoP bypass returns before any stage runs.
7. Engine + config crates stay in lockstep versions.
8. No god files; concern-split impls; module map in ARCHITECTURE.md stays
   true.
9. Determinism: no wall-clock dependence in logs/render identities.
10. Typed rejections over silent degradation.
11. Realtime rules documented in AGENTS.md are review-enforced.

## Where to look when something breaks

| Symptom | Likely subsystem | Key sources | Relevant tests |
|---|---|---|---|
| Clicking/popouts, underruns | Decode loop, output domain, endpoint | `engine/decode_loop/`, `dsp/graph/limiter.rs`, `output/endpoint.rs` | `realtime_allocation`, `ring_buffer_stress` |
| Wrong speed/pitch | Resampler | `dsp/resampler/`, `engine/stream.rs` | `resampler_quality`, `resampler_measurement` |
| Clipping despite limiter | Limiter config, true peak | `dsp/limiter.rs`, `dsp/true_peak.rs` | `limiter_correctness`, `limiter_measurement` |
| Track won't open | Decoder/scanner | `decode/scanner.rs`, `decode/decoder.rs`, `decode/codecs.rs` | `decoder_robustness`, `fuzz_mutation` |
| Wrong channel count | Channel policy/mix | `decode/channel_layout.rs`, `channel_mix.rs`, `config` enums | `multichannel_graph` |
| Gapless/crossfade broken | Stream/track loading | `engine/stream.rs`, `engine/crossfade.rs`, `engine/track_loading.rs` | `crossfade_gapless`, `gapless_seek`, `transition_tails` |
| No sound / device lost | Output/backends | `output/*`, `engine/output_setup.rs`, `engine/recovery.rs` | `headless_playback`, `output_profiles` |
| DSD not native | DSD transport | `engine/dsd_state.rs`, `decode/dsd/`, `output/*` | `decode/dsd/tests.rs` |
| Spatial sounds wrong | Spatial layer | `spatial/` (renderer of choice), `dsp/graph/nodes/spatial_node.rs` | `spatial_*` suites |
| Room/reverb wrong | Acoustic layer | `spatial/room.rs`, `spatial/acoustic/` | `spatial_room`, `acoustic_world`, `acoustic_bake` |
| EQ/compressor differs | DSP stages | `dsp/equalizer/`, `dsp/multiband_compressor.rs`, `dsp/graph/plan.rs` | `eq_*`, `graph_pipeline_equivalence` |
| Correction off | Correction pipeline | `dsp/correction/`, `nodes/correction_node.rs` | `ess_measurement`, `correction_inverse`, `room_correction_pipeline` |
| Regression in quality | Eval harness | `src/eval/` | `quality_harness` |
| Deterministic replay broken | Aelog | `dsp/aelog/` | `aelog_*` suites |
| Latency misreported | Latency accounting | `playback_info.rs`, `dsp/graph/plan.rs`, `dsp/graph2/latency.rs` | `latency_alignment`, `transition_tails` |

---

# 18. Glossary for a Non-Technical Owner

Each term: plain-language meaning, then one sentence on how it relates to
THIS engine.

| Term | Meaning | In this engine |
|---|---|---|
| **DSP** | Digital Signal Processing — mathematically manipulating a stream of numbers that represent sound | The whole `dsp/` tree; every effect is DSP |
| **PCM** | Pulse-Code Modulation — the standard way to represent audio as numbers (samples) | The engine's universal internal format: all decoders output PCM |
| **Sample rate** | How many samples per second (Hz); 44.1/48/96/192 kHz are common | Decoded native rate, resampled to the device rate when needed |
| **Bit depth** | How many bits per sample (16/24/32); more bits = more dynamic range | f32 (24-bit class) internally; converted at the device boundary; f64 in Quality mode |
| **LUFS** | Loudness Units Full Scale — perceived loudness per the EBU R128 standard | The engine measures and normalizes to it; can write it into tags |
| **True peak / dBTP** | The real peak level including between-sample overshoots | A 4× oversampled FIR detector feeds the limiter |
| **Dynamic range** | The gap between loudest and quietest parts | Measured as LRA (loudness range) and crest factor in profiles |
| **Convolution** | Multiplying two signals to apply one's "character" to the other — how reverb IRs work | Partitioned-FFT engine used for reverb, correction, and graph nodes |
| **FIR / IIR** | Finite / Infinite Impulse Response — two filter families; FIR is exact but longer, IIR is cheap | Biquads (IIR) for EQ/occlusion; FIR for convolution, HRIRs, true-peak |
| **FFT** | Fast Fourier Transform — converts between time and frequency domains quickly | Powers convolution, spectral analysis, profile spectral features |
| **HRTF** | Head-Related Transfer Function — how your head and ears alter sound from each direction | The binaural path uses the analytic head model or measured datasets |
| **HRIR** | The time-domain version of an HRTF (an impulse response per direction) | Stored in `HrtfDataset` grids; interpolated bilinearly |
| **SOFA** | A file standard (.sofa) for sharing measured HRTF data | Imported (NetCDF-3 subset) into the engine's corpus format |
| **VBAP** | Vector-Based Amplitude Panning — placing a sound by solving which speakers surround it | `VbapRenderer` with triplet solves + 2D reduction + fallback |
| **HOA / Ambisonics** | A way to encode an entire sound field (not per-speaker signals) that any layout can decode | Orders 1–3 implemented with exact basis + rotation + max-rE |
| **Binaural** | Two-ear rendering that recreates 3D spatial cues over headphones | `BinauralRenderer` renders the whole scene to L/R ears |
| **ITD** | Interaural Time Difference — sound arrives at the nearer ear first | Woodworth formula gives the delay per direction |
| **Head shadow** | The head blocks/alters high frequencies from the far side | Duda-Martens shelf filter models it |
| **Occlusion** | A wall between source and listener muffling sound | Attenuation + low-pass with smoothed cutoff |
| **Diffraction** | Sound bending around edges | Wedge-diffraction paths around fins and door jambs |
| **Transmission** | Sound passing through a wall | Per-band material transmission spectra + portals |
| **Directivity** | A source louder in some directions (like a speaker) | Cardioid/supercardioid/custom curves |
| **Doppler** | Pitch change from relative motion | Live per-block from source vs listener velocity |
| **Impulse response (IR)** | The "signature" of a room/device — its response to a single click | Used for reverb, correction, and HRTF data |
| **Latency** | Delay between cause (play/input) and audible effect | Reported end-to-end in `latency_ms`; position compensated |
| **Jitter** | Timing wobble between samples | The drift corrector handles slow clock offset; jitter is the device's problem |
| **Drift correction** | Continuously matching the stream to a device's slightly-off clock | Per-endpoint `Slip` trims ±500 ppm, reported in ppm |
| **Exclusive mode** | Bypassing the OS mixer for direct device access | ALSA hw:, WASAPI exclusive, CoreAudio hog, ASIO |
| **Bit-perfect** | Output bits identical to source bits | Only claimed when exclusive + no SRC + DSP bypass all verified |
| **DoP** | DSD-over-PCM — wrapping 1-bit DSD in 24-bit PCM frames | A pure passthrough bypass mode; no volume, no DSP |
| **DSD** | Direct Stream Digital — 1-bit audio at very high rates | DSF/DFF decoding, native/DoP/PCM transports, DSD64–DSD1024 |
| **Decimation** | Converting 1-bit DSD to multi-bit PCM | The `DsdToPcmDecimator`; the safe default transport |
| **WSOLA** | Waveform-Similarity Overlap-Add — time-stretching without pitch change | The timestretch/pitch engine; 3 quality tiers |
| **Crossfeed** | Feeding a little of the left channel to the right ear (and vice versa) to simulate speakers on headphones | Bauer / Chu Moy / J. Meier / custom |
| **Dither** | Adding shaped noise to mask quantization errors at low levels | TPDF dither at the integer boundary |
| **Graph compilation** | Turning a graph of effects into an ordered execution plan once, not every block | `PlanSet::compile`; the plan is data the audio path only reads |
| **Generation swap** | Replacing the whole processing graph live, atomically, at a block boundary | The Phase-2 reconfiguration mechanism |
| **SPSC ring** | Single-Producer Single-Consumer ring buffer — a lock-free pipe | Every audio pipe in the engine (rings, control queues) |
| **Cache** | Reusing precomputed results | Acoustic bakes, loudness scans, profiles, golden renders (content-addressed) |
| **Deterministic rendering** | Same input ⇒ byte-identical output, every time | aelog replay + golden captures + fixed evaluation stimuli |
| **Telemetry** | Live status data | `PlaybackInfo` published atomically every tick |
| **FFI** | Foreign Function Interface — calling one language from another | The stable C API (`c-ffi` feature) |
| **Loudness range (LRA)** | How much a program's loudness varies over time | Measured (10th–95th percentile) in scans and profiles |
| **BS.1770 / EBU R128** | The international standards for measuring loudness | The meter and normalization implement them exactly |
| **Sinc resampler** | Sample-rate conversion using a mathematically ideal low-pass (sinc) filter | Rubato-based; quality tiers choose filter length |
| **BS.775** | The standard for mapping multichannel audio to speakers | The downmix/mix templates follow it |

---

# 19. Developer/AI Handoff Appendix

This section is for future developers and coding agents. It is deliberately
technical and assumes Rust familiarity. **Always re-read `AGENTS.md` first**
— it is the binding project constitution (versioning rules, god-file
signals, realtime rules, completeness checklist).

## 19.1 Repository architecture

- Cargo workspace with two crates: `engine` (root, everything under `src/`)
  and `config` (`crates/config/`). **Versions must stay in lockstep**;
  every release bumps both + CHANGELOG + tag.
- `src/lib.rs` is the public surface: `pub mod` for everything, a `prelude`
  re-exporting the commonly used types.
- Feature-gated modules: `engine`/`output` (`audio-output`), `ffi`
  (`c-ffi`), codecs (`codec-*`), `sofa` (`sofa-import`), capture paths
  (`wasapi-native`, Windows), ASIO (`asio-native`, Windows), network
  (`network-streaming`), tags (`tag-write`), fingerprint (`fingerprint`),
  resample (`resample`).
- Binaries: `audio-engine-cli`, `replaygain-scanner` (requires
  `tag-write`), `aelog-replay`.
- Tests: `tests/` (headless integration), `tests/fidelity/` (55 named
  `[[test]]` suites, each registered in Cargo.toml), in-crate unit tests.
- Benchmarks: `benches/` (4 Criterion harnesses, `harness = false`).

## 19.2 Core public types

| Area | Types |
|---|---|
| Control | `EngineCommand` (one-way), `EngineHandle` (Clone+Send bridge), `EngineConfig` |
| Telemetry | `PlaybackInfo` (ArcSwap-published), `PlaybackState`, `LaneInfo`, `EndpointInfo`, `SpatialTelemetry`, `CorrectionInfo` |
| Events | `EngineEvent`, `OutputEvent` |
| Diagnostics | `Diagnostic`, `DiagnosticKind`, `BitPerfectCause` |
| Sources/sinks | `AudioSource` (File/Uri/Memory), `SampleSink` trait, `DacSink`/`NoopSink`/`VecSink` |
| Decode | `Decoder`, `Codec`, `DecodedChunk`, `TrackMetadata`, `LoudnessScanResult`, DSD types |
| DSP | `DspGraph` (production), `DspPipeline` (reference oracle), `GraphControlHandle`, plan/node types |
| Spatial | `SpatialScene`, `SpatialRenderer` trait, `BasicPanner`, `VbapRenderer`, `AmbisonicRenderer`, `BinauralRenderer`, `HrtfDataset`, `AcousticWorld`, `BakedScene` |
| Offline | `Graph2`, `OfflineExecutor`, `Timeline`, `AudioClock` (timeline), `Aelog`, `AelogRecorder`, `AelogCache`, `EvaluationReport` |
| Analysis | `AudioProfile`, `AnalysisMask`, `ProfileAnalyzer` |

## 19.3 Core traits / interfaces

- `SampleSink::push_interleaved(&[f32], channels) -> usize` — the output
  contract; must be allocation-free in steady state; returns accepted
  **frames** (engine retries the tail).
- `DspNode` (graph arena nodes) — capability metadata + process + control.
- `SpatialRenderer` — `prepare` (allocation) vs `process_block` /
  `process_hybrid_block` (zero-alloc), `RendererKind`, `RenderError`.
- `Output` trait + backend factory (`output.rs`) — the device abstraction.
- `AudioByteSource` — the byte-reader abstraction (file/URI/network).
- FFI: `pub extern "C" fn engine_*` opaque-handle API in `src/ffi.rs`
  (status-code contract; NULL-safe; no panics cross).

## 19.4 Major data flows

```
Host → EngineCommand (bounded crossbeam) → tick loop → per-domain handlers
      → graph control queues (SPSC) → applied at block boundary

Decode loop: decoder → channel stage → MixBusNode (slots) → AuxBusNode →
plan steps (correction/eq/dynamics/convolution/balance/crossfeed/stereo/
timestretch/volume/seek_fade/spatial) → output domain (resample → limiter
→ dither) → FixedFrameBuffer rings → endpoint workers → devices

Telemetry: tick publishes PlaybackInfo via ArcSwap::rcu(); hosts load().

Offline: Graph2 build → validate → compile (topo sort) → OfflineExecutor
render; Timeline → advance_block → events with in-block indices →
set_gain_step; AelogRecorder logs every mutation → replay_render →
byte-identical capture → AelogCache (SHA-256 content address).
```

## 19.5 Threading model

- One **tick thread** (host-owned: `tick_blocking` loop, the FFI's internal
  thread, or the CLI). Owns ALL mutable engine state.
- **Per-endpoint realtime threads** (backend callbacks): read only their
  ring, resampler/slip, and the shared graph read-only.
- **Background workers**: loudness scans, device monitor (hotplug →
  `AutoRecoverStream`), capture loopback fill.
- **Host threads**: any thread via `EngineHandle` (non-blocking) +
  `PlaybackInfo` reads (wait-free).

## 19.6 Real-time invariants (non-negotiable)

1. No heap allocation on the decode/DSP/output-callback hot paths (tested:
   `realtime_allocation.rs`).
2. No locks on hot paths — atomics + cache-padded SPSC rings only.
3. Shared audio-thread state (e.g. `AuxSendBus`) is safe **by contract**
   (same audio thread both sides); `unsafe impl Sync` documented next to
   the contract.
4. Reconfiguration = build fresh `GraphGeneration` on control thread →
   atomic publish at block boundary → deferred reclamation on control
   thread. User state mirrors to sticky atomics and replays on swap.
5. Denormal flushing at stage boundaries.
6. Disabled features are bit-exact absent (never reordered, never ×1.0
   placeholder in the wrong place).
7. Plan order (`dsp/graph/plan.rs::PlanSet::compile`) is the single source
   of truth — changing it breaks the equivalence suite.

## 19.7 Configuration model

- `config::EngineConfig` — serde struct with `#[serde(default)]` everywhere
  forward-compatible; `validate() -> ConfigValidation` (typed `ConfigIssue`
  with stable `kind.code()`); presets `Consumer`/`Fidelity`; versioned
  envelope `VersionedConfig` (schema v1; `migrate_step` is the forward
  migration table; legacy bare payloads load as current).
- Runtime mirror: `EngineCommand` variants per knob; graph nodes have
  per-node atomic control mirrors + SPSC queues; generation state carries
  config across swaps.

## 19.8 Error model

- Typed errors via `thiserror` (`DecodeError`, `Graph2Error`, `RenderError`,
  `ProfileError`, `AelogError`, `HrtfLoadError`, `SofaImportError`,
  `ConfigLoadError`, `EngineError`…).
- Telemetry: `engine_error: Option<String>` + `engine_diagnostics:
  Vec<Diagnostic>` (typed kind + human message).
- Counters: clips, NaNs, underruns, CPU overloads, deadline misses,
  endpoint drops/transport errors — u64, reset-on-read for clip/NaN.
- Convention: **typed rejection over silent degradation** (WavPack subset,
  SOFA nc4, DSD downgrades reported step-by-step in `DsdTransportReport`).
- Sink contract: never panic on valid input; return accepted frames.

## 19.9 Test organization

- `cargo test` — units + headless. `cargo test --test <suite>` for one
  fidelity suite. Key suites: `graph_pipeline_equivalence` (bit-exact
  oracle), `realtime_allocation`, `quality_harness`, `ring_buffer_stress`,
  `fuzz_mutation`/`decoder_robustness`, `aelog_*`, `spatial_*`,
  `acoustic_*`, `latency_alignment`, `timeline_scheduler`.
- CI (`ci.yml`): fmt, clippy -D warnings, test matrix across Linux/macOS/
  Windows, cross-target compile of native backends.
- Benchmarks: `cargo bench` (criterion). Fidelity thresholds are written
  as assertions with committed numbers (see the Phase-7 acceptance list in
  EVOLUTION.md for the style).
- Golden/reference discipline: deterministic stimuli, versioned vectors,
  content-addressed expectations (`eval`), golden captures (`aelog`).

## 19.10 Serialization formats

- **Config**: JSON (`VersionedConfig` envelope: `{version, …flattened}`).
- **Spatial scenes**: JSON via `save_scene_json`/`load_scene_json`
  (`SpatialSceneConfig`), renderer-independent, validated against caps.
- **aelog**: versioned JSON (`SessionHeader` + `RecordedCommand` list;
  `AELOG_VERSION` = 3; additive variants; no wall-clock fields).
- **Graph 2.0**: JSON round-trip (`Graph2` serde); Graphviz via `to_dot`.
- **HRTF corpora**: JSON (`save_hrtf_corpus_json`/`load`); SOFA import
  from NetCDF-3 classic.
- **Profiles**: JSON (`AudioProfile`, version-gated cache).
- **Cache entries**: aelog golden captures named by SHA-256 content
  address (LRU-bounded, `touched` stamps excluded from the address).
- **Config/auto-save**: spatial scene at `<data_local_dir>/engine/
  spatial_scene.json` (override via `spatial_autosave_path`).

## 19.11 Important feature flags

Default: `audio-output`, `resample`, `all-codecs`. Notable: `wasapi-native`
(Windows exclusive + loopback), `asio-native` (Windows ASIO), `c-ffi`,
`sofa-import`, `network-streaming`, `tag-write`, `fingerprint`,
`codec-opus`/`codec-tta`/`codec-wavpack`/`codec-ape` (native decoders).
`codec-dsd` is an accepted **no-op** (DSD compiled unconditionally).
`all-codecs` includes `codec-musepack`, but **no Musepack decoder is wired
in**: `Codec::Musepack` is declared as `DeclaredUnavailable` and `.mpc`/
`.mp+`/`.mpp` files are rejected at open with an explicit "not available in
this build" error (the code docs name a planned FFI-to-libmpcdec path that
was never taken). Treat it as a declared-but-unavailable codec.

## 19.12 Extension points (safe to add to)

- New `EngineCommand` variant + handler in `src/engine/commands/` (minor
  bump).
- New graph node: `dsp/graph/nodes/` + plan step if needed (must be
  disabled-exact; extend the equivalence suite).
- New codec: register in `decode/codecs.rs`, implement behind a `codec-*`
  feature, add robustness tests.
- New spatial renderer/behavior: `src/spatial/` behind the
  `SpatialRenderer` trait.
- New output backend: implement `Output`, register in factory + features.
- New analysis: extend `AudioProfile` sub-profiles (`AnalysisMask`),
  `eval` suites (register in `ReferenceVectorRegistry::build` +
  `run_quality`; bump `EXPECTED_SUITES`).
- New aelog command: additive `RecordedCommand` variant (keep
  `AELOG_VERSION` semantics), recorder + replay + cache identity coverage.

## 19.13 Areas requiring caution

- **Anything on the audio path**: run `realtime_allocation`; keep
  zero-alloc/zero-lock; extend it for new paths.
- **The plan order** and the equivalence suite: changing stage order or
  bypass semantics breaks `graph_pipeline_equivalence`.
- **`PlaybackInfo` / `EngineEvent` / FFI signatures**: public, semver-
  managed, and mirrored in the C FFI — additive changes only.
- **DSD native/DoP paths**: platform-verified; changes must be tested per
  backend and keep `DsdTransportReport` honest.
- **Spatial renderers**: mirror-symmetry and energy invariants are tested;
  any new path must stay allocation-free and disabled-exact.
- **Drift correction**: the slip controller is tuned; don't change the
  control law without endpoint stress testing.
- **Config defaults**: `#[serde(default)]` means old files silently adopt
  new defaults — change defaults deliberately (versioned envelope exists
  for real migrations).

## 19.14 Architectural invariants (do not violate)

See 19.6 plus: lockstep versioning; no god files (review signal: >800–1000
lines + multiple unrelated concerns + plumbing methods); module map in
`ARCHITECTURE.md` stays accurate; Apache-2.0 metadata consistent across
both crates + README + `LICENSE-APACHE`; `cargo fmt`/clippy -D warnings
green.

## 19.15 Known technical debt

- Documentation drift (mostly fixed): the README's test counts, layout
  lines, and chain summary now match the code; keep `ARCHITECTURE.md`'s
  module map in sync as the tree changes.
- `codec-dsd` no-op feature and `codec-musepack` placeholder — API
  compatibility baggage, cleanup candidates for a major version.
- Old `Room` (single absorption) vs `AcousticRoom` (per-wall spectra) —
  a convergence seam documented in code.
- `acoustic_taps` broadband reduction is test-only after the v3.40 per-path
  spectral filtering.
- README/ARCHITECTURE module maps lag the actual tree (e.g. `profile/`
  not in the top-level map); keep the maps in sync going forward.

---

*End of the Owner's Guide. For day-to-day rules, see `AGENTS.md`; for the
module map, `docs/ARCHITECTURE.md`; for the exact sample path,
`docs/SIGNAL_FLOW.md`; for embedding examples, `docs/EMBEDDING.md`; for the
quality methodology, `docs/QUALITY.md`; for the design history,
`docs/EVOLUTION.md`.*

