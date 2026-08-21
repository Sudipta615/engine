//! Crossfade, gapless sequencing, and RT-safety fidelity tests.

use config::ResamplerQuality;
use engine::dsp::{
    crossfade::{CrossfadeCurve, TrackMixer},
    resampler::AudioResamplerF32,
};

#[test]
fn test_track_mixer_linear_vs_equal_power() {
    let mut mixer_linear = TrackMixer::new(48000.0);
    mixer_linear.set_curve(CrossfadeCurve::Linear);
    mixer_linear.set_duration_ms(1000, 48000.0); // 1-second crossfade (48000 frames)
    mixer_linear.start_crossfade();

    let mut mixer_eq_power = TrackMixer::new(48000.0);
    mixer_eq_power.set_curve(CrossfadeCurve::EqualPower);
    mixer_eq_power.set_duration_ms(1000, 48000.0);
    mixer_eq_power.start_crossfade();

    // At frame 0: outgoing is 1.0, incoming is 0.0
    let (l_lin_0, _) = mixer_linear.process(1.0, 1.0, 1.0, 1.0);
    let (l_eq_0, _) = mixer_eq_power.process(1.0, 1.0, 1.0, 1.0);
    assert!((l_lin_0 - 1.0).abs() < 1e-4);
    assert!((l_eq_0 - 1.0).abs() < 1e-4);

    // Fast forward to midpoint (frame 24000)
    for _ in 1..24000 {
        mixer_linear.process(1.0, 1.0, 1.0, 1.0);
        mixer_eq_power.process(1.0, 1.0, 1.0, 1.0);
    }

    // Midpoint:
    // Linear: outgoing 0.5 + incoming 0.5 = 1.0
    // EqualPower: cos(pi/4) + sin(pi/4) ≈ 0.7071 + 0.7071 ≈ 1.4142 (power-sum is 1.0)
    let (l_lin_mid, _) = mixer_linear.process(1.0, 1.0, 1.0, 1.0);
    let (l_eq_mid, _) = mixer_eq_power.process(1.0, 1.0, 1.0, 1.0);

    assert!((l_lin_mid - 1.0).abs() < 0.05, "linear mid: {l_lin_mid}");
    assert!(
        (l_eq_mid - 1.414).abs() < 0.05,
        "equal power mid: {l_eq_mid} should be ~1.414"
    );

    // Fast forward to end of crossfade
    for _ in 24001..48000 {
        mixer_linear.process(1.0, 1.0, 1.0, 1.0);
        mixer_eq_power.process(1.0, 1.0, 1.0, 1.0);
    }

    assert!(!mixer_linear.is_crossfading());
    assert!(!mixer_eq_power.is_crossfading());

    // After crossfade completes: only incoming track is output
    let (out_l, _) = mixer_linear.process(0.0, 0.0, 0.8, 0.8);
    assert!((out_l - 0.8).abs() < 1e-4);
}

#[test]
fn test_dual_resampler_crossfade_mixed_sample_rates() {
    // Outgoing: 44.1 kHz track
    // Incoming: 48.0 kHz track
    // Hardware output: 96.0 kHz DAC
    let mut resampler_out =
        AudioResamplerF32::new(ResamplerQuality::Balanced, 44100.0, 96000.0).unwrap();
    let mut resampler_in =
        AudioResamplerF32::new(ResamplerQuality::Balanced, 48000.0, 96000.0).unwrap();
    let mut mixer = TrackMixer::new(96000.0);
    mixer.set_curve(CrossfadeCurve::EqualPower);
    mixer.set_duration_ms(1000, 96000.0); // 1-second at 96 kHz
    mixer.start_crossfade();

    let mut output_samples = Vec::new();

    // Feed 1 second worth of 44.1 kHz outgoing (1 kHz sine) and 48 kHz incoming (2 kHz sine)
    for i in 0..44100 {
        let s_out = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 44100.0).sin() * 0.5;
        resampler_out.feed(s_out, s_out);

        // Feed matching time slice for 48 kHz incoming
        let i_48 = (i as f64 * 48000.0 / 44100.0) as usize;
        let s_in = (2.0 * std::f32::consts::PI * 2000.0 * i_48 as f32 / 48000.0).sin() * 0.5;
        resampler_in.feed(s_in, s_in);

        while let (Some((out_l, out_r)), Some((in_l, in_r))) =
            (resampler_out.read(), resampler_in.read())
        {
            let (mix_l, mix_r) = mixer.process(out_l, out_r, in_l, in_r);
            assert!(mix_l.is_finite() && mix_r.is_finite());
            output_samples.push(mix_l);
        }
    }

    // Drain remaining buffered samples from both resamplers
    while let (Some((out_l, out_r)), Some((in_l, in_r))) =
        (resampler_out.read(), resampler_in.read())
    {
        let (mix_l, mix_r) = mixer.process(out_l, out_r, in_l, in_r);
        assert!(mix_l.is_finite() && mix_r.is_finite());
        output_samples.push(mix_l);
    }

    assert!(
        output_samples.len() > 75000,
        "Should generate substantial output frames across 1-sec crossfade, got {}",
        output_samples.len()
    );
}

#[test]
fn test_crossfade_resampling_rate_matrix_stays_sample_synchronised() {
    // These are the rate transitions most likely to expose a source/output
    // clock mismatch. Each case runs a real dual-resampler stream through the
    // stateful mixer rather than checking the mixer in isolation.
    let rate_pairs = [
        (44_100usize, 48_000usize, 48_000usize),
        (48_000, 96_000, 96_000),
        (96_000, 44_100, 44_100),
    ];
    let durations_ms = [500u64, 2_000, 5_000, 10_000];

    for (outgoing_rate, incoming_rate, output_rate) in rate_pairs {
        for duration_ms in durations_ms {
            let duration_frames =
                (duration_ms as f64 * output_rate as f64 / 1_000.0).round() as usize;
            // Include enough source material for the requested transition plus
            // a resampler filter margin. Both tracks represent the same amount
            // of source time, so the two output streams must remain pairable.
            let output_budget = duration_frames + 4_096;
            let outgoing_source_frames =
                (output_budget as f64 * outgoing_rate as f64 / output_rate as f64).ceil() as usize
                    + 512;
            let incoming_source_frames =
                (output_budget as f64 * incoming_rate as f64 / output_rate as f64).ceil() as usize
                    + 512;

            let mut outgoing = AudioResamplerF32::new(
                ResamplerQuality::Balanced,
                outgoing_rate as f32,
                output_rate as f32,
            )
            .unwrap();
            let mut incoming = AudioResamplerF32::new(
                ResamplerQuality::Balanced,
                incoming_rate as f32,
                output_rate as f32,
            )
            .unwrap();
            let mut outgoing_samples = Vec::new();
            let mut incoming_samples = Vec::new();
            for _ in 0..outgoing_source_frames {
                outgoing.feed(0.25, -0.25);
                while let Some(sample) = outgoing.read() {
                    outgoing_samples.push(sample);
                }
            }
            for _ in 0..incoming_source_frames {
                incoming.feed(-0.25, 0.25);
                while let Some(sample) = incoming.read() {
                    incoming_samples.push(sample);
                }
            }
            outgoing.flush();
            incoming.flush();
            while let Some(sample) = outgoing.read() {
                outgoing_samples.push(sample);
            }
            while let Some(sample) = incoming.read() {
                incoming_samples.push(sample);
            }

            let mut mixer = TrackMixer::new(output_rate as f32);
            mixer.set_curve(CrossfadeCurve::EqualPower);
            mixer.set_duration_ms(duration_ms, output_rate as f32);
            assert_eq!(mixer.duration_frames(), duration_frames);
            mixer.start_crossfade();

            assert!(
                outgoing_samples.len() >= duration_frames,
                "outgoing output too short: {} -> {} Hz, {} ms, {} < {}",
                outgoing_rate,
                output_rate,
                duration_ms,
                outgoing_samples.len(),
                duration_frames
            );
            assert!(
                incoming_samples.len() >= duration_frames,
                "incoming output too short: {} -> {} Hz, {} ms, {} < {}",
                incoming_rate,
                output_rate,
                duration_ms,
                incoming_samples.len(),
                duration_frames
            );
            let mut paired_frames = 0usize;
            let mut transition_frames = 0usize;
            for (&(out_l, out_r), &(in_l, in_r)) in
                outgoing_samples.iter().zip(incoming_samples.iter())
            {
                let (left, right) = mixer.process(out_l, out_r, in_l, in_r);
                assert!(left.is_finite() && right.is_finite());
                paired_frames += 1;
                if transition_frames < duration_frames {
                    transition_frames += 1;
                }
                if !mixer.is_crossfading() {
                    break;
                }
            }

            assert_eq!(
                transition_frames, duration_frames,
                "crossfade boundary drifted for {} -> {} Hz / {} -> {} Hz at {} ms",
                outgoing_rate, output_rate, incoming_rate, output_rate, duration_ms
            );
            assert!(
                paired_frames >= duration_frames,
                "not enough paired frames for {} -> {} Hz / {} -> {} Hz at {} ms: {paired_frames} < {duration_frames}",
                outgoing_rate, output_rate, incoming_rate, output_rate, duration_ms
            );
            let (left, right) = mixer.process(0.0, 0.0, 0.3, -0.3);
            assert!((left - 0.3).abs() < 1e-6 && (right + 0.3).abs() < 1e-6);
        }
    }
}

#[test]
fn test_crossfade_high_ratio_rate_matrix_stays_synchronised() {
    // The review called out 44.1 → 96, 44.1 → 192 and 48 → 192 as the rates
    // where a source-frame counter and an output-frame counter diverge. Each
    // case runs a real dual-resampler stream through the stateful mixer and
    // checks the transition lands exactly on its output-domain boundary.
    let rate_pairs = [
        (44_100usize, 44_100usize, 96_000usize),
        (44_100, 44_100, 192_000),
        (48_000, 48_000, 192_000),
        (44_100, 48_000, 192_000),
    ];
    let durations_ms = [500u64, 2_000];

    for (outgoing_rate, incoming_rate, output_rate) in rate_pairs {
        for duration_ms in durations_ms {
            let duration_frames =
                (duration_ms as f64 * output_rate as f64 / 1_000.0).round() as usize;
            let output_budget = duration_frames + 4_096;
            let outgoing_source_frames =
                (output_budget as f64 * outgoing_rate as f64 / output_rate as f64).ceil() as usize
                    + 512;
            let incoming_source_frames =
                (output_budget as f64 * incoming_rate as f64 / output_rate as f64).ceil() as usize
                    + 512;

            let mut outgoing = AudioResamplerF32::new(
                ResamplerQuality::Balanced,
                outgoing_rate as f32,
                output_rate as f32,
            )
            .unwrap();
            let mut incoming = AudioResamplerF32::new(
                ResamplerQuality::Balanced,
                incoming_rate as f32,
                output_rate as f32,
            )
            .unwrap();
            let mut outgoing_samples = Vec::new();
            let mut incoming_samples = Vec::new();
            for _ in 0..outgoing_source_frames {
                outgoing.feed(0.25, -0.25);
                while let Some(s) = outgoing.read() {
                    outgoing_samples.push(s);
                }
            }
            for _ in 0..incoming_source_frames {
                incoming.feed(-0.25, 0.25);
                while let Some(s) = incoming.read() {
                    incoming_samples.push(s);
                }
            }
            outgoing.flush();
            incoming.flush();
            while let Some(s) = outgoing.read() {
                outgoing_samples.push(s);
            }
            while let Some(s) = incoming.read() {
                incoming_samples.push(s);
            }

            let mut mixer = TrackMixer::new(output_rate as f32);
            mixer.set_curve(CrossfadeCurve::EqualPower);
            mixer.set_duration_ms(duration_ms, output_rate as f32);
            assert_eq!(mixer.duration_frames(), duration_frames);
            mixer.start_crossfade();

            assert!(
                outgoing_samples.len() >= duration_frames
                    && incoming_samples.len() >= duration_frames,
                "not enough resampled frames for {}->{} / {}->{} Hz at {} ms",
                outgoing_rate,
                output_rate,
                incoming_rate,
                output_rate,
                duration_ms
            );

            let mut transition_frames = 0usize;
            for (&(out_l, out_r), &(in_l, in_r)) in
                outgoing_samples.iter().zip(incoming_samples.iter())
            {
                let (l, r) = mixer.process(out_l, out_r, in_l, in_r);
                assert!(l.is_finite() && r.is_finite());
                transition_frames += 1;
                if !mixer.is_crossfading() {
                    break;
                }
            }

            assert_eq!(
                transition_frames, duration_frames,
                "crossfade boundary drifted for {}->{} / {}->{} Hz at {} ms",
                outgoing_rate, output_rate, incoming_rate, output_rate, duration_ms
            );
            let (l, r) = mixer.process(0.0, 0.0, 0.3, -0.3);
            assert!((l - 0.3).abs() < 1e-6 && (r + 0.3).abs() < 1e-6);
        }
    }
}

#[test]
fn test_crossfade_scratch_sized_for_worst_resampler_burst() {
    use engine::buffer::MAX_AUDIO_BLOCK_FRAMES;

    // The engine's crossfade scratch FIFOs are sized from the resampler's own
    // worst-case output-buffer bound; make sure a full realtime source block
    // never produces a single drain burst larger than that.
    let scratch = engine::engine::CROSSFADE_SCRATCH_FRAMES;
    assert!(
        scratch >= engine::dsp::resampler::MAX_OUTPUT_BUFFER_FRAMES,
        "crossfade scratch ({scratch}) must hold a full resampler output buffer"
    );

    for (source, output) in [
        (44_100usize, 96_000usize),
        (44_100, 192_000),
        (48_000, 192_000),
    ] {
        let mut r =
            AudioResamplerF32::new(ResamplerQuality::Balanced, source as f32, output as f32)
                .unwrap();

        // Feed a full realtime block in small slices, draining after each,
        // and record the largest single drain burst (the worst-case size the
        // crossfade scratch buffer must absorb).
        let mut max_burst = 0usize;
        let mut total_out = 0usize;
        let mut fed = 0usize;
        while fed < MAX_AUDIO_BLOCK_FRAMES {
            let batch = 256.min(MAX_AUDIO_BLOCK_FRAMES - fed);
            for i in 0..batch {
                let t = ((fed + i) as f32) * 0.01;
                r.feed(t.sin() * 0.5, -t.cos() * 0.5);
            }
            fed += batch;
            let mut burst = 0usize;
            while r.read().is_some() {
                burst += 1;
            }
            max_burst = max_burst.max(burst);
            total_out += burst;
        }
        r.flush();
        let mut burst = 0usize;
        while r.read().is_some() {
            burst += 1;
        }
        max_burst = max_burst.max(burst);
        total_out += burst;

        assert!(
            max_burst > 0,
            "{source}->{output}: resampler produced no output"
        );
        assert!(
            max_burst <= scratch,
            "{source}->{output}: single drain burst {max_burst} exceeds crossfade scratch {scratch}"
        );

        // Sanity: total output ≈ input × ratio (within filter group delay).
        let ratio = output as f64 / source as f64;
        let expected = MAX_AUDIO_BLOCK_FRAMES as f64 * ratio;
        assert!(
            (total_out as f64 - expected).abs() < expected * 0.05 + 4096.0,
            "{source}->{output}: produced {total_out} frames, expected ≈ {expected:.0}"
        );
    }
}

#[test]
fn test_crossfade_seek_and_early_completion() {
    let mut mixer = TrackMixer::new(48000.0);
    mixer.set_duration_ms(1000, 48000.0);
    mixer.start_crossfade();
    assert!(mixer.is_crossfading());

    // Advance 10,000 frames
    for _ in 0..10000 {
        mixer.process(0.5, 0.5, 0.5, 0.5);
    }
    assert!(mixer.is_crossfading());

    // Seeking or jumping resets mixer
    mixer.reset();
    assert!(!mixer.is_crossfading());

    // Start playing new track
    mixer.start_playing();
    let (l, r) = mixer.process(0.75, 0.75, 0.0, 0.0);
    assert_eq!(l, 0.75);
    assert_eq!(r, 0.75);
}
