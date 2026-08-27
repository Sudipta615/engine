# Signal Flow

This document traces one block of audio samples from file to speaker, plus
the analysis and capture side-paths. See [`ARCHITECTURE.md`](ARCHITECTURE.md)
for the module-level view.

## Playback path

```
file / URI / memory
        │
        ▼
┌─────────────────┐   ┌──────────────────┐
│ Format scanner  │──▶│ Decoder::open    │  probe by extension + magic bytes,
│ (scanner.rs)    │   │ (decoder.rs)     │  route to Symphonia / DSD / Opus,
└─────────────────┘   └────────┬─────────┘  TTA / WavPack / APE
└─────────────────┘   └────────┬─────────┘
                               │  AudioFrame<f32> blocks (native rate,
                               │  native channel layout, up to 7.1.4)
                               ▼
┌──────────────────────────────────────────────┐
│            Decode & process loop             │
│  (engine/decode_loop/)                       │
│                                              │
│  ┌─────────────────────────────────┐         │
│  │ Channel trim / routing /        │  multichannel path only: per-channel
│  │ bass mgmt / LFE                 │  gain, delay, polarity, crossover
│  └────────────────┬────────────────┘         │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Mix bus (MixBusNode)          │  N per-input chains: preamp + loudness
│  │ per-input pre-mix + sum       │  + user gain/balance/mute, summed under
│  │                               │  a TrackMixer-compatible envelope
│  │                               │  (gapless / crossfade / fade; the
│  │                               │  pre→post mixing boundary)
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Parametric EQ (+ graphic EQ)  │  up to 64 bands + AutoEQ presets,
│  │ (post-mix)                    │  10/15/31 ISO graphic layer, preamp
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Multiband compressor          │  3-band
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Convolution reverb            │  FFT partitioned, IR loader
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Balance → crossfeed → stereo  │  balance, Bauer / Chu Moy / J. Meier
│  │ enhancer                      │  crossfeed, mid-side width
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Timestretch / pitch           │  WSOLA (varispeed, time-stretch,
│  └────────────────┬──────────────┘  pitch-shift)
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Volume + seek fade            │  software gain w/ ramps
│  └───────────────────────────────┘           │
│                                              │
│              analyzer tap ──────────────────▶│  RMS / peak / spectrum /
│  (fed from the decode loop)                  │  dominant frequency → PlaybackInfo
│                                              │
│  Post-mix block then goes to the output      │
│  domain (not shown here; see below).         │
└──────────────────────────────────────────────┘

Then, in the **output domain** (after the process loop, at the output rate):

```
post-mix block (f32 or f64)
        │
        ▼
┌─────────────────────┐
│ Resampler           │  Rubato sinc → output rate, or bit-perfect
│ (or passthrough)    │  / DoP passthrough
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ Safety limiter      │  4× true-peak FIR, ceiling, lookahead
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ TPDF dither         │  applied at int conversion boundary only
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ FixedFrameBuffer    │  lock-free SPSC ring, interleaved f32
│ (output_buffer)     │
└──────────┬──────────┘
           ▼
┌─────────────────────┼─────────────────────┐
▼                     ▼                     ▼
cpal shared      native exclusive    ASIO / WASAPI / CoreAudio hog
mode callbacks   (ALSA hw:, WASAPI    (native DSD transport)
                 exclusive, CoreAudio)
```

### Precision modes

The whole chain runs in `f32` by default (Performance). In `f64` Quality
mode each sample is promoted to `f64` once at the start of the process loop
and every pre- and post-mix stage runs in double precision until the result
is demoted back to `f32` for the ring. Two hard bypass modes skip the entire
chain: **bit-perfect** (only volume ramps and seek fades survive) and **DoP
bypass** (a pure passthrough so 24-bit DSD-over-PCM words reach the DAC
unmodified). The safety limiter and dither run in the output domain in f32.

## Side paths

### System-audio capture (WASAPI loopback)

```
system mixer (all apps)
        │  IAudioCaptureClient packets
        ▼
loopback thread ──▶ capture ring (FixedFrameBuffer)
                          │  drained every tick
                          ▼
              WAV file (f32) ──▶ finalized header on stop
```

Capture is independent of playback state — you can record system audio while
the engine is idle or playing through a different endpoint.

### Additional endpoints (multi-endpoint routing matrix, v3.7.0)

```
master stereo block (output domain, primary rate)
        │  fan-out on the decode loop, once per flushed block
        ▼
per endpoint: SPSC ring (decode loop pushes, backend drains)
        │  resampler master → endpoint rate (None when rates match)
        │  endpoint-rate final limiter (resampled frames only)
        │  per-endpoint gain
        ▼
endpoint backend's realtime callback ──▶ device
```

Each `EngineConfig.additional_endpoints` entry drives one extra output
device independently: its own lock-free ring, its own rate domain, its own
final limiter sized for that rate, and its own level. A stuck endpoint
buffers at most `MAX_ENDPOINT_PENDING_FRAMES` ahead of its ring (oldest
frames dropped first) and can never take down the primary device. Clock
drift between independent devices is deliberately not corrected — each
endpoint resamples against its own nominal clock (drift correction is a
documented follow-up). Same-rate endpoints reuse the master's already-
limited block untouched.

### Loudness analysis

```
decode (offline, background thread)
        │  EBU R128 meter (ITU-R BS.1770 / EBU Tech 3342: 400 ms momentary
        │  blocks on a 100 ms hop, 3 s short-term window, gated)
        ▼
LoudnessScanResult { LUFS, dBTP, LRA, RG gain/peak }
        │
        ├─▶ applied to playback pipeline (loudness normalization)
        ├─▶ merged into metadata tags (`tag-write`)
        └─▶ emitted as LoudnessScanComplete event
```

### Fingerprinting (AcoustID)

```
decode (offline) ──▶ mono downmix ──▶ 16-bit PCM ──▶ Chromaprint
        ──▶ compact fingerprint + duration ──▶ submit to AcoustID API
```

### Telemetry

Every tick publishes a `PlaybackInfo` snapshot into an `ArcSwap`:
position, state, volume, format, DSP status, analyzer levels, queue state,
and u64 counters (clips, NaNs, underruns, CPU overloads, deadlock misses).
Hosts read it lock-free from any thread.
