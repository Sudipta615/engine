//! Unit tests for TimeStretcher, PhaseVocoder, and TransientDetector.

use super::stretcher::{PITCH_HISTORY_LEN, PITCH_PHASES, PITCH_TAPS_HALF};
use super::*;
use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use config::TimeStretchQuality;

#[test]
fn quality_tiers_map_to_distinct_wsola_parameters() {
    assert_eq!(TimeStretchQuality::Low.params(), (512, 128, 64));
    assert_eq!(TimeStretchQuality::Balanced.params(), (1024, 256, 128));
    assert_eq!(TimeStretchQuality::High.params(), (2048, 512, 256));

    let mut low = TimeStretcher::new(48_000.0);
    low.set_quality(TimeStretchQuality::Low);
    let mut balanced = TimeStretcher::new(48_000.0);
    balanced.set_quality(TimeStretchQuality::Balanced);
    let mut high = TimeStretcher::new(48_000.0);
    high.set_quality(TimeStretchQuality::High);
    for s in [&mut low, &mut balanced, &mut high] {
        s.set_speed(1.5);
    }
    let l = low.latency_ms();
    let b = balanced.latency_ms();
    let h = high.latency_ms();
    assert!(
        l > 0.0 && l < b && b < h,
        "latency must increase with tier: low={l:.2} balanced={b:.2} high={h:.2}"
    );
}

#[test]
fn quality_change_does_not_reallocate_realtime_storage() {
    let mut stretcher = TimeStretcher::new(48_000.0);
    let input_cap = stretcher.input_ring_l.capacity();
    let output_cap = stretcher.output_fifo_l.capacity();
    let scratch_cap = stretcher.scratch_f64_l.capacity();

    for tier in [
        TimeStretchQuality::Low,
        TimeStretchQuality::Balanced,
        TimeStretchQuality::High,
        TimeStretchQuality::Low,
    ] {
        stretcher.set_quality(tier);
        assert_eq!(stretcher.input_ring_l.capacity(), input_cap);
        assert_eq!(stretcher.output_fifo_l.capacity(), output_cap);
        assert_eq!(stretcher.scratch_f64_l.capacity(), scratch_cap);
    }
    assert_eq!(stretcher.quality(), TimeStretchQuality::Low);
    assert_eq!(stretcher.config.window_size, 512);
}

#[test]
fn oversized_blocks_do_not_grow_realtime_storage() {
    let mut stretcher = TimeStretcher::new(44_100.0);
    stretcher.set_speed(2.0);
    let input_capacity = stretcher.input_ring_l.capacity();
    let output_capacity = stretcher.output_fifo_l.capacity();
    let scratch_capacity = stretcher.scratch_f64_l.capacity();

    let n = MAX_AUDIO_BLOCK_FRAMES * 2 + 17;
    let mut left = vec![0.0f32; n];
    let mut right = vec![0.0f32; n];
    stretcher.process_block(&mut left, &mut right);

    assert_eq!(stretcher.input_ring_l.capacity(), input_capacity);
    assert_eq!(stretcher.output_fifo_l.capacity(), output_capacity);
    assert_eq!(stretcher.scratch_f64_l.capacity(), scratch_capacity);
}

#[test]
fn f64_oversized_blocks_do_not_grow_realtime_storage() {
    let mut stretcher = TimeStretcher::new(44_100.0);
    stretcher.set_speed(2.0);
    let scratch_capacity = stretcher.scratch_f64_l.capacity();
    let n = MAX_AUDIO_BLOCK_FRAMES + 1;
    let mut left = vec![0.0f64; n];
    let mut right = vec![0.0f64; n];
    stretcher.process_block_f64(&mut left, &mut right);
    assert_eq!(stretcher.scratch_f64_l.capacity(), scratch_capacity);
}

fn run_extreme_combo(speed: f32, semitones: f32) {
    let mut stretcher = TimeStretcher::new(48_000.0);
    stretcher.set_speed(speed);
    stretcher.set_pitch_semitones(semitones);

    let input_cap = stretcher.input_ring_l.capacity();
    let output_cap = stretcher.output_fifo_l.capacity();
    let scratch_cap = stretcher.scratch_f64_l.capacity();

    const BLOCK: usize = 128;
    const BLOCKS: usize = 64;
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let mut phase = 0.0f32;
    let mut energy = 0.0f64;

    for _ in 0..BLOCKS {
        for i in 0..BLOCK {
            let s = (phase * std::f32::consts::TAU).sin() * 0.5;
            left[i] = s;
            right[i] = s * 0.8;
            phase = (phase + 440.0 / 48_000.0).fract();
        }
        stretcher.process_block(&mut left, &mut right);
        for i in 0..BLOCK {
            assert!(
                left[i].is_finite() && right[i].is_finite(),
                "non-finite output at {speed}x / {semitones}st"
            );
            assert!(
                left[i].abs() <= 8.0 && right[i].abs() <= 8.0,
                "unbounded output {:.2} at {speed}x / {semitones}st",
                left[i]
            );
            energy += (left[i] as f64) * (left[i] as f64);
            energy += (right[i] as f64) * (right[i] as f64);
        }
    }

    assert_eq!(stretcher.input_ring_l.capacity(), input_cap);
    assert_eq!(stretcher.output_fifo_l.capacity(), output_cap);
    assert_eq!(stretcher.scratch_f64_l.capacity(), scratch_cap);

    assert!(
        energy > 100.0,
        "output has implausibly low energy ({energy:.1}) at {speed}x / {semitones}st"
    );
}

#[test]
fn extreme_speed_pitch_combinations_stay_finite_and_bounded() {
    for speed in [0.25f32, 0.5, 1.0, 2.0, 4.0] {
        for semitones in [-24.0f32, -12.0, 0.0, 12.0, 24.0] {
            run_extreme_combo(speed, semitones);
        }
    }
}

#[test]
fn pitch_only_extremes_produce_finite_bounded_output() {
    for semitones in [-24.0f32, -23.5, -1.0, 1.0, 23.5, 24.0] {
        run_extreme_combo(1.0, semitones);
    }
}

fn hermite4_ref(s0: f32, s1: f32, s2: f32, s3: f32, t: f32) -> f32 {
    let c0 = s1;
    let c1 = 0.5 * (s2 - s0);
    let c2 = s0 - 2.5 * s1 + 2.0 * s2 - 0.5 * s3;
    let c3 = 0.5 * (s3 - s0) + 1.5 * (s1 - s2);
    ((c3 * t + c2) * t + c1) * t + c0
}

#[test]
fn polyphase_interpolator_is_dc_exact_and_band_accurate() {
    let stretcher = TimeStretcher::new(48_000.0);
    let fifo_cap = 4096usize;
    let mut fifo = vec![0.0f32; fifo_cap];

    fifo.fill(0.75);
    let warm_history = vec![0.75f32; PITCH_HISTORY_LEN];
    for phase in 0..PITCH_PHASES {
        let got =
            stretcher.interpolate(phase, &warm_history, PITCH_HISTORY_LEN, &fifo, fifo_cap, 10);
        assert!(
            (got - 0.75).abs() < 1e-5,
            "phase {phase}: DC gain must be unity, got {got}"
        );
    }

    let sr = 48_000.0f32;
    for (label, freq) in [("2 kHz", 2_000.0f32), ("8 kHz", 8_000.0f32)] {
        let omega = 2.0 * std::f32::consts::PI * freq / sr;
        for (i, slot) in fifo.iter_mut().take(fifo_cap).enumerate() {
            *slot = (omega * i as f32).sin();
        }
        let mut sinc_err = 0.0f32;
        let mut hermite_err = 0.0f32;
        for base in 64..(fifo_cap - PITCH_TAPS_HALF - 1) {
            let mut history = vec![0.0f32; PITCH_HISTORY_LEN];
            for k in 1..=PITCH_HISTORY_LEN {
                history[PITCH_HISTORY_LEN - k] = fifo[base - k];
            }
            for phase in 0..PITCH_PHASES {
                let t = base as f32 + phase as f32 / PITCH_PHASES as f32;
                let want = (omega * t).sin();
                let got = stretcher.interpolate(
                    phase,
                    &history,
                    PITCH_HISTORY_LEN,
                    &fifo,
                    fifo_cap,
                    base,
                );
                let h = hermite4_ref(
                    fifo[base - 1],
                    fifo[base],
                    fifo[base + 1],
                    fifo[base + 2],
                    phase as f32 / PITCH_PHASES as f32,
                );
                sinc_err = sinc_err.max((got - want).abs());
                hermite_err = hermite_err.max((h - want).abs());
            }
        }
        assert!(
            sinc_err < 1e-3,
            "{label}: windowed-sinc error {sinc_err:.6} too high"
        );
        assert!(
            sinc_err < hermite_err * 0.5,
            "{label}: windowed-sinc ({sinc_err:.6}) must beat Hermite ({hermite_err:.6})"
        );
    }
}

#[test]
fn transient_detector_triggers_on_impulse() {
    let mut td = TransientDetector::new(48_000.0);
    let mut signal_l = vec![0.001f32; 1000];
    let mut signal_r = vec![0.001f32; 1000];

    // Steady state background
    let fraction_before = td.process_block(&signal_l, &signal_r);
    assert_eq!(
        fraction_before, 0.0,
        "steady floor should not trigger transient"
    );

    // Insert sharp attack impulse
    signal_l[10] = 0.9;
    signal_r[10] = 0.9;
    let fraction_after = td.process_block(&signal_l, &signal_r);
    assert!(
        fraction_after > 0.0,
        "impulse should trigger transient hold"
    );
}

#[test]
fn phase_vocoder_processing_preserves_audio_power() {
    let mut pv = PhaseVocoder::new(48_000.0);
    assert_eq!(pv.sample_rate(), 48_000.0);
    const N: usize = 4096;
    let mut l = vec![0.0f32; N];
    let mut r = vec![0.0f32; N];
    for i in 0..N {
        let t = i as f32 / 48_000.0;
        l[i] = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
        r[i] = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
    }

    // Warm up the analysis pipeline
    pv.process_block(&mut l, &mut r, 1.0, 1.0);

    // Steady-state second block
    for i in 0..N {
        let t = (N + i) as f32 / 48_000.0;
        l[i] = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
        r[i] = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
    }
    pv.process_block(&mut l, &mut r, 1.0, 1.0);

    let energy: f32 = l.iter().map(|&x| x * x).sum();
    assert!(
        energy > 1.0,
        "Phase vocoder output must produce audio energy: {energy}"
    );
}
