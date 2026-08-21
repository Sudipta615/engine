//! Fidelity tests — Limiter correctness
//!
//! Verifies that the `LookaheadLimiter` behaves correctly under:
//! * 0 dBFS sine → sample peak must not exceed ceiling
//! * Single-sample impulse → predictive envelope must prevent overshoot
//! * Sample-peak vs FIR true-peak: near-Nyquist sine triggers FIR but not SP
//! * Soft-clip vs transparent: both must respect ceiling, character differs

use engine::dsp::limiter::{LimiterMode, LookaheadLimiter, TruePeakMode};

const SR: f32 = 48000.0;
const CEILING_DB: f32 = -0.3;
const CEILING_LIN: f32 = 0.966; // 10^(-0.3/20)

fn make_limiter(mode: LimiterMode, tp: TruePeakMode) -> LookaheadLimiter {
    let mut lim = LookaheadLimiter::new_with_mode(SR, 5.0, 0.5, 100.0, CEILING_DB, mode);
    lim.set_true_peak_mode(tp);
    lim
}

/// Feed `n` samples, return max absolute output.
fn max_out_abs(lim: &mut LookaheadLimiter, signal: impl Iterator<Item = (f32, f32)>) -> f32 {
    // warm up
    let signal: Vec<_> = signal.collect();
    let warmup = ((SR as usize) * 5 / 1000).max(256); // 5 ms
    for _ in 0..warmup {
        lim.process(0.0, 0.0);
    }
    signal
        .into_iter()
        .map(|(l, r)| {
            let (ol, or_) = lim.process(l, r);
            ol.abs().max(or_.abs())
        })
        .fold(0.0_f32, f32::max)
}

#[test]
fn limiter_0dbfs_sine_stays_below_ceiling_sample_peak() {
    let sr = SR;
    let freq = 440.0_f32;
    let n = (sr / freq * 100.0) as usize;

    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    let signal = (0..n).map(|i| {
        let s = (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin();
        (s, s)
    });
    let max_out = max_out_abs(&mut lim, signal);
    assert!(
        max_out <= CEILING_LIN + 1e-4,
        "0dBFS sine (SP): output exceeded ceiling: {} vs {}",
        max_out,
        CEILING_LIN
    );
}

#[test]
fn limiter_0dbfs_sine_stays_below_ceiling_fir_peak() {
    let sr = SR;
    let freq = 440.0_f32;
    let n = (sr / freq * 100.0) as usize;

    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::Fir4x);
    let signal = (0..n).map(|i| {
        let s = (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin();
        (s, s)
    });
    let max_out = max_out_abs(&mut lim, signal);
    assert!(
        max_out <= CEILING_LIN + 1e-4,
        "0dBFS sine (FIR): output exceeded ceiling: {} vs {}",
        max_out,
        CEILING_LIN
    );
}

#[test]
fn limiter_impulse_no_overshoot() {
    // A single full-scale impulse must not produce output that exceeds the ceiling
    // after the lookahead window passes.
    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);

    // Pre-silence
    for _ in 0..1000 {
        lim.process(0.0, 0.0);
    }

    // Impulse
    lim.process(2.0, 2.0);

    // Drain lookahead
    let mut max_out = 0.0_f32;
    for _ in 0..1000 {
        let (l, _) = lim.process(0.0, 0.0);
        max_out = max_out.max(l.abs());
    }

    assert!(
        max_out <= CEILING_LIN + 1e-4,
        "Impulse: post-lookahead output exceeded ceiling: {} vs {}",
        max_out,
        CEILING_LIN
    );
}

#[test]
fn limiter_fir_triggers_on_near_nyquist_where_sample_peak_does_not() {
    // A sine at 0.45×(SR/2) with amplitude 0.95 has sample peaks ≤ 0.95.
    // But the inter-sample true peak can exceed the ceiling of 0.966.
    // The FIR detector should trigger gain reduction; the SP detector may not.
    let freq = 0.45 * (SR / 2.0);
    let amplitude = 0.95_f32;
    let n = (SR * 0.5) as usize; // 0.5 s

    let mut sp = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    let mut tp = make_limiter(LimiterMode::Transparent, TruePeakMode::Fir4x);

    let mut sp_min_gain = 1.0_f32;
    let mut tp_min_gain = 1.0_f32;
    for _ in 0..500 {
        sp.process(0.0, 0.0);
        tp.process(0.0, 0.0);
    } // warmup

    for i in 0..n {
        let s = amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
        sp.process(s, s);
        tp.process(s, s);
        sp_min_gain = sp_min_gain.min(sp.current_gain());
        tp_min_gain = tp_min_gain.min(tp.current_gain());
    }

    // FIR should apply more or equal gain reduction
    assert!(
        tp_min_gain <= sp_min_gain + 1e-4,
        "FIR true-peak should apply >= gain reduction: tp_min={} sp_min={}",
        tp_min_gain,
        sp_min_gain
    );
}

#[test]
fn limiter_transparent_respects_ceiling() {
    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    let n = (SR * 1.0) as usize;
    let freq = 100.0_f32;
    for _ in 0..500 {
        lim.process(0.0, 0.0);
    }
    let mut max_out = 0.0_f32;
    for i in 0..n {
        let s = 2.0 * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
        let (l, _) = lim.process(s, s);
        max_out = max_out.max(l.abs());
    }
    assert!(
        max_out <= CEILING_LIN + 1e-4,
        "Transparent mode exceeded ceiling: {} > {}",
        max_out,
        CEILING_LIN
    );
}

#[test]
fn limiter_saturate_respects_ceiling() {
    let mut lim = make_limiter(LimiterMode::Saturate, TruePeakMode::SamplePeak);
    let n = (SR * 1.0) as usize;
    let freq = 100.0_f32;
    for _ in 0..500 {
        lim.process(0.0, 0.0);
    }
    let mut max_out = 0.0_f32;
    for i in 0..n {
        let s = 2.0 * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
        let (l, _) = lim.process(s, s);
        max_out = max_out.max(l.abs());
    }
    assert!(
        max_out <= CEILING_LIN + 1e-4,
        "Saturate mode exceeded ceiling: {} > {}",
        max_out,
        CEILING_LIN
    );
}

#[test]
fn limiter_quiet_signal_passes_through() {
    // A very quiet signal (−30 dBFS) should pass through essentially unchanged.
    let amplitude = 0.031623; // -30 dBFS
    let freq = 440.0_f32;
    let n = (SR / freq * 20.0) as usize;

    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    let delay_samples = ((SR * 0.005) as usize).max(1); // 5 ms lookahead

    // Warm up
    for _ in 0..delay_samples + 256 {
        lim.process(0.0, 0.0);
    }

    let signal: Vec<f32> = (0..n)
        .map(|i| amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin())
        .collect();

    let output: Vec<f32> = signal
        .iter()
        .map(|&s| {
            let (l, _) = lim.process(s, s);
            l
        })
        .collect();

    let ss = n * 3 / 4;
    let in_rms: f32 = (signal[ss..].iter().map(|s| s * s).sum::<f32>() / (n - ss) as f32).sqrt();
    let out_rms: f32 = (output[ss..].iter().map(|s| s * s).sum::<f32>() / (n - ss) as f32).sqrt();
    let ratio = if in_rms > 1e-10 {
        out_rms / in_rms
    } else {
        1.0
    };

    // Should be within 0.1 dB of unity for quiet signals
    let gain_error_db = 20.0 * ratio.log10();
    assert!(
        gain_error_db.abs() < 0.5,
        "Quiet signal should pass through, gain error: {} dB",
        gain_error_db
    );
}

#[test]
fn limiter_gain_meter_nonzero_during_limiting() {
    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    // Feed a loud signal
    for _ in 0..2000 {
        lim.process(2.0, 2.0);
    }
    let gr = lim.gain_reduction_db();
    assert!(
        gr < -0.01,
        "Gain reduction should be non-zero during limiting, got {} dB",
        gr
    );
}

#[test]
fn test_true_peak_fir_polyphase_branch_unity_dc_gain() {
    let dc_gains = LookaheadLimiter::fir_branch_dc_gains();
    for (i, &gain) in dc_gains.iter().enumerate() {
        assert!(
            (gain - 1.0).abs() < 1e-4,
            "Polyphase branch {} DC gain is {}, expected ~1.0 (0.0 dB)",
            i,
            gain
        );
    }
}

#[test]
fn test_true_peak_fir_dc_constant_preservation() {
    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::Fir4x);
    // Warm up FIR state with constant DC = 0.5
    for _ in 0..100 {
        lim.process(0.5, 0.5);
    }
    lim.reset_peak_meters();

    // Now measure steady-state DC true peak
    for _ in 0..200 {
        lim.process(0.5, 0.5);
    }
    let tp = lim.max_true_peak_dbtp();
    let expected_db = 20.0 * 0.5_f32.log10();
    assert!(
        (tp - expected_db).abs() < 0.001,
        "Constant DC true peak error: observed {} dBTP, expected {} dBTP",
        tp,
        expected_db
    );
}

#[test]
fn test_true_peak_fir_detects_intersample_peaks() {
    // Construct a sine wave at fs/4 with a phase shift of 45 degrees (pi/4).
    // The samples at indices n = 0, 1, 2, 3 are:
    // sin(pi/4) = 0.7071, sin(3pi/4) = 0.7071, sin(5pi/4) = -0.7071, sin(7pi/4) = -0.7071.
    // The sample peak is only 0.7071 (approx -3.01 dBFS).
    // The true continuous-time analog peak of this sine is 1.0 (0.0 dBFS, a 3 dB overshoot!).
    let mut tp_lim = make_limiter(LimiterMode::Transparent, TruePeakMode::Fir4x);
    let mut sp_lim = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);

    let freq = 12000.0_f32; // 48000 / 4
    let phase = std::f32::consts::PI / 4.0;
    for i in 0..200 {
        let t = 2.0 * std::f32::consts::PI * freq * (i as f32) / SR + phase;
        let s = t.sin();
        tp_lim.process(s, s);
        sp_lim.process(s, s);
    }

    let tp_db = tp_lim.max_true_peak_dbtp();
    let sp_db = sp_lim.max_sample_peak_db();

    // Sample peak will report ~ -3.01 dBFS
    assert!(
        sp_db < -2.9,
        "Sample peak should be ~ -3.01 dBFS, got {}",
        sp_db
    );
    // True peak FIR detector must reconstruct close to 0 dBFS (-0.5 to +0.2 dBTP)
    assert!(
        tp_db > -0.5 && tp_db <= 0.2,
        "True peak FIR must detect intersample peak near 0 dBFS, got {} dBTP",
        tp_db
    );
}

/// Reference measurement of the true-peak FIR prototype.
///
/// The prototype is a Kaiser-windowed sinc 4× interpolator designed for
/// <0.01 dB passband ripple and ≥100 dB stopband attenuation (see
/// `dsp::true_peak` for the design spec). This test pins the *measured*
/// response against that reference target across the whole passband and
/// stopband rather than sampling convenient nulls.
#[test]
fn test_true_peak_fir_frequency_response_and_characterization() {
    let proto = LookaheadLimiter::fir_prototype_coefficients();
    assert_eq!(proto.len(), engine::dsp::true_peak::TRUE_PEAK_FIR_TAPS);

    let resp = |f: f32| -> f64 {
        let (mag, _) = LookaheadLimiter::fir_frequency_response(f, 48000.0);
        20.0 * (mag / 4.0).log10() // normalized to DC gain = 4.0
    };

    // 1. Exact linear phase / symmetry: h[n] == h[N-1-n].
    for i in 0..proto.len() / 2 {
        let diff = (proto[i] - proto[proto.len() - 1 - i]).abs();
        assert!(diff < 1e-12, "FIR asymmetry at tap {i}");
    }

    // 2. DC gain: sum(h) == 4.0 (1.0 per polyphase branch).
    let total_sum: f64 = proto.iter().sum();
    assert!((total_sum - 4.0).abs() < 1e-9, "total DC sum {total_sum}");
    for (i, &gain) in LookaheadLimiter::fir_branch_dc_gains().iter().enumerate() {
        assert!((gain - 1.0).abs() < 1e-4, "branch {i} DC gain {gain}");
    }

    // 3. Passband ripple (100 Hz – 20 kHz) < 0.01 dB.
    let mut worst_passband = 0.0f64;
    for freq_hz in (100..=20000).step_by(50) {
        worst_passband = worst_passband.max(resp(freq_hz as f32).abs());
    }
    assert!(
        worst_passband < 0.01,
        "passband ripple too high: {worst_passband:.5} dB (target < 0.01 dB)"
    );

    // 4. Stopband attenuation (24 kHz – 96 kHz, the whole stopband past the
    //    baseband Nyquist edge) ≥ 100 dB.
    //    (dB values are negative; "worst" = the LEAST attenuation = the max.)
    let mut worst_stopband = f64::NEG_INFINITY;
    for freq_hz in (24000..=96000).step_by(50) {
        worst_stopband = worst_stopband.max(resp(freq_hz as f32));
    }
    assert!(
        worst_stopband <= -100.0,
        "worst stopband attenuation too low: {worst_stopband:.2} dB (target ≥ 100 dB)"
    );

    // 5. Deep nulls at multiples of the baseband rate (48, 96 kHz) — where
    //    DAC reconstruction images actually land.
    for f in [48000.0f32, 96000.0] {
        assert!(resp(f) < -100.0, "expected deep null at {f} Hz");
    }
}

#[test]
fn test_limiter_997hz_aes17_calibration() {
    // Standard AES17 standard calibration test frequency: 997 Hz.
    let sr = 48000.0f32;
    let freq = 997.0_f32;
    let n = (sr * 0.5) as usize; // 0.5 seconds

    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::Fir4x);
    for _ in 0..500 {
        lim.process(0.0, 0.0);
    }

    // -0.1 dBFS sine: amplitude ≈ 0.988553
    let amp_minus_01 = 10.0_f32.powf(-0.1 / 20.0);
    let mut max_out = 0.0f32;
    for i in 0..n {
        let s = amp_minus_01 * (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin();
        let (l, _) = lim.process(s, s);
        max_out = max_out.max(l.abs());
    }

    // Ceiling is -0.3 dBFS (0.96605). The limiter must pull -0.1 dBFS down to ceiling
    assert!(
        max_out <= CEILING_LIN + 1e-4,
        "997 Hz at -0.1 dBFS must not exceed ceiling: {max_out} vs {CEILING_LIN}"
    );
    assert!(
        max_out > CEILING_LIN - 0.02,
        "997 Hz should limit close to ceiling: got {max_out}, ceiling is {CEILING_LIN}"
    );
}

#[test]
fn test_limiter_worst_case_intersample_peak_reconstruction() {
    // Test worst-case reconstruction waveforms across various near-Nyquist frequencies
    // and phase angles where discrete samples are low but continuous analog reconstruction
    // overshoots significantly (up to +3.01 dB).
    let sr = 48000.0f32;
    let phases = [
        0.0,
        std::f32::consts::FRAC_PI_8,
        std::f32::consts::FRAC_PI_6,
        std::f32::consts::FRAC_PI_4,
        std::f32::consts::FRAC_PI_3,
        std::f32::consts::FRAC_PI_2,
    ];
    let test_freqs = [6000.0f32, 8000.0, 10000.0, 12000.0, 14000.0, 18000.0];

    for &freq in &test_freqs {
        for &phase in &phases {
            let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::Fir4x);
            for _ in 0..500 {
                lim.process(0.0, 0.0);
            }

            // -0.1 dBFS amplitude sine
            let amp = 10.0_f32.powf(-0.1 / 20.0);
            let n = (sr * 0.1) as usize; // 100 ms

            let mut max_out = 0.0f32;
            for i in 0..n {
                let s = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / sr + phase).sin();
                let (l, _) = lim.process(s, s);
                max_out = max_out.max(l.abs());
            }

            assert!(
                max_out <= CEILING_LIN + 1e-4,
                "Intersample peak at {freq} Hz phase {phase}: output {max_out} exceeded ceiling {CEILING_LIN}"
            );
        }
    }
}

#[test]
fn test_limiter_lookahead_delay_and_impulse_alignment() {
    let sr = 48000.0f32;
    let lookahead_ms = 5.0f32;
    let lookahead_samples = ((sr * lookahead_ms / 1000.0).round() as usize).max(1);

    let mut lim = LookaheadLimiter::new_with_mode(
        sr,
        lookahead_ms,
        0.5,
        100.0,
        CEILING_DB,
        LimiterMode::Transparent,
    );
    lim.set_true_peak_mode(TruePeakMode::SamplePeak);

    // Warm up with silence
    for _ in 0..lookahead_samples * 2 {
        lim.process(0.0, 0.0);
    }

    // Feed a small single impulse
    let (l0, _) = lim.process(0.5, 0.5);
    // Because of lookahead delay, output sample at index 0 should be 0.0 (the pre-delay silence)
    assert_eq!(l0, 0.0, "Impulse should be delayed by lookahead window");

    // Advance until the delayed impulse appears
    let mut impulse_index = None;
    for i in 1..=lookahead_samples * 2 {
        let (out_l, _) = lim.process(0.0, 0.0);
        if out_l > 0.1 {
            impulse_index = Some(i);
            break;
        }
    }

    assert_eq!(
        impulse_index,
        Some(lookahead_samples),
        "Impulse must emerge after exactly {lookahead_samples} lookahead samples"
    );
}

#[test]
fn test_fir_detector_delay_is_compensated_in_audio_path() {
    let sr = 48000.0f32;
    let lookahead_ms = 5.0f32;
    let lookahead_samples = ((sr * lookahead_ms / 1000.0).round() as usize).max(1);
    let fir_delay = engine::dsp::true_peak::detector_delay_samples();

    let mut lim = LookaheadLimiter::new_with_mode(
        sr,
        lookahead_ms,
        0.5,
        100.0,
        CEILING_DB,
        LimiterMode::Transparent,
    );
    lim.set_true_peak_mode(TruePeakMode::Fir4x);

    // The audio delay line must be lengthened by the detector's group delay
    // so the predictive gain still runs ahead of the transient.
    assert_eq!(lim.lookahead_samples(), lookahead_samples + fir_delay);

    for _ in 0..(lookahead_samples + fir_delay) * 2 {
        lim.process(0.0, 0.0);
    }
    let (l0, _) = lim.process(0.5, 0.5);
    assert_eq!(l0, 0.0, "impulse must still be inside the delay line");

    let mut impulse_index = None;
    for i in 1..=(lookahead_samples + fir_delay) * 2 {
        let (out_l, _) = lim.process(0.0, 0.0);
        if out_l > 0.1 {
            impulse_index = Some(i);
            break;
        }
    }
    assert_eq!(
        impulse_index,
        Some(lookahead_samples + fir_delay),
        "impulse must emerge after lookahead + detector delay"
    );
}
