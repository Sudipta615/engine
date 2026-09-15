//! Node-level diagnostics and introspection (§6.4, Item 15).
//!
//! Provides granular diagnostic introspection into individual DSP graph nodes:
//! - Execution time / CPU cost
//! - Latency and ring-down tail
//! - Input and output peak levels (dBFS)
//! - Gain reduction (for limiters, compressors, and dynamic EQ)
//! - Error and non-finite sample counts.
//!
//! Enables answering: "Which node is responsible for CPU load, latency, or failure?"
//! without external profiling tools.

use serde::{Deserialize, Serialize};

/// Detailed diagnostic metrics for an individual graph node (§6.4, Item 15).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeDiagnostics {
    /// Node identity / arena slot index.
    pub node_id: u32,
    /// Human-readable node name (e.g. "limiter", "dynamics", "eq", "convolution").
    pub name: String,
    /// Whether the node is currently active in the signal path.
    pub active: bool,
    /// Measured or modeled processing cost in microseconds per block.
    pub cpu_cost_us: f32,
    /// Deterministic latency introduced by this node in samples.
    pub latency_samples: usize,
    /// Deterministic latency in milliseconds at the current sample rate.
    pub latency_ms: f32,
    /// Ring-down tail in samples.
    pub tail_samples: usize,
    /// Ring-down tail in milliseconds at the current sample rate.
    pub tail_ms: f32,
    /// Peak input amplitude across all channels in dBFS (-120.0 to 0.0+).
    pub input_peak_dbfs: f32,
    /// Peak output amplitude across all channels in dBFS.
    pub output_peak_dbfs: f32,
    /// Dynamic gain reduction applied by this node in dB (e.g. -3.5 dB; 0.0 for linear nodes).
    pub gain_reduction_db: f32,
    /// Internal processing errors encountered by this node.
    pub error_count: u64,
    /// Number of non-finite (NaN / ±Inf) samples caught by this node.
    pub non_finite_count: u64,
}

impl NodeDiagnostics {
    pub fn new(node_id: u32, name: impl Into<String>, active: bool) -> Self {
        Self {
            node_id,
            name: name.into(),
            active,
            cpu_cost_us: 0.0,
            latency_samples: 0,
            latency_ms: 0.0,
            tail_samples: 0,
            tail_ms: 0.0,
            input_peak_dbfs: -120.0,
            output_peak_dbfs: -120.0,
            gain_reduction_db: 0.0,
            error_count: 0,
            non_finite_count: 0,
        }
    }

    /// Builder method to populate latency and tail.
    pub fn with_latency_and_tail(
        mut self,
        latency_samples: usize,
        latency_ms: f32,
        tail_samples: usize,
        tail_ms: f32,
    ) -> Self {
        self.latency_samples = latency_samples;
        self.latency_ms = latency_ms;
        self.tail_samples = tail_samples;
        self.tail_ms = tail_ms;
        self
    }

    /// Builder method to populate peaks and gain reduction.
    pub fn with_metering(
        mut self,
        input_peak_dbfs: f32,
        output_peak_dbfs: f32,
        gain_reduction_db: f32,
    ) -> Self {
        self.input_peak_dbfs = input_peak_dbfs;
        self.output_peak_dbfs = output_peak_dbfs;
        self.gain_reduction_db = gain_reduction_db;
        self
    }

    /// Builder method to populate error counts.
    pub fn with_errors(mut self, error_count: u64, non_finite_count: u64) -> Self {
        self.error_count = error_count;
        self.non_finite_count = non_finite_count;
        self
    }

    /// Builder method to set CPU execution cost.
    pub fn with_cpu_cost(mut self, cpu_cost_us: f32) -> Self {
        self.cpu_cost_us = cpu_cost_us;
        self
    }
}

/// Convert a linear amplitude sample to dBFS with a floor at -120 dBFS.
#[inline]
pub fn linear_to_dbfs(linear: f32) -> f32 {
    if linear <= 1e-6 {
        -120.0
    } else {
        20.0 * linear.log10()
    }
}

/// Helper to scan a slice of channels for peak amplitude and non-finite counts.
#[inline]
pub fn scan_plane_peak_and_non_finites(planes: &[&[f32]]) -> (f32, u64) {
    let mut peak = 0.0f32;
    let mut non_finites = 0u64;

    for plane in planes {
        for &sample in *plane {
            if sample.is_finite() {
                let abs = sample.abs();
                if abs > peak {
                    peak = abs;
                }
            } else {
                non_finites = non_finites.saturating_add(1);
            }
        }
    }

    (linear_to_dbfs(peak), non_finites)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_to_dbfs_conversions() {
        assert!((linear_to_dbfs(1.0) - 0.0).abs() < 1e-4);
        assert!((linear_to_dbfs(0.5) - (-6.0206)).abs() < 1e-2);
        assert!((linear_to_dbfs(0.1) - (-20.0)).abs() < 1e-4);
        assert_eq!(linear_to_dbfs(0.0), -120.0);
    }

    #[test]
    fn node_diagnostics_builder_and_serialization() {
        let diag = NodeDiagnostics::new(3, "limiter", true)
            .with_latency_and_tail(128, 2.67, 2400, 50.0)
            .with_metering(-0.5, -0.1, -2.4)
            .with_errors(0, 0)
            .with_cpu_cost(14.5);

        assert_eq!(diag.node_id, 3);
        assert_eq!(diag.name, "limiter");
        assert!(diag.active);
        assert_eq!(diag.gain_reduction_db, -2.4);
        assert_eq!(diag.cpu_cost_us, 14.5);

        let json = serde_json::to_string(&diag).unwrap();
        let back: NodeDiagnostics = serde_json::from_str(&json).unwrap();
        assert_eq!(back, diag);
    }

    #[test]
    fn scan_plane_detects_peak_and_non_finites() {
        let ch0 = [0.1, 0.5, 0.2];
        let ch1 = [0.0, f32::NAN, 0.25];
        let planes: [&[f32]; 2] = [&ch0, &ch1];

        let (peak_dbfs, nans) = scan_plane_peak_and_non_finites(&planes);
        assert!((peak_dbfs - (-6.0206)).abs() < 1e-2);
        assert_eq!(nans, 1);
    }
}
