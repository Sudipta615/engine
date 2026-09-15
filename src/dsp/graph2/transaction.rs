//! Transactional graph editing runtime (spec §5.2).
//!
//! Enforces atomic, fail-safe graph reconfiguration following the canonical pipeline:
//!
//! ```text
//! begin_transaction
//!     ↓
//! modify_graph (add/remove nodes, edges, update parameters)
//!     ↓
//! validate (acyclic, port types, bounds)
//!     ↓
//! calculate_latency & compensate (PDC alignment)
//!     ↓
//! allocate_runtime_state (preallocate buffers, execution plans)
//!     ↓
//! warm_up (initialize filter states, flush denormals)
//!     ↓
//! publish_generation (atomic pointer swap on audio thread)
//! ```
//!
//! If any step fails during validation, latency compensation, or allocation:
//! - The transaction aborts with a descriptive [`TransactionError`].
//! - The currently running graph/generation is **completely untouched**.
//! - Real-time audio playback continues seamlessly without dropouts or interruption.
//! - The audio callback **never** constructs graphs or allocates heap memory.

use serde::{Deserialize, Serialize};

use super::latency::compensate_at;
use super::rt::{RtExecutor, RtPlan};
use super::validate::{validate, Graph2Error, ValidationReport};
use super::Graph2;
use crate::decode::ChannelLayout;

/// Errors produced during a transactional graph edit.
#[derive(Debug, thiserror::Error)]
pub enum TransactionError {
    #[error("graph validation failed: {0}")]
    Validation(#[from] Graph2Error),

    #[error("latency compensation failed: {0}")]
    LatencyCompensation(String),

    #[error("runtime plan allocation failed: {0}")]
    Allocation(String),

    #[error("transaction has already been committed")]
    AlreadyCommitted,

    #[error("transaction was explicitly cancelled")]
    Cancelled,
}

/// Summary report emitted upon successful transaction commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransactionReport {
    /// Number of nodes in the published generation.
    pub node_count: usize,
    /// Number of edges in the published generation.
    pub edge_count: usize,
    /// Total deterministic graph latency in samples.
    pub latency_samples: u64,
    /// Total latency in milliseconds.
    pub latency_ms: f32,
}

/// An active, isolated transaction for modifying an audio graph.
pub struct GraphTransaction {
    /// Staged working copy of the graph topology.
    pub staged: Graph2,
    /// Target sample rate in Hz.
    pub sample_rate: f32,
    /// Output channel layout.
    pub layout: ChannelLayout,
    /// Whether this transaction has finished/committed.
    committed: bool,
}

impl GraphTransaction {
    /// Begin a new transaction starting from a snapshot of an existing [`Graph2`].
    pub fn begin(graph: &Graph2, sample_rate: f32, layout: ChannelLayout) -> Self {
        Self {
            staged: graph.clone(),
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            layout,
            committed: false,
        }
    }

    /// Access the staged graph mutably to perform node and edge modifications.
    pub fn staged_mut(&mut self) -> Result<&mut Graph2, TransactionError> {
        if self.committed {
            return Err(TransactionError::AlreadyCommitted);
        }
        Ok(&mut self.staged)
    }

    /// Explicitly validate the staged topology without publishing.
    pub fn validate(&self) -> Result<ValidationReport, TransactionError> {
        if self.committed {
            return Err(TransactionError::AlreadyCommitted);
        }
        let report = validate(&self.staged.nodes, &self.staged.edges);
        if !report.is_ok() {
            if let Some(first_err) = report.first_error() {
                return Err(TransactionError::Validation(first_err.clone()));
            }
        }
        Ok(report)
    }

    /// Execute the complete validation -> PDC -> compile -> warm-up pipeline,
    /// invoking `publish` only if all stages succeed.
    pub fn commit<F>(mut self, publish: F) -> Result<TransactionReport, TransactionError>
    where
        F: FnOnce(Graph2, RtPlan),
    {
        if self.committed {
            return Err(TransactionError::AlreadyCommitted);
        }

        // Step 1: Validate topology (detect cycles, port spec violations, missing endpoints)
        let vr = validate(&self.staged.nodes, &self.staged.edges);
        if !vr.is_ok() {
            let err = vr
                .first_error()
                .cloned()
                .unwrap_or(Graph2Error::Cycle(Vec::new()));
            return Err(TransactionError::Validation(err));
        }

        // Step 2: Calculate latency and auto-compensate branch delays (PDC)
        let mut compensated_graph = compensate_at(&self.staged, self.sample_rate)
            .map_err(|e| TransactionError::LatencyCompensation(e.to_string()))?;

        let lat_report = super::latency::analyze(&compensated_graph, self.sample_rate)
            .map_err(|e| TransactionError::LatencyCompensation(e.to_string()))?;

        let order = compensated_graph
            .compile()
            .map_err(TransactionError::Validation)?
            .clone();

        // Step 3: Allocate runtime state (RtPlan with preallocated scratch buffers)
        let rt_plan =
            RtPlan::build(&compensated_graph, &order, 64, self.sample_rate, None, None)
                .map_err(|e| TransactionError::Allocation(format!("RtPlan build error: {e:?}")))?;

        // Step 4: Warm up state (simulate dry block to ensure caches/denormals are flushed)
        let mut warm_exec = RtExecutor::new(rt_plan);
        let mut dummy_out = vec![0.0f32; 64];
        warm_exec.render_block(&mut dummy_out);
        let rt_plan = warm_exec.into_plan();

        // Step 5: Publish atomically via the caller's swap closure
        publish(compensated_graph.clone(), rt_plan);
        self.committed = true;

        Ok(TransactionReport {
            node_count: compensated_graph.node_count(),
            edge_count: compensated_graph.edge_count(),
            latency_samples: lat_report.total_samples,
            latency_ms: lat_report.total_ms,
        })
    }

    /// Discard the transaction without altering the active graph.
    pub fn rollback(mut self) {
        self.committed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::graph2::PortId;

    #[test]
    fn test_successful_transaction_commit() {
        let mut g = Graph2::new();
        let src = g.add_source("s");
        let gain = g.add_gain("g", 0.5);
        let sink = g.add_sink("out");
        g.add_edge(src, PortId::OUT, gain, PortId::IN).unwrap();
        g.add_edge(gain, PortId::OUT, sink, PortId::IN).unwrap();

        let tx = GraphTransaction::begin(&g, 48000.0, ChannelLayout::Stereo);
        let mut published = false;

        let report = tx
            .commit(|_graph, _plan| {
                published = true;
            })
            .expect("commit should succeed");

        assert!(published);
        assert_eq!(report.node_count, 3);
        assert_eq!(report.edge_count, 2);
    }

    #[test]
    fn test_failed_transaction_aborts_without_publishing() {
        let mut g = Graph2::new();
        let n1 = g.add_gain("n1", 1.0);
        let n2 = g.add_gain("n2", 1.0);
        // Create an illegal cycle
        g.add_edge(n1, PortId::OUT, n2, PortId::IN).unwrap();
        g.add_edge(n2, PortId::OUT, n1, PortId::IN).unwrap();

        let tx = GraphTransaction::begin(&g, 48000.0, ChannelLayout::Stereo);
        let mut published = false;

        let result = tx.commit(|_graph, _plan| {
            published = true;
        });

        assert!(result.is_err());
        assert!(!published, "Must NEVER publish an invalid graph");
    }
}
