//! TTA (The Lossless True Audio) decoding — pure Rust, no FFI.
//!
//! Implements the TTA1 format: a 22-byte header, a frame-size table (which
//! doubles as the seek index), and back-to-back adaptive-filter + Rice-coded
//! audio frames, each with a trailing CRC-32.
//!
//! The decoder is a faithful implementation of the reference TTA1 algorithm
//! (as documented by the format spec and validated implementations):
//!
//! - **Bitstream**: LSB-first bit order within ascending bytes.
//! - **Rice coding**: per-channel adaptive `k0`/`k1` parameters with 4-bit
//!   shift accumulation (`sum += value - (sum >> 4)`), depth-0/depth-1
//!   escape structure, zigzag sign mapping.
//! - **Hybrid filter**: an 8-tap adaptive FIR (`qm` weights adapted by the
//!   sign of the previous error, `dx` step-size ladder, `dl` delay line)
//!   followed by fixed-order prediction (`PRED(x,k)`).
//! - **Channel decorrelation**: after each interleaved sample slot, the last
//!   channel absorbs half of its predecessor and backward differences run
//!   down to channel 0 (lossless joint-stereo generalised to N channels).
//! - **Framing**: every frame resets all channel state; frame N's sample
//!   count is `frame_length` except the final frame (`data_length %
//!   frame_length`, or full when it divides evenly). Each frame payload ends
//!   byte-aligned and carries a CRC-32 trailer; the size table carries its
//!   own CRC-32.

#![cfg(feature = "codec-tta")]

pub(crate) mod bitstream;
pub(crate) mod crc;
pub(crate) mod decoder;
pub(crate) mod filter;
pub(crate) mod rice;

#[cfg(test)]
mod tests;

pub use decoder::TtaDecoder;
