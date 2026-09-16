//! AES67 interoperability profile and Session Description Protocol (SDP) handling (§10.4, Item 33).
//!
//! Provides AES67 packet time profiles (1 ms, 125 µs, etc.), stream configurations,
//! RFC 4566 SDP generation and parsing, and compliance validation.

use serde::{Deserialize, Serialize};

/// AES67 standard packet times (transmission intervals).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Aes67PacketTime {
    /// 125 microseconds (ultra-low latency: 6 samples @ 48 kHz).
    Us125,
    /// 250 microseconds (12 samples @ 48 kHz).
    Us250,
    /// 333 microseconds (16 samples @ 48 kHz).
    Us333,
    /// 1 millisecond (mandatory baseline: 48 samples @ 48 kHz, 96 samples @ 96 kHz).
    Ms1,
    /// 4 milliseconds (192 samples @ 48 kHz).
    Ms4,
}

impl Aes67PacketTime {
    /// Returns the packet duration in microseconds.
    pub fn as_micros(&self) -> u32 {
        match self {
            Self::Us125 => 125,
            Self::Us250 => 250,
            Self::Us333 => 333,
            Self::Ms1 => 1000,
            Self::Ms4 => 4000,
        }
    }

    /// Returns the packet duration in fractional milliseconds.
    pub fn as_millis_f32(&self) -> f32 {
        match self {
            Self::Us125 => 0.125,
            Self::Us250 => 0.250,
            Self::Us333 => 0.333,
            Self::Ms1 => 1.0,
            Self::Ms4 => 4.0,
        }
    }

    /// Calculates the number of audio samples per channel in one packet for a given sample rate.
    pub fn samples_per_packet(&self, sample_rate: u32) -> usize {
        match self {
            Self::Us125 => ((sample_rate as u64 * 125) / 1_000_000) as usize,
            Self::Us250 => ((sample_rate as u64 * 250) / 1_000_000) as usize,
            Self::Us333 => ((sample_rate as u64 * 333) / 1_000_000) as usize,
            Self::Ms1 => (sample_rate / 1000) as usize,
            Self::Ms4 => ((sample_rate * 4) / 1000) as usize,
        }
    }
}

/// AES67 linear PCM audio encoding format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Aes67Encoding {
    /// 24-bit linear PCM (mandatory AES67 format, RFC 3190).
    L24,
    /// 16-bit linear PCM (optional AES67 format, RFC 3551).
    L16,
}

impl Aes67Encoding {
    pub fn bytes_per_sample(&self) -> usize {
        match self {
            Self::L24 => 3,
            Self::L16 => 2,
        }
    }

    pub fn mime_name(&self) -> &'static str {
        match self {
            Self::L24 => "L24",
            Self::L16 => "L16",
        }
    }
}

/// Structured configuration for an AES67 network audio stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aes67StreamConfig {
    pub stream_name: String,
    pub session_id: u64,
    pub destination_ip: String,
    pub destination_port: u16,
    pub sample_rate: u32,
    pub channels: u16,
    pub packet_time: Aes67PacketTime,
    pub encoding: Aes67Encoding,
    pub payload_type: u8,
    pub ptp_grandmaster_id: Option<String>,
}

impl Default for Aes67StreamConfig {
    fn default() -> Self {
        Self {
            stream_name: "Shadow-Audio-AES67".into(),
            session_id: 1001,
            destination_ip: "239.69.1.1".into(),
            destination_port: 5004,
            sample_rate: 48000,
            channels: 2,
            packet_time: Aes67PacketTime::Ms1,
            encoding: Aes67Encoding::L24,
            payload_type: 96,
            ptp_grandmaster_id: Some("00-11-22-FF-FE-33-44-55".into()),
        }
    }
}

impl Aes67StreamConfig {
    /// Generates an RFC 4566 SDP document representing this AES67 stream.
    pub fn to_sdp(&self) -> String {
        let ptime_str = format!("{:.3}", self.packet_time.as_millis_f32());
        let mut sdp = String::with_capacity(512);

        sdp.push_str("v=0\r\n");
        sdp.push_str(&format!(
            "o=- {} {} IN IP4 {}\r\n",
            self.session_id, self.session_id, self.destination_ip
        ));
        sdp.push_str(&format!("s={}\r\n", self.stream_name));
        sdp.push_str(&format!("c=IN IP4 {}/32\r\n", self.destination_ip));
        sdp.push_str("t=0 0\r\n");
        sdp.push_str(&format!(
            "m=audio {} RTP/AVP {}\r\n",
            self.destination_port, self.payload_type
        ));
        sdp.push_str(&format!(
            "a=rtpmap:{} {}/{}/{}\r\n",
            self.payload_type,
            self.encoding.mime_name(),
            self.sample_rate,
            self.channels
        ));
        sdp.push_str(&format!("a=ptime:{}\r\n", ptime_str));

        if let Some(ref gm_id) = self.ptp_grandmaster_id {
            sdp.push_str(&format!("a=ts-refclk:ptp=IEEE1588-2008:{}\r\n", gm_id));
            sdp.push_str("a=mediaclk:direct=0\r\n");
        } else {
            sdp.push_str("a=mediaclk:sender\r\n");
        }

        sdp
    }

    /// Parses an RFC 4566 SDP description into an AES67 stream configuration.
    pub fn from_sdp(sdp: &str) -> Result<Self, String> {
        let mut config = Self::default();
        let mut found_m = false;
        let mut found_c = false;

        for line in sdp.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("s=") {
                config.stream_name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("c=IN IP4 ") {
                let ip = rest.split('/').next().unwrap_or(rest).trim();
                config.destination_ip = ip.to_string();
                found_c = true;
            } else if let Some(rest) = line.strip_prefix("m=audio ") {
                let mut parts = rest.split_whitespace();
                if let Some(port_str) = parts.next() {
                    config.destination_port = port_str
                        .parse::<u16>()
                        .map_err(|e| format!("Invalid port in m= line: {e}"))?;
                }
                // Skip proto (e.g. RTP/AVP)
                let _ = parts.next();
                if let Some(pt_str) = parts.next() {
                    config.payload_type = pt_str
                        .parse::<u8>()
                        .map_err(|e| format!("Invalid payload type in m= line: {e}"))?;
                }
                found_m = true;
            } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
                // Example: 96 L24/48000/2
                let mut parts = rest.split_whitespace();
                let _pt = parts.next();
                if let Some(codec_spec) = parts.next() {
                    let mut tokens = codec_spec.split('/');
                    if let Some(enc_str) = tokens.next() {
                        match enc_str.to_uppercase().as_str() {
                            "L24" => config.encoding = Aes67Encoding::L24,
                            "L16" => config.encoding = Aes67Encoding::L16,
                            _ => return Err(format!("Unsupported AES67 encoding: {enc_str}")),
                        }
                    }
                    if let Some(sr_str) = tokens.next() {
                        config.sample_rate = sr_str
                            .parse::<u32>()
                            .map_err(|e| format!("Invalid sample rate: {e}"))?;
                    }
                    if let Some(ch_str) = tokens.next() {
                        config.channels = ch_str
                            .parse::<u16>()
                            .map_err(|e| format!("Invalid channels count: {e}"))?;
                    }
                }
            } else if let Some(rest) = line.strip_prefix("a=ptime:") {
                let ptime_ms: f32 = rest
                    .trim()
                    .parse()
                    .map_err(|e| format!("Invalid ptime: {e}"))?;
                if ptime_ms <= 0.15 {
                    config.packet_time = Aes67PacketTime::Us125;
                } else if ptime_ms <= 0.28 {
                    config.packet_time = Aes67PacketTime::Us250;
                } else if ptime_ms <= 0.5 {
                    config.packet_time = Aes67PacketTime::Us333;
                } else if ptime_ms <= 2.0 {
                    config.packet_time = Aes67PacketTime::Ms1;
                } else {
                    config.packet_time = Aes67PacketTime::Ms4;
                }
            } else if let Some(rest) = line.strip_prefix("a=ts-refclk:ptp=IEEE1588-2008:") {
                config.ptp_grandmaster_id = Some(rest.trim().to_string());
            }
        }

        if !found_m || !found_c {
            return Err("Incomplete SDP: missing mandatory 'm=' or 'c=' attributes".into());
        }

        Ok(config)
    }
}
