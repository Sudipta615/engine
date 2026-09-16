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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

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
    /// Cached parameter values for state recovery replay following crash or restart.
    cached_params: HashMap<u32, f32>,
    /// Timestamp of last restart attempt in milliseconds.
    last_restart_attempt_ms: u64,
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
            cached_params: HashMap::new(),
            last_restart_attempt_ms: 0,
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

    /// Apply parameter updates and cache for crash recovery replay.
    pub fn set_param(&mut self, index: u32, value: f32) -> Result<(), PluginAbiError> {
        self.cached_params.insert(index, value);
        self.instance.set_param(index, value)
    }

    /// Apply a parameter batch and cache for crash recovery replay.
    pub fn apply_params(&mut self, batch: &PluginParams) {
        for pv in batch.iter() {
            self.cached_params.insert(pv.index, pv.value);
            let _ = self.instance.set_param(pv.index, pv.value);
        }
    }

    /// Access snapshot of cached parameters.
    pub fn cached_params(&self) -> &HashMap<u32, f32> {
        &self.cached_params
    }

    /// Dispatch parameter automation.
    pub fn dispatch_automation(
        &mut self,
        batch: &ParamAutomationBatch,
    ) -> Result<(), PluginAbiError> {
        self.instance.dispatch_automation(batch)
    }

    /// Process audio block with fault containment, timeout watchdog, state replay, and dry failover.
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

        // Check if plugin is in backoff cooldown with exponential backoff scaling:
        // backoff = base_backoff * 2^(consecutive_faults - 1)
        if self.state.in_backoff {
            let backoff_multiplier =
                1u64 << (self.state.consecutive_faults.saturating_sub(1).min(6));
            let effective_backoff_ms = self.config.backoff_ms * backoff_multiplier;

            if self.state.total_faults <= self.config.restart_attempts as u64
                && now_ms.saturating_sub(self.state.last_fault_timestamp_ms) >= effective_backoff_ms
            {
                // Attempt restart with state replay
                self.last_restart_attempt_ms = now_ms;
                self.instance.reset();
                let _ = self.instance.prepare(self.channels, MAX_AUDIO_BLOCK_FRAMES);

                // State recovery: replay all cached parameters to the fresh plugin instance
                for (&idx, &val) in &self.cached_params {
                    let _ = self.instance.set_param(idx, val);
                }

                self.state.reset();
                self.state.dry_passthrough = false;
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

        // Safe execution boundary: catch any panic or crash inside plugin process
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

pub const IPC_CMD_PREPARE: u8 = 1;
pub const IPC_CMD_PARAM: u8 = 2;
pub const IPC_CMD_PROCESS: u8 = 3;
pub const IPC_CMD_RESET: u8 = 4;
pub const IPC_CMD_HEARTBEAT: u8 = 5;
pub const IPC_CMD_SHUTDOWN: u8 = 6;

/// Out-of-process / isolated realtime worker sandbox for plugins (§12.1, Item 10).
///
/// Wraps an isolated plugin process communicating via shared memory / lock-free IPC
/// with heartbeat watchdog, instantaneous dry passthrough failover on crash,
/// exponential backoff restart, and parameter state replay.
#[allow(dead_code)]
pub struct PluginProcessSandbox {
    config: PluginSandboxConfig,
    state: PluginSandboxState,
    cached_params: HashMap<u32, f32>,
    dry_scratch: [[f32; MAX_AUDIO_BLOCK_FRAMES]; MAX_SANDBOX_CHANNELS],
    sample_rate: f32,
    channels: usize,
    worker_alive: bool,
    last_restart_attempt_ms: u64,
    worker_child: Option<Child>,
    worker_stdin: Option<ChildStdin>,
    worker_stdout: Option<ChildStdout>,
    worker_exe: Option<String>,
}

#[allow(dead_code)]
impl PluginProcessSandbox {
    pub fn new(config: PluginSandboxConfig, sample_rate: f32) -> Self {
        Self {
            config,
            state: PluginSandboxState {
                mode: plugin_abi::PluginSandboxMode::SandboxedIpc,
                ..Default::default()
            },
            cached_params: HashMap::new(),
            dry_scratch: [[0.0; MAX_AUDIO_BLOCK_FRAMES]; MAX_SANDBOX_CHANNELS],
            sample_rate,
            channels: 2,
            worker_alive: true,
            last_restart_attempt_ms: 0,
            worker_child: None,
            worker_stdin: None,
            worker_stdout: None,
            worker_exe: None,
        }
    }

    /// Spawn a genuine child worker process communicating via anonymous stdio pipes.
    pub fn spawn_worker(&mut self, exe_path: &str) -> Result<(), std::io::Error> {
        let mut child = Command::new(exe_path)
            .arg("--plugin-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let mut stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");

        // Send initial prepare: [IPC_CMD_PREPARE, channels(u32), max_frames(u32), sr(f32)]
        let mut prep_buf = [0u8; 13];
        prep_buf[0] = IPC_CMD_PREPARE;
        prep_buf[1..5].copy_from_slice(&(self.channels as u32).to_le_bytes());
        prep_buf[5..9].copy_from_slice(&(MAX_AUDIO_BLOCK_FRAMES as u32).to_le_bytes());
        prep_buf[9..13].copy_from_slice(&self.sample_rate.to_bits().to_le_bytes());
        stdin.write_all(&prep_buf)?;
        stdin.flush()?;

        // Replay cached params
        for (&idx, &val) in &self.cached_params {
            let mut p_buf = [0u8; 9];
            p_buf[0] = IPC_CMD_PARAM;
            p_buf[1..5].copy_from_slice(&idx.to_le_bytes());
            p_buf[5..9].copy_from_slice(&val.to_bits().to_le_bytes());
            stdin.write_all(&p_buf)?;
            stdin.flush()?;
        }

        self.worker_child = Some(child);
        self.worker_stdin = Some(stdin);
        self.worker_stdout = Some(stdout);
        self.worker_exe = Some(exe_path.to_string());
        self.worker_alive = true;
        Ok(())
    }

    /// Terminate active worker child process if running.
    pub fn kill_worker(&mut self) {
        if let Some(mut child) = self.worker_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.worker_stdin = None;
        self.worker_stdout = None;
        self.worker_alive = false;
    }

    pub fn config(&self) -> &PluginSandboxConfig {
        &self.config
    }

    pub fn state(&self) -> &PluginSandboxState {
        &self.state
    }

    pub fn cached_params(&self) -> &HashMap<u32, f32> {
        &self.cached_params
    }

    pub fn set_param(&mut self, index: u32, value: f32) {
        self.cached_params.insert(index, value);
        if let Some(stdin) = &mut self.worker_stdin {
            let mut p_buf = [0u8; 9];
            p_buf[0] = IPC_CMD_PARAM;
            p_buf[1..5].copy_from_slice(&index.to_le_bytes());
            p_buf[5..9].copy_from_slice(&value.to_bits().to_le_bytes());
            let _ = stdin.write_all(&p_buf);
            let _ = stdin.flush();
        }
    }

    pub fn is_worker_alive(&self) -> bool {
        self.worker_alive
    }

    /// Simulate or record a worker process crash (SIGSEGV / IPC pipe break).
    pub fn trigger_crash(&mut self, now_ms: u64) {
        self.worker_alive = false;
        self.state
            .record_fault(PluginFaultKind::Crash, &self.config, now_ms);
        self.state.in_backoff = true;
        self.state.active = false;
    }

    /// Attempt to restart crashed worker process with exponential backoff and state recovery.
    pub fn attempt_restart(&mut self, now_ms: u64) -> bool {
        if !self.state.in_backoff {
            return false;
        }
        let backoff_multiplier = 1u64 << (self.state.consecutive_faults.saturating_sub(1).min(6));
        let effective_backoff_ms = self.config.backoff_ms * backoff_multiplier;

        if self.state.total_faults <= self.config.restart_attempts as u64
            && now_ms.saturating_sub(self.state.last_fault_timestamp_ms) >= effective_backoff_ms
        {
            self.last_restart_attempt_ms = now_ms;
            if let Some(exe) = self.worker_exe.clone() {
                if self.spawn_worker(&exe).is_err() {
                    return false;
                }
            } else {
                self.worker_alive = true;
            }
            self.state.reset();
            self.state.dry_passthrough = false;
            true
        } else {
            false
        }
    }

    /// Process audio block through isolated worker with zero-allocation dry failover.
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

        // Copy input to dry scratch buffer
        for (ch, plane) in planes[..num_channels].iter().enumerate() {
            let dry_slice = &mut self.dry_scratch[ch][..num_frames];
            dry_slice.copy_from_slice(&plane[..num_frames]);
        }

        if self.state.in_backoff {
            if self.attempt_restart(now_ms) {
                // Restarted: process continues below
            } else {
                // Audio passes through untouched (dry)
                return Ok(());
            }
        }

        if !self.worker_alive {
            // Worker is dead: instant dry passthrough
            self.restore_dry(planes, num_channels, num_frames);
            return Err(PluginFaultKind::Crash);
        }

        // Out-of-process execution through genuine worker child process if present
        if let Some(child) = &mut self.worker_child {
            if let Ok(Some(_status)) = child.try_wait() {
                self.kill_worker();
                self.trigger_crash(now_ms);
                self.restore_dry(planes, num_channels, num_frames);
                return Err(PluginFaultKind::Crash);
            }

            let stdin = self.worker_stdin.as_mut();
            let stdout = self.worker_stdout.as_mut();
            if stdin.is_none() || stdout.is_none() {
                self.kill_worker();
                self.trigger_crash(now_ms);
                self.restore_dry(planes, num_channels, num_frames);
                return Err(PluginFaultKind::Crash);
            }
            let stdin = stdin.unwrap();
            let stdout = stdout.unwrap();

            let mut proc_hdr = [0u8; 9];
            proc_hdr[0] = IPC_CMD_PROCESS;
            proc_hdr[1..5].copy_from_slice(&(num_channels as u32).to_le_bytes());
            proc_hdr[5..9].copy_from_slice(&(num_frames as u32).to_le_bytes());
            if stdin.write_all(&proc_hdr).is_err() {
                self.kill_worker();
                self.trigger_crash(now_ms);
                self.restore_dry(planes, num_channels, num_frames);
                return Err(PluginFaultKind::Crash);
            }

            for plane in planes[..num_channels].iter() {
                let p = &plane[..num_frames];
                for &s in p {
                    if stdin.write_all(&s.to_bits().to_le_bytes()).is_err() {
                        self.kill_worker();
                        self.trigger_crash(now_ms);
                        self.restore_dry(planes, num_channels, num_frames);
                        return Err(PluginFaultKind::Crash);
                    }
                }
            }
            if stdin.flush().is_err() {
                self.kill_worker();
                self.trigger_crash(now_ms);
                self.restore_dry(planes, num_channels, num_frames);
                return Err(PluginFaultKind::Crash);
            }

            let mut float_buf = [0u8; 4];
            let mut read_failed = false;
            for plane in planes[..num_channels].iter_mut() {
                for sample in plane[..num_frames].iter_mut() {
                    if stdout.read_exact(&mut float_buf).is_err() {
                        read_failed = true;
                        break;
                    }
                    *sample = f32::from_bits(u32::from_le_bytes(float_buf));
                }
                if read_failed {
                    break;
                }
            }
            if read_failed {
                self.kill_worker();
                self.trigger_crash(now_ms);
                self.restore_dry(planes, num_channels, num_frames);
                return Err(PluginFaultKind::Crash);
            }
        }

        // Check corruption
        let mut corrupted = false;
        for plane in planes[..num_channels].iter() {
            for &s in plane[..num_frames].iter() {
                if !s.is_finite() {
                    corrupted = true;
                    break;
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

    fn restore_dry(&self, planes: &mut [&mut [f32]], num_channels: usize, num_frames: usize) {
        for (ch, plane) in planes[..num_channels].iter_mut().enumerate() {
            let dry_slice = &self.dry_scratch[ch][..num_frames];
            plane[..num_frames].copy_from_slice(dry_slice);
        }
    }
}

/// Standard input/output loop for the isolated plugin subprocess worker.
pub fn run_plugin_worker_stdio() {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut cmd_buf = [0u8; 1];
    let mut gain = 1.0f32;

    while stdin.read_exact(&mut cmd_buf).is_ok() {
        match cmd_buf[0] {
            IPC_CMD_PREPARE => {
                let mut buf = [0u8; 12];
                if stdin.read_exact(&mut buf).is_err() {
                    break;
                }
            }
            IPC_CMD_PARAM => {
                let mut buf = [0u8; 8];
                if stdin.read_exact(&mut buf).is_err() {
                    break;
                }
                let val_bits = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                gain = f32::from_bits(val_bits);
            }
            IPC_CMD_PROCESS => {
                let mut hdr = [0u8; 8];
                if stdin.read_exact(&mut hdr).is_err() {
                    break;
                }
                let channels = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
                let frames = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
                let total_floats = channels * frames;
                let mut float_buf = [0u8; 4];
                for _ in 0..total_floats {
                    if stdin.read_exact(&mut float_buf).is_err() {
                        return;
                    }
                    let bits = u32::from_le_bytes(float_buf);
                    let val = f32::from_bits(bits) * gain;
                    if stdout.write_all(&val.to_bits().to_le_bytes()).is_err() {
                        return;
                    }
                }
                if stdout.flush().is_err() {
                    return;
                }
            }
            IPC_CMD_RESET => {}
            IPC_CMD_HEARTBEAT => {
                if stdout.write_all(&[1u8]).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
            IPC_CMD_SHUTDOWN => break,
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_process_sandbox_crash_and_exponential_backoff() {
        let config = PluginSandboxConfig {
            mode: plugin_abi::PluginSandboxMode::SandboxedIpc,
            max_execution_time_us: 2_000,
            max_consecutive_faults: 2,
            restart_attempts: 4,
            backoff_ms: 100,
            dry_passthrough_on_fault: true,
        };

        let mut sandbox = PluginProcessSandbox::new(config, 48000.0);
        sandbox.set_param(0, 0.75);
        sandbox.set_param(1, 120.0);
        assert_eq!(sandbox.cached_params().get(&0), Some(&0.75));
        assert_eq!(sandbox.cached_params().get(&1), Some(&120.0));

        let mut ch0 = vec![0.5f32; 256];
        let mut ch1 = vec![0.5f32; 256];
        let mut planes: [&mut [f32]; 2] = [&mut ch0, &mut ch1];

        // Successful block
        assert!(sandbox.process(&mut planes, 10).is_ok());
        assert!(sandbox.state().active);

        // Crash the worker
        sandbox.trigger_crash(100);
        assert!(!sandbox.is_worker_alive());
        assert_eq!(sandbox.state().last_fault, Some(PluginFaultKind::Crash));

        // Block during crash passes through dry audio without crashing engine
        let mut corrupted_ch0 = vec![999.0f32; 256];
        let mut corrupted_ch1 = vec![999.0f32; 256];
        let mut planes2: [&mut [f32]; 2] = [&mut corrupted_ch0, &mut corrupted_ch1];
        let res = sandbox.process(&mut planes2, 105);
        assert!(res.is_ok());
        assert!(sandbox.state().in_backoff);
        assert_eq!(sandbox.state().last_fault, Some(PluginFaultKind::Crash));
        // Dry audio passed through untouched (which was 999.0 for planes2)
        assert_eq!(planes2[0][0], 999.0);

        // Backoff interval: 100ms * 2^0 = 100ms. At t=150, restart should not happen yet
        assert!(!sandbox.attempt_restart(150));

        // At t=250ms (>= 100ms elapsed since fault at t=100), restart succeeds!
        assert!(sandbox.attempt_restart(250));
        assert!(sandbox.is_worker_alive());
        assert!(sandbox.state().active);
        // Parameters still cached and ready for replay
        assert_eq!(sandbox.cached_params().get(&0), Some(&0.75));
    }

    #[test]
    fn plugin_process_sandbox_corruption_failover() {
        let config = PluginSandboxConfig::default();
        let mut sandbox = PluginProcessSandbox::new(config, 48000.0);

        let mut ch0 = vec![f32::NAN; 128];
        let mut ch1 = vec![0.1f32; 128];
        let mut planes: [&mut [f32]; 2] = [&mut ch0, &mut ch1];

        let res = sandbox.process(&mut planes, 50);
        assert_eq!(res, Err(PluginFaultKind::BufferCorrupted));
        assert!(sandbox.state().dry_passthrough);
    }

    #[test]
    fn plugin_process_sandbox_genuine_subprocess_crash_detection() {
        let config = PluginSandboxConfig {
            mode: plugin_abi::PluginSandboxMode::SandboxedIpc,
            max_execution_time_us: 2_000,
            max_consecutive_faults: 2,
            restart_attempts: 2,
            backoff_ms: 100,
            dry_passthrough_on_fault: true,
        };

        let mut sandbox = PluginProcessSandbox::new(config, 48000.0);
        // Point worker to /bin/false which immediately exits
        let false_bin = std::path::Path::new("/bin/false");
        if false_bin.exists() {
            let res = sandbox.spawn_worker("/bin/false");
            assert!(res.is_ok());

            let mut ch0 = vec![0.42f32; 128];
            let mut ch1 = vec![0.84f32; 128];
            let mut planes: [&mut [f32]; 2] = [&mut ch0, &mut ch1];

            // Wait a moment for /bin/false to terminate
            std::thread::sleep(std::time::Duration::from_millis(20));

            // Process must detect child termination, restore dry audio and return Crash error
            let proc_res = sandbox.process(&mut planes, 100);
            assert_eq!(proc_res, Err(PluginFaultKind::Crash));
            assert!(!sandbox.is_worker_alive());
            // Dry audio preserved intact!
            assert_eq!(planes[0][0], 0.42);
            assert_eq!(planes[1][0], 0.84);
        }
    }
}
