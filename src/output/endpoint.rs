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
use rubato::audioadapter_buffers::direct::{SequentialSliceOfSlices, SequentialSliceOfVecs};
use rubato::{Adjustable, FixedAsync, Resampler, Slip};

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
    /// Per-endpoint clock drift correction (ring-fill feedback on the
    /// resampler ratio). Default true.
    #[serde(default = "default_drift_correction")]
    pub drift_correction: bool,
}

fn default_gain() -> f32 {
    1.0
}

fn default_enabled() -> bool {
    true
}

fn default_drift_correction() -> bool {
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
            drift_correction: true,
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
            drift_correction: config.drift_correction,
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
/// Maximum drift correction applied to the endpoint resampler ratio, as a
/// fraction of the nominal rate (500 ppm). Bounds the retune so a broken
/// device or a stuck ring can never drive the rate wildly off.
pub const MAX_DRIFT_RATIO: f64 = 0.0005;

/// Proportional gain of the drift controller, per frame of low-passed ring
/// fill error. The fill moves by roughly `production_per_push × K × error`
/// per push, so at balance the fill sits `drift_ppm × 1e-6 / K` frames off
/// the midpoint: ~63 frames for a 94 ppm drift at this gain — a small,
/// constant, harmless offset (the ring is thousands of frames). The gain is
/// small enough that the loop cannot oscillate: the per-push correction is a
/// tiny fraction of a frame even at the clamps.
const DRIFT_P_GAIN: f64 = 1.5e-6;

/// Ring-fill feedback controller that steers a per-endpoint [`Slip`]
/// resampler to the device's ACTUAL clock instead of its nominal rate.
///
/// Independent audio devices drift (typical crystal tolerance is 10–100 ppm);
/// with a fixed nominal ratio the endpoint ring either slowly fills (device
/// slower than nominal) or drains (device faster). The controller samples the
/// ring fill on every push, low-passes it (per-block chunk jitter is
/// ignored), and sets the slip ratio to `1 − K × error`: fill above the
/// midpoint slows the slip, below speeds it up. The plant is deliberately
/// transient-free — the slip just inserts or drops a frame with a short
/// crossfade, taking effect on the next chunk — so a plain proportional law
/// is stable and converges the ratio onto the true clock with zero steady
/// error in the rate (only a small constant fill offset, see
/// [`DRIFT_P_GAIN`]). The ratio is clamped to ±[`MAX_DRIFT_RATIO`].
/// Control-plane only — this never runs on a realtime callback.
pub struct DriftController {
    enabled: bool,
    /// Ring capacity in stereo frames.
    capacity: f64,
    /// Low-passed fill error (frames) vs the target midpoint.
    error_lp: f64,
    /// Current applied slip ratio (≈1.0); telemetry derives the ppm offset.
    ratio: f64,
}

impl DriftController {
    pub fn new(enabled: bool, capacity_frames: usize) -> Self {
        Self {
            enabled,
            capacity: capacity_frames.max(1) as f64,
            error_lp: 0.0,
            ratio: 1.0,
        }
    }

    /// Sample the ring fill (stereo frames waiting for the device) and
    /// update the slip ratio. Deterministic and allocation-free.
    pub fn update(&mut self, fill: usize) {
        if !self.enabled {
            return;
        }
        // Fill above the midpoint means the device consumes slower than we
        // produce → slow the slip (ratio < 1); below → speed it up.
        let error = fill as f64 - self.capacity * 0.5;
        self.error_lp += 0.01 * (error - self.error_lp);
        let target = 1.0 - DRIFT_P_GAIN * self.error_lp;
        self.ratio = target.clamp(1.0 - MAX_DRIFT_RATIO, 1.0 + MAX_DRIFT_RATIO);
    }

    /// Current slip ratio to apply to the resampler.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Current drift offset in ppm (telemetry; positive = device faster
    /// than nominal).
    pub fn offset_ppm(&self) -> i64 {
        ((self.ratio - 1.0) * 1e6).round() as i64
    }

    /// Whether drift correction is enabled and the resampler is active.
    pub fn active(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            // Disabling restores the nominal ratio and forgets the transient:
            // re-enabling observes the drift from a clean slate.
            self.ratio = 1.0;
            self.error_lp = 0.0;
        }
    }
}

/// A control-side endpoint worker. Its output object and ring are owned by
/// this endpoint only. The worker does not run engine/DSP code; the engine
/// produces frames and publishes them to the private ring. When the
/// endpoint's device rate differs from the master rate, the worker runs a
/// per-endpoint resampler (with optional clock-drift correction) before the
/// ring.
pub struct EndpointWorker {
    config: EndpointConfig,
    ring: EndpointRing,
    output: Option<Box<dyn Output>>,
    running: Arc<AtomicBool>,
    /// Resamples the master-domain block into this endpoint's rate domain at
    /// the NOMINAL device rate. `None` when the rates match (passthrough).
    /// Never retuned after open — drift is handled by [`EndpointWorker::slip`]
    /// so the FFT plan (and its chunk granularity) stays fixed.
    resampler: Option<crate::dsp::resampler::GenericResampler>,
    /// Rubato [`Slip`]: a 1:1 clutch that trims the nominal-rate stream to
    /// the device's ACTUAL clock by occasionally inserting/dropping a frame
    /// behind a short crossfade. Ratio ≈1.0, steered by [`DriftController`]
    /// from the ring fill. `None` when drift correction is disabled.
    slip: Option<Slip<f32>>,
    /// Clock-drift feedback controller (see [`DriftController`]).
    drift: DriftController,
    /// Planar staging buffers for the resampler output between pushes (fed
    /// into `slip`; or pushed straight to the ring when no slip is active).
    /// Preallocated on open; the decode-loop push stays allocation-free.
    slip_in_l: Vec<f32>,
    slip_in_r: Vec<f32>,
    slip_in_len: usize,
    /// Planar output scratch for one slip chunk.
    slip_out: [Vec<f32>; 2],
    /// Interleaved output batch for ring pushes.
    ring_batch: Vec<f32>,
    /// Reusable gain scaling scratch (allocated once on first use; the
    /// decode-loop push stays allocation-free in steady state).
    scratch: Vec<f32>,
}

impl EndpointWorker {
    /// Open an endpoint worker: build the device output, the per-endpoint
    /// resampler (master → device rate, when they differ) and the drift
    /// controller. `master_rate` is the decode loop's rate domain. Control
    /// path — allocation is legal here; the audio-side push is
    /// allocation-free.
    pub fn open(
        mut config: EndpointConfig,
        capacity_frames: usize,
        master_rate: u32,
        resampler_quality: config::ResamplerQuality,
        precision: config::PrecisionMode,
    ) -> Result<Self, OutputError> {
        config.gain = config.gain.clamp(0.0, 4.0);
        let ring = EndpointRing::new(config.id.clone(), capacity_frames)
            .map_err(|e| OutputError::StreamOpen(e.to_string()))?;
        let mut output = create_output(
            Arc::clone(ring.buffer()),
            config.backend,
            config.device.as_deref(),
            config::FallbackPolicy::Allow,
        )?;
        let rate = output.sample_rate();
        output.start()?;
        let resampler = if rate != master_rate && master_rate > 0 {
            let built = match precision {
                config::PrecisionMode::Performance => {
                    crate::dsp::resampler::AudioResampler::<f32>::new(
                        resampler_quality,
                        master_rate as f32,
                        rate as f32,
                    )
                    .map(crate::dsp::resampler::GenericResampler::F32)
                }
                config::PrecisionMode::Quality => {
                    crate::dsp::resampler::AudioResampler::<f64>::new(
                        resampler_quality,
                        master_rate as f32,
                        rate as f32,
                    )
                    .map(crate::dsp::resampler::GenericResampler::F64)
                }
            };
            match built {
                Ok(rs) => Some(rs),
                Err(e) => {
                    log::warn!(
                        "Endpoint '{}': resampler build failed ({} → {} Hz), pushing directly: {}",
                        config.id.as_str(),
                        master_rate,
                        rate,
                        e
                    );
                    None
                }
            }
        } else {
            None
        };
        // Drift correction only makes sense with a resampler in the chain:
        // the FFT resampler converts at the nominal ratio and the slip trims
        // the actual device clock. Same-rate endpoints have no ratio to trim.
        let drift_correction = config.drift_correction && resampler.is_some();
        let slip = if drift_correction {
            match Slip::<f32>::new(512, 2, FixedAsync::Output) {
                Ok(s) => Some(s),
                Err(e) => {
                    log::warn!(
                        "Endpoint '{}': slip resampler build failed, drift correction off: {}",
                        config.id.as_str(),
                        e
                    );
                    None
                }
            }
        } else {
            None
        };
        // Staging capacity: a high-quality FFT chunk can emit ~2.3k frames
        // in one push, plus the slip's buffered input (~515). 4096 covers
        // every quality tier; growth is logged defensively (never expected).
        let staging_cap = 4096usize;
        Ok(Self {
            config,
            ring,
            output: Some(output),
            running: Arc::new(AtomicBool::new(true)),
            resampler,
            slip,
            drift: DriftController::new(drift_correction, capacity_frames),
            slip_in_l: vec![0.0; staging_cap],
            slip_in_r: vec![0.0; staging_cap],
            slip_in_len: 0,
            slip_out: [vec![0.0; 520], vec![0.0; 520]],
            ring_batch: vec![0.0; 520 * 2],
            scratch: Vec::new(),
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

    /// Drift state for telemetry: (active, offset ppm). Active only when a
    /// resampler and a slip exist (same-rate endpoints have no ratio to
    /// correct).
    pub fn drift_state(&self) -> (bool, i64) {
        let active = self.slip.is_some() && self.drift.active();
        (active, if active { self.drift.offset_ppm() } else { 0 })
    }

    /// Push a master-domain block into this endpoint's chain: gain,
    /// resample into the device rate at the nominal ratio (when rates
    /// differ), trim to the device's actual clock via the drift slip, then
    /// push to the ring. Returns the frames accepted by the ring.
    /// Allocation-free; the control (decode) loop calls this, never a
    /// realtime callback.
    pub fn push_interleaved(&mut self, samples: &[f32], channels: usize) -> usize {
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        if frames == 0 {
            return 0;
        }
        if channels != 2 {
            // Multichannel endpoints push the master block directly (the
            // resampler and slip are stereo); rate matching applies to the
            // stereo path. Gains still apply.
            return self.push_direct(samples, channels);
        }
        let Some(rs) = &mut self.resampler else {
            return self.push_direct(samples, channels);
        };
        // Drift correction first: sample the current ring fill and steer the
        // slip ratio. The FFT resampler is never retuned (its plan and chunk
        // granularity stay fixed at the nominal ratio).
        self.drift.update(self.ring.stats().available_frames);
        if let Some(slip) = self.slip.as_mut() {
            let _ = slip.set_resample_ratio(self.drift.ratio(), false);
        }
        let gain = self.config.gain;
        // Feed each master frame; drain the resampled output into the planar
        // staging buffer.
        for i in 0..frames {
            let (l, r) = (samples[i * 2], samples[i * 2 + 1]);
            rs.feed_f32(l, r);
            while let Some((ol, or_)) = rs.read_f32() {
                if self.slip_in_len >= self.slip_in_l.len() {
                    // Never expected (staging is sized for the worst quality
                    // tier); grow defensively on this control path.
                    log::warn!(
                        "Endpoint '{}': staging buffer grew",
                        self.config.id.as_str()
                    );
                    self.slip_in_l.resize(self.slip_in_l.len() * 2, 0.0);
                    self.slip_in_r.resize(self.slip_in_l.len(), 0.0);
                }
                self.slip_in_l[self.slip_in_len] = ol * gain;
                self.slip_in_r[self.slip_in_len] = or_ * gain;
                self.slip_in_len += 1;
            }
        }
        let mut accepted = 0usize;
        if self.slip.is_some() {
            self.flush_slip_to_ring(&mut accepted);
        } else if self.slip_in_len > 0 {
            // No drift correction: push the resampled block straight through.
            let n = self.slip_in_len;
            for j in 0..n {
                self.ring_batch[j * 2] = self.slip_in_l[j];
                self.ring_batch[j * 2 + 1] = self.slip_in_r[j];
            }
            accepted += self.ring.push_interleaved(&self.ring_batch[..n * 2], 2);
            self.slip_in_len = 0;
        }
        accepted
    }

    /// Run complete slip chunks out of the staging buffer into the ring.
    /// Allocation-free; called from the decode-loop push.
    fn flush_slip_to_ring(&mut self, accepted: &mut usize) {
        let Some(slip) = self.slip.as_mut() else {
            return;
        };
        loop {
            let needed = slip.input_frames_next();
            if self.slip_in_len < needed {
                break;
            }
            let in_l = &self.slip_in_l[..needed];
            let in_r = &self.slip_in_r[..needed];
            let in_channels = [in_l, in_r];
            let input = SequentialSliceOfSlices::new(&in_channels, 2, needed)
                .expect("slip input slices are preallocated");
            let out_len = slip.output_frames_next();
            let mut output = SequentialSliceOfVecs::new_mut(&mut self.slip_out[..], 2, out_len)
                .expect("slip output slices are preallocated");
            if let Err(e) = slip.process_into_buffer(&input, &mut output, None) {
                log::warn!(
                    "Endpoint '{}': slip process error: {}",
                    self.config.id.as_str(),
                    e
                );
                self.slip_in_len = 0;
                return;
            }
            // Interleave the fixed-size slip output into the ring batch.
            for j in 0..out_len {
                self.ring_batch[j * 2] = self.slip_out[0][j];
                self.ring_batch[j * 2 + 1] = self.slip_out[1][j];
            }
            *accepted += self
                .ring
                .push_interleaved(&self.ring_batch[..out_len * 2], 2);
            // Compact the staging buffer past the consumed chunk.
            let remaining = self.slip_in_len - needed;
            if remaining > 0 {
                self.slip_in_l.copy_within(needed..needed + remaining, 0);
                self.slip_in_r.copy_within(needed..needed + remaining, 0);
            }
            self.slip_in_len = remaining;
        }
    }

    /// Same-rate / multichannel direct push with gain applied (the previous
    /// passthrough path, kept bit-identical for same-rate stereo endpoints).
    fn push_direct(&mut self, samples: &[f32], channels: usize) -> usize {
        let gain = self.config.gain;
        if (gain - 1.0).abs() <= f32::EPSILON {
            return self.ring.push_interleaved(samples, channels);
        }
        if samples.len() > self.scratch.len() {
            // First push (or a larger block): grow once, then reuse.
            self.scratch.resize(samples.len(), 0.0);
        }
        for (dst, src) in self.scratch[..samples.len()].iter_mut().zip(samples) {
            *dst = *src * gain;
        }
        self.ring
            .push_interleaved(&self.scratch[..samples.len()], channels)
    }

    pub fn reset(&self) {
        self.ring.reset();
        if let Some(output) = self.output() {
            output.reset_buffer();
        }
    }

    /// Update gain / enabled / drift-correction state without changing the
    /// live transport. Backend/device changes must use `AudioEngine`
    /// endpoint reconfiguration.
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
        self.drift
            .set_enabled(config.drift_correction && self.resampler.is_some());
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

    // ── Drift controller ──────────────────────────────────────────────────

    #[test]
    fn drift_controller_disabled_never_moves() {
        let mut ctrl = DriftController::new(false, 8192);
        for _ in 0..100 {
            ctrl.update(7000);
        }
        assert_eq!(ctrl.ratio(), 1.0, "disabled: ratio stays nominal");
        assert_eq!(ctrl.offset_ppm(), 0);
        assert!(!ctrl.active());
    }

    #[test]
    fn drift_controller_slow_device_lowers_ratio() {
        // A device consuming slower than nominal fills the ring (fill well
        // above the midpoint) → the controller must slow the slip (ratio < 1).
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(7000);
        }
        assert!(ctrl.ratio() < 1.0, "slow device → ratio below 1");
        assert!(ctrl.offset_ppm() < 0, "offset must be negative");
    }

    #[test]
    fn drift_controller_fast_device_raises_ratio() {
        // A device consuming faster than nominal drains the ring (fill below
        // the midpoint) → the controller must speed the slip (ratio > 1).
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(1000);
        }
        assert!(ctrl.ratio() > 1.0, "fast device → ratio above 1");
        assert!(ctrl.offset_ppm() > 0, "offset must be positive");
    }

    #[test]
    fn drift_controller_proportional_law() {
        // The ratio is a pure proportional law on the low-passed fill error:
        // offset ≈ −DRIFT_P_GAIN × 1e6 × (fill − midpoint) once converged.
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(4200); // error 104 frames
        }
        let expect_ppm = -(DRIFT_P_GAIN * 104.0 * 1e6).round() as i64;
        assert!(
            (ctrl.offset_ppm() - expect_ppm).abs() <= 2,
            "offset {}=≈ {expect_ppm} ppm (ratio {})",
            ctrl.offset_ppm(),
            ctrl.ratio()
        );
        // Midpoint: no error → nominal ratio.
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(4096);
        }
        assert_eq!(ctrl.ratio(), 1.0, "midpoint: ratio stays 1.0");
        assert_eq!(ctrl.offset_ppm(), 0);
    }

    #[test]
    fn drift_controller_clamps_at_max_drift() {
        // A pathological fill (ring pinned full / empty) saturates at ±500 ppm.
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(8192);
        }
        let max_ppm = (MAX_DRIFT_RATIO * 1e6).round() as i64;
        assert_eq!(ctrl.offset_ppm(), -max_ppm, "clamped at −500 ppm");

        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(0);
        }
        assert_eq!(ctrl.offset_ppm(), max_ppm, "clamped at +500 ppm");
    }

    #[test]
    fn drift_controller_enable_toggle_resets_state() {
        let mut ctrl = DriftController::new(true, 8192);
        for _ in 0..2000 {
            ctrl.update(7000);
        }
        assert_ne!(ctrl.offset_ppm(), 0);
        ctrl.set_enabled(false);
        assert_eq!(ctrl.ratio(), 1.0, "disable resets the ratio");
        assert_eq!(ctrl.offset_ppm(), 0, "disable resets the offset");
        ctrl.update(7000);
        assert_eq!(ctrl.ratio(), 1.0, "disabled: no movement");
        ctrl.set_enabled(true);
        for _ in 0..2000 {
            ctrl.update(7000);
        }
        assert_ne!(ctrl.offset_ppm(), 0, "re-enable resumes correcting");
    }

    // ── Endpoint worker: resampling + drift end to end ───────────────────

    /// Build a worker WITHOUT a real device output (the ring is the boundary;
    /// the test plays the "device" by popping at a simulated rate).
    fn worker_without_output(
        config: EndpointConfig,
        capacity: usize,
        master_rate: u32,
    ) -> EndpointWorker {
        let ring = EndpointRing::new(config.id.clone(), capacity).unwrap();
        let rate = 48_000; // the simulated device rate
        let resampler = if rate != master_rate && master_rate > 0 {
            Some(
                crate::dsp::resampler::AudioResampler::<f32>::new(
                    config::ResamplerQuality::Fast,
                    master_rate as f32,
                    rate as f32,
                )
                .map(crate::dsp::resampler::GenericResampler::F32)
                .unwrap(),
            )
        } else {
            None
        };
        let drift_correction = config.drift_correction && resampler.is_some();
        let slip = if drift_correction {
            Some(Slip::<f32>::new(512, 2, FixedAsync::Output).unwrap())
        } else {
            None
        };
        let drift = DriftController::new(drift_correction, capacity);
        EndpointWorker {
            config,
            ring,
            output: None,
            running: Arc::new(AtomicBool::new(true)),
            resampler,
            slip,
            drift,
            slip_in_l: vec![0.0; 4096],
            slip_in_r: vec![0.0; 4096],
            slip_in_len: 0,
            slip_out: [vec![0.0; 520], vec![0.0; 520]],
            ring_batch: vec![0.0; 520 * 2],
            scratch: Vec::new(),
        }
    }

    #[test]
    fn rate_mismatched_worker_resamples_into_endpoint_rate() {
        // Master 44.1 kHz → device 48 kHz: the ring must receive ≈ the
        // resampled frame count, and the resampled content must be a tone.
        let mut worker = worker_without_output(EndpointConfig::new("ep", None), 16_384, 44_100);
        assert!(worker.resampler.is_some());
        assert!(worker.drift.active());

        // ~0.5 s of a 440 Hz sine at the master rate.
        let master = 44_100u32;
        let n = 22_050usize;
        let mut batch = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / master as f32).sin() * 0.5;
            batch.push(s);
            batch.push(s);
        }
        // Feed in decode-loop-sized blocks (128 master frames each).
        for chunk in batch.chunks(128 * 2) {
            worker.push_interleaved(chunk, 2);
        }

        let stats = worker.ring.stats();
        let expect = (n as f64 * 48_000.0 / master as f64) as u64;
        assert!(
            stats.written_frames >= expect * 3 / 4 && stats.written_frames <= expect + 2048,
            "resampled frames {}/{} ≈ {expect}",
            stats.written_frames,
            stats.dropped_frames
        );
        // Content sanity: the ring holds a non-silent, finite signal.
        let mut out = vec![0.0f32; stats.written_frames as usize * 2];
        let got = worker.ring.pop_interleaved(&mut out, 2);
        assert!(got > 0);
        assert!(
            out[..got * 2].iter().any(|&v| v.abs() > 1e-3),
            "resampled output is silence"
        );
        assert!(out[..got * 2].iter().all(|v| v.is_finite()));
    }

    #[test]
    fn same_rate_worker_passes_through_with_gain() {
        let mut config = EndpointConfig::new("ep", None);
        config.gain = 0.5;
        let mut worker = worker_without_output(config, 1024, 48_000);
        assert!(worker.resampler.is_none(), "same rate: passthrough");
        assert!(!worker.drift.active(), "no resampler → no drift correction");
        let samples = [0.25f32; 256];
        let accepted = worker.push_interleaved(&samples, 2);
        assert_eq!(accepted, 128);
        let mut out = [0.0f32; 256];
        let got = worker.ring.pop_interleaved(&mut out, 2);
        assert_eq!(got, 128);
        for i in 0..128 {
            assert!((out[i * 2] - 0.125).abs() < 1e-6, "gain applied at {i}");
        }
    }

    #[test]
    fn drift_correction_tracks_a_slow_device_clock() {
        // Simulate a device whose real clock runs 100 ppm SLOWER than its
        // nominal 48 kHz. The master produces 88 frames per 2 ms cycle at
        // 44.1 kHz → 95.782 endpoint frames/cycle at the nominal ratio; the
        // device pops 95.782 * (1 − 100e-6) ≈ 95.773 frames/cycle, so the
        // ring fill creeps up at ≈0.009 frames/cycle. The controller must
        // engage, retune the resampler DOWN, and converge its offset near
        // −100 ppm — i.e. the ratio tracks the actual device clock.
        let mut worker = worker_without_output(EndpointConfig::new("ep", None), 8192, 44_100);
        let production_per_cycle = 88.0 * 48_000.0 / 44_100.0; // ≈ 95.78
        let device_pop_per_cycle = production_per_cycle * (1.0 - 100e-6);
        let master_block = vec![0.5f32; 88 * 2];
        let ring = worker.ring.clone();

        // Prime: fill the ring to the controller's midpoint so the sim starts
        // at steady state rather than the resampler warm-up deficit (the
        // FFT resampler lags its first chunk, which would otherwise read as
        // a "fast device"). The controller is DISABLED while priming so the
        // warm-up deficit cannot slam it to the positive clamp; re-enabling
        // restores the nominal rate and a clean slate.
        worker.drift.set_enabled(false);
        let mut fill = 0usize;
        while fill < 4096 {
            worker.push_interleaved(&master_block, 2);
            fill = worker.ring.stats().available_frames;
        }
        worker.drift.set_enabled(true);

        let mut pop_acc = 0.0f64;
        let mut fill_peak = 0usize;
        let mut fill_min = usize::MAX;
        // ≈60 000 cycles ≈ 2 simulated minutes — plenty for the proportional
        // law to converge: the fill error grows at ~0.009 frames/cycle while
        // the low-passed correction pulls it to its ~63-frame equilibrium
        // (τ ≈ 7k cycles), after which the ratio sits on the device's true
        // clock (offset ≈ −94 ppm).
        for _ in 0..60_000 {
            worker.push_interleaved(&master_block, 2);
            // Device pops at its real (drifted) rate: accumulate the
            // fractional frame count and pop whole frames each cycle.
            pop_acc += device_pop_per_cycle;
            let want = pop_acc.floor() as usize;
            pop_acc -= want as f64;
            let take = want.min(128);
            let mut drain = [0.0f32; 128 * 2];
            ring.pop_interleaved(&mut drain[..take * 2], 2);
            let fill = worker.ring.stats().available_frames;
            fill_peak = fill_peak.max(fill);
            fill_min = fill_min.min(fill);
        }
        let offset = worker.drift.offset_ppm();
        assert!(
            offset < 0,
            "slow device → negative drift offset, got {offset} ppm"
        );
        assert!(
            (-300..-40).contains(&offset),
            "offset converges near the −100 ppm drift, got {offset} ppm"
        );
        // The ring never saturated (8192) and never crashed to empty: the
        // proportional correction balanced the drift long before the buffer
        // filled or drained (the fill settles at midpoint + ~63 frames).
        assert!(
            fill_peak < 8192,
            "ring fill bounded by drift correction, peaked at {fill_peak}"
        );
        assert!(
            fill_min > 2048,
            "ring never crashed empty (min fill {fill_min})"
        );
    }

    #[test]
    fn drift_correction_tracks_a_fast_device_clock() {
        // Mirror of the slow-device sim: the device's real clock runs 100 ppm
        // FASTER than nominal → the ring drains and the controller must speed
        // the slip up (positive offset ≈ +94 ppm).
        let mut worker = worker_without_output(EndpointConfig::new("ep", None), 8192, 44_100);
        let production_per_cycle = 88.0 * 48_000.0 / 44_100.0;
        let device_pop_per_cycle = production_per_cycle * (1.0 + 100e-6);
        let master_block = vec![0.5f32; 88 * 2];
        let ring = worker.ring.clone();

        worker.drift.set_enabled(false);
        let mut fill = 0usize;
        while fill < 4096 {
            worker.push_interleaved(&master_block, 2);
            fill = worker.ring.stats().available_frames;
        }
        worker.drift.set_enabled(true);

        let mut pop_acc = 0.0f64;
        let mut fill_peak = 0usize;
        let mut fill_min = usize::MAX;
        for _ in 0..60_000 {
            worker.push_interleaved(&master_block, 2);
            pop_acc += device_pop_per_cycle;
            let want = pop_acc.floor() as usize;
            pop_acc -= want as f64;
            let take = want.min(128);
            let mut drain = [0.0f32; 128 * 2];
            ring.pop_interleaved(&mut drain[..take * 2], 2);
            let fill = worker.ring.stats().available_frames;
            fill_peak = fill_peak.max(fill);
            fill_min = fill_min.min(fill);
        }
        let offset = worker.drift.offset_ppm();
        assert!(
            offset > 0,
            "fast device → positive drift offset, got {offset} ppm"
        );
        assert!(
            (40..300).contains(&offset),
            "offset converges near the +100 ppm drift, got {offset} ppm"
        );
        assert!(
            fill_peak < 8192 && fill_min > 2048,
            "ring bounded by drift correction (peak {fill_peak}, min {fill_min})"
        );
    }
}
