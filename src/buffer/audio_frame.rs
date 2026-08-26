use super::{BufferError, MAX_CHANNELS};

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
        for ((slot, a), b) in channels
            .iter_mut()
            .zip(self.channels.iter())
            .zip(other.channels.iter())
        {
            *slot = a + t * (b - a);
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
        for ((slot, a), b) in channels
            .iter_mut()
            .zip(self.channels.iter())
            .zip(other.channels.iter())
        {
            *slot = a + t * (b - a);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frame_helpers_preserve_channel_behavior() {
        let stereo: AudioFrame<f32> = AudioFrame::stereo(0.5, -0.3);
        assert_eq!(stereo.num_channels, 2);
        assert!((stereo.get(0) - 0.5).abs() < 1e-6);
        assert!((stereo.get(1) + 0.3).abs() < 1e-6);
        assert!((stereo.get(2) - 0.0).abs() < 1e-6);

        let mono: AudioFrame<f32> = AudioFrame::mono(0.75);
        assert_eq!(mono.num_channels, 1);
        assert!((mono.get(0) - 0.75f32).abs() < 1e-6);
        assert!((mono.get(1) - 0.75f32).abs() < 1e-6);

        let mut frame: AudioFrame<f32> = AudioFrame::zero_stereo();
        frame.set(0, 0.5);
        frame.set(MAX_CHANNELS, 1.0);
        assert!((frame.get(0) - 0.5f32).abs() < 1e-6);
        assert!((frame.get(1) - 0.0f32).abs() < 1e-6);
    }

    #[test]
    fn audio_frame_interpolation_and_precision_helpers_work() {
        let a = AudioFrame::stereo(0.0f32, 1.0);
        let b = AudioFrame::stereo(1.0f32, 0.0);
        let mid = a.lerp(&b, 0.5);
        assert_eq!(mid.num_channels, 2);
        assert!((mid.get(0) - 0.5).abs() < 1e-6);
        assert!((mid.get(1) - 0.5).abs() < 1e-6);

        let mono = AudioFrame::mono(0.4);
        let stereo = AudioFrame::stereo(0.6, 0.8);
        let promoted = mono.lerp(&stereo, 0.5);
        assert_eq!(promoted.num_channels, 2);
        assert!((promoted.get(0) - 0.5).abs() < 1e-6);
        assert!((promoted.get(1) - 0.6).abs() < 1e-6);

        let f64_frame: AudioFrame<f64> = AudioFrame::stereo(0.5, -0.3);
        assert!((f64_frame.get(0) - 0.5).abs() < 1e-12);
        assert!((f64_frame.get(1) + 0.3).abs() < 1e-12);
    }

    #[test]
    fn audio_frame_zero_validates_channels() {
        assert!(AudioFrame::<f32>::zero(0).is_err());
        assert!(AudioFrame::<f32>::zero((MAX_CHANNELS + 1) as u8).is_err());
    }

    #[test]
    fn audio_chunk_handles_empty_and_populated_batches() {
        let chunk = AudioChunk::<f32>::new(44100, 100);
        assert_eq!(chunk.len(), 100);
        assert_eq!(chunk.sample_rate, 44100);
        assert!(!chunk.is_empty());
        assert_eq!(chunk.num_channels(), 2);

        let empty = AudioChunk::<f32>::new(44100, 0);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }
}
