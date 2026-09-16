//! Representation-tailored audio buffers (§4.4, Item 22).
//!
//! Separates physical loudspeaker frame buffers from continuous Higher-Order
//! Ambisonics soundfields, discrete object planes, and binaural ear buffers.
//!
//! # Guarantee
//! - Real-time safe: preallocated, zero heap allocation, zero locking on hot path.
//! - HOA order 9 (100 channels) is representable without increasing physical stereo buffers.

use super::channels::{ObjectCount, PhysicalChannelCount, SpatialFieldOrder};

/// Planar or interleaved audio buffer for physical loudspeaker reproduction.
#[derive(Debug, Clone)]
pub struct PhysicalBuffer {
    channels: PhysicalChannelCount,
    frames: usize,
    data: Vec<f32>,
}

impl PhysicalBuffer {
    /// Allocate a pre-sized physical buffer.
    pub fn new(channels: PhysicalChannelCount, frames: usize) -> Self {
        let capacity = channels.get() * frames;
        Self {
            channels,
            frames,
            data: vec![0.0; capacity],
        }
    }

    #[inline]
    pub fn channels(&self) -> usize {
        self.channels.get()
    }

    #[inline]
    pub fn frames(&self) -> usize {
        self.frames
    }

    #[inline]
    pub fn clear(&mut self) {
        self.data.fill(0.0);
    }

    #[inline]
    pub fn as_slice(&self) -> &[f32] {
        &self.data
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [f32] {
        &mut self.data
    }

    /// Access planar channel `ch`.
    #[inline]
    pub fn channel(&self, ch: usize) -> Option<&[f32]> {
        if ch < self.channels.get() {
            let start = ch * self.frames;
            Some(&self.data[start..start + self.frames])
        } else {
            None
        }
    }

    /// Mutably access planar channel `ch`.
    #[inline]
    pub fn channel_mut(&mut self, ch: usize) -> Option<&mut [f32]> {
        if ch < self.channels.get() {
            let start = ch * self.frames;
            Some(&mut self.data[start..start + self.frames])
        } else {
            None
        }
    }
}

/// Specialized planar buffer for Higher-Order Ambisonics (HOA).
/// Sized specifically for $(N+1)^2$ spherical harmonics channels.
#[derive(Debug, Clone)]
pub struct HoaBuffer {
    order: SpatialFieldOrder,
    frames: usize,
    data: Vec<f32>,
}

impl HoaBuffer {
    /// Allocate a preallocated HOA buffer for a given field order.
    pub fn new(order: SpatialFieldOrder, frames: usize) -> Self {
        let channels = order.channels();
        let capacity = channels * frames;
        Self {
            order,
            frames,
            data: vec![0.0; capacity],
        }
    }

    #[inline]
    pub fn order(&self) -> SpatialFieldOrder {
        self.order
    }

    #[inline]
    pub fn channels(&self) -> usize {
        self.order.channels()
    }

    #[inline]
    pub fn frames(&self) -> usize {
        self.frames
    }

    #[inline]
    pub fn clear(&mut self) {
        self.data.fill(0.0);
    }

    #[inline]
    pub fn channel(&self, ch: usize) -> Option<&[f32]> {
        if ch < self.order.channels() {
            let start = ch * self.frames;
            Some(&self.data[start..start + self.frames])
        } else {
            None
        }
    }

    #[inline]
    pub fn channel_mut(&mut self, ch: usize) -> Option<&mut [f32]> {
        if ch < self.order.channels() {
            let start = ch * self.frames;
            Some(&mut self.data[start..start + self.frames])
        } else {
            None
        }
    }

    /// Scale all ambisonic channels by `gain` in place.
    pub fn scale(&mut self, gain: f32) {
        for s in &mut self.data {
            *s *= gain;
        }
    }

    /// Accumulate another HOA buffer of identical order and frames in place.
    pub fn accumulate(&mut self, other: &HoaBuffer) {
        if self.order == other.order && self.frames == other.frames {
            for (dst, src) in self.data.iter_mut().zip(other.data.iter()) {
                *dst += *src;
            }
        }
    }
}

/// Planar buffer containing discrete mono audio planes for dynamic spatial objects.
#[derive(Debug, Clone)]
pub struct ObjectBuffer {
    object_count: ObjectCount,
    frames: usize,
    data: Vec<f32>,
}

impl ObjectBuffer {
    pub fn new(object_count: ObjectCount, frames: usize) -> Self {
        let capacity = object_count.get() * frames;
        Self {
            object_count,
            frames,
            data: vec![0.0; capacity],
        }
    }

    #[inline]
    pub fn object_count(&self) -> usize {
        self.object_count.get()
    }

    #[inline]
    pub fn frames(&self) -> usize {
        self.frames
    }

    #[inline]
    pub fn clear(&mut self) {
        self.data.fill(0.0);
    }

    #[inline]
    pub fn plane(&self, object_idx: usize) -> Option<&[f32]> {
        if object_idx < self.object_count.get() {
            let start = object_idx * self.frames;
            Some(&self.data[start..start + self.frames])
        } else {
            None
        }
    }

    #[inline]
    pub fn plane_mut(&mut self, object_idx: usize) -> Option<&mut [f32]> {
        if object_idx < self.object_count.get() {
            let start = object_idx * self.frames;
            Some(&mut self.data[start..start + self.frames])
        } else {
            None
        }
    }
}

/// Dedicated 2-channel binaural ear buffer (Left, Right).
#[derive(Debug, Clone)]
pub struct BinauralBuffer {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
}

impl BinauralBuffer {
    pub fn new(frames: usize) -> Self {
        Self {
            left: vec![0.0; frames],
            right: vec![0.0; frames],
        }
    }

    #[inline]
    pub fn frames(&self) -> usize {
        self.left.len()
    }

    #[inline]
    pub fn clear(&mut self) {
        self.left.fill(0.0);
        self.right.fill(0.0);
    }

    #[inline]
    pub fn interleave_into(&self, out: &mut [f32]) {
        let n = self.left.len().min(self.right.len()).min(out.len() / 2);
        for i in 0..n {
            out[2 * i] = self.left[i];
            out[2 * i + 1] = self.right[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hoa_buffer_order_9_allocation() {
        let order9 = SpatialFieldOrder::ORDER_9;
        assert_eq!(order9.channels(), 100);

        let mut buf = HoaBuffer::new(order9, 256);
        assert_eq!(buf.channels(), 100);
        assert_eq!(buf.frames(), 256);

        // Verify channel access
        if let Some(ch99) = buf.channel_mut(99) {
            ch99[0] = 1.0;
        }
        assert_eq!(buf.channel(99).unwrap()[0], 1.0);
        assert_eq!(buf.channel(0).unwrap()[0], 0.0);

        buf.scale(0.5);
        assert_eq!(buf.channel(99).unwrap()[0], 0.5);

        buf.clear();
        assert_eq!(buf.channel(99).unwrap()[0], 0.0);
    }

    #[test]
    fn binaural_buffer_interleave() {
        let mut bin = BinauralBuffer::new(4);
        bin.left.copy_from_slice(&[1.0, 2.0, 3.0, 4.0]);
        bin.right.copy_from_slice(&[-1.0, -2.0, -3.0, -4.0]);

        let mut interleaved = vec![0.0; 8];
        bin.interleave_into(&mut interleaved);
        assert_eq!(
            interleaved,
            vec![1.0, -1.0, 2.0, -2.0, 3.0, -3.0, 4.0, -4.0]
        );
    }
}
