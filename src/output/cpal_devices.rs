use std::sync::atomic::{AtomicBool, Ordering};

use config::AudioBackend;
use cpal::traits::{DeviceTrait, HostTrait};

/// Enumerate available output device names for a given audio backend
pub fn enumerate_devices(backend: AudioBackend) -> Vec<String> {
    let host = match backend {
        #[cfg(target_os = "linux")]
        AudioBackend::ExclusiveAlsa => {
            cpal::host_from_id(cpal::HostId::Alsa).unwrap_or_else(|_| cpal::default_host())
        }
        #[cfg(target_os = "windows")]
        AudioBackend::ExclusiveWasapi => {
            cpal::host_from_id(cpal::HostId::Wasapi).unwrap_or_else(|_| cpal::default_host())
        }
        #[cfg(all(target_os = "windows", feature = "asio"))]
        AudioBackend::ExclusiveAsio => {
            cpal::host_from_id(cpal::HostId::Asio).unwrap_or_else(|_| cpal::default_host())
        }
        #[cfg(target_os = "macos")]
        AudioBackend::ExclusiveCoreAudioHog => cpal::default_host(),
        _ => cpal::default_host(),
    };

    let mut device_names = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            if let Ok(desc) = d.description() {
                let name = desc.name().to_string();
                if !device_names.contains(&name) {
                    device_names.push(name);
                }
            }
        }
    }
    device_names
}

/// Escalate the current thread (the CPAL audio callback thread) to real-time
/// priority. Runs only once; subsequent calls return immediately.
pub fn escalate_callback_thread_priority(initialized: &AtomicBool) {
    if initialized.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = crate::buffer::enable_flush_zero_denormals_on_current_thread();

    #[cfg(feature = "thread-priority")]
    {
        use thread_priority::{set_current_thread_priority, ThreadPriority};
        let res = set_current_thread_priority(ThreadPriority::Max);
        if let Err(e) = res {
            log::debug!(
                "Audio callback thread priority escalation failed: {:?}; continuing with default priority",
                e
            );
        } else {
            log::info!("Audio callback thread priority escalated to Max successfully");
        }
    }
}
