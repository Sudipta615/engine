//! Professional industry-grade audio metering subsystem.
//!
//! # Features
//!
//! - **Sample Peak**: Peak amplitude in dBFS per channel and max peak observed.
//! - **True Peak**: Inter-sample true-peak in dBTP via ITU-R BS.1770-5 Annex 2 4× FIR oversampling.
//! - **RMS**: Windowed root-mean-square level in dBFS per channel.
//! - **EBU R128 / BS.1770 LUFS**:
//!   - Momentary LUFS (400 ms sliding window).
//!   - Short-Term LUFS (3 s sliding window).
//!   - Integrated LUFS (program loudness with absolute & relative gating).
//!   - Loudness Range (LRA) in LU.
//! - **DC Offset**: Mean sample offset per channel.
//! - **Clipping Counter**: Count of samples reaching or exceeding ±1.0 (0 dBFS).
//! - **Dynamic Range / Crest Factor**: Peak-to-RMS ratio in dB.
//! - **Realtime Safe**: Rate-limited snapshots, lock-free telemetry export.

use std::cell::{Cell, UnsafeCell};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::buffer::MAX_CHANNELS;
use crate::dsp::loudness::LoudnessMeter;

/// Point-in-time snapshot of the professional metering subsystem.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProfessionalMeterSnapshot {
    /// Sample peak in dBFS for each active channel.
    pub peak_db: Vec<f32>,
    /// Maximum sample peak observed since last reset in dBFS.
    pub max_peak_db: f32,
    /// Inter-sample true-peak in dBTP for each active channel.
    pub true_peak_dbtp: Vec<f32>,
    /// Maximum true-peak observed since last reset in dBTP.
    pub max_true_peak_dbtp: f32,
    /// RMS level in dBFS for each active channel.
    pub rms_db: Vec<f32>,
    /// Dynamic range / crest factor (Peak − RMS) in dB per channel.
    pub dynamic_range_db: Vec<f32>,
    /// DC offset (linear average sample offset) per channel.
    pub dc_offset: Vec<f32>,
    /// Momentary LUFS (400 ms window).
    pub lufs_momentary: f32,
    /// Short-term LUFS (3 s window).
    pub lufs_short_term: f32,
    /// Integrated program loudness in LUFS.
    pub lufs_integrated: f32,
    /// Loudness Range (LRA) in LU.
    pub loudness_range_lu: f32,
    /// Whether `loudness_range_lu` is statistically valid.
    pub lra_valid: bool,
    /// Cumulative sample clipping count (samples reaching ±1.0).
    pub clip_count: u64,
    /// Active channel count.
    pub channels: usize,
    /// Active sample rate in Hz.
    pub sample_rate: u32,
}

#[derive(Clone, Copy)]
struct MeterSnapshotData {
    peak_linear: [f32; MAX_CHANNELS],
    true_peak_linear: [f32; MAX_CHANNELS],
    rms_linear: [f32; MAX_CHANNELS],
    window_dc_offset: [f32; MAX_CHANNELS],
    max_peak_linear: f32,
    max_true_peak_linear: f32,
    lufs_momentary: f32,
    lufs_short_term: f32,
    lufs_integrated: f32,
    loudness_range_lu: f32,
    lra_valid: bool,
    clip_count: u64,
    channels: usize,
    sample_rate: u32,
    initialized: bool,
}

impl Default for MeterSnapshotData {
    fn default() -> Self {
        Self {
            peak_linear: [0.0; MAX_CHANNELS],
            true_peak_linear: [0.0; MAX_CHANNELS],
            rms_linear: [0.0; MAX_CHANNELS],
            window_dc_offset: [0.0; MAX_CHANNELS],
            max_peak_linear: 0.0,
            max_true_peak_linear: 0.0,
            lufs_momentary: f32::NEG_INFINITY,
            lufs_short_term: f32::NEG_INFINITY,
            lufs_integrated: f32::NEG_INFINITY,
            loudness_range_lu: 0.0,
            lra_valid: false,
            clip_count: 0,
            channels: 2,
            sample_rate: 44100,
            initialized: false,
        }
    }
}

const NEW_DATA_FLAG: usize = 0x80;

struct MeterState {
    channels: usize,
    sample_rate: u32,
    loudness_meter: LoudnessMeter,
    sum_sq: [f64; MAX_CHANNELS],
    sum_samples: [f64; MAX_CHANNELS],
    peak_linear: [f32; MAX_CHANNELS],
    window_peak_linear: [f32; MAX_CHANNELS],
    window_rms_linear: [f32; MAX_CHANNELS],
    window_dc_offset: [f32; MAX_CHANNELS],
    max_peak_linear: f32,
    max_true_peak_linear: f32,
    frames_in_window: usize,
    window_size: usize,
    clip_count: u64,
}

impl MeterState {
    fn new(sample_rate: u32, channels: usize) -> Self {
        let ch = channels.clamp(1, MAX_CHANNELS);
        let sr = sample_rate.max(1);
        let window_size = (sr as f32 * 0.3) as usize; // 300 ms RMS window

        Self {
            channels: ch,
            sample_rate: sr,
            loudness_meter: LoudnessMeter::new(sr as f32, ch),
            sum_sq: [0.0; MAX_CHANNELS],
            sum_samples: [0.0; MAX_CHANNELS],
            peak_linear: [0.0; MAX_CHANNELS],
            window_peak_linear: [0.0; MAX_CHANNELS],
            window_rms_linear: [0.0; MAX_CHANNELS],
            window_dc_offset: [0.0; MAX_CHANNELS],
            max_peak_linear: 0.0,
            max_true_peak_linear: 0.0,
            frames_in_window: 0,
            window_size: window_size.max(64),
            clip_count: 0,
        }
    }

    fn reset(&mut self) {
        self.loudness_meter.reset();
        self.sum_sq = [0.0; MAX_CHANNELS];
        self.sum_samples = [0.0; MAX_CHANNELS];
        self.peak_linear = [0.0; MAX_CHANNELS];
        self.window_peak_linear = [0.0; MAX_CHANNELS];
        self.window_rms_linear = [0.0; MAX_CHANNELS];
        self.window_dc_offset = [0.0; MAX_CHANNELS];
        self.max_peak_linear = 0.0;
        self.max_true_peak_linear = 0.0;
        self.frames_in_window = 0;
        self.clip_count = 0;
    }

    fn reconfigure(&mut self, sample_rate: u32, channels: usize) {
        let ch = channels.clamp(1, MAX_CHANNELS);
        let sr = sample_rate.max(1);
        self.channels = ch;
        self.sample_rate = sr;
        self.window_size = ((sr as f32 * 0.3) as usize).max(64);
        self.loudness_meter.set_sample_rate(sr as f32);
        self.loudness_meter
            .set_channel_layout(&crate::decode::ChannelLayout::from_count(ch));
        self.reset();
    }
}

/// Unified professional metering subsystem.
pub struct ProfessionalMeters {
    enabled: AtomicBool,
    processing: AtomicBool,
    state: UnsafeCell<MeterState>,
    buffers: [UnsafeCell<MeterSnapshotData>; 3],
    writer_slot: Cell<usize>,
    shared_slot: AtomicUsize,
    reader_slot: Mutex<usize>,
    reset_requested: AtomicBool,
    reconfig_requested: AtomicBool,
    pending_reconfig_sr: AtomicU32,
    pending_reconfig_ch: AtomicUsize,
    latest_momentary_lufs: AtomicU32,
    latest_peak_db: AtomicU32,
    latest_true_peak_db: AtomicU32,
    total_clips: AtomicU64,
}

unsafe impl Send for ProfessionalMeters {}
unsafe impl Sync for ProfessionalMeters {}

impl ProfessionalMeters {
    /// Create a new professional meter instance.
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            state: UnsafeCell::new(MeterState::new(sample_rate, channels)),
            buffers: [
                UnsafeCell::new(MeterSnapshotData::default()),
                UnsafeCell::new(MeterSnapshotData::default()),
                UnsafeCell::new(MeterSnapshotData::default()),
            ],
            writer_slot: Cell::new(0),
            shared_slot: AtomicUsize::new(1),
            reader_slot: Mutex::new(2),
            reset_requested: AtomicBool::new(false),
            reconfig_requested: AtomicBool::new(false),
            pending_reconfig_sr: AtomicU32::new(sample_rate),
            pending_reconfig_ch: AtomicUsize::new(channels),
            latest_momentary_lufs: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
            latest_peak_db: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
            latest_true_peak_db: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
            total_clips: AtomicU64::new(0),
        }
    }

    /// Check if professional metering is actively enabled.
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Enable or disable professional metering processing.
    #[inline]
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Reset all meters and cumulative stats.
    pub fn reset(&self) {
        self.reset_requested.store(true, Ordering::Release);
        self.latest_momentary_lufs
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.latest_peak_db
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.latest_true_peak_db
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.total_clips.store(0, Ordering::Relaxed);
    }

    /// Update the sample rate and channel layout when a new track loads.
    pub fn reconfigure(&self, sample_rate: u32, channels: usize) {
        let ch = channels.clamp(1, MAX_CHANNELS);
        let sr = sample_rate.max(1);
        self.pending_reconfig_sr.store(sr, Ordering::Release);
        self.pending_reconfig_ch.store(ch, Ordering::Release);
        self.reconfig_requested.store(true, Ordering::Release);
    }

    /// Process a block of interleaved audio frames through the metering subsystem.
    ///
    /// ## Concurrency & Realtime Guarantee
    /// Zero mutex acquisition, zero locks, and zero heap allocations.
    /// Uses an atomic compare-exchange guard and lock-free triple-buffered
    /// snapshot exchange to publish telemetry to asynchronous readers.
    pub fn process_interleaved(&self, interleaved: &[f32], channels: usize) {
        if interleaved.is_empty() {
            return;
        }

        // Lock-free non-blocking entry: if another thread is processing, return
        if self
            .processing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let state = unsafe { &mut *self.state.get() };

        if self.reconfig_requested.swap(false, Ordering::Acquire) {
            let sr = self.pending_reconfig_sr.load(Ordering::Acquire);
            let ch = self.pending_reconfig_ch.load(Ordering::Acquire);
            state.reconfigure(sr, ch);
        } else if self.reset_requested.swap(false, Ordering::Acquire) {
            state.reset();
        }

        let ch = channels.clamp(1, MAX_CHANNELS);
        state.channels = ch;
        let frame_count = interleaved.len() / ch;
        if frame_count == 0 {
            self.processing.store(false, Ordering::Release);
            return;
        }

        // 1. EBU R128 loudness metering & 4x FIR true peak (BS.1770-5)
        state.loudness_meter.process_interleaved(interleaved, ch);

        // 2. Per-channel peak, RMS, DC offset, clipping
        for f in 0..frame_count {
            for c in 0..ch {
                let sample = interleaved[f * ch + c];
                let abs_sample = sample.abs();

                // Peak tracking
                if abs_sample > state.peak_linear[c] {
                    state.peak_linear[c] = abs_sample;
                }
                if abs_sample > state.max_peak_linear {
                    state.max_peak_linear = abs_sample;
                }

                // Clipping detection (sample >= 1.0)
                if abs_sample >= 1.0 {
                    state.clip_count += 1;
                    self.total_clips.fetch_add(1, Ordering::Relaxed);
                }

                // RMS and DC offset accumulators
                state.sum_sq[c] += (sample as f64) * (sample as f64);
                state.sum_samples[c] += sample as f64;
            }

            state.frames_in_window += 1;
            if state.frames_in_window >= state.window_size {
                let n = state.frames_in_window as f64;
                for c in 0..ch {
                    state.window_peak_linear[c] = state.peak_linear[c];
                    state.window_rms_linear[c] = (state.sum_sq[c] / n).max(1e-12).sqrt() as f32;
                    state.window_dc_offset[c] = (state.sum_samples[c] / n) as f32;
                    state.peak_linear[c] = 0.0;
                    state.sum_sq[c] = 0.0;
                    state.sum_samples[c] = 0.0;
                }
                state.frames_in_window = 0;
            }
        }

        // Check running true-peak maxima from the loudness meter (all channels up to ch)
        let mut max_tp = state.max_true_peak_linear;
        let mut tp_linear = [0.0f32; MAX_CHANNELS];
        for (c, tp_slot) in tp_linear.iter_mut().enumerate().take(ch) {
            let tp = state.loudness_meter.true_peak_meters()[c].max_true_peak_linear() as f32;
            *tp_slot = tp;
            if tp > max_tp {
                max_tp = tp;
            }
        }
        state.max_true_peak_linear = max_tp;

        // Update atomic quick-read meters
        let max_p = state.max_peak_linear;
        let p_db = if max_p > 0.0 {
            20.0 * max_p.log10()
        } else {
            -120.0
        };
        self.latest_peak_db.store(p_db.to_bits(), Ordering::Relaxed);

        let tp_db = if max_tp > 0.0 {
            20.0 * max_tp.log10()
        } else {
            -120.0
        };
        self.latest_true_peak_db
            .store(tp_db.to_bits(), Ordering::Relaxed);

        let l_meas = state.loudness_meter.snapshot();
        self.latest_momentary_lufs
            .store(l_meas.momentary_lufs.to_bits(), Ordering::Relaxed);

        // Publish snapshot to writer slot (triple buffer)
        let w_idx = self.writer_slot.get();
        let mut snap_peak = [0.0f32; MAX_CHANNELS];
        let mut snap_rms = [0.0f32; MAX_CHANNELS];
        let mut snap_dc = [0.0f32; MAX_CHANNELS];

        for c in 0..ch {
            let p = if state.frames_in_window > 0 {
                state.peak_linear[c].max(state.window_peak_linear[c])
            } else {
                state.window_peak_linear[c]
            };
            snap_peak[c] = p;

            let rms = if state.frames_in_window > 0 {
                (state.sum_sq[c] / state.frames_in_window as f64)
                    .max(1e-12)
                    .sqrt() as f32
            } else {
                state.window_rms_linear[c]
            };
            snap_rms[c] = rms;

            let dc = if state.frames_in_window > 0 {
                (state.sum_samples[c] / state.frames_in_window as f64) as f32
            } else {
                state.window_dc_offset[c]
            };
            snap_dc[c] = dc;
        }

        let snap_data = MeterSnapshotData {
            peak_linear: snap_peak,
            true_peak_linear: tp_linear,
            rms_linear: snap_rms,
            window_dc_offset: snap_dc,
            max_peak_linear: state.max_peak_linear,
            max_true_peak_linear: state.max_true_peak_linear,
            lufs_momentary: l_meas.momentary_lufs,
            lufs_short_term: l_meas.short_term_lufs,
            lufs_integrated: l_meas.integrated_lufs,
            loudness_range_lu: l_meas.lra_lu,
            lra_valid: l_meas.lra_valid,
            clip_count: state.clip_count,
            channels: ch,
            sample_rate: state.sample_rate,
            initialized: true,
        };

        unsafe {
            *self.buffers[w_idx].get() = snap_data;
        }

        // Atomically publish writer slot to shared slot
        let prev_shared = self
            .shared_slot
            .swap(w_idx | NEW_DATA_FLAG, Ordering::Release);
        self.writer_slot.set(prev_shared & 0x03);

        self.processing.store(false, Ordering::Release);
    }

    /// Capture a complete, point-in-time professional metering snapshot.
    ///
    /// Reads from the lock-free triple buffer without contending with or blocking
    /// the audio thread.
    pub fn snapshot(&self) -> ProfessionalMeterSnapshot {
        let Ok(mut reader_guard) = self.reader_slot.lock() else {
            return ProfessionalMeterSnapshot::default();
        };

        let cur_shared = self.shared_slot.load(Ordering::Acquire);
        if (cur_shared & NEW_DATA_FLAG) != 0 {
            let prev = self.shared_slot.swap(*reader_guard, Ordering::AcqRel);
            *reader_guard = prev & 0x03;
        }

        let r_idx = *reader_guard;
        let data = unsafe { *self.buffers[r_idx].get() };
        drop(reader_guard);

        if !data.initialized {
            return ProfessionalMeterSnapshot::default();
        }

        let ch = data.channels;
        let mut peak_db = Vec::with_capacity(ch);
        let mut true_peak_dbtp = Vec::with_capacity(ch);
        let mut rms_db = Vec::with_capacity(ch);
        let mut dynamic_range_db = Vec::with_capacity(ch);
        let mut dc_offset = Vec::with_capacity(ch);

        for c in 0..ch {
            let p = data.peak_linear[c];
            let p_db = if p > 0.0 { 20.0 * p.log10() } else { -120.0 };
            peak_db.push(p_db);

            let tp = data.true_peak_linear[c];
            let tp_db = if tp > 0.0 { 20.0 * tp.log10() } else { -120.0 };
            true_peak_dbtp.push(tp_db);

            let rms = data.rms_linear[c];
            let r_db = if rms > 0.0 {
                20.0 * rms.log10()
            } else {
                -120.0
            };
            rms_db.push(r_db);

            let dr = (p_db - r_db).max(0.0);
            dynamic_range_db.push(dr);

            dc_offset.push(data.window_dc_offset[c]);
        }

        let max_peak_db = if data.max_peak_linear > 0.0 {
            20.0 * data.max_peak_linear.log10()
        } else {
            -120.0
        };

        let max_true_peak_dbtp = if data.max_true_peak_linear > 0.0 {
            20.0 * data.max_true_peak_linear.log10()
        } else {
            -120.0
        };

        ProfessionalMeterSnapshot {
            peak_db,
            max_peak_db,
            true_peak_dbtp,
            max_true_peak_dbtp,
            rms_db,
            dynamic_range_db,
            dc_offset,
            lufs_momentary: data.lufs_momentary,
            lufs_short_term: data.lufs_short_term,
            lufs_integrated: data.lufs_integrated,
            loudness_range_lu: data.loudness_range_lu,
            lra_valid: data.lra_valid,
            clip_count: data.clip_count,
            channels: ch,
            sample_rate: data.sample_rate,
        }
    }
}

impl Default for ProfessionalMeters {
    fn default() -> Self {
        Self::new(44100, 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_professional_meters_basic() {
        let meters = ProfessionalMeters::new(44100, 2);

        // Generate 0.5 amplitude sine wave on L, 1.0 amplitude on R
        let mut buf = Vec::new();
        for i in 0..4410 {
            let t = i as f32 / 44100.0;
            let val_l = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            let val_r = 1.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            buf.push(val_l);
            buf.push(val_r);
        }

        meters.process_interleaved(&buf, 2);
        let snap = meters.snapshot();

        assert_eq!(snap.channels, 2);
        assert!(snap.peak_db[0] < -5.5 && snap.peak_db[0] > -6.5); // 0.5 ≈ -6.02 dBFS
        assert!(snap.peak_db[1] > -0.5 && snap.peak_db[1] <= 0.0); // 1.0 ≈ 0 dBFS
        assert!(snap.max_peak_db > -0.5);
        assert!(snap.rms_db[0] < snap.peak_db[0]);
    }

    #[test]
    fn test_professional_meters_multichannel_16ch() {
        let meters = ProfessionalMeters::new(48000, 16);

        // Generate 16-channel audio: channel 0 at 0.5, channel 15 at 0.25, others zero
        let mut buf = vec![0.0f32; 16 * 1024];
        for f in 0..1024 {
            buf[f * 16] = 0.5;
            buf[f * 16 + 15] = 0.25;
        }

        meters.process_interleaved(&buf, 16);
        let snap = meters.snapshot();

        assert_eq!(snap.channels, 16);
        assert_eq!(snap.peak_db.len(), 16);
        assert_eq!(snap.true_peak_dbtp.len(), 16);
        assert!(snap.peak_db[0] > -7.0 && snap.peak_db[0] < -5.0);
        assert!(snap.peak_db[15] > -13.0 && snap.peak_db[15] < -11.0);
        assert_eq!(snap.peak_db[1], -120.0);
    }

    #[test]
    fn test_professional_meters_concurrent_audio_and_snapshots() {
        use std::sync::Arc;
        use std::thread;

        let meters = Arc::new(ProfessionalMeters::new(48000, 2));
        let m_writer = Arc::clone(&meters);
        let m_reader1 = Arc::clone(&meters);
        let m_reader2 = Arc::clone(&meters);

        // Audio thread running process_interleaved in a tight loop
        let writer_handle = thread::spawn(move || {
            let buf = [0.1f32; 256 * 2];
            for _ in 0..500 {
                m_writer.process_interleaved(&buf, 2);
            }
        });

        // Telemetry thread 1 continuously taking snapshots
        let reader1_handle = thread::spawn(move || {
            for _ in 0..1000 {
                let _ = m_reader1.snapshot();
            }
        });

        // Telemetry thread 2 calling reset occasionally and reading snapshots
        let reader2_handle = thread::spawn(move || {
            for i in 0..500 {
                if i % 100 == 0 {
                    m_reader2.reset();
                }
                let _ = m_reader2.snapshot();
            }
        });

        writer_handle.join().expect("writer should finish");
        reader1_handle.join().expect("reader 1 should finish");
        reader2_handle.join().expect("reader 2 should finish");
    }
}
