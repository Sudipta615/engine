use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam::utils::CachePadded;

/// Maximum number of frames accepted by one realtime DSP operation.
///
/// Callers that receive larger decoder/device blocks must split them before
/// entering the audio path. Keeping this contract explicit lets every DSP
/// stage preallocate for the same worst-case callback size.
pub const MAX_AUDIO_BLOCK_FRAMES: usize = 4096;
/// Maximum number of frames in the decode-to-DSP buffer
pub const DECODE_BUFFER_FRAMES: usize = 16384;
/// Maximum number of frames in the DSP-to-output buffer
pub const OUTPUT_BUFFER_FRAMES: usize = 8192;
/// Default sample rate
pub const DEFAULT_SAMPLE_RATE: u32 = 44100;
/// Maximum channels we support (16, including 7.1.4 and future layouts).
/// The commonly used 7.1.4 layout occupies 12 channels; keeping headroom
/// here avoids advertising a layout that the realtime frame/scratch types
/// cannot actually carry.
pub const MAX_CHANNELS: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum AudioBlockError {
    #[error("audio block has {frames} frames; maximum is {max}")]
    TooLarge { frames: usize, max: usize },
}

#[inline]
pub fn validate_audio_block(frames: usize) -> Result<(), AudioBlockError> {
    if frames > MAX_AUDIO_BLOCK_FRAMES {
        Err(AudioBlockError::TooLarge {
            frames,
            max: MAX_AUDIO_BLOCK_FRAMES,
        })
    } else {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    #[error("FixedFrameBuffer capacity must be > 0, got {0}")]
    InvalidCapacity(usize),
    #[error("AudioFrame channel count must be between 1 and {0}")]
    InvalidChannelCount(u8),
}

/// A single audio frame (interleaved, up to MAX_CHANNELS)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioFrame<T = f32> {
    pub channels: [T; MAX_CHANNELS],
    pub num_channels: u8,
}

pub type AudioFrameF32 = AudioFrame<f32>;
pub type AudioFrameF64 = AudioFrame<f64>;

impl<T: Copy + Default> AudioFrame<T> {
    #[inline]
    pub fn stereo(left: T, right: T) -> Self {
        let mut channels = [T::default(); MAX_CHANNELS];
        channels[0] = left;
        channels[1] = right;
        Self {
            channels,
            num_channels: 2,
        }
    }

    /// Create a mono frame. The sample is duplicated to both channels so that
    /// downstream stereo code (output device, stereo pipeline) receives the
    /// correct signal on both L and R instead of silence on the right channel.
    #[inline]
    pub fn mono(sample: T) -> Self {
        let mut channels = [T::default(); MAX_CHANNELS];
        channels[0] = sample;
        channels[1] = sample;
        Self {
            channels,
            num_channels: 1,
        }
    }

    /// Create a multi-channel frame from a slice of channel samples.
    #[inline]
    pub fn multichannel(ch_slice: &[T]) -> Self {
        let mut channels = [T::default(); MAX_CHANNELS];
        let n = ch_slice.len().min(MAX_CHANNELS);
        for (i, &s) in ch_slice.iter().take(n).enumerate() {
            channels[i] = s;
        }
        Self {
            channels,
            num_channels: n as u8,
        }
    }

    #[inline]
    pub fn zero(num_channels: u8) -> Result<Self, BufferError> {
        if num_channels == 0 || num_channels > MAX_CHANNELS as u8 {
            return Err(BufferError::InvalidChannelCount(MAX_CHANNELS as u8));
        }
        Ok(Self {
            channels: [T::default(); MAX_CHANNELS],
            num_channels,
        })
    }

    #[inline]
    pub fn zero_stereo() -> Self {
        let channels = [T::default(); MAX_CHANNELS];
        Self {
            channels,
            num_channels: 2,
        }
    }

    #[inline]
    pub fn get(&self, channel: usize) -> T {
        self.channels.get(channel).copied().unwrap_or_default()
    }

    #[inline]
    pub fn set(&mut self, channel: usize, value: T) {
        if channel < MAX_CHANNELS {
            self.channels[channel] = value;
        }
    }
}

impl AudioFrame<f32> {
    /// Scale all channel slots by `gain`.
    #[inline]
    pub fn scale(&mut self, gain: f32) {
        for ch in &mut self.channels {
            *ch *= gain;
        }
    }

    /// Interpolate between two frames.
    #[inline]
    pub fn lerp(&self, other: &AudioFrame<f32>, t: f32) -> AudioFrame<f32> {
        let mut channels = [0.0f32; MAX_CHANNELS];
        for i in 0..MAX_CHANNELS {
            channels[i] = self.channels[i] + t * (other.channels[i] - self.channels[i]);
        }
        AudioFrame {
            channels,
            num_channels: self.num_channels.max(other.num_channels),
        }
    }
}

impl AudioFrame<f64> {
    /// Scale all channel slots by `gain` in f64 precision.
    #[inline]
    pub fn scale_f64(&mut self, gain: f64) {
        for ch in &mut self.channels {
            *ch *= gain;
        }
    }

    /// Interpolate between two frames in native f64 precision.
    #[inline]
    pub fn lerp_f64(&self, other: &AudioFrame<f64>, t: f64) -> AudioFrame<f64> {
        let mut channels = [0.0f64; MAX_CHANNELS];
        for i in 0..MAX_CHANNELS {
            channels[i] = self.channels[i] + t * (other.channels[i] - self.channels[i]);
        }
        AudioFrame {
            channels,
            num_channels: self.num_channels.max(other.num_channels),
        }
    }
}

/// A chunk of audio frames for batch processing
#[derive(Debug, Clone)]
pub struct AudioChunk<T = f32> {
    pub frames: Vec<AudioFrame<T>>,
    pub sample_rate: u32,
}

impl<T: Copy + Default> AudioChunk<T> {
    pub fn new(sample_rate: u32, capacity: usize) -> Self {
        let mut frames = Vec::with_capacity(capacity);
        frames.resize(capacity, AudioFrame::stereo(T::default(), T::default()));
        Self {
            frames,
            sample_rate,
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn num_channels(&self) -> u8 {
        self.frames.first().map(|f| f.num_channels).unwrap_or(2)
    }
}

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
        let ch = (frame.num_channels as usize).max(1).min(MAX_CHANNELS);
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
        let ch = channels.max(1).min(MAX_CHANNELS);
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
        if channels == 0 {
            0
        } else {
            (self.pcm.available() / channels).min(self.frame_capacity)
        }
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

/// Unified handle for audio output buffers supporting both f32 and f64 precisions.
#[derive(Clone)]
pub enum OutputBuffer {
    F32(Arc<FixedFrameBuffer<f32>>),
    F64(Arc<FixedFrameBuffer<f64>>),
}

impl OutputBuffer {
    pub fn new_f32(capacity: usize) -> Result<Self, BufferError> {
        Ok(Self::F32(Arc::new(FixedFrameBuffer::<f32>::new(capacity)?)))
    }

    pub fn new_f64(capacity: usize) -> Result<Self, BufferError> {
        Ok(Self::F64(Arc::new(FixedFrameBuffer::<f64>::new(capacity)?)))
    }

    pub fn reset(&self) {
        match self {
            Self::F32(b) => b.reset(),
            Self::F64(b) => b.reset(),
        }
    }

    pub fn available(&self) -> usize {
        match self {
            Self::F32(b) => b.available(),
            Self::F64(b) => b.available(),
        }
    }

    pub fn capacity(&self) -> usize {
        match self {
            Self::F32(b) => b.capacity(),
            Self::F64(b) => b.capacity(),
        }
    }

    pub fn as_f32(&self) -> Option<&Arc<FixedFrameBuffer<f32>>> {
        match self {
            Self::F32(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<&Arc<FixedFrameBuffer<f64>>> {
        match self {
            Self::F64(b) => Some(b),
            _ => None,
        }
    }
}

pub use crate::commands::EngineCommand;
pub use crate::dsp_utils::{
    enable_flush_zero_denormals_on_current_thread, flush_denormal, flush_denormal_f64,
    DENORMAL_OFFSET,
};
pub use crate::playback_info::{PlaybackInfo, PlaybackState};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_block_limit() {
        assert!(validate_audio_block(MAX_AUDIO_BLOCK_FRAMES).is_ok());
        assert!(matches!(
            validate_audio_block(MAX_AUDIO_BLOCK_FRAMES + 1),
            Err(AudioBlockError::TooLarge { .. })
        ));
    }

    #[test]
    fn test_audio_frame_stereo() {
        let f: AudioFrame<f32> = AudioFrame::stereo(0.5f32, -0.3f32);
        assert_eq!(f.num_channels, 2);
        assert!((f.get(0) - 0.5f32).abs() < 1e-6);
        assert!((f.get(1) - (-0.3f32)).abs() < 1e-6);
        assert!((f.get(2) - 0.0f32).abs() < 1e-6); // out of range returns 0
    }

    #[test]
    fn test_audio_frame_mono() {
        let f: AudioFrame<f32> = AudioFrame::mono(0.75f32);
        assert_eq!(f.num_channels, 1);
        assert!((f.get(0) - 0.75f32).abs() < 1e-6);
        assert!((f.get(1) - 0.75f32).abs() < 1e-6); // mono duplicates to ch1
    }

    #[test]
    fn test_audio_frame_zero() {
        let f: AudioFrame<f32> = AudioFrame::zero_stereo();
        assert_eq!(f.num_channels, 2);
        assert!((f.get(0) - 0.0f32).abs() < 1e-6);
    }

    #[test]
    fn test_audio_frame_zero_invalid_channels() {
        assert!(AudioFrame::<f32>::zero(0).is_err());
        assert!(AudioFrame::<f32>::zero((MAX_CHANNELS + 1) as u8).is_err());
    }

    #[test]
    fn test_audio_frame_scale() {
        let mut f: AudioFrame<f32> = AudioFrame::stereo(1.0f32, 2.0f32);
        f.scale(0.5f32);
        assert!((f.get(0) - 0.5f32).abs() < 1e-6);
        assert!((f.get(1) - 1.0f32).abs() < 1e-6);
    }

    #[test]
    fn test_audio_frame_lerp_same_channels() {
        let a = AudioFrame::stereo(0.0f32, 1.0f32);
        let b = AudioFrame::stereo(1.0f32, 0.0f32);
        let mid = a.lerp(&b, 0.5f32);
        assert_eq!(mid.num_channels, 2);
        assert!((mid.get(0) - 0.5f32).abs() < 1e-6);
        assert!((mid.get(1) - 0.5f32).abs() < 1e-6);
    }

    #[test]
    fn test_audio_frame_lerp_mono_stereo_promotes() {
        let a = AudioFrame::mono(0.4f32);
        let b = AudioFrame::stereo(0.6f32, 0.8f32);
        let result = a.lerp(&b, 0.5f32);
        assert_eq!(result.num_channels, 2);
        assert!((result.get(0) - 0.5f32).abs() < 1e-6);
        assert!((result.get(1) - 0.6f32).abs() < 1e-6); // mono ch0 duplicated, not 0
    }

    #[test]
    fn test_audio_frame_f64() {
        let f: AudioFrame<f64> = AudioFrame::stereo(0.5f64, -0.3f64);
        assert_eq!(f.num_channels, 2);
        assert!((f.get(0) - 0.5f64).abs() < 1e-12);
        assert!((f.get(1) - (-0.3f64)).abs() < 1e-12);
    }

    #[test]
    fn test_audio_frame_set() {
        let mut f: AudioFrame<f32> = AudioFrame::stereo(0.0f32, 0.0f32);
        f.set(0, 0.5f32);
        assert!((f.get(0) - 0.5f32).abs() < 1e-6);
        f.set(5, 1.0f32); // out of range, should be no-op
        assert!((f.get(1) - 0.0f32).abs() < 1e-6);
    }

    #[test]
    fn test_fixed_frame_buffer_compat() {
        let buf = FixedFrameBuffer::<f32>::new(8).unwrap();
        assert_eq!(buf.capacity(), 8);
        buf.push(AudioFrame::stereo(0.5f32, 0.5f32));
        let f = buf.pop().unwrap();
        assert!((f.get(0) - 0.5f32).abs() < 1e-6);
    }

    #[test]
    fn test_fixed_frame_buffer_f64() {
        let buf = FixedFrameBuffer::<f64>::new(8).unwrap();
        assert_eq!(buf.capacity(), 8);
        buf.push(AudioFrame::stereo(
            0.123456789012345f64,
            -0.987654321098765f64,
        ));
        let f = buf.pop().unwrap();
        assert!((f.get(0) - 0.123456789012345f64).abs() < 1e-14);
        assert!((f.get(1) - (-0.987654321098765f64)).abs() < 1e-14);
    }

    #[test]
    fn test_fixed_frame_buffer_reset_is_safe() {
        let buf = FixedFrameBuffer::<f32>::new(16).unwrap();
        for i in 0..8 {
            assert!(buf.push(AudioFrame::stereo(i as f32, 0.0f32)));
        }
        assert_eq!(buf.available(), 8);
        buf.reset();
        assert_eq!(buf.available(), 0);
        assert!(buf.pop().is_none());
    }

    #[test]
    fn test_playback_info_default() {
        let info = PlaybackInfo::default();
        assert_eq!(info.state, PlaybackState::Stopped);
        assert_eq!(info.position_secs, 0.0);
        assert!(
            (info.volume - 0.75).abs() < 1e-6,
            "default volume should be 0.75, got {}",
            info.volume
        );
        assert_eq!(info.cpu_overloads, 0);
        assert!(!info.resampler_disabled);
        assert!(!info.convolution_ir_needs_reload);
    }

    #[test]
    fn test_engine_command_debug_clone() {
        let cmd = EngineCommand::Seek(42.5);
        let cloned = cmd.clone();
        assert_eq!(cmd, cloned);
        let debug_str = format!("{:?}", cmd);
        assert!(debug_str.contains("Seek"));
    }

    #[test]
    fn test_flush_denormal() {
        assert!((flush_denormal(0.0) - 0.0).abs() < 1e-15);
        assert!((flush_denormal(1e-40) - 0.0).abs() < 1e-45);
        assert!((flush_denormal(1e-20) - 1e-20).abs() < 1e-25);
        assert!((flush_denormal(0.5) - 0.5).abs() < 1e-15);
    }

    #[test]
    fn test_audio_chunk() {
        let chunk = AudioChunk::<f32>::new(44100, 100);
        assert_eq!(chunk.len(), 100);
        assert_eq!(chunk.sample_rate, 44100);
        assert!(!chunk.is_empty());
        assert_eq!(chunk.num_channels(), 2);
    }

    #[test]
    fn test_dsd_byte_buffer_stereo_u8_frames() {
        // Stereo DSD_U8: frame width = 2 bytes (one byte per channel per
        // 8-DSD-sample group). The byte stream must round-trip frame-aligned.
        let buf = DsdByteBuffer::new(4096);
        let frames: Vec<u8> = (0..64u8).collect(); // 32 frames of 2 bytes
        let written = buf.push_frames(&frames, 2);
        assert_eq!(written, 32);
        assert_eq!(buf.available_bytes(), 64);
        let mut out = vec![0u8; 64];
        let read = buf.pop_frames(&mut out, 2);
        assert_eq!(read, 32);
        assert_eq!(out, frames);
        assert_eq!(buf.available_bytes(), 0);
    }

    #[test]
    fn test_dsd_byte_buffer_never_splits_frames() {
        // A full ring must reject a partial frame instead of leaving a
        // dangling half-frame (which would desync the DSD stream).
        let buf = DsdByteBuffer::new(8); // capacity rounds up to 8 bytes
                                         // Fill with 4 stereo frames.
        let written = buf.push_frames(&[1u8; 8], 2);
        assert_eq!(written, 4);
        // One more frame does not fit (free = 0) -> nothing written.
        assert_eq!(buf.push_frames(&[2u8; 2], 2), 0);
        // Even with 1 byte of free space (odd consumer read), a 2-byte frame
        // must not be written.
        buf.pop_bytes(&mut [0u8; 1]);
        assert_eq!(buf.push_frames(&[3u8; 2], 2), 0);
        let mut out = vec![0u8; 7];
        let n = buf.pop_bytes(&mut out);
        assert_eq!(n, 7);
    }

    #[test]
    fn test_dsd_byte_buffer_u32_frame_width() {
        // Stereo DSD_U32: 4 bytes per channel per frame -> frame width 8.
        let buf = DsdByteBuffer::new(1024);
        let frames: Vec<u8> = (0..80u8).collect(); // 10 frames of 8 bytes
        assert_eq!(buf.push_frames(&frames, 8), 10);
        let mut out = vec![0u8; 80];
        assert_eq!(buf.pop_frames(&mut out, 8), 10);
        assert_eq!(out, frames);
        // A byte slice that is not a multiple of the frame width is clamped
        // to whole frames: 9 bytes -> 1 whole 8-byte frame, 1 byte dropped.
        assert_eq!(buf.push_frames(&[9u8; 9], 8), 1);
        assert_eq!(buf.available_bytes(), 8);
        buf.reset();
        assert_eq!(buf.push_frames(&[9u8; 16], 8), 2);
    }

    #[test]
    fn test_dsd_byte_buffer_reset() {
        let buf = DsdByteBuffer::new(1024);
        buf.push_frames(&[0x69u8; 64], 2);
        assert_eq!(buf.available_bytes(), 64);
        buf.reset();
        assert_eq!(buf.available_bytes(), 0);
    }

    #[test]
    fn test_audio_chunk_empty() {
        let chunk = AudioChunk::<f32>::new(44100, 0);
        assert!(chunk.is_empty());
        assert_eq!(chunk.len(), 0);
    }
}
