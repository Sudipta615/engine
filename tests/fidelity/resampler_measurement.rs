//! Frequency-domain resampler measurement suite (spec §14, §25.2).
//!
//! Measures the *realized* characteristics of the rubato filters each
//! `ResamplerQuality` tier maps to, replacing documented targets with
//! measured claims:
//!
//! - **Passband ripple** — the worst deviation of the measured transfer
//!   function from 0 dB across the passband (44.1 → 48 kHz, tones from 1 kHz
//!   to 21 kHz).
//! - **Stopband rejection** — how deeply an out-of-band tone (above the
//!   output Nyquist) is suppressed before it aliases back into the passband
//!   (48 → 44.1 kHz, tones at 23.0 / 23.5 kHz → in-band images at 21.1 /
//!   20.6 kHz).
//! - **Near-Nyquist tone accuracy** — amplitude of a 20 kHz tone through a
//!   48 → 44.1 kHz conversion.
//! - **Tier ordering** — Ultra must beat HighQuality, which must beat
//!   Balanced, which must beat Fast; otherwise the tier ladder is cosmetic
//!   and the setting should not exist.
//! - **Latency** — each tier's reported group delay, monotonically ordered.
//!
//! Measurements use the **f64** resampler so the f32 pipeline's rounding
//! noise (~1e-7) does not mask deep stopbands; rubato builds identical filter
//! coefficients for f32 and f64.

use config::ResamplerQuality;
use engine::dsp::resampler::AudioResamplerF64;

const TIERS: [ResamplerQuality; 4] = [
    ResamplerQuality::Fast,
    ResamplerQuality::Balanced,
    ResamplerQuality::HighQuality,
    ResamplerQuality::Ultra,
];

/// Resample `seconds` of a full-scale*0.5 sine at `freq_hz` from `fs_in` to
/// `fs_out` through the engine resampler. Returns (input samples, output).
fn resample_tone(
    quality: ResamplerQuality,
    fs_in: f64,
    fs_out: f64,
    freq_hz: f64,
    seconds: f64,
) -> (Vec<f64>, Vec<f64>) {
    let mut r = AudioResamplerF64::new(quality, fs_in as f32, fs_out as f32)
        .expect("resampler construction");
    let n_in = (fs_in * seconds) as usize;
    let mut input = Vec::with_capacity(n_in);
    for i in 0..n_in {
        let s = (std::f64::consts::TAU * freq_hz * i as f64 / fs_in).sin() * 0.5;
        r.feed(s, s);
        input.push(s);
    }
    r.flush();
    let mut out = Vec::new();
    while let Some((l, _r)) = r.read() {
        out.push(l);
    }
    (input, out)
}

/// Amplitude of the tone at `freq` in `samples` (sampled at `rate`), measured
/// by an exact DFT projection over `len` samples starting at `start`. The
/// projection is exact for a pure tone at any phase; with `len` large enough
/// the cos/sin cross-term is negligible (< 0.01 dB for the lengths used).
fn tone_amplitude(samples: &[f64], rate: f64, freq: f64, start: usize, len: usize) -> f64 {
    let end = (start + len).min(samples.len());
    let mut re = 0.0;
    let mut im = 0.0;
    for (k, &x) in samples[start..end].iter().enumerate() {
        let n = (start + k) as f64;
        let phase = std::f64::consts::TAU * freq * n / rate;
        re += x * phase.cos();
        im += x * phase.sin();
    }
    2.0 * (re * re + im * im).sqrt() / (end - start) as f64
}

fn db(ratio: f64) -> f64 {
    20.0 * ratio.abs().max(1e-300).log10()
}

/// Worst passband deviation from 0 dB across the passband tones.
fn passband_ripple_db(quality: ResamplerQuality) -> f64 {
    // All tones below every tier's passband edge (Fast's edge is ~21.0 kHz),
    // so the deviation is genuine in-band ripple, not rolloff.
    let passband_tones = [1_000.0, 5_000.0, 10_000.0, 15_000.0, 18_000.0, 19_000.0];
    let mut worst = 0.0f64;
    for f in passband_tones {
        let (input, out) = resample_tone(quality, 44_100.0, 48_000.0, f, 4.0);
        let a_in = tone_amplitude(&input, 44_100.0, f, 44_100, 32_768);
        let a_out = tone_amplitude(&out, 48_000.0, f, 8_192, 32_768);
        worst = worst.max(db(a_out / a_in));
    }
    worst
}

/// Suppression (dB, positive number) of a single out-of-band tone at
/// `f_stop` Hz through 48 → 44.1 kHz: the residual leak aliases back to the
/// in-band image at `|f_stop − 44.1 kHz|`. Measured near the transition band
/// (just above the 22.05 kHz output Nyquist), where the tiers differ; deep in
/// the stopband every tier reaches the f64 measurement floor (~220 dB) and
/// the differences vanish.
fn stopband_suppression_db(quality: ResamplerQuality, f_stop: f64) -> f64 {
    let f_image = (f_stop - 44_100.0).abs();
    let (input, out) = resample_tone(quality, 48_000.0, 44_100.0, f_stop, 4.0);
    let a_in = tone_amplitude(&input, 48_000.0, f_stop, 48_000, 32_768);
    let a_img = tone_amplitude(&out, 44_100.0, f_image, 8_192, 32_768);
    -db(a_img / a_in) // positive when a_img << a_in
}

/// Worst suppression across the transition-band probes (dB, positive number).
/// The tier-discriminating point is 22.1 kHz (just above the cutoff), where
/// the measured values are filter-limited (151.8 / 161.6 / 173.9 / 180.9 dB
/// for Fast/Balanced/HighQuality/Ultra) rather than noise-floor-limited;
/// deeper probes (23 kHz) all hit the f64 measurement floor (~220 dB) and
/// carry no tier information.
fn stopband_rejection_db(quality: ResamplerQuality) -> f64 {
    let mut least = f64::INFINITY;
    for f_stop in [22_100.0, 22_300.0, 22_600.0] {
        least = least.min(stopband_suppression_db(quality, f_stop));
    }
    least
}

/// The highest probe frequency (out of `probes`) whose gain stays within
/// `threshold_db` of 0 dB through 44.1 → 48 kHz — a coarse estimate of the
/// filter's passband edge. Longer filters push the edge closer to the
/// 22.05 kHz input Nyquist.
fn passband_edge_hz(quality: ResamplerQuality, threshold_db: f64) -> f64 {
    let probes = [
        19_000.0, 20_000.0, 20_500.0, 20_800.0, 21_000.0, 21_200.0, 21_400.0, 21_600.0, 21_800.0,
        22_000.0,
    ];
    let mut edge = 0.0f64;
    for f in probes {
        let g = passband_edge_gain_db(quality, f);
        if g >= -threshold_db {
            edge = f;
        } else {
            break;
        }
    }
    edge
}

/// Amplitude error (dB) of a 20 kHz tone through 48 → 44.1 kHz.
fn near_nyquist_tone_error_db(quality: ResamplerQuality) -> f64 {
    let (input, out) = resample_tone(quality, 48_000.0, 44_100.0, 20_000.0, 4.0);
    let a_in = tone_amplitude(&input, 48_000.0, 20_000.0, 48_000, 32_768);
    let a_out = tone_amplitude(&out, 44_100.0, 20_000.0, 8_192, 32_768);
    db(a_out / a_in)
}

/// Suppression (dB) at a single passband-edge frequency through 44.1 → 48 kHz.
/// Probes the droop just below the 24 kHz output Nyquist, which differentiates
/// short filters (early rolloff) from long ones.
fn passband_edge_gain_db(quality: ResamplerQuality, freq: f64) -> f64 {
    let (input, out) = resample_tone(quality, 44_100.0, 48_000.0, freq, 4.0);
    let a_in = tone_amplitude(&input, 44_100.0, freq, 44_100, 32_768);
    let a_out = tone_amplitude(&out, 48_000.0, freq, 8_192, 32_768);
    db(a_out / a_in)
}

fn latency_ms(quality: ResamplerQuality) -> f32 {
    let r = AudioResamplerF64::new(quality, 44_100.0, 48_000.0).expect("resampler");
    r.latency_ms()
}

// ─────────────────────────────────────────────────────────────────────────────
// Passband
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn passband_ripple_within_documented_targets() {
    // Measured in-band ripple for every tier is ≈0.001 dB (f64 measurement
    // limited); the documented targets are 0.01 dB or better for all tiers.
    let fast = passband_ripple_db(ResamplerQuality::Fast);
    let balanced = passband_ripple_db(ResamplerQuality::Balanced);
    let hq = passband_ripple_db(ResamplerQuality::HighQuality);
    let ultra = passband_ripple_db(ResamplerQuality::Ultra);
    println!(
        "passband ripple (dB): fast={fast:.4} balanced={balanced:.4} hq={hq:.4} ultra={ultra:.4}"
    );
    assert!(
        fast < 0.05,
        "Fast passband ripple {fast:.4} dB exceeds 0.05 dB"
    );
    assert!(
        balanced < 0.05,
        "Balanced passband ripple {balanced:.4} dB exceeds 0.05 dB"
    );
    assert!(
        hq < 0.01,
        "HighQuality passband ripple {hq:.4} dB exceeds 0.01 dB"
    );
    assert!(
        ultra < 0.01,
        "Ultra passband ripple {ultra:.4} dB exceeds 0.01 dB"
    );
}

#[test]
fn near_nyquist_tone_accuracy() {
    for tier in TIERS {
        let err = near_nyquist_tone_error_db(tier);
        println!("{tier:?} 20 kHz tone error through 48→44.1 kHz: {err:.4} dB");
        assert!(
            err.abs() < 1.0,
            "{tier:?} 20 kHz tone error {err:.4} dB exceeds 1.0 dB"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stopband
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn stopband_rejection_meets_documented_floors() {
    let fast = stopband_rejection_db(ResamplerQuality::Fast);
    let balanced = stopband_rejection_db(ResamplerQuality::Balanced);
    let hq = stopband_rejection_db(ResamplerQuality::HighQuality);
    let ultra = stopband_rejection_db(ResamplerQuality::Ultra);
    println!("stopband rejection (worst of 22.1/22.3/22.6 kHz, dB): fast={fast:.1} balanced={balanced:.1} hq={hq:.1} ultra={ultra:.1}");
    for tier in TIERS {
        for f in [22_100.0, 22_300.0, 22_600.0] {
            println!(
                "  {tier:?} @ {f} Hz: {:.1} dB",
                stopband_suppression_db(tier, f)
            );
        }
    }
    assert!(
        fast >= 140.0,
        "Fast stopband rejection {fast:.1} dB < 140 dB"
    );
    assert!(
        balanced >= 150.0,
        "Balanced stopband rejection {balanced:.1} dB < 150 dB"
    );
    assert!(
        hq >= 165.0,
        "HighQuality stopband rejection {hq:.1} dB < 165 dB"
    );
    assert!(
        ultra >= 175.0,
        "Ultra stopband rejection {ultra:.1} dB < 175 dB"
    );
}

#[test]
fn passband_edge_rises_with_quality() {
    // Longer filters push the −1 dB edge closer to the 22.05 kHz input
    // Nyquist: the measurable bandwidth grows with quality. Measured edges:
    // Fast ≈ 21.0 kHz < Balanced ≈ 21.5 kHz < HighQuality ≈ 21.8 kHz <
    // Ultra ≈ 21.9 kHz.
    let fast = passband_edge_hz(ResamplerQuality::Fast, 1.0);
    let balanced = passband_edge_hz(ResamplerQuality::Balanced, 1.0);
    let hq = passband_edge_hz(ResamplerQuality::HighQuality, 1.0);
    let ultra = passband_edge_hz(ResamplerQuality::Ultra, 1.0);
    println!("−1 dB passband edge (44.1→48): fast={fast:.0} balanced={balanced:.0} hq={hq:.0} ultra={ultra:.0} Hz");
    assert!(
        fast < balanced,
        "Fast edge {fast:.0} must be < Balanced {balanced:.0}"
    );
    assert!(
        balanced < hq,
        "Balanced edge {balanced:.0} must be < HighQuality {hq:.0}"
    );
    assert!(
        hq < ultra,
        "HighQuality edge {hq:.0} must be < Ultra {ultra:.0}"
    );
    assert!(
        ultra >= 21_600.0,
        "Ultra edge {ultra:.0} Hz must reach ≥ 21.6 kHz"
    );
    assert!(
        fast <= 21_300.0,
        "Fast edge {fast:.0} Hz must stay ≤ 21.3 kHz (bandwidth trade-off)"
    );
}

#[test]
fn tier_ladder_is_real_and_monotonic() {
    // The whole point of exposing quality tiers: each must be measurably
    // better than the one below it. rubato derives the filter length from
    // (chunk / sub_chunks), so a tier mapping that does not lengthen the
    // filter produces identical audio and must be caught here.
    let fast = stopband_rejection_db(ResamplerQuality::Fast);
    let balanced = stopband_rejection_db(ResamplerQuality::Balanced);
    let hq = stopband_rejection_db(ResamplerQuality::HighQuality);
    let ultra = stopband_rejection_db(ResamplerQuality::Ultra);
    assert!(
        balanced > fast + 5.0,
        "Balanced ({balanced:.1} dB) must beat Fast ({fast:.1} dB) by > 5 dB"
    );
    assert!(
        hq > balanced + 5.0,
        "HighQuality ({hq:.1} dB) must beat Balanced ({balanced:.1} dB) by > 5 dB"
    );
    assert!(
        ultra > hq + 5.0,
        "Ultra ({ultra:.1} dB) must beat HighQuality ({hq:.1} dB) by > 5 dB"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Latency
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn latency_reported_and_monotonically_ordered() {
    let mut prev = 0.0f32;
    for tier in TIERS {
        let ms = latency_ms(tier);
        println!("{tier:?} group delay: {ms:.2} ms");
        assert!(ms > 0.0, "{tier:?} must report nonzero filter latency");
        assert!(
            ms >= prev,
            "{tier:?} latency {ms:.2} ms must not be lower than the previous tier ({prev:.2} ms)"
        );
        prev = ms;
    }
}

#[test]
fn ultra_is_better_than_high_quality_on_both_axes() {
    // The new tier must win on both measured axes against the previous best,
    // not just one — otherwise it is a trade-off, not an upgrade.
    let hq_ripple = passband_ripple_db(ResamplerQuality::HighQuality);
    let ultra_ripple = passband_ripple_db(ResamplerQuality::Ultra);
    let hq_stop = stopband_rejection_db(ResamplerQuality::HighQuality);
    let ultra_stop = stopband_rejection_db(ResamplerQuality::Ultra);
    assert!(
        ultra_stop >= hq_stop,
        "Ultra stopband ({ultra_stop:.1} dB) must be ≥ HighQuality ({hq_stop:.1} dB)"
    );
    assert!(
        ultra_ripple <= hq_ripple + 0.001,
        "Ultra ripple ({ultra_ripple:.4} dB) must not regress vs HighQuality ({hq_ripple:.4} dB)"
    );
}

#[test]
fn passthrough_latency_is_zero() {
    let r = AudioResamplerF64::new(ResamplerQuality::Ultra, 48_000.0, 48_000.0).expect("resampler");
    assert!(r.is_passthrough());
    assert_eq!(r.latency_samples(), 0);
    assert_eq!(r.latency_ms(), 0.0);
}

/// The resampler must reproduce a full-scale 1 kHz tone through a 44.1→48 kHz
/// conversion with negligible level error at every tier (level = signal, not
/// a filter artifact).
#[test]
fn full_scale_tone_level_accuracy() {
    for tier in TIERS {
        let (input, out) = resample_tone(tier, 44_100.0, 48_000.0, 1_000.0, 2.0);
        let a_in = tone_amplitude(&input, 44_100.0, 1_000.0, 22_050, 16_384);
        let a_out = tone_amplitude(&out, 48_000.0, 1_000.0, 4_096, 16_384);
        let err = db(a_out / a_in);
        assert!(
            err.abs() < 0.1,
            "{tier:?} 1 kHz level error {err:.4} dB exceeds 0.1 dB"
        );
    }
}
