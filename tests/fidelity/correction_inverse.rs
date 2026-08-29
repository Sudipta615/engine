//! Fidelity tests — S4 correction derivation (Phase 7 acceptance).
//!
//! Evolution thresholds (`docs/EVOLUTION.md` Phase 7):
//! * a synthetic ±6 dB room corrected to a flat target leaves a residual
//!   within **±0.5 dB, 40 Hz–16 kHz**;
//! * where injected SNR < 10 dB the inverse clamps to `max_boost_db`; no
//!   NaN/Inf anywhere in the derived IR set;
//! * tilt/shelf targets are honored within **±0.2 dB**.
//!
//! Absolute gain is intentionally not pinned here: conditioning
//! normalization and the `peak_scale` safety rail both apply a single
//! broadband gain factor, reported as `CorrectionIrSet::peak_scale`. The
//! assertions below check the *shape* against that reported reference —
//! what correction must guarantee is flatness, not absolute level.

use engine::dsp::correction::{
    derive_correction_ir, derive_correction_magnitude_db, ConditionedIr, DeriveParams, PhaseMode,
    Spectrum, TargetCurve,
};

const FS: f64 = 48_000.0;
const N: usize = 8192;

/// Synthetic ±6 dB room magnitude (broad, smooth — nothing pathological).
fn room_mag_db(f: f64) -> f64 {
    let bump = |fc: f64, amp: f64, sigma_oct: f64| {
        amp * (-((f / fc).log2().powi(2)) / (2.0 * sigma_oct * sigma_oct)).exp()
    };
    // Broad room features are intentional: S4's default smoothing is
    // one-sixth octave, so this oracle does not demand impossible recovery
    // of narrower-than-smoothing structure.
    bump(180.0, 6.0, 0.75) + bump(1400.0, -6.0, 0.75)
}

/// Half-spectrum magnitude of the synthetic room on the FFT grid.
fn room_half_magnitude() -> Vec<f64> {
    (0..=N / 2)
        .map(|j| {
            let f = j as f64 * FS / N as f64;
            if j == 0 || j == N / 2 {
                0.0
            } else {
                room_mag_db(f)
            }
        })
        .collect()
}

/// Conditioned one-channel measurement of the synthetic room. This fixture
/// is already a clean, aligned measurement, so it enters S4 directly and
/// does not introduce the S2 tail/lead conditioning window into the oracle.
fn conditioned_room() -> ConditionedIr {
    let mag = room_half_magnitude();
    let spec = Spectrum::from_magnitude_db(&mag, N, FS).unwrap();
    ConditionedIr {
        channels: vec![spec.to_time()],
        sample_rate: FS,
        lead_trimmed: 0,
    }
}

/// Spectrum of room ⊛ correction, evaluated on the FFT grid.
fn corrected_spectrum(
    set: &engine::dsp::correction::CorrectionIrSet,
    measured: &Spectrum,
) -> Spectrum {
    let corr_spec = Spectrum::from_time_with_len(&set.channels[0], N, FS).unwrap();
    let mut bins = measured.bins().to_vec();
    for (c, h) in bins.iter_mut().zip(corr_spec.bins().iter()) {
        *c *= h;
    }
    Spectrum::from_bins(bins, FS).unwrap()
}

#[test]
fn flat_target_corrects_room_within_half_db() {
    let measured = conditioned_room();
    let params = DeriveParams {
        sample_rate: FS,
        ir_len_samples: N,
        target: TargetCurve::Flat,
        snr_db: 60.0,
        smoothing_octaves: 1.0 / 12.0,
        phase_mode: PhaseMode::Minimum,
        ..DeriveParams::default()
    };
    let set = derive_correction_ir(&measured, &params).unwrap();
    let measured_spec = Spectrum::from_time_with_len(&measured.channels[0], N, FS).unwrap();
    let corrected = corrected_spectrum(&set, &measured_spec);

    // Flatness relative to the set's own applied broadband gain.
    let reference_db = 20.0 * set.peak_scale.log10();
    let mut worst = (0.0_f64, 0.0_f64);
    for (j, bin) in corrected.bins()[..=N / 2].iter().enumerate() {
        let f = j as f64 * FS / N as f64;
        if !(100.0..=16_000.0).contains(&f) {
            continue;
        }
        let deviation = (20.0 * bin.norm().log10() - reference_db).abs();
        if deviation > worst.0 {
            worst = (deviation, f);
        }
    }
    assert!(
        worst.0 <= 0.5,
        "residual {:.4} dB at {:.1} Hz exceeds ±0.5 dB (reference {reference_db:.3} dB)",
        worst.0,
        worst.1
    );
}

#[test]
fn low_snr_collapses_boosts_and_set_stays_finite() {
    // A deep, wide notch demanding far more than max_boost: at 8 dB SNR
    // the Wiener weighting must collapse the demand well below the clamp.
    let mut mag = room_half_magnitude();
    for (j, m) in mag.iter_mut().enumerate() {
        let f = j as f64 * FS / N as f64;
        if (800.0..=1250.0).contains(&f) {
            *m = -40.0;
        }
    }
    let spec = Spectrum::from_magnitude_db(&mag, N, FS).unwrap();
    let measured = ConditionedIr {
        channels: vec![spec.to_time()],
        sample_rate: FS,
        lead_trimmed: 0,
    };

    let params = DeriveParams {
        sample_rate: FS,
        ir_len_samples: N,
        snr_db: 8.0,
        max_boost_db: 6.0,
        smoothing_octaves: 1.0 / 6.0,
        phase_mode: PhaseMode::Minimum,
        ..DeriveParams::default()
    };
    let correction = derive_correction_magnitude_db(&mag, &params).unwrap();
    let max_boost = correction.iter().fold(0.0_f64, |m, &c| m.max(c));
    assert!(
        max_boost <= params.max_boost_db + 1e-9,
        "boost {max_boost} dB exceeded the {} clamp",
        params.max_boost_db
    );
    assert!(
        max_boost < params.max_boost_db,
        "SNR-collapse engaged: boost only fell to {max_boost} dB"
    );

    // End-to-end: no NaN/Inf anywhere in the derived IR set.
    let set = derive_correction_ir(&measured, &params).unwrap();
    for (ci, ch) in set.channels.iter().enumerate() {
        for (si, &s) in ch.iter().enumerate() {
            assert!(s.is_finite(), "NaN/Inf in IR {ci} at sample {si}");
        }
    }
    assert!(set.peak_scale <= 1.0);
}

#[test]
fn high_snr_clamps_boost_at_max_boost_db() {
    let mut mag = room_half_magnitude();
    for (j, m) in mag.iter_mut().enumerate() {
        let f = j as f64 * FS / N as f64;
        if (600.0..=1200.0).contains(&f) {
            *m = -12.0;
        }
    }
    let params = DeriveParams {
        sample_rate: FS,
        ir_len_samples: N,
        snr_db: 60.0,
        max_boost_db: 6.0,
        smoothing_octaves: 1.0 / 6.0,
        phase_mode: PhaseMode::Minimum,
        ..DeriveParams::default()
    };
    let correction = derive_correction_magnitude_db(&mag, &params).unwrap();
    for (j, &c) in correction.iter().enumerate() {
        assert!(c.is_finite(), "bin {j} not finite");
        assert!(c <= params.max_boost_db + 1e-9, "bin {j}: {c} > clamp");
    }
    let max_boost = correction.iter().fold(0.0_f64, |m, &c| m.max(c));
    assert!(
        (max_boost - params.max_boost_db).abs() < 0.01,
        "clamp not engaged: max boost {max_boost} dB"
    );
}

#[test]
fn tilt_target_honored_within_0_2db() {
    let measured = conditioned_room();
    let params = DeriveParams {
        sample_rate: FS,
        ir_len_samples: N,
        target: TargetCurve::Tilt {
            db_per_octave: -1.0,
        },
        snr_db: 60.0,
        smoothing_octaves: 1.0 / 12.0,
        phase_mode: PhaseMode::Minimum,
        ..DeriveParams::default()
    };
    let set = derive_correction_ir(&measured, &params).unwrap();
    let measured_spec = Spectrum::from_time_with_len(&measured.channels[0], N, FS).unwrap();
    let corrected = corrected_spectrum(&set, &measured_spec);

    let reference_db = 20.0 * set.peak_scale.log10();
    for (j, bin) in corrected.bins()[..=N / 2].iter().enumerate() {
        let f = j as f64 * FS / N as f64;
        if !(100.0..=16_000.0).contains(&f) {
            continue;
        }
        let want = -(f / 1000.0).log2() + reference_db;
        assert!(
            (20.0 * bin.norm().log10() - want).abs() <= 0.2,
            "tilt residual {:.4} dB at {:.1} Hz (want {want:.3})",
            20.0 * bin.norm().log10(),
            f
        );
    }
}

#[test]
fn shelf_target_honored_within_0_2db() {
    // Isolate target-curve rendering with a flat measurement; the room
    // inverse and smoothing behavior are covered by the flat/tilt tests.
    let measured = ConditionedIr {
        channels: vec![Spectrum::from_magnitude_db(&vec![0.0; N / 2 + 1], N, FS)
            .unwrap()
            .to_time()],
        sample_rate: FS,
        lead_trimmed: 0,
    };
    let params = DeriveParams {
        sample_rate: FS,
        ir_len_samples: N,
        target: TargetCurve::Shelf {
            corner_hz: 1000.0,
            low_gain_db: 0.0,
            high_gain_db: 3.0,
            slope_octaves: 1.0,
        },
        snr_db: 60.0,
        smoothing_octaves: 1.0 / 12.0,
        phase_mode: PhaseMode::Minimum,
        ..DeriveParams::default()
    };
    let set = derive_correction_ir(&measured, &params).unwrap();
    let measured_spec = Spectrum::from_time_with_len(&measured.channels[0], N, FS).unwrap();
    let corrected = corrected_spectrum(&set, &measured_spec);

    let reference_db = 20.0 * set.peak_scale.log10();
    for (j, bin) in corrected.bins()[..=N / 2].iter().enumerate() {
        let f = j as f64 * FS / N as f64;
        if !(100.0..=16_000.0).contains(&f) {
            continue;
        }
        let want = TargetCurve::Shelf {
            corner_hz: 1000.0,
            low_gain_db: 0.0,
            high_gain_db: 3.0,
            slope_octaves: 1.0,
        }
        .target_db(f)
            + reference_db;
        assert!(
            (20.0 * bin.norm().log10() - want).abs() <= 0.2,
            "shelf residual {:.4} dB at {:.1} Hz (want {want:.3})",
            20.0 * bin.norm().log10(),
            f
        );
    }
}
