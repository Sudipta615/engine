//! Real-time audio callback health monitor (§6.1, Item 14).
//!
//! Provides zero-allocation, lock-free telemetry tracking for every audio callback:
//! - Callback duration (latest, moving average, worst-case)
//! - CPU budget usage and real-time deadline adherence
//! - Dropouts (XRuns, underruns, overruns, late callbacks)
//! - Buffer fill level, clock drift, resampler ratio, graph generation, and latency.
//!
//! Hot-path methods only perform atomic operations (`AtomicU64`, `AtomicU32`, `AtomicI32`).
//! Telemetry readers obtain a coherent point-in-time [`RealtimeHealthSnapshot`] without
//! blocking or disturbing the audio callback.

use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Scale factor for fixed-point atomic percentages (100.00% = 100_000).
const PCT_SCALE: f32 = 1000.0;
/// Scale factor for resampler ratio (1.000000 = 1_000_000).
const RATIO_SCALE: f32 = 1_000_000.0;

/// Snapshot of the real-time audio callback health telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct RealtimeHealthSnapshot {
    /// Latest callback duration in microseconds.
    pub callback_duration_us: f32,
    /// Exponential moving average callback duration in microseconds.
    pub avg_duration_us: f32,
    /// Peak / worst-case callback duration in microseconds since last reset.
    pub worst_duration_us: f32,
    /// Estimated audio thread CPU usage percentage (duration / deadline * 100).
    pub cpu_usage_pct: f32,
    /// Nominal deadline duration per callback period in microseconds.
    pub deadline_us: f32,
    /// Total buffer xruns (underruns + overruns) observed.
    pub xruns: u64,
    /// Total ring/device buffer underruns.
    pub underruns: u64,
    /// Total buffer overruns.
    pub overruns: u64,
    /// Number of callbacks whose execution exceeded the real-time deadline.
    pub late_callbacks: u64,
    /// Device or ring buffer fill percentage `[0.0, 100.0]`.
    pub buffer_fill_pct: f32,
    /// Clock drift relative to nominal rate in parts-per-million (ppm).
    pub clock_drift_ppm: i32,
    /// Current asynchronous resampler ratio.
    pub resampler_ratio: f32,
    /// Active DSP graph generation number.
    pub graph_generation: u64,
    /// Total pipeline latency in samples.
    pub latency_samples: u64,
}

/// Real-time safe audio health monitor.
///
/// Implemented entirely via atomic registers. Can be embedded directly inside
/// an output backend, an endpoint worker, or the engine supervisor.
#[derive(Debug)]
pub struct RealtimeHealthMonitor {
    callback_duration_ns: AtomicU64,
    avg_duration_ns: AtomicU64,
    worst_duration_ns: AtomicU64,
    cpu_usage_scaled: AtomicU32,
    deadline_ns: AtomicU64,
    xruns: AtomicU64,
    underruns: AtomicU64,
    overruns: AtomicU64,
    late_callbacks: AtomicU64,
    buffer_fill_scaled: AtomicU32,
    clock_drift_ppm: AtomicI32,
    resampler_ratio_scaled: AtomicU32,
    graph_generation: AtomicU64,
    latency_samples: AtomicU64,
}

impl Default for RealtimeHealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl RealtimeHealthMonitor {
    /// Create a new real-time health monitor initialized to zero.
    pub fn new() -> Self {
        Self {
            callback_duration_ns: AtomicU64::new(0),
            avg_duration_ns: AtomicU64::new(0),
            worst_duration_ns: AtomicU64::new(0),
            cpu_usage_scaled: AtomicU32::new(0),
            deadline_ns: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            late_callbacks: AtomicU64::new(0),
            buffer_fill_scaled: AtomicU32::new(0),
            clock_drift_ppm: AtomicI32::new(0),
            resampler_ratio_scaled: AtomicU32::new(RATIO_SCALE as u32),
            graph_generation: AtomicU64::new(0),
            latency_samples: AtomicU64::new(0),
        }
    }

    /// Mark the entry of an audio callback. Returns the start timestamp.
    #[inline(always)]
    pub fn record_callback_start(&self) -> Instant {
        Instant::now()
    }

    /// Record the completion of an audio callback with duration and buffer status.
    ///
    /// This method is strictly non-allocating and lock-free.
    #[inline]
    pub fn record_callback_end(
        &self,
        start: Instant,
        frames: u32,
        sample_rate: u32,
        buffer_fill: usize,
        buffer_capacity: usize,
    ) {
        let elapsed = start.elapsed();
        let dur_ns = elapsed.as_nanos() as u64;

        // Store latest duration
        self.callback_duration_ns.store(dur_ns, Ordering::Relaxed);

        // Update exponential moving average (alpha = 1/8)
        let old_avg = self.avg_duration_ns.load(Ordering::Relaxed);
        let new_avg = if old_avg == 0 {
            dur_ns
        } else {
            (old_avg.saturating_mul(7) + dur_ns) / 8
        };
        self.avg_duration_ns.store(new_avg, Ordering::Relaxed);

        // Retain worst-case duration using atomic compare-exchange loop
        let mut current_worst = self.worst_duration_ns.load(Ordering::Relaxed);
        while dur_ns > current_worst {
            match self.worst_duration_ns.compare_exchange_weak(
                current_worst,
                dur_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current_worst = actual,
            }
        }

        // Calculate nominal deadline = (frames / sample_rate) in nanoseconds
        if sample_rate > 0 {
            let deadline_ns = (frames as u64).saturating_mul(1_000_000_000) / (sample_rate as u64);
            self.deadline_ns.store(deadline_ns, Ordering::Relaxed);

            if deadline_ns > 0 {
                // Check for deadline miss (late callback)
                if dur_ns > deadline_ns {
                    self.late_callbacks.fetch_add(1, Ordering::Relaxed);
                }

                // CPU usage = (dur_ns / deadline_ns) * 100 * PCT_SCALE
                let cpu_pct_scaled =
                    ((dur_ns as f64 / deadline_ns as f64) * 100.0 * (PCT_SCALE as f64))
                        .clamp(0.0, u32::MAX as f64) as u32;
                self.cpu_usage_scaled
                    .store(cpu_pct_scaled, Ordering::Relaxed);
            }
        }

        // Update buffer fill percentage
        if buffer_capacity > 0 {
            let fill_pct = (buffer_fill as f32 / buffer_capacity as f32) * 100.0 * PCT_SCALE;
            self.buffer_fill_scaled
                .store(fill_pct as u32, Ordering::Relaxed);
        }
    }

    /// Record an underrun incident.
    #[inline]
    pub fn record_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an overrun incident.
    #[inline]
    pub fn record_overrun(&self) {
        self.overruns.fetch_add(1, Ordering::Relaxed);
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    /// Update measured clock drift (in ppm) and current resampler ratio.
    #[inline]
    pub fn update_drift_and_ratio(&self, drift_ppm: i32, ratio: f32) {
        self.clock_drift_ppm.store(drift_ppm, Ordering::Relaxed);
        let ratio_scaled = (ratio.clamp(0.0, 100.0) * RATIO_SCALE) as u32;
        self.resampler_ratio_scaled
            .store(ratio_scaled, Ordering::Relaxed);
    }

    /// Update the active graph generation.
    #[inline]
    pub fn update_graph_generation(&self, generation: u64) {
        self.graph_generation.store(generation, Ordering::Relaxed);
    }

    /// Update total pipeline latency in samples.
    #[inline]
    pub fn update_latency(&self, latency_samples: u64) {
        self.latency_samples
            .store(latency_samples, Ordering::Relaxed);
    }

    /// Reset the recorded worst-case callback duration (e.g. after mode/rate change).
    pub fn reset_worst_duration(&self) {
        self.worst_duration_ns.store(0, Ordering::Relaxed);
    }

    /// Sample a telemetry snapshot. Safe to call from any thread without locking.
    pub fn snapshot(&self) -> RealtimeHealthSnapshot {
        let dur_ns = self.callback_duration_ns.load(Ordering::Relaxed);
        let avg_ns = self.avg_duration_ns.load(Ordering::Relaxed);
        let worst_ns = self.worst_duration_ns.load(Ordering::Relaxed);
        let deadline_ns = self.deadline_ns.load(Ordering::Relaxed);
        let cpu_scaled = self.cpu_usage_scaled.load(Ordering::Relaxed);
        let fill_scaled = self.buffer_fill_scaled.load(Ordering::Relaxed);
        let drift_ppm = self.clock_drift_ppm.load(Ordering::Relaxed);
        let ratio_scaled = self.resampler_ratio_scaled.load(Ordering::Relaxed);

        RealtimeHealthSnapshot {
            callback_duration_us: dur_ns as f32 / 1000.0,
            avg_duration_us: avg_ns as f32 / 1000.0,
            worst_duration_us: worst_ns as f32 / 1000.0,
            cpu_usage_pct: cpu_scaled as f32 / PCT_SCALE,
            deadline_us: deadline_ns as f32 / 1000.0,
            xruns: self.xruns.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            late_callbacks: self.late_callbacks.load(Ordering::Relaxed),
            buffer_fill_pct: fill_scaled as f32 / PCT_SCALE,
            clock_drift_ppm: drift_ppm,
            resampler_ratio: ratio_scaled as f32 / RATIO_SCALE,
            graph_generation: self.graph_generation.load(Ordering::Relaxed),
            latency_samples: self.latency_samples.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn health_monitor_tracks_metrics_without_allocating() {
        let monitor = RealtimeHealthMonitor::new();
        assert_eq!(monitor.snapshot().xruns, 0);

        let start = monitor.record_callback_start();
        thread::sleep(Duration::from_millis(1));
        monitor.record_callback_end(start, 256, 48000, 128, 256);

        monitor.record_underrun();
        monitor.record_overrun();
        monitor.update_drift_and_ratio(15, 1.000015);
        monitor.update_graph_generation(3);
        monitor.update_latency(512);

        let snap = monitor.snapshot();
        assert!(snap.callback_duration_us > 500.0);
        assert!(snap.worst_duration_us >= snap.callback_duration_us);
        assert_eq!(snap.xruns, 2);
        assert_eq!(snap.underruns, 1);
        assert_eq!(snap.overruns, 1);
        assert_eq!(snap.clock_drift_ppm, 15);
        assert!((snap.resampler_ratio - 1.000015).abs() < 1e-4);
        assert_eq!(snap.graph_generation, 3);
        assert_eq!(snap.latency_samples, 512);
        assert!((snap.buffer_fill_pct - 50.0).abs() < 0.1);
    }

    #[test]
    fn worst_duration_retention_and_reset() {
        let monitor = RealtimeHealthMonitor::new();
        let start1 = monitor.record_callback_start();
        thread::sleep(Duration::from_millis(2));
        monitor.record_callback_end(start1, 256, 48000, 50, 100);

        let peak1 = monitor.snapshot().worst_duration_us;
        assert!(peak1 > 1000.0);

        let start2 = monitor.record_callback_start();
        // zero sleep
        monitor.record_callback_end(start2, 256, 48000, 50, 100);
        let peak2 = monitor.snapshot().worst_duration_us;
        assert_eq!(peak1, peak2, "worst duration should be retained");

        monitor.reset_worst_duration();
        assert_eq!(monitor.snapshot().worst_duration_us, 0.0);
    }
}
