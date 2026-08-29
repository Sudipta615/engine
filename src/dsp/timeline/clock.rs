//! The master audio clock (v3.28) — Direction 3's `AudioClock` shape.
//!
//! A single source of musical truth for the offline / control path:
//!
//! ```text
//! AudioClock
//! ├── sample_position   (the looped playhead)
//! ├── sample_rate
//! ├── tempo_bpm
//! ├── musical_position  (bars / beats / ticks)
//! ├── transport_state   (Playing / Paused / Stopped)
//! ├── loop_region
//! └── tempo_ramp        (linear BPM change over time)
//! ```
//!
//! The clock keeps **two** positions: `position` (the playhead, which wraps
//! inside the loop region when looping) drives transport display and musical
//! position; `master_position` is a monotonic, never-wrapping counter of
//! total samples rendered and is what the [`Timeline`](super::Timeline)
//! scheduler keys events against — scheduled events fire once, exactly once,
//! regardless of looping.

use serde::{Deserialize, Serialize};

/// Transport state of the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportState {
    Playing,
    Paused,
    Stopped,
}

/// A linear tempo ramp: `tempo_bpm` slews from `from` to `to` over
/// `duration_samples`. After the ramp completes, the tempo holds at `to`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoRamp {
    pub from: f32,
    pub to: f32,
    pub duration_samples: f32,
    pub elapsed_samples: f32,
}

impl TempoRamp {
    /// The instantaneous BPM at the current elapsed time.
    pub fn bpm_now(&self) -> f32 {
        if self.duration_samples <= 0.0 {
            return self.to;
        }
        let t = (self.elapsed_samples / self.duration_samples).clamp(0.0, 1.0);
        self.from + (self.to - self.from) * t
    }
}

/// The master clock (v3.28). Deterministic, allocation-free time math.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioClock {
    sample_rate: f32,
    /// The playhead in samples. Wraps within the loop region when looping.
    position: u64,
    /// Monotonic total samples rendered (never wraps; event scheduling key).
    master_position: u64,
    tempo_bpm: f32,
    /// Beats per bar (time signature numerator; denominator fixed at 4 = a
    /// quarter-note beat, the standard convention).
    beats_per_bar: f32,
    state: TransportState,
    loop_enabled: bool,
    loop_start: Option<u64>,
    loop_end: Option<u64>,
    ramp: Option<TempoRamp>,
    /// MIDI-standard ticks (pulses) per quarter note — default 480.
    ticks_per_beat: u32,
}

impl AudioClock {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate: sample_rate.max(1.0),
            position: 0,
            master_position: 0,
            tempo_bpm: 120.0,
            beats_per_bar: 4.0,
            state: TransportState::Stopped,
            loop_enabled: false,
            loop_start: None,
            loop_end: None,
            ramp: None,
            ticks_per_beat: 480,
        }
    }

    // ── Getters ─────────────────────────────────────────────────────────────

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// The looped playhead (samples).
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Monotonic total samples rendered (event-scheduling key).
    pub fn master_position(&self) -> u64 {
        self.master_position
    }

    /// Current BPM. When a [`TempoRamp`] is active this is the instantaneous
    /// ramping value.
    pub fn tempo_bpm(&self) -> f32 {
        self.ramp.map(|r| r.bpm_now()).unwrap_or(self.tempo_bpm)
    }

    pub fn beats_per_bar(&self) -> f32 {
        self.beats_per_bar
    }

    pub fn state(&self) -> TransportState {
        self.state
    }

    pub fn is_playing(&self) -> bool {
        self.state == TransportState::Playing
    }

    pub fn ticks_per_beat(&self) -> u32 {
        self.ticks_per_beat
    }

    // ── Setters ─────────────────────────────────────────────────────────────

    pub fn set_sample_rate(&mut self, sr: f32) {
        if sr > 0.0 {
            self.sample_rate = sr;
        }
    }

    /// Set tempo immediately, cancelling any in-flight ramp.
    pub fn set_tempo(&mut self, bpm: f32) {
        self.tempo_bpm = bpm.max(0.01);
        self.ramp = None;
    }

    /// Set the time signature numerator.
    pub fn set_time_signature(&mut self, beats_per_bar: f32) {
        self.beats_per_bar = beats_per_bar.max(0.25);
    }

    pub fn set_ticks_per_beat(&mut self, ticks: u32) {
        if ticks > 0 {
            self.ticks_per_beat = ticks;
        }
    }

    /// Begin a linear tempo ramp from the current BPM to `target` over
    /// `duration_samples`.
    pub fn set_tempo_ramp(&mut self, target: f32, duration_samples: f32) {
        self.ramp = Some(TempoRamp {
            from: self.tempo_bpm(),
            to: target.max(0.01),
            duration_samples: duration_samples.max(0.0),
            elapsed_samples: 0.0,
        });
    }

    /// Enable looping within `[start, end)` samples (playhead wraps).
    pub fn set_loop(&mut self, start: u64, end: u64) {
        self.loop_start = Some(start);
        self.loop_end = Some(end.max(start));
        self.loop_enabled = true;
    }

    pub fn set_loop_enabled(&mut self, enabled: bool) {
        self.loop_enabled = enabled;
    }

    /// `Stopped` returns the playhead to `scratch` (default 0) and holds;
    /// `Paused` holds the playhead in place. `master_position` is never
    /// reset — scheduled events keep their once semantics across the
    /// Timeline's lifetime.
    pub fn set_state(&mut self, state: TransportState, scratch: u64) {
        self.state = state;
        if state == TransportState::Stopped {
            self.position = scratch;
            self.ramp = None;
        }
    }

    pub fn set_scratch(&mut self, scratch: u64) {
        self.position = scratch;
    }

    // ── Positioning ─────────────────────────────────────────────────────────

    /// Move the clock forward `samples`. No-ops unless `Playing`. When
    /// looping, the playhead wraps within `[start, end)`; `master_position`
    /// always advances (monotonic).
    pub fn advance(&mut self, samples: u64) {
        if self.state != TransportState::Playing {
            return;
        }
        self.master_position += samples;
        if let Some(r) = self.ramp.as_mut() {
            r.elapsed_samples += samples as f32;
            self.tempo_bpm = r.to.max(0.01); // hold target after completion
        }
        if self.loop_enabled {
            if let (Some(s), Some(e)) = (self.loop_start, self.loop_end) {
                let span = e.saturating_sub(s);
                if span > 0 && self.position >= s {
                    self.position = s + (self.position - s + samples) % span;
                    return;
                }
            }
        }
        self.position = self.position.saturating_add(samples);
    }

    // ── Musical time ────────────────────────────────────────────────────────

    /// Elapsed seconds from the playhead.
    pub fn seconds(&self) -> f64 {
        self.position as f64 / self.sample_rate as f64
    }

    /// Fractional beat position of the playhead (bpm × seconds / 60).
    pub fn position_beats(&self) -> f64 {
        self.tempo_bpm() as f64 * self.seconds() / 60.0
    }

    /// Whole bars, whole beats, and ticks within the current beat, computed
    /// from the playhead. `ticks`/`ticks_per_beat` yields the remainder
    /// within the beat.
    pub fn bars_beats_ticks(&self) -> (u64, u64, u32) {
        let beats = self.position_beats();
        let whole = beats.floor();
        let bar = (whole / self.beats_per_bar as f64).floor() as u64;
        let beat_in_bar = (whole as i64 % self.beats_per_bar as i64) as u64;
        let frac = beats - whole;
        let ticks = (frac * self.ticks_per_beat as f64).floor() as u32;
        (bar, beat_in_bar, ticks)
    }

    /// Samples for a number of beats at the current tempo.
    pub fn samples_for_beats(&self, beats: f64) -> f64 {
        beats * 60.0 * self.sample_rate as f64 / self.tempo_bpm() as f64
    }

    /// Beats for a number of samples at the current tempo.
    pub fn beats_for_samples(&self, samples: u64) -> f64 {
        self.tempo_bpm() as f64 * samples as f64 / (60.0 * self.sample_rate as f64)
    }

    /// Samples for a whole number of bars at the current tempo.
    pub fn samples_for_bars(&self, bars: f64) -> f64 {
        self.samples_for_beats(bars * self.beats_per_bar as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_moves_playing_only() {
        let mut c = AudioClock::new(48_000.0);
        c.set_state(TransportState::Paused, 0);
        c.advance(1000);
        assert_eq!(c.master_position(), 0);
        c.set_state(TransportState::Playing, 0);
        c.advance(1000);
        assert_eq!(c.master_position(), 1000);
        assert_eq!(c.position(), 1000);
    }

    #[test]
    fn loop_wraps_playhead_but_not_master() {
        let mut c = AudioClock::new(48_000.0);
        c.set_state(TransportState::Playing, 0);
        c.set_loop(0, 4800);
        c.advance(9600); // 2 full loops
        assert_eq!(c.position(), 0, "playhead wrapped");
        assert_eq!(c.master_position(), 9600, "master monotonic");
    }

    #[test]
    fn beats_and_bars_are_correct_at_120bpm() {
        // 120 BPM = 2 beats/sec = 1 beat per 24000 samples.
        let mut c = AudioClock::new(48_000.0);
        c.set_tempo(120.0);
        c.set_state(TransportState::Playing, 0);
        c.advance(48_000); // 1 second = 2 beats
        assert!((c.position_beats() - 2.0).abs() < 1e-6);
        assert!((c.samples_for_beats(1.0) - 24_000.0).abs() < 1e-6);
        assert_eq!(c.bars_beats_ticks().0, 0, "bar 0");
        assert_eq!(c.bars_beats_ticks().1, 2, "beat 2 in bar 0");
        c.advance(144_000); // total 4s = 8 beats = 2 bars of 4/4
        assert_eq!(c.bars_beats_ticks().0, 2);
        assert_eq!(c.bars_beats_ticks().1, 0);
        assert_eq!(c.bars_beats_ticks().2, 0);
    }

    #[test]
    fn tempo_ramp_slews_linear() {
        let mut c = AudioClock::new(48_000.0);
        c.set_tempo(60.0);
        c.set_tempo_ramp(120.0, 48_000.0); // over 1 second
        c.set_state(TransportState::Playing, 0);
        c.advance(24_000); // halfway
        assert!(
            (c.tempo_bpm() - 90.0).abs() < 1e-3,
            "mid-ramp {}",
            c.tempo_bpm()
        );
        c.advance(48_000); // past end holds at target
        assert!((c.tempo_bpm() - 120.0).abs() < 1e-3);
    }

    #[test]
    fn stop_returns_to_scratch() {
        let mut c = AudioClock::new(48_000.0);
        c.set_state(TransportState::Playing, 0);
        c.advance(4800);
        assert_eq!(c.position(), 4800);
        c.set_state(TransportState::Stopped, 1000);
        assert_eq!(c.position(), 1000);
    }
}
