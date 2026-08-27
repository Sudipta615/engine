//! Multi-endpoint routing primitives for Phase 5.
//!
//! Endpoint state is deliberately independent: each endpoint owns one ring,
//! one configuration snapshot, and one set of counters. The registry itself
//! is control-side state; realtime callbacks only receive their endpoint ring.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use serde::{Deserialize, Serialize};

use crate::buffer::FixedFrameBuffer;
use crate::output::{create_output, Output, OutputError};

/// Stable endpoint identifier. Prefer an OS/backend identifier; names are a
/// compatibility fallback for CPAL devices that do not expose one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EndpointId(String);

impl EndpointId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for EndpointId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for EndpointId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// One output endpoint in the routing matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub id: EndpointId,
    pub backend: config::AudioBackend,
    pub device: Option<String>,
    #[serde(default = "default_gain")]
    pub gain: f32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_gain() -> f32 {
    1.0
}

fn default_enabled() -> bool {
    true
}

impl EndpointConfig {
    pub fn new(id: impl Into<EndpointId>, device: Option<String>) -> Self {
        Self {
            id: id.into(),
            backend: config::AudioBackend::Auto,
            device,
            gain: 1.0,
            enabled: true,
        }
    }
}

/// Counters published by an endpoint without sharing mutable audio state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndpointStats {
    pub written_frames: u64,
    pub dropped_frames: u64,
    pub available_frames: usize,
}

impl EndpointConfig {
    pub fn from_config(config: config::EndpointConfig) -> Self {
        Self {
            id: EndpointId::new(config.id),
            backend: config.backend,
            device: config.device,
            gain: config.gain,
            enabled: config.enabled,
        }
    }
}

/// Per-endpoint ring and counters. No ring is shared between endpoints.
#[derive(Clone)]
pub struct EndpointRing {
    id: EndpointId,
    buffer: Arc<FixedFrameBuffer>,
    written: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl EndpointRing {
    pub fn new(id: EndpointId, capacity_frames: usize) -> Result<Self, crate::buffer::BufferError> {
        Ok(Self {
            id,
            buffer: Arc::new(FixedFrameBuffer::new(capacity_frames)?),
            written: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn id(&self) -> &EndpointId {
        &self.id
    }

    pub fn reset(&self) {
        self.buffer.reset();
    }

    pub fn pop_interleaved(&self, output: &mut [f32], channels: usize) -> usize {
        self.buffer.pop_frames_interleaved(output, channels.max(1))
    }

    pub fn buffer(&self) -> &Arc<FixedFrameBuffer> {
        &self.buffer
    }

    pub fn push_interleaved(&self, samples: &[f32], channels: usize) -> usize {
        let channels = channels.max(1);
        let requested = samples.len() / channels;
        let frames = self.buffer.push_frames_interleaved(samples, channels);
        self.written.fetch_add(frames as u64, Ordering::Relaxed);
        self.dropped
            .fetch_add((requested - frames) as u64, Ordering::Relaxed);
        frames
    }

    pub fn stats(&self) -> EndpointStats {
        EndpointStats {
            written_frames: self.written.load(Ordering::Relaxed),
            dropped_frames: self.dropped.load(Ordering::Relaxed),
            available_frames: self.buffer.available(),
        }
    }
}

/// A control-side endpoint worker. Its output object and ring are owned by
/// this endpoint only. The worker does not run engine/DSP code; the engine
/// produces frames and publishes them to the private ring.
pub struct EndpointWorker {
    config: EndpointConfig,
    ring: EndpointRing,
    output: Option<Box<dyn Output>>,
    running: Arc<AtomicBool>,
}

impl EndpointWorker {
    pub fn open(mut config: EndpointConfig, capacity_frames: usize) -> Result<Self, OutputError> {
        config.gain = config.gain.clamp(0.0, 4.0);
        let ring = EndpointRing::new(config.id.clone(), capacity_frames)
            .map_err(|e| OutputError::StreamOpen(e.to_string()))?;
        let mut output = create_output(
            Arc::clone(ring.buffer()),
            config.backend,
            config.device.as_deref(),
            config::FallbackPolicy::Allow,
        )?;
        output.start()?;
        Ok(Self {
            config,
            ring,
            output: Some(output),
            running: Arc::new(AtomicBool::new(true)),
        })
    }

    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    pub fn ring(&self) -> &EndpointRing {
        &self.ring
    }

    pub fn output(&self) -> Option<&dyn Output> {
        self.output.as_deref()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn reset(&self) {
        self.ring.reset();
        if let Some(output) = self.output() {
            output.reset_buffer();
        }
    }

    /// Update gain and enabled state without changing the live transport.
    /// Backend/device changes must use `AudioEngine` endpoint reconfiguration.
    pub fn update_config(&mut self, mut config: EndpointConfig) {
        debug_assert_eq!(
            config.id, self.config.id,
            "endpoint ID is immutable while open"
        );
        debug_assert_eq!(
            config.backend, self.config.backend,
            "backend changes require reopen"
        );
        debug_assert_eq!(
            config.device, self.config.device,
            "device changes require reopen"
        );
        config.id = self.config.id.clone();
        config.backend = self.config.backend;
        config.device = self.config.device.clone();
        config.gain = config.gain.clamp(0.0, 4.0);
        self.config = config;
    }

    /// Update live gain/enabled state while preserving the transport.
    pub fn set_config(&mut self, config: EndpointConfig) -> EndpointConfig {
        let previous = self.config.clone();
        self.update_config(config);
        previous
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(mut output) = self.output.take() {
            output.stop();
        }
    }
}

impl Drop for EndpointWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Deterministic virtual endpoint used by headless tests and hosts that want
/// to consume endpoint streams without opening an OS audio device.
#[derive(Clone)]
pub struct VirtualEndpoint {
    config: EndpointConfig,
    ring: EndpointRing,
}

impl VirtualEndpoint {
    pub fn new(
        config: EndpointConfig,
        capacity_frames: usize,
    ) -> Result<Self, crate::buffer::BufferError> {
        let ring = EndpointRing::new(config.id.clone(), capacity_frames)?;
        Ok(Self { config, ring })
    }

    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
    }

    pub fn ring(&self) -> &EndpointRing {
        &self.ring
    }

    /// Consume frames from this endpoint's private ring.
    pub fn pop_interleaved(&self, output: &mut [f32], channels: usize) -> usize {
        self.ring
            .buffer
            .pop_frames_interleaved(output, channels.max(1))
    }
}

/// Control-side registry for endpoint configurations and private rings.
#[derive(Default)]
pub struct EndpointRegistry {
    endpoints: Vec<(EndpointConfig, EndpointRing)>,
}

impl EndpointRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &(EndpointConfig, EndpointRing)> {
        self.endpoints.iter()
    }

    pub fn add(
        &mut self,
        config: EndpointConfig,
        capacity_frames: usize,
    ) -> Result<EndpointRing, crate::buffer::BufferError> {
        if let Some((_, ring)) = self.endpoints.iter().find(|(c, _)| c.id == config.id) {
            return Ok(ring.clone());
        }
        let ring = EndpointRing::new(config.id.clone(), capacity_frames)?;
        self.endpoints.push((config, ring.clone()));
        Ok(ring)
    }

    pub fn remove(&mut self, id: &EndpointId) -> Option<(EndpointConfig, EndpointRing)> {
        let pos = self.endpoints.iter().position(|(c, _)| &c.id == id)?;
        Some(self.endpoints.remove(pos))
    }

    pub fn get(&self, id: &EndpointId) -> Option<&(EndpointConfig, EndpointRing)> {
        self.endpoints.iter().find(|(c, _)| &c.id == id)
    }

    pub fn get_mut(&mut self, id: &EndpointId) -> Option<&mut (EndpointConfig, EndpointRing)> {
        self.endpoints.iter_mut().find(|(c, _)| &c.id == id)
    }

    pub fn update(&mut self, mut config: EndpointConfig) -> Option<EndpointConfig> {
        let entry = self.get_mut(&config.id)?;
        config.gain = config.gain.clamp(0.0, 4.0);
        Some(std::mem::replace(&mut entry.0, config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_allocates_independent_rings() {
        let mut registry = EndpointRegistry::new();
        let a = registry.add(EndpointConfig::new("a", None), 64).unwrap();
        let b = registry.add(EndpointConfig::new("b", None), 64).unwrap();
        assert_eq!(registry.len(), 2);
        assert!(!Arc::ptr_eq(a.buffer(), b.buffer()));
    }

    #[test]
    fn ring_counts_short_writes_without_shared_state() {
        let ring = EndpointRing::new(EndpointId::new("a"), 2).unwrap();
        let samples = [0.0f32; 8];
        assert_eq!(ring.push_interleaved(&samples, 2), 2);
        assert_eq!(ring.stats().dropped_frames, 2);
    }

    #[test]
    fn virtual_endpoints_can_be_consumed_independently() {
        let a = VirtualEndpoint::new(EndpointConfig::new("a", None), 8).unwrap();
        let b = VirtualEndpoint::new(EndpointConfig::new("b", None), 8).unwrap();
        a.ring().push_interleaved(&[1.0, 2.0, 3.0, 4.0], 2);
        b.ring().push_interleaved(&[5.0, 6.0], 2);
        let mut out = [0.0; 4];
        assert_eq!(a.pop_interleaved(&mut out, 2), 2);
        assert_eq!(&out[..4], &[1.0, 2.0, 3.0, 4.0]);
        let mut one = [0.0; 2];
        assert_eq!(b.pop_interleaved(&mut one, 2), 1);
        assert_eq!(one, [5.0, 6.0]);
    }
}
