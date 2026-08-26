use std::sync::Arc;

use super::{AudioFrame, BufferError, PcmRingBuffer, MAX_CHANNELS};

/// Generic SPSC audio buffer handle supporting both single-frame and bulk PCM operations.
pub struct FixedFrameBuffer<T: Copy + Default + Send + Sync + 'static = f32> {
    pcm: Arc<PcmRingBuffer<T>>,
    /// Logical frame capacity. The backing ring reserves enough samples for
    /// the maximum supported channel width so multichannel output can use
    /// bulk frame operations instead of per-frame pops.
    frame_capacity: usize,
}

pub type FixedFrameBufferF32 = FixedFrameBuffer<f32>;
pub type FixedFrameBufferF64 = FixedFrameBuffer<f64>;

impl<T: Copy + Default + Send + Sync + 'static> FixedFrameBuffer<T> {
    pub fn new(capacity: usize) -> Result<Self, BufferError> {
        if capacity == 0 {
            return Err(BufferError::InvalidCapacity(capacity));
        }
        let pcm_cap = capacity
            .checked_mul(MAX_CHANNELS)
            .ok_or(BufferError::InvalidCapacity(capacity))?
            .next_power_of_two();
        Ok(Self {
            pcm: Arc::new(PcmRingBuffer::new(pcm_cap)),
            frame_capacity: capacity,
        })
    }

    pub fn pcm(&self) -> &PcmRingBuffer<T> {
        &self.pcm
    }

    /// Push an [`AudioFrame`] into the ring buffer, preserving its channel count.
    #[inline]
    pub fn push(&self, frame: AudioFrame<T>) -> bool {
        let ch = (frame.num_channels as usize).clamp(1, MAX_CHANNELS);
        let written = self.pcm.write_interleaved(&frame.channels[..ch], ch);
        written == 1
    }

    /// Pop a stereo [`AudioFrame`] from the ring buffer.
    #[inline]
    pub fn pop(&self) -> Option<AudioFrame<T>> {
        self.pop_multichannel(2)
    }

    /// Pop an N-channel [`AudioFrame`] from the ring buffer.
    #[inline]
    pub fn pop_multichannel(&self, channels: usize) -> Option<AudioFrame<T>> {
        let ch = channels.clamp(1, MAX_CHANNELS);
        let mut frame = AudioFrame {
            channels: [T::default(); MAX_CHANNELS],
            num_channels: ch as u8,
        };
        let n = self.pcm.read_interleaved(&mut frame.channels[..ch], ch);
        if n == 1 {
            Some(frame)
        } else {
            None
        }
    }

    /// Available frames in the buffer for a given channel count.
    #[inline]
    pub fn available_frames(&self, channels: usize) -> usize {
        self.pcm
            .available()
            .checked_div(channels)
            .unwrap_or(0)
            .min(self.frame_capacity)
    }

    /// Available stereo frames in the buffer.
    #[inline]
    pub fn available(&self) -> usize {
        self.available_frames(2)
    }

    pub fn reset(&self) {
        self.pcm.reset();
    }

    pub fn capacity(&self) -> usize {
        self.frame_capacity
    }

    #[inline]
    pub fn push_block_interleaved(&self, samples: &[T]) -> usize {
        let bounded = &samples[..samples.len().min(self.pcm.capacity())];
        self.pcm.push_block(bounded)
    }

    /// Push only complete interleaved frames. This prevents a multichannel
    /// producer from leaving a partial frame in the ring when the device FIFO
    /// is nearly full.
    #[inline]
    pub fn push_frames_interleaved(&self, samples: &[T], channels: usize) -> usize {
        if channels == 0 {
            return 0;
        }
        let max_samples = self.frame_capacity.saturating_mul(channels);
        let bounded_len = (samples.len().min(max_samples) / channels) * channels;
        self.pcm
            .write_interleaved(&samples[..bounded_len], channels)
    }

    #[inline]
    pub fn pop_block_interleaved(&self, out: &mut [T]) -> usize {
        let bounded_len = out.len().min(self.pcm.capacity());
        self.pcm.pop_block(&mut out[..bounded_len])
    }

    /// Pop only complete interleaved frames in one bulk operation.
    #[inline]
    pub fn pop_frames_interleaved(&self, out: &mut [T], channels: usize) -> usize {
        if channels == 0 {
            return 0;
        }
        let max_samples = self.frame_capacity.saturating_mul(channels);
        let bounded_len = (out.len().min(max_samples) / channels) * channels;
        self.pcm.read_interleaved(&mut out[..bounded_len], channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_frame_buffer_supports_f32_and_f64() {
        let f32_buffer = FixedFrameBuffer::<f32>::new(8).unwrap();
        assert_eq!(f32_buffer.capacity(), 8);
        f32_buffer.push(AudioFrame::stereo(0.5, 0.5));
        assert!((f32_buffer.pop().unwrap().get(0) - 0.5).abs() < 1e-6);

        let f64_buffer = FixedFrameBuffer::<f64>::new(8).unwrap();
        f64_buffer.push(AudioFrame::stereo(0.123456789012345, -0.987654321098765));
        let frame = f64_buffer.pop().unwrap();
        assert!((frame.get(0) - 0.123456789012345).abs() < 1e-14);
        assert!((frame.get(1) + 0.987654321098765).abs() < 1e-14);
    }

    #[test]
    fn fixed_frame_buffer_reset_is_safe() {
        let buffer = FixedFrameBuffer::<f32>::new(16).unwrap();
        for i in 0..8 {
            assert!(buffer.push(AudioFrame::stereo(i as f32, 0.0)));
        }
        assert_eq!(buffer.available(), 8);
        buffer.reset();
        assert_eq!(buffer.available(), 0);
        assert!(buffer.pop().is_none());
    }
}
