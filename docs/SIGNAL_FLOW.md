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
│  │ Preamp + loudness normalizer  │  pre-mix: gain ramps + EBU R128 /
│  │ (out & in)                    │  ReplayGain (optional)
│  └────────────────┬──────────────┘           │
│                   ▼                          │
│  ┌────────────────┴──────────────┐           │
│  │ Track mixer                   │  gapless / crossfade blend
│  │ (dual-decoder)                │  (pre→post mixing boundary)
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
