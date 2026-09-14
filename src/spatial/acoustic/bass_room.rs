//! Bass-Aware Room Modeling (spec Part II §8, Schroeder modal acoustics).
//!
//! Below the Schroeder frequency (typically 80–150 Hz in domestic rooms),
//! sound propagation is dominated by wave resonance (standing wave room modes)
//! rather than geometrical ray reflections.
//!
//! This module provides:
//! - Exact modal frequency calculation for axial, tangential, and oblique modes.
//! - Standing wave pressure distribution $\psi(x,y,z) = \cos(\frac{n_x \pi x}{L_x}) \cos(\frac{n_y \pi y}{L_y}) \cos(\frac{n_z \pi z}{L_z})$.
//! - Schroeder transition frequency $f_s \approx 2000 \sqrt{RT_{60} / V}$.
//! - Source-listener spatial modal coupling and resonant biquad filters.

use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};
use crate::spatial::math::Vec3;

/// Maximum number of low-frequency resonant modes tracked simultaneously.
pub const MAX_MODAL_RESONATORS: usize = 16;

/// Classification of a room mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeKind {
    /// Axial mode: travels between 2 parallel walls (highest energy).
    Axial,
    /// Tangential mode: reflects between 4 walls.
    Tangential,
    /// Oblique mode: reflects between all 6 surfaces.
    Oblique,
}

/// A discrete standing-wave acoustic mode of a rectangular enclosure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoomMode {
    pub nx: u32,
    pub ny: u32,
    pub nz: u32,
    pub frequency_hz: f32,
    pub q: f32,
    pub kind: ModeKind,
}

/// Modal room acoustics engine for frequencies below the Schroeder frequency (20..150 Hz).
#[derive(Debug, Clone)]
pub struct ModalBassRoom {
    width: f32,
    depth: f32,
    height: f32,
    rt60_sec: f32,
    speed_of_sound: f32,
    schroeder_freq_hz: f32,
    modes: Vec<RoomMode>,
    resonator_coeffs: Vec<BiquadCoeffsF32>,
    resonator_states: Vec<BiquadStateF32>,
    enabled: bool,
}

impl ModalBassRoom {
    /// Construct a new modal bass room with dimensions in metres and RT60 in seconds.
    pub fn new(
        width: f32,
        depth: f32,
        height: f32,
        rt60_sec: f32,
        speed_of_sound: f32,
        sample_rate: f32,
    ) -> Self {
        let w = width.max(1.0);
        let d = depth.max(1.0);
        let h = height.max(1.0);
        let rt = rt60_sec.clamp(0.05, 5.0);
        let c = if speed_of_sound > 100.0 {
            speed_of_sound
        } else {
            343.0
        };

        // Volume V = w * d * h
        let volume = w * d * h;
        // Schroeder frequency: fs ≈ 2000 * sqrt(RT60 / V)
        let schroeder_freq_hz = (2000.0 * (rt / volume).sqrt()).clamp(40.0, 250.0);

        let mut room = Self {
            width: w,
            depth: d,
            height: h,
            rt60_sec: rt,
            speed_of_sound: c,
            schroeder_freq_hz,
            modes: Vec::new(),
            resonator_coeffs: Vec::new(),
            resonator_states: Vec::new(),
            enabled: true,
        };

        room.compute_modes(sample_rate);
        room
    }

    /// Return the Schroeder frequency in Hz.
    pub fn schroeder_frequency(&self) -> f32 {
        self.schroeder_freq_hz
    }

    /// Return the computed active standing-wave modes below the Schroeder frequency.
    pub fn modes(&self) -> &[RoomMode] {
        &self.modes
    }

    /// Compute all modal resonances up to the Schroeder frequency.
    fn compute_modes(&mut self, sample_rate: f32) {
        let c = self.speed_of_sound;
        let lx = self.width;
        let ly = self.depth;
        let lz = self.height;
        let fs = self.schroeder_freq_hz;

        let mut candidate_modes = Vec::new();

        // Enumerate modal indices (nx, ny, nz)
        let max_n = 6;
        for nx in 0..=max_n {
            for ny in 0..=max_n {
                for nz in 0..=max_n {
                    if nx == 0 && ny == 0 && nz == 0 {
                        continue;
                    }

                    // f = (c / 2) * sqrt((nx/Lx)^2 + (ny/Ly)^2 + (nz/Lz)^2)
                    let kx = nx as f32 / lx;
                    let ky = ny as f32 / ly;
                    let kz = nz as f32 / lz;
                    let freq = (c * 0.5) * (kx * kx + ky * ky + kz * kz).sqrt();

                    if freq >= 20.0 && freq <= fs {
                        let non_zeros = (nx > 0) as u8 + (ny > 0) as u8 + (nz > 0) as u8;
                        let kind = match non_zeros {
                            1 => ModeKind::Axial,
                            2 => ModeKind::Tangential,
                            _ => ModeKind::Oblique,
                        };

                        // Modal bandwidth and Q: Q ≈ freq * RT60 / 2.2
                        let q = (freq * self.rt60_sec / 2.2).clamp(1.0, 30.0);

                        candidate_modes.push(RoomMode {
                            nx,
                            ny,
                            nz,
                            frequency_hz: freq,
                            q,
                            kind,
                        });
                    }
                }
            }
        }

        // Sort by perceptual significance: Axial first, then lowest frequency
        candidate_modes.sort_by(|a, b| {
            let priority_a = match a.kind {
                ModeKind::Axial => 0,
                ModeKind::Tangential => 1,
                ModeKind::Oblique => 2,
            };
            let priority_b = match b.kind {
                ModeKind::Tangential => 1,
                ModeKind::Axial => 0,
                ModeKind::Oblique => 2,
            };
            priority_a
                .cmp(&priority_b)
                .then_with(|| a.frequency_hz.total_cmp(&b.frequency_hz))
        });

        // Retain the top MAX_MODAL_RESONATORS modes
        self.modes = candidate_modes
            .into_iter()
            .take(MAX_MODAL_RESONATORS)
            .collect();

        // Build peaking resonator biquad coefficients for each mode
        self.resonator_coeffs.clear();
        self.resonator_states.clear();

        for m in &self.modes {
            // Nominal modal boost at antinode
            let gain_db = match m.kind {
                ModeKind::Axial => 6.0,
                ModeKind::Tangential => 3.0,
                ModeKind::Oblique => 1.5,
            };
            let coeffs = BiquadCoeffsF32::peaking(sample_rate, m.frequency_hz, gain_db, m.q);
            self.resonator_coeffs.push(coeffs);
            self.resonator_states.push(BiquadStateF32::default());
        }
    }

    /// Spatial pressure eigenfunction $\psi(x, y, z) \in [-1, 1]$ at a given point in the room.
    #[inline]
    pub fn mode_eigenfunction(&self, mode: &RoomMode, pos: Vec3) -> f32 {
        let pi = std::f32::consts::PI;
        // Clamp position within room boundary [0, L]
        let x = pos.x.clamp(0.0, self.width);
        let y = pos.y.clamp(0.0, self.depth);
        let z = pos.z.clamp(0.0, self.height);

        let psix = (mode.nx as f32 * pi * x / self.width).cos();
        let psiy = (mode.ny as f32 * pi * y / self.depth).cos();
        let psiz = (mode.nz as f32 * pi * z / self.height).cos();

        psix * psiy * psiz
    }

    /// Spatial coupling between a source position and a listener position for a given mode.
    /// Returns a factor in `[-1.0, 1.0]`.
    #[inline]
    pub fn source_listener_coupling(
        &self,
        mode: &RoomMode,
        source_pos: Vec3,
        listener_pos: Vec3,
    ) -> f32 {
        let psi_s = self.mode_eigenfunction(mode, source_pos);
        let psi_l = self.mode_eigenfunction(mode, listener_pos);
        psi_s * psi_l
    }

    /// Process a block of low-frequency audio through the modal resonators.
    ///
    /// Modulates modal excitation by the source and listener positions within the enclosure.
    pub fn process_modal_bass(
        &mut self,
        source_pos: Vec3,
        listener_pos: Vec3,
        samples: &mut [f32],
        wet_mix: f32,
    ) {
        if !self.enabled || wet_mix <= 0.001 || self.modes.is_empty() {
            return;
        }

        let mix = wet_mix.clamp(0.0, 1.0);

        // Precompute spatial coupling weight for each active mode
        let mut couplings = [0.0f32; MAX_MODAL_RESONATORS];
        for (i, m) in self.modes.iter().enumerate() {
            couplings[i] = self.source_listener_coupling(m, source_pos, listener_pos);
        }

        for s in samples.iter_mut() {
            let x = *s;
            let mut modal_sum = 0.0f32;

            for (i, coeff) in self.resonator_coeffs.iter().enumerate() {
                let weight = couplings[i];
                if weight.abs() > 0.01 {
                    let y = self.resonator_states[i].process(x, coeff);
                    // Resonator difference signal scaled by spatial coupling
                    modal_sum += (y - x) * weight;
                }
            }

            *s = x + modal_sum * mix;
        }
    }

    /// Reset internal resonator filter states.
    pub fn reset(&mut self) {
        for state in &mut self.resonator_states {
            state.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schroeder_frequency_scales_with_volume() {
        // Room 6x4x3 = 72 m3, RT60 = 0.5s
        // fs ≈ 2000 * sqrt(0.5 / 72) ≈ 2000 * 0.0833 ≈ 166 Hz
        let room = ModalBassRoom::new(6.0, 4.0, 3.0, 0.5, 343.0, 48000.0);
        let fs = room.schroeder_frequency();
        assert!(
            fs > 100.0 && fs < 200.0,
            "Unexpected Schroeder freq: {}",
            fs
        );
    }

    #[test]
    fn corner_loading_has_maximum_coupling() {
        let room = ModalBassRoom::new(5.0, 4.0, 2.5, 0.4, 343.0, 48000.0);
        assert!(!room.modes().is_empty());

        let mode = room.modes()[0];
        // In the corner (0,0,0), eigenfunction is cos(0)*cos(0)*cos(0) = 1.0
        let corner = Vec3::new(0.0, 0.0, 0.0);
        let coupling = room.source_listener_coupling(&mode, corner, corner);
        assert!((coupling - 1.0).abs() < 1e-4);
    }

    #[test]
    fn modal_filtering_does_not_blow_up() {
        let mut room = ModalBassRoom::new(6.0, 5.0, 3.0, 0.6, 343.0, 48000.0);
        let mut block = vec![0.5f32; 128];
        let src = Vec3::new(1.0, 1.0, 1.0);
        let listener = Vec3::new(3.0, 2.5, 1.2);

        room.process_modal_bass(src, listener, &mut block, 0.3);

        for &val in &block {
            assert!(val.is_finite());
            assert!(val.abs() < 5.0);
        }
    }
}
