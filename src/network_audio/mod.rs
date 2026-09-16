//! Professional Network Audio subsystem (§10.4, Item 33).
//!
//! Implements RFC 3550 RTP streaming, AES67 interoperability profiles,
//! IEEE 1588-2008 PTP clock synchronization, and RFC 2974 SAP session announcements.

pub mod aes67;
pub mod clock;
pub mod rtp;
pub mod session;

pub use aes67::{Aes67Encoding, Aes67PacketTime, Aes67StreamConfig};
pub use clock::{PtpClock, PtpClockState, PtpClockTelemetry, PtpTimestamp};
pub use rtp::{PcmPayloadCodec, RtpError, RtpHeader, RtpPacket, RTP_HEADER_MIN_SIZE, RTP_VERSION};
pub use session::{AdaptiveJitterBuffer, JitterBufferStats, SapPacket};

/// High-level network audio transmitter stream.
pub struct NetworkAudioSender {
    config: Aes67StreamConfig,
    sequence_number: u16,
    timestamp: u32,
    ssrc: u32,
    samples_per_packet: usize,
}

impl NetworkAudioSender {
    pub fn new(config: Aes67StreamConfig, ssrc: u32) -> Self {
        let samples_per_packet = config.packet_time.samples_per_packet(config.sample_rate);
        Self {
            config,
            sequence_number: 0,
            timestamp: 0,
            ssrc,
            samples_per_packet,
        }
    }

    /// Access active configuration.
    pub fn config(&self) -> &Aes67StreamConfig {
        &self.config
    }

    /// Encode one block of audio into an RTP packet.
    pub fn encode_packet(&mut self, channels: &[&[f32]]) -> RtpPacket {
        let payload = match self.config.encoding {
            Aes67Encoding::L24 => PcmPayloadCodec::encode_l24(channels, self.samples_per_packet),
            Aes67Encoding::L16 => PcmPayloadCodec::encode_l16(channels, self.samples_per_packet),
        };

        let header = RtpHeader {
            version: RTP_VERSION,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: self.sequence_number == 0,
            payload_type: self.config.payload_type,
            sequence_number: self.sequence_number,
            timestamp: self.timestamp,
            ssrc: self.ssrc,
            csrc: Vec::new(),
        };

        self.sequence_number = self.sequence_number.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.samples_per_packet as u32);

        RtpPacket::new(header, payload)
    }
}
