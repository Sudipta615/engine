use super::PcmRingBuffer;

/// Lock-free single-producer single-consumer byte ring for native DSD
/// transport (§7).
///
/// Native DSD bitstreams cannot be represented in the f32 PCM ring (the DoP
/// path cheats by packing 24-bit words, but raw 1-bit DSD has no PCM form).
/// This buffer carries the exact interleaved byte stream the DAC expects:
///
/// - **DSD_U8**: 1 byte per channel per frame; a stereo frame is `[ch0][ch1]`,
///   and each byte holds 8 DSD samples (LSB-first).
/// - **DSD_U16**: 2 bytes per channel per frame (16 DSD samples per word),
///   endianness per the negotiated format.
/// - **DSD_U32**: 4 bytes per channel per frame (32 DSD samples per word).
///
/// `frame_width = channels * bytes_per_word` for the negotiated format. The
/// producer (engine tick) pushes interleaved bytes; the consumer (ALSA render
/// thread) pops them and writes via `snd_pcm_writei`. Push/pop are strictly
/// lock-free and allocation-free, matching the f32 ring's discipline.
pub struct DsdByteBuffer {
    ring: PcmRingBuffer<u8>,
}

impl DsdByteBuffer {
    /// Create a byte ring with at least `min_capacity` bytes (rounded up to a
    /// power of two). Sized for the device's worst-case period so the render
    /// thread never starves between ticks.
    pub fn new(min_capacity: usize) -> Self {
        Self {
            ring: PcmRingBuffer::<u8>::new(min_capacity),
        }
    }

    /// Number of bytes available to the consumer.
    #[inline]
    pub fn available_bytes(&self) -> usize {
        self.ring.available()
    }

    /// Total byte capacity.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }

    /// Push whole DSD frames. A frame is `frame_width` interleaved bytes
    /// (e.g. 2 for stereo DSD_U8, 4 for stereo DSD_U16). Returns the number
    /// of frames actually written.
    #[inline]
    pub fn push_frames(&self, bytes: &[u8], frame_width: usize) -> usize {
        if frame_width == 0 || bytes.len() < frame_width {
            return 0;
        }
        let free = self.ring.free_slots();
        let n_frames = (bytes.len() / frame_width).min(free / frame_width);
        let n = n_frames * frame_width;
        if n == 0 {
            return 0;
        }
        self.ring.push_block(&bytes[..n]);
        n_frames
    }

    /// Pop whole DSD frames into `out`. Returns the number of frames read.
    #[inline]
    pub fn pop_frames(&self, out: &mut [u8], frame_width: usize) -> usize {
        if frame_width == 0 || out.len() < frame_width {
            return 0;
        }
        let available = self.ring.available();
        let n_frames = (out.len() / frame_width).min(available / frame_width);
        let n = n_frames * frame_width;
        if n == 0 {
            return 0;
        }
        self.ring.pop_block(&mut out[..n]);
        n_frames
    }

    /// Pop up to `max_bytes` raw bytes (for diagnostics/backpressure tests).
    #[inline]
    pub fn pop_bytes(&self, out: &mut [u8]) -> usize {
        self.ring.pop_block(out)
    }

    /// Drop all buffered bytes.
    pub fn reset(&self) {
        self.ring.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsd_buffer_preserves_stereo_frame_alignment() {
        let buffer = DsdByteBuffer::new(8);
        assert_eq!(buffer.push_frames(&[1, 2, 3, 4, 5, 6, 7, 8], 2), 4);
        assert_eq!(buffer.push_frames(&[9, 10], 2), 0);
        let mut one = [0; 1];
        assert_eq!(buffer.pop_bytes(&mut one), 1);
        assert_eq!(buffer.push_frames(&[9, 10], 2), 0);
        let mut rest = [0; 7];
        assert_eq!(buffer.pop_bytes(&mut rest), 7);
    }

    #[test]
    fn dsd_buffer_round_trips_multiple_wire_widths() {
        let buffer = DsdByteBuffer::new(1024);
        let input: Vec<u8> = (0..80).collect();
        assert_eq!(buffer.push_frames(&input[..64], 2), 32);
        let mut output = vec![0; 64];
        assert_eq!(buffer.pop_frames(&mut output, 2), 32);
        assert_eq!(output, input[..64]);

        assert_eq!(buffer.push_frames(&input, 8), 10);
        let mut output = vec![0; 80];
        assert_eq!(buffer.pop_frames(&mut output, 8), 10);
        assert_eq!(output, input);
    }

    #[test]
    fn dsd_buffer_reset_discards_bytes() {
        let buffer = DsdByteBuffer::new(1024);
        buffer.push_frames(&[0x69; 64], 2);
        assert_eq!(buffer.available_bytes(), 64);
        buffer.reset();
        assert_eq!(buffer.available_bytes(), 0);
    }
}
