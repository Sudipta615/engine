//! Stream reconfiguration policy: format/channel/rate negotiation.

use cpal::SampleFormat;

/// Lightweight snapshot of one supported stream configuration, decoupled from
/// cpal's `SupportedStreamConfig` so the reconfiguration policy is
/// unit-testable without a live audio device.
pub(crate) struct SupportedConfig {
    pub(crate) format: SampleFormat,
    pub(crate) channels: u16,
    pub(crate) min_rate: u32,
    pub(crate) max_rate: u32,
}

impl SupportedConfig {
    pub(crate) fn from_cpal(c: &cpal::SupportedStreamConfigRange) -> Self {
        Self {
            format: c.sample_format(),
            channels: c.channels(),
            min_rate: c.min_sample_rate(),
            max_rate: c.max_sample_rate(),
        }
    }
}

/// The negotiated outcome of a reconfiguration: container, layout, and rate.
#[derive(Debug, PartialEq)]
pub(crate) struct ReconfigureChoice {
    pub(crate) format: SampleFormat,
    pub(crate) channels: u16,
    pub(crate) rate: u32,
}

/// Choose the format/channels/rate to reopen the device with.
///
/// This is the policy behind [`CpalOutput::reconfigure_sample_rate`]: it
/// **preserves** the preferred format and channel count whenever the device
/// still advertises them, moving only the sample rate. A rate change must not
/// silently flip the container or layout (guarded by the regression tests
/// below). Returns `None` only when the preferred format/channels are no
/// longer advertised (a device change), leaving the caller to re-negotiate.
pub(crate) fn select_reconfigure_config(
    supported: &[SupportedConfig],
    preferred_format: SampleFormat,
    channels: u16,
    target_rate: u32,
) -> Option<ReconfigureChoice> {
    // 1. Preferred format + channels at the exact target rate.
    if let Some(c) = supported.iter().find(|c| {
        c.format == preferred_format
            && c.channels == channels
            && c.min_rate <= target_rate
            && c.max_rate >= target_rate
    }) {
        return Some(ReconfigureChoice {
            format: c.format,
            channels: c.channels,
            rate: target_rate,
        });
    }

    // 2. Same format + channels, rate clamped into the supported range. The
    //    container/layout stay stable; only the rate is allowed to move.
    supported
        .iter()
        .find(|c| c.format == preferred_format && c.channels == channels)
        .map(|c| ReconfigureChoice {
            format: c.format,
            channels: c.channels,
            rate: target_rate.clamp(c.min_rate, c.max_rate),
        })
}

/// Re-negotiate after the previous format/channels vanished (device change):
/// fall back through the standard format priority, first at the exact target
/// rate, then clamped. Channels follow the first matching config.
pub(crate) fn renegotiate_reconfigure_config(
    supported: &[SupportedConfig],
    preferred_format: SampleFormat,
    target_rate: u32,
) -> Option<ReconfigureChoice> {
    let mut format_priority = vec![preferred_format];
    for fmt in [
        SampleFormat::F32,
        SampleFormat::I32,
        SampleFormat::I16,
        SampleFormat::U16,
        SampleFormat::F64,
    ] {
        if !format_priority.contains(&fmt) {
            format_priority.push(fmt);
        }
    }

    format_priority
        .iter()
        .find_map(|&fmt| {
            supported
                .iter()
                .find(|c| c.format == fmt && c.min_rate <= target_rate && c.max_rate >= target_rate)
                .map(|c| ReconfigureChoice {
                    format: c.format,
                    channels: c.channels,
                    rate: target_rate,
                })
        })
        .or_else(|| {
            format_priority.iter().find_map(|&fmt| {
                supported
                    .iter()
                    .find(|c| c.format == fmt)
                    .map(|c| ReconfigureChoice {
                        format: c.format,
                        channels: c.channels,
                        rate: target_rate.clamp(c.min_rate, c.max_rate),
                    })
            })
        })
}
