//! `IAudioEndpointVolume` change-notification callback.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use windows::core::implement;
use windows::Win32::Media::{
    Audio::Endpoints::{IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl},
    Audio::AUDIO_VOLUME_NOTIFICATION_DATA,
};

/// Shared state between the `IAudioEndpointVolume` change-notification
/// callback (fires on the OS audio service thread) and the output owner.
/// Written by [`EndpointVolumeCallback`], read/cleared by
/// [`WasapiOutput::take_external_volume_change`].
#[derive(Clone, Default)]
pub(crate) struct VolumeCallbackState {
    /// Latest master volume level (linear 0.0–1.0) from the last
    /// notification, stored as f32 bits.
    pub(crate) volume_linear: Arc<AtomicU32>,
    /// Muted flag from the last notification. Recorded for completeness;
    /// `PlaybackInfo` has no mute field, so it is not surfaced yet.
    pub(crate) muted: Arc<AtomicBool>,
    /// Set on every notification; cleared by `take_external_volume_change`
    /// so the engine only publishes a volume update when something changed.
    pub(crate) changed: Arc<AtomicBool>,
}

/// COM callback receiving `IAudioEndpointVolume` change notifications —
/// the OS volume slider, a hardware knob, or programmatic sets from any
/// process. windows-rs 0.59 implements COM interfaces via the `implement`
/// macro; the vtable dispatches to `IAudioEndpointVolumeCallback_Impl`.
#[implement(IAudioEndpointVolumeCallback)]
pub(crate) struct EndpointVolumeCallback {
    pub(crate) state: VolumeCallbackState,
}

impl IAudioEndpointVolumeCallback_Impl for EndpointVolumeCallback_Impl {
    fn OnNotify(&self, pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows_core::Result<()> {
        if pnotify.is_null() {
            return Ok(());
        }
        let data = unsafe { &*pnotify };
        self.state
            .volume_linear
            .store(data.fMasterVolume.to_bits(), Ordering::Release);
        self.state
            .muted
            .store(data.bMuted.0 != 0, Ordering::Release);
        self.state.changed.store(true, Ordering::Release);
        Ok(())
    }
}
