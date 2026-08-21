//! Real-time engine telemetry: CPU timing, deadline misses, and underrun metrics.

use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct EngineTelemetry {
    pub(crate) dsp_time: Duration,
    pub(crate) total_time: Duration,
    pub(crate) tick_start: Option<Instant>,
    pub(crate) last_cpu_reset: Instant,
    pub(crate) worst_dsp_time: Duration,
    pub(crate) worst_tick_time: Duration,
    pub(crate) deadline_miss_window: u32,
    pub(crate) underruns_window: u32,
    pub(crate) underruns_total: u32,
}

impl Default for EngineTelemetry {
    fn default() -> Self {
        Self {
            dsp_time: Duration::ZERO,
            total_time: Duration::ZERO,
            tick_start: None,
            last_cpu_reset: Instant::now(),
            worst_dsp_time: Duration::ZERO,
            worst_tick_time: Duration::ZERO,
            deadline_miss_window: 0,
            underruns_window: 0,
            underruns_total: 0,
        }
    }
}
