//! Real-time audio analyzer — peak/RMS level meters and an FFT magnitude
//! spectrum, shared between the engine and host applications.
//!
//! # Design
//!
//! - [`AudioAnalyzer::update`] is called from the engine's decode (tick)
//!   thread once per pushed block — **not** from a realtime audio callback —
//!   so a short mutex + a rate-limited FFT are acceptable there.
//! - Peak/RMS values are published as `AtomicU32` bit-casts for lock-free
//!   reads from any host thread ([`AudioAnalyzer::snapshot`]).
//! - The FFT (a Hann-windowed magnitude spectrum via `realfft`, the same
//!   crate the convolution and true-peak engines use) is computed at most
//!   `~30` times per second regardless of block size, so the CPU cost is
//!   negligible on any desktop.
//!
//! The engine calls [`AudioAnalyzer::set_sample_rate`] whenever a track
//! loads so the spectrum frequency axis is correct.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Default FFT size for the spectrum (1024 → 513 bins, ~21 Hz resolution at
/// 44.1 kHz).
pub const ANALYZER_FFT_SIZE: usize = 1024;

/// Default spectrum refresh cadence (Hz).
pub const ANALYZER_UPDATE_HZ: u32 = 30;

/// A point-in-time snapshot of the analyzer's meters and spectrum.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzerSnapshot {
    /// Left-channel peak level in dBFS (0 dBFS = full scale).
    pub peak_db_l: f32,
    /// Right-channel peak level in dBFS.
    pub peak_db_r: f32,
    /// Left-channel RMS level in dBFS (windowed).
    pub rms_db_l: f32,
    /// Right-channel RMS level in dBFS (windowed).
    pub rms_db_r: f32,
    /// FFT magnitude spectrum in dBFS (bins `0..=fft_size/2`), most recent
    /// window. Bin `i` is centered at `i * sample_rate / fft_size` Hz.
    pub spectrum_db: Vec<f32>,
    /// Sample rate used for the frequency axis (Hz).
    pub sample_rate: u32,
    /// FFT size used for the spectrum.
    pub fft_size: usize,
}

impl AnalyzerSnapshot {
    /// Frequency (Hz) of the loudest spectrum bin, if the spectrum is
    /// non-empty.
    pub fn dominant_frequency_hz(&self) -> Option<f32> {
        if self.spectrum_db.is_empty() {
            return None;
        }
        let (bin, _) = self
            .spectrum_db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))?;
        Some(bin as f32 * self.sample_rate as f32 / self.fft_size.max(1) as f32)
    }
}

struct AnalyzerState {
    fft_size: usize,
    rfft: Option<std::sync::Arc<dyn realfft::RealToComplex<f32>>>,
    window: Vec<f32>,
    /// Rolling mono mix of the most recent `fft_size` frames.
    recent: Vec<f32>,
    sum_sq_l: f64,
    sum_sq_r: f64,
    peak_win_l: f32,
    peak_win_r: f32,
    frames_in_window: usize,
    update_interval: usize,
    spectrum: Vec<f32>,
    scratch_complex: Vec<realfft::num_complex::Complex32>,
}

/// Real-time analyzer shared (via `Arc`) between the engine and hosts.
pub struct AudioAnalyzer {
    state: Mutex<AnalyzerState>,
    peak_l: AtomicU32,
    peak_r: AtomicU32,
    rms_l: AtomicU32,
    rms_r: AtomicU32,
    sample_rate: AtomicU32,
    enabled: AtomicU32,
}

impl AudioAnalyzer {
    /// Create an analyzer with the default 1024-point spectrum.
    pub fn new_default() -> Self {
        Self::new(ANALYZER_FFT_SIZE)
    }

    /// Create an analyzer with the given FFT size (power of two, 256..=8192).
    pub fn new(fft_size: usize) -> Self {
        let fft_size = fft_size.clamp(256, 8192).next_power_of_two();
        let mut planner = realfft::RealFftPlanner::<f32>::new();
        let rfft = planner.plan_fft_forward(fft_size);
        // Hann window (periodic, not symmetric, to match the FFT length).
        let window: Vec<f32> = (0..fft_size)
            .map(|i| 0.5 - 0.5 * ((2.0 * std::f32::consts::PI * i as f32) / fft_size as f32).cos())
            .collect();
        let spectrum = vec![0.0; fft_size / 2 + 1];
        let scratch_complex =
            vec![realfft::num_complex::Complex32::new(0.0, 0.0); fft_size / 2 + 1];
        Self {
            state: Mutex::new(AnalyzerState {
                fft_size,
                rfft: Some(rfft),
                window,
                recent: Vec::with_capacity(fft_size),
                sum_sq_l: 0.0,
                sum_sq_r: 0.0,
                peak_win_l: 0.0,
                peak_win_r: 0.0,
                frames_in_window: 0,
                update_interval: DEFAULT_SAMPLE_RATE_USIZE / ANALYZER_UPDATE_HZ as usize,
                spectrum,
                scratch_complex,
            }),
            peak_l: AtomicU32::new(0.0f32.to_bits()),
            peak_r: AtomicU32::new(0.0f32.to_bits()),
            rms_l: AtomicU32::new(0.0f32.to_bits()),
            rms_r: AtomicU32::new(0.0f32.to_bits()),
            sample_rate: AtomicU32::new(DEFAULT_SAMPLE_RATE_U32),
            enabled: AtomicU32::new(1),
        }
    }

    /// Enable or disable analysis (meters + spectrum). Disabling skips the
    /// per-block accumulation entirely — a zero-cost bypass.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled as u32, Ordering::Relaxed);
    }

    /// Whether analysis is currently enabled.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed) != 0
    }

    /// Inform the analyzer of the current sample rate (for the frequency
    /// axis and the RMS window cadence).
    pub fn set_sample_rate(&self, sample_rate: u32) {
        if sample_rate == 0 {
            return;
        }
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
        let mut st = self.state.lock().unwrap();
        st.update_interval = (sample_rate as usize / ANALYZER_UPDATE_HZ as usize).max(64);
    }

    /// Feed one interleaved block of audio. `channels` ≥ 1; samples are
    /// assumed interleaved (`L,R,L,R,...` for stereo; channel 0/1 are treated
    /// as L/R for wider layouts).
    pub fn update(&self, samples: &[f32], channels: usize) {
        if !self.enabled() || samples.is_empty() {
            return;
        }
        let ch = channels.max(1);
        let mut st = match self.state.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };

        let mut frames = 0usize;
        let mut i = 0;
        while i + ch <= samples.len() {
            let l = samples[i];
            let r = if ch > 1 { samples[i + 1] } else { l };
            i += ch;
            frames += 1;

            let al = l.abs();
            let ar = r.abs();
            if al > st.peak_win_l {
                st.peak_win_l = al;
            }
            if ar > st.peak_win_r {
                st.peak_win_r = ar;
            }
            st.sum_sq_l += (l as f64) * (l as f64);
            st.sum_sq_r += (r as f64) * (r as f64);
            // Spectrum tracks the left channel (mono-mix `(L+R)/2` cancels
            // for out-of-phase stereo content, so it is not a robust
            // visualization basis).
            st.recent.push(l);
        }
        let overflow = st.recent.len().saturating_sub(st.fft_size);
        if overflow > 0 {
            st.recent.drain(..overflow);
        }

        st.frames_in_window += frames;
        if st.frames_in_window >= st.update_interval {
            let n = st.frames_in_window.max(1) as f64;
            let rms_l = ((st.sum_sq_l / n).sqrt() as f32).min(1.0);
            let rms_r = ((st.sum_sq_r / n).sqrt() as f32).min(1.0);
            self.rms_l.store(rms_l.to_bits(), Ordering::Relaxed);
            self.rms_r.store(rms_r.to_bits(), Ordering::Relaxed);
            self.peak_l
                .store(st.peak_win_l.min(1.0).to_bits(), Ordering::Relaxed);
            self.peak_r
                .store(st.peak_win_r.min(1.0).to_bits(), Ordering::Relaxed);
            st.compute_spectrum();
            st.sum_sq_l = 0.0;
            st.sum_sq_r = 0.0;
            st.peak_win_l = 0.0;
            st.peak_win_r = 0.0;
            st.frames_in_window = 0;
        }
    }

    /// Current levels and spectrum. Cheap — two atomic loads plus a mutex
    /// guard for the spectrum copy.
    pub fn snapshot(&self) -> AnalyzerSnapshot {
        let state = match self.state.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        AnalyzerSnapshot {
            peak_db_l: dbfs(self.peak_l.load(Ordering::Relaxed)),
            peak_db_r: dbfs(self.peak_r.load(Ordering::Relaxed)),
            rms_db_l: dbfs(self.rms_l.load(Ordering::Relaxed)),
            rms_db_r: dbfs(self.rms_r.load(Ordering::Relaxed)),
            spectrum_db: state.spectrum.clone(),
            sample_rate: self.sample_rate.load(Ordering::Relaxed),
            fft_size: state.fft_size,
        }
    }
}

impl AnalyzerState {
    /// Compute the magnitude spectrum (dBFS) of the recent-window buffer.
    fn compute_spectrum(&mut self) {
        let Some(rfft) = self.rfft.as_mut() else {
            return;
        };
        let n = self.fft_size;
        // Pad/trim the recent buffer to exactly `n` samples.
        let mut input = self.window.clone(); // reuse window as a zeroed template
        input.fill(0.0);
        let copy_len = self.recent.len().min(n);
        input[..copy_len].copy_from_slice(&self.recent[..copy_len]);
        // Apply Hann window.
        for (v, w) in input.iter_mut().zip(self.window.iter()) {
            *v *= w;
        }
        let mut spectrum = self.scratch_complex.clone();
        if rfft.process(&mut input, &mut spectrum).is_err() {
            return;
        }
        let inv = 1.0 / n as f32;
        for (out, bin) in self.spectrum.iter_mut().zip(spectrum.iter()) {
            let mag = bin.norm() * inv;
            *out = 20.0 * (mag.max(1e-9)).log10();
        }
    }
}

const DEFAULT_SAMPLE_RATE_U32: u32 = 48_000;
const DEFAULT_SAMPLE_RATE_USIZE: usize = DEFAULT_SAMPLE_RATE_U32 as usize;

/// Decode an atomic-stored f32 to dBFS (`20 * log10(v)`), with `-inf`
/// clamped to `-120 dB` and silence reported as `-120 dB`.
fn dbfs(bits: u32) -> f32 {
    let v = f32::from_bits(bits);
    if v <= 1e-6 {
        -120.0
    } else {
        (20.0 * v.log10()).max(-120.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_sine(a: &AudioAnalyzer, rate: u32, freq: f32, amp: f32, seconds: f32) {
        let n = (rate as f32 * seconds) as usize;
        let mut buf = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * amp;
            buf.push(s);
            buf.push(-s); // anti-phase right channel
        }
        for chunk in buf.chunks(1024) {
            a.update(chunk, 2);
        }
    }

    #[test]
    fn meter_levels_match_sine_amplitude() {
        let a = AudioAnalyzer::new_default();
        a.set_sample_rate(48_000);
        let amp = 0.5f32;
        feed_sine(&a, 48_000, 1000.0, amp, 1.5);
        let snap = a.snapshot();

        // Peak ≈ 20*log10(0.5) = -6.02 dB.
        assert!(
            (snap.peak_db_l - (20.0 * amp.log10())).abs() < 1.0,
            "peak L: {}",
            snap.peak_db_l
        );
        assert!(
            (snap.peak_db_r - (20.0 * amp.log10())).abs() < 1.0,
            "peak R: {}",
            snap.peak_db_r
        );
        // RMS of a full-scale sine of amplitude 0.5 = 0.5/√2 → -9.03 dB.
        let expected_rms = 20.0 * (amp / std::f32::consts::SQRT_2).log10();
        assert!(
            (snap.rms_db_l - expected_rms).abs() < 1.0,
            "rms L: {} (expected {})",
            snap.rms_db_l,
            expected_rms
        );
    }

    #[test]
    fn spectrum_peaks_at_sine_frequency() {
        let a = AudioAnalyzer::new(1024);
        a.set_sample_rate(48_000);
        feed_sine(&a, 48_000, 2000.0, 0.4, 1.0);
        let snap = a.snapshot();
        assert_eq!(snap.spectrum_db.len(), 1024 / 2 + 1);

        let dominant = snap.dominant_frequency_hz().unwrap();
        assert!(
            (dominant - 2000.0).abs() < 100.0,
            "dominant frequency: {} Hz",
            dominant
        );
    }

    #[test]
    fn silence_reports_mute_levels() {
        let a = AudioAnalyzer::new_default();
        a.set_sample_rate(48_000);
        for _ in 0..(48_000 / 1024 + 2) {
            a.update(&[0.0; 2048], 2);
        }
        let snap = a.snapshot();
        assert!(snap.peak_db_l <= -60.0, "peak: {}", snap.peak_db_l);
        assert!(snap.rms_db_l <= -60.0, "rms: {}", snap.rms_db_l);
    }

    #[test]
    fn disable_is_zero_cost_and_mute() {
        let a = AudioAnalyzer::new_default();
        a.set_sample_rate(48_000);
        a.set_enabled(false);
        let amp = 0.8f32;
        feed_sine(&a, 48_000, 440.0, amp, 0.5);
        let snap = a.snapshot();
        assert!(snap.peak_db_l <= -60.0, "disabled analyzer must not meter");
    }
}
