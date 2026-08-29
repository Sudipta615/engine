//! Fidelity tests — Phase 7 S5 room-correction pipeline THROUGH THE GRAPH.
//!
//! Evolution thresholds (`docs/EVOLUTION.md` Phase 7):
//! * pink noise through a corrected synthetic room → octave-band residual
//!   within **±0.5 dB, 40 Hz–16 kHz**;
//! * **disabled = bit-exact**: plans without the correction step remain
//!   bit-identical to the frozen master (equivalence-suite discipline);
//! * all three phase modes produce identical magnitude (**±0.01 dB**),
//!   differing only in phase/latency;
//! * the reported correction latency matches the IR group delay.
//!
//! The live-toggle / IR-hot-load-across-a-generation-swap and
//! `position_secs_compensated` acceptance live in the engine command tests
//! (`src/engine/tests/commands.rs`), which exercise the engine's real
//! reconfigure path.

use std::sync::Arc;

use engine::dsp::correction::{
    convolve, derive_correction_ir, minimum_phase_ir, ConditionedIr, CorrectionIrSet, DeriveParams,
    PhaseMode, Spectrum, TargetCurve,
};
use engine::dsp::{DspGraph, DspNode};
use engine::EngineConfig;

const FS: f64 = 48_000.0;
/// FFT / IR render length.
const N: usize = 8192;
/// Graph processing block.
const BLOCK: usize = 1024;
/// 4 s of test audio.
const FRAMES: usize = 48_000 * 4;

/// Synthetic ±6 dB room magnitude (broad, smooth — nothing pathological),
/// shared with the S4 acceptance suite.
fn room_mag_db(f: f64) -> f64 {
    let bump = |fc: f64, amp: f64, sigma_oct: f64| {
        amp * (-((f / fc).log2().powi(2)) / (2.0 * sigma_oct * sigma_oct)).exp()
    };
    bump(180.0, 6.0, 0.75) + bump(1400.0, -6.0, 0.75)
}

/// The synthetic room as a causal (minimum-phase) time-domain IR.
fn room_ir() -> Vec<f64> {
    let mag: Vec<f64> = (0..=N / 2)
        .map(|j| {
            let f = j as f64 * FS / N as f64;
            if j == 0 || j == N / 2 {
                0.0
            } else {
                room_mag_db(f)
            }
        })
        .collect();
    let spec = Spectrum::from_magnitude_db(&mag, N, FS).unwrap();
    minimum_phase_ir(&spec).unwrap()
}

/// Derive the correction set for the synthetic room in the given phase mode.
fn correction_set(phase: PhaseMode) -> CorrectionIrSet {
    let params = DeriveParams {
        sample_rate: FS,
        ir_len_samples: N,
        target: TargetCurve::Flat,
        snr_db: 60.0,
        smoothing_octaves: 1.0 / 6.0,
        phase_mode: phase,
        ..DeriveParams::default()
    };
    derive_correction_ir(
        &ConditionedIr {
            channels: vec![room_ir()],
            sample_rate: FS,
            lead_trimmed: 0,
        },
        &params,
    )
    .unwrap()
}

/// Process `input` (stereo) through a fresh graph with the given correction
/// state; returns the left-channel output.
fn run_through_graph(
    set: Option<&CorrectionIrSet>,
    enabled: bool,
    input: &[(f32, f32)],
) -> Vec<f32> {
    let mut graph = DspGraph::from_config(&EngineConfig::default(), FS as f32);
    if let Some(set) = set {
        graph.load_correction_ir(Arc::new(set.clone()));
    }
    graph.set_correction_enabled(enabled);
    graph.drain_queued_control();

    let mut out = Vec::with_capacity(input.len());
    for chunk in input.chunks(BLOCK) {
        let mut l: Vec<f32> = chunk.iter().map(|(x, _)| *x).collect();
        let mut r: Vec<f32> = chunk.iter().map(|(_, x)| *x).collect();
        graph.process_block(&mut l, &mut r);
        out.extend_from_slice(&l);
    }
    out
}

/// Pink-ish noise (white shaped 1/√f) with a deterministic seed, at −24 dBFS.
fn pink_noise(frames: usize, seed: u64) -> Vec<(f32, f32)> {
    let mut white = Vec::with_capacity(frames);
    let mut state = seed;
    for _ in 0..frames {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let u = ((state >> 11) as f64 / 9007199254740992.0) * 2.0 - 1.0;
        white.push(u);
    }
    let n = frames.next_power_of_two();
    let spec = Spectrum::from_time_with_len(&white, n, FS).unwrap();
    let mut bins = spec.bins().to_vec();
    for (j, b) in bins.iter_mut().enumerate() {
        let f = j as f64 * FS / n as f64;
        let gain = if f < 1.0 {
            1.0
        } else {
            1.0 / (f / 1000.0).sqrt()
        };
        b.re *= gain;
        b.im *= gain;
    }
    let shaped = Spectrum::from_bins(bins, FS).unwrap().to_time();
    shaped
        .iter()
        .take(frames)
        .map(|&s| {
            let v = (s * 0.25).clamp(-1.0, 1.0) as f32;
            (v, v)
        })
        .collect()
}

/// Octave-band power levels (dB, arbitrary reference) of `samples` via FFT.
fn octave_bands_db(samples: &[f32], fs: f64) -> Vec<(f64, f64)> {
    let n = samples.len().next_power_of_two();
    let padded: Vec<f64> = samples.iter().map(|&s| s as f64).collect();
    let spec = Spectrum::from_time_with_len(&padded, n, fs).unwrap();
    const CENTERS: [f64; 9] = [
        63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
    ];
    CENTERS
        .iter()
        .map(|&fc| {
            let f_lo = fc / 2.0f64.sqrt();
            let f_hi = fc * 2.0f64.sqrt();
            let mut p = 0.0f64;
            for (j, b) in spec.bins()[..=n / 2].iter().enumerate() {
                let f = j as f64 * fs / n as f64;
                if (f_lo..f_hi).contains(&f) {
                    p += b.norm_sqr() * 2.0; // + mirror
                }
            }
            (fc, 10.0 * p.max(1e-18).log10())
        })
        .collect()
}

#[test]
fn disabled_correction_is_bit_exact() {
    // The same input through a correction-loaded-but-disabled graph and a
    // never-touched graph must be bit-identical: an inactive correction
    // step returns without touching a sample.
    let input = pink_noise(FRAMES, 7);
    let set = correction_set(PhaseMode::Minimum);
    let with_disabled = run_through_graph(Some(&set), false, &input);
    let without = run_through_graph(None, false, &input);
    assert_eq!(with_disabled.len(), without.len());
    for (i, (a, b)) in with_disabled.iter().zip(without.iter()).enumerate() {
        assert!(
            a.to_bits() == b.to_bits(),
            "disabled correction is not bit-exact at frame {i}: {a} vs {b}"
        );
    }
}

#[test]
fn all_phase_modes_identical_magnitude() {
    // Min / linear / hybrid renders share the correction magnitude — the
    // graph outputs must match within ±0.01 dB regardless of phase mode.
    // A single-bin FFT of noise has Rayleigh variance, so the probe is a
    // small impulse: the graph's impulse response is deterministic and its
    // magnitude spectrum is exactly the correction's (the chain is
    // transparent otherwise).
    let capture = N * 2 + 4096;
    let mut input = vec![(0.0f32, 0.0f32); capture];
    input[0] = (0.1, 0.1);

    let min_set = correction_set(PhaseMode::Minimum);
    let lin_set = correction_set(PhaseMode::Linear);
    let hyb_set = correction_set(PhaseMode::Hybrid);
    let min = run_through_graph(Some(&min_set), true, &input);
    let lin = run_through_graph(Some(&lin_set), true, &input);
    let hyb = run_through_graph(Some(&hyb_set), true, &input);

    // Each set carries its own `peak_scale` safety rail (a single broadband
    // gain that legitimately differs per phase mode — the linear IR's peak
    // is lower than the min-phase IR's). The magnitude-identity contract is
    // about the correction RESPONSE, so the comparison normalizes it out.
    let mag_db = |buf: &[f32], scale: f64| -> Vec<(f64, f64)> {
        // An impulse input makes everything outside the chain's IR exactly
        // zero, so the whole capture FFTs to the exact IR magnitude (no
        // windowing). The capture is long enough for the linear IR's
        // n/2-centered energy plus the chain delay.
        let n = capture.next_power_of_two();
        let data: Vec<f64> = buf.iter().map(|&s| s as f64).collect();
        let spec = Spectrum::from_time_with_len(&data, n, FS).unwrap();
        let ref_db = 20.0 * scale.log10();
        spec.bins()[..=n / 2]
            .iter()
            .enumerate()
            .filter(|(j, _)| {
                let f = *j as f64 * FS / n as f64;
                (40.0..=16_000.0).contains(&f)
            })
            .map(|(j, b)| {
                (
                    j as f64 * FS / n as f64,
                    20.0 * b.norm().max(1e-12).log10() - ref_db,
                )
            })
            .collect()
    };
    let (a, b, c) = (
        mag_db(&min, min_set.peak_scale),
        mag_db(&lin, lin_set.peak_scale),
        mag_db(&hyb, hyb_set.peak_scale),
    );
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len(), c.len());
    for (i, (&(fa, x), &(fb, y))) in a.iter().zip(b.iter()).enumerate() {
        assert!(
            (x - y).abs() <= 0.01,
            "min vs linear magnitude differs {:.4} dB at {:.0} Hz",
            (x - y).abs(),
            fa
        );
        assert!(
            (x - c[i].1).abs() <= 0.01,
            "min vs hybrid magnitude differs {:.4} dB at {:.0} Hz (min {x:.3}, hyb {:.3})",
            (x - c[i].1).abs(),
            fb,
            c[i].1
        );
    }
}

#[test]
fn corrected_room_residual_within_half_db() {
    // Pink noise through the room (convolved in, as a recording would be),
    // then through the correction → flat response: octave-band residual
    // within ±0.5 dB, 40 Hz–16 kHz (relative to the set's own broadband
    // gain — conditioning normalization / the peak_scale rail are not
    // pinned).
    let set = correction_set(PhaseMode::Minimum);
    let pink = pink_noise(FRAMES, 13);
    // room ⊛ pink — the recording of noise played through the room, which
    // the correction must flatten back to the pink input.
    let room = room_ir();
    let mono: Vec<f64> = pink.iter().map(|(l, _)| *l as f64).collect();
    let roomed: Vec<f64> = convolve(&mono, &room);
    let input: Vec<(f32, f32)> = roomed
        .iter()
        .take(FRAMES) // drop the room's ring-out tail; lengths must match
        .map(|&s| (s as f32, s as f32))
        .collect();
    let dry = run_through_graph(None, false, &pink);
    let corrected = run_through_graph(Some(&set), true, &input);

    let skip = 4800;
    let in_bands = octave_bands_db(&dry[skip..], FS);
    let out_bands = octave_bands_db(&corrected[skip..], FS);

    // Broadband reference: the mean residual over the 125 Hz–10 kHz region
    // (the correction's own applied gain + the room's average).
    let delta: Vec<(f64, f64)> = out_bands
        .iter()
        .zip(in_bands.iter())
        .map(|((fc, o), (_, i))| (*fc, o - i))
        .collect();
    let reference: f64 = delta
        .iter()
        .filter(|(fc, _)| (125.0..=10_000.0).contains(fc))
        .map(|(_, d)| *d)
        .sum::<f64>()
        / delta
            .iter()
            .filter(|(fc, _)| (125.0..=10_000.0).contains(fc))
            .count() as f64;

    for &(fc, d) in &delta {
        let residual = (d - reference).abs();
        assert!(
            residual <= 0.5,
            "octave band {fc:.0} Hz residual {residual:.3} dB exceeds ±0.5 dB \
             (delta {d:.3}, reference {reference:.3})"
        );
    }
}

#[test]
fn reported_latency_matches_ir_group_delay() {
    // The node's declared latency must equal the IR set's phase-mode group
    // delay (0 for minimum phase; n/2 for linear), and it must flow into
    // the graph's total latency and the latency report.
    let min = correction_set(PhaseMode::Minimum);
    let lin = correction_set(PhaseMode::Linear);

    let mut graph = DspGraph::from_config(&EngineConfig::default(), FS as f32);
    graph.load_correction_ir(Arc::new(min.clone()));
    graph.set_correction_enabled(true);
    graph.drain_queued_control();
    assert!(graph.correction().is_active());

    let min_ms = graph.correction().latency_ms(FS as f32);
    assert!(
        (min_ms - (min.delay_samples as f32 / FS as f32) * 1000.0).abs() < 1e-3,
        "min-phase latency {min_ms} ms != declared delay {} ms",
        (min.delay_samples as f32 / FS as f32) * 1000.0
    );
    assert!(min_ms == 0.0, "min-phase correction must add no latency");
    // The partition block is the engine's own convolution artifact (one
    // 512-sample block), so the reported total sits in [delay, delay + block].
    let block_ms = 512.0 / FS as f32 * 1000.0;

    graph.load_correction_ir(Arc::new(lin.clone()));
    graph.drain_queued_control();
    let lin_ms = graph.correction().latency_ms(FS as f32);
    let lin_delay_ms = (lin.delay_samples / FS) * 1000.0;
    assert!(
        (lin_ms as f64 - lin_delay_ms).abs() < 1e-3,
        "linear-phase latency {lin_ms} ms != declared delay {lin_delay_ms} ms"
    );
    assert!(lin_delay_ms > 0.0, "linear phase must declare a real delay");

    // The latency report and total must include the correction term.
    let report = graph.latency_report(0.0, 0.0, 0.0);
    assert!(
        (report.correction_latency_ms - lin_ms).abs() < 1e-3,
        "latency report correction term {} ms != node latency {lin_ms} ms",
        report.correction_latency_ms
    );
    let total = graph.total_latency_ms();
    assert!(
        total >= lin_ms && total <= lin_ms + block_ms,
        "total latency {total} ms outside [{lin_ms}, {lin_ms} + {block_ms}] ms"
    );
}
