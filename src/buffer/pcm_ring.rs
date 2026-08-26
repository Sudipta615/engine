use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam::utils::CachePadded;

/// Lock-free single-producer single-consumer ring buffer of interleaved
/// PCM samples. Designed for the audio hot path between the decode
/// thread (producer) and the cpal audio callback (consumer).
pub struct PcmRingBuffer<T: Copy + Default + Send + Sync + 'static = f32> {
    /// Interleaved sample storage. Length is always a power of two.
    buf: UnsafeCell<Box<[T]>>,
    /// `buf.len() - 1`. Used as a bitmask for O(1) wrap-around.
    mask: usize,
    /// Total capacity in samples (== `buf.len()`).
    capacity: usize,
    /// Write position (producer-only). Wraps monotonically; the actual
    /// index in `buf` is `head & mask`.
    head: CachePadded<AtomicUsize>,
    /// Read position (consumer-only). Wraps monotonically; the actual
    /// index in `buf` is `tail & mask`.
    tail: CachePadded<AtomicUsize>,
}

impl<T: Copy + Default + Send + Sync + 'static> PcmRingBuffer<T> {
    /// Create a new ring buffer with at least `min_capacity` sample slots.
    /// The actual capacity is rounded up to the next power of two so the
    /// wrap-around can use a bitmask instead of a modulo.
    pub fn new(min_capacity: usize) -> Self {
        let cap = min_capacity.max(2).next_power_of_two();
        Self {
            buf: UnsafeCell::new(vec![T::default(); cap].into_boxed_slice()),
            mask: cap - 1,
            capacity: cap,
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
        }
    }

    /// Number of samples that can be pushed without blocking.
    #[inline]
    pub fn free_slots(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        self.capacity - head.wrapping_sub(tail)
    }

    /// Number of samples available to be popped.
    #[inline]
    pub fn available(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Push a block of interleaved samples into the ring buffer.
    /// Returns the number of samples actually written.
    #[inline]
    pub fn push_block(&self, samples: &[T]) -> usize {
        if samples.is_empty() {
            return 0;
        }
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let free = self.capacity - head.wrapping_sub(tail);
        let n = samples.len().min(free);
        if n == 0 {
            return 0;
        }
        let start = head & self.mask;
        let first = n.min(self.capacity - start);
        unsafe {
            let buf_ptr = self.buf.get();
            let buf_slice = std::slice::from_raw_parts_mut((*buf_ptr).as_mut_ptr(), self.capacity);
            buf_slice[start..start + first].copy_from_slice(&samples[..first]);
            let second = n - first;
            if second > 0 {
                buf_slice[..second].copy_from_slice(&samples[first..n]);
            }
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    /// Pop a block of interleaved samples from the ring buffer into `out`.
    /// Returns the number of samples actually read.
    #[inline]
    pub fn pop_block(&self, out: &mut [T]) -> usize {
        if out.is_empty() {
            return 0;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let available = head.wrapping_sub(tail);
        let n = out.len().min(available);
        if n == 0 {
            return 0;
        }
        let start = tail & self.mask;
        let first = n.min(self.capacity - start);
        unsafe {
            let buf_ptr = self.buf.get();
            let buf_slice = std::slice::from_raw_parts((*buf_ptr).as_ptr(), self.capacity);
            out[..first].copy_from_slice(&buf_slice[start..start + first]);
            let second = n - first;
            if second > 0 {
                out[first..n].copy_from_slice(&buf_slice[..second]);
            }
        }
        self.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Push whole frames of interleaved samples (each frame is `channels`
    /// consecutive samples). Only whole frames are written. Returns the
    /// number of frames actually written.
    #[inline]
    pub fn write_interleaved(&self, samples: &[T], channels: usize) -> usize {
        if channels == 0 || samples.len() < channels {
            return 0;
        }
        let free = self.free_slots();
        let n_frames = (samples.len() / channels).min(free / channels);
        let n = n_frames * channels;
        if n == 0 {
            return 0;
        }
        self.push_block(&samples[..n]);
        n_frames
    }

    /// Pop whole frames of interleaved samples (each frame is `channels`
    /// consecutive samples) into `out`. Only whole frames are read. Returns
    /// the number of frames actually read.
    #[inline]
    pub fn read_interleaved(&self, out: &mut [T], channels: usize) -> usize {
        if channels == 0 || out.len() < channels {
            return 0;
        }
        let available = self.available();
        let n_frames = (out.len() / channels).min(available / channels);
        let n = n_frames * channels;
        if n == 0 {
            return 0;
        }
        self.pop_block(&mut out[..n]);
        n_frames
    }

    /// Reset the ring to empty.
    pub fn reset(&self) {
        const MAX_RESET_RETRIES: usize = 8;
        for _ in 0..MAX_RESET_RETRIES {
            let head = self.head.load(Ordering::Acquire);
            let tail = self.tail.load(Ordering::Relaxed);
            if tail == head {
                return;
            }
            if self
                .tail
                .compare_exchange(tail, head, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
        let head = self.head.load(Ordering::Acquire);
        self.tail.store(head, Ordering::Release);
    }
}

unsafe impl<T: Copy + Default + Send + Sync + 'static> Send for PcmRingBuffer<T> {}
unsafe impl<T: Copy + Default + Send + Sync + 'static> Sync for PcmRingBuffer<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_round_trips_interleaved_frames() {
        let ring = PcmRingBuffer::<f32>::new(8);
        let input = [0.0, 1.0, 2.0, 3.0];
        assert_eq!(ring.write_interleaved(&input, 2), 2);
        let mut output = [0.0; 4];
        assert_eq!(ring.read_interleaved(&mut output, 2), 2);
        assert_eq!(output, input);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn ring_wraps_and_resets() {
        let ring = PcmRingBuffer::<u8>::new(4);
        assert_eq!(ring.push_block(&[1, 2, 3, 4]), 4);
        let mut first = [0; 3];
        assert_eq!(ring.pop_block(&mut first), 3);
        assert_eq!(first, [1, 2, 3]);
        assert_eq!(ring.push_block(&[5, 6, 7]), 3);
        let mut all = [0; 4];
        assert_eq!(ring.pop_block(&mut all), 4);
        assert_eq!(all, [4, 5, 6, 7]);
        ring.reset();
        assert_eq!(ring.available(), 0);
    }
}
