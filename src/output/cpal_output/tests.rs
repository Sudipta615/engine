use super::*;

#[test]
fn device_name_match_prefers_exact() {
    // A substring candidate must not shadow an exact match.
    assert_eq!(
        classify_device_name_match("USB DAC", "USB DAC"),
        Some(DeviceNameMatch::Exact)
    );
    assert_eq!(
        classify_device_name_match("USB DAC", "USB DAC Pro"),
        Some(DeviceNameMatch::Substring)
    );
    assert_eq!(classify_device_name_match("USB DAC", "Speakers"), None);
}

#[test]
fn device_name_match_case_insensitive_exact() {
    assert_eq!(
        classify_device_name_match("usb dac", "USB DAC"),
        Some(DeviceNameMatch::ExactCaseInsensitive)
    );
    assert_eq!(
        classify_device_name_match("USB DAC", "usb dac"),
        Some(DeviceNameMatch::ExactCaseInsensitive)
    );
}

#[test]
fn device_name_match_ordering() {
    // The derived Ord ranks Exact as highest confidence.
    assert!(DeviceNameMatch::Exact < DeviceNameMatch::ExactCaseInsensitive);
    assert!(DeviceNameMatch::ExactCaseInsensitive < DeviceNameMatch::Substring);
}

fn cfg(format: SampleFormat, channels: u16, min_rate: u32, max_rate: u32) -> SupportedConfig {
    SupportedConfig {
        format,
        channels,
        min_rate,
        max_rate,
    }
}

#[test]
fn reconfigure_preserves_format_and_channels_across_rate_change() {
    // Regression for the rate-only reconfiguration bug: the device supports
    // several formats and a 6-channel layout at the target rate, but a
    // rate change must keep the already-negotiated F32/2ch instead of
    // silently flipping to i16 or 6 channels.
    let supported = vec![
        cfg(SampleFormat::F32, 2, 44_100, 192_000),
        cfg(SampleFormat::I16, 2, 44_100, 192_000),
        cfg(SampleFormat::F32, 6, 44_100, 192_000),
    ];
    let choice =
        select_reconfigure_config(&supported, SampleFormat::F32, 2, 96_000).expect("choice");
    assert_eq!(choice.format, SampleFormat::F32);
    assert_eq!(choice.channels, 2);
    assert_eq!(choice.rate, 96_000);
}

#[test]
fn reconfigure_clamps_rate_instead_of_switching_format() {
    // F32/2ch only reaches 48 kHz while i16/2ch reaches 192 kHz. A request
    // for 96 kHz must clamp to 48 kHz and keep F32/2ch rather than
    // switching containers to satisfy the rate.
    let supported = vec![
        cfg(SampleFormat::F32, 2, 44_100, 48_000),
        cfg(SampleFormat::I16, 2, 44_100, 192_000),
    ];
    let choice =
        select_reconfigure_config(&supported, SampleFormat::F32, 2, 96_000).expect("choice");
    assert_eq!(choice.format, SampleFormat::F32);
    assert_eq!(choice.channels, 2);
    assert_eq!(choice.rate, 48_000);
}

#[test]
fn reconfigure_returns_none_when_format_and_channels_vanish() {
    // When the negotiated format/channels are gone entirely (device
    // change), the preserve path must yield `None` so the caller can
    // re-negotiate rather than silently misconfigure the stream.
    let supported = vec![cfg(SampleFormat::I16, 2, 44_100, 48_000)];
    assert!(select_reconfigure_config(&supported, SampleFormat::F32, 2, 48_000).is_none());
}

#[test]
fn reconfigure_sample_format_switches_container_when_requested() {
    // The DoP path requests I32 explicitly; that request must be honored
    // (the preserve rule only applies to rate-only reconfiguration).
    let supported = vec![
        cfg(SampleFormat::F32, 2, 44_100, 192_000),
        cfg(SampleFormat::I32, 2, 44_100, 192_000),
    ];
    let choice =
        select_reconfigure_config(&supported, SampleFormat::I32, 2, 176_400).expect("choice");
    assert_eq!(choice.format, SampleFormat::I32);
    assert_eq!(choice.channels, 2);
    assert_eq!(choice.rate, 176_400);
}
