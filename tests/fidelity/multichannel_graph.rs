//! Reference checks for the layout-aware multichannel graph.
//!
//! These tests deliberately use analytical signals rather than broad smoke
//! assertions: the mains high-pass must reject DC, a channel EQ must affect
//! only its selected channel, and named mix templates must produce stable
//! role-aware matrices.

use config::{
    BassManagementConfig, ChannelEqConfig, ChannelEqEntry, ChannelMixTemplate, EngineConfig,
    EqBandConfig, FilterType, LfeConfig,
};
use engine::decode::{
    mix_interleaved_to_stereo_with_template, mix_interleaved_with_template, ChannelLayout,
};
use engine::dsp::ChannelTrimmer;

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

#[test]
fn mains_highpass_rejects_dc_but_lfe_path_is_separate() {
    let mut trimmer = ChannelTrimmer::new(48_000.0);
    trimmer.set_config(&Default::default(), 48_000.0);
    trimmer.set_lfe(&LfeConfig {
        enabled: true,
        gain_db: 0.0,
        crossover_hz: None,
    });
    trimmer.set_lfe_channels(vec![2]);
    trimmer.set_bass_management(
        &BassManagementConfig {
            enabled: true,
            mains_highpass_enabled: true,
            crossover_hz: 100.0,
            q: std::f32::consts::FRAC_1_SQRT_2,
        },
        48_000.0,
    );

    let mut planes = vec![vec![1.0; 4096], vec![1.0; 4096], vec![1.0; 4096]];
    trimmer.process_planes(&mut planes, 3, 4096);

    assert!(rms(&planes[0][1024..]) < 1.0e-3, "main DC must be removed");
    assert!(rms(&planes[1][1024..]) < 1.0e-3, "main DC must be removed");
    assert!(
        (planes[2][4095] - 1.0).abs() < 1.0e-5,
        "LFE must not receive the mains HP"
    );
}

#[test]
fn per_channel_eq_isolated_and_wired_through_pipeline() {
    let sample_rate = 48_000.0f32;
    let n = 4096usize;
    let frequency = 1_000.0f32;
    let input: Vec<f32> = (0..n)
        .map(|i| (2.0 * std::f32::consts::PI * frequency * i as f32 / sample_rate).sin() * 0.25)
        .collect();

    let mut config = EngineConfig::default();
    config.limiter.enabled = false;
    config.channel_eq = ChannelEqConfig {
        enabled: true,
        entries: vec![ChannelEqEntry {
            channel: 2,
            bands: vec![EqBandConfig {
                enabled: true,
                filter_type: FilterType::Peaking,
                frequency,
                gain_db: 6.0,
                q: 1.0,
            }],
        }],
    };

    let mut pipeline = engine::dsp::DspPipeline::from_config(&config, sample_rate);
    pipeline.volume.snap();
    let mut interleaved = vec![0.0f32; n * 4];
    for frame in 0..n {
        for channel in 0..4 {
            interleaved[frame * 4 + channel] = input[frame];
        }
    }
    pipeline.process_block_multichannel(&mut interleaved, 4);

    let untouched_rms = rms(&interleaved[4 * 1024..]
        .iter()
        .step_by(4)
        .copied()
        .collect::<Vec<_>>());
    let eq_rms = rms(&interleaved[4 * 1024 + 2..]
        .iter()
        .step_by(4)
        .copied()
        .collect::<Vec<_>>());
    assert!((untouched_rms - rms(&input[1024..])).abs() < 1.0e-4);
    assert!(
        eq_rms > untouched_rms * 1.5,
        "selected channel should receive EQ boost"
    );

    for channel in [0usize, 1, 3] {
        for frame in 1024..n {
            assert!((interleaved[frame * 4 + channel] - input[frame]).abs() < 1.0e-5);
        }
    }
}

#[test]
fn stereo_to_five_one_template_is_role_aware() {
    let source = [1.0f32, 2.0];
    let mut output = [0.0f32; 6];
    let frames = mix_interleaved_with_template(
        &source,
        &ChannelLayout::Stereo,
        2,
        &ChannelLayout::FivePointOne,
        &ChannelMixTemplate::StereoToFiveOne,
        &mut output,
        1,
    );
    assert_eq!(frames, 1);
    assert!((output[0] - 1.0).abs() < 1.0e-6); // FL
    assert!((output[1] - 2.0).abs() < 1.0e-6); // FR
    assert!((output[2] - 3.0 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1.0e-6); // C
    assert_eq!(output[3], 0.0); // LFE is not fabricated
    assert!((output[4] - 0.5).abs() < 1.0e-6); // SL
    assert!((output[5] - 1.0).abs() < 1.0e-6); // SR
}

#[test]
fn five_one_downmix_template_excludes_lfe_and_is_deterministic() {
    let source = [1.0f32, 2.0, 3.0, 100.0, 4.0, 5.0];
    let mut left = [0.0f32; 1];
    let mut right = [0.0f32; 1];
    let frames = mix_interleaved_to_stereo_with_template(
        &source,
        &ChannelLayout::FivePointOne,
        6,
        &ChannelMixTemplate::FiveOneToStereo,
        &mut left,
        &mut right,
        1,
    );
    assert_eq!(frames, 1);
    let expected_left =
        1.0 + 3.0 * std::f32::consts::FRAC_1_SQRT_2 + 4.0 * std::f32::consts::FRAC_1_SQRT_2;
    let expected_right =
        2.0 + 3.0 * std::f32::consts::FRAC_1_SQRT_2 + 5.0 * std::f32::consts::FRAC_1_SQRT_2;
    assert!((left[0] - expected_left).abs() < 1.0e-5);
    assert!((right[0] - expected_right).abs() < 1.0e-5);
}

#[test]
fn seven_one_four_layout_and_custom_matrix_are_supported() {
    assert_eq!(ChannelLayout::SevenPointOneFour.channel_count(), 12);
    assert_eq!(ChannelLayout::SevenPointOneFour.channel_ids().len(), 12);

    let source = [1.0f32, 2.0];
    let mut output = [0.0f32; 12];
    let frames = mix_interleaved_with_template(
        &source,
        &ChannelLayout::Stereo,
        2,
        &ChannelLayout::SevenPointOneFour,
        &ChannelMixTemplate::StereoToSevenPointOneFour,
        &mut output,
        1,
    );
    assert_eq!(frames, 1);
    assert!(
        output[8] > 0.0 && output[9] > 0.0,
        "front heights receive explicit fill"
    );
    assert!(
        output[10] > 0.0 && output[11] > 0.0,
        "rear heights receive explicit fill"
    );

    let mut custom_output = [0.0f32; 3];
    let frames = mix_interleaved_with_template(
        &source,
        &ChannelLayout::Stereo,
        2,
        &ChannelLayout::Custom(vec![
            engine::decode::ChannelId::FrontLeft,
            engine::decode::ChannelId::FrontRight,
            engine::decode::ChannelId::Center,
        ]),
        &ChannelMixTemplate::Custom(vec![vec![1.0, 0.0, 0.25], vec![0.0, 1.0, 0.5]]),
        &mut custom_output,
        1,
    );
    assert_eq!(frames, 1);
    assert_eq!(custom_output, [1.0, 2.0, 1.25]);
}
