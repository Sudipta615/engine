//! Output status reporting — actual vs. requested backend and sample rate.
//!
//! `OutputInfo` surfaces what the audio system actually negotiated with the
//! hardware, including whether an exclusive-mode fallback occurred.  The UI
//! can use this to display a "Bit-Perfect" badge or a "Shared Mode" warning.

use config::AudioBackend;
use serde::{Deserialize, Serialize};

use crate::dsp::pipeline::OutputSampleFormat;
use crate::output::capabilities::{OutputAccessMode, OutputAccessState};

/// Describes the audio output as it was actually opened, including whether
/// the requested configuration was honoured or a fallback occurred.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutputInfo {
    /// The backend the caller requested.
    pub requested_backend: Option<AudioBackend>,
    /// The backend that was actually used.
    pub actual_backend: Option<AudioBackend>,

    /// The sample rate the caller requested (or 0 if default was requested).
    pub requested_rate: u32,
    /// The sample rate the device actually opened at.
    pub actual_rate: u32,

    /// The number of output channels.
    pub channels: u16,

    /// Negotiated callback buffer size in frames (the OS/device buffer the
    /// audio callback fills each period). 0 when unknown. This is the
    /// output-device buffering term for the graph latency model.
    ///
    /// When [`Self::buffer_size_estimated`] is `true`, this value is a
    /// backend target rather than a size actually reported by the driver
    /// (e.g. cpal reports `SupportedBufferSize::Unknown` for some hosts),
    /// so the derived latency is only an estimate.
    #[serde(default)]
    pub buffer_size_frames: u32,

    /// Whether `buffer_size_frames` is an estimate (the driver did not report
    /// a buffer size) rather than a negotiated value. `false` for backends
    /// that query the real device buffer (e.g. the native WASAPI backend
    /// reads `IAudioClient::GetBufferSize`).
    #[serde(default)]
    pub buffer_size_estimated: bool,

    /// Concrete sample container negotiated with the device.
    pub sample_format: OutputSampleFormat,

    /// Whether TPDF dither is currently applied at the integer-quantization
    /// boundary of this stream. No-op when `sample_format` is f32/f64, so
    /// UI should pair this with `sample_format.is_float()`. Follows the
    /// engine's `config.dither_enabled` preference and is forced off during
    /// DSD DoP.
    #[serde(default)]
    pub dither_enabled: bool,

    /// Operating access mode (Shared, Exclusive, DirectHw, BitstreamPassthrough).
    pub access_mode: OutputAccessMode,

    /// Comprehensive output access state (requested vs actual vs verified).
    pub access_state: OutputAccessState,

    /// Whether a fallback occurred (e.g. exclusive mode → shared mode).
    pub is_fallback: bool,

    /// Human-readable reason for the fallback, if any.
    pub fallback_reason: Option<String>,

    /// Whether the output is believed to be bit-perfect (no OS mixing).
    /// True only when exclusive mode is confirmed and no processing is
    /// applied between the engine and the hardware.
    pub is_exclusive: bool,

    /// The CPAL/OS device name.
    pub device_name: String,
}

impl OutputInfo {
    /// Create an `OutputInfo` reflecting a cleanly-opened exclusive stream.
    pub fn exclusive(
        device_name: String,
        rate: u32,
        channels: u16,
        backend: Option<AudioBackend>,
    ) -> Self {
        let dev_lower = device_name.to_lowercase();
        let access_mode = if dev_lower.starts_with("hw:") {
            OutputAccessMode::DirectHw
        } else if dev_lower.starts_with("plughw:") {
            // ALSA's plug plugin may convert rate/format, so it is not a
            // verified bit-perfect transport even though it targets hardware.
            OutputAccessMode::Shared
        } else {
            OutputAccessMode::Exclusive
        };

        let verified = !dev_lower.starts_with("plughw:");
        let access_state = OutputAccessState {
            requested: OutputAccessMode::Exclusive,
            actual: access_mode,
            verified,
        };

        Self {
            requested_backend: backend.clone(),
            actual_backend: backend,
            requested_rate: rate,
            actual_rate: rate,
            channels,
            buffer_size_frames: 0,
            buffer_size_estimated: true,
            sample_format: OutputSampleFormat::Unknown,
            dither_enabled: false,
            access_mode,
            access_state,
            is_fallback: false,
            fallback_reason: None,
            is_exclusive: verified,
            device_name,
        }
    }

    /// Create an `OutputInfo` reflecting a shared / fallback stream.
    pub fn shared(
        device_name: String,
        requested_rate: u32,
        actual_rate: u32,
        channels: u16,
        requested_backend: Option<AudioBackend>,
        actual_backend: Option<AudioBackend>,
        reason: impl Into<String>,
    ) -> Self {
        let is_req_exclusive = requested_backend
            .as_ref()
            .map(|b| {
                matches!(
                    b,
                    AudioBackend::ExclusiveAlsa
                        | AudioBackend::ExclusiveWasapi
                        | AudioBackend::ExclusiveAsio
                        | AudioBackend::ExclusiveCoreAudioHog
                )
            })
            .unwrap_or(false);
        let access_state = OutputAccessState {
            requested: if is_req_exclusive {
                OutputAccessMode::Exclusive
            } else {
                OutputAccessMode::Shared
            },
            actual: OutputAccessMode::Shared,
            verified: false,
        };

        Self {
            requested_backend,
            actual_backend,
            requested_rate,
            actual_rate,
            channels,
            buffer_size_frames: 0,
            buffer_size_estimated: true,
            sample_format: OutputSampleFormat::Unknown,
            dither_enabled: false,
            access_mode: OutputAccessMode::Shared,
            access_state,
            is_fallback: true,
            fallback_reason: Some(reason.into()),
            is_exclusive: false,
            device_name,
        }
    }

    /// Short container/rate/dither summary for UI displays — e.g.
    /// `"i16 @ 48 kHz (TPDF dither)"` or `"f32 @ 44.1 kHz"`. The dither
    /// suffix is only shown when it is meaningful (enabled AND the container
    /// is integer); f32/f64 streams quantize nowhere.
    pub fn format_summary(&self) -> String {
        let rate_str = if self.actual_rate > 0 && self.actual_rate % 1000 == 0 {
            format!("{} kHz", self.actual_rate / 1000)
        } else if self.actual_rate > 0 {
            format!("{:.1} kHz", self.actual_rate as f32 / 1000.0)
        } else {
            "? kHz".to_string()
        };
        let dither = if self.dither_enabled && !self.sample_format.is_float() {
            " (TPDF dither)"
        } else {
            ""
        };
        format!("{} @ {}{}", self.sample_format.label(), rate_str, dither)
    }

    /// One-line summary suitable for diagnostic displays and log lines.
    pub fn summary(&self) -> String {
        let mode = if self.is_exclusive {
            "exclusive"
        } else {
            "shared"
        };
        let fallback = if self.is_fallback {
            format!(
                " [fallback: {}]",
                self.fallback_reason.as_deref().unwrap_or("unknown reason")
            )
        } else {
            String::new()
        };
        format!(
            "{} @ {} Hz {} ch ({mode}){fallback}",
            self.device_name, self.actual_rate, self.channels
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(rate: u32, sample_format: OutputSampleFormat, dither: bool) -> OutputInfo {
        OutputInfo {
            requested_backend: None,
            actual_backend: None,
            requested_rate: rate,
            actual_rate: rate,
            channels: 2,
            buffer_size_frames: 0,
            buffer_size_estimated: true,
            sample_format,
            dither_enabled: dither,
            access_mode: OutputAccessMode::Shared,
            access_state: OutputAccessState::default(),
            is_fallback: false,
            fallback_reason: None,
            is_exclusive: false,
            device_name: "test".into(),
        }
    }

    #[test]
    fn plughw_conversion_is_not_bit_perfect() {
        let info = OutputInfo::exclusive(
            "plughw:2,0".to_string(),
            96_000,
            2,
            Some(AudioBackend::ExclusiveAlsa),
        );
        assert_eq!(info.access_mode, OutputAccessMode::Shared);
        assert!(!info.access_state.is_bit_perfect());
        assert!(!info.is_exclusive);
    }

    #[test]
    fn format_summary_shows_container_rate_and_dither() {
        // i16 @ 48 kHz with dither → the full string.
        assert_eq!(
            info(48_000, OutputSampleFormat::I16, true).format_summary(),
            "i16 @ 48 kHz (TPDF dither)"
        );
        // Dither disabled → no suffix.
        assert_eq!(
            info(48_000, OutputSampleFormat::I16, false).format_summary(),
            "i16 @ 48 kHz"
        );
        // Non-integer kHz gets one decimal.
        assert_eq!(
            info(44_100, OutputSampleFormat::F32, true).format_summary(),
            "f32 @ 44.1 kHz"
        );
        // 24-bit-in-32 surfaces its exact label.
        assert_eq!(
            info(192_000, OutputSampleFormat::I24Le, true).format_summary(),
            "i24le @ 192 kHz (TPDF dither)"
        );
        // f32 never claims dither even when the flag is set (no-op).
        assert_eq!(
            info(96_000, OutputSampleFormat::F32, true).format_summary(),
            "f32 @ 96 kHz"
        );
    }
}
