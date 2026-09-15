# Shadow Desktop Engine Specification (`ENGINE_SPEC.md`)

**Document Version**: 5.2.0  
**Specification Status**: Authoritative Engineering Contract  
**Standard**: Strict ISO / ITU-R / EBU Audiophile Architecture

---

## Table of Contents

1. [Audio Model](#1-audio-model)
2. [Thread Model & Concurrency](#2-thread-model--concurrency)
3. [Real-Time Guarantees](#3-real-time-guarantees)
4. [Buffer Model & Ring Buffers](#4-buffer-model--ring-buffers)
5. [Precision Model](#5-precision-model)
6. [Graph Semantics (Graph 2.0)](#6-graph-semantics-graph-20)
7. [Latency Semantics & PDC](#7-latency-semantics--pdc)
8. [Tail Semantics](#8-tail-semantics)
9. [Automation Semantics](#9-automation-semantics)
10. [Plugin ABI](#10-plugin-abi)
11. [Spatial Coordinate System](#11-spatial-coordinate-system)
12. [Channel Ordering & Speaker Layouts](#12-channel-ordering--speaker-layouts)
13. [Loudness Standards](#13-loudness-standards)
14. [Output Semantics & Multi-Endpoint Matrix](#14-output-semantics--multi-endpoint-matrix)
15. [Diagnostics & Telemetry](#15-diagnostics--telemetry)
16. [Error Semantics & Failure Recovery](#16-error-semantics--failure-recovery)
17. [Serialization & State](#17-serialization--state)
18. [Determinism Guarantees](#18-determinism-guarantees)
19. [Compatibility Guarantees](#19-compatibility-guarantees)
20. [Security Model](#20-security-model)
21. [Testing & Verification Requirements](#21-testing--verification-requirements)

---

## 1. Audio Model

### 1.1 Fundamental Units
- **Sample**: A discrete scalar representation of signal amplitude at a point in time, typed as `f32` (performance domain) or `f64` (reference/precision domain).
- **Frame**: A synchronous temporal slice across all active channels. A frame contains exactly $C$ samples, where $C$ is the active channel count.
- **Block**: An atomic processing quantum consisting of $N$ contiguous frames. Standard block sizes are powers of two ($N \in [16, 1024]$), with nominal $N = 256$ frames.

### 1.2 Layout Architecture
- **Interleaved Audio**: Hot audio rings and external backend boundaries pass audio in interleaved format: $[L_0, R_0, L_1, R_1, \dots, L_{N-1}, R_{N-1}]$.
- **Planar Audio**: Internal SIMD vectorized stages and Higher-Order Ambisonic (HOA) processors unpack interleaved frames into contiguous per-channel planes: $[[L_0 \dots L_{N-1}], [R_0 \dots R_{N-1}]]$.

### 1.3 Sample Rates
- The engine natively supports continuous sample rates from $44.1\,\text{kHz}$ to $384.0\,\text{kHz}$ (including integer and fractional multiples of $44.1\,\text{kHz}$ and $48.0\,\text{kHz}$).
- Direct Stream Digital (DSD) bitstreams are decoded natively (DoP / DSD over PCM or native raw bitstreams up to DSD512).

---

## 2. Thread Model & Concurrency

The audio engine strictly enforces a multi-tier decoupled thread topology:

```text
Host / UI Thread
      │ (EngineCommand via crossbeam channel)
      ▼
Control Thread (Engine State Machine)
      │ (Atomic swap, preallocated memory, lock-free SPSC)
      ▼
Audio Render Thread (DSP Graph Execution)
      │ (Lock-free SPSC audio rings)
      ▼
Endpoint Realtime Threads (1 per output device)
```

1. **Control Thread**: Owns playlist management, track decoding initiation, metadata parsing, graph topological compilation, and parameter validation. Heap allocation and file I/O are permitted **only** on this thread.
2. **Audio Render Thread**: Drives the compiled DSP execution plan (`RtPlan`). Operates on pre-allocated scratch planes. Runs synchronously or on timer/ring triggers.
3. **Endpoint Realtime Threads**: Each configured physical DAC/audio endpoint has a dedicated real-time thread running at OS audio priority (`SCHED_FIFO` / `THREAD_PRIORITY_TIME_CRITICAL`). Each endpoint reads from its own dedicated lock-free SPSC ring buffer and applies drift-corrected resampling.

---

## 3. Real-Time Guarantees

Hot audio paths (audio render thread, endpoint callbacks, decode loop hot loops) adhere to the following non-negotiable invariants:

1. **Zero Heap Allocation**: Never invoke `malloc`, `realloc`, `free`, `Vec::push`, `Box::new`, or any allocating construct during audio rendering. All buffer planes, delay lines, and scratch spaces are pre-allocated during control-side compilation.
2. **Zero Blocking Locks**: No `Mutex`, `RwLock`, futex, or thread parking construct may be acquired on the audio path. Synchronization is restricted to cache-line padded lock-free SPSC queues (`RingBuffer`), atomic pointers (`AtomicPtr`), and atomic integer primitives (`AtomicU64`, `AtomicBool`).
3. **Zero Filesystem and Network I/O**: No synchronous file reads, tag writes, socket communications, or telemetry flushes.
4. **Bounded Execution**: No unbounded iterative loops or unbounded convergence algorithms. All filter processing, convolution partitioning, and math evaluations are $O(N)$ with static iteration bounds.
5. **Denormal Suppression**: Audio threads configure processor registers (`MXCSR` on x86, `FPCR` on ARM) to Flush-to-Zero (FTZ) and Denormals-Are-Zero (DAZ).

---

## 4. Buffer Model & Ring Buffers

### 4.1 Maximum Channel Capacity
The physical channel capacity is bounded at compile-time by `MAX_CHANNELS = 16`. This accommodates up to 9.1.6 Dolby Atmos / ITU-R BS.2051 System J speaker configurations.

### 4.2 Lock-Free SPSC Rings
- SPSC (Single Producer, Single Consumer) ring buffers utilize power-of-two capacities with bitmask index wrapping.
- Head and tail pointers are stored on separate 64-byte cache lines with explicit `#[repr(align(64))]` padding to prevent false sharing.
- Atomic ordering: Producers write payload data with `Relaxed` ordering, followed by a `Release` write to the head index. Consumers read payload data after an `Acquire` load of the head index.

---

## 5. Precision Model

The engine operates under an explicit dual-precision floating-point architecture:

1. **Audio Path Precision (`AudioFloat`)**:
   - High-throughput vector stages: Single precision IEEE 754 `f32` (24-bit mantissa, $>144\,\text{dB}$ dynamic range).
   - Precision accumulators & filter state: Double precision IEEE 754 `f64` (53-bit mantissa, $>300\,\text{dB}$ dynamic range).
2. **Filter State Recursion**: All second-order IIR biquad sections (EQ, K-weighting pre-filters) maintain internal delay states ($z_1, z_2$) in `f64` using Transposed Direct Form II (DFII-T). This eliminates coefficient quantization noise, limit cycles, and low-frequency accumulation drift.
3. **Integer Sample Formats**: Decoded 16-bit, 24-bit, and 32-bit integer PCM is converted to normalized floats in $[-1.0, 1.0]$. Dithering is applied prior to requantization on output.

---

## 6. Graph Semantics (Graph 2.0)

### 6.1 Topology Model
- The graph is defined as a directed acyclic multigraph $G = (V, E)$, where $V$ is the set of nodes (`NodeDef`) and $E$ is the set of directed edges (`EdgeDef`).
- Every node declares typed input and output ports (`PortSpec`) carrying a specific `SignalType` (`Audio`, `Control`, `Acoustic`) and channel count.
- Dynamic graph topology is strictly verified for acyclicity using depth-first search (DFS) with grey/white/black cycle detection.

### 6.2 Topological Compilation
- Execution order is derived deterministically via Kahn's algorithm with ascending `NodeId` tie-breaking.
- Execution plans are lowered to immutable, pre-allocated `RtPlan` structures.

### 6.3 Transactional Editing
Graph modifications follow a transactional contract:
```text
begin_transaction -> modify -> validate -> PDC calculate -> allocate RtPlan -> warm up -> atomic swap
```
If validation or compilation fails at any stage, the transaction aborts with `TransactionError` and the active graph continues running without interruption.

---

## 7. Latency Semantics & PDC

### 7.1 Latency Classification
Every node reports deterministic latency decomposed into:
- **Intrinsic Latency**: Algorithm group delay (e.g. linear-phase FIR).
- **Lookahead Latency**: Brickwall limiter lookahead buffer delay ($5.0\,\text{ms}$).
- **Plugin Latency**: Reported VST/native plugin processing delay.
- **Resampler Latency**: Polyphase Kaiser-windowed sinc half-filter delay.
- **Convolution Latency**: FFT partition block size ($256$ frames).
- **Device Latency**: Physical hardware DAC DMA buffer size.

### 7.2 Plugin Delay Compensation (PDC)
When parallel signal paths reconverge at a summing junction (`NodeKind::Mix`), the engine calculates cumulative path latencies and automatically inserts compensation delay nodes (`NodeKind::Delay`) on the faster branches, ensuring sample-accurate phase alignment.

---

## 8. Tail Semantics

1. **Tail Definition**: Time in samples required for a stage's energy to decay below $-90\,\text{dBFS}$ after input excitation ceases.
2. **Reverb & Delay Tail Preservation**: When a node is bypassed or playback enters crossfade/stop, nodes with active tails are maintained in processing until their declared tail duration elapses, preventing clicks or unnatural truncation.
3. **Flushing**: Resetting a graph zeroes all internal delay buffers, ring buffers, and biquad states.

---

## 9. Automation Semantics

1. **Parameter Curves**: Parameters support linear, exponential, logarithmic, and S-curve smoothing.
2. **Block-Rate vs Sample-Accurate**:
   - Volume and pan transitions are evaluated with sample-accurate linear interpolation ramps across block boundaries.
   - Heavy structural parameters (filter coefficients, convolution impulse responses) update once per block boundary using parameter smoothing filters to prevent zipper noise.
3. **Timeline Scheduling**: Events are scheduled on a monotonic audio sample clock (`AudioClock`).

---

## 10. Plugin ABI

The engine implements a pure-Rust, versioned C-ABI plugin standard:
1. **Memory Ownership**: Plugins never allocate memory inside audio callbacks. Scratch buffers are owned and supplied by the host.
2. **Safe Facade**: The host wraps foreign plugin vtables in a safe Rust facade, validating buffer bounds, channel configurations, and parameter sanity.
3. **Fault Isolation**: Plugins reporting non-finite values or violating latency bounds are automatically bypassed by the host.

---

## 11. Spatial Coordinate System

### 11.1 Coordinate Frame
The spatial engine adheres to the **Right-Handed Cartesian** convention (ISO 2631 / Audio Engineering standard):
- $+X$: To the listener's **Right**.
- $+Y$: Directly **Forward** (straight ahead).
- $+Z$: Directly **Up** (towards the ceiling).
- Origin $(0, 0, 0)$: Center of the listener's head.

### 11.2 Polar Conventions
- **Azimuth** ($\theta$): Angle in the horizontal ($XY$) plane, measured in radians, positive counter-clockwise ($0 = \text{Forward } (+Y)$, $+\frac{\pi}{2} = \text{Left } (-X)$, $-\frac{\pi}{2} = \text{Right } (+X)$).
- **Elevation** ($\phi$): Angle above/below the horizontal plane ($0 = \text{Horizon}$, $+\frac{\pi}{2} = \text{Zenith } (+Z)$, $-\frac{\pi}{2} = \text{Nadir } (-Z)$).
- **Distance** ($r$): Euclidean radius in meters ($r = \sqrt{x^2 + y^2 + z^2}$).

### 11.3 Higher-Order Ambisonics (HOA)
- Ordering: Ambisonic Channel Number (**ACN**), where index $i = l(l + 1) + m$.
- Normalization: Schmidt Semi-Normalized (**SN3D**) per ITU-R BS.2076 ADM and MPEG-H 3D Audio, with Max-rE decoding options.

---

## 12. Channel Ordering & Speaker Layouts

Interleaved channel order strictly follows ITU-R BS.775, ITU-R BS.2051, and SMPTE ST 2036 standards:

| Layout | Channels | Standard Interleaved Channel Order |
|---|---|---|
| **Mono** | 1 | Center |
| **Stereo** | 2 | Left, Right |
| **5.1 Surround** | 6 | Left, Right, Center, LFE, Left Surround, Right Surround |
| **7.1 Surround** | 8 | Left, Right, Center, LFE, Left Surround, Right Surround, Left Rear, Right Rear |
| **7.1.4 Immersive**| 12 | 7.1 channels + Top Front Left, Top Front Right, Top Rear Left, Top Rear Right |
| **9.1.6 Immersive**| 16 | 7.1.4 channels + Wide Left, Wide Right, Top Side Left, Top Side Right |

---

## 13. Loudness Standards

The engine implements formal loudness measurement adhering to:

1. **ITU-R BS.1770-5** (Nov 2023):
   - K-Weighting Pre-Filter: 2nd-order high shelf (DeMan coefficients, $+4.0\,\text{dB}$ at $1.68\,\text{kHz}$) in series with RLB high-pass filter ($80\,\text{Hz}$).
   - Absolute Gating: Discard all $400\,\text{ms}$ momentary blocks below $-70.0\,\text{LKFS}$.
   - Relative Gating: Discard all blocks below $(\text{Ungated Mean} - 10.0\,\text{LU})$.
2. **True-Peak Measurement** (ITU-R BS.1770-5 Annex 2):
   - $4\times$ oversampled linear-phase polyphase FIR interpolation filter ($\ge 100\,\text{dB}$ stopband rejection, $<0.01\,\text{dB}$ ripple).
3. **EBU R128 & Tech 3342 (LRA)**:
   - Target: $-23.0\,\text{LUFS} \pm 0.5\,\text{LU}$.
   - Max True Peak: $-1.0\,\text{dBTP}$.
   - Loudness Range: Difference between 10th and 95th percentiles of gated short-term ($3\,\text{s}$) blocks.
4. **ReplayGain 2.0**:
   - Reference target: $-18.0\,\text{LUFS}$ ($89.0\,\text{dB SPL}$).

---

## 14. Output Semantics & Multi-Endpoint Matrix

1. **Multi-Endpoint Fan-Out**: The master stereo or multichannel bus can be routed simultaneously to multiple independent hardware endpoints (e.g. USB DAC, onboard ALSA, network sink).
2. **Independent Drift Correction**: Because disparate hardware devices run on independent crystal oscillators, each endpoint worker thread hosts an independent clock-drift-corrected resampler (Rubato / sinc interpolation) adjusting pitch in fractional parts-per-million (PPM) to track ring buffer watermarks.
3. **Bit-Perfect Verification**: When DSP stages (EQ, volume, spatial, resamplers) are disabled or at unity, the engine proves and certifies Bit-Perfect output via automated cryptographic checksum matching (`BitPerfectReport`).

---

## 15. Diagnostics & Telemetry

1. **Lock-Free Telemetry Publication**: Telemetry snapshots (`PlaybackInfo`) are constructed control-side and published via `ArcSwap` pointers, ensuring readers never block writers.
2. **Structured Diagnostic Categories**: Errors and notices carry typed `DiagnosticKind` tags:
   - `Internal`, `Decoder`, `Output`, `Resampler`, `Stream`, `Endpoint`, `BitPerfect`, `Configuration`, `Spatial`, `Loudness`, `Dsp`, `Clock`, `Plugin`, `Graph`, `Security`.
3. **Arithmetic Anomaly Containment**: All non-finite floats (`NaN`, `+Inf`, `-Inf`) generate structured `NonFiniteIncident` diagnostic records detailing offending node ID, channel index, sample offset, and graph generation.

---

## 16. Error Semantics & Failure Recovery

1. **Non-Fatal Recovery**:
   - Audio endpoint disconnection (USB unplug) transitions the affected endpoint to detached state while keeping core playback and other endpoints active.
   - Stream underruns/overruns (XRuns) trigger zero-fill concealment without interrupting transport state.
2. **Defensive Processing**:
   - Defective DSP nodes producing non-finite values are contained by `NonFinitePolicy::Clamp` or `Silence`, preventing cascade corruption of the master mix bus.
   - Corrupted decoders or tags fail gracefully with typed `DecodeError` rather than crashing the engine.

---

## 17. Serialization & State

1. **Schema Versioning**: All serialized data structures (`Graph2`, `SpatialScene`, `AdmDocument`, `AudioProfile`) carry explicit schema version tags (`version = X`).
2. **Format**: State models serialize to canonical JSON via Serde with strict backwards-compatible field defaults (`#[serde(default)]`).
3. **Deterministic Round-Trip**: Deserializing a valid configuration and re-serializing it produces identical JSON outputs (`BTreeMap` keys guarantee deterministic property order).

---

## 18. Determinism Guarantees

Output reproducibility is formally classified into three equivalence tiers:

1. **Bit-Exact**: Bit-for-bit identical floating-point bit patterns ($f32\text{::to\_bits}() == f32\text{::to\_bits}()$). Guaranteed for passthrough, bit-perfect streaming, delay lines, and channel matrices.
2. **Numerically Equivalent**: Minor differences due to SIMD vectorization, FMA (Fused Multiply-Add), or float order-of-addition variations:
   $$\text{Max Absolute Delta} < 10^{-5}, \quad \text{SNR} \ge 120.0\,\text{dB}$$
3. **Perceptually Equivalent**: Differences below psychoacoustic masking thresholds ($\text{SNR} \ge 80.0\,\text{dB}$).

---

## 19. Compatibility Guarantees

1. **Semantic Versioning (`x.y.z`)**:
   - Major bumps ($x$): Incompatible public API / C-FFI changes.
   - Minor bumps ($y$): Backward-compatible feature additions.
   - Patch bumps ($z$): Backward-compatible fixes and performance optimizations.
2. **Workspace Synchronization**: All member crates (`engine`, `config`, `plugin-abi`, `plugin-test-echo`) stay locked in 100% version lockstep.
3. **C FFI Stability**: Foreign function interface structs utilize explicit `#[repr(C)]` layouts with fixed-size types and versioned negotiation headers.

---

## 20. Security Model

1. **Untrusted Input Handling**: All audio files, network byte streams, CUE sheets, and metadata tags are classified as untrusted input.
2. **Allocation Bounds**: Parsers enforce strict upper bounds on memory allocation:
   - Max channel count: 16
   - Max block frames: 8192
   - Max metadata string length: 64 KB
3. **Integer Arithmetic Safety**: All sample calculations, buffer strides, and file offsets utilize overflow-checked or wrapping arithmetic to eliminate buffer-overflow vulnerabilities.
4. **Fuzzing Standard**: Continuous mutation fuzzing verifies that malformed headers, integer wraps, and truncations yield clean error returns without panics.

---

## 21. Testing & Verification Requirements

Every release candidate must satisfy the following qualification criteria:

1. **Compilation & Static Analysis**:
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets -- -D warnings`
2. **Fidelity & Measurement Test Suite**:
   - Independent mathematical oracle verification (`tests/fidelity/independent_references.rs`).
   - Zero real-time allocation assertion (`tests/fidelity/realtime_allocation.rs`).
   - Dual-threshold loudness compliance validation (`tests/fidelity/loudness_ebu_r128.rs`).
3. **Robustness & Fuzzing**:
   - Seeded mutation parser fuzzing (`tests/fidelity/fuzz_expanded.rs`, `fuzz_mutation.rs`).
4. **Automated Release Qualification**:
   - Execution of `release-qualification` pipeline with output `qualification_status == "PASS"`.
