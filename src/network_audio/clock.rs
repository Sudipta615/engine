//! IEEE 1588-2008 Precision Time Protocol (PTPv2 / AES67) clock synchronization (§10.4, Item 33).
//!
//! Provides PTPv2 timestamp arithmetic, 4-timestamp delay and offset calculation,
//! jitter filtering, PLL drift tracking in PPM, and lock status estimation.

use serde::{Deserialize, Serialize};

/// High-precision IEEE 1588-2008 PTP timestamp (seconds + nanoseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PtpTimestamp {
    /// 48-bit integer seconds since epoch.
    pub seconds: u64,
    /// Nanoseconds (0..999_999_999).
    pub nanoseconds: u32,
}

impl PtpTimestamp {
    pub const fn new(seconds: u64, nanoseconds: u32) -> Self {
        Self {
            seconds,
            nanoseconds,
        }
    }

    /// Converts the timestamp to total nanoseconds (as i128 to prevent overflow).
    pub fn as_total_nanos(&self) -> i128 {
        (self.seconds as i128 * 1_000_000_000) + self.nanoseconds as i128
    }

    /// Creates a timestamp from total nanoseconds.
    pub fn from_total_nanos(nanos: i128) -> Self {
        let positive_nanos = if nanos < 0 { 0 } else { nanos };
        let seconds = (positive_nanos / 1_000_000_000) as u64;
        let nanoseconds = (positive_nanos % 1_000_000_000) as u32;
        Self {
            seconds,
            nanoseconds,
        }
    }

    /// Difference in nanoseconds (`self - other`).
    pub fn diff_nanos(&self, other: &Self) -> i64 {
        (self.as_total_nanos() - other.as_total_nanos()) as i64
    }
}

/// Operational state of the local PTP clock slave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtpClockState {
    #[default]
    Unsynchronized,
    Synchronizing,
    Locked,
    Holdover,
    Fault,
}

/// Telemetry metrics snapshot for network clock synchronization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PtpClockTelemetry {
    pub state: PtpClockState,
    pub offset_ns: f64,
    pub mean_path_delay_ns: f64,
    pub jitter_ns: f64,
    pub drift_ppm: f64,
    pub lock_loss_count: u32,
    pub sync_samples_count: u64,
    pub grandmaster_id: String,
}

/// IEEE 1588-2008 PTP Clock synchronization engine.
pub struct PtpClock {
    grandmaster_id: String,
    state: PtpClockState,
    filtered_offset_ns: f64,
    filtered_path_delay_ns: f64,
    jitter_ns: f64,
    drift_ppm: f64,
    last_offset_ns: f64,
    sync_samples: u64,
    lock_loss_count: u32,
    /// Threshold (ns) below which clock is considered locked (e.g., 1000 ns = 1 µs).
    lock_threshold_ns: f64,
    /// Smoothing factor alpha (0.0 < alpha <= 1.0) for EMA filtering.
    filter_alpha: f64,
}

impl Default for PtpClock {
    fn default() -> Self {
        Self::new("00-00-00-00-00-00-00-00")
    }
}

impl PtpClock {
    /// Create a new PTP clock tracking the specified grandmaster identifier.
    pub fn new(grandmaster_id: &str) -> Self {
        Self {
            grandmaster_id: grandmaster_id.to_string(),
            state: PtpClockState::Unsynchronized,
            filtered_offset_ns: 0.0,
            filtered_path_delay_ns: 0.0,
            jitter_ns: 0.0,
            drift_ppm: 0.0,
            last_offset_ns: 0.0,
            sync_samples: 0,
            lock_loss_count: 0,
            lock_threshold_ns: 1_000.0, // 1 microsecond lock target
            filter_alpha: 0.1,          // 10% weight to new sample
        }
    }

    /// Process a standard 4-timestamp PTP exchange:
    /// - `t1`: Sync message egress timestamp from master
    /// - `t2`: Sync message ingress timestamp at slave
    /// - `t3`: Delay_Req message egress timestamp from slave
    /// - `t4`: Delay_Resp message ingress timestamp at master
    pub fn process_timestamp_exchange(
        &mut self,
        t1: PtpTimestamp,
        t2: PtpTimestamp,
        t3: PtpTimestamp,
        t4: PtpTimestamp,
    ) {
        let master_to_slave = t2.diff_nanos(&t1) as f64;
        let slave_to_master = t4.diff_nanos(&t3) as f64;

        // Path delay = ((t2 - t1) + (t4 - t3)) / 2
        let raw_delay = (master_to_slave + slave_to_master) / 2.0;
        // Offset = ((t2 - t1) - (t4 - t3)) / 2
        let raw_offset = (master_to_slave - slave_to_master) / 2.0;

        self.sync_samples += 1;

        if self.sync_samples == 1 {
            self.filtered_offset_ns = raw_offset;
            self.filtered_path_delay_ns = raw_delay.max(0.0);
            self.last_offset_ns = raw_offset;
            self.state = PtpClockState::Synchronizing;
        } else {
            // Update jitter metric: deviation between raw offset and filtered offset
            let dev = (raw_offset - self.filtered_offset_ns).abs();
            self.jitter_ns = (1.0 - self.filter_alpha) * self.jitter_ns + self.filter_alpha * dev;

            // Frequency drift estimation (derivative of offset in PPM: ns/s = 1e-9 = 1 ppb = 1e-3 ppm)
            let delta_offset = raw_offset - self.last_offset_ns;
            self.drift_ppm = delta_offset * 1e-3;
            self.last_offset_ns = raw_offset;

            // Low-pass EMA filter update
            self.filtered_offset_ns = (1.0 - self.filter_alpha) * self.filtered_offset_ns
                + self.filter_alpha * raw_offset;
            self.filtered_path_delay_ns = (1.0 - self.filter_alpha) * self.filtered_path_delay_ns
                + self.filter_alpha * raw_delay.max(0.0);
        }

        // State machine transitions
        let abs_offset = self.filtered_offset_ns.abs();
        if abs_offset <= self.lock_threshold_ns {
            self.state = PtpClockState::Locked;
        } else if self.state == PtpClockState::Locked {
            self.lock_loss_count += 1;
            self.state = PtpClockState::Synchronizing;
        }
    }

    /// Local time translation: convert raw local time to PTP grandmaster domain.
    pub fn local_to_ptp(&self, local_nanos: u64) -> u64 {
        if self.state == PtpClockState::Unsynchronized {
            return local_nanos;
        }
        let adjusted = local_nanos as f64 + self.filtered_offset_ns;
        if adjusted < 0.0 {
            0
        } else {
            adjusted as u64
        }
    }

    /// Read live telemetry snapshot.
    pub fn telemetry(&self) -> PtpClockTelemetry {
        PtpClockTelemetry {
            state: self.state,
            offset_ns: self.filtered_offset_ns,
            mean_path_delay_ns: self.filtered_path_delay_ns,
            jitter_ns: self.jitter_ns,
            drift_ppm: self.drift_ppm,
            lock_loss_count: self.lock_loss_count,
            sync_samples_count: self.sync_samples,
            grandmaster_id: self.grandmaster_id.clone(),
        }
    }

    /// Reset clock tracking.
    pub fn reset(&mut self) {
        self.state = PtpClockState::Unsynchronized;
        self.filtered_offset_ns = 0.0;
        self.filtered_path_delay_ns = 0.0;
        self.jitter_ns = 0.0;
        self.drift_ppm = 0.0;
        self.sync_samples = 0;
    }
}
