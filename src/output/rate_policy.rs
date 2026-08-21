//! Sample rate selection policy — the desktop equivalent of Poweramp's
//! "Base-Rate Sync Scaling" and "Follow Track" output-rate policies.
//!
//! ## Policies
//!
//! | Policy                   | Behaviour                                                   |
//! |--------------------------|-------------------------------------------------------------|
//! | `DeviceDefault`          | Use whatever rate the OS/driver reports as default          |
//! | `FollowTrack`            | Match the source sample rate exactly if the device supports it |
//! | `Fixed(rate)`            | Always use this specific rate                               |
//! | `BestSupported`          | Use the highest rate the device supports                    |
//! | `BaseRateSyncExactFirst` | Exact rate first; same-family multiple if exact unavailable |
//! | `BaseRateSyncHighest`    | Always use the highest integer-multiple of the source family|
//! | `BaseRateSync`           | Alias for `BaseRateSyncExactFirst`                          |

pub use config::{apply_fallback, clock_family, RateFallbackPolicy, SampleRatePolicy};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_track_exact() {
        let supported = vec![44100u32, 48000, 88200, 96000, 192000];
        assert_eq!(
            SampleRatePolicy::FollowTrack.select_rate(96000, &supported),
            96000
        );
    }

    #[test]
    fn follow_track_fallback_nearest() {
        let supported = vec![48000u32, 96000, 192000];
        // 44100 not in list → nearest is 48000
        let rate = SampleRatePolicy::FollowTrack.select_rate(44100, &supported);
        assert_eq!(rate, 48000);
    }

    #[test]
    fn follow_track_fallback_prefer_higher() {
        let supported = vec![32000u32, 48000, 96000];
        let rate = SampleRatePolicy::FollowTrack.select_rate_with_fallback(
            44100,
            &supported,
            RateFallbackPolicy::PreferHigher,
        );
        assert_eq!(rate, 48000);
    }

    #[test]
    fn follow_track_fallback_prefer_lower() {
        let supported = vec![32000u32, 48000, 96000];
        let rate = SampleRatePolicy::FollowTrack.select_rate_with_fallback(
            44100,
            &supported,
            RateFallbackPolicy::PreferLower,
        );
        assert_eq!(rate, 32000);
    }

    #[test]
    fn base_rate_sync_exact_first_preserves_exact() {
        let supported = vec![44100u32, 48000, 88200, 96000, 176400, 192000];
        let rate = SampleRatePolicy::BaseRateSync.select_rate(44100, &supported);
        assert_eq!(rate, 44100);
    }

    #[test]
    fn base_rate_sync_highest_44_family() {
        let supported = vec![44100u32, 48000, 88200, 96000, 176400, 192000];
        let rate = SampleRatePolicy::BaseRateSyncHighest.select_rate(44100, &supported);
        assert_eq!(rate, 176_400);
    }

    #[test]
    fn base_rate_sync_highest_48_family() {
        let supported = vec![44100u32, 48000, 88200, 96000, 192000];
        let rate = SampleRatePolicy::BaseRateSyncHighest.select_rate(48000, &supported);
        assert_eq!(rate, 192_000);
    }

    #[test]
    fn base_rate_sync_highest_96k_source() {
        let supported = vec![48000u32, 96000, 192000, 384000];
        let rate = SampleRatePolicy::BaseRateSyncHighest.select_rate(96000, &supported);
        assert_eq!(rate, 384_000);
    }

    #[test]
    fn best_supported() {
        let supported = vec![44100u32, 48000, 96000, 192000, 384000];
        assert_eq!(
            SampleRatePolicy::BestSupported.select_rate(44100, &supported),
            384_000
        );
    }

    #[test]
    fn fixed() {
        let supported = vec![44100u32, 48000, 96000];
        assert_eq!(
            SampleRatePolicy::Fixed(48000).select_rate(44100, &supported),
            48000
        );
    }
}
