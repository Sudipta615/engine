//! Fidelity tests — sample rate policy logic
//!
//! Tests the `SampleRatePolicy` rate selection algorithm without requiring
//! actual CPAL audio devices.  Validates all policies and the base-rate-sync
//! clock family detection logic.

use engine::output::rate_policy::SampleRatePolicy;

/// Simulated device that supports the "48 kHz family" common on USB DACs.
fn usb_dac_48_family() -> Vec<u32> {
    vec![44100, 48000, 88200, 96000, 176400, 192000]
}

/// Simulated device that only supports 44100 and 48000 (integrated audio).
fn integrated_audio() -> Vec<u32> {
    vec![44100, 48000]
}

/// Simulated hi-res USB DAC (supports up to 384 kHz, both families).
fn hires_dac() -> Vec<u32> {
    vec![44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000]
}

// ─────────────────────────────────────────────────────────────────────────────
// FollowTrack
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn follow_track_exact_44100() {
    let supported = usb_dac_48_family();
    let rate = SampleRatePolicy::FollowTrack.select_rate(44100, &supported);
    assert_eq!(rate, 44100, "FollowTrack should select exact match 44100");
}

#[test]
fn follow_track_exact_96000() {
    let supported = usb_dac_48_family();
    let rate = SampleRatePolicy::FollowTrack.select_rate(96000, &supported);
    assert_eq!(rate, 96000, "FollowTrack should select exact match 96000");
}

#[test]
fn follow_track_fallback_nearest() {
    let supported = integrated_audio(); // only 44100, 48000
                                        // 88200 not supported → nearest is 96000... but not in list. Nearest = 48000.
    let rate = SampleRatePolicy::FollowTrack.select_rate(88200, &supported);
    assert!(
        supported.contains(&rate),
        "FollowTrack fallback should pick a supported rate, got {}",
        rate
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// BaseRateSync (Exact First & Highest)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn base_rate_sync_exact_first_44100_preserves_exact() {
    let supported = usb_dac_48_family(); // includes 44100, 88200, 176400
    let rate = SampleRatePolicy::BaseRateSyncExactFirst.select_rate(44100, &supported);
    assert_eq!(
        rate, 44100,
        "44.1kHz source → BaseRateSyncExactFirst should preserve exact 44.1 kHz"
    );
}

#[test]
fn base_rate_sync_highest_44100_source_to_176400() {
    let supported = usb_dac_48_family(); // includes 44100, 88200, 176400
    let rate = SampleRatePolicy::BaseRateSyncHighest.select_rate(44100, &supported);
    assert_eq!(
        rate, 176400,
        "44.1kHz source → BaseRateSyncHighest should pick 176.4 kHz"
    );
}

#[test]
fn base_rate_sync_highest_48000_source_to_192000() {
    let supported = usb_dac_48_family();
    let rate = SampleRatePolicy::BaseRateSyncHighest.select_rate(48000, &supported);
    assert_eq!(
        rate, 192000,
        "48kHz source → BaseRateSyncHighest should pick 192 kHz"
    );
}

#[test]
fn base_rate_sync_highest_88200_stays_in_44_family() {
    let supported = usb_dac_48_family(); // 88200, 176400
    let rate = SampleRatePolicy::BaseRateSyncHighest.select_rate(88200, &supported);
    assert_eq!(
        rate, 176400,
        "88.2kHz source (44.1 family) → highest in 44.1 family = 176.4 kHz"
    );
}

#[test]
fn base_rate_sync_highest_96000_stays_in_48_family() {
    let supported = usb_dac_48_family(); // 96000, 192000
    let rate = SampleRatePolicy::BaseRateSyncHighest.select_rate(96000, &supported);
    assert_eq!(
        rate, 192000,
        "96kHz source (48 family) → highest in 48 family = 192 kHz"
    );
}

#[test]
fn base_rate_sync_hires_dac_highest_44100() {
    let supported = hires_dac();
    let rate = SampleRatePolicy::BaseRateSyncHighest.select_rate(44100, &supported);
    assert_eq!(
        rate, 352800,
        "44.1kHz source on hi-res DAC with Highest → 352.8 kHz"
    );
}

#[test]
fn base_rate_sync_hires_dac_highest_48000() {
    let supported = hires_dac();
    let rate = SampleRatePolicy::BaseRateSyncHighest.select_rate(48000, &supported);
    assert_eq!(
        rate, 384000,
        "48kHz source on hi-res DAC with Highest → 384 kHz"
    );
}

#[test]
fn base_rate_sync_no_family_member_falls_back_to_follow_track() {
    // Device only supports 48000 — no 44.1 family members
    let supported = vec![48000u32, 96000, 192000];
    let rate = SampleRatePolicy::BaseRateSync.select_rate(44100, &supported);
    // Should fall back to FollowTrack → nearest to 44100 in the list
    assert!(
        supported.contains(&rate),
        "BaseRateSync fallback should pick a supported rate, got {}",
        rate
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// BestSupported
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn best_supported_picks_highest() {
    let supported = hires_dac();
    let rate = SampleRatePolicy::BestSupported.select_rate(44100, &supported);
    assert_eq!(rate, 384000);
}

// ─────────────────────────────────────────────────────────────────────────────
// Fixed
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn fixed_exact_match() {
    let supported = usb_dac_48_family();
    let rate = SampleRatePolicy::Fixed(96000).select_rate(44100, &supported);
    assert_eq!(rate, 96000);
}

#[test]
fn fixed_unsupported_falls_back_to_nearest() {
    let supported = integrated_audio();
    let rate = SampleRatePolicy::Fixed(96000).select_rate(44100, &supported);
    assert!(
        supported.contains(&rate),
        "Fixed fallback should pick nearest supported, got {}",
        rate
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Edge cases
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn empty_supported_list_returns_source_rate() {
    let supported: Vec<u32> = vec![];
    let rate = SampleRatePolicy::FollowTrack.select_rate(44100, &supported);
    assert_eq!(
        rate, 44100,
        "Empty supported list should return source rate"
    );
}

#[test]
fn single_supported_rate() {
    let supported = vec![48000u32];
    let rate = SampleRatePolicy::BaseRateSync.select_rate(44100, &supported);
    assert_eq!(rate, 48000);
}
