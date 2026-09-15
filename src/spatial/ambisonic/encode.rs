//! Plane-wave ambisonic encoding up to order 9.

use super::basis::{channel_count, sh_foa, sh_n, MAX_AMBISONIC_ORDER};
use crate::spatial::math::Vec3;

/// Encode a plane wave from `dir` (gain `g`) into one order-1 FOA bus frame
/// (`[W, Y, Z, X]`). `dir` is normalised defensively; a zero direction
/// encodes silence rather than NaN.
pub fn encode_plane_wave(dir: Vec3, gain: f32, out: &mut [f32; 4]) {
    let d = dir.normalized().unwrap_or(Vec3::Y);
    let y = sh_foa(d);
    for (o, &v) in out.iter_mut().zip(y.iter()) {
        *o = v * gain;
    }
}

/// Encode a plane wave from `dir` (gain `g`) into a bus frame of any
/// supported order (write `channel_count(order)` values into `out`).
pub fn encode_plane_wave_n(order: u8, dir: Vec3, gain: f32, out: &mut [f32]) {
    let d = dir.normalized().unwrap_or(Vec3::Y);
    sh_n(order, d, out);
    for v in out.iter_mut().take(channel_count(order)) {
        *v *= gain;
    }
}

/// Higher-Order Ambisonic encoder.
///
/// Encodes mono or multi-channel audio objects into an ambisonic bus frame
/// up to order 9 (`MAX_AMBISONIC_ORDER = 9`, 100 channels). Allocation-free.
#[derive(Debug, Clone)]
pub struct AmbisonicEncoder {
    order: u8,
    channel_count: usize,
}

impl AmbisonicEncoder {
    /// Create a new encoder for `order` (clamped to `MAX_AMBISONIC_ORDER`).
    pub fn new(order: u8) -> Self {
        let o = order.min(MAX_AMBISONIC_ORDER);
        Self {
            order: o,
            channel_count: channel_count(o),
        }
    }

    /// Return the current encoding order.
    pub fn order(&self) -> u8 {
        self.order
    }

    /// Number of channels produced by this encoder.
    pub fn channels(&self) -> usize {
        self.channel_count
    }

    /// Encode a single source direction with gain into `out`.
    /// `out` must have length at least `self.channels()`. Allocation-free.
    #[inline]
    pub fn encode(&self, dir: Vec3, gain: f32, out: &mut [f32]) {
        encode_plane_wave_n(self.order, dir, gain, out);
    }
}
