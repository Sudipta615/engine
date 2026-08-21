//! Device-name matching shared by output backends and the profile system.
//!
//! Higher-precision matches must win so substring matching can never select
//! the wrong endpoint when several devices share similar names. `Ord` is
//! derived in confidence order: `Exact` < `ExactCaseInsensitive` <
//! `Substring`.

/// Confidence of a device-name match against a requested target name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceNameMatch {
    /// Identical strings (highest confidence).
    Exact,
    /// Case-insensitively identical.
    ExactCaseInsensitive,
    /// Case-insensitive substring (lowest confidence; inherently ambiguous).
    Substring,
}

/// Compare a requested target name against a discovered device name.
///
/// `target` is the requested pattern (e.g. a profile's `device_match` entry);
/// `name` is the device's reported name. Returns the best match confidence,
/// or `None` when the device does not match.
pub fn classify_device_name_match(target: &str, name: &str) -> Option<DeviceNameMatch> {
    if name == target {
        Some(DeviceNameMatch::Exact)
    } else if name.eq_ignore_ascii_case(target) {
        Some(DeviceNameMatch::ExactCaseInsensitive)
    } else if name.to_lowercase().contains(&target.to_lowercase()) {
        Some(DeviceNameMatch::Substring)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_beats_substring() {
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
    fn case_insensitive_matches() {
        assert_eq!(
            classify_device_name_match("usb dac", "USB DAC"),
            Some(DeviceNameMatch::ExactCaseInsensitive)
        );
        assert_eq!(
            classify_device_name_match("USB DAC", "usb dac"),
            Some(DeviceNameMatch::ExactCaseInsensitive)
        );
        assert_eq!(
            classify_device_name_match("usb dac", "USB DAC Pro"),
            Some(DeviceNameMatch::Substring)
        );
    }

    #[test]
    fn ordering_is_confidence_order() {
        assert!(DeviceNameMatch::Exact < DeviceNameMatch::ExactCaseInsensitive);
        assert!(DeviceNameMatch::ExactCaseInsensitive < DeviceNameMatch::Substring);
    }
}
