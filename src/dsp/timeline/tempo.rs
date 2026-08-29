//! Tempo map (v3.28) — the guide's "tempo changes" / "tempo ramps over
//! musical time" substrate.
//!
//! A [`TempoMap`] is an ordered list of [`TempoPoint`]s: "at this beat the
//! tempo becomes this BPM". Between points the tempo is constant, so
//! beat↔sample conversion is exact piecewise-constant integration — the
//! basis for scheduling a musical position when the tempo changes. (The
//! clock's single in-flight linear [`TempoRamp`](super::clock::TempoRamp)
//! handles instantaneous ramps; a fully *ramped* tempo map over long spans
//! is squared integration and is future work.)

/// A tempo change point: from `beat` onward the tempo is `bpm`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoPoint {
    pub beat: f64,
    pub bpm: f64,
}

/// An ordered tempo map. Points are kept sorted by beat; the first point is
/// conventionally at beat 0 (the map falls back to 120 BPM before any
/// point).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TempoMap {
    points: Vec<TempoPoint>,
}

impl TempoMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a tempo change. Beats must be non-decreasing; points are kept
    /// sorted, so a host may push in any order (the list sorts on insert).
    pub fn push(&mut self, beat: f64, bpm: f64) {
        self.points.push(TempoPoint {
            beat: beat.max(0.0),
            bpm: bpm.max(0.01),
        });
        // Stable sort keeps the map deterministic for equal beats.
        self.points
            .sort_by(|a, b| a.beat.partial_cmp(&b.beat).unwrap());
    }

    /// Remove all points.
    pub fn clear(&mut self) {
        self.points.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// The BPM in effect at `beats` (the last point with `beat <= beats`,
    /// or 120 if none / if the point precedes it).
    pub fn bpm_at_beat(&self, beats: f64) -> f64 {
        let mut bpm = 120.0;
        for p in &self.points {
            if p.beat <= beats {
                bpm = p.bpm;
            } else {
                break;
            }
        }
        bpm
    }

    /// Sample position (at `sample_rate`) of a beat position, integrating
    /// the constant-BPM segments exactly.
    pub fn sample_at_beat(&self, beats: f64, sample_rate: f32) -> f64 {
        let beats = beats.max(0.0);
        let mut samples = 0.0f64;
        let sr = sample_rate as f64;
        let mut prev_beat = 0.0f64;
        let mut prev_bpm = 120.0f64;
        let mut reached = false;
        for p in &self.points {
            let b = p.beat.min(beats);
            if b >= prev_beat {
                samples += (b - prev_beat) * 60.0 * sr / prev_bpm;
                prev_beat = b;
            }
            prev_bpm = p.bpm;
            if p.beat >= beats {
                // Include the partial remainder into/at this point's bpm
                // below, then stop.
                reached = true;
                prev_bpm = p.bpm;
                break;
            }
        }
        if !reached {
            // Past the last point: extend at the final tempo.
            samples += (beats - prev_beat) * 60.0 * sr / prev_bpm;
        }
        samples
    }

    /// Beat position of a sample, inverting [`TempoMap::sample_at_beat`].
    pub fn beat_at_sample(&self, samples: f64, sample_rate: f32) -> f64 {
        let sr = sample_rate as f64;
        let mut remaining = samples.max(0.0);
        let mut prev_beat = 0.0f64;
        let mut prev_bpm = 120.0f64;
        for p in &self.points {
            let seg_beats = p.beat - prev_beat;
            let seg_samples = seg_beats * 60.0 * sr / prev_bpm;
            if remaining <= seg_samples {
                return prev_beat + remaining * prev_bpm / (60.0 * sr);
            }
            remaining -= seg_samples;
            prev_beat = p.beat;
            prev_bpm = p.bpm;
        }
        // Beyond the last point at the final tempo.
        prev_beat + remaining * prev_bpm / (60.0 * sr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    #[test]
    fn constant_map_converts_like_the_clock() {
        let m = TempoMap::new();
        // Empty map defaults to 120 BPM → 1 beat = 24000 samples.
        assert!((m.sample_at_beat(2.0, SR) - 48_000.0).abs() < 1e-6);
        assert!((m.beat_at_sample(48_000.0, SR) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn tempo_change_segments_integrate() {
        let mut m = TempoMap::new();
        m.push(0.0, 120.0); // beat 0..2 at 120 (1 beat = 24000)
        m.push(2.0, 240.0); // beat 2..  at 240 (1 beat = 12000)
                            // beat 4 = 2 beats @120 (48000) + 2 beats @240 (24000) = 72000.
        assert!((m.sample_at_beat(4.0, SR) - 72_000.0).abs() < 1e-6);
        // Round-trip.
        assert!((m.beat_at_sample(m.sample_at_beat(6.0, SR), SR) - 6.0).abs() < 1e-6);
        // bpm_at_beat.
        assert!((m.bpm_at_beat(1.0) - 120.0).abs() < 1e-9);
        assert!((m.bpm_at_beat(3.0) - 240.0).abs() < 1e-9);
    }

    #[test]
    fn first_point_need_not_start_at_zero() {
        let mut m = TempoMap::new();
        m.push(4.0, 200.0);
        // Before the first point, 120 BPM (the default); 8 beats @120 = 4s.
        assert!((m.sample_at_beat(4.0, SR) - 96_000.0).abs() < 1e-6);
    }
}
