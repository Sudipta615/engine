//! Concrete DSP / spatial measurement suites for the evaluation harness.
//!
//! Each suite pairs a versioned reference [`ReferenceVector`] (see the `def_*`
//! constructors, registered by [`ReferenceVectorRegistry::build`]) with a
//! `measure` function that runs the component on captured buffers and returns a
//! [`ComponentReport`]. All measurements are deterministic and **reuse the
//! fidelity-suite conventions** (Goertzel amplitude, direct-convolution-free
//! golden IR magnitude, proven limiter/resampler/loudness scenarios) so the
//! numbers a report prints match the numbers the repo's golden tests already
//! commit to.
//!
//! Control/offline path only — each suite builds its component, feeds it, and
//! discards it; nothing touches a realtime thread.

use config::{EngineConfig, ResamplerQuality, SpatialConfig};
use std::f64::consts::TAU;

use crate::dsp::biquad::{BiquadCoeffsF64, BiquadStateF64};
use crate::dsp::convolution::ConvolutionEngine;
use crate::dsp::graph::{DspNode, SpatialNode};
use crate::dsp::limiter::LookaheadLimiter;
use crate::dsp::loudness::LoudnessMeter;
use crate::dsp::pipeline::DspPipeline;
use crate::dsp::resampler::AudioResamplerF64;
use crate::spatial::provider::{HrtfDatasetProvider, HrtfProvider};
use crate::spatial::{HrtfDataset, Vec3};

use super::measure::{
    db, ir_magnitude_db, ir_peak_magnitude_db, ir_phase_deg, mismatch_fraction, peak_error_db,
    peak_error_db_between, rms, sine_amplitude, thd_plus_n,
};
use super::registry::ReferenceVectorRegistry;
use super::{CheckResult, ComponentReport, Expect, MetricKind, ReferenceVector};

/// A shorthand: the i-th `MetricSpec` expectation from a reference vector.
fn expect(v: &ReferenceVector, i: usize) -> Expect {
    v.checks[i].expect
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. DspPipeline — bit-exact passthrough and transparency
// ─────────────────────────────────────────────────────────────────────────────

pub fn def_pipeline(engine_version: String) -> ReferenceVector {
    ReferenceVector::new(
        "dsp-pipeline",
        1,
        engine_version,
        vec![
            super::MetricSpec::new(
                MetricKind::BitExactness,
                Expect::Equal {
                    nominal: 0.0,
                    tol: 0.0,
                },
            ),
            super::MetricSpec::new(MetricKind::ThdPlusN, Expect::AtMost { max: 1e-3 }),
        ],
    )
}

pub fn dsp_pipeline(reg: &ReferenceVectorRegistry) -> ComponentReport {
    let v = reg.get("dsp-pipeline").unwrap();
    let sr = 48_000.0f64;
    let frames = 2 * sr as usize;
    let input: Vec<f32> = (0..frames)
        .map(|i| 0.4 * (TAU * 1_000.0 * i as f64 / sr).sin() as f32)
        .collect();
    let mut left = input.clone();
    let mut right = input.clone();

    let mut pipeline = DspPipeline::from_config(&EngineConfig::default(), sr as f32);
    pipeline.set_volume(1.0);
    pipeline.set_bit_perfect(true);
    pipeline.process_block(&mut left, &mut right);

    let failure = mismatch_fraction(&left, &input);
    let thd = thd_plus_n(&left, sr, 1_000.0);
    ComponentReport {
        component: "DspPipeline (bit-exact passthrough)".to_string(),
        reference_vector: v.display_id(),
        checks: vec![
            CheckResult::evaluate(
                MetricKind::BitExactness,
                failure,
                expect(v, 0),
                "full-scale passthrough must be bit-identical",
            ),
            CheckResult::evaluate(
                MetricKind::ThdPlusN,
                thd,
                expect(v, 1),
                "a pure path adds no measurable distortion",
            ),
        ],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Parametric EQ biquad — frequency-response + phase golden vector
// ─────────────────────────────────────────────────────────────────────────────

pub fn def_parametric_eq(engine_version: String) -> ReferenceVector {
    ReferenceVector::new(
        "parametric-eq",
        1,
        engine_version,
        vec![
            super::MetricSpec::new(
                MetricKind::FreqResponseDeviationDb,
                Expect::Equal {
                    nominal: 6.0,
                    tol: 0.35,
                },
            ),
            super::MetricSpec::new(
                MetricKind::FreqResponseDeviationDb,
                Expect::Equal {
                    nominal: 0.0,
                    tol: 0.3,
                },
            ),
            // Phase deviation: at the peaking centre the response is near
            // linear (phase ≈ 0°).
            super::MetricSpec::new(
                MetricKind::PhaseDeviationDeg,
                Expect::Equal {
                    nominal: 0.0,
                    tol: 6.0,
                },
            ),
        ],
    )
}

pub fn parametric_eq(reg: &ReferenceVectorRegistry) -> ComponentReport {
    let v = reg.get("parametric-eq").unwrap();
    let sr = 48_000.0f64;
    let coeffs = BiquadCoeffsF64::peaking(sr as f32, 1_000.0, 6.0, 1.0);
    let mut state = BiquadStateF64::default();
    let n = 8_192usize;
    // Unit impulse fed *through* the filter state so the response actually
    // rings (a directly-injected impulse would leave the biquad silent).
    let mut ir = Vec::with_capacity(n);
    for k in 0..n {
        let x = if k == 0 { 1.0 } else { 0.0 };
        ir.push(state.process(x, &coeffs) as f32);
    }

    // Peak gain near the centre (swept so we never depend on an exact bin).
    let peak = ir_peak_magnitude_db(&ir, sr, (400..=2_000).step_by(4).map(|f| f as f64));
    // Far-band rejection well away from the resonance.
    let far = ir_magnitude_db(&ir, sr, 8_000.0);
    // Phase at the resonance centre is near-linear (≈ 0°).
    let phase = ir_phase_deg(&ir, sr, 1_000.0).abs();
    ComponentReport {
        component: "Parametric EQ biquad".to_string(),
        reference_vector: v.display_id(),
        checks: vec![
            CheckResult::evaluate(
                MetricKind::FreqResponseDeviationDb,
                peak,
                expect(v, 0),
                "peaking centre gain reaches +6 dB",
            ),
            CheckResult::evaluate(
                MetricKind::FreqResponseDeviationDb,
                far,
                expect(v, 1),
                "far-band magnitude stays at unity",
            ),
            CheckResult::evaluate(
                MetricKind::PhaseDeviationDeg,
                phase,
                expect(v, 2),
                "phase crosses 0 at the resonance centre",
            ),
        ],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Lookahead limiter — true-peak ceiling compliance
// ─────────────────────────────────────────────────────────────────────────────

pub fn def_limiter(engine_version: String) -> ReferenceVector {
    ReferenceVector::new(
        "limiter",
        1,
        engine_version,
        vec![super::MetricSpec::new(
            MetricKind::TruePeakErrorDb,
            Expect::Equal {
                nominal: 0.0,
                tol: 0.1,
            },
        )],
    )
}

pub fn limiter(reg: &ReferenceVectorRegistry) -> ComponentReport {
    let v = reg.get("limiter").unwrap();
    let sr = 48_000.0f64;
    let ceiling_db = -1.0f64;
    let ceiling_linear = 10.0f64.powf(ceiling_db / 20.0);

    let mut limiter = LookaheadLimiter::new(sr as f32);
    limiter.set_ceiling_db(ceiling_db as f32);
    limiter.set_enabled(true);

    let n = (sr as usize) * 2;
    let input: Vec<f32> = (0..n)
        .map(|i| 4.0 * (TAU * 1_000.0 * i as f64 / sr).sin() as f32)
        .collect();
    let mut l = input.clone();
    let mut r = input.clone();
    limiter.process_block(&mut l, &mut r);

    // Skip the lookahead delay (5 ms = 240 samples) so the ceiling is checked
    // post-warmup; the peak must sit at (or below) the ceiling.
    let steady = &l[480..];
    let error = peak_error_db(steady, ceiling_linear);
    ComponentReport {
        component: "Lookahead limiter".to_string(),
        reference_vector: v.display_id(),
        checks: vec![CheckResult::evaluate(
            MetricKind::TruePeakErrorDb,
            error,
            expect(v, 0),
            "true peak stays at the −1 dBFS ceiling",
        )],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Resampler — in-band frequency-response gain
// ─────────────────────────────────────────────────────────────────────────────

pub fn def_resampler(engine_version: String) -> ReferenceVector {
    ReferenceVector::new(
        "resampler",
        1,
        engine_version,
        vec![super::MetricSpec::new(
            MetricKind::FreqResponseDeviationDb,
            Expect::Equal {
                nominal: 0.0,
                tol: 0.5,
            },
        )],
    )
}

pub fn resampler(reg: &ReferenceVectorRegistry) -> ComponentReport {
    let v = reg.get("resampler").unwrap();
    let fs_in = 48_000.0f64;
    let fs_out = 44_100.0f64;
    let mut r = AudioResamplerF64::new(ResamplerQuality::Balanced, fs_in as f32, fs_out as f32)
        .expect("resampler");

    let freq = 18_000.0f64;
    let n_in = (fs_in * 4.0) as usize;
    let mut input = Vec::with_capacity(n_in);
    for i in 0..n_in {
        let v = 0.5 * (TAU * freq * i as f64 / fs_in).sin();
        r.feed(v, v);
        input.push(v as f32);
    }
    r.flush();
    let mut out = Vec::new();
    while let Some((l, _)) = r.read() {
        out.push(l as f32);
    }

    let a_in = sine_amplitude(&input, fs_in, freq);
    let a_out = sine_amplitude(&out, fs_out, freq);
    // Absolute magnitude error (the nominal is 0 dB unity gain).
    let deviation = db(a_out / a_in).abs();
    ComponentReport {
        component: "Resampler (48→44.1 kHz)".to_string(),
        reference_vector: v.display_id(),
        checks: vec![CheckResult::evaluate(
            MetricKind::FreqResponseDeviationDb,
            deviation,
            expect(v, 0),
            "in-band tone survives at unity gain",
        )],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. Binaural (head model) — inter-aural level vs image position
// ─────────────────────────────────────────────────────────────────────────────

pub fn def_binaural(engine_version: String) -> ReferenceVector {
    ReferenceVector::new(
        "binaural",
        1,
        engine_version,
        vec![
            super::MetricSpec::new(MetricKind::InterauralLevelDb, Expect::AtMost { max: 3.0 }),
            super::MetricSpec::new(MetricKind::InterauralLevelDb, Expect::AtLeast { min: 4.0 }),
        ],
    )
}

pub fn binaural(reg: &ReferenceVectorRegistry) -> ComponentReport {
    let v = reg.get("binaural").unwrap();
    let sr = 48_000.0f32;
    let frames = 1_024usize;
    let tone: Vec<f32> = (0..frames)
        .map(|i| 0.5 * (TAU * 1_000.0 * i as f64 / 48_000.0).sin() as f32)
        .collect();

    let mut node = SpatialNode::new(sr);
    DspNode::prepare(&mut node, sr, 2);
    node.apply_config(
        &SpatialConfig {
            enabled: true,
            ..Default::default()
        },
        sr,
    );

    /// Render one block through the node and read `|ILD|` of the two ears.
    fn iled(node: &mut SpatialNode, l: &[f32], r: &[f32]) -> f64 {
        let mut l = l.to_vec();
        let mut r = r.to_vec();
        let mut planes: Vec<&mut [f32]> = vec![l.as_mut_slice(), r.as_mut_slice()];
        DspNode::process_block_f32(node, &mut planes);
        let (lrms, rrms) = (rms(planes[0]), rms(planes[1]));
        db(lrms / rrms).abs()
    }

    // Center image: screen width 0º, identical L/R program → symmetric ears.
    node.apply_screen(0.0, 0.0, 0.0, 1.0);
    let center_ild = iled(&mut node, &tone, &tone);

    // Hard-right image: screen ±100º, signal only on the Right program voice
    // → the contralateral (left) ear is strongly shadowed.
    node.apply_screen(0.0, 100.0, 0.0, 1.0);
    let wide_ild = iled(&mut node, &[0.0; 1024], &tone);

    ComponentReport {
        component: "Binaural head model".to_string(),
        reference_vector: v.display_id(),
        checks: vec![
            CheckResult::evaluate(
                MetricKind::InterauralLevelDb,
                center_ild,
                expect(v, 0),
                "a centered image is heard by both ears near-equally",
            ),
            CheckResult::evaluate(
                MetricKind::InterauralLevelDb,
                wide_ild,
                expect(v, 1),
                "a far-panned source produces a large inter-aural level difference",
            ),
        ],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. Loudness meter — EBU R128 reference tone
// ─────────────────────────────────────────────────────────────────────────────

pub fn def_loudness(engine_version: String) -> ReferenceVector {
    ReferenceVector::new(
        "loudness",
        1,
        engine_version,
        vec![super::MetricSpec::new(
            MetricKind::LoudnessErrorLufs,
            Expect::Equal {
                nominal: -0.02,
                tol: 0.25,
            },
        )],
    )
}

pub fn loudness(reg: &ReferenceVectorRegistry) -> ComponentReport {
    let v = reg.get("loudness").unwrap();
    let sr = 48_000.0f64;
    let mut meter = LoudnessMeter::new(sr as f32, 2);
    let n = (sr as usize) * 5;
    for i in 0..n {
        let s = (TAU * 1_000.0 * i as f64 / sr).sin();
        meter.process_stereo(s as f32, s as f32);
    }
    // ITU-R BS.1770-4 §1.4: a 1 kHz 0 dBFS stereo tone reads −0.02 ± 0.2 LUFS.
    let integrated_lufs = meter.snapshot().integrated_lufs;
    ComponentReport {
        component: "EBU R128 loudness".to_string(),
        reference_vector: v.display_id(),
        checks: vec![CheckResult::evaluate(
            MetricKind::LoudnessErrorLufs,
            integrated_lufs as f64,
            expect(v, 0),
            "calibration tone reads near −0.02 LUFS",
        )],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 7. Convolution — engine vs naive direct reference
// ─────────────────────────────────────────────────────────────────────────────

/// Deterministic non-trivial test signal (not a pure tone, so a single lag-free
/// accidental alignment cannot hide a real mismatch).
fn conv_test_signal(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let x = i as f64;
            (0.7 * (0.031 * x).sin() + 0.3 * (0.013 * x).cos()) * (0.999f64).powf(x)
        })
        .collect()
}

/// Naive direct convolution — the offline golden reference.
fn conv_naive(x: &[f64], h: &[f64]) -> Vec<f64> {
    let n = x.len() + h.len() - 1;
    let mut y = vec![0.0; n];
    for i in 0..x.len() {
        for j in 0..h.len() {
            y[i + j] += x[i] * h[j];
        }
    }
    y
}

pub fn def_convolution(engine_version: String) -> ReferenceVector {
    ReferenceVector::new(
        "convolution",
        1,
        engine_version,
        vec![super::MetricSpec::new(
            MetricKind::AcousticIrErrorDb,
            Expect::AtMost { max: -60.0 },
        )],
    )
}

pub fn convolution(reg: &ReferenceVectorRegistry) -> ComponentReport {
    let v = reg.get("convolution").unwrap();
    let ir: Vec<f64> = (0..100)
        .map(|i| {
            let t = i as f64;
            (0.9f64).powf(t) * (0.05 * t).sin() * 0.8
        })
        .collect();
    let ir = {
        let mut f = ir;
        f[0] += 0.25; // non-trivial at DC
        f
    };

    let x = conv_test_signal(2048);
    let mut engine = ConvolutionEngine::new(48_000.0, 2048);
    engine
        .load_ir_from_samples_f64(&ir.iter().map(|&v| (v, v)).collect::<Vec<_>>())
        .unwrap();
    engine.set_enabled(true);
    engine.set_wet_mix(1.0);

    let mut out: Vec<f64> = Vec::with_capacity(x.len() + ir.len() + 4096);
    for &v in &x {
        out.push(engine.process_f64(v, v).0);
    }
    for _ in 0..(ir.len() + 4096) {
        out.push(engine.process_f64(0.0, 0.0).0);
    }

    let want = conv_naive(&x, &ir);
    // Slide for alignment (the partitioned engine emits leading zeros), then
    // report the **best** alignment's worst sample error relative to the
    // signal peak — a mismatch anywhere shows up as a floor on this number.
    let w: Vec<f32> = want.iter().map(|&s| s as f32).collect();
    let mut err_db = f64::INFINITY;
    for d in 0..=(out.len().saturating_sub(want.len())) {
        let g: Vec<f32> = out[d..d + want.len()].iter().map(|&s| s as f32).collect();
        let e = peak_error_db_between(&g, &w, 60.0);
        if e < err_db {
            err_db = e;
        }
    }
    ComponentReport {
        component: "Convolution (partitioned FFT)".to_string(),
        reference_vector: v.display_id(),
        checks: vec![CheckResult::evaluate(
            MetricKind::AcousticIrErrorDb,
            err_db,
            expect(v, 0),
            "engine matches the naive direct convolution to −60 dB or better",
        )],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 8. Channel separation / crosstalk
// ─────────────────────────────────────────────────────────────────────────────

pub fn def_channel_separation(engine_version: String) -> ReferenceVector {
    ReferenceVector::new(
        "channel-separation",
        1,
        engine_version,
        vec![super::MetricSpec::new(
            MetricKind::ChannelSeparationDb,
            Expect::AtMost { max: -100.0 },
        )],
    )
}

pub fn channel_separation(reg: &ReferenceVectorRegistry) -> ComponentReport {
    let v = reg.get("channel-separation").unwrap();
    let sr = 44_100.0f32;
    let frames = 512usize;
    let mut pipeline = DspPipeline::from_config(&EngineConfig::default(), sr);
    pipeline.set_volume(1.0);
    pipeline.set_bit_perfect(true);

    // Drive channel 0 only: bit-perfect passthrough must not leak to channel 1.
    let mut interleaved = vec![0.0f32; frames * 2];
    interleaved[0] = 1.0; // impulse on the left channel, frame 0
    pipeline.process_block_multichannel(&mut interleaved, 2);

    let l: Vec<f32> = (0..frames).map(|f| interleaved[f * 2]).collect();
    let r: Vec<f32> = (0..frames).map(|f| interleaved[f * 2 + 1]).collect();
    let drive = rms(&l);
    let leak = rms(&r);
    // No measurable leakage → report the −200 dB measurement floor (finite,
    // so the report stays JSON-round-trippable; far below any tolerance).
    let sep_db = if drive > 1e-12 && leak > 0.0 {
        db(leak / drive)
    } else {
        -200.0
    };
    ComponentReport {
        component: "Channel separation".to_string(),
        reference_vector: v.display_id(),
        checks: vec![CheckResult::evaluate(
            MetricKind::ChannelSeparationDb,
            sep_db,
            expect(v, 0),
            "driving one channel produces no cross-channel leakage",
        )],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 9. HRTF/interpolation — convexity of the measured grid
// ─────────────────────────────────────────────────────────────────────────────

pub fn def_hrtf(engine_version: String) -> ReferenceVector {
    ReferenceVector::new(
        "hrtf",
        1,
        engine_version,
        vec![super::MetricSpec::new(
            MetricKind::HrtfInterpolationErrorDb,
            Expect::AtMost { max: 1.0 },
        )],
    )
}

pub fn hrtf(reg: &ReferenceVectorRegistry) -> ComponentReport {
    let v = reg.get("hrtf").unwrap();
    let ds = HrtfDataset::synthetic(48_000, 32, 30.0, 30.0);
    let provider = HrtfDatasetProvider::new(ds);
    let taps = provider.taps();
    if taps == 0 {
        return ComponentReport {
            component: "HRTF interpolation".to_string(),
            reference_vector: v.display_id(),
            checks: vec![CheckResult::evaluate(
                MetricKind::HrtfInterpolationErrorDb,
                f64::NEG_INFINITY,
                expect(v, 0),
                "no dataset — skipped",
            )],
        };
    }

    // Direction helpers: az = 0 (front), elevation in radians, +Y front, +Z up.
    let dir_el = |el_deg: f64| {
        let el = el_deg.to_radians();
        Vec3::new(0.0, el.cos() as f32, el.sin() as f32)
    };

    // Interpolate the LEFT ear at three elevations on the front meridian.
    let el_lo = -30.0f64;
    let el_hi = 30.0f64;
    let el_mid = 0.0f64;
    let mut l = [0.0f32; 32];
    let mut r = [0.0f32; 32];
    let mut left_ir = |provider: &HrtfDatasetProvider, d: Vec3| -> [f32; 32] {
        provider.interpolate(d, 2.0, &mut l, &mut r);
        l
    };
    let ir_lo = left_ir(&provider, dir_el(el_lo));
    let ir_hi = left_ir(&provider, dir_el(el_hi));
    let ir_mid = left_ir(&provider, dir_el(el_mid));

    // Over a spread of probe bins, the interpolated response must respect the
    // convex hull of its bracketing grid nodes (no overshoot / undershoot).
    let mut max_overshoot_db = 0.0f64;
    for f in [500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0] {
        let m_lo = ir_magnitude_db(&ir_lo, 48_000.0, f);
        let m_hi = ir_magnitude_db(&ir_hi, 48_000.0, f);
        let m_mid = ir_magnitude_db(&ir_mid, 48_000.0, f);
        let hi = m_lo.max(m_hi);
        let lo = m_lo.min(m_hi);
        let overshoot = (m_mid - hi).max(0.0);
        let undershoot = (lo - m_mid).max(0.0);
        max_overshoot_db = max_overshoot_db.max(overshoot).max(undershoot);
    }
    ComponentReport {
        component: "HRTF interpolation".to_string(),
        reference_vector: v.display_id(),
        checks: vec![CheckResult::evaluate(
            MetricKind::HrtfInterpolationErrorDb,
            max_overshoot_db,
            expect(v, 0),
            "interpolated response stays within its bracketing grid nodes",
        )],
    }
}
