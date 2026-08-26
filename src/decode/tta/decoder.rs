//! TTA container parser, frame decoder, and public decoder handle.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use super::bitstream::BitReaderLsb;
use super::crc::crc32;
use super::filter::{pred, zigzag_decode, TtaFilter};
use super::rice::TtaRice;
use crate::decode::{AudioFormatInfo, ChannelLayout, DecodeError, DecodeInfo, DecodedChunk};

/// TTA1 signature.
pub(crate) const TTA1_MAGIC: &[u8; 4] = b"TTA1";
/// Header size in bytes: magic + format + channels + bits + rate + length + crc32.
pub(crate) const HEADER_SIZE: usize = 22;
/// Reference demuxer rejects sample rates above 1 MHz as nonsense; mirror it
/// so a hostile header cannot drive an absurd `frame_length` allocation.
pub(crate) const MAX_SAMPLE_RATE: u32 = 1_000_000;
/// Upper bound on the per-frame interleaved scratch (samples across all
/// channels). Real content stays orders of magnitude below this; the cap
/// turns a hostile header into a clean error instead of a large allocation.
pub(crate) const MAX_FRAME_SAMPLES: usize = 8 << 20;
/// Upper bound on one frame's compressed byte size (including its CRC
/// trailer). Real hi-res frames are a few MB; the cap keeps a hostile size
/// table from driving a huge single allocation.
pub(crate) const MAX_FRAME_BYTES: usize = 64 << 20;

/// Full per-channel state, reset at every frame boundary.
pub(crate) struct ChannelState {
    pub(crate) predictor: i32,
    pub(crate) filter: TtaFilter,
    pub(crate) rice: TtaRice,
}

impl ChannelState {
    pub(crate) fn new(bits_per_sample: u16) -> Self {
        let shift = match bits_per_sample.div_ceil(8) {
            1 => 10,
            2 => 9,
            _ => 10,
        };
        Self {
            predictor: 0,
            filter: TtaFilter {
                qm: [0; 8],
                dx: [0; 8],
                dl: [0; 8],
                error: 0,
                shift,
                round: 0,
            },
            rice: TtaRice {
                k0: 10,
                k1: 10,
                sum0: 0,
                sum1: 0,
            },
        }
    }

    pub(crate) fn reset(&mut self) {
        self.predictor = 0;
        self.filter.init(self.filter.shift);
        self.rice.init();
    }
}

pub(crate) struct TtaHeader {
    pub(crate) channels: u16,
    pub(crate) bits_per_sample: u16,
    pub(crate) sample_rate: u32,
    /// Total samples per channel.
    pub(crate) data_length: u32,
}

impl TtaHeader {
    pub(crate) fn frame_length(&self) -> u32 {
        256 * self.sample_rate / 245
    }

    /// Samples in the final frame (full length when the data divides evenly).
    pub(crate) fn last_frame_length(&self) -> u32 {
        let rem = self.data_length % self.frame_length();
        if rem == 0 {
            self.frame_length()
        } else {
            rem
        }
    }

    pub(crate) fn total_frames(&self) -> u32 {
        self.data_length / self.frame_length()
            + u32::from(self.last_frame_length() < self.frame_length())
    }
}

/// Skip a leading ID3v2 tag if present (real-world `.tta` files sometimes
/// carry one before the TTA1 header). Returns the offset of the TTA data.
pub(crate) fn skip_id3v2(reader: &mut BufReader<File>) -> std::io::Result<u64> {
    let mut probe = [0u8; 10];
    let start = reader.stream_position()?;
    reader.read_exact(&mut probe)?;
    if &probe[..3] != b"ID3" {
        reader.seek(SeekFrom::Start(start))?;
        return Ok(start);
    }
    // Syncsafe integer: 4 × 7-bit groups.
    let size = ((probe[6] as u64) << 21)
        | ((probe[7] as u64) << 14)
        | ((probe[8] as u64) << 7)
        | (probe[9] as u64);
    let target = start + 10 + size;
    reader.seek(SeekFrom::Start(target))?;
    Ok(target)
}

pub(crate) fn parse_header(reader: &mut BufReader<File>) -> Result<TtaHeader, DecodeError> {
    skip_id3v2(reader).map_err(DecodeError::Io)?;

    let mut header = [0u8; HEADER_SIZE];
    reader
        .read_exact(&mut header)
        .map_err(|e| DecodeError::FileOpen(format!("truncated TTA header: {e}")))?;

    if &header[..4] != TTA1_MAGIC {
        return Err(DecodeError::UnsupportedFormat(
            "not a TTA file (missing TTA1 signature)".into(),
        ));
    }

    // Header integrity: CRC-32 over the first 18 bytes.
    let stored_crc = u32::from_le_bytes(header[18..22].try_into().unwrap());
    if crc32(&header[..18]) != stored_crc {
        return Err(DecodeError::UnsupportedFormat(
            "TTA header CRC mismatch".into(),
        ));
    }

    let format = u16::from_le_bytes(header[4..6].try_into().unwrap());
    let channels = u16::from_le_bytes(header[6..8].try_into().unwrap());
    let bits_per_sample = u16::from_le_bytes(header[8..10].try_into().unwrap());
    let sample_rate = u32::from_le_bytes(header[10..14].try_into().unwrap());
    let data_length = u32::from_le_bytes(header[14..18].try_into().unwrap());

    if format != 1 {
        if format == 2 {
            return Err(DecodeError::UnsupportedFormat(
                "encrypted TTA (format 2) is not supported".into(),
            ));
        }
        return Err(DecodeError::UnsupportedFormat(format!(
            "unsupported TTA format version {format}"
        )));
    }
    if channels == 0 || channels > 16 {
        return Err(DecodeError::UnsupportedFormat(format!(
            "invalid TTA channel count {channels}"
        )));
    }
    let bytes_per_sample = bits_per_sample.div_ceil(8);
    if !(1..=3).contains(&bytes_per_sample) {
        return Err(DecodeError::UnsupportedFormat(format!(
            "unsupported TTA bit depth {bits_per_sample} (expected 8/16/24)"
        )));
    }
    if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
        return Err(DecodeError::UnsupportedFormat(format!(
            "invalid TTA sample rate {sample_rate}"
        )));
    }
    if data_length == 0 {
        return Err(DecodeError::UnsupportedFormat(
            "TTA data length is zero".into(),
        ));
    }

    let frame_samples = 256u64 * sample_rate as u64 / 245;
    if frame_samples * channels as u64 > MAX_FRAME_SAMPLES as u64 {
        return Err(DecodeError::UnsupportedFormat(format!(
            "TTA frame too large ({frame_samples} samples × {channels} channels)"
        )));
    }

    Ok(TtaHeader {
        channels,
        bits_per_sample,
        sample_rate,
        data_length,
    })
}

/// Read the frame-size table that follows the header: `total_frames` little-
/// endian sizes (each including the frame's own 4-byte CRC trailer) plus a
/// CRC-32 over the table itself.
pub(crate) fn parse_size_table(
    reader: &mut BufReader<File>,
    header: &TtaHeader,
    data_start: u64,
) -> Result<Vec<u32>, DecodeError> {
    let total_frames = header.total_frames();

    // Structural bound: the table plus its CRC must physically fit between
    // the header and the end of file. This caps `total_frames` by the actual
    // file size instead of trusting the header.
    let file_len = reader.seek(SeekFrom::End(0)).map_err(DecodeError::Io)?;
    reader
        .seek(SeekFrom::Start(data_start))
        .map_err(DecodeError::Io)?;
    let table_bytes = 4u64 * total_frames as u64 + 4;
    if data_start + table_bytes > file_len {
        return Err(DecodeError::UnsupportedFormat(
            "TTA size table exceeds file size (truncated file?)".into(),
        ));
    }

    let mut raw = vec![0u8; table_bytes as usize];
    reader.read_exact(&mut raw).map_err(DecodeError::Io)?;

    let expected_crc = u32::from_le_bytes(raw[raw.len() - 4..].try_into().unwrap());
    if crc32(&raw[..raw.len() - 4]) != expected_crc {
        return Err(DecodeError::UnsupportedFormat(
            "TTA size table CRC mismatch".into(),
        ));
    }

    let sizes = raw[..raw.len() - 4]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect::<Vec<u32>>();
    Ok(sizes)
}

/// Decode one frame payload (excluding its 4-byte CRC trailer) into
/// interleaved `output` (length `frames * channels`). Channel states are
/// reset here, exactly as the reference does at each frame start.
pub(crate) fn decode_frame_payload(
    states: &mut [ChannelState],
    payload: &[u8],
    frames: usize,
    channels: usize,
    bytes_per_sample: usize,
    output: &mut [i32],
) -> Result<(), DecodeError> {
    let mut br = BitReaderLsb::new(payload);
    for state in states.iter_mut() {
        state.reset();
    }
    let mut cur_chan = 0usize;
    for slot in 0..frames {
        for s in 0..channels {
            let slot_base = slot * channels;
            let p_index = slot_base + s;

            // ── Rice decode + parameter adaptation ───────────────────
            let value = states[cur_chan].rice.decode(&mut br)?;

            // ── Zigzag → hybrid filter → fixed prediction ────────────
            let mut value = zigzag_decode(value);
            value = states[cur_chan].filter.process(value);
            let k_pred = if bytes_per_sample == 1 { 4 } else { 5 };
            value = value.wrapping_add(pred(states[cur_chan].predictor, k_pred));
            states[cur_chan].predictor = value;
            output[p_index] = value;

            // ── Channel decorrelation at slot boundaries ─────────────
            if cur_chan < channels - 1 {
                cur_chan += 1;
            } else {
                if channels > 1 {
                    // Mirror the reference pointer walk exactly:
                    //   last += prev / 2   (truncating division)
                    //   then backward wrapping differences to channel 0.
                    let last = output[p_index];
                    let prev = output[p_index - 1];
                    output[p_index] = last.wrapping_add(prev / 2);
                    for j in (0..channels - 1).rev() {
                        let idx = slot_base + j;
                        output[idx] = output[idx + 1].wrapping_sub(output[idx]);
                    }
                }
                cur_chan = 0;
            }
        }
    }
    Ok(())
}

/// TTA (True Audio) source implementing the engine's decoder interface.
pub struct TtaDecoder {
    reader: BufReader<File>,
    header: TtaHeader,
    /// Compressed size (including CRC trailer) of each frame.
    frame_sizes: Vec<u32>,
    /// Prefix sums: absolute file offset of each frame's payload start.
    frame_offsets: Vec<u64>,
    /// Index of the next frame to decode.
    current_frame: usize,
    /// Intra-frame samples to discard after decoding the current frame
    /// (set by `seek` for sample-accurate positioning).
    skip_samples: usize,
    states: Vec<ChannelState>,
    /// Decoded but not yet handed-out interleaved f32 samples.
    pending: Vec<f32>,
    pending_pos: usize,
    info: DecodeInfo,
    format_info: AudioFormatInfo,
}

impl TtaDecoder {
    /// Open a `.tta` file for playback.
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path)
            .map_err(|e| DecodeError::FileOpen(format!("Cannot open {}: {}", path.display(), e)))?;
        let mut reader = BufReader::new(file);

        let header = parse_header(&mut reader)?;
        let data_start = reader.stream_position().map_err(DecodeError::Io)?;
        let frame_sizes = parse_size_table(&mut reader, &header, data_start)?;

        // Frame offsets via prefix sums (each size includes the 4-byte CRC).
        let mut frame_offsets = Vec::with_capacity(frame_sizes.len());
        let mut offset = data_start + 4 * frame_sizes.len() as u64 + 4;
        for &size in &frame_sizes {
            frame_offsets.push(offset);
            offset += size as u64;
        }

        let channels = header.channels as usize;
        let duration_secs = header.data_length as f64 / header.sample_rate as f64;
        let avg_kbps = {
            let total_bytes: u64 = frame_sizes.iter().map(|&s| s as u64).sum();
            let secs = duration_secs.max(1e-9);
            ((total_bytes * 8) as f64 / secs / 1000.0).round() as u32
        };

        let info = DecodeInfo {
            sample_rate: header.sample_rate,
            channels,
            duration_secs: duration_secs as f32,
            codec: "TTA".to_string(),
            bitrate_kbps: (avg_kbps > 0).then_some(avg_kbps),
        };

        let format_info = AudioFormatInfo {
            codec: "TTA".to_string(),
            container: "True Audio (TTA)".to_string(),
            sample_rate: header.sample_rate,
            input_sample_rate: None,
            channels,
            channel_layout: ChannelLayout::from_count(channels),
            bit_depth: Some(header.bits_per_sample as u32),
            sample_format: format!("i{}", header.bits_per_sample),
            duration_secs: Some(duration_secs),
            bitrate_kbps: (avg_kbps > 0).then_some(avg_kbps),
            gapless: None,
            replaygain_track_db: None,
            replaygain_album_db: None,
            ebu_r128_loudness: None,
            true_peak_dbtp: None,
            is_lossless: true,
            is_dsd: false,
        };

        let states = (0..channels)
            .map(|_| ChannelState::new(header.bits_per_sample))
            .collect();

        Ok(Self {
            reader,
            header,
            frame_sizes,
            frame_offsets,
            current_frame: 0,
            skip_samples: 0,
            states,
            pending: Vec::with_capacity(4096 * channels),
            pending_pos: 0,
            info,
            format_info,
        })
    }

    /// Decode the next chunk of up to `max_frames` interleaved frames.
    pub fn decode_next(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        let max_frames = max_frames.max(1);
        let channels = self.header.channels as usize;
        let bytes_per_sample = self.header.bits_per_sample.div_ceil(8) as usize;

        while self.pending.len() - self.pending_pos < max_frames * channels {
            if self.current_frame >= self.frame_sizes.len() {
                break;
            }
            let frame_idx = self.current_frame;
            let size = self.frame_sizes[frame_idx] as usize;
            if size < 4 {
                self.current_frame += 1;
                return Err(DecodeError::Decode(format!(
                    "corrupt TTA frame {frame_idx} (size {size} smaller than CRC)"
                )));
            }
            if size > MAX_FRAME_BYTES {
                self.current_frame += 1;
                return Err(DecodeError::Decode(format!(
                    "corrupt TTA frame {frame_idx} (size {size} exceeds limit)"
                )));
            }

            self.reader
                .seek(SeekFrom::Start(self.frame_offsets[frame_idx]))
                .map_err(DecodeError::Io)?;
            let mut raw = vec![0u8; size];
            self.reader.read_exact(&mut raw).map_err(|e| {
                DecodeError::Decode(format!("truncated TTA frame {frame_idx}: {e}"))
            })?;

            // Frame integrity: CRC-32 over the payload, trailer excluded.
            let (payload, crc_bytes) = raw.split_at(size - 4);
            let stored_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());
            if crc32(payload) != stored_crc {
                self.current_frame += 1;
                return Err(DecodeError::Decode(format!(
                    "TTA frame {frame_idx} CRC mismatch"
                )));
            }

            let is_last = frame_idx + 1 == self.frame_sizes.len();
            let frames = if is_last {
                self.header.last_frame_length() as usize
            } else {
                self.header.frame_length() as usize
            };

            let mut decoded = vec![0i32; frames * channels];
            decode_frame_payload(
                &mut self.states,
                payload,
                frames,
                channels,
                bytes_per_sample,
                &mut decoded,
            )?;

            // Normalise to the engine's f32 convention, then apply any
            // intra-frame seek skip.
            let mut normalised: Vec<f32> = Vec::with_capacity(decoded.len());
            for &v in &decoded {
                let sample = match bytes_per_sample {
                    // 8-bit TTA stores signed values; the engine's unsigned
                    // convention adds 128, so normalise directly around zero.
                    1 => v as f32 / 128.0,
                    2 => (v as i16) as f32 / 32768.0,
                    _ => v as f32 / 8_388_608.0,
                };
                normalised.push(sample);
            }
            if self.skip_samples > 0 {
                let drop_frames = self.skip_samples.min(frames);
                let drop = drop_frames * channels;
                normalised.drain(..drop);
                self.skip_samples -= drop_frames;
            }
            self.pending.extend_from_slice(&normalised);
            self.current_frame += 1;
        }

        let available = self.pending.len() - self.pending_pos;
        if available == 0 {
            return Err(DecodeError::EndOfStream);
        }
        let take = (available.min(max_frames * channels) / channels) * channels;
        let samples: Vec<f32> = self.pending[self.pending_pos..self.pending_pos + take].to_vec();
        self.pending_pos += take;
        if self.pending_pos >= self.pending.len() {
            self.pending.clear();
            self.pending_pos = 0;
        }

        let frame_count = samples.len() / channels;
        Ok(DecodedChunk {
            samples,
            channels,
            channel_layout: self.format_info.channel_layout.clone(),
            sample_rate: self.info.sample_rate,
            frame_count,
            raw_dsd: None,
        })
    }

    /// Seek to a position in seconds using the frame-size index (exact frame
    /// granularity, then intra-frame sample skipping).
    pub fn seek(&mut self, position_secs: f32) -> Result<(), DecodeError> {
        if !position_secs.is_finite() || position_secs < 0.0 {
            return Err(DecodeError::Seek(format!(
                "Invalid seek position: {position_secs}"
            )));
        }
        let target_sample = ((position_secs as f64) * self.header.sample_rate as f64)
            .round()
            .max(0.0) as u64;
        let target_sample = target_sample.min(self.header.data_length as u64);

        let frame_len = self.header.frame_length() as u64;
        let frame_idx = (target_sample / frame_len) as usize;
        let skip = (target_sample % frame_len) as usize;

        if frame_idx >= self.frame_sizes.len() {
            // Past the end: clamp to EOF.
            self.current_frame = self.frame_sizes.len();
            self.pending.clear();
            self.pending_pos = 0;
            return Ok(());
        }

        self.current_frame = frame_idx;
        self.skip_samples = skip;
        self.pending.clear();
        self.pending_pos = 0;
        Ok(())
    }

    pub fn info(&self) -> &DecodeInfo {
        &self.info
    }

    pub fn duration_secs(&self) -> f32 {
        self.info.duration_secs
    }

    pub fn format_info(&self) -> &AudioFormatInfo {
        &self.format_info
    }
}
