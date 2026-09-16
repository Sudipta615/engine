//! Plugin isolation and sandboxing models (§12.1, Item 32).
//!
//! Provides operational execution modes, structured fault classifications,
//! configuration tolerances, and state tracking for fault-tolerant plugin hosting.

#[cfg(feature = "serde-types")]
use serde::{Deserialize, Serialize};

/// Operational isolation mode for hosting DSP plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde-types", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde-types", serde(rename_all = "snake_case"))]
pub enum PluginSandboxMode {
    /// Plugin executes directly on the audio thread inside the host plan step.
    /// Lowest possible latency; fault isolation relies on panic catching and watchdog guards.
    #[default]
    InProcessTrusted,
    /// Plugin executes on a dedicated isolated realtime worker thread connected
    /// via lock-free SPSC channels. Prevents plugin thread hangs from blocking the host audio thread.
    IsolatedWorker,
    /// Plugin executes in a separate sandboxed process communicating via shared memory / IPC.
    /// Full memory protection and crash immunity.
    SandboxedIpc,
}

/// Structured fault classification for plugin failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde-types", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde-types", serde(rename_all = "snake_case"))]
pub enum PluginFaultKind {
    /// Plugin triggered a Rust panic during execution.
    Panic,
    /// Plugin exceeded the allotted block execution time budget (watchdog trigger).
    Timeout,
    /// Plugin attempted forbidden heap allocation or lock acquisition on the realtime path.
    AllocationViolation,
    /// Plugin returned non-finite samples (NaN/Infinity) or buffer corrupted samples.
    BufferCorrupted,
    /// Plugin crashed, terminated unexpectedly, or IPC pipe dropped.
    Crash,
}

/// Configuration parameters for plugin execution sandboxing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-types", derive(Serialize, Deserialize))]
pub struct PluginSandboxConfig {
    /// Active sandboxing mode.
    pub mode: PluginSandboxMode,
    /// Maximum processing time allowed per block in microseconds before a timeout fault is triggered.
    pub max_execution_time_us: u64,
    /// Maximum number of consecutive faults before forcing bypass or backoff.
    pub max_consecutive_faults: u32,
    /// Number of restart attempts allowed before permanently disabling the plugin.
    pub restart_attempts: u32,
    /// Cooldown / backoff time in milliseconds before attempting to restart an errored plugin.
    pub backoff_ms: u64,
    /// If true, automatically switches to dry-passthrough bypass immediately upon fault.
    pub dry_passthrough_on_fault: bool,
}

impl Default for PluginSandboxConfig {
    fn default() -> Self {
        Self {
            mode: PluginSandboxMode::InProcessTrusted,
            max_execution_time_us: 5_000, // 5 ms block watchdog
            max_consecutive_faults: 3,
            restart_attempts: 5,
            backoff_ms: 1_000, // 1 second backoff
            dry_passthrough_on_fault: true,
        }
    }
}

/// Runtime telemetry and health state of a plugin sandbox instance.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-types", derive(Serialize, Deserialize))]
pub struct PluginSandboxState {
    /// Active sandbox execution mode.
    pub mode: PluginSandboxMode,
    /// Whether the plugin is currently running and processing audio.
    pub active: bool,
    /// Cumulative total faults encountered since instantiation.
    pub total_faults: u64,
    /// Number of consecutive faults without a successful block.
    pub consecutive_faults: u32,
    /// Most recent fault kind, if any.
    pub last_fault: Option<PluginFaultKind>,
    /// Whether the plugin is currently in backoff cooldown.
    pub in_backoff: bool,
    /// Timestamp (ms) of the last fault occurrence.
    pub last_fault_timestamp_ms: u64,
    /// Whether audio is currently bypassing the plugin in dry-passthrough mode.
    pub dry_passthrough: bool,
}

impl Default for PluginSandboxState {
    fn default() -> Self {
        Self {
            mode: PluginSandboxMode::InProcessTrusted,
            active: true,
            total_faults: 0,
            consecutive_faults: 0,
            last_fault: None,
            in_backoff: false,
            last_fault_timestamp_ms: 0,
            dry_passthrough: false,
        }
    }
}

impl PluginSandboxState {
    /// Records a fault occurrence and updates backoff and passthrough status.
    pub fn record_fault(
        &mut self,
        kind: PluginFaultKind,
        config: &PluginSandboxConfig,
        now_ms: u64,
    ) {
        self.total_faults += 1;
        self.consecutive_faults += 1;
        self.last_fault = Some(kind);
        self.last_fault_timestamp_ms = now_ms;

        if config.dry_passthrough_on_fault {
            self.dry_passthrough = true;
        }

        if self.consecutive_faults >= config.max_consecutive_faults {
            self.in_backoff = true;
            self.active = false;
        }
    }

    /// Records a successful block execution, resetting consecutive fault counters.
    pub fn record_success(&mut self) {
        self.consecutive_faults = 0;
        self.in_backoff = false;
        self.active = true;
    }

    /// Evaluates whether backoff cooldown has expired and a restart can be attempted.
    pub fn can_attempt_restart(&self, config: &PluginSandboxConfig, now_ms: u64) -> bool {
        if !self.in_backoff {
            return false;
        }
        if self.total_faults > config.restart_attempts as u64 {
            return false;
        }
        now_ms.saturating_sub(self.last_fault_timestamp_ms) >= config.backoff_ms
    }

    /// Resets the sandbox state to healthy initial parameters.
    pub fn reset(&mut self) {
        self.active = true;
        self.consecutive_faults = 0;
        self.last_fault = None;
        self.in_backoff = false;
        self.dry_passthrough = false;
    }
}
