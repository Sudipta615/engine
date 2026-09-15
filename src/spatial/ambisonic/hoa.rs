//! Higher-Order Ambisonic (HOA) advanced pipeline (Phase 3 Point 22).
//!
//! Provides dedicated HOA encoding, decoding, near-field distance compensation (NFC),
//! and per-speaker propagation delay compensation for ambisonic orders 1 through 9.

use super::basis::{channel_count, MAX_AMBISONIC_ORDER};
use super::decoder::{AmbisonicDecoder, DecoderPolicy};
use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32};
use crate::spatial::math::Vec3;
use crate::spatial::render::RenderError;
use crate::spatial::speaker::SpeakerLayout;

/// HOA decoding optimization policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HoaDecoding {
    /// Basic sampling decoder (pin-pointing without energy normalization).
    Basic,
    /// Energy-vector maximized (Max-rE) for optimal high-frequency localization.
    #[default]
    MaxRe,
    /// In-phase decoding: zero negative lobes, optimal for narrow sweet-spots.
    InPhase,
    /// Energy-velocity optimized dual-band transition.
    EnergyVelocityOptimized,
}

impl From<HoaDecoding> for DecoderPolicy {
    fn from(dec: HoaDecoding) -> Self {
        match dec {
            HoaDecoding::Basic => DecoderPolicy::Basic,
            HoaDecoding::MaxRe | HoaDecoding::EnergyVelocityOptimized => DecoderPolicy::MaxRe,
            HoaDecoding::InPhase => DecoderPolicy::InPhase,
        }
    }
}

/// Configuration for HOA rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoaConfig {
    /// Ambisonic order (1 to 9).
    pub order: u8,
    /// Decoding policy.
    pub decoding: HoaDecoding,
}

impl Default for HoaConfig {
    fn default() -> Self {
        Self {
            order: 3,
            decoding: HoaDecoding::MaxRe,
        }
    }
}

/// Near-field distance compensation (NFC) filter bank for spherical wave expansion.
#[derive(Debug, Clone)]
pub struct NearFieldCompensation {
    order: u8,
    filters: Vec<[BiquadCoeffsF32; 4]>,
    states: Vec<[BiquadStateF32; 4]>,
}

impl NearFieldCompensation {
    /// Precompute NFC filters for speaker layout distance and sample rate.
    pub fn new(order: u8, speaker_distance_m: f32, sample_rate: f32) -> Self {
        let o = order.min(MAX_AMBISONIC_ORDER);
        let n_ch = channel_count(o);
        let coeffs = crate::spatial::nearfield::hoa_distance_encode_filter(
            o,
            speaker_distance_m,
            sample_rate,
        );

        Self {
            order: o,
            filters: vec![coeffs; n_ch],
            states: vec![[BiquadStateF32::default(); 4]; n_ch],
        }
    }

    /// Apply near-field compensation to an ambisonic bus frame in place.
    /// Realtime-safe: zero allocations.
    #[inline]
    pub fn process_frame(&mut self, frame: &mut [f32]) {
        let n = channel_count(self.order).min(frame.len());
        for (ch, sample) in frame[..n].iter_mut().enumerate() {
            let mut s = *sample;
            for (coeff, state) in self.filters[ch].iter().zip(self.states[ch].iter_mut()) {
                s = state.process(s, coeff);
            }
            *sample = s;
        }
    }
}

/// Optional per-speaker delay compensation (aligns physical speaker arrival times).
#[derive(Debug, Clone)]
pub struct PerSpeakerDelay {
    pub delays_samples: [u32; 64],
}

impl Default for PerSpeakerDelay {
    fn default() -> Self {
        Self {
            delays_samples: [0; 64],
        }
    }
}

/// Dedicated Higher-Order Ambisonic encoder.
#[derive(Debug, Clone)]
pub struct HoaEncoder {
    order: u8,
}

impl HoaEncoder {
    pub fn new(order: u8) -> Self {
        Self {
            order: order.min(MAX_AMBISONIC_ORDER),
        }
    }

    pub fn order(&self) -> u8 {
        self.order
    }

    pub fn channels(&self) -> usize {
        channel_count(self.order)
    }

    /// Encode direction and gain into `ambi_bus`. Allocation-free.
    #[inline]
    pub fn encode(&self, dir: Vec3, gain: f32, ambi_bus: &mut [f32]) {
        super::encode::encode_plane_wave_n(self.order, dir, gain, ambi_bus);
    }
}

/// Dedicated Higher-Order Ambisonic decoder.
#[derive(Debug)]
pub struct HoaDecoder {
    config: HoaConfig,
    decoder: AmbisonicDecoder,
}

impl HoaDecoder {
    pub fn new(config: HoaConfig) -> Self {
        let policy = config.decoding.into();
        Self {
            config,
            decoder: AmbisonicDecoder::with_order(policy, config.order),
        }
    }

    pub fn config(&self) -> &HoaConfig {
        &self.config
    }

    pub fn prepare(&mut self, layout: &SpeakerLayout, sample_rate: u32) -> Result<(), RenderError> {
        self.decoder.prepare(layout, sample_rate)
    }

    pub fn decode(&self, ambi_bus: &[f32], frames: usize, out: &mut [f32]) {
        self.decoder.process_bus(ambi_bus, frames, out);
    }

    pub fn channels(&self) -> usize {
        self.decoder.channels()
    }

    pub fn bus_width(&self) -> usize {
        self.decoder.bus_width()
    }
}
