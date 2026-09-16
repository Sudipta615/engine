//! BWF (EBU Tech 3285) & BW64 (ITU-R BS.2088) container interoperability (§4.2, Item 24).
//!
//! Provides parsing, construction, and serialization for professional broadcast wave
//! and 64-bit multi-track containers carrying ADM immersive metadata:
//! - `bext` (Broadcast Audio Extension: loudness, true peak, timecode, originator, UMID)
//! - `chna` (Channel allocation chunk linking audio track indices to ADM UIDs)
//! - `axml` (ADM XML metadata payload)
//! - `iXML` (Production / location audio XML metadata)
//! - `ds64` (64-bit RF64/BW64 data size descriptors)

use std::io::{Read, Seek, SeekFrom, Write};
use thiserror::Error;

use super::adm::{parse_adm_xml, to_adm_xml, AdmError};
use crate::standards::adm::AdmDocument;

pub const RIFF_ID: [u8; 4] = *b"RIFF";
pub const BW64_ID: [u8; 4] = *b"BW64";
pub const RF64_ID: [u8; 4] = *b"RF64";
pub const WAVE_ID: [u8; 4] = *b"WAVE";
pub const DS64_ID: [u8; 4] = *b"ds64";
pub const FMT_ID: [u8; 4] = *b"fmt ";
pub const DATA_ID: [u8; 4] = *b"data";
pub const BEXT_ID: [u8; 4] = *b"bext";
pub const CHNA_ID: [u8; 4] = *b"chna";
pub const AXML_ID: [u8; 4] = *b"axml";
pub const IXML_ID: [u8; 4] = *b"iXML";

/// Errors encountered during BWF/BW64 processing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Bw64Error {
    #[error("I/O error during BWF/BW64 read/write: {0}")]
    Io(String),
    #[error("invalid RIFF/BW64 header format")]
    InvalidHeader,
    #[error("truncated or corrupted chunk: {0}")]
    CorruptChunk(String),
    #[error("unsupported audio encoding in fmt chunk: {0}")]
    UnsupportedFormat(u16),
    #[error("ADM metadata error: {0}")]
    Adm(String),
}

impl From<std::io::Error> for Bw64Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<AdmError> for Bw64Error {
    fn from(e: AdmError) -> Self {
        Self::Adm(e.to_string())
    }
}

/// Container format type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bw64ContainerType {
    StandardRiff,
    Bw64,
    Rf64,
}

/// Broadcast Audio Extension (`bext`) chunk per EBU Tech 3285.
#[derive(Debug, Clone, PartialEq)]
pub struct BextChunk {
    pub description: String,
    pub originator: String,
    pub originator_reference: String,
    pub origination_date: String,
    pub origination_time: String,
    pub time_reference: u64,
    pub version: u16,
    pub umid: [u8; 64],
    pub loudness_value_lufs: Option<f32>,
    pub loudness_range_lu: Option<f32>,
    pub max_true_peak_dbtp: Option<f32>,
    pub max_momentary_lufs: Option<f32>,
    pub max_short_term_lufs: Option<f32>,
    pub coding_history: String,
}

impl Default for BextChunk {
    fn default() -> Self {
        Self {
            description: String::new(),
            originator: "Shadow Engine".to_string(),
            originator_reference: String::new(),
            origination_date: "2026-09-15".to_string(),
            origination_time: "12:00:00".to_string(),
            time_reference: 0,
            version: 2,
            umid: [0u8; 64],
            loudness_value_lufs: None,
            loudness_range_lu: None,
            max_true_peak_dbtp: None,
            max_momentary_lufs: None,
            max_short_term_lufs: None,
            coding_history: String::new(),
        }
    }
}

impl BextChunk {
    /// Parse raw `bext` chunk payload.
    pub fn parse(data: &[u8]) -> Result<Self, Bw64Error> {
        if data.len() < 256 + 32 + 32 + 10 + 8 + 8 + 2 + 64 {
            return Err(Bw64Error::CorruptChunk(
                "bext chunk is too small".to_string(),
            ));
        }

        let description = read_fixed_ascii(&data[0..256]);
        let originator = read_fixed_ascii(&data[256..288]);
        let originator_reference = read_fixed_ascii(&data[288..320]);
        let origination_date = read_fixed_ascii(&data[320..330]);
        let origination_time = read_fixed_ascii(&data[330..338]);

        let time_ref_low = u32::from_le_bytes(data[338..342].try_into().unwrap()) as u64;
        let time_ref_high = u32::from_le_bytes(data[342..346].try_into().unwrap()) as u64;
        let time_reference = (time_ref_high << 32) | time_ref_low;

        let version = u16::from_le_bytes(data[346..348].try_into().unwrap());
        let mut umid = [0u8; 64];
        umid.copy_from_slice(&data[348..412]);

        let mut loudness_value_lufs = None;
        let mut loudness_range_lu = None;
        let mut max_true_peak_dbtp = None;
        let mut max_momentary_lufs = None;
        let mut max_short_term_lufs = None;

        if version >= 1 && data.len() >= 412 + 10 {
            let lv = i16::from_le_bytes(data[412..414].try_into().unwrap());
            let lr = i16::from_le_bytes(data[414..416].try_into().unwrap());
            let tp = i16::from_le_bytes(data[416..418].try_into().unwrap());
            loudness_value_lufs = Some(lv as f32 / 100.0);
            loudness_range_lu = Some(lr as f32 / 100.0);
            max_true_peak_dbtp = Some(tp as f32 / 100.0);

            if version >= 2 && data.len() >= 412 + 14 {
                let mm = i16::from_le_bytes(data[418..420].try_into().unwrap());
                let ms = i16::from_le_bytes(data[420..422].try_into().unwrap());
                max_momentary_lufs = Some(mm as f32 / 100.0);
                max_short_term_lufs = Some(ms as f32 / 100.0);
            }
        }

        let coding_history_offset = if version >= 1 { 412 + 190 } else { 412 };
        let coding_history = if data.len() > coding_history_offset {
            read_fixed_ascii(&data[coding_history_offset..])
        } else {
            String::new()
        };

        Ok(Self {
            description,
            originator,
            originator_reference,
            origination_date,
            origination_time,
            time_reference,
            version,
            umid,
            loudness_value_lufs,
            loudness_range_lu,
            max_true_peak_dbtp,
            max_momentary_lufs,
            max_short_term_lufs,
            coding_history,
        })
    }

    /// Serialize `bext` chunk into bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0u8; 412 + 190];
        write_fixed_ascii(&mut bytes[0..256], &self.description);
        write_fixed_ascii(&mut bytes[256..288], &self.originator);
        write_fixed_ascii(&mut bytes[288..320], &self.originator_reference);
        write_fixed_ascii(&mut bytes[320..330], &self.origination_date);
        write_fixed_ascii(&mut bytes[330..338], &self.origination_time);

        let time_low = (self.time_reference & 0xFFFF_FFFF) as u32;
        let time_high = (self.time_reference >> 32) as u32;
        bytes[338..342].copy_from_slice(&time_low.to_le_bytes());
        bytes[342..346].copy_from_slice(&time_high.to_le_bytes());

        bytes[346..348].copy_from_slice(&self.version.to_le_bytes());
        bytes[348..412].copy_from_slice(&self.umid);

        if let Some(lv) = self.loudness_value_lufs {
            let val = (lv * 100.0).round() as i16;
            bytes[412..414].copy_from_slice(&val.to_le_bytes());
        }
        if let Some(lr) = self.loudness_range_lu {
            let val = (lr * 100.0).round() as i16;
            bytes[414..416].copy_from_slice(&val.to_le_bytes());
        }
        if let Some(tp) = self.max_true_peak_dbtp {
            let val = (tp * 100.0).round() as i16;
            bytes[416..418].copy_from_slice(&val.to_le_bytes());
        }
        if let Some(mm) = self.max_momentary_lufs {
            let val = (mm * 100.0).round() as i16;
            bytes[418..420].copy_from_slice(&val.to_le_bytes());
        }
        if let Some(ms) = self.max_short_term_lufs {
            let val = (ms * 100.0).round() as i16;
            bytes[420..422].copy_from_slice(&val.to_le_bytes());
        }

        if !self.coding_history.is_empty() {
            bytes.extend_from_slice(self.coding_history.as_bytes());
        }

        bytes
    }
}

/// Single channel allocation entry in a `chna` chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChnaTrackUid {
    pub track_index: u16,
    pub uid: String,
    pub track_format_id: String,
    pub pack_format_id: String,
}

/// Channel allocation chunk (`chna`) per ITU-R BS.2088 §3.3.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChnaChunk {
    pub entries: Vec<ChnaTrackUid>,
}

impl ChnaChunk {
    pub fn parse(data: &[u8]) -> Result<Self, Bw64Error> {
        if data.len() < 4 {
            return Err(Bw64Error::CorruptChunk("chna chunk too small".to_string()));
        }
        let _num_tracks = u16::from_le_bytes(data[0..2].try_into().unwrap());
        let num_uids = u16::from_le_bytes(data[2..4].try_into().unwrap());

        let mut entries = Vec::with_capacity(num_uids as usize);
        let mut offset = 4;

        for _ in 0..num_uids {
            if offset + 40 > data.len() {
                break;
            }
            let track_index = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
            let uid = read_fixed_ascii(&data[offset + 2..offset + 14]);
            let track_format_id = read_fixed_ascii(&data[offset + 14..offset + 28]);
            let pack_format_id = read_fixed_ascii(&data[offset + 28..offset + 39]);

            entries.push(ChnaTrackUid {
                track_index,
                uid,
                track_format_id,
                pack_format_id,
            });
            offset += 40;
        }

        Ok(Self { entries })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let num_tracks = self
            .entries
            .iter()
            .map(|e| e.track_index)
            .max()
            .unwrap_or(0);
        let num_uids = self.entries.len() as u16;

        let mut bytes = Vec::with_capacity(4 + self.entries.len() * 40);
        bytes.extend_from_slice(&num_tracks.to_le_bytes());
        bytes.extend_from_slice(&num_uids.to_le_bytes());

        for entry in &self.entries {
            bytes.extend_from_slice(&entry.track_index.to_le_bytes());
            let mut uid_buf = [0u8; 12];
            write_fixed_ascii(&mut uid_buf, &entry.uid);
            bytes.extend_from_slice(&uid_buf);

            let mut tf_buf = [0u8; 14];
            write_fixed_ascii(&mut tf_buf, &entry.track_format_id);
            bytes.extend_from_slice(&tf_buf);

            let mut pf_buf = [0u8; 11];
            write_fixed_ascii(&mut pf_buf, &entry.pack_format_id);
            bytes.extend_from_slice(&pf_buf);

            bytes.push(0); // padding byte to 40 bytes
        }

        bytes
    }
}

/// Metadata payload bundled inside a BWF/BW64 container.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Bw64Metadata {
    pub bext: Option<BextChunk>,
    pub chna: Option<ChnaChunk>,
    pub axml: Option<String>,
    pub ixml: Option<String>,
}

impl Bw64Metadata {
    /// Extract parsed [`AdmDocument`] from `axml` if present.
    pub fn parse_adm(&self) -> Result<Option<AdmDocument>, Bw64Error> {
        if let Some(ref xml) = self.axml {
            let doc = parse_adm_xml(xml)?;
            Ok(Some(doc))
        } else {
            Ok(None)
        }
    }

    /// Embed an [`AdmDocument`] into this metadata payload as `axml`.
    pub fn set_adm(&mut self, doc: &AdmDocument) {
        let xml = to_adm_xml(doc);
        self.axml = Some(xml);
    }
}

/// Parsed BWF/BW64 audio container file representation.
#[derive(Debug, Clone, PartialEq)]
pub struct Bw64File {
    pub container_type: Bw64ContainerType,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub metadata: Bw64Metadata,
    pub audio_data_len: u64,
}

impl Bw64File {
    /// Parse container headers and metadata chunks from a reader.
    pub fn parse<R: Read + Seek>(reader: &mut R) -> Result<Self, Bw64Error> {
        let mut header = [0u8; 12];
        reader.read_exact(&mut header)?;

        let container_type = match &header[0..4] {
            h if h == RIFF_ID => Bw64ContainerType::StandardRiff,
            h if h == BW64_ID => Bw64ContainerType::Bw64,
            h if h == RF64_ID => Bw64ContainerType::Rf64,
            _ => return Err(Bw64Error::InvalidHeader),
        };

        if header[8..12] != WAVE_ID {
            return Err(Bw64Error::InvalidHeader);
        }

        let mut channels = 2u16;
        let mut sample_rate = 48000u32;
        let mut bits_per_sample = 24u16;
        let mut audio_data_len = 0u64;

        let mut metadata = Bw64Metadata::default();

        let mut chunk_hdr = [0u8; 8];
        while reader.read_exact(&mut chunk_hdr).is_ok() {
            let chunk_id = &chunk_hdr[0..4];
            let chunk_size = u32::from_le_bytes(chunk_hdr[4..8].try_into().unwrap()) as u64;

            match chunk_id {
                c if c == FMT_ID => {
                    let mut fmt_buf = vec![0u8; chunk_size as usize];
                    reader.read_exact(&mut fmt_buf)?;
                    if fmt_buf.len() >= 16 {
                        channels = u16::from_le_bytes(fmt_buf[2..4].try_into().unwrap());
                        sample_rate = u32::from_le_bytes(fmt_buf[4..8].try_into().unwrap());
                        bits_per_sample = u16::from_le_bytes(fmt_buf[14..16].try_into().unwrap());
                    }
                }
                c if c == BEXT_ID => {
                    let mut buf = vec![0u8; chunk_size as usize];
                    reader.read_exact(&mut buf)?;
                    metadata.bext = Some(BextChunk::parse(&buf)?);
                }
                c if c == CHNA_ID => {
                    let mut buf = vec![0u8; chunk_size as usize];
                    reader.read_exact(&mut buf)?;
                    metadata.chna = Some(ChnaChunk::parse(&buf)?);
                }
                c if c == AXML_ID => {
                    let mut buf = vec![0u8; chunk_size as usize];
                    reader.read_exact(&mut buf)?;
                    metadata.axml = Some(String::from_utf8_lossy(&buf).to_string());
                }
                c if c == IXML_ID => {
                    let mut buf = vec![0u8; chunk_size as usize];
                    reader.read_exact(&mut buf)?;
                    metadata.ixml = Some(String::from_utf8_lossy(&buf).to_string());
                }
                c if c == DATA_ID => {
                    audio_data_len = chunk_size;
                    reader.seek(SeekFrom::Current(chunk_size as i64))?;
                }
                _ => {
                    reader.seek(SeekFrom::Current(chunk_size as i64))?;
                }
            }

            // Word alignment (pad byte if odd)
            if !chunk_size.is_multiple_of(2) {
                let _ = reader.seek(SeekFrom::Current(1));
            }
        }

        Ok(Self {
            container_type,
            channels,
            sample_rate,
            bits_per_sample,
            metadata,
            audio_data_len,
        })
    }

    /// Write container file including metadata chunks and audio payload.
    pub fn write_to<W: Write>(&self, writer: &mut W, audio_data: &[u8]) -> Result<(), Bw64Error> {
        let is_bw64 = self.container_type == Bw64ContainerType::Bw64;
        let magic = if is_bw64 { BW64_ID } else { RIFF_ID };

        // Precompute chunks
        let mut chunks = Vec::new();

        // 1. fmt chunk
        let mut fmt_bytes = vec![0u8; 16];
        fmt_bytes[0..2].copy_from_slice(&1u16.to_le_bytes()); // PCM
        fmt_bytes[2..4].copy_from_slice(&self.channels.to_le_bytes());
        fmt_bytes[4..8].copy_from_slice(&self.sample_rate.to_le_bytes());
        let byte_rate =
            self.sample_rate * (self.channels as u32) * (self.bits_per_sample as u32 / 8);
        fmt_bytes[8..12].copy_from_slice(&byte_rate.to_le_bytes());
        let block_align = self.channels * (self.bits_per_sample / 8);
        fmt_bytes[12..14].copy_from_slice(&block_align.to_le_bytes());
        fmt_bytes[14..16].copy_from_slice(&self.bits_per_sample.to_le_bytes());
        chunks.push((FMT_ID, fmt_bytes));

        // 2. bext chunk
        if let Some(ref bext) = self.metadata.bext {
            chunks.push((BEXT_ID, bext.to_bytes()));
        }

        // 3. chna chunk
        if let Some(ref chna) = self.metadata.chna {
            chunks.push((CHNA_ID, chna.to_bytes()));
        }

        // 4. axml chunk
        if let Some(ref axml) = self.metadata.axml {
            chunks.push((AXML_ID, axml.as_bytes().to_vec()));
        }

        // 5. ixml chunk
        if let Some(ref ixml) = self.metadata.ixml {
            chunks.push((IXML_ID, ixml.as_bytes().to_vec()));
        }

        // 6. data chunk
        chunks.push((DATA_ID, audio_data.to_vec()));

        // Calculate total size
        let mut total_body_size = 4u32; // "WAVE"
        for (_, data) in &chunks {
            total_body_size += 8 + data.len() as u32 + (data.len() as u32 % 2);
        }

        // Write container header
        writer.write_all(&magic)?;
        writer.write_all(&total_body_size.to_le_bytes())?;
        writer.write_all(&WAVE_ID)?;

        // Write chunks
        for (id, data) in chunks {
            writer.write_all(&id)?;
            writer.write_all(&(data.len() as u32).to_le_bytes())?;
            writer.write_all(&data)?;
            if data.len() % 2 != 0 {
                writer.write_all(&[0])?;
            }
        }

        Ok(())
    }
}

fn read_fixed_ascii(slice: &[u8]) -> String {
    let len = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..len]).trim().to_string()
}

fn write_fixed_ascii(dst: &mut [u8], src: &str) {
    let bytes = src.as_bytes();
    let n = dst.len().min(bytes.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    if n < dst.len() {
        dst[n..].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bw64_container_write_and_parse_roundtrip() {
        let metadata = Bw64Metadata {
            bext: Some(BextChunk {
                description: "Surround Test Mix".to_string(),
                originator: "Shadow Test".to_string(),
                loudness_value_lufs: Some(-23.0),
                loudness_range_lu: Some(8.5),
                max_true_peak_dbtp: Some(-1.0),
                ..Default::default()
            }),
            chna: Some(ChnaChunk {
                entries: vec![
                    ChnaTrackUid {
                        track_index: 1,
                        uid: "ATU_00000001".to_string(),
                        track_format_id: "AT_00010001_01".to_string(),
                        pack_format_id: "AP_00010001".to_string(),
                    },
                    ChnaTrackUid {
                        track_index: 2,
                        uid: "ATU_00000002".to_string(),
                        track_format_id: "AT_00010002_01".to_string(),
                        pack_format_id: "AP_00010001".to_string(),
                    },
                ],
            }),
            axml: Some("<ituBS2076><coreMetadata/></ituBS2076>".to_string()),
            ..Default::default()
        };

        let bw64 = Bw64File {
            container_type: Bw64ContainerType::Bw64,
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 24,
            metadata,
            audio_data_len: 12,
        };

        let dummy_audio = vec![0u8; 12];
        let mut cursor = Cursor::new(Vec::new());
        bw64.write_to(&mut cursor, &dummy_audio).unwrap();

        cursor.set_position(0);
        let parsed = Bw64File::parse(&mut cursor).unwrap();

        assert_eq!(parsed.container_type, Bw64ContainerType::Bw64);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.sample_rate, 48000);
        assert_eq!(parsed.bits_per_sample, 24);
        assert_eq!(parsed.audio_data_len, 12);

        let bext = parsed.metadata.bext.unwrap();
        assert_eq!(bext.description, "Surround Test Mix");
        assert_eq!(bext.loudness_value_lufs, Some(-23.0));
        assert_eq!(bext.max_true_peak_dbtp, Some(-1.0));

        let chna = parsed.metadata.chna.unwrap();
        assert_eq!(chna.entries.len(), 2);
        assert_eq!(chna.entries[0].uid, "ATU_00000001");
        assert_eq!(chna.entries[1].track_index, 2);

        assert!(parsed.metadata.axml.is_some());
    }
}
