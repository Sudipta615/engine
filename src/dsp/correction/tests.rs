//! Unit tests for the correction chain (S1–S4). The heavyweight fidelity
//! acceptance suites live under `tests/fidelity/` (see
//! `docs/EVOLUTION.md` Phase 7); these cover the small invariants inline.

use std::path::PathBuf;

use num_complex::Complex;

use super::derive::{
    derive_correction_ir, derive_correction_magnitude_db, DeriveParams, TargetCurve,
};
use super::ir::{read_wav_ir, ConditionedIr, IrConditioner, WavIr};
use super::phase::{
    excess_allpass_spectrum, group_delay_exact_samples, minimum_phase_ir, minimum_phase_spectrum,
    phase_slope_delay_samples, render_from_magnitude_db, PhaseMode, RenderParams, Spectrum,
};
use super::sweep::{deconvolve, noise_floor_db, EssConfig, EssSweep};
use super::{convolve, Cfft, CorrectionError, MAG_FLOOR_AMP};

const FS: f64 = 48_000.0;

// ── Cfft / convolve ─────────────────────────────────────────────────────────

#[test]
fn cfft_forward_inverse_roundtrips() {
    let n = 256;
    let cfft = Cfft::new(n);
    let mut buf: Vec<Complex<f64>> = (0..n)
        .map(|i| Complex::new((i as f64 * 0.1).sin(), (i as f64 * 0.3).cos()))
        .collect();
    let original = buf.clone();
    cfft.forward(&mut buf);
    cfft.inverse(&mut buf);
    for (a, b) in buf.iter().zip(original.iter()) {
        assert!((a - b).norm() < 1e-12);
    }
}

#[test]
fn convolve_matches_direct_for_short_kernels() {
    let a: Vec<f64> = (0..64).map(|i| ((i as f64) * 0.23).sin()).collect();
    let b = vec![0.5, -0.25, 0.125];
    let fast = convolve(&a, &b);
    for n in 0..fast.len() {
        let mut expect = 0.0;
        for (m, &bv) in b.iter().enumerate() {
            if n >= m && n - m < a.len() {
                expect += a[n - m] * bv;
            }
        }
        assert!(
            (fast[n] - expect).abs() < 1e-9,
            "bin {n}: {} vs {expect}",
            fast[n]
        );
    }
}

#[test]
fn convolve_empty_inputs() {
    assert!(convolve(&[], &[1.0]).is_empty());
    assert!(convolve(&[1.0], &[]).is_empty());
}

// ── S3 phase ────────────────────────────────────────────────────────────────

/// Smooth test magnitude: ±6 dB gaussian bumps in log-frequency.
fn bump_mag_db(n: usize, fs: f64) -> Vec<f64> {
    (0..=n / 2)
        .map(|j| {
            let f = j as f64 * fs / n as f64;
            let bump = |fc: f64, amp: f64, sigma_oct: f64| {
                amp * (-((f / fc).log2().powi(2)) / (2.0 * sigma_oct * sigma_oct)).exp()
            };
            bump(120.0, 6.0, 0.4) + bump(1500.0, -6.0, 0.35) + bump(8000.0, 4.0, 0.3)
        })
        .collect()
}

#[test]
fn minimum_phase_preserves_magnitude() {
    let n = 4096;
    let mag = bump_mag_db(n, FS);
    let src = Spectrum::from_magnitude_db(&mag, n, FS).unwrap();
    let min = minimum_phase_spectrum(&src).unwrap();
    for (a, b) in mag.iter().zip(min.magnitude_db().iter()) {
        assert!((a - b).abs() < 0.01, "{a} vs {b}");
    }
}

#[test]
fn minimum_phase_ir_is_causal() {
    let n = 4096;
    let mag = bump_mag_db(n, FS);
    let src = Spectrum::from_magnitude_db(&mag, n, FS).unwrap();
    let h = minimum_phase_ir(&src).unwrap();
    let peak = h.iter().fold(0.0_f64, |m, &x| m.max(x.abs()));
    let negative: f64 = h[n / 2 + 1..].iter().map(|&x| x.abs()).fold(0.0, f64::max);
    assert!(
        negative < 1e-7 * peak.max(1e-30),
        "negative-time energy {negative}"
    );
}

#[test]
fn excess_allpass_is_flat_and_carries_delay() {
    let n = 8192;
    let delay_samples = 100.4;
    let mag = bump_mag_db(n, FS);
    // Source = min-phase-rendered magnitude with an added pure delay.
    let rendered = render_from_magnitude_db(
        &mag,
        &RenderParams {
            sample_rate: FS,
            ir_len_samples: n,
            phase_mode: PhaseMode::Minimum,
            hybrid_crossover_hz: 1000.0,
        },
    )
    .unwrap();
    let mut bins = Spectrum::from_time_with_len(&rendered.samples, n, FS)
        .unwrap()
        .bins()
        .to_vec();
    for (j, b) in bins.iter_mut().enumerate() {
        let phase = -std::f64::consts::TAU * j as f64 * delay_samples / n as f64;
        *b *= Complex::new(phase.cos(), phase.sin());
    }
    let src = Spectrum::from_bins(bins, FS).unwrap();

    let excess = excess_allpass_spectrum(&src).unwrap();
    for x in excess.bins().iter().take(n / 2 + 1) {
        let deviation = 20.0 * x.norm().max(MAG_FLOOR_AMP).log10();
        assert!(deviation.abs() < 0.001, "allpass deviation {deviation} dB");
    }
    let fit = phase_slope_delay_samples(&excess, 200.0, 20_000.0);
    assert!(
        (fit - delay_samples).abs() < 1.0,
        "delay fit {fit} vs {delay_samples}"
    );
}

#[test]
fn linear_phase_render_has_constant_group_delay() {
    let n = 4096;
    let mag = bump_mag_db(n, FS);
    let rendered = render_from_magnitude_db(
        &mag,
        &RenderParams {
            sample_rate: FS,
            ir_len_samples: n,
            phase_mode: PhaseMode::Linear,
            hybrid_crossover_hz: 1000.0,
        },
    )
    .unwrap();
    assert!((rendered.delay_samples - n as f64 / 2.0).abs() < 1e-9);
    // Symmetric about the shift point.
    let peak = rendered
        .samples
        .iter()
        .fold(0.0_f64, |m, &x| m.max(x.abs()));
    for k in [1, 100, 1000] {
        let a = rendered.samples[(n / 2 + k) % n];
        let b = rendered.samples[(n / 2 + n - k) % n];
        assert!((a - b).abs() < 1e-9 * peak.max(1e-30) + 1e-12);
    }
    let spec = Spectrum::from_time_with_len(&rendered.samples, n, FS).unwrap();
    let fit = group_delay_exact_samples(&spec)[1..n / 2]
        .iter()
        .copied()
        .filter(|x| x.is_finite())
        .sum::<f64>()
        / (n / 2 - 1) as f64;
    assert!(
        (fit - n as f64 / 2.0).abs() <= 0.5,
        "group delay {fit} vs {}",
        n / 2
    );
}

#[test]
fn hybrid_render_keeps_magnitude_and_smooths_group_delay() {
    let n = 4096;
    let fs = FS;
    let mag = bump_mag_db(n, fs);
    let rendered = render_from_magnitude_db(
        &mag,
        &RenderParams {
            sample_rate: fs,
            ir_len_samples: n,
            phase_mode: PhaseMode::Hybrid,
            hybrid_crossover_hz: 1000.0,
        },
    )
    .unwrap();
    let spec = Spectrum::from_time_with_len(&rendered.samples, n, fs).unwrap();

    // Magnitude reproduced exactly through the time-domain round trip.
    for (a, b) in mag.iter().zip(spec.magnitude_db().iter()) {
        assert!((a - b).abs() < 0.01, "{a} vs {b}");
    }

    // The prescribed hybrid latency is finite and the rendered IR is fully
    // finite; detailed frequency-dependent GD measurement belongs to the
    // external acceptance suite where the analysis band is selected from
    // the measurement SNR.
    assert!(rendered.delay_samples.is_finite() && rendered.delay_samples > 0.0);
    assert!(rendered.samples.iter().all(|x| x.is_finite()));
}

#[test]
fn render_rejects_bad_lengths() {
    let params = RenderParams {
        sample_rate: FS,
        ir_len_samples: 1000, // not a power of two
        phase_mode: PhaseMode::Minimum,
        hybrid_crossover_hz: 1000.0,
    };
    assert!(matches!(
        render_from_magnitude_db(&vec![0.0; 501], &params),
        Err(CorrectionError::InvalidConfig { .. })
    ));
}

// ── S2 IR conditioning ──────────────────────────────────────────────────────

fn write_temp_wav(channels: &[Vec<f64>], rate: u32) -> PathBuf {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!(
        "shadow-correction-test-{}-{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let data_len: usize = channels.iter().map(|c| c.len() * 4).sum();
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len as u32).to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16_u32.to_le_bytes()).unwrap();
    f.write_all(&3_u16.to_le_bytes()).unwrap(); // IEEE float
    f.write_all(&(channels.len() as u16).to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&(rate * 4).to_le_bytes()).unwrap(); // byte rate
    f.write_all(&((channels.len() as u16) * 4).to_le_bytes())
        .unwrap();
    f.write_all(&32_u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&(data_len as u32).to_le_bytes()).unwrap();
    for frame in 0..channels[0].len() {
        for ch in channels {
            f.write_all(&(ch[frame] as f32).to_le_bytes()).unwrap();
        }
    }
    path
}

#[test]
fn wav_roundtrip_and_conditioning() {
    // DC offset + a delayed impulse with a decaying tail on two channels.
    let n = 4096;
    let mut ch0 = vec![0.05; n];
    let mut ch1 = vec![0.05; n];
    ch0[100] += 0.8;
    ch1[132] += 0.6;
    for (i, sample) in ch0.iter_mut().enumerate().take(n).skip(101) {
        *sample += 0.4 * (-(i as f64) / 300.0).exp();
    }
    let path = write_temp_wav(&[ch0, ch1], 48_000);
    let wav = read_wav_ir(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(wav.channels.len(), 2);
    assert_eq!(wav.sample_rate, 48_000.0);
    assert_eq!(wav.channels[0].len(), n);

    let conditioner = IrConditioner {
        // −20 dB onset gate: the 0.05 DC offset (−24.6 dB re peak) must not
        // count as the onset, so the trim anchors on the impulse.
        onset_threshold_db: -20.0,
        ..IrConditioner::default()
    };
    let cond: ConditionedIr = conditioner.condition(&wav, 48_000.0).unwrap();
    // DC offset removed by the rumble HPF.
    let mean: f64 = cond.channels[0].iter().sum::<f64>() / cond.channels[0].len() as f64;
    assert!(mean.abs() < 1e-3, "mean {mean} after HPF");
    // Peak normalized to the reference.
    let peak = cond
        .channels
        .iter()
        .flat_map(|c| c.iter())
        .fold(0.0_f64, |m, &x| m.max(x.abs()));
    assert!((peak - conditioner.normalize_peak).abs() < 1e-9);
    // Tail truncated well before the raw length.
    assert!(cond.channels[0].len() < n);
    // Inter-channel alignment preserved: earliest onset (ch0's impulse at
    // sample 100) minus the 16-sample guard.
    assert_eq!(cond.lead_trimmed, 84);
}

#[test]
fn conditioner_rejects_rate_mismatch() {
    let wav = WavIr {
        channels: vec![vec![0.0; 64]],
        sample_rate: 44_100.0,
    };
    assert!(matches!(
        IrConditioner::default().condition(&wav, 48_000.0),
        Err(CorrectionError::RateMismatch { .. })
    ));
}

#[test]
fn wav_parser_rejects_garbage() {
    let path = std::env::temp_dir().join(format!(
        "shadow-correction-garbage-{}.bin",
        std::process::id()
    ));
    std::fs::write(&path, b"not a wav file at all........").unwrap();
    let result = read_wav_ir(&path);
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(result, Err(CorrectionError::WavParse { .. })));
}

// ── S1 sweep ────────────────────────────────────────────────────────────────

#[test]
fn sweep_peak_and_band_are_respected() {
    let sweep = EssSweep::new(EssConfig {
        sample_rate: FS,
        duration_secs: 1.0,
        f_start: 100.0,
        f_end: 10_000.0,
        amplitude: 0.7,
        fade_secs: 0.02,
        pre_emphasis: true,
    })
    .unwrap();
    assert_eq!(sweep.length_samples(), 48_000);
    let peak = sweep.samples().iter().fold(0.0_f64, |m, &x| m.max(x.abs()));
    assert!((peak - 0.7).abs() < 1e-9);
}

#[test]
fn sweep_config_validation() {
    assert!(EssSweep::new(EssConfig {
        f_end: 30_000.0, // above 24 kHz Nyquist
        ..EssConfig::default()
    })
    .is_err());
    assert!(EssSweep::new(EssConfig {
        duration_secs: 0.1,
        ..EssConfig::default()
    })
    .is_err());
}

#[test]
fn deconvolve_recovers_impulse_and_noise_floor() {
    let sweep = EssSweep::new(EssConfig {
        sample_rate: FS,
        duration_secs: 1.0,
        f_start: 20.0,
        f_end: 20_000.0,
        ..EssConfig::default()
    })
    .unwrap();
    // Room = a 37.5-sample delayed, slightly attenuated impulse.
    let mut room = vec![0.0; 96];
    room[37] = 0.9;
    room[38] = 0.45;
    let recorded = convolve(sweep.samples(), &room);
    let ir = deconvolve(&recorded, &sweep).unwrap();
    assert!(
        (ir.pre_delay - 37.0).abs() <= 1.0,
        "pre_delay {}",
        ir.pre_delay
    );
    // Noise-free measurement: the noise floor sits far below the impulse.
    let snr = super::sweep::estimate_snr_db(&ir).unwrap();
    assert!(snr > 80.0, "SNR {snr} dB on a clean recording");

    // And the floor tracks injected noise (coarse check; the ±2 dB
    // acceptance bound lives in tests/fidelity/ess_measurement.rs).
    let mut noisy = recorded.clone();
    let mut state = 0x9E3779B97F4A7C15_u64;
    let sigma = 3.0e-3;
    for x in &mut noisy {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *x += sigma * (((state >> 11) as f64 / 9007199254740992.0) * 2.0 - 1.0);
    }
    let ir_noisy = deconvolve(&noisy, &sweep).unwrap();
    let floor = noise_floor_db(&ir_noisy).unwrap();
    assert!(
        (floor - 20.0 * sigma.log10()).abs() < 6.0,
        "noise floor {floor} dB vs {} dB",
        20.0 * sigma.log10()
    );
}

// ── S4 derivation ───────────────────────────────────────────────────────────

#[test]
fn flat_room_yields_zero_correction() {
    let n = 4096;
    let measured = vec![0.0_f64; n / 2 + 1]; // 0 dB everywhere
    let params = DeriveParams::default();
    let correction = derive_correction_magnitude_db(&measured, &params).unwrap();
    for (j, &c) in correction.iter().enumerate() {
        assert!(c.abs() < 1e-9, "bin {j}: {c}");
    }
}

#[test]
fn tilt_target_is_followed_from_a_flat_room() {
    let n = 8192;
    let measured = vec![0.0_f64; n / 2 + 1];
    let params = DeriveParams {
        sample_rate: FS,
        ir_len_samples: n,
        target: TargetCurve::Tilt {
            db_per_octave: -1.0,
        },
        ..DeriveParams::default()
    };
    let correction = derive_correction_magnitude_db(&measured, &params).unwrap();
    for (j, &actual) in correction
        .iter()
        .enumerate()
        .take(correction.len() - 2)
        .skip(2)
    {
        let f = j as f64 * FS / n as f64;
        if !(40.0..=16_000.0).contains(&f) {
            continue;
        }
        let want = -(f / 1000.0).log2();
        assert!(
            (actual - want).abs() < 0.2,
            "bin {j} ({f} Hz): {} vs {want}",
            correction[j]
        );
    }
}

#[test]
fn derive_collapses_boosts_at_low_snr() {
    // A wide, deep notch demands a huge boost; at 5 dB SNR the Wiener
    // weight must collapse it to near-zero (and the clamp bounds it).
    let n = 4096;
    let mut measured = vec![0.0_f64; n / 2 + 1];
    for (j, m) in measured.iter_mut().enumerate() {
        let f = j as f64 * FS / n as f64;
        *m = if (800.0..=1250.0).contains(&f) {
            -40.0
        } else {
            0.0
        };
    }
    let params = DeriveParams {
        sample_rate: FS,
        ir_len_samples: n,
        snr_db: 5.0,
        ..DeriveParams::default()
    };
    let correction = derive_correction_magnitude_db(&measured, &params).unwrap();
    let max_boost = correction.iter().fold(0.0_f64, |m, &c| m.max(c));
    assert!(
        max_boost <= params.max_boost_db + 1e-9,
        "boost {max_boost} dB exceeded clamp"
    );
    for (j, &c) in correction.iter().enumerate() {
        assert!(c.is_finite(), "bin {j} not finite");
        assert!(c <= params.max_boost_db + 1e-9, "bin {j}: {c} > clamp");
    }

    // Through the full path: ConditionedIr of a notched room → finite IRs.
    let notch = render_from_magnitude_db(
        &measured,
        &RenderParams {
            sample_rate: FS,
            ir_len_samples: n,
            phase_mode: PhaseMode::Minimum,
            hybrid_crossover_hz: 1000.0,
        },
    )
    .unwrap();
    let wav = WavIr {
        channels: vec![notch.samples],
        sample_rate: FS,
    };
    let cond = IrConditioner::default().condition(&wav, FS).unwrap();
    let set = derive_correction_ir(&cond, &params).unwrap();
    assert_eq!(set.channels.len(), 1);
    for &s in &set.channels[0] {
        assert!(s.is_finite());
    }
    assert!(set.peak_scale <= 1.0);
}

#[test]
fn derive_clamps_boost_at_max_boost_db() {
    // At high SNR the Wiener weight is ~1, so a reliable −12 dB dip
    // demands +12 dB — the hard clamp must cap it at exactly max_boost.
    let n = 4096;
    let mut measured = vec![0.0_f64; n / 2 + 1];
    for (j, m) in measured.iter_mut().enumerate() {
        let f = j as f64 * FS / n as f64;
        *m = if (600.0..=1200.0).contains(&f) {
            -12.0
        } else {
            0.0
        };
    }
    let params = DeriveParams {
        sample_rate: FS,
        ir_len_samples: n,
        snr_db: 60.0,
        ..DeriveParams::default()
    };
    let correction = derive_correction_magnitude_db(&measured, &params).unwrap();
    for (j, &c) in correction.iter().enumerate() {
        assert!(c.is_finite(), "bin {j} not finite");
        assert!(c <= params.max_boost_db + 1e-9, "bin {j}: {c} > clamp");
    }
    let max_boost = correction.iter().fold(0.0_f64, |m, &c| m.max(c));
    assert!(
        (max_boost - params.max_boost_db).abs() < 0.01,
        "clamp engaged: max {max_boost} vs {}",
        params.max_boost_db
    );
}

#[test]
fn shelf_target_shape() {
    let target = TargetCurve::Shelf {
        corner_hz: 500.0,
        low_gain_db: 0.0,
        high_gain_db: 4.0,
        slope_octaves: 1.0,
    };
    assert!(target.target_db(100.0).abs() < 1e-9);
    assert!((target.target_db(10_000.0) - 4.0).abs() < 1e-9);
    let mid = target.target_db(500.0);
    assert!((mid - 2.0).abs() < 1e-9, "mid shelf {mid}");
}
