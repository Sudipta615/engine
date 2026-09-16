//! AES67 / SAP session management, jitter buffer, and Packet Loss Concealment (PLC) (§10.4, Item 33).
//!
//! Provides Session Announcement Protocol (RFC 2974) discovery, an adaptive
//! audio jitter buffer for RTP packets, and seamless concealment of missing network packets.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use super::aes67::Aes67StreamConfig;
use super::rtp::{sequence_diff, PcmPayloadCodec, RtpPacket};

/// Session Announcement Protocol (RFC 2974) packet parser and serializer.
pub struct SapPacket {
    pub message_hash: u16,
    pub originating_source: [u8; 4],
    pub sdp_payload: String,
}

impl SapPacket {
    pub fn new(source_ip: [u8; 4], sdp: String) -> Self {
        Self {
            message_hash: 0,
            originating_source: source_ip,
            sdp_payload: sdp,
        }
    }

    /// Serializes SAP header + MIME type + SDP payload.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 24 + self.sdp_payload.len());
        // Byte 0: V=1 (bits 7-5 = 001), A=0 (bit 4), R=0 (bit 3), T=0 (bit 2), E=0 (bit 1), C=0 (bit 0)
        out.push(0x20);
        out.push(0x00); // auth len
        out.extend_from_slice(&self.message_hash.to_be_bytes());
        out.extend_from_slice(&self.originating_source);
        // MIME type
        out.extend_from_slice(b"application/sdp\0");
        out.extend_from_slice(self.sdp_payload.as_bytes());
        out
    }

    /// Parses a SAP announcement packet.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 8 {
            return Err("SAP packet too short".into());
        }
        let b0 = bytes[0];
        let version = (b0 >> 5) & 0x07;
        if version != 1 {
            return Err(format!("Unsupported SAP version: {version}"));
        }
        let auth_len = bytes[1] as usize;
        let hash = u16::from_be_bytes([bytes[2], bytes[3]]);
        let ip = [bytes[4], bytes[5], bytes[6], bytes[7]];

        let header_end = 8 + (auth_len * 4);
        if bytes.len() <= header_end {
            return Err("Truncated SAP payload".into());
        }

        let payload_bytes = &bytes[header_end..];
        // Scan for null terminator separating MIME type from SDP payload
        let sdp_start = if let Some(idx) = payload_bytes.iter().position(|&b| b == 0) {
            idx + 1
        } else {
            0
        };

        let sdp_str = String::from_utf8_lossy(&payload_bytes[sdp_start..]).to_string();
        Ok(Self {
            message_hash: hash,
            originating_source: ip,
            sdp_payload: sdp_str,
        })
    }
}

/// Jitter buffer and network receiver telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JitterBufferStats {
    pub packets_received: u64,
    pub packets_lost: u64,
    pub packets_late: u64,
    pub concealment_frames: u64,
    pub current_depth_packets: usize,
    pub target_depth_packets: usize,
    pub current_depth_ms: f32,
}

/// Adaptive jitter buffer with Packet Loss Concealment (PLC).
pub struct AdaptiveJitterBuffer {
    config: Aes67StreamConfig,
    queue: VecDeque<RtpPacket>,
    expected_sequence: Option<u16>,
    target_depth: usize,
    max_depth: usize,
    packets_received: u64,
    packets_lost: u64,
    packets_late: u64,
    concealment_frames: u64,
    /// Last valid frame decoded for linear extrapolation / zero-crossing hold PLC.
    last_valid_frame: Vec<f32>,
}

impl AdaptiveJitterBuffer {
    /// Creates a jitter buffer with the given stream configuration and target packet depth.
    pub fn new(config: Aes67StreamConfig, target_depth_packets: usize) -> Self {
        let channels = config.channels as usize;
        Self {
            config,
            queue: VecDeque::with_capacity(target_depth_packets * 4),
            expected_sequence: None,
            target_depth: target_depth_packets.max(2),
            max_depth: (target_depth_packets * 8).max(32),
            packets_received: 0,
            packets_lost: 0,
            packets_late: 0,
            concealment_frames: 0,
            last_valid_frame: vec![0.0; channels],
        }
    }

    /// Push a newly arrived RTP packet into the jitter queue.
    pub fn push_packet(&mut self, packet: RtpPacket) {
        self.packets_received += 1;

        if let Some(expected) = self.expected_sequence {
            let diff = sequence_diff(packet.header.sequence_number, expected);
            if diff < 0 {
                // Packet arrived too late for playout; discarded
                self.packets_late += 1;
                return;
            }
        }

        // Insert in ascending sequence order
        let seq = packet.header.sequence_number;
        let mut insert_idx = self.queue.len();
        for (i, p) in self.queue.iter().enumerate() {
            let diff = sequence_diff(seq, p.header.sequence_number);
            if diff < 0 {
                insert_idx = i;
                break;
            } else if diff == 0 {
                // Duplicate packet; ignore
                return;
            }
        }

        self.queue.insert(insert_idx, packet);

        // Cap maximum queue capacity to prevent unbounded memory growth under pathological networks
        while self.queue.len() > self.max_depth {
            let _ = self.queue.pop_front();
        }
    }

    /// Reads one audio block into planar destination channels.
    /// If packets are missing or the buffer is empty, invokes Packet Loss Concealment (PLC).
    pub fn read_audio_block(&mut self, channels: &mut [&mut [f32]]) -> usize {
        if channels.is_empty() {
            return 0;
        }
        let requested_frames = channels[0].len();
        let samples_per_pkt = self
            .config
            .packet_time
            .samples_per_packet(self.config.sample_rate);

        // Check if queue has reached playout readiness
        if self.expected_sequence.is_none() {
            if self.queue.len() < self.target_depth {
                // Buffer pre-rolling: output silence
                self.apply_silence(channels, requested_frames);
                return requested_frames;
            }
            if let Some(first) = self.queue.front() {
                self.expected_sequence = Some(first.header.sequence_number);
            }
        }

        let expected = self.expected_sequence.unwrap();

        // Check if head packet matches expected sequence
        let mut head_matches = false;
        if let Some(head) = self.queue.front() {
            let diff = sequence_diff(head.header.sequence_number, expected);
            if diff == 0 {
                head_matches = true;
            } else if diff < 0 {
                // Stale packet at head, discard and retry
                self.queue.pop_front();
                return self.read_audio_block(channels);
            }
        }

        if head_matches {
            let pkt = self.queue.pop_front().unwrap();
            let decode_res = match self.config.encoding {
                super::aes67::Aes67Encoding::L24 => {
                    PcmPayloadCodec::decode_l24(&pkt.payload, channels)
                }
                super::aes67::Aes67Encoding::L16 => {
                    PcmPayloadCodec::decode_l16(&pkt.payload, channels)
                }
            };

            let frames_decoded = decode_res.unwrap_or(0);
            // Save last frame for PLC extrapolation
            let num_ch = channels.len().min(self.last_valid_frame.len());
            if frames_decoded > 0 {
                let last_idx = frames_decoded - 1;
                for (ch, slot) in self.last_valid_frame[..num_ch].iter_mut().enumerate() {
                    *slot = channels[ch][last_idx];
                }
            }

            self.expected_sequence = Some(expected.wrapping_add(1));
            frames_decoded
        } else {
            // Underflow / packet loss: apply Packet Loss Concealment (PLC)
            self.packets_lost += 1;
            self.concealment_frames += samples_per_pkt as u64;
            self.apply_plc(channels, samples_per_pkt);
            self.expected_sequence = Some(expected.wrapping_add(1));
            samples_per_pkt
        }
    }

    /// Packet Loss Concealment: exponential decaying hold of the last valid samples.
    fn apply_plc(&mut self, channels: &mut [&mut [f32]], num_frames: usize) {
        let num_ch = channels.len().min(self.last_valid_frame.len());
        let decay_rate = 0.96f32; // Decays over ~50 samples (~1ms)

        for frame in 0..num_frames {
            let decay = decay_rate.powi(frame as i32);
            for (ch, &last_s) in self.last_valid_frame[..num_ch].iter().enumerate() {
                if frame < channels[ch].len() {
                    channels[ch][frame] = last_s * decay;
                }
            }
        }
    }

    fn apply_silence(&self, channels: &mut [&mut [f32]], num_frames: usize) {
        for plane in channels.iter_mut() {
            let len = plane.len().min(num_frames);
            plane[..len].fill(0.0);
        }
    }

    /// Current jitter buffer diagnostic statistics.
    pub fn stats(&self) -> JitterBufferStats {
        let samples_per_pkt = self
            .config
            .packet_time
            .samples_per_packet(self.config.sample_rate);
        let depth_ms =
            (self.queue.len() * samples_per_pkt * 1000) as f32 / self.config.sample_rate as f32;

        JitterBufferStats {
            packets_received: self.packets_received,
            packets_lost: self.packets_lost,
            packets_late: self.packets_late,
            concealment_frames: self.concealment_frames,
            current_depth_packets: self.queue.len(),
            target_depth_packets: self.target_depth,
            current_depth_ms: depth_ms,
        }
    }

    /// Reset jitter buffer state.
    pub fn reset(&mut self) {
        self.queue.clear();
        self.expected_sequence = None;
        self.last_valid_frame.fill(0.0);
    }
}
