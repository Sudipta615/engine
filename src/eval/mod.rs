//! # Unified quality-evaluation harness (Phase 2)
//!
//! A reusable framework for measuring whether a DSP / spatial component is
//! *technically correct* — independent of the engine's deeper Perceptual
//! layer. The harness separates **bit-exact correctness** from
//! **perceptual quality** (the deterministic unit and golden suites in
//! `tests/fidelity/` cover the latter per-component; this module gives those
//! measurements a shared, reportable shape a host or CI can consume).
//!
//! ```text
//!   ReferenceVector (versioned spec: id, version, metric expectations)
//!        │  content-addressed (SHA-256, via the aelog substrate)
//!        ▼
//!   suites::*  →  CheckResult (measured vs nominal ± tolerance)
//!        │
//!        ▼
//!   ComponentReport (one per DSP/spatial component)
//!        │
//!        ▼
//!   EvaluationReport  (engine version + registry version + all components)
//!        ├─ to_json()          machine-readable
//!        └─ render_text()      human-readable PASS/FAIL table
//! ```
//!
//! **Reuses the golden/aelog substrate** directly: the versioning discipline
//! mirrors `AELOG_VERSION`, and reference-vector identity is a content address
//! computed with `aelog::cache::{sha256, to_hex}` over the canonical checks.
//! A changed expectation changes the address (and `.reference_vector` a report
//! carries), so a stale vector is always detectable.
//!
//! Everything runs **off the audio path** (measurements are pure functions of
//! captured buffers). It does not touch the realtime callback.

pub mod measure;
pub mod registry;
pub mod suites;

pub use measure::*;
pub use registry::{MetricSpec, ReferenceVector, ReferenceVectorRegistry};

/// Errors produced by the evaluation harness (registry serialization and
/// version rejection).
#[derive(Debug)]
pub enum EvalError {
    Serialize(serde_json::Error),
    RegistryVersion(u32),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::Serialize(e) => write!(f, "eval serialize: {e}"),
            EvalError::RegistryVersion(v) => {
                write!(f, "unsupported reference-vector registry version {v}")
            }
        }
    }
}

impl std::error::Error for EvalError {}

use serde::{Deserialize, Serialize};

/// The metric the harness evaluates. Each has stable units and a display
/// string so reports are self-describing. Treat the snake_case codes as part
/// of the public report schema — do not rename casually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// Mismatch fraction vs a reference capture (0 = bit exact).
    BitExactness,
    /// Magnitude deviation from nominal at a probe frequency (dB).
    FreqResponseDeviationDb,
    /// Phase deviation vs reference (degrees).
    PhaseDeviationDeg,
    /// Total harmonic distortion + noise, as a linear fraction.
    ThdPlusN,
    /// Intermodulation distortion, as a linear fraction.
    IntermodDistortion,
    /// Noise floor below full scale (dBFS).
    NoiseFloorDb,
    /// Channel separation / crosstalk (dB).
    ChannelSeparationDb,
    /// Inter-channel timing offset (samples).
    InterchannelTimingSamples,
    /// Loudness error vs target (LUFS).
    LoudnessErrorLufs,
    /// Crest-factor change (dB).
    CrestFactorDeltaDb,
    /// True-peak error vs a ceiling (dB; ≤ 0 = at/below ceiling).
    TruePeakErrorDb,
    /// Localization error (degrees).
    LocalizationErrorDeg,
    /// Inter-aural (or inter-channel) level balance (dB; 0 = centered).
    InterauralLevelDb,
    /// Sample-rate error vs nominal (ppm).
    SampleRateErrorPpm,
    /// Error of a rendered impulse response vs a naive-direct reference,
    /// relative to the signal peak (dB; lower is better).
    AcousticIrErrorDb,
    /// HRTF/interpolation error: how far an interpolated response falls
    /// outside the convex hull of its bracketing grid nodes (dB; 0 = convex).
    HrtfInterpolationErrorDb,
}

impl MetricKind {
    pub fn code(self) -> &'static str {
        match self {
            MetricKind::BitExactness => "bit_exactness",
            MetricKind::FreqResponseDeviationDb => "freq_response_deviation_db",
            MetricKind::PhaseDeviationDeg => "phase_deviation_deg",
            MetricKind::ThdPlusN => "thd_plus_n",
            MetricKind::IntermodDistortion => "intermod_distortion",
            MetricKind::NoiseFloorDb => "noise_floor_db",
            MetricKind::ChannelSeparationDb => "channel_separation_db",
            MetricKind::InterchannelTimingSamples => "interchannel_timing_samples",
            MetricKind::LoudnessErrorLufs => "loudness_error_lufs",
            MetricKind::CrestFactorDeltaDb => "crest_factor_delta_db",
            MetricKind::TruePeakErrorDb => "true_peak_error_db",
            MetricKind::LocalizationErrorDeg => "localization_error_deg",
            MetricKind::InterauralLevelDb => "interaural_level_db",
            MetricKind::SampleRateErrorPpm => "sample_rate_error_ppm",
            MetricKind::AcousticIrErrorDb => "acoustic_ir_error_db",
            MetricKind::HrtfInterpolationErrorDb => "hrtf_interpolation_error_db",
        }
    }

    /// Human-fronting unit for the value.
    pub fn unit(self) -> &'static str {
        match self {
            MetricKind::BitExactness => "frac",
            MetricKind::FreqResponseDeviationDb
            | MetricKind::NoiseFloorDb
            | MetricKind::AcousticIrErrorDb
            | MetricKind::HrtfInterpolationErrorDb => "dB",
            MetricKind::PhaseDeviationDeg | MetricKind::LocalizationErrorDeg => "deg",
            MetricKind::InterauralLevelDb | MetricKind::TruePeakErrorDb => "dB",
            MetricKind::CrestFactorDeltaDb | MetricKind::ChannelSeparationDb => "dB",
            MetricKind::LoudnessErrorLufs => "LUFS",
            MetricKind::ThdPlusN | MetricKind::IntermodDistortion => "frac",
            MetricKind::InterchannelTimingSamples => "samples",
            MetricKind::SampleRateErrorPpm => "ppm",
        }
    }

    /// Format a measured value with its unit.
    pub fn display(self, value: f64) -> String {
        match self {
            MetricKind::ThdPlusN | MetricKind::IntermodDistortion => {
                format!("{:.3} ({:.1}%)", value, value * 100.0)
            }
            MetricKind::BitExactness => format!("{:.2e} mismatch", value),
            MetricKind::InterchannelTimingSamples => {
                format!("{value:.3} samples")
            }
            _ => format!("{value:.2} {}", self.unit()),
        }
    }
}

/// How a metric's expectation is stated. Kept distinct from a simple nominal
/// so one-sided limits (e.g. "peak must not exceed the ceiling") are explicit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expect {
    /// `|measured − nominal| ≤ tol` in the metric's units.
    Equal { nominal: f64, tol: f64 },
    /// `measured ≤ max`.
    AtMost { max: f64 },
    /// `measured ≥ min`.
    AtLeast { min: f64 },
}

impl Expect {
    fn passes(self, measured: f64) -> bool {
        match self {
            Expect::Equal { nominal, tol } => (measured - nominal).abs() <= tol,
            Expect::AtMost { max } => measured <= max,
            Expect::AtLeast { min } => measured >= min,
        }
    }

    /// A compact human description, e.g. `0.00 ± 0.30 dB`.
    fn describe(self, metric: MetricKind) -> String {
        match self {
            Expect::Equal { nominal, tol } => {
                format!("{} ± {}", metric.display(nominal), metric.display(tol))
            }
            Expect::AtMost { max } => format!("≤ {}", metric.display(max)),
            Expect::AtLeast { min } => format!("≥ {}", metric.display(min)),
        }
    }
}

/// A single measured check within a component report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub metric: MetricKind,
    pub measured: f64,
    pub expect: Expect,
    pub verdict: Verdict,
    pub note: String,
}

impl CheckResult {
    pub fn evaluate(
        metric: MetricKind,
        measured: f64,
        expect: Expect,
        note: impl Into<String>,
    ) -> Self {
        let verdict = if expect.passes(measured) {
            Verdict::Pass
        } else {
            Verdict::Fail
        };
        Self {
            metric,
            measured,
            expect,
            verdict,
            note: note.into(),
        }
    }
}

/// PASS / FAIL verdict (the guide's binary contract; skip/advisory states are
/// modelled by excluding the check rather than a third verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

impl Verdict {
    pub fn code(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
        }
    }
}

/// One component's full report: a named editorial label plus every checked
/// metric tied to a versioned reference vector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentReport {
    pub component: String,
    /// `"{id}@{version}"` of the reference vector this report evaluates.
    pub reference_vector: String,
    pub checks: Vec<CheckResult>,
}

impl ComponentReport {
    pub fn is_pass(&self) -> bool {
        self.checks.iter().all(|c| c.verdict == Verdict::Pass)
    }

    pub fn pass_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.verdict == Verdict::Pass)
            .count()
    }

    pub fn fail_count(&self) -> usize {
        self.checks.len() - self.pass_count()
    }
}

/// The complete evaluation result for one engine build: every component that
/// opted into the harness, plus versioning provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationReport {
    /// SemVer of the *engine crate* that produced the measurements.
    pub engine_version: String,
    /// Version of the reference-vector registry format.
    pub registry_version: u32,
    /// UNIX epoch (ms) of report generation — *not* part of any identity.
    pub generated_unix_ms: u64,
    pub components: Vec<ComponentReport>,
}

impl EvaluationReport {
    pub fn is_all_pass(&self) -> bool {
        self.components.iter().all(|c| c.is_pass())
    }

    pub fn component(&self, name: &str) -> Option<&ComponentReport> {
        self.components.iter().find(|c| c.component == name)
    }

    pub fn total_checks(&self) -> usize {
        self.components.iter().map(|c| c.checks.len()).sum()
    }

    pub fn total_passes(&self) -> usize {
        self.components.iter().map(|c| c.pass_count()).sum()
    }

    pub fn total_failures(&self) -> usize {
        self.total_checks() - self.total_passes()
    }

    /// Machine-readable form (compact JSON).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Machine-readable form (pretty JSON).
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// The human-readable PASS/FAIL table (the guide's report shape).
    pub fn render_text(&self) -> String {
        let rule = "─".repeat(66);
        let mut out = String::new();
        out.push_str(&format!(
            "Quality evaluation report — engine {0} (reference registry v{1})\n",
            self.engine_version, self.registry_version
        ));
        out.push_str(&rule);
        out.push('\n');
        for comp in &self.components {
            out.push_str(&format!(
                "Component: {0} ({1})\n",
                comp.component, comp.reference_vector
            ));
            for check in &comp.checks {
                let label = format!("{:>4}", check.verdict.code().to_uppercase());
                out.push_str(&format!(
                    "  {label}  {0:<26} measured {1:<20} expect {2}{3}\n",
                    check.metric.code(),
                    check.metric.display(check.measured),
                    check.expect.describe(check.metric),
                    if check.note.is_empty() {
                        String::new()
                    } else {
                        format!("  — {}", check.note)
                    }
                ));
            }
            out.push('\n');
        }
        out.push_str(&rule);
        out.push('\n');
        let overall = if self.is_all_pass() {
            "ALL PASS"
        } else {
            "FAILURES"
        };
        out.push_str(&format!(
            "Summary: {0} components · {1} checks — {2} PASS, {3} FAIL ({overall})\n",
            self.components.len(),
            self.total_checks(),
            self.total_passes(),
            self.total_failures()
        ));
        out
    }
}

/// The kind of change a [`VersionComparison`] flagged for one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    /// Old and new measured values + verdicts are identical.
    Unchanged,
    /// The value drifted but the PASS/FAIL verdict did not change.
    Drift,
    /// A previously-failing check now passes (an improvement).
    Improvement,
    /// A previously-passing check now fails (a regression).
    Regression,
}

impl DeltaKind {
    pub fn code(self) -> &'static str {
        match self {
            DeltaKind::Unchanged => "unchanged",
            DeltaKind::Drift => "drift",
            DeltaKind::Improvement => "improvement",
            DeltaKind::Regression => "regression",
        }
    }
}

/// The per-check delta between two engine(-version) evaluation reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckDelta {
    pub component: String,
    pub metric: MetricKind,
    pub old: f64,
    pub new: f64,
    pub verdict_old: Verdict,
    pub verdict_new: Verdict,
    pub kind: DeltaKind,
}

/// A cross-version comparison of two [`EvaluationReport`]s — the automatic
/// regression-detection surface the harness promises (results comparable
/// between engine versions).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct VersionComparison {
    /// Total (component, metric) pairs common to both reports.
    pub checked: usize,
    /// Checks that flipped from Pass → Fail (must stay 0 across a clean bump).
    pub regressions: usize,
    /// Checks that flipped from Fail → Pass.
    pub improvements: usize,
    /// Checks whose value moved without a verdict change (informational only).
    pub drifts: usize,
    pub unchanged: usize,
    pub details: Vec<CheckDelta>,
}

impl VersionComparison {
    pub fn is_clean(&self) -> bool {
        self.regressions == 0
    }

    /// Human-readable summary of what changed between the two versions.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Version comparison — {0} shared checks: {1} unchanged, {2} drifted, {3} improved, {4} regressed\n",
            self.checked,
            self.unchanged,
            self.drifts,
            self.improvements,
            self.regressions
        ));
        if self.is_clean() {
            out.push_str("No regressions — measurement surface is stable.\n");
        } else {
            out.push_str("Regressions detected:\n");
        }
        for d in self
            .details
            .iter()
            .filter(|d| d.kind != DeltaKind::Unchanged)
        {
            out.push_str(&format!(
                "  {:>4} {:<12} {:<28} {} → {} ({})\n",
                d.kind.code().to_uppercase(),
                d.component,
                d.metric.code(),
                d.metric.display(d.old),
                d.metric.display(d.new),
                if d.verdict_new == Verdict::Fail {
                    "NOW FAILING".to_string()
                } else {
                    String::new()
                }
            ));
        }
        out
    }
}

impl EvaluationReport {
    /// Diff this report against another (typically a later engine build):
    /// for every shared (component, metric) check, classify it as unchanged,
    /// drift, improvement, or regression, so regressions are detectable
    /// automatically across engine versions. Offline path only.
    pub fn compare(&self, other: &EvaluationReport) -> VersionComparison {
        let mut out = VersionComparison::default();
        for c in &self.components {
            let other_comp = other.component(&c.component);
            if other_comp.is_none() {
                continue;
            }
            let other_comp = other_comp.unwrap();
            for (i, check) in c.checks.iter().enumerate() {
                // Pair checks by metric **and by occurrence index** so a
                // component with several checks of the same metric (e.g. two
                // inter-aural level checks) compares like-for-like instead of
                // every one matching the first.
                let occ = c.checks[..i]
                    .iter()
                    .filter(|x| x.metric == check.metric)
                    .count();
                let o = other_comp
                    .checks
                    .iter()
                    .filter(|x| x.metric == check.metric)
                    .nth(occ);
                let Some(o) = o else {
                    continue;
                };
                out.checked += 1;
                let kind = if o.verdict == check.verdict && o.measured == check.measured {
                    DeltaKind::Unchanged
                } else if check.verdict == Verdict::Pass && o.verdict == Verdict::Fail {
                    DeltaKind::Regression
                } else if check.verdict == Verdict::Fail && o.verdict == Verdict::Pass {
                    DeltaKind::Improvement
                } else {
                    DeltaKind::Drift
                };
                match kind {
                    DeltaKind::Unchanged => out.unchanged += 1,
                    DeltaKind::Drift => out.drifts += 1,
                    DeltaKind::Improvement => out.improvements += 1,
                    DeltaKind::Regression => out.regressions += 1,
                }
                out.details.push(CheckDelta {
                    component: c.component.clone(),
                    metric: check.metric,
                    old: check.measured,
                    new: o.measured,
                    verdict_old: check.verdict,
                    verdict_new: o.verdict,
                    kind,
                });
            }
        }
        out
    }
}

/// Run every registered DSP/spatial suite and assemble an [`EvaluationReport`].
/// Deterministic; never panics (each suite measures captured buffers). This is
/// the entry point a host or CI test calls to get the unified report.
pub fn run_quality() -> EvaluationReport {
    let registry = ReferenceVectorRegistry::build();
    let mut report = EvaluationReport {
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        registry_version: registry.format_version,
        generated_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        components: Vec::new(),
    };
    report.components.push(suites::dsp_pipeline(&registry));
    report.components.push(suites::parametric_eq(&registry));
    report.components.push(suites::limiter(&registry));
    report.components.push(suites::resampler(&registry));
    report.components.push(suites::binaural(&registry));
    report.components.push(suites::loudness(&registry));
    report.components.push(suites::convolution(&registry));
    report
        .components
        .push(suites::channel_separation(&registry));
    report.components.push(suites::hrtf(&registry));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_details_are_stable_and_rooted_in_math() {
        assert_eq!(Verdict::Pass.code(), "pass");
        assert_eq!(MetricKind::ThdPlusN.code(), "thd_plus_n");
        assert_eq!(MetricKind::ThdPlusN.unit(), "frac");
        assert_eq!(
            MetricKind::InterchannelTimingSamples.code(),
            "interchannel_timing_samples"
        );

        let p = CheckResult::evaluate(
            MetricKind::ThdPlusN,
            0.0004,
            Expect::AtMost { max: 0.001 },
            "clean passthrough",
        );
        assert_eq!(p.verdict, Verdict::Pass);
        let f = CheckResult::evaluate(
            MetricKind::ThdPlusN,
            0.02,
            Expect::AtMost { max: 0.001 },
            "distorted",
        );
        assert_eq!(f.verdict, Verdict::Fail);
    }

    #[test]
    fn expect_descriptions_are_readable() {
        assert_eq!(
            Expect::Equal {
                nominal: 0.0,
                tol: 0.3
            }
            .describe(MetricKind::FreqResponseDeviationDb),
            "0.00 dB ± 0.30 dB"
        );
        let r = EvaluationReport {
            engine_version: "test".into(),
            registry_version: 1,
            generated_unix_ms: 0,
            components: vec![],
        };
        assert!(r.render_text().contains("0 checks"));
        assert!(r.is_all_pass());
    }

    #[test]
    fn render_text_presents_a_pass_and_a_fail() {
        let comp = ComponentReport {
            component: "Dummy".into(),
            reference_vector: "dummy@1".into(),
            checks: vec![
                CheckResult::evaluate(
                    MetricKind::BitExactness,
                    0.0,
                    Expect::Equal {
                        nominal: 0.0,
                        tol: 0.0,
                    },
                    "",
                ),
                CheckResult::evaluate(
                    MetricKind::NoiseFloorDb,
                    -10.0,
                    Expect::AtMost { max: -60.0 },
                    "hot",
                ),
            ],
        };
        let r = EvaluationReport {
            engine_version: "3.0.0".into(),
            registry_version: 1,
            generated_unix_ms: 0,
            components: vec![comp],
        };
        let text = r.render_text();
        assert!(text.contains("PASS"), "{text}");
        assert!(text.contains("FAIL"), "{text}");
        assert!(text.contains("1 PASS, 1 FAIL"), "{text}");
        assert!(!r.is_all_pass());

        let json = r.to_json().unwrap();
        assert!(json.contains("\"verdict\":\"fail\"") || json.contains("\"verdict\":\"pass\""));
        let back: EvaluationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.components.len(), 1);
    }

    #[test]
    fn run_quality_assembles_all_components_and_passes() {
        let report = run_quality();
        assert_eq!(report.components.len(), 9, "nine suites registered");
        for (n, comp) in report.components.iter().enumerate() {
            assert!(
                comp.is_pass(),
                "{} must pass: {}",
                comp.component,
                report.render_text()
            );
            assert!(!comp.checks.is_empty(), "component {n} has no checks");
            assert!(
                comp.reference_vector.contains('@'),
                "reference vector must be versioned"
            );
        }
        assert!(report.is_all_pass());
        // Machine and human forms both render.
        assert!(!report.render_text().is_empty());
        assert!(report.to_json().is_ok());
    }

    #[test]
    fn compare_mirrors_run_quality_and_detects_a_regression() {
        let a = run_quality();
        // Two runs of the same build: nothing moved → clean comparison.
        let same = a.compare(&run_quality());
        assert_eq!(
            same.checked,
            same.unchanged,
            "drifting checks: {:?}",
            same.details
                .iter()
                .filter(|d| d.kind != DeltaKind::Unchanged)
                .map(|d| (
                    d.component.clone(),
                    d.metric.code().to_string(),
                    d.old,
                    d.new
                ))
                .collect::<Vec<_>>()
        );
        assert!(same.regressions == 0 && same.drifts == 0 && same.is_clean());
        assert!(same.render_text().contains("No regressions"));

        // A synthetic second report where one previously-passing check now
        // fails → exactly one regression, reported in both forms.
        let mut b = a.clone();
        let comp = b.components.first_mut().unwrap();
        let check = comp.checks.first_mut().unwrap();
        check.measured = 1e9; // wildly out of spec
        check.verdict = Verdict::Fail; // a later build that actually fails
        let cmp = a.compare(&b);
        assert!(cmp.regressions == 1, "{cmp:?}");
        assert!(!cmp.is_clean());
        assert_eq!(cmp.details[0].kind, DeltaKind::Regression);
        let text = cmp.render_text();
        assert!(text.contains("Regression"), "{text}");
    }
}
