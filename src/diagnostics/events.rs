//! Structured diagnostic events and real-time safe event ring (§6.5, Item 16).
//!
//! Provides structured, machine-readable diagnostic event reporting across all
//! engine subsystems (DSP, Output, Clock, Plugin, Graph, Decoder, Spatial, Security).
//!
//! Includes a bounded, zero-allocation lock-free ring buffer ([`RealtimeDiagnosticQueue`])
//! allowing real-time audio threads to emit diagnostic events safely without heap
//! allocation or blocking synchronization.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::DiagnosticKind;

/// Severity classification for structured diagnostic events.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    #[default]
    Info,
    Warning,
    Error,
    Critical,
}

impl DiagnosticSeverity {
    pub fn code(self) -> &'static str {
        match self {
            DiagnosticSeverity::Info => "info",
            DiagnosticSeverity::Warning => "warning",
            DiagnosticSeverity::Error => "error",
            DiagnosticSeverity::Critical => "critical",
        }
    }
}

impl std::fmt::Display for DiagnosticSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// Fully structured diagnostic event (§6.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    /// Unix timestamp in nanoseconds when the incident occurred.
    pub timestamp_ns: u64,
    /// Active DSP graph generation at incident time.
    pub graph_generation: u64,
    /// Identifier of the specific node responsible (if applicable).
    pub node_id: Option<u32>,
    /// Coarse subsystem category.
    pub category: DiagnosticKind,
    /// Severity classification.
    pub severity: DiagnosticSeverity,
    /// Whether the incident originated on the real-time audio thread.
    pub realtime: bool,
    /// Whether the subsystem automatically recovered without halting playback.
    pub recoverable: bool,
    /// Stable machine-readable error or incident code.
    pub code: String,
    /// Detailed diagnostic description.
    pub message: String,
}

impl DiagnosticEvent {
    pub fn new(
        category: DiagnosticKind,
        severity: DiagnosticSeverity,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let timestamp_ns = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        Self {
            timestamp_ns,
            graph_generation: 0,
            node_id: None,
            category,
            severity,
            realtime: false,
            recoverable: true,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn with_realtime(mut self, realtime: bool) -> Self {
        self.realtime = realtime;
        self
    }

    pub fn with_node(mut self, node_id: u32) -> Self {
        self.node_id = Some(node_id);
        self
    }

    pub fn with_generation(mut self, generation: u64) -> Self {
        self.graph_generation = generation;
        self
    }

    pub fn with_recoverable(mut self, recoverable: bool) -> Self {
        self.recoverable = recoverable;
        self
    }
}

/// Fixed maximum length for inlined real-time diagnostic message bytes.
pub const MAX_RT_DIAGNOSTIC_MSG_LEN: usize = 96;

/// Bounded inline event structure stored directly inside the lock-free ring.
#[derive(Clone, Copy)]
pub struct RawDiagnosticEvent {
    pub timestamp_ns: u64,
    pub graph_generation: u64,
    pub node_id: Option<u32>,
    pub category: DiagnosticKind,
    pub severity: DiagnosticSeverity,
    pub realtime: bool,
    pub recoverable: bool,
    pub code: &'static str,
    pub message: [u8; MAX_RT_DIAGNOSTIC_MSG_LEN],
    pub message_len: u8,
}

impl RawDiagnosticEvent {
    pub fn new(
        category: DiagnosticKind,
        severity: DiagnosticSeverity,
        code: &'static str,
        message_str: &str,
    ) -> Self {
        let mut msg_bytes = [0u8; MAX_RT_DIAGNOSTIC_MSG_LEN];
        let bytes = message_str.as_bytes();
        let len = bytes.len().min(MAX_RT_DIAGNOSTIC_MSG_LEN);
        msg_bytes[..len].copy_from_slice(&bytes[..len]);

        Self {
            timestamp_ns: 0,
            graph_generation: 0,
            node_id: None,
            category,
            severity,
            realtime: true,
            recoverable: true,
            code,
            message: msg_bytes,
            message_len: len as u8,
        }
    }

    pub fn to_event(&self) -> DiagnosticEvent {
        let msg = std::str::from_utf8(&self.message[..self.message_len as usize])
            .unwrap_or("<invalid utf-8>");
        DiagnosticEvent {
            timestamp_ns: self.timestamp_ns,
            graph_generation: self.graph_generation,
            node_id: self.node_id,
            category: self.category,
            severity: self.severity,
            realtime: self.realtime,
            recoverable: self.recoverable,
            code: self.code.to_string(),
            message: msg.to_string(),
        }
    }
}

/// Capacity of the bounded real-time diagnostic event queue.
pub const DIAGNOSTIC_QUEUE_CAPACITY: usize = 64;

/// Real-time safe lock-free diagnostic event ring buffer.
pub struct RealtimeDiagnosticQueue {
    buffer: [std::cell::UnsafeCell<Option<RawDiagnosticEvent>>; DIAGNOSTIC_QUEUE_CAPACITY],
    write_head: AtomicUsize,
    read_tail: AtomicUsize,
}

// SAFETY: Single-writer multiple-reader or serialized SPSC audio->telemetry flow.
unsafe impl Sync for RealtimeDiagnosticQueue {}
unsafe impl Send for RealtimeDiagnosticQueue {}

impl Default for RealtimeDiagnosticQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl RealtimeDiagnosticQueue {
    pub fn new() -> Self {
        // Initialize array of UnsafeCell
        let buffer = std::array::from_fn(|_| std::cell::UnsafeCell::new(None));
        Self {
            buffer,
            write_head: AtomicUsize::new(0),
            read_tail: AtomicUsize::new(0),
        }
    }

    /// Push an event from the real-time audio thread without heap allocation.
    /// Drops the oldest unread event if queue is completely saturated.
    pub fn push(&self, event: RawDiagnosticEvent) -> bool {
        let head = self.write_head.load(Ordering::Relaxed);
        let tail = self.read_tail.load(Ordering::Acquire);

        // Advance tail if queue is full to preserve bounded memory
        if head.wrapping_sub(tail) >= DIAGNOSTIC_QUEUE_CAPACITY {
            self.read_tail
                .store(tail.wrapping_add(1), Ordering::Release);
        }

        let idx = head % DIAGNOSTIC_QUEUE_CAPACITY;
        unsafe {
            *self.buffer[idx].get() = Some(event);
        }
        self.write_head
            .store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Drain all queued diagnostic events. Intended for the control/telemetry thread.
    pub fn drain(&self) -> Vec<DiagnosticEvent> {
        let mut events = Vec::new();
        let head = self.write_head.load(Ordering::Acquire);
        let mut tail = self.read_tail.load(Ordering::Relaxed);

        while tail != head {
            let idx = tail % DIAGNOSTIC_QUEUE_CAPACITY;
            let raw_opt = unsafe { (*self.buffer[idx].get()).take() };
            if let Some(raw) = raw_opt {
                events.push(raw.to_event());
            }
            tail = tail.wrapping_add(1);
        }
        self.read_tail.store(tail, Ordering::Release);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_construction_and_serialization() {
        let evt = DiagnosticEvent::new(
            DiagnosticKind::Dsp,
            DiagnosticSeverity::Warning,
            "sample_rate_mismatch",
            "Hardware rate 48000 does not match source rate 44100",
        )
        .with_node(5)
        .with_generation(2)
        .with_realtime(true);

        assert_eq!(evt.node_id, Some(5));
        assert_eq!(evt.graph_generation, 2);
        assert!(evt.realtime);

        let json = serde_json::to_string(&evt).unwrap();
        let back: DiagnosticEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, evt);
    }

    #[test]
    fn realtime_queue_push_and_drain() {
        let queue = RealtimeDiagnosticQueue::new();
        assert!(queue.drain().is_empty());

        let raw = RawDiagnosticEvent::new(
            DiagnosticKind::Clock,
            DiagnosticSeverity::Error,
            "xrun_detected",
            "Output FIFO starvation observed",
        );
        assert!(queue.push(raw));

        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].category, DiagnosticKind::Clock);
        assert_eq!(drained[0].code, "xrun_detected");
        assert_eq!(drained[0].message, "Output FIFO starvation observed");

        // Subsequent drain is empty
        assert!(queue.drain().is_empty());
    }
}
