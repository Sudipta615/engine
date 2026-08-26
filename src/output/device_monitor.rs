//! Dynamic Audio Device Hotplug Monitoring.
//!
//! Periodically inspects available audio output endpoints for the active backend,
//! detects device arrivals (e.g. USB DAC plugged in) and removals (e.g. DAC unplugged),
//! and reports delta changes to the engine runtime for event emission and auto-recovery.

use config::AudioBackend;
use std::time::{Duration, Instant};

/// Result of polling audio device changes.
#[derive(Debug, Clone, Default)]
pub struct DeviceDelta {
    /// Newly discovered devices since last poll.
    pub connected: Vec<String>,
    /// Removed devices since last poll.
    pub disconnected: Vec<String>,
    /// Full snapshot of currently available device names.
    pub current_devices: Vec<String>,
    /// Whether the device list changed.
    pub changed: bool,
}

/// Dynamic monitor for audio output endpoint connectivity.
#[derive(Debug)]
pub struct DeviceMonitor {
    backend: AudioBackend,
    known_devices: Vec<String>,
    last_poll: Option<Instant>,
    poll_interval: Duration,
}

impl Default for DeviceMonitor {
    fn default() -> Self {
        Self::new(AudioBackend::default(), Duration::from_millis(1500))
    }
}

impl DeviceMonitor {
    /// Create a new device monitor for the given backend and polling interval.
    pub fn new(backend: AudioBackend, poll_interval: Duration) -> Self {
        let initial_devices = super::cpal_devices::enumerate_devices(backend);
        Self {
            backend,
            known_devices: initial_devices,
            last_poll: Some(Instant::now()),
            poll_interval,
        }
    }

    /// Set the active audio backend.
    pub fn set_backend(&mut self, backend: AudioBackend) {
        if self.backend != backend {
            self.backend = backend;
            self.known_devices = super::cpal_devices::enumerate_devices(backend);
            self.last_poll = Some(Instant::now());
        }
    }

    /// List currently known output devices.
    pub fn current_devices(&self) -> &[String] {
        &self.known_devices
    }

    /// Poll for device changes if `poll_interval` has elapsed, or if `force` is true.
    pub fn poll(&mut self, force: bool) -> Option<DeviceDelta> {
        let now = Instant::now();
        if !force {
            if let Some(last) = self.last_poll {
                if now.duration_since(last) < self.poll_interval {
                    return None;
                }
            }
        }
        self.last_poll = Some(now);

        let latest_devices = super::cpal_devices::enumerate_devices(self.backend);

        let mut connected = Vec::new();
        for dev in &latest_devices {
            if !self.known_devices.contains(dev) {
                connected.push(dev.clone());
            }
        }

        let mut disconnected = Vec::new();
        for dev in &self.known_devices {
            if !latest_devices.contains(dev) {
                disconnected.push(dev.clone());
            }
        }

        let changed = !connected.is_empty() || !disconnected.is_empty();
        self.known_devices = latest_devices.clone();

        Some(DeviceDelta {
            connected,
            disconnected,
            current_devices: latest_devices,
            changed,
        })
    }
}
