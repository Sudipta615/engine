//! Low-Frequency Oscillator (LFO) for modulation (Item 31).

use serde::{Deserialize, Serialize};
use std::f32::consts::PI;

/// LFO wave shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum LfoWaveform {
    #[default]
    Sine,
    Triangle,
    SawUp,
    SawDown,
    Square,
    SampleAndHold,
}

/// Tempo-synced beat division for LFO frequency.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LfoSyncRate {
    Whole,        // 4 beats
    Half,         // 2 beats
    Quarter,      // 1 beat
    Eighth,       // 1/2 beat
    Sixteenth,    // 1/4 beat
    ThirtySecond, // 1/8 beat
    TripletEighth,
    DottedEighth,
}

impl LfoSyncRate {
    /// Duration of one period in musical beats.
    pub fn beats(self) -> f64 {
        match self {
            Self::Whole => 4.0,
            Self::Half => 2.0,
            Self::Quarter => 1.0,
            Self::Eighth => 0.5,
            Self::Sixteenth => 0.25,
            Self::ThirtySecond => 0.125,
            Self::TripletEighth => 1.0 / 3.0,
            Self::DottedEighth => 0.75,
        }
    }
}

/// Low-Frequency Oscillator with free-running and tempo-synced modes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lfo {
    pub waveform: LfoWaveform,
    pub frequency_hz: f32,
    pub sync_rate: Option<LfoSyncRate>,
    pub bipolar: bool,
    pub phase: f32,
    pub phase_offset: f32,
    sample_rate: f32,
    sh_current: f32,
    sh_rng_state: u32,
}

impl Default for Lfo {
    fn default() -> Self {
        Self::new(48000.0, 1.0)
    }
}

impl Lfo {
    pub fn new(sample_rate: f32, frequency_hz: f32) -> Self {
        Self {
            waveform: LfoWaveform::Sine,
            frequency_hz: frequency_hz.max(0.001),
            sync_rate: None,
            bipolar: true,
            phase: 0.0,
            phase_offset: 0.0,
            sample_rate: sample_rate.max(1.0),
            sh_current: 0.0,
            sh_rng_state: 0x12345678,
        }
    }

    /// Reset LFO phase to its configured offset.
    pub fn reset_phase(&mut self) {
        self.phase = (self.phase_offset / (2.0 * PI)).fract();
        if self.phase < 0.0 {
            self.phase += 1.0;
        }
    }

    /// Update effective frequency given active tempo in BPM (if tempo-synced).
    pub fn update_tempo(&mut self, bpm: f64) {
        if let Some(sync) = self.sync_rate {
            let beats_per_sec = (bpm / 60.0).max(0.1);
            let period_sec = sync.beats() / beats_per_sec;
            self.frequency_hz = (1.0 / period_sec) as f32;
        }
    }

    /// Advance LFO by one sample and return output value.
    #[inline]
    pub fn next_sample(&mut self) -> f32 {
        let phase = (self.phase + self.phase_offset / (2.0 * PI)).fract();
        let norm_phase = if phase < 0.0 { phase + 1.0 } else { phase };

        let val = match self.waveform {
            LfoWaveform::Sine => (norm_phase * 2.0 * PI).sin(),
            LfoWaveform::Triangle => {
                if norm_phase < 0.5 {
                    4.0 * norm_phase - 1.0
                } else {
                    3.0 - 4.0 * norm_phase
                }
            }
            LfoWaveform::SawUp => 2.0 * norm_phase - 1.0,
            LfoWaveform::SawDown => 1.0 - 2.0 * norm_phase,
            LfoWaveform::Square => {
                if norm_phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            LfoWaveform::SampleAndHold => self.sh_current,
        };

        // Advance phase
        let phase_inc = self.frequency_hz / self.sample_rate;
        let old_phase = self.phase;
        self.phase = (self.phase + phase_inc).fract();

        // Sample and hold step on cycle wrap
        if self.phase < old_phase && self.waveform == LfoWaveform::SampleAndHold {
            // Xorshift PRNG
            self.sh_rng_state ^= self.sh_rng_state << 13;
            self.sh_rng_state ^= self.sh_rng_state >> 17;
            self.sh_rng_state ^= self.sh_rng_state << 5;
            let rand_val = (self.sh_rng_state as f32) / (u32::MAX as f32);
            self.sh_current = rand_val * 2.0 - 1.0;
        }

        if self.bipolar {
            val
        } else {
            (val + 1.0) * 0.5
        }
    }

    /// Render a block of LFO output values into `out`. Zero allocation.
    pub fn render_block(&mut self, out: &mut [f32]) {
        for sample in out.iter_mut() {
            *sample = self.next_sample();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lfo_bipolar_sine_range() {
        let mut lfo = Lfo::new(48000.0, 10.0);
        lfo.waveform = LfoWaveform::Sine;
        lfo.bipolar = true;

        let mut buf = [0.0f32; 4800];
        lfo.render_block(&mut buf);

        for &x in &buf {
            assert!((-1.0001..=1.0001).contains(&x));
        }
    }

    #[test]
    fn test_lfo_unipolar_triangle_range() {
        let mut lfo = Lfo::new(48000.0, 10.0);
        lfo.waveform = LfoWaveform::Triangle;
        lfo.bipolar = false;

        let mut buf = [0.0f32; 4800];
        lfo.render_block(&mut buf);

        for &x in &buf {
            assert!((-0.0001..=1.0001).contains(&x));
        }
    }

    #[test]
    fn test_lfo_tempo_sync() {
        let mut lfo = Lfo::new(48000.0, 1.0);
        lfo.sync_rate = Some(LfoSyncRate::Quarter); // 1 beat at 120 bpm = 2 Hz
        lfo.update_tempo(120.0);
        assert!((lfo.frequency_hz - 2.0).abs() < 1e-3);
    }
}
