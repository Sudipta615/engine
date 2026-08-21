//! Fidelity tests — Limiter measurement suite (spec §12 measurement claims)
//!
//! Where `limiter_correctness` verifies *qualitative* behavior (ceiling
//! respected, impulse no overshoot, FIR triggers earlier), this suite makes
//! *quantitative* measurements:
//!
//! * steady-state input/output transfer curve — the limiter must behave like
//!   `min(input, ceiling)` to within ±0.1 dB once settled;
//! * multichannel gain coherence — all channels of a frame receive identical
//!   gain reduction (spatial image preservation), measured per channel;
//! * release-time measurement — after a burst, gain must recover to within
//!   1 dB of unity within a bounded time (smooth, not snapped, and not
//!   dragged);
//! * transparent-mode THD — steady-state limiting of a sine must be
//!   *gain reduction*, not distortion (THD < 0.1 % while saturate mode adds
//!   measurable coloration);
//! * block vs per-frame equivalence.

use engine::dsp::limiter::{LimiterMode, LookaheadLimiter, TruePeakMode};

const SR: f32 = 48_000.0;
const CEILING_DB: f32 = -0.3;

fn ceiling_lin() -> f32 {
    10.0_f32.powf(CEILING_DB / 20.0)
}

fn make_limiter(mode: LimiterMode, tp: TruePeakMode) -> LookaheadLimiter {
    let mut lim = LookaheadLimiter::new_with_mode(SR, 5.0, 0.5, 100.0, CEILING_DB, mode);
    lim.set_true_peak_mode(tp);
    lim
}

fn db(x: f32) -> f32 {
    20.0 * x.abs().max(1e-12).log10()
}

/// Feed a steady sine and return the max output magnitude after the
/// settle window (attack + release ramp convergence).
fn steady_sine_peak(lim: &mut LookaheadLimiter, freq: f32, amplitude: f32, seconds: f32) -> f32 {
    // Settle: skip the first 10 % (attack transient + lookahead priming).
    let n = (SR * seconds) as usize;
    let settle = n / 10;
    let mut max_out = 0.0f32;
    for i in 0..n {
        let s = amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
        let (l, _r) = lim.process(s, s);
        if i >= settle {
            max_out = max_out.max(l.abs());
        }
    }
    max_out
}

/// H3: the settled transfer curve is `min(input, ceiling)` — inputs below
/// the ceiling pass at unity gain (±0.05 dB), inputs above are pulled down
/// to the ceiling (±0.1 dB). This is the reference-grade contract of a
/// clean limiter: it must not pump quiet material nor fail to protect loud
/// material.
#[test]
fn limiter_transfer_curve_matches_ideal_compressor() {
    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    let freq = 997.0_f32; // AES17 calibration tone

    for input_db in [-30.0_f32, -20.0, -10.0, -3.0, -0.1, 3.0, 6.0, 12.0] {
        let amp = 10.0_f32.powf(input_db / 20.0);
        let out = steady_sine_peak(&mut lim, freq, amp, 0.6);
        let ideal_db = input_db.min(CEILING_DB);
        let measured_db = db(out);
        let error_db = measured_db - ideal_db;
        let tol = if input_db < CEILING_DB { 0.05 } else { 0.1 };
        assert!(
            error_db.abs() < tol,
            "input {input_db:.1} dBFS: ideal output {ideal_db:.2} dBFS, measured {measured_db:.2} dBFS (error {error_db:.3} dB)"
        );
    }
}

/// H3: multichannel gain reduction must be identical across channels —
/// the detector uses the max peak across the frame and applies one gain to
/// all channels (spatial image preservation). Measured per channel: every
/// channel's settled output/input ratio must be identical within 1e-3, the
/// loudest channel must sit at the ceiling, and no channel may exceed it.
#[test]
fn limiter_multichannel_gain_coherence() {
    let mut lim =
        LookaheadLimiter::new_with_mode(SR, 5.0, 0.5, 100.0, CEILING_DB, LimiterMode::Transparent);
    lim.set_true_peak_mode(TruePeakMode::SamplePeak);

    let channels = 6usize;
    // Different per-channel amplitudes — all above the ceiling — so any
    // per-channel processing would show up as different outputs.
    let amps: Vec<f32> = (0..channels).map(|c| 1.5 + 0.15 * c as f32).collect();
    let freq = 997.0_f32;
    let seconds = 0.6;
    let n = (SR * seconds) as usize;
    let settle = n / 10;

    let mut peaks = vec![0.0f32; channels];
    let mut out = [0.0f64; engine::buffer::MAX_CHANNELS];
    let mut inp = [0.0f64; engine::buffer::MAX_CHANNELS];
    for i in 0..n {
        let phase = 2.0 * std::f32::consts::PI * freq * i as f32 / SR;
        for c in 0..channels {
            inp[c] = (amps[c] * phase.sin()) as f64;
        }
        lim.process_sample_multichannel(&inp[..channels], &mut out[..channels]);
        if i >= settle {
            for c in 0..channels {
                peaks[c] = peaks[c].max(out[c].abs() as f32);
            }
        }
    }

    let ceiling = ceiling_lin();
    // The loudest channel (max amplitude) is exactly at the ceiling; every
    // other channel is scaled by the same factor.
    let expected_gain = ceiling / amps[channels - 1];
    let mut max_spread = 0.0f32;
    for c in 0..channels {
        let ratio = peaks[c] / amps[c];
        max_spread = max_spread.max((ratio / expected_gain - 1.0).abs());
        assert!(
            peaks[c] <= ceiling + 1e-3,
            "channel {c} exceeded ceiling: {:.4}",
            peaks[c]
        );
    }
    assert!(
        max_spread < 1e-3,
        "channel-to-channel gain spread too large: {max_spread:.5}"
    );
    // And the loudest channel is pulled exactly to the ceiling.
    assert!(
        (db(peaks[channels - 1]) - CEILING_DB).abs() < 0.05,
        "loudest channel must sit at ceiling, got {:.3} dBFS",
        db(peaks[channels - 1])
    );
}

/// H3: release is exponential, not instant and not dragged. After a 0 dBFS
/// burst (gain driven to ≈ ceiling/1.0 ≈ 0.966), recovery to within 1 dB of
/// unity must complete in a bounded window derived from the 100 ms release
/// time constant (τ·ln((1−g₀)/0.109) ≈ 156 ms for g₀ ≈ 0.48, but bounded
/// with margin).
#[test]
fn limiter_release_recovers_within_bounded_time() {
    let mut lim = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);

    // Burst: +6 dBFS sine (amp 2.0) for 100 ms → sustained gain reduction
    // of ~ −6.3 dB (0.966/2.0), deep enough to measure recovery.
    for i in 0..(SR * 0.1) as usize {
        let s = 2.0 * (2.0 * std::f32::consts::PI * 997.0 * i as f32 / SR).sin();
        lim.process(s, s);
    }
    let gr_at_burst_end = lim.gain_reduction_db();
    assert!(
        gr_at_burst_end < -0.1,
        "burst must actually engage gain reduction, got {gr_at_burst_end} dB"
    );

    // Silence: count samples until gain is within 1 dB of unity.
    let mut recovered_at: Option<usize> = None;
    for i in 0..(SR * 1.0) as usize {
        lim.process(0.0, 0.0);
        if lim.gain_reduction_db() > -1.0 && recovered_at.is_none() {
            recovered_at = Some(i);
        }
    }
    let recovered = recovered_at.expect("gain must recover within 1 s of silence");
    let recover_ms = recovered as f32 / SR * 1000.0;

    // Exponential release with τ = 100 ms: must take substantially longer
    // than an instant snap (< 20 ms) but finish well within 500 ms.
    assert!(
        recover_ms > 20.0,
        "release snapped back too fast ({recover_ms:.1} ms) — not a smoothed release"
    );
    assert!(
        recover_ms < 500.0,
        "release too slow ({recover_ms:.1} ms) for a 100 ms time constant"
    );
}

/// Harmonic amplitude via a windowed DFT at k·freq (integer cycles ⇒ exact).
fn harmonic_amp(signal: &[f32], freq: f32, k: u32) -> f32 {
    let n = signal.len();
    let mut re = 0.0f64;
    let mut im = 0.0f64;
    for (i, &s) in signal.iter().enumerate() {
        let phase = 2.0 * std::f64::consts::PI * freq as f64 * k as f64 * i as f64 / SR as f64;
        re += s as f64 * phase.cos();
        im += s as f64 * phase.sin();
    }
    (2.0 * (re * re + im * im).sqrt() / n as f64) as f32
}

fn thd(signal: &[f32], freq: f32) -> f32 {
    let fund = harmonic_amp(signal, freq, 1);
    if fund < 1e-9 {
        return f32::INFINITY;
    }
    let mut dist = 0.0f64;
    for k in 2..=8 {
        let h = harmonic_amp(signal, freq, k) as f64;
        dist += h * h;
    }
    (dist.sqrt() / fund as f64) as f32
}

/// H3: transparent-mode limiting is gain reduction, not distortion. A
/// 1 kHz sine 6 dB over the ceiling must come out at the ceiling with
/// THD < 0.1 % in steady state; saturate mode adds measurable coloration
/// (THD at least 10× higher) — that is its documented character.
#[test]
fn limiter_transparent_mode_is_gain_reduction_not_distortion() {
    let freq = 1000.0_f32;
    let amplitude = ceiling_lin() * 2.0; // +6 dB over ceiling
    let cycles = 100usize;
    let n = (SR / freq * cycles as f32) as usize;

    let mut transparent = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    let mut out_t = Vec::with_capacity(n);
    for i in 0..n {
        let s = amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
        let (l, _) = transparent.process(s, s);
        out_t.push(l);
    }
    // Skip the settled prefix (attack ramp): use the last 50 cycles.
    let ss = n / 2;
    let thd_t = thd(&out_t[ss..], freq);
    let peak_t = out_t[ss..].iter().cloned().fold(0.0f32, f32::max);
    assert!(
        (peak_t - ceiling_lin()).abs() < 1e-3,
        "transparent limited sine must sit at the ceiling, got {peak_t}"
    );
    assert!(
        thd_t < 0.001,
        "transparent limiting THD too high: {:.4} % (target < 0.1 %)",
        thd_t * 100.0
    );

    let mut saturate = make_limiter(LimiterMode::Saturate, TruePeakMode::SamplePeak);
    let mut out_s = Vec::with_capacity(n);
    for i in 0..n {
        let s = amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
        let (l, _) = saturate.process(s, s);
        out_s.push(l);
    }
    let thd_s = thd(&out_s[ss..], freq);
    assert!(
        thd_s > thd_t * 10.0,
        "saturate mode must add measurable coloration: THD {:.4} % vs transparent {:.4} %",
        thd_s * 100.0,
        thd_t * 100.0
    );
}

/// H3: the block API must be sample-identical to per-frame processing.
#[test]
fn limiter_block_matches_per_frame() {
    let n = 4096usize;
    let mut input = Vec::with_capacity(n);
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 997.0 * i as f32 / SR).sin();
        input.push((s * 2.0, s * -1.5)); // both channels over the ceiling
    }

    let mut frame = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    let mut ref_l = Vec::with_capacity(n);
    let mut ref_r = Vec::with_capacity(n);
    for &(l, r) in &input {
        let (ol, or_) = frame.process(l, r);
        ref_l.push(ol);
        ref_r.push(or_);
    }

    for block in [7usize, 64, 128, 512, 1024] {
        let mut blk = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
        let mut l: Vec<f32> = input.iter().map(|&(a, _)| a).collect();
        let mut r: Vec<f32> = input.iter().map(|&(_, b)| b).collect();
        for (lc, rc) in l.chunks_mut(block).zip(r.chunks_mut(block)) {
            blk.process_block(lc, rc);
        }
        for i in 0..n {
            assert!(
                (l[i] - ref_l[i]).abs() < 1e-6,
                "block {block} L mismatch at {i}: {} vs {}",
                l[i],
                ref_l[i]
            );
            assert!(
                (r[i] - ref_r[i]).abs() < 1e-6,
                "block {block} R mismatch at {i}: {} vs {}",
                r[i],
                ref_r[i]
            );
        }
    }
}

/// H3: multichannel block limiting applies the same protection as the
/// per-frame multichannel API.
#[test]
fn limiter_multichannel_block_matches_per_frame() {
    let channels = 6usize;
    let n = 2048usize;
    let mut input = vec![0.0f32; n * channels];
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 997.0 * i as f32 / SR).sin();
        for c in 0..channels {
            input[i * channels + c] = s * (1.0 + 0.2 * c as f32);
        }
    }

    let mut frame = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    let mut ref_out = vec![0.0f32; n * channels];
    {
        let mut inp = [0.0f64; engine::buffer::MAX_CHANNELS];
        let mut outp = [0.0f64; engine::buffer::MAX_CHANNELS];
        for i in 0..n {
            for c in 0..channels {
                inp[c] = input[i * channels + c] as f64;
            }
            frame.process_sample_multichannel(&inp[..channels], &mut outp[..channels]);
            for c in 0..channels {
                ref_out[i * channels + c] = outp[c] as f32;
            }
        }
    }

    let mut blk = make_limiter(LimiterMode::Transparent, TruePeakMode::SamplePeak);
    let mut out = input.clone();
    blk.process_block_multichannel(&mut out, channels);

    for i in 0..n * channels {
        assert!(
            (out[i] - ref_out[i]).abs() < 1e-6,
            "multichannel block mismatch at sample {i}: {} vs {}",
            out[i],
            ref_out[i]
        );
    }
}
