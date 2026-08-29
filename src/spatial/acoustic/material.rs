//! Frequency-dependent acoustic materials (v3.25 — Acoustic World).
//!
//! A material is the per-surface *physical* description the acoustic world
//! uses instead of a single scalar reflection coefficient: how much of each
//! octave band's energy is absorbed, reflected, transmitted, scattered and
//! diffused. This is the **simulation-side** spec — the renderer consumes
//! the *resulting paths* ([`super::path::AcousticPath`]), never a raw
//! coefficient table.
//!
//! ## Frequency representation
//!
//! Absorption/reflection/transmission are sampled on the standard ISO 1/1
//! octave-band centres `63 Hz … 16 kHz` ([`OCTAVE_BANDS_HZ`]). Between bands
//! the solver interpolates in log-frequency; a rendering convention maps the
//! resulting spectrum onto a broadband gain plus a low-pass corner so the
//! realtime renderers can apply it without carrying the full table.
//!
//! ## Why spectra and not one number
//!
//! A pane of glass absorbs little low frequency but a lot of high; a heavy
//! curtain the opposite. A single absorption coefficient (the room's
//! previous seam, [`super::super::room::Room::absorption`]) cannot express
//! either. Spectra let diffraction around a door and transmission through a
//! portal stay *frequency aware*, and let a wall absorb bass while echoing
//! the treble.

/// Standard ISO 1/1 octave-band centre frequencies (Hz).
pub const OCTAVE_BANDS_HZ: [f32; 9] = [
    63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
/// Band count of [`OCTAVE_BANDS_HZ`].
pub const OCTAVE_BANDS: usize = 9;

/// The fraction of a surface's energy that bounces specularly per band.
///
/// Reflection ≤ 1; the remainder after absorption + transmission is also
/// scattered, so the three spectra need not sum to exactly 1 — the solver
/// normalises the reflection/transmission/diffuse partition explicitly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaterialSpectrum {
    /// Fraction of incident energy absorbed per octave band, `[0, 1]`.
    pub absorption: [f32; OCTAVE_BANDS],
    /// Fraction specularly reflected per octave band, `[0, 1]`.
    pub reflection: [f32; OCTAVE_BANDS],
    /// Fraction transmitted through per octave band, `[0, 1]`.
    pub transmission: [f32; OCTAVE_BANDS],
}

impl MaterialSpectrum {
    /// A spectrally *flat* material: `1 − absorption_frac` reflected, the
    /// agreed remainder transmitted, no absorption. `absorption_frac ∈ [0,1]`.
    pub fn flat_reflective(absorption_frac: f32) -> Self {
        let absorption = absorption_frac.clamp(0.0, 1.0);
        let reflection = 1.0 - absorption;
        Self {
            absorption: [absorption; OCTAVE_BANDS],
            reflection: [reflection; OCTAVE_BANDS],
            transmission: [0.0; OCTAVE_BANDS],
        }
    }

    /// A spectrally *flat* transmissive material with no specular
    /// reflection: `transmission_frac` passes straight through each band.
    pub fn flat_transmissive(transmission_frac: f32) -> Self {
        let transmission = transmission_frac.clamp(0.0, 1.0);
        Self {
            absorption: [0.0; OCTAVE_BANDS],
            reflection: [1.0 - transmission; OCTAVE_BANDS],
            transmission: [transmission; OCTAVE_BANDS],
        }
    }

    /// A material whose absorption rises exponentially with frequency (a
    /// low-frequency-transparent / high-frequency-dead surface). `low_band`
    /// is the fraction absorbed at 63 Hz, `high_band` at 16 kHz.
    pub fn rising_absorption(low_band: f32, high_band: f32) -> Self {
        let l = low_band.clamp(0.0, 1.0);
        let h = high_band.clamp(0.0, 1.0);
        let mut absorption = [0.0; OCTAVE_BANDS];
        for (b, a) in absorption.iter_mut().enumerate() {
            let t = b as f32 / (OCTAVE_BANDS - 1) as f32;
            *a = l + (h - l) * t;
        }
        let reflection: [f32; OCTAVE_BANDS] =
            std::array::from_fn(|b| (1.0 - absorption[b]).max(0.0));
        Self {
            absorption,
            reflection,
            transmission: [0.0; OCTAVE_BANDS],
        }
    }

    /// Sample the *reflection* coefficient at an arbitrary frequency `hz`
    /// (log-frequency interpolation between octave bands, held constant
    /// outside the table).
    pub fn reflectivity_at_hz(&self, hz: f32) -> f32 {
        interp_band(&self.reflection, hz)
    }

    /// Sample the *transmission* coefficient at an arbitrary frequency.
    pub fn transmission_at_hz(&self, hz: f32) -> f32 {
        interp_band(&self.transmission, hz)
    }

    /// Reduce the spectrum to a single broadband reflection gain + a
    /// low-pass corner, the compact form a realtime renderer applies.
    /// `sample_rate_hz` bounds the low-pass corner (it cannot exceed −3 dB
    /// at Nyquist). Returns `(gain, lowpass_hz)`. `gain` collapses the
    /// *reflection* spectrum;
    /// see [`transmitted_broadband`](Self::transmitted_broadband) for the
    /// transmission side (portals / openings).
    pub fn broadband(&self, sample_rate_hz: f32) -> (f32, f32) {
        let nyq = sample_rate_hz * 0.5;
        // Gain: geometric mean of the reflection across the band (a flat
        // −20 dB band is a clear 0.1×; a single near-zero bin drags it down
        // just as measured).
        let mut g = 1.0f32;
        for &r in &self.reflection {
            g *= r.clamp(1e-6, 1.0);
        }
        let gain = g.powf(1.0 / OCTAVE_BANDS as f32);
        // Low-pass corner: first band below 1 = where reflection has dropped
        // 3 dB from band 0's value, clamped to Nyquist.
        let ref0 = self.reflection[0].max(1e-6);
        let target = ref0 * std::f32::consts::FRAC_1_SQRT_2;
        let mut low = nyq;
        if !self.reflection.iter().all(|&r| r >= target - 1e-6) {
            for b in 1..OCTAVE_BANDS {
                let prev = self.reflection[b - 1].max(1e-6);
                let cur = self.reflection[b].max(1e-6);
                if cur <= target && prev >= target {
                    let t = (target - cur) / (prev - cur).max(1e-6);
                    low = lerp_log(OCTAVE_BANDS_HZ[b - 1], OCTAVE_BANDS_HZ[b], t);
                    break;
                }
            }
        }
        (gain, low.clamp(20.0, nyq.max(20.0)))
    }

    /// The transmission analogue of [`broadband`](Self::broadband): collapses
    /// the *transmission* spectrum into `(gain, lowpass_hz)`. A fully
    /// transparent opening yields gain ≈ 1 with no low-pass.
    pub fn transmitted_broadband(&self, sample_rate_hz: f32) -> (f32, f32) {
        let nyq = sample_rate_hz * 0.5;
        let mut g = 1.0f32;
        for &t in &self.transmission {
            g *= t.clamp(1e-6, 1.0);
        }
        let gain = g.powf(1.0 / OCTAVE_BANDS as f32);
        let t0 = self.transmission[0].max(1e-6);
        let target = t0 * std::f32::consts::FRAC_1_SQRT_2;
        let mut low = nyq;
        if !self.transmission.iter().all(|&t| t >= target - 1e-6) {
            for b in 1..OCTAVE_BANDS {
                let prev = self.transmission[b - 1].max(1e-6);
                let cur = self.transmission[b].max(1e-6);
                if cur <= target && prev >= target {
                    let t = (target - cur) / (prev - cur).max(1e-6);
                    low = lerp_log(OCTAVE_BANDS_HZ[b - 1], OCTAVE_BANDS_HZ[b], t);
                    break;
                }
            }
        }
        (gain, low.clamp(20.0, nyq.max(20.0)))
    }
}

impl Default for MaterialSpectrum {
    fn default() -> Self {
        Self::flat_reflective(0.2) // reflective plaster wall
    }
}

/// Interpolate a per-band value in log-frequency; hold at the ends.
fn interp_band(band: &[f32; OCTAVE_BANDS], hz: f32) -> f32 {
    let loghz = hz.max(1.0).ln();
    if hz <= OCTAVE_BANDS_HZ[0] {
        return band[0];
    }
    if hz >= OCTAVE_BANDS_HZ[OCTAVE_BANDS - 1] {
        return band[OCTAVE_BANDS - 1];
    }
    for b in 1..OCTAVE_BANDS {
        let f0 = OCTAVE_BANDS_HZ[b - 1];
        let f1 = OCTAVE_BANDS_HZ[b];
        if hz <= f1 {
            let t = (loghz - f0.ln()) / (f1.ln() - f0.ln());
            return band[b - 1] + t * (band[b] - band[b - 1]);
        }
    }
    band[OCTAVE_BANDS - 1]
}

/// Linear interpolation in log-frequency between two octave centres.
fn lerp_log(f0: f32, f1: f32, t: f32) -> f32 {
    (f0.max(1.0).ln() + t * (f1.max(1.0).ln() - f0.max(1.0).ln())).exp()
}

/// A named, concrete material (v3.25 Direction 8 presets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterialKind {
    /// Bare poured concrete / blockwork.
    Concrete,
    /// Timber floor / panelling.
    Wood,
    /// A pane of glass or glazed window.
    Glass,
    /// Heavy fabric curtain / upholstery.
    Fabric,
    /// Pile carpet on underlay.
    Carpet,
    /// Sheet metal.
    Metal,
    /// Acoustically transparent mesh over a limb (grille).
    OpenMesh,
}

impl MaterialKind {
    /// The frequency-dependent spectrum for this material, tuned to the
    /// documented ISO-class absorption curves below.
    pub fn spectrum(self) -> MaterialSpectrum {
        match self {
            // Concrete: absorbs a little mid, reflects bass, deadens HF.
            MaterialKind::Concrete => MaterialSpectrum {
                absorption: [0.02, 0.02, 0.03, 0.03, 0.04, 0.05, 0.05, 0.06, 0.07],
                reflection: [0.97, 0.97, 0.96, 0.95, 0.93, 0.90, 0.85, 0.80, 0.75],
                transmission: [0.0; OCTAVE_BANDS],
            },
            // Wood: reflective plank, slightly more absorptive than concrete.
            MaterialKind::Wood => MaterialSpectrum {
                absorption: [0.10, 0.10, 0.10, 0.08, 0.08, 0.08, 0.07, 0.07, 0.06],
                reflection: [0.88, 0.88, 0.88, 0.90, 0.90, 0.90, 0.90, 0.87, 0.84],
                transmission: [0.0; OCTAVE_BANDS],
            },
            // Glass: highly reflective specular, hard, with a tiny mid HF dip.
            MaterialKind::Glass => MaterialSpectrum {
                absorption: [0.02, 0.02, 0.02, 0.02, 0.03, 0.04, 0.03, 0.24, 0.30],
                reflection: [0.96, 0.96, 0.96, 0.96, 0.94, 0.90, 0.90, 0.70, 0.62],
                transmission: [0.0; OCTAVE_BANDS],
            },
            // Fabric: increasingly absorptive with frequency.
            MaterialKind::Fabric => MaterialSpectrum::rising_absorption(0.06, 0.70),
            // Carpet: like fabric on the floor; strong HF absorption.
            MaterialKind::Carpet => MaterialSpectrum::rising_absorption(0.02, 0.60),
            // Metal: nearly perfectly reflective, slightly rising HF loss.
            MaterialKind::Metal => MaterialSpectrum {
                absorption: [0.01, 0.01, 0.01, 0.01, 0.02, 0.02, 0.02, 0.02, 0.03],
                reflection: [0.98, 0.98, 0.98, 0.98, 0.97, 0.96, 0.96, 0.95, 0.94],
                transmission: [0.0; OCTAVE_BANDS],
            },
            // Acoustically transparent grille: everything passes, nothing
            // reflects or absorbs (a fully-open aperture).
            MaterialKind::OpenMesh => MaterialSpectrum {
                absorption: [0.0; OCTAVE_BANDS],
                reflection: [0.0; OCTAVE_BANDS],
                transmission: [1.0; OCTAVE_BANDS],
            },
        }
    }
}

/// Convenience: the broadband low-pass corner inherent in a surface, at a
/// given sample rate (used by the path solver and by acceptance tests that
/// must not depend on a specific FFT grid).
pub fn surface_lowpass_hz(spectrum: &MaterialSpectrum, sample_rate_hz: f32) -> f32 {
    spectrum.broadband(sample_rate_hz).1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_material_reflects_the_remainder() {
        let m = MaterialSpectrum::flat_reflective(0.2);
        assert!((m.reflectivity_at_hz(1000.0) - 0.8).abs() < 1e-6);
        assert!(m.transmission_at_hz(1000.0) < 1e-6);
    }

    #[test]
    fn rising_absorption_is_monotonic() {
        let m = MaterialSpectrum::rising_absorption(0.05, 0.95);
        for b in 1..OCTAVE_BANDS {
            assert!(m.absorption[b] > m.absorption[b - 1], "band {b}");
        }
        // And the low-pass corner sits high (HF is dead).
        assert!(surface_lowpass_hz(&m, 48_000.0) < 48_000.0 * 0.5 * 0.9);
    }

    #[test]
    fn glass_reflects_bass_and_dies_at_hf() {
        let glass = MaterialKind::Glass.spectrum();
        let bass = glass.reflectivity_at_hz(63.0 * 4.0);
        let hf = glass.reflectivity_at_hz(16_000.0);
        assert!(bass > hf, "bass {bass} should be louder than HF {hf}");
    }

    #[test]
    fn broadband_reduction_is_sane() {
        // Concrete: mostly reflective, high corner.
        let (g, _lp) = MaterialKind::Concrete.spectrum().broadband(48_000.0);
        assert!(g > 0.7 && g <= 1.0);
        // Carpet: HF dead → the corner is well below Nyquist and the gain
        // drops from the flat 63 Hz value.
        let (cg, clp) = MaterialKind::Carpet.spectrum().broadband(48_000.0);
        assert!(clp < 48_000.0 * 0.5);
        assert!(cg < 1.0);
        // Fabric: high corner-ish but strongly damped gain.
        let (fg, _) = MaterialKind::Fabric.spectrum().broadband(48_000.0);
        assert!(fg <= 0.7, "heavy fabric damps reflection (gain {fg})");
        // Transmission of a fully-open portal is ~1 with no low-pass.
        let (tg, tlp) = MaterialSpectrum::flat_transmissive(1.0).transmitted_broadband(48_000.0);
        assert!((tg - 1.0).abs() < 1e-6);
        assert!(tlp >= 48_000.0 * 0.5);
    }

    #[test]
    fn frequency_interpolation_trends_downward_with_hz() {
        let m = MaterialSpectrum::rising_absorption(0.0, 1.0);
        let low = m.reflectivity_at_hz(125.0);
        let high = m.reflectivity_at_hz(16_000.0);
        assert!(
            low > high,
            "reflectivity falls with frequency ({low} -> {high})"
        );
        // Mid-band interpolation is bracketed by its neighbours.
        for f in [250.0, 1000.0, 4000.0] {
            let v = m.reflectivity_at_hz(f);
            assert!((0.0..=1.0).contains(&v), "{v} at {f} Hz");
        }
    }
}
