//! Target acoustic response curves (§11.2, Item 35).
//!
//! Provides standard reference curves (Flat, Harman listener, Diffuse field, HF tilt)
//! and custom piecewise-linear target curves with log-frequency interpolation.

use serde::{Deserialize, Serialize};

/// Standard target curve classifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TargetCurveKind {
    /// Perfectly flat magnitude response across all frequencies.
    Flat,
    /// Harman preferred listener target: bass boost below 150 Hz and gradual HF roll-off.
    HarmanListener {
        bass_boost_db: f64,
        treble_tilt_db: f64,
    },
    /// Diffuse field acoustic response.
    DiffuseField,
    /// Linear slope in dB per octave relative to a pivot frequency.
    HighFrequencyTilt {
        slope_db_per_octave: f64,
        pivot_freq_hz: f64,
    },
    /// Custom user-specified (frequency_hz, gain_db) control points.
    Custom { points: Vec<(f64, f64)> },
}

impl Default for TargetCurveKind {
    fn default() -> Self {
        Self::HarmanListener {
            bass_boost_db: 4.0,
            treble_tilt_db: -2.0,
        }
    }
}

/// Target acoustic response curve definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetCurve {
    pub name: String,
    pub kind: TargetCurveKind,
}

impl Default for TargetCurve {
    fn default() -> Self {
        Self {
            name: "Harman Room Target".into(),
            kind: TargetCurveKind::default(),
        }
    }
}

impl TargetCurve {
    /// Creates a flat reference target.
    pub fn flat() -> Self {
        Self {
            name: "Flat Reference Target".into(),
            kind: TargetCurveKind::Flat,
        }
    }

    /// Evaluates the target curve gain in decibels (dB) at a specific frequency in Hz.
    pub fn evaluate_db(&self, freq_hz: f64) -> f64 {
        if freq_hz <= 0.0 {
            return 0.0;
        }

        match &self.kind {
            TargetCurveKind::Flat => 0.0,
            TargetCurveKind::HarmanListener {
                bass_boost_db,
                treble_tilt_db,
            } => {
                let mut db = 0.0;
                // Bass shelf: full boost below 60 Hz, transitioning to 0 dB at 200 Hz
                if freq_hz <= 60.0 {
                    db += *bass_boost_db;
                } else if freq_hz < 200.0 {
                    let factor =
                        (200.0f64.log10() - freq_hz.log10()) / (200.0f64.log10() - 60.0f64.log10());
                    db += *bass_boost_db * factor;
                }

                // Treble tilt: roll-off starting above 1 kHz up to 20 kHz
                if freq_hz >= 1000.0 {
                    let octaves = (freq_hz / 1000.0).log2();
                    // Tilt distributed across ~4.3 octaves (1k to 20k)
                    db += (*treble_tilt_db / 4.3) * octaves;
                }

                db
            }
            TargetCurveKind::DiffuseField => {
                // Approximate standard diffuse field ear/room compensation curve
                if freq_hz < 1000.0 {
                    0.0
                } else if freq_hz <= 3000.0 {
                    // +3 dB peak around 3 kHz ear canal resonance
                    let factor = (freq_hz - 1000.0) / 2000.0;
                    3.0 * factor
                } else if freq_hz <= 8000.0 {
                    3.0 - (freq_hz - 3000.0) / 5000.0 * 2.0
                } else {
                    1.0 - ((freq_hz - 8000.0) / 12000.0) * 4.0
                }
            }
            TargetCurveKind::HighFrequencyTilt {
                slope_db_per_octave,
                pivot_freq_hz,
            } => {
                let octaves = (freq_hz / *pivot_freq_hz).log2();
                slope_db_per_octave * octaves
            }
            TargetCurveKind::Custom { points } => {
                if points.is_empty() {
                    return 0.0;
                }
                if points.len() == 1 || freq_hz <= points[0].0 {
                    return points[0].1;
                }
                if freq_hz >= points[points.len() - 1].0 {
                    return points[points.len() - 1].1;
                }

                // Find bracketing control points and interpolate in log-frequency
                for i in 0..points.len() - 1 {
                    let (f0, g0) = points[i];
                    let (f1, g1) = points[i + 1];
                    if freq_hz >= f0 && freq_hz <= f1 {
                        let t = (freq_hz.log10() - f0.log10()) / (f1.log10() - f0.log10());
                        return g0 + t * (g1 - g0);
                    }
                }

                0.0
            }
        }
    }
}
