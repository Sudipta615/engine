//! Device and output stream recovery workflow state machine (§10.1, Item 20).
//!
//! Enforces the authoritative 6-phase device reconnection pipeline:
//!
//! ```text
//! device disappears
//!        ↓
//! preserve engine state
//!        ↓
//!   reopen device
//!        ↓
//! reconfigure format
//!        ↓
//!   restore clock
//!        ↓
//! restore output profile
//!        ↓
//!  resume playback
//! ```
//!
//! # Playhead Protection
//! The workflow ensures that during USB replugs or hardware device reconnects,
//! the exact sample-accurate decoder playhead position is preserved and clock
//! time is rescaled to prevent playhead skips or playback restarts.

use serde::{Deserialize, Serialize};

/// Current phase in the 6-phase device recovery state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    #[default]
    Idle,
    /// 1. Hardware device was unplugged or stopped responding.
    DeviceDisappeared,
    /// 2. Audio playback state, playhead position, and volume are snapshotted.
    PreserveEngineState,
    /// 3. Device detection, debounce settling, and backend handle re-opening.
    ReopenDevice,
    /// 4. Hardware format, sample rate, and DSD/PCM wire format re-negotiated.
    ReconfigureFormat,
    /// 5. Output clock sample rate updated, monotonic timeline rescaled.
    RestoreClock,
    /// 6. Speaker calibrations, delays, and channel trims re-applied.
    RestoreOutputProfile,
    /// 7. Playback stream restarted and audio resumes seamlessly.
    ResumePlayback,
    /// Recovery permanently aborted after exceeding retry budget.
    Failed,
}

impl RecoveryPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            RecoveryPhase::Idle => "idle",
            RecoveryPhase::DeviceDisappeared => "device_disappeared",
            RecoveryPhase::PreserveEngineState => "preserve_engine_state",
            RecoveryPhase::ReopenDevice => "reopen_device",
            RecoveryPhase::ReconfigureFormat => "reconfigure_format",
            RecoveryPhase::RestoreClock => "restore_clock",
            RecoveryPhase::RestoreOutputProfile => "restore_output_profile",
            RecoveryPhase::ResumePlayback => "resume_playback",
            RecoveryPhase::Failed => "failed",
        }
    }
}

impl std::fmt::Display for RecoveryPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Snapshotted playback state preserved across a device disappearance.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PreservedPlaybackSnapshot {
    /// Exact source playhead position in frames.
    pub source_frames: u64,
    /// Source sample rate in Hz.
    pub source_sample_rate: u32,
    /// Output sample rate in Hz before disconnect.
    pub old_output_sample_rate: u32,
    /// Current volume level.
    pub volume: f32,
    /// Active output profile identifier.
    pub output_profile_id: Option<String>,
    /// Whether playback was active before disconnect.
    pub was_playing: bool,
}

/// Controller coordinating the 6-phase device recovery state machine (§10.1).
#[derive(Debug, Clone, Default)]
pub struct OutputRecoveryController {
    phase: RecoveryPhase,
    snapshot: Option<PreservedPlaybackSnapshot>,
    attempts: u32,
    max_attempts: u32,
}

impl OutputRecoveryController {
    pub fn new(max_attempts: u32) -> Self {
        Self {
            phase: RecoveryPhase::Idle,
            snapshot: None,
            attempts: 0,
            max_attempts: max_attempts.max(1),
        }
    }

    /// Current recovery phase.
    pub fn phase(&self) -> RecoveryPhase {
        self.phase
    }

    /// Total recovery attempts in the current burst.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Whether the retry budget has been exhausted.
    pub fn is_exhausted(&self) -> bool {
        self.attempts >= self.max_attempts
    }

    /// Transition to `DeviceDisappeared` and begin a recovery cycle.
    pub fn on_device_disappeared(&mut self) {
        self.phase = RecoveryPhase::DeviceDisappeared;
        self.attempts = self.attempts.saturating_add(1);
    }

    /// Capture and preserve engine state before tearing down the old stream.
    pub fn preserve_state(&mut self, snapshot: PreservedPlaybackSnapshot) {
        self.snapshot = Some(snapshot);
        self.phase = RecoveryPhase::PreserveEngineState;
    }

    /// Mark device handle re-opened.
    pub fn on_device_reopened(&mut self) {
        self.phase = RecoveryPhase::ReopenDevice;
    }

    /// Mark format re-negotiated.
    pub fn on_format_reconfigured(&mut self) {
        self.phase = RecoveryPhase::ReconfigureFormat;
    }

    /// Mark clock rate updated and frame counters rescaled.
    pub fn on_clock_restored(&mut self) {
        self.phase = RecoveryPhase::RestoreClock;
    }

    /// Mark profile and calibration restored.
    pub fn on_profile_restored(&mut self) {
        self.phase = RecoveryPhase::RestoreOutputProfile;
    }

    /// Mark playback resumed successfully. Resets the attempt counter.
    pub fn on_playback_resumed(&mut self) {
        self.phase = RecoveryPhase::ResumePlayback;
        self.attempts = 0;
        self.phase = RecoveryPhase::Idle;
    }

    /// Mark recovery aborted/failed.
    pub fn on_recovery_failed(&mut self) {
        self.phase = RecoveryPhase::Failed;
    }

    /// Access the preserved snapshot if available.
    pub fn preserved_snapshot(&self) -> Option<&PreservedPlaybackSnapshot> {
        self.snapshot.as_ref()
    }
}

/// Rescale an output frame count when output sample rate changes across device recovery.
#[inline]
pub fn rescale_clock_frames(frames: u64, old_rate: u32, new_rate: u32) -> u64 {
    if old_rate == 0 || new_rate == 0 || old_rate == new_rate {
        return frames;
    }
    ((frames as u128) * (new_rate as u128) / (old_rate as u128)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_state_machine_flow() {
        let mut controller = OutputRecoveryController::new(5);
        assert_eq!(controller.phase(), RecoveryPhase::Idle);

        controller.on_device_disappeared();
        assert_eq!(controller.phase(), RecoveryPhase::DeviceDisappeared);
        assert_eq!(controller.attempts(), 1);

        controller.preserve_state(PreservedPlaybackSnapshot {
            source_frames: 48000,
            source_sample_rate: 48000,
            old_output_sample_rate: 48000,
            volume: 0.8,
            output_profile_id: Some("headphones".to_string()),
            was_playing: true,
        });
        assert_eq!(controller.phase(), RecoveryPhase::PreserveEngineState);

        controller.on_device_reopened();
        assert_eq!(controller.phase(), RecoveryPhase::ReopenDevice);

        controller.on_format_reconfigured();
        assert_eq!(controller.phase(), RecoveryPhase::ReconfigureFormat);

        controller.on_clock_restored();
        assert_eq!(controller.phase(), RecoveryPhase::RestoreClock);

        controller.on_profile_restored();
        assert_eq!(controller.phase(), RecoveryPhase::RestoreOutputProfile);

        controller.on_playback_resumed();
        assert_eq!(controller.phase(), RecoveryPhase::Idle);
        assert_eq!(controller.attempts(), 0);
    }

    #[test]
    fn clock_frames_rescaling_is_exact() {
        // 44.1 kHz -> 88.2 kHz: exactly double
        assert_eq!(rescale_clock_frames(44100, 44100, 88200), 88200);
        // 96 kHz -> 48 kHz: exactly half
        assert_eq!(rescale_clock_frames(96000, 96000, 48000), 48000);
        // zero / identity
        assert_eq!(rescale_clock_frames(12345, 48000, 48000), 12345);
        assert_eq!(rescale_clock_frames(12345, 0, 48000), 12345);
    }

    #[test]
    fn hot_plug_disconnect_reconnect_preserves_playhead_and_rescales() {
        let mut controller = OutputRecoveryController::new(5);

        // 1. Playback starts at 44.1 kHz, playhead reaches 132,300 frames (3.00 seconds)
        let initial_playhead_44k = 132_300u64;
        let initial_rate = 44_100u32;
        let initial_volume = 0.85f32;

        // 2. Simulate device disconnect (DeviceNotAvailable / disconnect)
        controller.on_device_disappeared();
        assert_eq!(controller.phase(), RecoveryPhase::DeviceDisappeared);

        // 3. Preserve state: audio pauses, playhead is snapshotted
        controller.preserve_state(PreservedPlaybackSnapshot {
            source_frames: initial_playhead_44k,
            source_sample_rate: 44_100,
            old_output_sample_rate: initial_rate,
            volume: initial_volume,
            output_profile_id: Some("dac_usb_zone".to_string()),
            was_playing: true,
        });
        assert_eq!(controller.phase(), RecoveryPhase::PreserveEngineState);

        // 4. Simulate device reconnect with DIFFERENT sample rate (44.1 kHz -> 48 kHz)
        let new_rate = 48_000u32;
        controller.on_device_reopened();
        assert_eq!(controller.phase(), RecoveryPhase::ReopenDevice);

        controller.on_format_reconfigured();
        assert_eq!(controller.phase(), RecoveryPhase::ReconfigureFormat);

        // 5. Verify clock frame rescaling: playhead_48k = playhead_44k * 48000 / 44100
        let snapshot = controller
            .preserved_snapshot()
            .expect("snapshot must exist");
        let rescaled_frames = rescale_clock_frames(
            snapshot.source_frames,
            snapshot.old_output_sample_rate,
            new_rate,
        );
        let expected_frames = (132_300u128 * 48_000 / 44_100) as u64; // Exactly 144,000 frames = 3.00 seconds!
        assert_eq!(rescaled_frames, expected_frames);
        assert_eq!(rescaled_frames, 144_000);

        controller.on_clock_restored();
        assert_eq!(controller.phase(), RecoveryPhase::RestoreClock);

        // 6. Restore profile and resume playback seamlessly
        controller.on_profile_restored();
        assert_eq!(controller.phase(), RecoveryPhase::RestoreOutputProfile);

        controller.on_playback_resumed();
        assert_eq!(controller.phase(), RecoveryPhase::Idle);

        // 7. Verify zero sample loss: time duration before and after matches exactly
        let duration_before_s = initial_playhead_44k as f64 / initial_rate as f64;
        let duration_after_s = rescaled_frames as f64 / new_rate as f64;
        assert!((duration_after_s - duration_before_s).abs() < 1e-9);
    }
}
