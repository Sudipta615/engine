//! Formal Performance Matrix Validation Suite (spec §8.1, Punch List Item 25).
//!
//! Validates:
//! 1. Formal performance matrix benchmarks across block sizes (16..1024) and sample rates (44.1..384 kHz).
//! 2. Multi-format scalability: Stereo, Multichannel (5.1, 7.1.4), Binaural HRTF, and HOA (Orders 1..3).
//! 3. Strictly zero heap allocations during the real-time processing loop.
//! 4. Bounded callback execution within real-time deadlines.
//! 5. Structured JSON and human-readable ASCII rendering.

use engine::eval::performance_matrix::{
    benchmark_cell, execute_performance_matrix, MatrixFormat, PerformanceMatrixConfig,
};

#[test]
fn test_formal_performance_matrix_cell_stereo_rates_and_blocks() {
    let block_sizes = [16, 64, 256, 1024];
    let sample_rates = [44100.0, 48000.0, 96000.0, 192000.0];

    for &sr in &sample_rates {
        for &bs in &block_sizes {
            let res = benchmark_cell(MatrixFormat::Stereo, bs, sr, 50, 10, 3.5);

            assert_eq!(
                res.allocations, 0,
                "Stereo processing must perform 0 allocations (block={}, sr={})",
                bs, sr
            );
            assert!(
                res.mean_callback_us < res.block_deadline_us,
                "Mean callback ({:.1} µs) exceeded deadline ({:.1} µs) at block={}, sr={}",
                res.mean_callback_us,
                res.block_deadline_us,
                bs,
                sr
            );
            assert!(
                res.worst_callback_us < res.block_deadline_us * 2.5,
                "Worst callback ({:.1} µs) exceeded jitter allowance ({:.1} µs) at block={}, sr={}",
                res.worst_callback_us,
                res.block_deadline_us * 2.5,
                bs,
                sr
            );
            assert!(
                res.mean_cpu_percent < 50.0,
                "Mean CPU% was too high: {:.2}% at block={}, sr={}",
                res.mean_cpu_percent,
                bs,
                sr
            );
            assert!(
                res.ns_per_sample > 0.0,
                "ns_per_sample must be positive and non-zero"
            );
        }
    }
}

#[test]
fn test_formal_performance_matrix_multichannel_and_spatial() {
    let formats = [
        MatrixFormat::Mono,
        MatrixFormat::Stereo,
        MatrixFormat::TwoPointOne,
        MatrixFormat::Multichannel5Point1,
        MatrixFormat::Multichannel7Point1,
        MatrixFormat::Multichannel7Point1Point4,
        MatrixFormat::Multichannel9Point1Point6,
        MatrixFormat::BinauralHrtf,
        MatrixFormat::HoaOrder1,
        MatrixFormat::HoaOrder2,
        MatrixFormat::HoaOrder3,
    ];

    let sr = 48000.0;
    let bs = 128;

    for &fmt in &formats {
        let res = benchmark_cell(fmt, bs, sr, 30, 5, 3.5);

        assert_eq!(
            res.allocations, 0,
            "Format {:?} must perform 0 allocations",
            fmt
        );
        assert!(
            res.mean_callback_us < res.block_deadline_us,
            "Format {:?} mean callback ({:.1} µs) exceeded budget ({:.1} µs)",
            fmt,
            res.mean_callback_us,
            res.block_deadline_us
        );
        assert!(
            res.worst_callback_us < res.block_deadline_us * 2.5,
            "Format {:?} worst callback ({:.1} µs) exceeded jitter allowance ({:.1} µs)",
            fmt,
            res.worst_callback_us,
            res.block_deadline_us * 2.5
        );
        assert!(
            res.memory_footprint_bytes > 0,
            "Memory footprint must be calculated"
        );
    }
}

#[test]
fn test_formal_performance_matrix_report_generation() {
    let config = PerformanceMatrixConfig {
        block_sizes: vec![64, 256],
        sample_rates: vec![48000.0, 96000.0],
        formats: vec![
            MatrixFormat::Stereo,
            MatrixFormat::Multichannel5Point1,
            MatrixFormat::BinauralHrtf,
            MatrixFormat::HoaOrder1,
        ],
        iterations_per_cell: 20,
        warmup_iterations: 5,
        cpu_nominal_ghz: 3.5,
    };

    let report = execute_performance_matrix(&config);

    assert_eq!(report.total_allocations, 0);
    assert!(report.passed_deadline);
    assert!(!report.entries.is_empty());

    let json = report.to_json();
    assert!(json.contains("engine_version"));
    assert!(json.contains("entries"));

    let table = report.render_table();
    assert!(table.contains("FORMAL PERFORMANCE MATRIX"));
    assert!(table.contains("Stereo (2ch)"));
    assert!(table.contains("Summary:"));
}
