//! DSF / DFF container parsing and audio payload reading.

use std::{
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use crate::audio_io::AudioByteSource;
use crate::decode::{AudioFormatInfo, ChannelLayout};

use super::{DsdBlock, DsdError, DsdPcmBlock, DsdRate, DsdToPcmDecimator};

/// DSD Audio File Reader (DSF / DFF format parser + audio payload reader).
pub struct DsdReader {
    rate: DsdRate,
    /// Source channel count (1–6 for DSF; 1–2 for DFF).
    channels: usize,
    /// DSD samples per channel used for duration reporting
    /// (includes DSF's final-block zero padding).
    total_samples: u64,
    /// Bit order inside each payload byte: true = LSB-first (default).
    lsbf: bool,
    container: &'static str,
    reader: BufReader<Box<dyn AudioByteSource>>,
    /// Per-channel block size in bytes (DSF: `fmt ` field; DFF: 4096).
    block_size: u32,
    /// Absolute file offset of the first audio byte.
    data_start_offset: u64,
    /// Readable DSD frames per channel remaining in the file.
    audio_frames: u64,
    /// Frames per channel consumed so far.
    frames_consumed: u64,
    decimator: DsdToPcmDecimator,
}

impl DsdReader {
    /// Open a DSF or DFF source via a byte source.
    pub fn open_source(mut source: Box<dyn AudioByteSource>) -> Result<Self, DsdError> {
        let _ = source.seek(SeekFrom::Start(0));
        let mut reader = BufReader::new(source);

        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;

        if &magic == b"DSD " {
            Self::parse_dsf(reader)
        } else if &magic == b"FRM8" {
            Self::parse_dff(reader)
        } else {
            Err(DsdError::InvalidHeader(format!(
                "Unknown DSD container magic: {:?}",
                String::from_utf8_lossy(&magic)
            )))
        }
    }

    /// Open a DSF or DFF file (convenience wrapper).
    pub fn open(path: &Path) -> Result<Self, DsdError> {
        let source = crate::audio_io::FileByteSource::open(path).map_err(DsdError::Io)?;
        Self::open_source(Box::new(source))
    }

    fn parse_dsf(mut reader: BufReader<Box<dyn AudioByteSource>>) -> Result<Self, DsdError> {
        // Read DSF chunk size (8 bytes, little-endian)
        let mut buf8 = [0u8; 8];
        reader.read_exact(&mut buf8)?;
        let _chunk_size = u64::from_le_bytes(buf8);

        // Read total file size
        reader.read_exact(&mut buf8)?;
        let _total_size = u64::from_le_bytes(buf8);

        // Read metadata ptr (ID3v2 tag offset; may be 0)
        reader.read_exact(&mut buf8)?;

        // Now expect 'fmt ' chunk
        let mut fmt_magic = [0u8; 4];
        reader.read_exact(&mut fmt_magic)?;
        if &fmt_magic != b"fmt " {
            return Err(DsdError::InvalidHeader(
                "Missing 'fmt ' chunk in DSF".to_string(),
            ));
        }

        reader.read_exact(&mut buf8)?;
        let _fmt_size = u64::from_le_bytes(buf8);

        let mut buf4 = [0u8; 4];
        reader.read_exact(&mut buf4)?;
        let _fmt_version = u32::from_le_bytes(buf4);

        reader.read_exact(&mut buf4)?;
        let _fmt_id = u32::from_le_bytes(buf4);

        reader.read_exact(&mut buf4)?;
        let _channel_type = u32::from_le_bytes(buf4);

        reader.read_exact(&mut buf4)?;
        let channel_count = u32::from_le_bytes(buf4) as usize;

        reader.read_exact(&mut buf4)?;
        let sample_rate = u32::from_le_bytes(buf4);

        reader.read_exact(&mut buf4)?;
        let bits_per_sample = u32::from_le_bytes(buf4);

        reader.read_exact(&mut buf8)?;
        let sample_count = u64::from_le_bytes(buf8);

        reader.read_exact(&mut buf4)?;
        let block_size_per_channel = u32::from_le_bytes(buf4);

        // Skip reserved bytes
        reader.read_exact(&mut buf4)?;

        // Now locate 'data' chunk
        let mut data_magic = [0u8; 4];
        reader.read_exact(&mut data_magic)?;
        if &data_magic != b"data" {
            return Err(DsdError::InvalidHeader(
                "Missing 'data' chunk in DSF".to_string(),
            ));
        }

        reader.read_exact(&mut buf8)?;
        let data_chunk_size = u64::from_le_bytes(buf8);
        // The data chunk size field includes its own 12-byte header
        // ("data" magic + size field), per the DSF spec.
        let audio_bytes = data_chunk_size.saturating_sub(12);
        let data_start_offset = reader.stream_position()?;

        if !(1..=6).contains(&channel_count) {
            return Err(DsdError::UnsupportedChannels(channel_count));
        }
        if block_size_per_channel == 0 {
            return Err(DsdError::InvalidHeader(
                "DSF block size per channel is zero".to_string(),
            ));
        }

        let lsbf = match bits_per_sample {
            1 => true,
            8 => false,
            other => {
                return Err(DsdError::InvalidHeader(format!(
                    "Unsupported DSF bits-per-sample value: {} (expected 1 or 8)",
                    other
                )))
            }
        };

        let rate = DsdRate::from_hz(sample_rate).ok_or(DsdError::UnsupportedRate(sample_rate))?;

        Ok(Self {
            rate,
            channels: channel_count,
            total_samples: sample_count,
            lsbf,
            container: "DSF",
            reader,
            block_size: block_size_per_channel,
            data_start_offset,
            audio_frames: audio_bytes / channel_count as u64,
            frames_consumed: 0,
            decimator: DsdToPcmDecimator::new(channel_count),
        })
    }

    fn parse_dff(mut reader: BufReader<Box<dyn AudioByteSource>>) -> Result<Self, DsdError> {
        let mut buf8 = [0u8; 8];
        reader.read_exact(&mut buf8)?;
        let _total_size = u64::from_be_bytes(buf8);

        let mut dsd_magic = [0u8; 4];
        reader.read_exact(&mut dsd_magic)?;
        if &dsd_magic != b"DSD " {
            return Err(DsdError::InvalidHeader(
                "Missing 'DSD ' form in DFF".to_string(),
            ));
        }

        let mut rate = None;
        let mut channels = None;
        let mut audio_size = None;

        loop {
            let mut id = [0u8; 4];
            if reader.read_exact(&mut id).is_err() {
                // Reached EOF without finding the audio chunk.
                break;
            }
            reader.read_exact(&mut buf8)?;
            let size = u64::from_be_bytes(buf8);

            match &id {
                b"FVER" => skip_bytes(&mut reader, size)?,
                b"PROP" => {
                    if let Some((r, c)) = parse_dff_prop(&mut reader, size)? {
                        rate = Some(r);
                        channels = Some(c);
                    }
                }
                b"DSD " => {
                    audio_size = Some(size);
                    break;
                }
                b"DST " => {
                    return Err(DsdError::UnsupportedCompression(
                        "the file contains a 'DST ' chunk (DST-compressed DSD)".to_string(),
                    ))
                }
                _ => skip_bytes(&mut reader, size)?,
            }

            // IFF-style chunks are aligned to even byte boundaries.
            if size % 2 == 1 {
                skip_bytes(&mut reader, 1)?;
            }
        }

        let audio_size = audio_size.ok_or_else(|| {
            DsdError::InvalidHeader("Missing 'DSD ' audio chunk in DFF".to_string())
        })?;
        let rate = rate.ok_or_else(|| {
            DsdError::InvalidHeader(
                "Missing 'FS' sampling-frequency field in DFF PROP chunk".to_string(),
            )
        })?;
        let channels = channels.ok_or_else(|| {
            DsdError::InvalidHeader(
                "Missing 'CHNL' channel-count field in DFF PROP chunk".to_string(),
            )
        })?;
        if !(1..=2).contains(&channels) {
            return Err(DsdError::UnsupportedChannels(channels));
        }

        let rate = DsdRate::from_hz(rate).ok_or(DsdError::UnsupportedRate(rate))?;
        let data_start_offset = reader.stream_position()?;

        Ok(Self {
            rate,
            channels,
            total_samples: audio_size / channels as u64,
            lsbf: true,
            container: "DFF",
            reader,
            block_size: 4096,
            data_start_offset,
            audio_frames: audio_size / channels as u64,
            frames_consumed: 0,
            decimator: DsdToPcmDecimator::new(channels),
        })
    }

    /// Read the next block of raw DSD payload, de-interleaved per channel.
    ///
    /// At most `max_frames` DSD frames (per channel) are returned. The read never
    /// straddles a block boundary, so the returned data is always contiguous per
    /// channel. Returns `Ok(None)` at end of stream (or if `max_frames` is 0).
    ///
    /// The underlying file is positioned with a small per-channel seek because
    /// channel data is interleaved per block (`[ch0 block][ch1 block]…`), so a
    /// purely sequential read cannot return partial blocks.
    pub fn read_dsd_block(&mut self, max_frames: u32) -> Result<Option<DsdBlock>, DsdError> {
        let frames_remaining = self.audio_frames.saturating_sub(self.frames_consumed);
        if frames_remaining == 0 || max_frames == 0 {
            return Ok(None);
        }
        let want = (max_frames as u64).min(frames_remaining) as usize;

        // Clamp to the current per-channel block so each channel's bytes are
        // contiguous in the file.
        let block = self.block_size as u64;
        let frame_in_block = self.frames_consumed % block;
        let take = want.min((block - frame_in_block) as usize);
        if take == 0 {
            return Ok(None);
        }

        // DSF zero-pads its final block to a full `block_size` per channel; DFF
        // stores the final partial block compactly (no padding), so the channel
        // stride inside the final block is the partial length, not `block_size`.
        let block_idx = self.frames_consumed / block;
        let last_block_idx = (self.audio_frames - 1) / block;
        let nch = self.channels as u64;
        let final_partial = block_idx == last_block_idx && !self.audio_frames.is_multiple_of(block);
        let ch_stride = if final_partial {
            self.audio_frames % block
        } else {
            block
        };

        let mut channels = vec![vec![0u8; take]; self.channels];
        for (ch, buf) in channels.iter_mut().enumerate() {
            let file_off = self.data_start_offset
                + block_idx * block * nch
                + ch as u64 * ch_stride
                + frame_in_block;
            self.reader.seek(SeekFrom::Start(file_off))?;
            self.reader.read_exact(buf)?;
        }

        self.frames_consumed += take as u64;
        Ok(Some(DsdBlock {
            frames: take as u32,
            channels,
        }))
    }

    /// Read the next block and decimate it to f32 PCM in one step.
    ///
    /// Returns `Ok(None)` at end of stream. PCM output rate is the DSD bit rate
    /// divided by 32 (88.2 kHz for DSD64). A trailing block shorter than 8 bytes
    /// per channel may produce an empty PCM block (no samples) — that is not an error.
    pub fn decode_block(&mut self, max_frames: u32) -> Result<Option<DsdPcmBlock>, DsdError> {
        let Some(block) = self.read_dsd_block(max_frames)? else {
            return Ok(None);
        };
        let nch = block.channels.len();
        let mut pcm_channels: Vec<Vec<f32>> = (0..nch).map(|_| Vec::new()).collect();
        let channel_refs: Vec<&[u8]> = block.channels.iter().map(|c| c.as_slice()).collect();
        let mut out_refs: Vec<&mut Vec<f32>> = pcm_channels.iter_mut().collect();
        self.decimator
            .decimate_channels(&channel_refs, self.lsbf, &mut out_refs);
        Ok(Some(DsdPcmBlock {
            frames: block.frames,
            channels: pcm_channels,
        }))
    }

    pub fn rate(&self) -> DsdRate {
        self.rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// True when payload bytes are LSB-first (bit 0 = earliest sample).
    pub fn is_lsb_first(&self) -> bool {
        self.lsbf
    }

    /// Total readable DSD frames per channel (includes DSF block padding).
    pub fn total_dsd_frames(&self) -> u64 {
        self.audio_frames
    }

    /// Seek to an absolute DSD frame position (per channel).
    ///
    /// The decimator's filter state is reset so no pre-seek history bleeds
    /// into the new position (the first few PCM samples after a seek are
    /// warm-up from silence — the same transient a hardware DAC produces).
    /// The next [`read_dsd_block`](Self::read_dsd_block) starts at `frame`,
    /// which may land mid-block (per-channel seeks handle that).
    pub fn seek_to_dsd_frame(&mut self, frame: u64) {
        self.frames_consumed = frame.min(self.audio_frames);
        self.decimator.reset();
    }

    /// DSD frames per channel consumed so far.
    pub fn position_dsd_frames(&self) -> u64 {
        self.frames_consumed
    }

    /// DSD frames per channel remaining.
    pub fn frames_remaining(&self) -> u64 {
        self.audio_frames.saturating_sub(self.frames_consumed)
    }

    pub fn format_info(&self) -> AudioFormatInfo {
        AudioFormatInfo {
            codec: format!("{:?}", self.rate),
            container: self.container.to_string(),
            sample_rate: self.rate.sample_rate_hz(),
            input_sample_rate: None,
            channels: self.channels,
            channel_layout: ChannelLayout::from_count(self.channels),
            bit_depth: Some(1),
            sample_format: "dsd1".to_string(),
            duration_secs: if self.total_samples > 0 {
                Some(self.total_samples as f64 / self.rate.sample_rate_hz() as f64)
            } else {
                None
            },
            bitrate_kbps: Some(self.rate.sample_rate_hz() / 1000 * self.channels as u32),
            gapless: None,
            replaygain_track_db: None,
            replaygain_album_db: None,
            ebu_r128_loudness: None,
            true_peak_dbtp: None,
            is_lossless: true,
            is_dsd: true,
        }
    }
}

/// Read and discard `n` bytes (used to skip chunks we do not care about).
fn skip_bytes(
    reader: &mut BufReader<Box<dyn AudioByteSource>>,
    mut n: u64,
) -> Result<(), DsdError> {
    let mut buf = [0u8; 4096];
    while n > 0 {
        let take = n.min(buf.len() as u64) as usize;
        reader.read_exact(&mut buf[..take])?;
        n -= take as u64;
    }
    Ok(())
}

/// Parse the `PROP` chunk of a DFF file, extracting the `FS` sampling frequency
/// (real Hz, big-endian) and `CHNL` channel count, and validating the `CMPR`
/// compression type. Consumes exactly `size` bytes of the PROP chunk.
fn parse_dff_prop(
    reader: &mut BufReader<Box<dyn AudioByteSource>>,
    size: u64,
) -> Result<Option<(u32, usize)>, DsdError> {
    let mut remaining = size;

    let mut form = [0u8; 4];
    reader.read_exact(&mut form)?;
    remaining = remaining
        .checked_sub(4)
        .ok_or_else(|| DsdError::InvalidHeader("DFF PROP chunk too small".to_string()))?;
    if &form != b"SND " {
        return Err(DsdError::InvalidHeader(
            "Missing 'SND ' form type in DFF PROP chunk".to_string(),
        ));
    }

    let mut rate = None;
    let mut channels = None;

    while remaining > 0 {
        // Sub-chunk headers are ID + 8-byte size. The 'FS' ID is only 2 bytes
        // (a DSDIFF quirk); 'CHNL'/'CMPR' use the standard 4-byte IDs, so the
        // smallest header is 2 + 8 bytes.
        if remaining < 10 {
            // Tolerate a stray trailing byte (odd-sized chunk padding).
            skip_bytes(reader, remaining)?;
            break;
        }
        // Read the ID incrementally: 'FS' is only 2 bytes, so reading 4 bytes
        // up front would swallow the first two bytes of its size field.
        let mut id4 = [0u8; 4];
        reader.read_exact(&mut id4[..2])?;
        let id: &[u8] = if &id4[..2] == b"FS" {
            &id4[..2]
        } else {
            reader.read_exact(&mut id4[2..])?;
            &id4[..]
        };
        let id_len = id.len() as u64;
        let mut buf8 = [0u8; 8];
        reader.read_exact(&mut buf8)?;
        let sub_size = u64::from_be_bytes(buf8);
        remaining -= id_len + 8;
        if sub_size > remaining {
            return Err(DsdError::InvalidHeader(
                "DFF PROP sub-chunk overruns its PROP chunk".to_string(),
            ));
        }

        let consumed: u64 = match id {
            b"FS" => {
                let mut buf4 = [0u8; 4];
                reader.read_exact(&mut buf4)?;
                rate = Some(u32::from_be_bytes(buf4));
                4
            }
            b"CHNL" => {
                let mut buf2 = [0u8; 2];
                reader.read_exact(&mut buf2)?;
                channels = Some(u16::from_be_bytes(buf2) as usize);
                2
            }
            b"CMPR" => {
                let mut code = [0u8; 4];
                reader.read_exact(&mut code)?;
                if &code != b"DSD " {
                    return Err(DsdError::UnsupportedCompression(format!(
                        "compression code {:?} (only 'DSD ' is supported)",
                        String::from_utf8_lossy(&code)
                    )));
                }
                4
            }
            _ => 0,
        };
        if sub_size > consumed {
            skip_bytes(reader, sub_size - consumed)?;
        }
        remaining -= sub_size;
        if sub_size % 2 == 1 && remaining > 0 {
            skip_bytes(reader, 1)?;
            remaining -= 1;
        }
    }

    Ok(match (rate, channels) {
        (Some(r), Some(c)) => Some((r, c)),
        _ => None,
    })
}
