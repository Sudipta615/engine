//! Platform-specific hardware endpoint volume helpers (ALSA / CoreAudio).

#[cfg(target_os = "linux")]
use super::OutputError;
#[cfg(target_os = "linux")]
use alsa::mixer::{Mixer, SelemId};

#[cfg(target_os = "linux")]
pub(crate) fn alsa_card_name(device_name: &str) -> String {
    let lower = device_name.to_lowercase();
    if lower.starts_with("hw:") || lower.starts_with("plughw:") {
        let prefix_len = if lower.starts_with("hw:") { 3 } else { 7 };
        let rest = &device_name[prefix_len..];
        if let Some(card) = rest.split(',').next() {
            if !card.is_empty() {
                return format!("hw:{}", card);
            }
        }
    }
    "default".to_string()
}

#[cfg(target_os = "linux")]
const ALSA_CANDIDATE_CONTROLS: &[&str] = &[
    "Master",
    "PCM",
    "Playback",
    "Headphone",
    "Speaker",
    "Front",
    "Digital",
];

/// Check whether native ALSA mixer hardware volume is supported for `device_name`.
#[cfg(target_os = "linux")]
pub(crate) fn alsa_hardware_volume_supported(device_name: &str) -> bool {
    let card = alsa_card_name(device_name);
    let Ok(mixer) = Mixer::new(&card, false).or_else(|_| Mixer::new("default", false)) else {
        return false;
    };
    for &ctrl in ALSA_CANDIDATE_CONTROLS {
        let selem_id = SelemId::new(ctrl, 0);
        if let Some(selem) = mixer.find_selem(&selem_id) {
            if selem.has_playback_volume() {
                return true;
            }
        }
    }
    false
}

/// Set ALSA hardware endpoint volume in-process via ALSA mixer API.
///
/// Discovers per-card / default controls (`Master`, `PCM`, `Playback`, etc.)
/// and sets volume directly with millibel (0.01 dB) precision without
/// spawning any subprocesses.
#[cfg(target_os = "linux")]
pub(crate) fn set_alsa_volume_db(db: f32, device_name: &str) -> Result<(), OutputError> {
    let card = alsa_card_name(device_name);
    let mixer = Mixer::new(&card, false)
        .or_else(|_| Mixer::new("default", false))
        .map_err(|e| {
            OutputError::StreamError(format!("ALSA mixer open failed for {}: {}", card, e))
        })?;

    for &ctrl in ALSA_CANDIDATE_CONTROLS {
        let selem_id = SelemId::new(ctrl, 0);
        if let Some(selem) = mixer.find_selem(&selem_id) {
            if selem.has_playback_volume() {
                // Try setting volume in dB (ALSA dB unit is 0.01 dB = MilliBel)
                let (alsa::mixer::MilliBel(min_db), alsa::mixer::MilliBel(max_db)) =
                    selem.get_playback_db_range();
                if min_db < max_db {
                    let target_mdb = (db * 100.0).round() as i64;
                    let clamped_mdb = target_mdb.clamp(min_db, max_db);
                    if selem
                        .set_playback_db_all(alsa::mixer::MilliBel(clamped_mdb), alsa::Round::Floor)
                        .is_ok()
                    {
                        log::info!(
                            "Hardware volume: set ALSA {} on {} to {:.2} dB",
                            ctrl,
                            device_name,
                            db
                        );
                        return Ok(());
                    }
                }
                // Fallback to integer volume range scaling
                let (min_vol, max_vol) = selem.get_playback_volume_range();
                if min_vol < max_vol {
                    let linear = if db <= -96.0 {
                        0.0
                    } else {
                        10.0_f32.powf(db / 20.0).clamp(0.0, 1.0)
                    };
                    let target_vol = min_vol + ((max_vol - min_vol) as f32 * linear).round() as i64;
                    let clamped_vol = target_vol.clamp(min_vol, max_vol);
                    if selem.set_playback_volume_all(clamped_vol).is_ok() {
                        log::info!(
                            "Hardware volume: set ALSA {} on {} to vol {}/{} ({:.2} dB)",
                            ctrl,
                            device_name,
                            clamped_vol,
                            max_vol,
                            db
                        );
                        return Ok(());
                    }
                }
            }
        }
    }

    Err(OutputError::StreamError(format!(
        "No supported playback volume mixer element found on ALSA card {} for {}",
        card, device_name
    )))
}

// ── macOS CoreAudio hardware volume helpers ─────────────────────────────

/// CoreAudio HAL bindings for hardware endpoint volume.
///
/// `objc2-core-audio` exposes the `AudioObject` C API with pre-generated
/// bindings (no build-time bindgen). The deprecated `AudioHardwareService.h`
/// selectors are defined here by their FourCC values because that header is
/// not covered by the crate's bindings.
///
/// Volume is applied to the **system default output device**, matching the
/// semantics of the hardware-service virtual master volume (this is the
/// volume the macOS Sound menu controls). A future refinement could match the
/// engine's selected device by name/UID; the HAL call is identical.
#[cfg(target_os = "macos")]
pub(crate) mod coreaudio {
    use std::ffi::c_void;
    use std::ptr::NonNull;

    use objc2_core_audio::{
        kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
        AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
        AudioObjectSetPropertyData,
    };

    /// `kAudioHardwareServiceDeviceProperty_VirtualMainVolume` = 'vmvc'.
    const VIRTUAL_MAIN_VOLUME: u32 = 0x766d_7663;
    /// `kAudioHardwareServiceDeviceProperty_VirtualMasterVolume` = 'vmvl'.
    const VIRTUAL_MASTER_VOLUME: u32 = 0x766d_766c;

    /// Convert a dB gain ([-96, 0]) to a CoreAudio linear scalar ([0, 1]).
    pub fn db_to_linear(db: f32) -> f32 {
        if db <= -96.0 {
            0.0
        } else {
            10.0_f32.powf(db / 20.0).clamp(0.0, 1.0)
        }
    }

    fn property_address(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: element,
        }
    }

    /// The system default output device (`kAudioObjectSystemObject` +
    /// `kAudioHardwarePropertyDefaultOutputDevice`).
    pub fn default_output_device() -> Option<AudioObjectID> {
        let mut device: AudioObjectID = 0;
        let mut size = std::mem::size_of::<AudioObjectID>() as u32;
        let mut address = property_address(
            kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        // SAFETY: `address`, `size` and `device` are valid, non-null and
        // outlive the call; this property takes no qualifier data.
        let status = unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject as AudioObjectID,
                NonNull::from(&mut address),
                0,
                std::ptr::null::<c_void>(),
                NonNull::from(&mut size),
                NonNull::from(&mut device).cast::<c_void>(),
            )
        };
        if status == 0 && device != 0 {
            Some(device)
        } else {
            None
        }
    }

    /// Whether `device` exposes a settable virtual main/master volume.
    pub fn supports_virtual_volume(device: AudioObjectID) -> bool {
        let mut volume: f32 = 1.0;
        let mut size = std::mem::size_of::<f32>() as u32;
        for selector in [VIRTUAL_MAIN_VOLUME, VIRTUAL_MASTER_VOLUME] {
            let mut address = property_address(
                selector,
                kAudioObjectPropertyScopeOutput,
                kAudioObjectPropertyElementMain,
            );
            // SAFETY: valid buffers, no qualifier; a non-zero status only
            // means this device lacks that property.
            let status = unsafe {
                AudioObjectGetPropertyData(
                    device,
                    NonNull::from(&mut address),
                    0,
                    std::ptr::null::<c_void>(),
                    NonNull::from(&mut size),
                    NonNull::from(&mut volume).cast::<c_void>(),
                )
            };
            if status == 0 {
                return true;
            }
        }
        false
    }

    /// Set `device`'s virtual main/master volume to a linear scalar in [0, 1].
    ///
    /// Tries `VirtualMainVolume` first, then falls back to the deprecated
    /// `VirtualMasterVolume` selector for older devices.
    pub fn set_virtual_volume(device: AudioObjectID, linear: f32) -> Result<(), String> {
        let volume = linear.clamp(0.0, 1.0);
        let mut last_status = -1i32;
        for selector in [VIRTUAL_MAIN_VOLUME, VIRTUAL_MASTER_VOLUME] {
            let mut address = property_address(
                selector,
                kAudioObjectPropertyScopeOutput,
                kAudioObjectPropertyElementMain,
            );
            // SAFETY: `address` and `volume` are valid and outlive the call;
            // this property takes no qualifier data.
            let status = unsafe {
                AudioObjectSetPropertyData(
                    device,
                    NonNull::from(&mut address),
                    0,
                    std::ptr::null::<c_void>(),
                    std::mem::size_of::<f32>() as u32,
                    NonNull::from(&volume).cast::<c_void>(),
                )
            };
            if status == 0 {
                log::info!(
                    "Hardware volume: set CoreAudio virtual volume to {:.3}",
                    volume
                );
                return Ok(());
            }
            last_status = status;
        }
        Err(format!(
            "CoreAudio refused VirtualMainVolume/VirtualMasterVolume set (OSStatus {last_status})"
        ))
    }
}
