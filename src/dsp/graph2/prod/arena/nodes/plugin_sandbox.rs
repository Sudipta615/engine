//! Realtime-safe plugin sandbox wrapper (§12.1, Item 32).
//!
//! Provides isolated, fault-tolerant execution around `plugin_abi::PluginInstance`
//! with a execution watchdog timer, panic-catching boundary, sample corruption
//! verification, instantaneous dry-passthrough failover, and auto-restart backoff.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

use plugin_abi::{
    ParamAutomationBatch, PluginAbiError, PluginFaultKind, PluginInstance, PluginParams,
    PluginSandboxConfig, PluginSandboxState, TransportInfo,
};

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::dsp::graph2::prod::arena::node::DspNode;
use crate::dsp::pipeline::{DspStageCapability, StageChannelSupport, StagePrecision};

/// Maximum channels preserved in the realtime dry-passthrough scratch buffer.
pub const MAX_SANDBOX_CHANNELS: usize = 8;

/// Sandboxed wrapper around a single `PluginInstance`.
#[allow(dead_code)]
pub struct SandboxedPluginInstance {
    instance: PluginInstance,
    config: PluginSandboxConfig,
    state: PluginSandboxState,
    /// Scratch buffer storing dry input channels for zero-allocation failover restoration.
    dry_scratch: [[f32; MAX_AUDIO_BLOCK_FRAMES]; MAX_SANDBOX_CHANNELS],
    sample_rate: f32,
    channels: usize,
}

#[allow(dead_code)]
impl SandboxedPluginInstance {
    /// Wrap a plugin instance with sandboxing configuration.
    pub fn new(instance: PluginInstance, config: PluginSandboxConfig, sample_rate: f32) -> Self {
        Self {
            instance,
            config,
            state: PluginSandboxState::default(),
            dry_scratch: [[0.0; MAX_AUDIO_BLOCK_FRAMES]; MAX_SANDBOX_CHANNELS],
            sample_rate,
            channels: 2,
        }
    }

    /// Reference to sandbox configuration.
    pub fn config(&self) -> &PluginSandboxConfig {
        &self.config
    }

    /// Mutable reference to sandbox configuration.
    pub fn config_mut(&mut self) -> &mut PluginSandboxConfig {
        &mut self.config
    }

    /// Reference to sandbox runtime health and telemetry state.
    pub fn state(&self) -> &PluginSandboxState {
        &self.state
    }

    /// Mutable reference to sandbox runtime health state.
    pub fn state_mut(&mut self) -> &mut PluginSandboxState {
        &mut self.state
    }

    /// Access the underlying plugin instance.
    pub fn instance(&self) -> &PluginInstance {
        &self.instance
    }

    /// Mutable access to the underlying plugin instance.
    pub fn instance_mut(&mut self) -> &mut PluginInstance {
        &mut self.instance
    }

    /// Prepare the sandboxed instance for playback.
    pub fn prepare(
        &mut self,
        max_channels: usize,
        max_block_frames: usize,
    ) -> Result<(), PluginAbiError> {
        self.channels = max_channels.min(MAX_SANDBOX_CHANNELS);
        self.instance.prepare(max_channels, max_block_frames)
    }

    /// Reset internal state and underlying plugin.
    pub fn reset(&mut self) {
        self.instance.reset();
        self.state.reset();
    }

    /// Set bypass mode on the underlying instance.
    pub fn set_bypass(&mut self, bypass: bool) {
        self.instance.set_bypass(bypass);
    }

    /// Set transport metadata.
    pub fn set_transport(&mut self, transport: TransportInfo) {
        self.instance.set_transport(transport);
    }

    /// Apply parameter updates.
    pub fn set_param(&mut self, index: u32, value: f32) -> Result<(), PluginAbiError> {
        self.instance.set_param(index, value)
    }

    /// Apply a parameter batch.
    pub fn apply_params(&mut self, batch: &PluginParams) {
        for pv in batch.iter() {
            let _ = self.instance.set_param(pv.index, pv.value);
        }
    }

    /// Dispatch parameter automation.
    pub fn dispatch_automation(
        &mut self,
        batch: &ParamAutomationBatch,
    ) -> Result<(), PluginAbiError> {
        self.instance.dispatch_automation(batch)
    }

    /// Process audio block with fault containment, timeout watchdog, and dry failover.
    pub fn process(
        &mut self,
        planes: &mut [&mut [f32]],
        now_ms: u64,
    ) -> Result<(), PluginFaultKind> {
        if planes.is_empty() {
            return Ok(());
        }

        let num_channels = planes.len().min(MAX_SANDBOX_CHANNELS);
        let num_frames = planes[0].len().min(MAX_AUDIO_BLOCK_FRAMES);

        // Check if plugin is in backoff cooldown
        if self.state.in_backoff {
            if self.state.can_attempt_restart(&self.config, now_ms) {
                // Attempt restart
                self.instance.reset();
                let _ = self.instance.prepare(self.channels, MAX_AUDIO_BLOCK_FRAMES);
                self.state.reset();
            } else {
                // In backoff: audio passes through untouched (dry)
                return Ok(());
            }
        }

        // If bypass / dry-passthrough is requested, pass through without calling plugin
        if self.state.dry_passthrough {
            return Ok(());
        }

        // Copy input planes into zero-alloc scratch buffer for instant failover recovery
        for (ch, plane) in planes[..num_channels].iter().enumerate() {
            let dry_slice = &mut self.dry_scratch[ch][..num_frames];
            dry_slice.copy_from_slice(&plane[..num_frames]);
        }

        let start_time = Instant::now();

        // Safe execution boundary: catch any panic inside plugin process
        let process_result = catch_unwind(AssertUnwindSafe(|| unsafe {
            self.instance.process(planes)
        }));

        let elapsed_us = start_time.elapsed().as_micros() as u64;

        match process_result {
            Err(_) => {
                // Panic occurred in plugin: restore pristine dry input immediately
                self.restore_dry(planes, num_channels, num_frames);
                self.state
                    .record_fault(PluginFaultKind::Panic, &self.config, now_ms);
                Err(PluginFaultKind::Panic)
            }
            Ok(Err(_abi_err)) => {
                self.restore_dry(planes, num_channels, num_frames);
                self.state
                    .record_fault(PluginFaultKind::Crash, &self.config, now_ms);
                Err(PluginFaultKind::Crash)
            }
            Ok(Ok(())) => {
                // Check execution watchdog
                if elapsed_us > self.config.max_execution_time_us {
                    if self.config.dry_passthrough_on_fault {
                        self.restore_dry(planes, num_channels, num_frames);
                    }
                    self.state
                        .record_fault(PluginFaultKind::Timeout, &self.config, now_ms);
                    return Err(PluginFaultKind::Timeout);
                }

                // Check sample corruption (NaN / Inf)
                let mut corrupted = false;
                'outer: for plane in planes[..num_channels].iter() {
                    for &s in plane[..num_frames].iter() {
                        if !s.is_finite() {
                            corrupted = true;
                            break 'outer;
                        }
                    }
                }

                if corrupted {
                    self.restore_dry(planes, num_channels, num_frames);
                    self.state
                        .record_fault(PluginFaultKind::BufferCorrupted, &self.config, now_ms);
                    return Err(PluginFaultKind::BufferCorrupted);
                }

                self.state.record_success();
                Ok(())
            }
        }
    }

    /// Restores original uncorrupted dry audio back to the processing planes.
    fn restore_dry(&self, planes: &mut [&mut [f32]], num_channels: usize, num_frames: usize) {
        for (ch, plane) in planes[..num_channels].iter_mut().enumerate() {
            let dry_slice = &self.dry_scratch[ch][..num_frames];
            plane[..num_frames].copy_from_slice(dry_slice);
        }
    }
}

/// A standalone DspNode hosting a sandboxed plugin.
#[allow(dead_code)]
pub struct PluginSandboxNode {
    sandbox: SandboxedPluginInstance,
    enabled: bool,
    sample_rate: f32,
    max_channels: usize,
    now_ms_counter: u64,
}

#[allow(dead_code)]
impl PluginSandboxNode {
    /// Create a new sandboxed plugin node.
    pub fn new(instance: PluginInstance, config: PluginSandboxConfig, sample_rate: f32) -> Self {
        Self {
            sandbox: SandboxedPluginInstance::new(instance, config, sample_rate),
            enabled: true,
            sample_rate,
            max_channels: 2,
            now_ms_counter: 0,
        }
    }

    /// Access internal sandbox.
    pub fn sandbox(&self) -> &SandboxedPluginInstance {
        &self.sandbox
    }

    /// Mutable access to internal sandbox.
    pub fn sandbox_mut(&mut self) -> &mut SandboxedPluginInstance {
        &mut self.sandbox
    }

    /// Set node enabled status.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl DspNode for PluginSandboxNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "plugin_sandbox",
            channel_support: StageChannelSupport::AllChannels,
            position: "insert seam",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.enabled && self.sandbox.state().active
    }

    fn latency_samples(&self) -> usize {
        0
    }

    fn tail_samples(&self) -> usize {
        0
    }

    fn reset(&mut self) {
        self.sandbox.reset();
    }

    fn prepare(&mut self, sample_rate: f32, max_channels: usize) {
        self.sample_rate = sample_rate;
        self.max_channels = max_channels;
        let _ = self.sandbox.prepare(max_channels, MAX_AUDIO_BLOCK_FRAMES);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if !self.enabled {
            return;
        }
        // Approximate time counter increment based on block frames (e.g., 256 samples @ 48kHz ~ 5.3ms)
        let frames = planes.first().map(|p| p.len()).unwrap_or(0);
        let block_ms = ((frames as f64 / self.sample_rate as f64) * 1000.0).max(1.0) as u64;
        self.now_ms_counter += block_ms;

        let _ = self.sandbox.process(planes, self.now_ms_counter);
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if !self.enabled {
            return;
        }
        let frames = planes.first().map(|p| p.len()).unwrap_or(0);
        if frames == 0 || frames > MAX_AUDIO_BLOCK_FRAMES {
            return;
        }
        let mut l32 = [0f32; MAX_AUDIO_BLOCK_FRAMES];
        let mut r32 = [0f32; MAX_AUDIO_BLOCK_FRAMES];
        if let Some(p) = planes.first() {
            for (d, s) in l32.iter_mut().zip(p.iter()) {
                *d = *s as f32;
            }
        }
        if let Some(p) = planes.get(1) {
            for (d, s) in r32.iter_mut().zip(p.iter()) {
                *d = *s as f32;
            }
        }
        {
            let mut pair: [&mut [f32]; 2] = [&mut l32[..frames], &mut r32[..frames]];
            self.process_block_f32(&mut pair);
        }
        if let Some(dst) = planes.first_mut() {
            for (d, s) in dst.iter_mut().zip(l32[..frames].iter()) {
                *d = *s as f64;
            }
        }
        if let Some(dst) = planes.get_mut(1) {
            for (d, s) in dst.iter_mut().zip(r32[..frames].iter()) {
                *d = *s as f64;
            }
        }
    }
}
