//! Streaming IEEE-float32 WAV writer.
//!
//! Writes interleaved `f32` frames to a RIFF/WAVE file with a placeholder
//! header, updating the RIFF/data sizes on finalize so the file is valid
//! even if it is written incrementally over a long capture. Float32 was
//! chosen as the capture container: it is lossless relative to the WASAPI
//! mix-format samples the capture path produces, needs no dither, and is
//! readable by every WAV consumer (foobar2000, Audacity, sox, ffmpeg...).
//!
//! The writer is deliberately tiny and synchronous — it is driven from the
//! engine tick loop (a control thread), never from a realtime callback.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// Error returned by [`WavFileWriter`].
#[derive(Debug, thiserror::Error)]
pub enum WavWriteError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// A streaming interleaved-f32 WAV file.
pub struct WavFileWriter {
    writer: BufWriter<File>,
    /// Total data bytes written so far (excluding the 44-byte header).
    data_bytes: u64,
    sample_rate: u32,
    channels: u16,
    finalized: bool,
}

const HEADER_SIZE: u64 = 44;

impl WavFileWriter {
    /// Create a new float32 WAV at `path` with the given rate/channels.
    /// Writes the placeholder header immediately; call [`Self::finalize`]
    /// before dropping to fill in the size fields.
    pub fn create(path: &Path, sample_rate: u32, channels: u16) -> Result<Self, WavWriteError> {
        let mut writer = BufWriter::new(File::create(path)?);
        writer.write_all(&[0u8; HEADER_SIZE as usize])?;
        Ok(Self {
            writer,
            data_bytes: 0,
            sample_rate,
            channels: channels.max(1),
            finalized: false,
        })
    }

    /// Append interleaved `f32` frames (`samples.len()` must be a multiple
    /// of the channel count; trailing partial frames are dropped).
    pub fn write_frames(&mut self, samples: &[f32]) -> io::Result<()> {
        let ch = self.channels as usize;
        let complete = (samples.len() / ch) * ch;
        let mut buf = [0u8; 4096];
        let mut i = 0;
        while i < complete {
            let mut n = 0;
            while i < complete && n + 4 <= buf.len() {
                buf[n..n + 4].copy_from_slice(&samples[i].to_le_bytes());
                n += 4;
                i += 1;
            }
            self.writer.write_all(&buf[..n])?;
        }
        self.data_bytes += (complete * 4) as u64;
        Ok(())
    }

    /// Finalize the file: write the RIFF header with the real sizes and
    /// flush. Idempotent. Must be called before the writer is dropped for
    /// the file to be valid.
    pub fn finalize(&mut self) -> io::Result<()> {
        if self.finalized {
            return Ok(());
        }
        self.writer.flush()?;
        let data_len = self.data_bytes;

        let header = build_header(self.sample_rate, self.channels, data_len as u32);
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(&header)?;
        self.writer.seek(SeekFrom::End(0))?;
        self.writer.flush()?;
        self.finalized = true;
        Ok(())
    }

    /// Bytes of audio data written so far (before the header).
    pub fn data_bytes(&self) -> u64 {
        self.data_bytes
    }

    /// Frames written so far.
    pub fn frames_written(&self) -> u64 {
        if self.channels == 0 {
            return 0;
        }
        self.data_bytes / 4 / self.channels as u64
    }
}

impl Drop for WavFileWriter {
    fn drop(&mut self) {
        // Best-effort finalize so a dropped (not explicitly finalized)
        // writer still produces a playable file.
        let _ = self.finalize();
    }
}

/// Build a 44-byte RIFF/WAVE header for float32 PCM.
fn build_header(sample_rate: u32, channels: u16, data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let byte_rate = sample_rate * channels as u32 * 4;
    let block_align = channels * 4;

    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36u32 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    h[20..22].copy_from_slice(&3u16.to_le_bytes()); // WAVE_FORMAT_IEEE_FLOAT
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&32u16.to_le_bytes()); // bits per sample
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn header_and_payload_are_valid() {
        let dir = std::env::temp_dir().join(format!("engine_wav_writer_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.wav");

        let mut w = WavFileWriter::create(&path, 48000, 2).unwrap();
        // 100 frames of stereo, plus a partial frame that must be dropped.
        let mut samples = Vec::with_capacity(201);
        for i in 0..100 {
            samples.push((i as f32) / 100.0);
            samples.push(-(i as f32) / 100.0);
        }
        samples.push(0.5); // incomplete frame → dropped
        w.write_frames(&samples).unwrap();
        assert_eq!(w.frames_written(), 100);
        w.finalize().unwrap();

        let mut bytes = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();

        // Header fields.
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[20..22], &3u16.to_le_bytes(), "IEEE float");
        assert_eq!(&bytes[22..24], &2u16.to_le_bytes(), "channels");
        assert_eq!(&bytes[24..28], &48000u32.to_le_bytes(), "rate");
        assert_eq!(&bytes[34..36], &32u16.to_le_bytes(), "bits");
        // data length = 100 frames * 2 ch * 4 bytes.
        assert_eq!(&bytes[40..44], &800u32.to_le_bytes(), "data size");

        // First payload sample round-trips.
        let first = f32::from_le_bytes(bytes[44..48].try_into().unwrap());
        assert!((first - 0.0).abs() < 1e-6);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
