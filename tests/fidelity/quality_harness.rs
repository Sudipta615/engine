//! Unified quality-evaluation harness (Phase 2) integration test.
//!
//! Runs every registered DSP/spatial suite through [`engine::eval::run_quality`]
//! and asserts the report is all-PASS, machine-readable (JSON round-trips),
//! human-readable, deterministically re-runnable, and versioned (every component
//! cites a `{id}@{version}` content-addressed reference vector). This is the
//! CI-facing boundary of the harness documented in `docs/EVOLUTION.md`.

use engine::eval::{ComponentReport, EvaluationReport, ReferenceVectorRegistry};

/// The current number of component suites the harness registers.
/// Keep in sync with `ReferenceVectorRegistry::build` + `run_quality`.
const EXPECTED_SUITES: usize = 9;

#[test]
fn quality_report_passes_every_component_and_round_trips() {
    let report = engine::eval::run_quality();
    assert!(
        report.is_all_pass(),
        "harness must report all PASS:\n{}",
        report.render_text()
    );
    assert_eq!(report.components.len(), EXPECTED_SUITES);

    // Every component carries a versioned reference vector.
    for comp in &report.components {
        assert!(comp.reference_vector.contains('@'), "{}", comp.component);
    }

    // Machine-readable form round-trips losslessly.
    let json = report.to_json().unwrap();
    let back: EvaluationReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.components.len(), report.components.len());
    assert_eq!(back.total_checks(), report.total_checks());
    assert_eq!(back.total_failures(), report.total_failures());
    assert!(report.to_json_pretty().unwrap().contains("components"));
}

#[test]
fn quality_report_is_deterministic_and_re_runnable() {
    let a = engine::eval::run_quality();
    let b = engine::eval::run_quality();
    // Deterministic measurements → identical measured values and pass/fail,
    // despite the separate `generated_unix_ms` stamp.
    for (ca, cb) in a.components.iter().zip(&b.components) {
        assert_eq!(ca.checks.len(), cb.checks.len());
        for (x, y) in ca.checks.iter().zip(&cb.checks) {
            assert_eq!(x.measured, y.measured, "{}", x.metric.code());
            assert_eq!(x.verdict, y.verdict);
        }
    }
}

#[test]
fn reference_registry_is_versioned_and_addressable() {
    let reg = ReferenceVectorRegistry::build();
    assert!(reg.format_version >= 1);
    let pipeline = reg.get("dsp-pipeline").expect("pipeline vector");
    assert_eq!(pipeline.address.len(), 64, "SHA-256 hex content address");
    // The address is a pure function of expectations + version.
    assert_eq!(
        reg.get("dsp-pipeline").unwrap().address,
        ReferenceVectorRegistry::build()
            .get("dsp-pipeline")
            .unwrap()
            .address
    );
}

#[test]
fn a_failed_check_is_visible_in_both_report_forms() {
    // Prove the FAIL path is representable end-to-end: a synthetic component
    // with a deliberately broken check must be flagged in text and JSON.
    let broken = ComponentReport {
        component: "Synthetic".into(),
        reference_vector: "synthetic@1".into(),
        checks: vec![engine::eval::CheckResult::evaluate(
            engine::eval::MetricKind::NoiseFloorDb,
            -20.0,
            engine::eval::Expect::AtMost { max: -60.0 },
            "forced failure",
        )],
    };
    let report = EvaluationReport {
        engine_version: "test".into(),
        registry_version: 1,
        generated_unix_ms: 0,
        components: vec![broken],
    };
    assert!(!report.is_all_pass());
    let text = report.render_text();
    assert!(text.contains("FAIL"), "{text}");
    let json = report.to_json().unwrap();
    assert!(json.contains("\"verdict\":\"fail\"") || json.contains("\"verdict\": \"fail\""));
}

#[test]
fn cross_version_compare_detects_regressions_before_there_is_a_regression() {
    use engine::eval::{DeltaKind, VersionComparison};
    // A clean bump: comparing two runs of this engine build must be clean
    // (the measurement surface is stable), and the comparison is serializable
    // so older vs newer build reports can be diffed automatically.
    let a = engine::eval::run_quality();
    let b = engine::eval::run_quality();
    let cmp: VersionComparison = a.compare(&b);
    assert!(
        cmp.is_clean(),
        "same build must not regress: {}",
        cmp.render_text()
    );
    assert!(cmp.checked > 0);
    assert_eq!(cmp.checked, cmp.unchanged);
    assert!(cmp.details.iter().all(|d| d.kind == DeltaKind::Unchanged));

    // Simulate the 'later build' drifting: flip one measured value into a
    // definite fail; the comparison must flag exactly one regression and the
    // JSON form carries it.
    let mut later = a.clone();
    later.components[0].checks[0].measured = 1e9;
    later.components[0].checks[0].verdict = engine::eval::Verdict::Fail;
    let drift = a.compare(&later);
    assert!(!drift.is_clean());
    assert_eq!(drift.regressions, 1);
    let j = serde_json::to_string(&drift).unwrap();
    assert!(j.contains("regression"));
}
