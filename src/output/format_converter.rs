//! `AudioFormatConverter` — owns dither and performs all `f32 → integer`
//! format conversions at the final quantization boundary.
//!
//! This is the single correct place for quantization in the engine.  Previously
//! the conversion logic was duplicated across the i16 and u16 CPAL callbacks.
//! Centralizing it here ensures:
//!
//! 1. **Dither is applied exactly once**, at the quantization boundary.
//! 2. **All format-specific clamping math is in one place** — no more copy-paste.
//! 3. **The `Dither` field in `DspPipeline` is no longer needed** — the DSP
//!    pipeline operates entirely in `f32`/`f64` and does not quantize.
//!
//! # Usage (audio callback)
//!
//! ```rust,ignore
//! let converter = AudioFormatConverter::new(SampleFormat::I16, 16, DitherType::Triangular);
//! // In the i16 callback:
//! for frame in data.chunks_mut(2) {
//!     let l = scratch[i]; let r = scratch[i+1];
//!     let (l16, r16) = converter.convert_stereo_to_i16(l, r);
//!     frame[0] = l16; frame[1] = r16;
//! }
//! ```

use crate::dsp::dither::{Dither, DitherType};

/// Describes the target sample format for format conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFormat {
    /// 32-bit float — no conversion needed, passthrough
    F32,
    /// 64-bit float
    F64,
    /// Signed 16-bit integer (most common DAC format)
    I16,
    /// Unsigned 16-bit integer (some ALSA/macOS devices)
    U16,
    /// Signed 24-bit integer packed in 32 bits (high-quality USB DACs)
    I24Le,
    /// Signed 32-bit integer
    I32,
}

/// Converts `f32` audio samples to the target hardware format, applying dither
/// at the quantization boundary.
pub struct AudioFormatConverter {
    format: TargetFormat,
    dither: Dither,
    dither_enabled: bool,
}

impl AudioFormatConverter {
    /// Create a new format converter.
    ///
    /// * `format`         — target sample format
    /// * `dither_type`    — which dithering algorithm to apply
    pub fn new(format: TargetFormat, dither_type: DitherType) -> Self {
        let bit_depth = match format {
            TargetFormat::F32 | TargetFormat::F64 => 32,
            TargetFormat::I16 | TargetFormat::U16 => 16,
            TargetFormat::I24Le => 24,
            TargetFormat::I32 => 32,
        };
        Self {
            format,
            dither: Dither::new(dither_type, bit_depth),
            dither_enabled: dither_type != DitherType::None,
        }
    }

    pub fn set_dither_enabled(&mut self, enabled: bool) {
        self.dither_enabled = enabled;
        self.dither.set_enabled(enabled);
    }

    pub fn is_dither_enabled(&self) -> bool {
        self.dither_enabled
    }

    pub fn format(&self) -> TargetFormat {
        self.format
    }

    /// Convert a stereo `f32` pair to signed 16-bit integers.
    ///
    /// Applies dither before quantization. Clamps to `[-32768, 32767]`.
    #[inline]
    pub fn convert_stereo_to_i16(&mut self, left: f32, right: f32) -> (i16, i16) {
        let (l, r) = if self.dither_enabled {
            self.dither.process(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        (
            (l * 32768.0).clamp(-32768.0, 32767.0) as i16,
            (r * 32768.0).clamp(-32768.0, 32767.0) as i16,
        )
    }

    /// Convert a stereo `f64` pair to signed 16-bit integers with native 64-bit dither & quantization.
    #[inline]
    pub fn convert_f64_stereo_to_i16(&mut self, left: f64, right: f64) -> (i16, i16) {
        let (l, r) = if self.dither_enabled {
            self.dither.process_f64(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        (
            (l * 32768.0).clamp(-32768.0, 32767.0) as i16,
            (r * 32768.0).clamp(-32768.0, 32767.0) as i16,
        )
    }

    /// Convert a mono `f32` sample to a signed 16-bit integer.
    #[inline]
    pub fn convert_mono_to_i16(&mut self, sample: f32) -> i16 {
        let s = if self.dither_enabled {
            self.dither.process_mono(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        (s * 32768.0).clamp(-32768.0, 32767.0) as i16
    }

    /// Convert a mono `f64` sample to a signed 16-bit integer with native 64-bit dither & quantization.
    #[inline]
    pub fn convert_f64_mono_to_i16(&mut self, sample: f64) -> i16 {
        let s = if self.dither_enabled {
            self.dither.process_mono_f64(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        (s * 32768.0).clamp(-32768.0, 32767.0) as i16
    }

    /// Convert a stereo `f32` pair to unsigned 16-bit integers.
    /// The zero-level maps to 32768 (offset-binary / mid-point).
    #[inline]
    pub fn convert_stereo_to_u16(&mut self, left: f32, right: f32) -> (u16, u16) {
        let (l, r) = if self.dither_enabled {
            self.dither.process(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        (
            (((l + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16,
            (((r + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16,
        )
    }

    /// Convert a stereo `f64` pair to unsigned 16-bit integers with native 64-bit precision.
    #[inline]
    pub fn convert_f64_stereo_to_u16(&mut self, left: f64, right: f64) -> (u16, u16) {
        let (l, r) = if self.dither_enabled {
            self.dither.process_f64(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        (
            (((l + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16,
            (((r + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16,
        )
    }

    /// Convert a mono `f32` sample to an unsigned 16-bit integer.
    #[inline]
    pub fn convert_mono_to_u16(&mut self, sample: f32) -> u16 {
        let s = if self.dither_enabled {
            self.dither.process_mono(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        (((s + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16
    }

    /// Convert a mono `f64` sample to an unsigned 16-bit integer.
    #[inline]
    pub fn convert_f64_mono_to_u16(&mut self, sample: f64) -> u16 {
        let s = if self.dither_enabled {
            self.dither.process_mono_f64(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        (((s + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16
    }

    /// Convert a stereo `f32` pair to signed 24-bit-in-32 integers (I24 LE).
    #[inline]
    pub fn convert_stereo_to_i24le(&mut self, left: f32, right: f32) -> (i32, i32) {
        let (l, r) = if self.dither_enabled {
            self.dither.process(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        const SCALE: f32 = 8388608.0; // 2^23
        let li = (l * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        let ri = (r * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        (li, ri)
    }

    /// Convert a stereo `f64` pair to signed 24-bit-in-32 integers (I24 LE) with full 64-bit dither & scaling.
    #[inline]
    pub fn convert_f64_stereo_to_i24le(&mut self, left: f64, right: f64) -> (i32, i32) {
        let (l, r) = if self.dither_enabled {
            self.dither.process_f64(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        const SCALE: f64 = 8388608.0; // 2^23
        let li = (l * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        let ri = (r * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        (li, ri)
    }

    /// Convert a stereo `f32` pair to signed 32-bit integers.
    #[inline]
    pub fn convert_stereo_to_i32(&mut self, left: f32, right: f32) -> (i32, i32) {
        let (l, r) = if self.dither_enabled && self.dither.bit_depth() == 32 {
            self.dither.process(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        const SCALE: f64 = 2147483648.0; // 2^31
        let li = ((l as f64) * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        let ri = ((r as f64) * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        (li, ri)
    }

    /// Convert a stereo `f64` pair to signed 32-bit integers with full 64-bit scaling and optional 32-bit dither.
    #[inline]
    pub fn convert_f64_stereo_to_i32(&mut self, left: f64, right: f64) -> (i32, i32) {
        let (l, r) = if self.dither_enabled && self.dither.bit_depth() == 32 {
            self.dither.process_f64(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        const SCALE: f64 = 2147483648.0; // 2^31
        let li = (l * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        let ri = (r * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        (li, ri)
    }

    /// Convert a mono `f32` sample to a signed 32-bit integer.
    #[inline]
    pub fn convert_mono_to_i32(&mut self, sample: f32) -> i32 {
        let s = if self.dither_enabled && self.dither.bit_depth() == 32 {
            self.dither.process_mono(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        const SCALE: f64 = 2147483648.0;
        ((s as f64) * SCALE).clamp(-SCALE, SCALE - 1.0) as i32
    }

    /// Convert a mono `f64` sample to a signed 32-bit integer with native 64-bit precision.
    #[inline]
    pub fn convert_f64_mono_to_i32(&mut self, sample: f64) -> i32 {
        let s = if self.dither_enabled && self.dither.bit_depth() == 32 {
            self.dither.process_mono_f64(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        const SCALE: f64 = 2147483648.0;
        (s * SCALE).clamp(-SCALE, SCALE - 1.0) as i32
    }

    /// Convert a stereo `f32` pair to `f32` (passthrough with clamp).
    #[inline]
    pub fn convert_stereo_to_f32(&mut self, left: f32, right: f32) -> (f32, f32) {
        (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
    }

    /// Convert a stereo `f64` pair to `f32`.
    #[inline]
    pub fn convert_f64_stereo_to_f32(&mut self, left: f64, right: f64) -> (f32, f32) {
        (left.clamp(-1.0, 1.0) as f32, right.clamp(-1.0, 1.0) as f32)
    }

    /// Convert a stereo `f32` pair to `f64`.
    #[inline]
    pub fn convert_stereo_to_f64(&mut self, left: f32, right: f32) -> (f64, f64) {
        (left.clamp(-1.0, 1.0) as f64, right.clamp(-1.0, 1.0) as f64)
    }

    /// Convert a stereo `f64` pair to `f64` (passthrough with clamp).
    #[inline]
    pub fn convert_f64_stereo_to_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
    }

    /// Convert a mono `f32` sample to `f64`.
    #[inline]
    pub fn convert_mono_to_f64(&mut self, sample: f32) -> f64 {
        sample.clamp(-1.0, 1.0) as f64
    }

    /// Convert a mono `f64` sample to `f64`.
    #[inline]
    pub fn convert_f64_mono_to_f64(&mut self, sample: f64) -> f64 {
        sample.clamp(-1.0, 1.0)
    }

    /// Convert a `f32` sample to the target format and write into a byte slice.
    #[inline]
    pub fn convert_sample_to_bytes(&mut self, sample: f32, out: &mut [u8]) {
        match self.format {
            TargetFormat::F32 => {
                let bytes = sample.to_le_bytes();
                out[..4].copy_from_slice(&bytes);
            }
            TargetFormat::I16 => {
                let v = self.convert_mono_to_i16(sample);
                let bytes = v.to_le_bytes();
                out[..2].copy_from_slice(&bytes);
            }
            TargetFormat::U16 => {
                let v = self.convert_mono_to_u16(sample);
                let bytes = v.to_le_bytes();
                out[..2].copy_from_slice(&bytes);
            }
            TargetFormat::I24Le => {
                let (v, _) = self.convert_stereo_to_i24le(sample, sample);
                out[0] = (v & 0xFF) as u8;
                out[1] = ((v >> 8) & 0xFF) as u8;
                out[2] = ((v >> 16) & 0xFF) as u8;
            }
            TargetFormat::I32 => {
                let (v, _) = self.convert_stereo_to_i32(sample, sample);
                let bytes = v.to_le_bytes();
                out[..4].copy_from_slice(&bytes);
            }
            TargetFormat::F64 => {
                let v = self.convert_mono_to_f64(sample);
                let bytes = v.to_le_bytes();
                out[..8].copy_from_slice(&bytes);
            }
        }
    }

    /// Reset internal dither state (e.g. between tracks).
    pub fn reset(&mut self) {
        self.dither.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_i16_full_scale() {
        let mut c = AudioFormatConverter::new(TargetFormat::I16, DitherType::None);
        let (l, r) = c.convert_stereo_to_i16(1.0, -1.0);
        assert!(l > 0);
        assert!(r < 0);
    }

    #[test]
    fn test_u16_midpoint_at_zero() {
        let mut c = AudioFormatConverter::new(TargetFormat::U16, DitherType::None);
        let (l, _) = c.convert_stereo_to_u16(0.0, 0.0);
        assert!(
            (l as i32 - 32768).abs() < 10,
            "mid-point should be ~32768, got {}",
            l
        );
    }

    #[test]
    fn test_i24_range() {
        let mut c = AudioFormatConverter::new(TargetFormat::I24Le, DitherType::None);
        let (l, _) = c.convert_stereo_to_i24le(1.0, 0.0);
        // Max 24-bit signed = 2^23 - 1 = 8388607
        assert!((-8388608..=8388607).contains(&l));
    }

    #[test]
    fn test_dithered_i16_bounded() {
        let mut c = AudioFormatConverter::new(TargetFormat::I16, DitherType::Triangular);
        for _ in 0..10000 {
            let (l, r) = c.convert_stereo_to_i16(0.999, -0.999);
            assert!(l > 30000);
            assert!(r < -30000);
        }
    }

    #[test]
    fn test_f64_to_i24_precision() {
        let mut c = AudioFormatConverter::new(TargetFormat::I24Le, DitherType::None);
        // Test precision around 24-bit LSB (1 / 8388608 = ~1.1920928955078125e-7)
        let sample = 1.0 / 8388608.0;
        let (l, _) = c.convert_f64_stereo_to_i24le(sample, 0.0);
        assert_eq!(l, 1);

        let half_sample = 0.5 / 8388608.0;
        let (l_half, _) = c.convert_f64_stereo_to_i24le(half_sample, 0.0);
        assert_eq!(l_half, 0); // Truncated/rounded without dither
    }

    #[test]
    fn test_f64_to_i32_precision() {
        let mut c = AudioFormatConverter::new(TargetFormat::I32, DitherType::None);
        // Test precision around 32-bit LSB (1 / 2147483648 = ~4.656612873077393e-10)
        let sample = 100.0 / 2147483648.0;
        let (l, _) = c.convert_f64_stereo_to_i32(sample, 0.0);
        assert_eq!(l, 100);
    }
}
