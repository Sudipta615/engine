//! Output capability querying — enumerates the sample rates, formats and
//! channel counts that the selected audio device actually supports.
//!
//! Use [`OutputCapabilities::query`] to build a capability snapshot, then pass
//! it to [`SampleRatePolicy::select_rate`] to choose the best output rate for a
//! given track and user preference.

use cpal::{
    traits::{DeviceTrait, HostTrait},
    Device, SampleFormat,
};

use crate::output::rate_policy::SampleRatePolicy;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutputValidationError {
    #[error("device does not support {rate} Hz")]
    UnsupportedRate { rate: u32 },
    #[error("device does not support sample format {format:?}")]
    UnsupportedFormat { format: SampleFormat },
    #[error("device does not support {channels} output channels")]
    UnsupportedChannels { channels: u16 },
    #[error("device access is not verified as direct/exclusive")]
    DirectAccessUnverified,
}

// The `OutputAccessMode` / `OutputAccessState` types live in the `config`
// crate (compiled in all builds) so the DSP pipeline's bit-perfect report
// can carry the real access vocabulary. Re-exported here for API compatibility.
pub use config::{OutputAccessMode, OutputAccessState};

/// Snapshot of what a CPAL output device can do.
#[derive(Debug, Clone)]
pub struct OutputCapabilities {
    /// All standard discrete sample rates the device will accept, sorted ascending.
    pub sample_rates: Vec<u32>,
    /// Continuous hardware sample rate ranges (min..=max) reported by the driver.
    pub hardware_ranges: Vec<(u32, u32)>,
    /// All sample formats the device supports.
    pub formats: Vec<SampleFormat>,
    /// All channel counts the device supports.
    pub channels: Vec<u16>,
    /// Human-readable device name.
    pub device_name: String,
    /// Operating access mode (Shared, Exclusive, DirectHw, BitstreamPassthrough).
    pub access_mode: OutputAccessMode,
    /// Full access state breakdown (requested vs actual vs verified).
    pub access_state: OutputAccessState,
    /// Heuristic indicator whether the device represents a direct hardware endpoint
    /// (e.g. direct ALSA hw:X, WASAPI exclusive, CoreAudio direct) rather than a software mixer.
    pub likely_direct_access: bool,
    /// Legacy compatibility alias for `likely_direct_access`.
    pub supports_exclusive: bool,
}

impl OutputCapabilities {
    /// Query capabilities from a CPAL device.
    pub fn query(device: &Device) -> Self {
        let device_name = device
            .description()
            .map(|desc| desc.name().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let supported = device.supported_output_configs().ok();
        let mut sample_rates: Vec<u32> = Vec::new();
        let mut hardware_ranges: Vec<(u32, u32)> = Vec::new();
        let mut formats: Vec<SampleFormat> = Vec::new();
        let mut channels: Vec<u16> = Vec::new();

        if let Some(supported) = supported {
            for cfg in supported {
                let min = cfg.min_sample_rate();
                let max = cfg.max_sample_rate();
                if !hardware_ranges.contains(&(min, max)) {
                    hardware_ranges.push((min, max));
                }

                // Collect all discrete rates in standard audio range
                for &rate in STANDARD_RATES.iter() {
                    if rate >= min && rate <= max && !sample_rates.contains(&rate) {
                        sample_rates.push(rate);
                    }
                }

                let fmt = cfg.sample_format();
                if !formats.contains(&fmt) {
                    formats.push(fmt);
                }
                let ch = cfg.channels();
                if !channels.contains(&ch) {
                    channels.push(ch);
                }
            }
        }

        sample_rates.sort_unstable();
        hardware_ranges.sort_unstable();

        let dev_lower = device_name.to_lowercase();
        let is_software_shared = dev_lower.contains("pulse")
            || dev_lower.contains("pipewire")
            || dev_lower.contains("dmix")
            || dev_lower == "default"
            || dev_lower == "sysdefault";

        let likely_direct_access = !sample_rates.is_empty() && !is_software_shared;
        let supports_exclusive = likely_direct_access;

        let access_mode = if dev_lower.starts_with("hw:") {
            OutputAccessMode::DirectHw
        } else if dev_lower.starts_with("plughw:") {
            // The ALSA plug plugin can convert the stream, so keep this
            // explicitly non-direct until a live backend verifies otherwise.
            OutputAccessMode::Shared
        } else if likely_direct_access {
            OutputAccessMode::Exclusive
        } else {
            OutputAccessMode::Shared
        };

        // A static device query cannot verify exclusivity: exclusive/direct
        // access is a property of the *opened stream* (backend, device, and
        // fallback state), not of the device name or its supported rates.
        // Marking `verified = access_mode.is_direct()` here would claim
        // bit-perfect access without ever negotiating a stream. The access
        // state is therefore left unverified (`Shared` / not verified) and
        // only a live `CpalOutput::capabilities()` (which overlays the
        // actually-opened stream's state) can report verified exclusive or
        // direct hardware access. `access_mode` / `likely_direct_access`
        // remain explicitly-heuristic estimates for UI hints.
        let access_state = OutputAccessState {
            requested: OutputAccessMode::Shared,
            actual: OutputAccessMode::Shared,
            verified: false,
        };

        Self {
            sample_rates,
            hardware_ranges,
            formats,
            channels,
            device_name,
            access_mode,
            access_state,
            likely_direct_access,
            supports_exclusive,
        }
    }

    /// Query capabilities from the system default output device.
    pub fn query_default() -> Option<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        Some(Self::query(&device))
    }

    /// Whether the device supports the given sample rate (either discrete or within hardware range).
    pub fn supports_rate(&self, rate: u32) -> bool {
        self.sample_rates.contains(&rate)
            || self
                .hardware_ranges
                .iter()
                .any(|&(min, max)| rate >= min && rate <= max)
    }

    /// Select the best output sample rate given a source rate and policy.
    pub fn best_rate_for(&self, source_rate: u32, policy: &SampleRatePolicy) -> u32 {
        policy.select_rate_with_ranges(
            source_rate,
            &self.sample_rates,
            &self.hardware_ranges,
            config::RateFallbackPolicy::Nearest,
        )
    }

    /// Whether the negotiated device advertises any output layout wider than
    /// stereo. Codec decoding support is intentionally separate: callers
    /// must combine this result with `CodecCapability::multichannel_decode`.
    pub fn supports_multichannel_output(&self) -> bool {
        self.channels.iter().any(|&channels| channels > 2)
    }

    /// Whether the device supports f32 output.
    pub fn supports_f32(&self) -> bool {
        self.formats.contains(&SampleFormat::F32)
    }

    /// Validate a concrete stream request against the device snapshot before
    /// opening a stream. This keeps device-specific failures actionable and
    /// avoids treating a heuristic capability claim as a negotiated result.
    pub fn validate_stream(
        &self,
        rate: u32,
        format: SampleFormat,
        channels: u16,
        require_direct: bool,
    ) -> Result<(), OutputValidationError> {
        if !self.supports_rate(rate) {
            return Err(OutputValidationError::UnsupportedRate { rate });
        }
        if !self.formats.contains(&format) {
            return Err(OutputValidationError::UnsupportedFormat { format });
        }
        if !self.channels.contains(&channels) {
            return Err(OutputValidationError::UnsupportedChannels { channels });
        }
        if require_direct && !self.access_state.is_bit_perfect() {
            return Err(OutputValidationError::DirectAccessUnverified);
        }
        Ok(())
    }

    /// Default sample rate from CPAL (fast path — avoids full capability query).
    pub fn default_rate_from_device(device: &Device) -> Option<u32> {
        device.default_output_config().ok().map(|c| c.sample_rate())
    }
}

/// Standard sample rates to probe when querying capabilities. Shared with the
/// native WASAPI backend, which probes each rate with a real exclusive-mode
/// `IsFormatSupported` call.
pub(crate) const STANDARD_RATES: &[u32] = &[
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
    352_800, 384_000, 705_600, 768_000,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::rate_policy::SampleRatePolicy;

    #[test]
    fn best_rate_follow_track_exact_match() {
        let caps = OutputCapabilities {
            sample_rates: vec![44100, 48000, 88200, 96000, 192000],
            hardware_ranges: vec![(44100, 192000)],
            formats: vec![SampleFormat::F32],
            channels: vec![2],
            device_name: "test".into(),
            access_mode: OutputAccessMode::Shared,
            access_state: OutputAccessState {
                requested: OutputAccessMode::Exclusive,
                actual: OutputAccessMode::DirectHw,
                verified: true,
            },
            likely_direct_access: true,
            supports_exclusive: true,
        };
        let rate = caps.best_rate_for(96000, &SampleRatePolicy::FollowTrack);
        assert_eq!(rate, 96000);
        assert!(caps.supports_rate(88200));
        assert!(caps
            .validate_stream(96000, SampleFormat::F32, 2, true)
            .is_ok());
        assert!(matches!(
            caps.validate_stream(96000, SampleFormat::I16, 2, true),
            Err(OutputValidationError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn best_rate_follow_track_honors_continuous_hardware_range() {
        let caps = OutputCapabilities {
            sample_rates: vec![44_100, 48_000, 96_000, 192_000],
            hardware_ranges: vec![(44_100, 192_000)],
            formats: vec![SampleFormat::F32],
            channels: vec![2, 6],
            device_name: "continuous test device".into(),
            access_mode: OutputAccessMode::Shared,
            access_state: OutputAccessState::default(),
            likely_direct_access: false,
            supports_exclusive: false,
        };
        assert_eq!(
            caps.best_rate_for(50_000, &SampleRatePolicy::FollowTrack),
            50_000
        );
        assert_eq!(
            caps.best_rate_for(50_000, &SampleRatePolicy::Fixed(50_000)),
            50_000
        );
        assert!(caps.supports_multichannel_output());
    }

    #[test]
    fn best_rate_follow_track_no_exact_falls_to_default() {
        let caps = OutputCapabilities {
            sample_rates: vec![48000, 96000, 192000],
            hardware_ranges: vec![(48000, 192000)],
            formats: vec![SampleFormat::F32],
            channels: vec![2],
            device_name: "test".into(),
            access_mode: OutputAccessMode::Shared,
            access_state: OutputAccessState::default(),
            likely_direct_access: false,
            supports_exclusive: false,
        };
        let rate = caps.best_rate_for(44100, &SampleRatePolicy::FollowTrack);
        assert!(
            caps.sample_rates.contains(&rate),
            "rate {} not in caps",
            rate
        );
    }
}
