# Quality evaluation (Phase 2 — `engine::eval`)

This page documents how the engine measures whether processing is *technically
correct and perceptually sensible*: the objective harness (`src/eval/`) and the
controlled listening-test layer that sits beside it. It follows the Phase-2
rules: **bit-exact correctness is separated from perceptual quality**, stimuli
are reproducible, reference signals are retained, test conditions are
documented, and no single metric is ever treated as a universal measure of
quality.

---

## 1. The objective harness

`engine::eval` is the machine-readable evaluation framework. Its shape:

```
ReferenceVectorRegistry (versioned, content-addressed specs)
        ↓
suites::*  →  CheckResult { metric, measured, expect, verdict, note }
        ↓
ComponentReport  →  EvaluationReport { engine_version, registry_version, components }
        ├─ to_json() / to_json_pretty()      machine-readable
        ├─ render_text()                     human-readable PASS/FAIL table
        └─ compare(&other) → VersionComparison  (cross-version regression check)
```

### Reference vectors are versioned and content-addressed

- Every component cites a `{id}@{version}` reference vector whose `address` is
  a **SHA-256 content address** over the canonical expectations + version
  (computed with the aelog substrate, `dsp::aelog::cache::{sha256, to_hex}`).
  Changing an expectation changes the address — a stale spec is detectable from
  the address alone.
- `ReferenceVectorRegistry::register` bumps a vector's version only when its
  spec actually changes; the registry itself is versioned
  (`format_version`) and serde-serializable, so the set of vectors an engine
  build evaluates is a shareable, versioned artifact.
- Engine provenance (`engine_version`) is deliberately **not** in the address:
  behavior should stay addressable across builds; the report records the engine
  version separately for provenance.

### Measurements commit to the fidelity-suite numbers

The suites reuse the exact scenarios the repo's golden/fidelity tests already
commit to, so a printed value matches a committed test:

| Component | Vector id | Metrics |
|---|---|---|
| DspPipeline passthrough | `dsp-pipeline@1` | bit-exactness, THD+N |
| Parametric EQ biquad | `parametric-eq@1` | freq-response deviation (centre +6 dB, far-band unity), phase deviation |
| Lookahead limiter | `limiter@1` | true-peak error vs −1 dBFS ceiling |
| Resampler 48→44.1k | `resampler@1` | in-band freq-response deviation (18 kHz tone, unity) |
| Binaural head model | `binaural@1` | inter-aural level (centered ≈ 0 dB, far-panned ≥ 4 dB) |
| EBU R128 loudness | `loudness@1` | loudness error vs the BS.1770-4 −0.02 LUFS reference tone |
| Partitioned-FFT convolution | `convolution@1` | acoustic-IR error vs naive direct convolution (≤ −60 dB) |
| Channel separation | `channel-separation@1` | crosstalk leakage (≤ −100 dB) |
| HRTF interpolation | `hrtf@1` | interpolation convexity vs bracketing grid nodes |

### Cross-version regression detection

`EvaluationReport::compare(&later)` diffs every shared (component, metric,
occurrence) check and classifies it as `unchanged` / `drift` / `improvement` /
`regression`. A clean engine bump must report `regressions == 0`; CI treats any
regression as a failure. Duplicate metrics within one component are paired by
occurrence index so multi-check components compare like-for-like.

### Adding a suite

1. Add a `def_*` (the versioned `ReferenceVector`) and a matching `measure`
   function in `src/eval/suites.rs`; register the `def_*` in
   `ReferenceVectorRegistry::build` and push the measure in `eval::run_quality`.
2. Use a deterministic stimulus and the fidelity-suite convention for the
   metric (Goertzel amplitude, DTFT IR magnitude, naive-direct reference, …).
3. Bump `EXPECTED_SUITES` in `tests/fidelity/quality_harness.rs`; the harness
   test asserts every suite passes and the report is deterministic + versioned.
4. Never assert a single metric as "quality": a PASS means the measured value
   is inside the committed tolerance for that specific stimulus.

### Report discipline

- `measured` values are finite by convention (perfect isolation / exact match
  report a documented floor, e.g. −200 dB / −60 dB) so reports round-trip
  through JSON.
- Machine form is the contract (`metric.code()`, `verdict`, `expect`); the
  human table is derived from it.
- The harness runs entirely off the audio path — no realtime callback is ever
  touched.

---

## 2. Perceptual (listening) evaluation layer

Where objective metrics are insufficient — spatial localization, brightness,
distortion *annoyance*, low-level artifacts — evaluation is a **controlled
listening test**, not an anecdote. The procedure below is the repo's standard;
a test that does not satisfy it is not an evaluation result.

### Dataset & stimuli

- **Reproducible stimuli**: every stimulus is generated deterministically
  (pink noise, ESS sweeps, stepped sine, calibrated program segments) or is a
  versioned reference file retained in the repo. Record the exact generator
  parameters / file hash in the test log.
- **Reference retention**: the dry (unprocessed) stimulus is always kept
  alongside the processed variant — a listening test without the reference is
  not attributable.
- **Prefer anchored tests**: ABX / ABC ("hidden reference") over absolute
  ratings; when rating scales are used, anchor them with known-good and
  known-bad examples.

### Conditions

- **Separation of concerns**: bit-exactness is decided by the harness; a
  listening test answers *perceptual* questions only. Never report a perceptual
  verdict where an objective measurement decides the question.
- **Codec/source limits are not engine artifacts**: when a listener hears a
  deficit, first confirm the deficit is absent from a reference chain of the
  same source (e.g. decode-only path). Document the source chain in every
  result.
- **Blind/controlled**: listeners are blind to the condition; runs are
  randomized/counter-balanced; results are reported per-condition with
  listener counts, not as a single anecdote.
- **Document the conditions**: sample rate, latency path, output device,
  room/headphone, and the engine + reference-vector versions.

### Reporting

- A perceptual result is a dataset + procedure + per-condition summary, never
  "sounds better". Machine-readable form uses the harness's report shape
  (`metric`, `measured`/`count`, `verdict`, `note`) with a
  `listening` marker so objective and perceptual rows are never conflated.
- **No single metric is a universal measure of quality.** A PASS on one metric
  plus a PASS on a listening test for one stimulus does not generalize; report
  the coverage (which components, which stimuli, which versions).

---

## 3. Regression flow

1. `cargo test --test quality_harness` — objective surface, all components
   must pass; JSON round-trip and determinism are asserted.
2. For a version bump, run the report from the previous engine build and
   `compare()`; the diff must show `regressions == 0` (drift is informational
   and must be explained in the changelog).
3. Perceptual changes additionally run the controlled listening procedure above
   and record the dataset in the PR.
