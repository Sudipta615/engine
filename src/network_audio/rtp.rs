//! RFC 3550 Real-time Transport Protocol (RTP) packet handling (§10.4, Item 33).
//!
//! Provides RTP packet parsing, serialization, sequence roll-over handling,
//! timestamp arithmetic, and linear PCM (L16 / L24) payload codecs.

use serde::{Deserialize, Serialize};

/// Fixed size of the standard RFC 3550 RTP header without CSRC or extensions.
pub const RTP_HEADER_MIN_SIZE: usize = 12;

/// Standard RTP Version 2.
pub const RTP_VERSION: u8 = 2;

/// Structured error for RTP packet processing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RtpError {
    PacketTooShort { actual: usize, minimum: usize },
    InvalidVersion(u8),
    BufferTooSmall { required: usize, provided: usize },
    InvalidPayloadLength { expected: usize, actual: usize },
    UnsupportedPayloadType(u8),
    MalformedHeader(String),
}

impl std::fmt::Display for RtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PacketTooShort { actual, minimum } => {
                write!(
                    f,
                    "RTP packet too short: {actual} bytes, expected at least {minimum}"
                )
            }
            Self::InvalidVersion(v) => write!(f, "Unsupported RTP version: {v}, expected 2"),
            Self::BufferTooSmall { required, provided } => {
                write!(
                    f,
                    "Buffer too small: {provided} bytes provided, {required} required"
                )
            }
            Self::InvalidPayloadLength { expected, actual } => {
                write!(
                    f,
                    "Invalid payload length: expected {expected}, got {actual}"
                )
            }
            Self::UnsupportedPayloadType(pt) => write!(f, "Unsupported payload type: {pt}"),
            Self::MalformedHeader(msg) => write!(f, "Malformed RTP header: {msg}"),
        }
    }
}

impl std::error::Error for RtpError {}

/// Parsed RFC 3550 RTP header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RtpHeader {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub csrc_count: u8,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub csrc: Vec<u32>,
}

impl Default for RtpHeader {
    fn default() -> Self {
        Self {
            version: RTP_VERSION,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: false,
            payload_type: 96, // Dynamic payload type commonly used for AES67 L24
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0x12345678,
            csrc: Vec::new(),
        }
    }
}

impl RtpHeader {
    /// Serializes this header into the provided byte buffer. Returns written bytes count.
    pub fn serialize(&self, buf: &mut [u8]) -> Result<usize, RtpError> {
        let required = RTP_HEADER_MIN_SIZE + (self.csrc.len() * 4);
        if buf.len() < required {
            return Err(RtpError::BufferTooSmall {
                required,
                provided: buf.len(),
            });
        }

        let b0 = (self.version << 6)
            | ((self.padding as u8) << 5)
            | ((self.extension as u8) << 4)
            | (self.csrc_count.min(15) & 0x0F);
        let b1 = ((self.marker as u8) << 7) | (self.payload_type & 0x7F);

        buf[0] = b0;
        buf[1] = b1;
        buf[2..4].copy_from_slice(&self.sequence_number.to_be_bytes());
        buf[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());

        let mut offset = 12;
        for &csrc_val in self.csrc.iter().take(self.csrc_count as usize) {
            buf[offset..offset + 4].copy_from_slice(&csrc_val.to_be_bytes());
            offset += 4;
        }

        Ok(offset)
    }

    /// Parses an RTP header from raw bytes. Returns the header and offset to the payload.
    pub fn parse(bytes: &[u8]) -> Result<(Self, usize), RtpError> {
        if bytes.len() < RTP_HEADER_MIN_SIZE {
            return Err(RtpError::PacketTooShort {
                actual: bytes.len(),
                minimum: RTP_HEADER_MIN_SIZE,
            });
        }

        let b0 = bytes[0];
        let version = (b0 >> 6) & 0x03;
        if version != RTP_VERSION {
            return Err(RtpError::InvalidVersion(version));
        }

        let padding = ((b0 >> 5) & 0x01) != 0;
        let extension = ((b0 >> 4) & 0x01) != 0;
        let csrc_count = b0 & 0x0F;

        let b1 = bytes[1];
        let marker = ((b1 >> 7) & 0x01) != 0;
        let payload_type = b1 & 0x7F;

        let sequence_number = u16::from_be_bytes([bytes[2], bytes[3]]);
        let timestamp = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let ssrc = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);

        let header_len = RTP_HEADER_MIN_SIZE + (csrc_count as usize * 4);
        if bytes.len() < header_len {
            return Err(RtpError::PacketTooShort {
                actual: bytes.len(),
                minimum: header_len,
            });
        }

        let mut csrc = Vec::with_capacity(csrc_count as usize);
        let mut offset = 12;
        for _ in 0..csrc_count {
            let val = u32::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            csrc.push(val);
            offset += 4;
        }

        // Check if header extension is present
        if extension {
            if bytes.len() < offset + 4 {
                return Err(RtpError::PacketTooShort {
                    actual: bytes.len(),
                    minimum: offset + 4,
                });
            }
            let ext_len_words = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
            let ext_total_bytes = 4 + (ext_len_words * 4);
            if bytes.len() < offset + ext_total_bytes {
                return Err(RtpError::PacketTooShort {
                    actual: bytes.len(),
                    minimum: offset + ext_total_bytes,
                });
            }
            offset += ext_total_bytes;
        }

        let header = Self {
            version,
            padding,
            extension,
            csrc_count,
            marker,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            csrc,
        };

        Ok((header, offset))
    }
}

/// An RFC 3550 RTP packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RtpPacket {
    pub header: RtpHeader,
    pub payload: Vec<u8>,
}

impl RtpPacket {
    /// Creates a new RTP packet.
    pub fn new(header: RtpHeader, payload: Vec<u8>) -> Self {
        Self { header, payload }
    }

    /// Parses an RTP packet from raw bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, RtpError> {
        let (header, payload_start) = RtpHeader::parse(bytes)?;
        let mut payload_end = bytes.len();

        if header.padding {
            if payload_end <= payload_start {
                return Err(RtpError::MalformedHeader(
                    "Padding flag set on empty payload".into(),
                ));
            }
            let pad_len = bytes[payload_end - 1] as usize;
            if pad_len == 0 || pad_len > (payload_end - payload_start) {
                return Err(RtpError::MalformedHeader("Invalid padding length".into()));
            }
            payload_end -= pad_len;
        }

        let payload = bytes[payload_start..payload_end].to_vec();
        Ok(Self { header, payload })
    }

    /// Serializes the complete RTP packet into a byte vector.
    pub fn to_bytes(&self) -> Vec<u8> {
        let header_len = RTP_HEADER_MIN_SIZE + (self.header.csrc.len() * 4);
        let mut buf = vec![0u8; header_len + self.payload.len()];
        let _ = self.header.serialize(&mut buf[..header_len]);
        buf[header_len..].copy_from_slice(&self.payload);
        buf
    }
}

/// Calculates signed difference between two 16-bit RTP sequence numbers (handles rollover).
#[inline]
pub fn sequence_diff(a: u16, b: u16) -> i32 {
    (a as i16).wrapping_sub(b as i16) as i32
}

/// Calculates signed difference between two 32-bit RTP timestamps (handles rollover).
#[inline]
pub fn timestamp_diff(a: u32, b: u32) -> i64 {
    (a as i32).wrapping_sub(b as i32) as i64
}

/// Encoders and decoders for standard uncompressed Linear PCM audio payloads.
pub struct PcmPayloadCodec;

impl PcmPayloadCodec {
    /// Encodes multichannel planar float samples into big-endian signed 16-bit linear PCM (RFC 3551 L16).
    pub fn encode_l16(channels: &[&[f32]], num_frames: usize) -> Vec<u8> {
        let num_ch = channels.len();
        let mut out = vec![0u8; num_ch * num_frames * 2];
        let mut idx = 0;

        for frame in 0..num_frames {
            for ch_samples in channels.iter() {
                let sample = ch_samples.get(frame).copied().unwrap_or(0.0);
                let clamped = sample.clamp(-1.0, 1.0);
                let val = if clamped >= 0.0 {
                    (clamped * 32767.0) as i16
                } else {
                    (clamped * 32768.0) as i16
                };
                let bytes = val.to_be_bytes();
                out[idx] = bytes[0];
                out[idx + 1] = bytes[1];
                idx += 2;
            }
        }
        out
    }

    /// Decodes big-endian signed 16-bit linear PCM (RFC 3551 L16) payload into planar float channels.
    pub fn decode_l16(payload: &[u8], channels: &mut [&mut [f32]]) -> Result<usize, RtpError> {
        let num_ch = channels.len();
        if num_ch == 0 {
            return Ok(0);
        }
        let bytes_per_frame = num_ch * 2;
        let num_frames = payload.len() / bytes_per_frame;

        let mut idx = 0;
        for frame in 0..num_frames {
            for ch_buf in channels.iter_mut() {
                let val = i16::from_be_bytes([payload[idx], payload[idx + 1]]);
                let s = if val >= 0 {
                    val as f32 / 32767.0
                } else {
                    val as f32 / 32768.0
                };
                if frame < ch_buf.len() {
                    ch_buf[frame] = s;
                }
                idx += 2;
            }
        }
        Ok(num_frames)
    }

    /// Encodes multichannel planar float samples into big-endian signed 24-bit linear PCM (RFC 3190 / AES67 L24).
    pub fn encode_l24(channels: &[&[f32]], num_frames: usize) -> Vec<u8> {
        let num_ch = channels.len();
        let mut out = vec![0u8; num_ch * num_frames * 3];
        let mut idx = 0;

        for frame in 0..num_frames {
            for ch_samples in channels.iter() {
                let sample = ch_samples.get(frame).copied().unwrap_or(0.0);
                let clamped = sample.clamp(-1.0, 1.0);
                let val: i32 = if clamped >= 0.0 {
                    (clamped as f64 * 8388607.0) as i32
                } else {
                    (clamped as f64 * 8388608.0) as i32
                };
                let be = val.to_be_bytes(); // [b0, b1, b2, b3] where b1..b3 are 24-bit
                out[idx] = be[1];
                out[idx + 1] = be[2];
                out[idx + 2] = be[3];
                idx += 3;
            }
        }
        out
    }

    /// Decodes big-endian signed 24-bit linear PCM (RFC 3190 / AES67 L24) payload into planar float channels.
    pub fn decode_l24(payload: &[u8], channels: &mut [&mut [f32]]) -> Result<usize, RtpError> {
        let num_ch = channels.len();
        if num_ch == 0 {
            return Ok(0);
        }
        let bytes_per_frame = num_ch * 3;
        let num_frames = payload.len() / bytes_per_frame;

        let mut idx = 0;
        for frame in 0..num_frames {
            for ch_buf in channels.iter_mut() {
                let b0 = payload[idx];
                let b1 = payload[idx + 1];
                let b2 = payload[idx + 2];
                // Sign extend 24-bit to 32-bit
                let sign_extend = if (b0 & 0x80) != 0 { 0xFF } else { 0x00 };
                let val = i32::from_be_bytes([sign_extend, b0, b1, b2]);
                let s = if val >= 0 {
                    val as f32 / 8388607.0
                } else {
                    val as f32 / 8388608.0
                };
                if frame < ch_buf.len() {
                    ch_buf[frame] = s;
                }
                idx += 3;
            }
        }
        Ok(num_frames)
    }
}
