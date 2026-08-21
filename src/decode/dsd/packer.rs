//! DoP (DSD over PCM) v1.1 frame packing.

/// DoP (DSD over PCM) v1.1 frame packer.
///
/// Encapsulates 16 bits of 1-bit DSD data into 24-bit PCM samples with
/// alternating `0x05` / `0xFA` marker bytes in the MSB:
///
/// ```text
/// [23..16: 0x05 or 0xFA] [15..8: DSD byte 0] [7..0: DSD byte 1]
/// ```
pub struct DopPacker {
    toggle: bool,
}

impl Default for DopPacker {
    fn default() -> Self {
        Self::new()
    }
}

impl DopPacker {
    pub fn new() -> Self {
        Self { toggle: false }
    }

    /// Marker byte for a 16-bit payload, advancing the frame toggle once.
    ///
    /// The marker alternates `0x05` (even frames) / `0xFA` (odd frames) so
    /// the DAC can lock onto the DoP framing. Per DoP v1.1, when the payload's
    /// upper byte would itself look like a marker (`0x05`/`0xFA`), the marker
    /// is replaced with `0x06`/`0xFB` so the DSD data can never be mistaken
    /// for the sync pattern.
    #[inline]
    fn marker_for(&mut self, dsd_16: u16) -> u8 {
        let base = if self.toggle { 0xFA } else { 0x05 };
        self.toggle = !self.toggle;
        if (dsd_16 >> 8) == 0x05 {
            0x06
        } else if (dsd_16 >> 8) == 0xFA {
            0xFB
        } else {
            base
        }
    }

    /// Pack a 16-bit DSD word into a 24-bit integer PCM sample (in i32 format,
    /// left-aligned: `word24 << 8`). Advances the marker toggle per word.
    #[inline]
    pub fn pack_sample(&mut self, dsd_16: u16) -> i32 {
        let marker = self.marker_for(dsd_16) as u32;
        let word_24 = (marker << 16) | (dsd_16 as u32);
        // Sign-extend 24-bit to 32-bit integer PCM representation (left-aligned)
        (word_24 as i32) << 8
    }

    /// Pack a stereo pair of 16-bit DSD words into one DoP frame.
    ///
    /// Both channels of a frame carry the **same** marker byte (DoP v1.1), and
    /// the toggle advances once per frame, so the 0x05/0xFA pattern is aligned
    /// across L and R. Each channel's substitution is evaluated independently.
    #[inline]
    pub fn pack_stereo_frame(&mut self, dsd_l: u16, dsd_r: u16) -> (i32, i32) {
        let base = if self.toggle { 0xFA } else { 0x05 };
        self.toggle = !self.toggle;
        let ml = if (dsd_l >> 8) == 0x05 {
            0x06
        } else if (dsd_l >> 8) == 0xFA {
            0xFB
        } else {
            base
        };
        let mr = if (dsd_r >> 8) == 0x05 {
            0x06
        } else if (dsd_r >> 8) == 0xFA {
            0xFB
        } else {
            base
        };
        let pack = |m: u8, w: u16| ((((m as u32) << 16) | (w as u32)) as i32) << 8;
        (pack(ml, dsd_l), pack(mr, dsd_r))
    }

    /// Like [`Self::pack_stereo_frame`] but normalized to `f32` in [-1, 1]
    /// (`word24 / 2^23`), the representation the engine's ring buffer carries.
    ///
    /// The round trip `f32 → i32` (× 2^31 in the output callback) is exact for
    /// 24-bit values, so the DoP word reaches the DAC bit-perfectly.
    #[inline]
    pub fn pack_stereo_frame_f32(&mut self, dsd_l: u16, dsd_r: u16) -> (f32, f32) {
        let (l, r) = self.pack_stereo_frame(dsd_l, dsd_r);
        (
            ((l >> 8) as f32) / 8_388_608.0,
            ((r >> 8) as f32) / 8_388_608.0,
        )
    }

    /// Reset marker toggle (used when seeking: the frame parity restarts at
    /// the new position, which is fine — the DAC re-locks on the 0x05/0xFA
    /// pattern).
    pub fn reset(&mut self) {
        self.toggle = false;
    }
}

/// Native DSD wire format — the byte layout a DAC/backend accepts for raw
/// 1-bit DSD transport.
///
/// Mirror of the ALSA `SND_PCM_FORMAT_DSD_U8 / U16 / U32` family. The DSD
/// samples are LSB-first within each byte (bit 0 = earliest sample); the
/// endianness variant selects the byte order of each multi-byte word on the
/// wire. A byte holds 8 DSD samples, a 16-bit word 16, a 32-bit word 32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DsdWireFormat {
    /// `SND_PCM_FORMAT_DSD_U8` — 1 byte per channel per 8 DSD samples.
    U8,
    /// `SND_PCM_FORMAT_DSD_U16_LE`.
    U16Le,
    /// `SND_PCM_FORMAT_DSD_U16_BE` (also the DoP container order).
    U16Be,
    /// `SND_PCM_FORMAT_DSD_U32_LE`.
    U32Le,
    /// `SND_PCM_FORMAT_DSD_U32_BE`.
    U32Be,
}

impl DsdWireFormat {
    /// Bytes per channel per word (1 / 2 / 2 / 4 / 4).
    pub fn bytes_per_word(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16Le | Self::U16Be => 2,
            Self::U32Le | Self::U32Be => 4,
        }
    }

    /// DSD samples carried per word per channel (8 / 16 / 16 / 32 / 32).
    pub fn samples_per_word(self) -> usize {
        self.bytes_per_word() * 8
    }

    /// The frame (sample) rate a driver must be configured at for a given DSD
    /// bit rate: `bit_rate / samples_per_word`. E.g. DSD64 (2.8224 MHz) →
    /// 352.8 kHz for DSD_U8, 176.4 kHz for DSD_U16, 88.2 kHz for DSD_U32.
    pub fn frame_rate_hz(self, bit_rate: u32) -> u32 {
        (bit_rate / self.samples_per_word() as u32).max(1)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::U8 => "DSD_U8",
            Self::U16Le => "DSD_U16_LE",
            Self::U16Be => "DSD_U16_BE",
            Self::U32Le => "DSD_U32_LE",
            Self::U32Be => "DSD_U32_BE",
        }
    }
}

/// Packs de-interleaved, bit-order-normalized (LSB-first) DSD byte planes
/// into the interleaved byte stream of a negotiated [`DsdWireFormat`].
///
/// Input per channel is the reader's payload bytes (bit 0 = earliest sample
/// after MSB-first sources have been reversed); output is the exact byte
/// sequence `snd_pcm_writei` must receive for the negotiated format —
/// channel-interleaved words: `[ch0 word][ch1 word][ch0 word]…`, each word in
/// the format's endian byte order.
pub struct NativeDsdPacker;

impl NativeDsdPacker {
    /// Pack `channel_bytes` (one normalized byte-slice per channel, LSB-first)
    /// into `out` in the wire layout of `format`. Returns the number of whole
    /// words packed per channel (all channels consume the same word count).
    ///
    /// `out` is cleared and refilled; the caller owns the allocation so the
    /// engine hot path stays allocation-free after warm-up.
    pub fn pack(format: DsdWireFormat, channel_bytes: &[&[u8]], out: &mut Vec<u8>) -> usize {
        out.clear();
        if channel_bytes.is_empty() {
            return 0;
        }
        let bpw = format.bytes_per_word();
        // Words per channel — every channel must have the full word.
        let words = channel_bytes
            .iter()
            .map(|c| c.len() / bpw)
            .min()
            .unwrap_or(0);
        out.reserve(words * channel_bytes.len() * bpw);
        for w in 0..words {
            for ch in channel_bytes {
                let base = w * bpw;
                match format {
                    DsdWireFormat::U8 => out.push(ch[base]),
                    DsdWireFormat::U16Le => {
                        out.push(ch[base]);
                        out.push(ch[base + 1]);
                    }
                    DsdWireFormat::U16Be => {
                        out.push(ch[base + 1]);
                        out.push(ch[base]);
                    }
                    DsdWireFormat::U32Le => {
                        out.extend_from_slice(&ch[base..base + 4]);
                    }
                    DsdWireFormat::U32Be => {
                        out.push(ch[base + 3]);
                        out.push(ch[base + 2]);
                        out.push(ch[base + 1]);
                        out.push(ch[base]);
                    }
                }
            }
        }
        words
    }
}
