//! Ogg Opus decoder adapter (RFC 7845) — pure Rust (`ogg` demux + the
//! `opus-decoder` crate, RFC 8251, no unsafe / no FFI).
//!
//! The adapter presents an Opus source through the engine's unified
//! [`crate::decode::Decoder`] interface: `DecodeInfo` / `AudioFormatInfo`,
//! gapless handling (OpusHead pre-skip + final granule end-trim), tag
//! metadata (OpusTags / Vorbis comments), sample-accurate granule seeking,
//! and multichannel support (channel mapping families 0 and 1).
//!
//! # Sample-rate semantics
//!
//! Opus always decodes at 48 kHz; the OpusHead `input_sample_rate` field is
//! metadata only (the original recording rate) and is **not** the decode
//! rate. The engine therefore reports 48 kHz for every Opus source and lets
//! the resampler handle conversion to the output device rate — the same
//! convention used by other desktop players. The original rate is kept in
//! `AudioFormatInfo` for display.
//!
//! # Gapless
//!
//! Per RFC 7845 §4.5: the final page's granule position counts the pre-skip,
//! so `total_logical = final_granule − pre_skip`. The adapter discards the
//! first `pre_skip` decoded samples and trims the tail to the logical length,
//! exposing `GaplessInfo { encoder_delay: pre_skip, total_logical_frames }`.
//!
//! # Seeking
//!
//! Seeking uses the Ogg granule position (binary search over pages via
//! `PacketReader::seek_absgp`), then decodes and discards up to the target,
//! with a fresh decoder state at the new position (± one 20 ms packet).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, SeekFrom};
use std::path::Path;

use ogg::reading::{OggReadError, PacketReader};

use crate::decode::{AudioFormatInfo, ChannelLayout, DecodeError, DecodeInfo, GaplessInfo};

/// The Opus decode rate (Hz). Opus packets always decode to 48 kHz.
pub const OPUS_DECODE_RATE: u32 = 48_000;

const OPUS_HEAD_MAGIC: &[u8; 8] = b"OpusHead";
const OPUS_TAGS_MAGIC: &[u8; 8] = b"OpusTags";
/// Maximum decoded frames per channel (120 ms at 48 kHz).
const MAX_FRAME_SIZE: usize = 5760;

/// Parsed OpusHead (RFC 7845 §5.1).
#[derive(Debug, Clone)]
struct OpusHead {
    channels: usize,
    pre_skip: u64,
    input_sample_rate: u32,
    stream_count: usize,
    coupled_count: usize,
    mapping: Vec<u8>,
}

fn parse_opus_head(data: &[u8]) -> Result<OpusHead, DecodeError> {
    if data.len() < 19 || &data[..8] != OPUS_HEAD_MAGIC {
        return Err(DecodeError::UnsupportedFormat(
            "invalid OpusHead packet".to_string(),
        ));
    }
    let version = data[8];
    if version != 1 {
        return Err(DecodeError::UnsupportedFormat(format!(
            "unsupported Opus stream version {version}"
        )));
    }
    let channels = data[9] as usize;
    if channels == 0 {
        return Err(DecodeError::UnsupportedFormat(
            "OpusHead channel count is 0".to_string(),
        ));
    }
    let pre_skip = u16::from_le_bytes([data[10], data[11]]) as u64;
    let input_sample_rate = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    let family = data[18];
    let (stream_count, coupled_count, mapping) = match family {
        0 => match channels {
            1 => (1, 0, vec![0u8]),
            2 => (1, 1, vec![0u8, 1]),
            n => {
                return Err(DecodeError::UnsupportedFormat(format!(
                    "Opus channel mapping family 0 with {n} channels is invalid"
                )))
            }
        },
        1 => {
            if data.len() < 21 + channels {
                return Err(DecodeError::UnsupportedFormat(
                    "truncated OpusHead mapping table".to_string(),
                ));
            }
            let streams = data[19] as usize;
            let coupled = data[20] as usize;
            if streams == 0 || coupled > streams {
                return Err(DecodeError::UnsupportedFormat(
                    "invalid Opus stream/coupled counts".to_string(),
                ));
            }
            let mapping = data[21..21 + channels].to_vec();
            (streams, coupled, mapping)
        }
        255 => {
            return Err(DecodeError::UnsupportedFormat(
                "Opus channel mapping family 255 (custom) is not supported".to_string(),
            ))
        }
        f => {
            return Err(DecodeError::UnsupportedFormat(format!(
                "unsupported Opus channel mapping family {f}"
            )))
        }
    };
    Ok(OpusHead {
        channels,
        pre_skip,
        input_sample_rate,
        stream_count,
        coupled_count,
        mapping,
    })
}

/// Parse the OpusTags packet (RFC 7845 §5.2) into a key → first-value map
/// (keys lowercased). Malformed comment fields are skipped defensively so
/// hostile metadata can never panic the parser or allocate unbounded memory
/// (lengths are bounds-checked against the packet).
fn parse_opus_tags(data: &[u8]) -> HashMap<String, String> {
    let mut tags = HashMap::new();
    if data.len() < 16 || &data[..8] != OPUS_TAGS_MAGIC {
        return tags;
    }
    let u32_at = |pos: usize| -> Option<u32> {
        data.get(pos..pos + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut pos = 8;
    // Vendor string: length-prefixed. Malformed length → return what we have.
    let Some(vendor_len) = u32_at(pos) else {
        return tags;
    };
    let vendor_len = vendor_len as usize;
    let Some(next) = pos.checked_add(4).and_then(|p| p.checked_add(vendor_len)) else {
        return tags;
    };
    pos = next;
    let Some(count) = u32_at(pos) else {
        return tags;
    };
    let count = count as usize;
    pos += 4;
    for _ in 0..count.min(1 << 20) {
        // Bounds-checked; truncated packets simply stop producing tags.
        let Some(len) = u32_at(pos) else { break };
        let len = len as usize;
        let Some(start) = pos.checked_add(4) else {
            break;
        };
        let Some(end) = start.checked_add(len) else {
            break;
        };
        if end > data.len() {
            break;
        }
        pos = end;
        let field = String::from_utf8_lossy(&data[start..end]);
        if let Some(eq) = field.find('=') {
            let key = field[..eq].trim().to_ascii_lowercase();
            let value = field[eq + 1..].trim().to_string();
            if !value.is_empty() && !tags.contains_key(&key) {
                tags.insert(key, value);
            }
        }
    }
    tags
}

/// Map an Ogg demux error onto the engine's unified decode error.
fn map_ogg_error(e: OggReadError) -> DecodeError {
    match e {
        OggReadError::ReadError(io) => DecodeError::Io(io),
        other => DecodeError::Decode(format!("Ogg demux error: {other}")),
    }
}

/// A decoded Ogg Opus source.
pub struct OpusSource {
    reader: PacketReader<BufReader<File>>,
    decoder: opus_decoder::OpusMultistreamDecoder,
    info: DecodeInfo,
    format_info: AudioFormatInfo,
    tags: HashMap<String, String>,
    /// Serial number of the Opus logical stream we follow.
    serial: u32,
    /// Multistream layout (streams, coupled streams, channel mapping) so a
    /// post-seek decoder rebuild can reconstruct the exact configuration.
    stream_layout: (usize, usize, Vec<u8>),
    pre_skip: u64,
    /// Total logical (post-pre-skip) samples, from the final page granule.
    total_logical: Option<u64>,
    /// Logical samples emitted so far (post pre-skip).
    produced: u64,
    /// Samples to discard before emitting (pre-skip at start; the seek
    /// target after a seek). This doubles as the seek accuracy bound.
    skip_remaining: u64,
    /// Reusable interleaved output buffer (allocation retained across calls).
    interleaved: Vec<f32>,
    /// Reusable decode scratch (allocation retained across calls).
    scratch: Vec<f32>,
}

impl OpusSource {
    /// Content sniff: true when the file's first Ogg packet is an OpusHead.
    /// Used by the decoder dispatch to route `.oga` files — which may hold
    /// Opus *or* Vorbis — to the right backend.
    pub fn probe(path: &Path) -> bool {
        let Ok(file) = File::open(path) else {
            return false;
        };
        let mut reader = PacketReader::new(BufReader::new(file));
        match reader.read_packet() {
            Ok(Some(packet)) => packet.data.len() >= 8 && &packet.data[..8] == OPUS_HEAD_MAGIC,
            _ => false,
        }
    }

    /// Open an Ogg Opus file. Parses OpusHead/OpusTags, builds the
    /// multistream decoder, and scans the final granule position for the
    /// logical duration. Returns an explicit error for malformed files.
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path).map_err(|e| DecodeError::FileOpen(e.to_string()))?;
        let mut reader = PacketReader::new(BufReader::new(file));

        // ── Headers ──────────────────────────────────────────────────────
        let mut head: Option<OpusHead> = None;
        let mut tags: HashMap<String, String> = HashMap::new();
        let mut serial = 0u32;
        while head.is_none() {
            match reader.read_packet().map_err(map_ogg_error)? {
                Some(packet) => {
                    serial = packet.stream_serial();
                    if packet.data.len() >= 8 && &packet.data[..8] == OPUS_HEAD_MAGIC {
                        head = Some(parse_opus_head(&packet.data)?);
                        // OpusTags is the next packet of the same stream.
                        match reader.read_packet().map_err(map_ogg_error)? {
                            Some(t) if &t.data[..8] == OPUS_TAGS_MAGIC => {
                                tags = parse_opus_tags(&t.data);
                            }
                            _ => {}
                        }
                        break;
                    }
                    // A non-Opus first packet: reject (could be a chained
                    // non-Opus stream, which we do not follow).
                    return Err(DecodeError::UnsupportedFormat(
                        "first Ogg packet is not an OpusHead".to_string(),
                    ));
                }
                None => {
                    return Err(DecodeError::UnsupportedFormat(
                        "no OpusHead found in Ogg stream".to_string(),
                    ))
                }
            }
        }
        let head = head.expect("OpusHead parsed above");
        let channels = head.channels;

        // ── Duration: scan to the final page's granule position ─────────
        // Opus granule positions count samples including the pre-skip, so
        // `total_logical = final_granule − pre_skip` (RFC 7845 §4.5).
        let mut last_granule: Option<u64> = None;
        while let Some(packet) = reader.read_packet().map_err(map_ogg_error)? {
            if packet.stream_serial() == serial
                && packet.data.len() >= 8
                && &packet.data[..8] != OPUS_HEAD_MAGIC
                && &packet.data[..8] != OPUS_TAGS_MAGIC
            {
                last_granule = Some(packet.absgp_page());
            }
        }
        let total_logical = last_granule.map(|g| g.saturating_sub(head.pre_skip));

        // Rewind to the start; the decoder state below is rebuilt fresh.
        reader
            .seek_bytes(SeekFrom::Start(0))
            .map_err(|e| DecodeError::Seek(e.to_string()))?;

        let decoder = opus_decoder::OpusMultistreamDecoder::new(
            OPUS_DECODE_RATE,
            channels,
            head.stream_count,
            head.coupled_count,
            &head.mapping,
        )
        .map_err(|e| DecodeError::UnsupportedFormat(format!("Opus decoder init: {e}")))?;

        let duration_secs = total_logical
            .map(|n| n as f64 / OPUS_DECODE_RATE as f64)
            .unwrap_or(0.0) as f32;
        let info = DecodeInfo {
            sample_rate: OPUS_DECODE_RATE,
            channels,
            duration_secs,
            codec: "Opus".to_string(),
            bitrate_kbps: None,
        };
        let gapless = GaplessInfo {
            encoder_delay: head.pre_skip,
            end_padding: 0,
            priming_frames: head.pre_skip,
            total_logical_frames: total_logical,
        };
        let format_info = AudioFormatInfo {
            codec: "Opus".to_string(),
            container: "Ogg".to_string(),
            sample_rate: OPUS_DECODE_RATE,
            input_sample_rate: Some(head.input_sample_rate),
            channels,
            channel_layout: ChannelLayout::from_count(channels),
            bit_depth: None,
            sample_format: "f32".to_string(),
            duration_secs: Some(duration_secs as f64),
            bitrate_kbps: None,
            gapless: Some(gapless),
            replaygain_track_db: replaygain_value(&tags, "replaygain_track_gain"),
            replaygain_album_db: replaygain_value(&tags, "replaygain_album_gain"),
            ebu_r128_loudness: r128_value(&tags, "r128_track_gain"),
            true_peak_dbtp: None,
            is_lossless: false,
            is_dsd: false,
        };

        Ok(Self {
            reader,
            decoder,
            info,
            format_info,
            tags,
            serial,
            stream_layout: (head.stream_count, head.coupled_count, head.mapping.clone()),
            pre_skip: head.pre_skip,
            total_logical,
            produced: 0,
            skip_remaining: head.pre_skip,
            interleaved: Vec::with_capacity(4096 * channels),
            scratch: vec![0.0f32; MAX_FRAME_SIZE * channels],
        })
    }

    /// Decode the next chunk of up to `max_frames` interleaved f32 frames.
    pub fn decode_next(
        &mut self,
        max_frames: usize,
    ) -> Result<crate::decode::DecodedChunk, DecodeError> {
        if self.produced >= self.total_logical.unwrap_or(u64::MAX) && self.skip_remaining == 0 {
            return Err(DecodeError::EndOfStream);
        }
        self.interleaved.clear();
        let channels = self.info.channels;
        loop {
            let packet = match self.reader.read_packet().map_err(map_ogg_error)? {
                Some(p) => p,
                None => break,
            };
            // Skip headers / other logical streams.
            if packet.stream_serial() != self.serial
                || (packet.data.len() >= 8
                    && (&packet.data[..8] == OPUS_HEAD_MAGIC
                        || &packet.data[..8] == OPUS_TAGS_MAGIC))
            {
                continue;
            }
            let frames = self
                .decoder
                .decode_float(&packet.data, &mut self.scratch, false)
                .map_err(|e| DecodeError::Decode(format!("Opus decode: {e}")))?;
            if frames == 0 {
                continue;
            }
            let mut kept = frames as u64;
            // Pre-skip / post-seek discard.
            if self.skip_remaining > 0 {
                let drop = kept.min(self.skip_remaining);
                self.skip_remaining -= drop;
                kept -= drop;
            }
            // End trim.
            if let Some(total) = self.total_logical {
                let remaining = total.saturating_sub(self.produced);
                kept = kept.min(remaining);
            }
            if kept > 0 {
                let start = (frames as u64 - kept) as usize;
                for i in 0..(kept as usize * channels) {
                    self.interleaved.push(self.scratch[start * channels + i]);
                }
                self.produced += kept;
            }
            if self.interleaved.len() / channels >= max_frames {
                break;
            }
            if kept == 0 && self.produced >= self.total_logical.unwrap_or(u64::MAX) {
                // Trailing packet beyond the logical length — done.
                break;
            }
        }
        let n = self.interleaved.len() / channels;
        if n == 0 {
            return Err(DecodeError::EndOfStream);
        }
        let cap = self.interleaved.capacity();
        let samples = std::mem::replace(&mut self.interleaved, Vec::with_capacity(cap));
        Ok(crate::decode::DecodedChunk {
            samples,
            channels,
            channel_layout: ChannelLayout::from_count(channels),
            sample_rate: OPUS_DECODE_RATE,
            frame_count: n,
            raw_dsd: None,
        })
    }

    /// Seek to a logical position in seconds. Granule-based (binary search
    /// over Ogg pages); the fresh decoder state starts at the target page
    /// and output lands within ± one 20 ms packet of the requested position.
    pub fn seek(&mut self, position_secs: f32) -> Result<(), DecodeError> {
        if !position_secs.is_finite() || position_secs < 0.0 {
            return Err(DecodeError::Seek(format!(
                "Invalid seek position: {position_secs}"
            )));
        }
        let target_logical = if let Some(total) = self.total_logical {
            ((position_secs as f64 * OPUS_DECODE_RATE as f64).round() as u64).min(total)
        } else {
            (position_secs as f64 * OPUS_DECODE_RATE as f64).round() as u64
        };
        let target_granule = self.pre_skip.saturating_add(target_logical);
        let ok = self
            .reader
            .seek_absgp(Some(self.serial), target_granule)
            .map_err(map_ogg_error)?;
        if !ok {
            return Err(DecodeError::Seek("Ogg granule seek failed".to_string()));
        }
        // Fresh decoder state at the new position (Opus decoders hold
        // internal prediction state that must not carry across a jump).
        self.decoder = opus_decoder::OpusMultistreamDecoder::new(
            OPUS_DECODE_RATE,
            self.info.channels,
            self.decoder_streams(),
            self.decoder_coupled(),
            &self.decoder_mapping(),
        )
        .map_err(|e| DecodeError::Decode(format!("Opus decoder re-init: {e}")))?;
        self.produced = 0;
        self.skip_remaining = target_logical;
        Ok(())
    }

    pub fn info(&self) -> &DecodeInfo {
        &self.info
    }

    pub fn format_info(&self) -> &AudioFormatInfo {
        &self.format_info
    }

    /// Parsed OpusTags (Vorbis comment) metadata, keys lowercased.
    pub fn tags(&self) -> &HashMap<String, String> {
        &self.tags
    }

    pub fn duration_secs(&self) -> f32 {
        self.info.duration_secs
    }

    fn decoder_streams(&self) -> usize {
        self.stream_layout.0
    }
    fn decoder_coupled(&self) -> usize {
        self.stream_layout.1
    }
    fn decoder_mapping(&self) -> Vec<u8> {
        self.stream_layout.2.clone()
    }
}

/// Decode a "X dB" ReplayGain tag value (like Symphonia's extractor).
fn replaygain_value(tags: &HashMap<String, String>, key: &str) -> Option<f32> {
    let s = tags.get(key)?;
    let trimmed: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    trimmed.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// Decode an EBU R128 tag: integer LUFS × 100, or a plain LUFS float.
fn r128_value(tags: &HashMap<String, String>, key: &str) -> Option<f32> {
    let s = tags.get(key)?;
    let trimmed: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    let v = trimmed.parse::<f32>().ok()?;
    if !v.is_finite() {
        return None;
    }
    if v.abs() > 200.0 {
        Some(v / 100.0)
    } else {
        Some(v)
    }
}

// ── Standalone metadata extractors (used by `symphonia_decoder` dispatch) ──

/// Parse the OpusHead + OpusTags from an Ogg Opus file. Returns
/// (tags, total_logical_secs) or `None` for non-Opus / malformed files.
pub fn extract_opus_info(path: &Path) -> Option<(HashMap<String, String>, f64)> {
    let file = File::open(path).ok()?;
    let mut reader = PacketReader::new(BufReader::new(file));
    let mut tags = HashMap::new();
    let mut seen_head = false;
    let mut last_granule: Option<u64> = None;
    let mut serial = 0u32;
    loop {
        let packet = match reader.read_packet().ok()? {
            Some(p) => p,
            None => break,
        };
        if packet.data.len() < 8 {
            continue;
        }
        if &packet.data[..8] == OPUS_HEAD_MAGIC {
            seen_head = true;
            serial = packet.stream_serial();
        } else if &packet.data[..8] == OPUS_TAGS_MAGIC {
            tags = parse_opus_tags(&packet.data);
        } else if seen_head && packet.stream_serial() == serial {
            last_granule = Some(packet.absgp_page());
        }
    }
    if !seen_head {
        return None;
    }
    // Re-read the head for the pre-skip (cheap second pass only when the
    // first pass found tags; avoids duplicating the head parser on the
    // audio path).
    let pre_skip = {
        let file = File::open(path).ok()?;
        let mut r = PacketReader::new(BufReader::new(file));
        let mut ps = 0u64;
        while let Some(p) = r.read_packet().ok()? {
            if p.data.len() >= 12 && &p.data[..8] == OPUS_HEAD_MAGIC {
                ps = u16::from_le_bytes([p.data[10], p.data[11]]) as u64;
                break;
            }
        }
        ps
    };
    let total = last_granule
        .map(|g| g.saturating_sub(pre_skip) as f64 / OPUS_DECODE_RATE as f64)
        .unwrap_or(0.0);
    Some((tags, total))
}

/// Extract ReplayGain / EBU R128 loudness metadata from OpusTags.
pub fn extract_loudness_metadata(path: &Path) -> crate::dsp::loudness::LoudnessMetadata {
    use crate::dsp::loudness::LoudnessMetadata;
    let mut meta = LoudnessMetadata::default();
    let Some((tags, _)) = extract_opus_info(path) else {
        return meta;
    };
    meta.replaygain_track_db = replaygain_value(&tags, "replaygain_track_gain");
    meta.replaygain_album_db = replaygain_value(&tags, "replaygain_album_gain");
    meta.replaygain_track_peak = replaygain_value(&tags, "replaygain_track_peak");
    meta.replaygain_album_peak = replaygain_value(&tags, "replaygain_album_peak");
    meta.ebu_r128_loudness =
        r128_value(&tags, "r128_track_gain").or_else(|| r128_value(&tags, "r128_album_gain"));
    meta
}

/// Extract title, artist, album, duration (seconds), and a formatted
/// duration string from OpusTags. Same shape as Symphonia's extractor so the
/// `decode::extract_track_metadata` dispatcher can route by extension.
pub fn extract_track_metadata(path: &Path) -> (String, String, String, f64, String) {
    let default_title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown Track")
        .to_string();
    let Some((tags, duration_secs)) = extract_opus_info(path) else {
        return (
            default_title,
            "Unknown Artist".into(),
            "Unknown Album".into(),
            0.0,
            "0:00".into(),
        );
    };
    let get = |key: &str| -> Option<String> {
        tags.get(key)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };
    let title = get("title").unwrap_or(default_title);
    let artist = get("artist").unwrap_or_else(|| "Unknown Artist".to_string());
    let album = get("album").unwrap_or_else(|| "Unknown Album".to_string());
    let duration_str = if duration_secs > 0.0 {
        format!(
            "{}:{:02}",
            (duration_secs as i32) / 60,
            (duration_secs as i32) % 60
        )
    } else {
        "0:00".to_string()
    };
    (title, artist, album, duration_secs, duration_str)
}

/// Extract embedded cover art from the `METADATA_BLOCK_PICTURE` tag
/// (base64-encoded FLAC picture block, RFC 7845 §5.2.1). Returns the raw
/// image bytes and a file-extension hint.
pub fn extract_cover_art(path: &Path) -> Option<(Vec<u8>, &'static str)> {
    let (tags, _) = extract_opus_info(path)?;
    let b64 = tags.get("metadata_block_picture")?;
    let bytes = base64_decode(b64)?;
    parse_flac_picture(&bytes)
}

/// Minimal standard-alphabet base64 decoder (no whitespace handling needed
/// for METADATA_BLOCK_PICTURE, which is emitted unpadded or padded by the
/// encoder). Returns `None` on malformed input.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in input.bytes() {
        if c == b'=' {
            break; // padding; ignore the rest
        }
        let v = TABLE.iter().position(|&t| t == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Parse a FLAC `PICTURE` metadata block: big-endian type(4), mime len(4),
/// mime, desc len(4), desc, width(4), height(4), depth(4), colors(4), data
/// len(4), data. Returns (image bytes, extension).
fn parse_flac_picture(bytes: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    let u32be = |pos: usize| -> Option<u32> {
        bytes
            .get(pos..pos + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let _picture_type = u32be(0)?;
    let mime_len = u32be(4)? as usize;
    let mime_start = 8usize.checked_add(mime_len)?;
    if mime_start > bytes.len() {
        return None;
    }
    let mime = std::str::from_utf8(&bytes[8..mime_start])
        .ok()?
        .to_ascii_lowercase();
    let desc_len = u32be(mime_start)? as usize;
    let desc_end = mime_start.checked_add(4)?.checked_add(desc_len)?;
    if desc_end > bytes.len() {
        return None;
    }
    // Skip width(4) height(4) depth(4) colors(4) = 16 bytes.
    let data_len_pos = desc_end.checked_add(16)?;
    let data_len = u32be(data_len_pos)? as usize;
    let data_start = data_len_pos.checked_add(4)?;
    let data_end = data_start.checked_add(data_len)?;
    if data_end > bytes.len() {
        return None;
    }
    let ext = if mime.contains("png") {
        "png"
    } else if mime.contains("jpeg") || mime.contains("jpg") {
        "jpg"
    } else {
        "jpg"
    };
    Some((bytes[data_start..data_end].to_vec(), ext))
}

// ── Test support: deterministic Ogg Opus fixture generation ─────────────────
//
// Test-only: encodes a 440 Hz sine with the pure-Rust `rusty-opus` encoder
// and wraps the packets in a valid Ogg Opus container via `ogg`'s
// `PacketWriter`. No committed binary assets; the fixture is derived from
// the sine parameters at test time.

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Write;

    use ogg::writing::{PacketWriteEndInfo, PacketWriter};

    /// Encode `n_frames` frames (plus `pre_skip` leading silence samples) of
    /// a 440 Hz sine at 48 kHz and return the bytes of a complete Ogg Opus
    /// file with the given tags. `channels` must be 1 or 2.
    pub fn build_ogg_opus_bytes(
        channels: usize,
        n_frames: usize,
        pre_skip: u16,
        tags: &[(&str, &str)],
    ) -> Vec<u8> {
        assert!(channels == 1 || channels == 2);
        let frame_size = 960usize; // 20 ms at 48 kHz

        // OpusHead (RFC 7845 §5.1).
        let mut head = Vec::new();
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(channels as u8);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
        head.extend_from_slice(&0u16.to_le_bytes()); // output gain (Q7.8)
        head.push(0); // channel mapping family 0

        // OpusTags (RFC 7845 §5.2).
        let mut tags_packet = Vec::new();
        tags_packet.extend_from_slice(b"OpusTags");
        let vendor = "engine-test";
        tags_packet.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags_packet.extend_from_slice(vendor.as_bytes());
        tags_packet.extend_from_slice(&(tags.len() as u32).to_le_bytes());
        for (k, v) in tags {
            let field = format!("{k}={v}");
            tags_packet.extend_from_slice(&(field.len() as u32).to_le_bytes());
            tags_packet.extend_from_slice(field.as_bytes());
        }

        // Encode pre_skip silence + sine.
        let mut encoder =
            rusty_opus::OpusEncoder::new(48_000, channels, rusty_opus::Application::Audio)
                .expect("rusty-opus encoder");
        // Opus only encodes the RFC frame sizes (120/240/480/960/1920/2880),
        // so the final partial block is zero-padded to a full frame; the final
        // page's granule is set to the exact logical length (`pre_skip +
        // n_frames`) so the decoder trims the padding — the standard Ogg Opus
        // end-padding convention (RFC 7845 §4.5).
        let total_input = pre_skip as usize + n_frames;
        let packets = total_input.div_ceil(frame_size);
        let mut pcm = vec![0.0f32; frame_size * channels];
        let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(packets);
        for p in 0..packets {
            let base = p * frame_size;
            for f in 0..frame_size {
                let idx = base + f;
                let s = if idx >= pre_skip as usize && idx < total_input {
                    let t = (idx - pre_skip as usize) as f32 / 48_000.0;
                    0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                } else {
                    0.0 // pre-skip silence / end padding
                };
                pcm[f * channels] = s;
                if channels == 2 {
                    pcm[f * channels + 1] = s;
                }
            }
            let mut out = vec![0u8; 8192];
            let n = encoder
                .encode(&pcm, frame_size, &mut out)
                .expect("opus encode");
            encoded.push(out[..n].to_vec());
        }

        // Wrap in Ogg pages. OpusHead and OpusTags each get their own page.
        let serial = 0x0E5u32;
        let mut buf = Vec::new();
        {
            let mut w = PacketWriter::new(&mut buf);
            w.write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
                .expect("write OpusHead page");
            w.write_packet(tags_packet, serial, PacketWriteEndInfo::EndPage, 0)
                .expect("write OpusTags page");
            let mut granule = pre_skip as u64;
            for (i, p) in encoded.iter().enumerate() {
                let last = i + 1 == encoded.len();
                // Full frame for every packet except the last, whose granule
                // lands exactly on the logical end (`pre_skip + n_frames`).
                // Monotone for the fixture's callers (n_frames ≥ packets·960
                // − 960, i.e. the final partial block is never longer than a
                // full frame's worth of *trailing* samples).
                granule += if last {
                    debug_assert!(
                        n_frames >= i * frame_size,
                        "fixture end granule must stay monotone"
                    );
                    (n_frames - i * frame_size) as u64
                } else {
                    frame_size as u64
                };
                let end = if last {
                    PacketWriteEndInfo::EndStream
                } else {
                    PacketWriteEndInfo::NormalPacket
                };
                w.write_packet(p.clone(), serial, end, granule)
                    .expect("write audio page");
            }
        }
        buf.flush().ok();
        buf
    }

    /// Write a deterministic Ogg Opus test file and return its path.
    pub fn write_test_opus(
        channels: usize,
        n_frames: usize,
        pre_skip: u16,
        tags: &[(&str, &str)],
    ) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "engine_opus_test_{}_{}.opus",
            std::process::id(),
            n
        ));
        let bytes = build_ogg_opus_bytes(channels, n_frames, pre_skip, tags);
        std::fs::write(&path, &bytes).expect("write opus fixture");
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::DecodeError;
    use test_support::write_test_opus;

    #[test]
    fn test_head_and_tags_parsing() {
        let tags = [
            ("TITLE", "Test Track"),
            ("ARTIST", "Tester"),
            ("REPLAYGAIN_TRACK_GAIN", "-7.10 dB"),
            ("R128_TRACK_GAIN", "-1600"),
        ];
        let path = write_test_opus(2, 48_000, 312, &tags);
        let src = OpusSource::open(&path).expect("open opus");
        assert_eq!(src.info().sample_rate, 48_000);
        assert_eq!(src.info().channels, 2);
        assert_eq!(src.info().codec, "Opus");
        // total = n_frames (granule counts pre_skip, subtracted back).
        let expected = 48_000.0 / 48_000.0;
        assert!((src.info().duration_secs - expected).abs() < 0.05);
        assert_eq!(
            src.tags().get("title").map(String::as_str),
            Some("Test Track")
        );
        assert_eq!(
            src.format_info().replaygain_track_db,
            Some(-7.1),
            "ReplayGain parsed from OpusTags"
        );
        assert!(
            (src.format_info().ebu_r128_loudness.unwrap() + 16.0).abs() < 0.01,
            "R128 integer ×100 form parsed"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_full_decode_length_and_gapless_trim() {
        let path = write_test_opus(2, 48_000, 312, &[]);
        let mut src = OpusSource::open(&path).expect("open opus");
        let mut total_frames = 0usize;
        loop {
            match src.decode_next(4096) {
                Ok(chunk) => {
                    assert_eq!(chunk.channels, 2);
                    assert_eq!(chunk.sample_rate, 48_000);
                    assert_eq!(chunk.samples.len(), chunk.frame_count * 2);
                    total_frames += chunk.frame_count;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        // The container's granule yields exactly n_frames logical samples.
        assert_eq!(
            total_frames, 48_000,
            "logical frame count after pre-skip + trim"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_decoded_signal_is_the_sine_and_pre_skip_applied() {
        let path = write_test_opus(2, 48_000, 312, &[]);
        let mut src = OpusSource::open(&path).expect("open opus");
        let mut collected: Vec<f32> = Vec::new();
        loop {
            match src.decode_next(8192) {
                Ok(c) => collected.extend_from_slice(&c.samples),
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        // Pre-skip discarded: the first kept frame is the start of the sine.
        // After the lossy codec's startup, amplitude must be ~0.5 mid-stream.
        let mid = &collected[24_000 * 2..28_000 * 2];
        let peak = mid.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!(peak > 0.3, "sine amplitude mid-stream, got {peak}");
        // Pitch check: zero crossings ≈ 2 per 440 Hz period.
        let window = &collected[8_000 * 2..16_000 * 2];
        let mut crossings = 0usize;
        for i in 1..window.len() / 2 {
            let l0 = window[(i - 1) * 2];
            let l1 = window[i * 2];
            if (l0 <= 0.0 && l1 > 0.0) || (l0 >= 0.0 && l1 < 0.0) {
                crossings += 1;
            }
        }
        // 8000 frames at 48 kHz = 1/6 s → ≈ 440*2/6 ≈ 146 crossings.
        assert!(
            (100..=200).contains(&crossings),
            "440 Hz sine zero crossings, got {crossings}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_seek_lands_near_target_with_exact_remaining_count() {
        let path = write_test_opus(2, 48_000, 312, &[]);
        let mut src = OpusSource::open(&path).expect("open opus");
        src.seek(0.25).expect("seek");
        let mut total = 0usize;
        let mut nonzero = false;
        loop {
            match src.decode_next(4096) {
                Ok(c) => {
                    total += c.frame_count;
                    nonzero |= c.samples.iter().any(|s| s.abs() > 0.01);
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error after seek: {e}"),
            }
        }
        // Remaining logical frames ≈ 0.75 s (36 000), within ±1 packet (960).
        assert!(
            (36_000..=36_960).contains(&total),
            "frames after seek: {total}"
        );
        assert!(nonzero, "audio present after seek");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_malformed_files_rejected() {
        // Not Ogg at all.
        let path = std::env::temp_dir().join(format!("bad_opus_{}.opus", std::process::id()));
        std::fs::write(&path, b"this is not an ogg file at all, sorry").unwrap();
        assert!(OpusSource::open(&path).is_err());
        let _ = std::fs::remove_file(&path);

        // Ogg container but no OpusHead.
        let path = std::env::temp_dir().join(format!("bad_opus2_{}.opus", std::process::id()));
        std::fs::write(&path, b"OggS").unwrap();
        assert!(OpusSource::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_mono_decode() {
        let path = write_test_opus(1, 24_000, 312, &[]);
        let mut src = OpusSource::open(&path).expect("open mono opus");
        assert_eq!(src.info().channels, 1);
        let mut total = 0usize;
        loop {
            match src.decode_next(4096) {
                Ok(c) => total += c.frame_count,
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(total, 24_000);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_metadata_extractors() {
        let tags = [("TITLE", "Song"), ("ARTIST", "Artist"), ("ALBUM", "Album")];
        let path = write_test_opus(2, 24_000, 0, &tags);
        let (title, artist, album, dur, dur_str) = extract_track_metadata(&path);
        assert_eq!(title, "Song");
        assert_eq!(artist, "Artist");
        assert_eq!(album, "Album");
        assert!((dur - 0.5).abs() < 0.05, "duration {dur}");
        assert_eq!(dur_str, "0:00", "under a minute formats as 0:SS");
        let meta = extract_loudness_metadata(&path);
        assert!(meta.replaygain_track_db.is_none(), "no gain tag in fixture");
        let _ = std::fs::remove_file(&path);
    }
}
