use super::decimator::FirStage;
use super::*;
use crate::decode::ChannelLayout;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(ext: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "playtune_dsd_test_{}_{}.{}",
        std::process::id(),
        n,
        ext
    ))
}

fn write_bytes(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write test file");
}

/// Interleave per-channel payloads into the DSF/DFF on-disk block layout:
/// `[ch0 block][ch1 block][ch0 block][ch1 block]…` with `block_size` bytes per
/// channel per block.
///
/// `padded` mirrors the container's final-block policy: DSF zero-pads the last
/// block to a full `block_size` per channel (and its size fields count the
/// padding), while DFF stores the partial block compactly.
fn interleave_blocks(block_size: usize, ch0: &[u8], ch1: Option<&[u8]>, padded: bool) -> Vec<u8> {
    let nch = if ch1.is_some() { 2 } else { 1 };
    let frames = ch0.len();
    let out_frames = if padded {
        frames.div_ceil(block_size) * block_size
    } else {
        frames
    };
    let mut out = vec![0u8; out_frames * nch];
    let chans: [&[u8]; 2] = [ch0, ch1.unwrap_or(&[])];
    for (ch, data) in chans.iter().take(nch).enumerate() {
        for (b, chunk) in data.chunks(block_size).enumerate() {
            // Within block `b`, channel `ch` starts after the previous
            // channels' bytes. Full blocks contribute `block_size` bytes per
            // channel; a padded (DSF) last block is also full-sized, while a
            // compact (DFF) last block contributes only `chunk.len()`.
            let per_channel = if padded || chunk.len() == block_size {
                block_size
            } else {
                chunk.len()
            };
            let base = b * block_size * nch + ch * per_channel;
            out[base..base + chunk.len()].copy_from_slice(chunk);
        }
    }
    out
}

fn build_dsf(
    channels: usize,
    block_size: u32,
    bits_per_sample: u32,
    rate_hz: u32,
    frames_per_channel: u64,
    ch0: &[u8],
    ch1: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"DSD ");
    out.extend_from_slice(&28u64.to_le_bytes()); // DSD chunk size
                                                 // Total file size: patched below.
    let total_size_pos = out.len();
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // metadata pointer

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&52u64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // format version
    out.extend_from_slice(&0u32.to_le_bytes()); // format id (DSD raw)
    out.extend_from_slice(&(channels as u32).to_le_bytes()); // channel type
    out.extend_from_slice(&(channels as u32).to_le_bytes()); // channel num
    out.extend_from_slice(&rate_hz.to_le_bytes()); // sampling frequency
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    // Sample count: bits per channel, padded to whole blocks.
    let padded_frames = frames_per_channel.div_ceil(block_size as u64) * block_size as u64;
    out.extend_from_slice(&(padded_frames * 8).to_le_bytes());
    out.extend_from_slice(&block_size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved

    let audio = interleave_blocks(block_size as usize, ch0, ch1, true);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&((audio.len() as u64) + 12).to_le_bytes());
    out.extend_from_slice(&audio);

    let total = out.len() as u64;
    out[total_size_pos..total_size_pos + 8].copy_from_slice(&total.to_le_bytes());
    out
}

/// Build a DSF container with an arbitrary channel count. `data` holds one
/// payload byte-slice per channel; every channel must be the same length
/// (padded to whole blocks by the caller, matching the DSF on-disk layout).
fn build_dsf_nch(
    channels: usize,
    block_size: u32,
    bits_per_sample: u32,
    rate_hz: u32,
    frames_per_channel: u64,
    data: &[Vec<u8>],
) -> Vec<u8> {
    assert_eq!(data.len(), channels);
    let mut out = Vec::new();
    out.extend_from_slice(b"DSD ");
    out.extend_from_slice(&28u64.to_le_bytes());
    let total_size_pos = out.len();
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // metadata pointer

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&52u64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // format version
    out.extend_from_slice(&0u32.to_le_bytes()); // format id (DSD raw)
    out.extend_from_slice(&(channels as u32).to_le_bytes()); // channel type
    out.extend_from_slice(&(channels as u32).to_le_bytes()); // channel num
    out.extend_from_slice(&rate_hz.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    let padded_frames = frames_per_channel.div_ceil(block_size as u64) * block_size as u64;
    out.extend_from_slice(&(padded_frames * 8).to_le_bytes());
    out.extend_from_slice(&block_size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    // Interleave per-block: `[ch0 block][ch1 block]…[chN block]` repeated.
    let block = block_size as usize;
    let mut audio = vec![0u8; (padded_frames as usize) * channels];
    for (ch, chan_data) in data.iter().enumerate() {
        for (b, chunk) in chan_data.chunks(block).enumerate() {
            let base = b * block * channels + ch * block;
            audio[base..base + chunk.len()].copy_from_slice(chunk);
        }
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&((audio.len() as u64) + 12).to_le_bytes());
    out.extend_from_slice(&audio);

    let total = out.len() as u64;
    out[total_size_pos..total_size_pos + 8].copy_from_slice(&total.to_le_bytes());
    out
}

fn build_dff(
    rate_hz: u32,
    channels: u16,
    compression: &[u8; 4],
    ch0: &[u8],
    ch1: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"FRM8");
    let total_size_pos = out.len();
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(b"DSD ");

    // FVER
    out.extend_from_slice(b"FVER");
    out.extend_from_slice(&4u64.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes()); // version

    // PROP -> SND -> FS / CHNL / CMPR
    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend_from_slice(b"FS");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(&rate_hz.to_be_bytes());
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&2u64.to_be_bytes());
    prop.extend_from_slice(&channels.to_be_bytes());
    prop.extend_from_slice(b"CMPR");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(compression);
    out.extend_from_slice(b"PROP");
    out.extend_from_slice(&(prop.len() as u64).to_be_bytes());
    out.extend_from_slice(&prop);

    // DSD audio
    let audio = interleave_blocks(4096, ch0, ch1, false);
    out.extend_from_slice(b"DSD ");
    out.extend_from_slice(&(audio.len() as u64).to_be_bytes());
    out.extend_from_slice(&audio);

    let total = out.len() as u64;
    out[total_size_pos..total_size_pos + 8].copy_from_slice(&(total - 12).to_be_bytes());
    out
}

// ── DoP packer ───────────────────────────────────────────────────────────

#[test]
fn test_dop_marker_alternation() {
    let mut p = DopPacker::new();
    // The 24-bit word is left-aligned in i32 (word24 << 8), so the marker
    // occupies bits 31-24.
    assert_eq!(p.pack_sample(0x1234) >> 24 & 0xFF, 0x05);
    assert_eq!(p.pack_sample(0x1234) >> 24 & 0xFF, 0xFA);
    assert_eq!(p.pack_sample(0x1234) >> 24 & 0xFF, 0x05);
}

#[test]
fn test_dop_marker_substitution() {
    let mut p = DopPacker::new();
    // Payload upper byte 0x05 must not be confused with the marker.
    assert_eq!(p.pack_sample(0x0567) >> 24 & 0xFF, 0x06);
    // Payload upper byte 0xFA -> marker 0xFB.
    assert_eq!(p.pack_sample(0xFA67) >> 24 & 0xFF, 0xFB);
    // Unrelated payload keeps the regular marker.
    assert_eq!(p.pack_sample(0x0123) >> 24 & 0xFF, 0x05);
}

#[test]
fn test_dop_stereo_frame_shared_marker() {
    let mut p = DopPacker::new();
    let (l0, r0) = p.pack_stereo_frame(0x1111, 0x2222);
    let (l1, r1) = p.pack_stereo_frame(0x3333, 0x4444);
    // Both channels of a frame share the marker; the pair alternates.
    assert_eq!(l0 >> 24 & 0xFF, 0x05);
    assert_eq!(r0 >> 24 & 0xFF, 0x05);
    assert_eq!(l1 >> 24 & 0xFF, 0xFA);
    assert_eq!(r1 >> 24 & 0xFF, 0xFA);
    // Payloads survive verbatim in the low 16 bits of the word (bits
    // 15-0 of the container's shifted form).
    assert_eq!(((l0 as u32) >> 8) & 0xFFFF, 0x1111);
    assert_eq!(((r0 as u32) >> 8) & 0xFFFF, 0x2222);
    assert_eq!(((l1 as u32) >> 8) & 0xFFFF, 0x3333);
    assert_eq!(((r1 as u32) >> 8) & 0xFFFF, 0x4444);

    // Per-channel substitution: L mimics 0x05, R mimics 0xFA.
    let (l2, r2) = p.pack_stereo_frame(0x05FF, 0xFAFF);
    assert_eq!(l2 >> 24 & 0xFF, 0x06);
    assert_eq!(r2 >> 24 & 0xFF, 0xFB);
}

#[test]
fn test_dop_f32_roundtrip_is_exact() {
    let mut p = DopPacker::new();
    let (l, r) = p.pack_stereo_frame_f32(0xABCD, 0x4321);
    // f32 -> i32 via the output callback's scale (×2^31) must recover the
    // exact left-aligned 24-bit word.
    let li = (l as f64 * 2_147_483_648.0) as i64;
    let ri = (r as f64 * 2_147_483_648.0) as i64;
    assert_eq!(li, (0x05ABCDu32 as i32 as i64) << 8);
    assert_eq!(ri, (0x054321u32 as i32 as i64) << 8);
}

#[test]
fn test_dop_reset() {
    let mut p = DopPacker::new();
    assert_eq!(p.pack_sample(1) >> 24 & 0xFF, 0x05);
    assert_eq!(p.pack_sample(1) >> 24 & 0xFF, 0xFA);
    p.reset();
    assert_eq!(p.pack_sample(1) >> 24 & 0xFF, 0x05);
}

// ── DSF ──────────────────────────────────────────────────────────────────

#[test]
fn test_dsf_read_stereo() {
    let path = temp_path("dsf");
    let block_size = 16u32;
    let frames = 48u64; // 3 full blocks
    let ch0: Vec<u8> = (0..frames as u8).collect();
    let ch1: Vec<u8> = (0..frames as u8).rev().collect();
    write_bytes(
        &path,
        &build_dsf(2, block_size, 1, 2_822_400, frames, &ch0, Some(&ch1)),
    );

    let mut reader = DsdReader::open(&path).expect("open DSF");
    assert_eq!(reader.rate(), DsdRate::Dsd64);
    assert_eq!(reader.channels(), 2);
    assert!(reader.is_lsb_first());
    assert_eq!(reader.total_dsd_frames(), frames);

    let mut got_frames = 0u64;
    while let Some(block) = reader.read_dsd_block(17).expect("read block") {
        assert!(block.frames <= 17);
        let right = block.right().expect("stereo");
        assert_eq!(block.left().len(), block.frames as usize);
        assert_eq!(right.len(), block.frames as usize);
        let start = got_frames as usize;
        let end = start + block.frames as usize;
        assert_eq!(block.left(), &ch0[start..end], "left channel mismatch");
        assert_eq!(right, &ch1[start..end], "right channel mismatch");
        got_frames += block.frames as u64;
    }
    assert_eq!(got_frames, frames, "all frames consumed");
    assert_eq!(reader.frames_remaining(), 0);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dsf_read_mono() {
    let path = temp_path("dsf");
    let block_size = 16u32;
    let frames = 32u64;
    let ch0: Vec<u8> = (0..frames as u8).map(|i| i.wrapping_mul(7)).collect();
    write_bytes(
        &path,
        &build_dsf(1, block_size, 1, 5_644_800, frames, &ch0, None),
    );

    let mut reader = DsdReader::open(&path).expect("open DSF");
    assert_eq!(reader.rate(), DsdRate::Dsd128);
    assert_eq!(reader.channels(), 1);

    // Reads clamp to block boundaries (16-frame blocks here), so loop.
    let mut got = Vec::new();
    while let Some(block) = reader.read_dsd_block(32).expect("read") {
        assert!(block.right().is_none(), "mono file has no right channel");
        got.extend_from_slice(block.left());
    }
    assert_eq!(got, ch0);
    assert!(reader.read_dsd_block(32).expect("read").is_none());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dsf_msbf_bit_order() {
    // bits_per_sample == 8 selects MSB-first byte order.
    let path = temp_path("dsf");
    let block_size = 8u32;
    let frames = 8u64;
    let ch0 = vec![0x80u8; frames as usize]; // 10000000
    let ch1 = vec![0x01u8; frames as usize]; // 00000001
    write_bytes(
        &path,
        &build_dsf(2, block_size, 8, 2_822_400, frames, &ch0, Some(&ch1)),
    );

    let reader = DsdReader::open(&path).expect("open DSF");
    assert!(!reader.is_lsb_first());

    // 0x80 MSB-first and 0x01 LSB-first both expand to [1,0,0,0,0,0,0,0],
    // so their PCM output must be identical.
    let mut dec = DsdToPcmDecimator::new_32x();
    let mut msbf_l = Vec::new();
    let mut msbf_r = Vec::new();
    dec.decimate_channels(&[&ch0, &ch1], false, &mut [&mut msbf_l, &mut msbf_r]);

    let mut dec = DsdToPcmDecimator::new_32x();
    let mut lsbf_l = Vec::new();
    let mut lsbf_r = Vec::new();
    dec.decimate_channels(
        &[&[0x01u8; 8], &[0x80u8; 8]],
        true,
        &mut [&mut lsbf_l, &mut lsbf_r],
    );

    // 8 bytes = 64 samples + 63 warm-up history -> 2 outputs.
    assert_eq!(msbf_l.len(), 2);
    assert_eq!(msbf_l, lsbf_l, "0x80 MSB-first == 0x01 LSB-first");
    assert_eq!(msbf_r, lsbf_r);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dsf_decode_block_pcm() {
    let path = temp_path("dsf");
    let block_size = 64u32;
    let frames = 1024u64; // 16 blocks × 64
    let ones = vec![0xFFu8; frames as usize]; // all 1-bits -> +1.0
    let zeros = vec![0x00u8; frames as usize]; // all 0-bits -> -1.0
    write_bytes(
        &path,
        &build_dsf(2, block_size, 1, 2_822_400, frames, &ones, Some(&zeros)),
    );

    let mut reader = DsdReader::open(&path).expect("open DSF");
    let mut total_pcm = 0usize;
    let mut seen = 0u32;
    let mut block_no = 0;
    while let Some(pcm) = reader.decode_block(64).expect("decode") {
        seen += pcm.frames;
        assert!(!pcm.left().is_empty());
        assert_eq!(pcm.left().len(), pcm.right().len());
        // The five-stage cascade has a longer startup transient than the
        // former single FIR. After the cascade has filled its delay lines,
        // a constant DSD stream must settle to +1.0 / -1.0.
        if block_no > 8 {
            for &s in pcm.left() {
                assert!(
                    (s - 1.0).abs() < 1e-4,
                    "left all-ones must decode to +1.0, got {s}"
                );
            }
            for &s in pcm.right() {
                assert!(
                    (s + 1.0).abs() < 1e-4,
                    "right all-zeros must decode to -1.0, got {s}"
                );
            }
        }
        total_pcm += pcm.left().len();
        block_no += 1;
    }
    assert_eq!(seen, frames as u32);
    // 1024 payload bytes × 8 DSD bits / 32 = 256 PCM frames.
    assert_eq!(total_pcm, 256);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dsf_multichannel_exposes_all_channels() {
    let path = temp_path("dsf");
    let block_size = 16u32;
    let frames = 64u64; // 4 full blocks per channel
    let channels = 6usize; // FL FR C LFE SL SR (5.1)
    let data: Vec<Vec<u8>> = (0..channels)
        .map(|ch| {
            (0..frames as u8)
                .map(|i| i.wrapping_mul(ch as u8 + 1))
                .collect()
        })
        .collect();
    write_bytes(
        &path,
        &build_dsf_nch(channels, block_size, 1, 2_822_400, frames, &data),
    );

    let mut reader = DsdReader::open(&path).expect("open multichannel DSF");
    assert_eq!(reader.channels(), channels);
    assert_eq!(
        reader.format_info().channel_layout,
        ChannelLayout::FivePointOne
    );

    // Raw bitstream: every channel is exposed in file order.
    let mut decoded: Vec<Vec<u8>> = vec![Vec::new(); channels];
    while let Some(block) = reader.read_dsd_block(20).expect("read block") {
        assert_eq!(block.channels.len(), channels);
        for (ch, dst) in decoded.iter_mut().enumerate().take(channels) {
            dst.extend_from_slice(&block.channels[ch]);
        }
    }
    for (ch, chan) in decoded.iter().enumerate().take(channels) {
        assert_eq!(chan, &data[ch], "channel {ch} bitstream mismatch");
    }

    // PCM path: every channel produces decimated output.
    reader.seek_to_dsd_frame(0);
    let mut pcm_channels: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut total_pcm = 0usize;
    while let Some(pcm) = reader.decode_block(16).expect("decode block") {
        assert_eq!(pcm.channels.len(), channels);
        for (ch, dst) in pcm_channels.iter_mut().enumerate().take(channels) {
            dst.extend_from_slice(&pcm.channels[ch]);
        }
        total_pcm += pcm.channels[0].len();
    }
    // 64 bytes × 8 bits / 32 = 16 PCM frames per channel.
    assert_eq!(total_pcm, 16);
    for (ch, chan) in pcm_channels.iter().enumerate().take(channels) {
        assert_eq!(chan.len(), total_pcm, "channel {ch} PCM count");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_decimator_state_carries_across_blocks() {
    // Feeding a signal block-by-block must produce exactly the same PCM
    // as feeding the whole signal in one call — i.e. no discontinuity at
    // block boundaries.
    let block1 = [0xFFu8; 64];
    let block2 = [0xABu8; 64];
    let block3 = [0x00u8; 64];
    let blocks = [&block1[..], &block2[..], &block3[..]];

    // Split path: one decimate_channels call per block.
    let mut dec_split = DsdToPcmDecimator::new_32x();
    let mut split_l = Vec::new();
    let mut split_r = Vec::new();
    for b in &blocks {
        dec_split.decimate_channels(&[b, b], true, &mut [&mut split_l, &mut split_r]);
    }

    // One-shot path: the same bytes in a single call.
    let mut all = Vec::new();
    for b in &blocks {
        all.extend_from_slice(b);
    }
    let mut dec_once = DsdToPcmDecimator::new_32x();
    let mut once_l = Vec::new();
    let mut once_r = Vec::new();
    dec_once.decimate_channels(&[&all, &all], true, &mut [&mut once_l, &mut once_r]);

    assert_eq!(
        split_l, once_l,
        "block-split decoding must match one-shot decoding (left)"
    );
    assert_eq!(
        split_r, once_r,
        "block-split decoding must match one-shot decoding (right)"
    );
}

#[test]
fn test_decimator_reset() {
    // After reset, decoding must behave exactly as a fresh decimator.
    let mut dec = DsdToPcmDecimator::new_32x();
    let mut l = Vec::new();
    let mut r = Vec::new();
    dec.decimate_channels(&[&[0xFFu8; 64], &[0xFFu8; 64]], true, &mut [&mut l, &mut r]);
    dec.reset();

    l.clear();
    r.clear();
    dec.decimate_channels(&[&[0xFFu8; 64], &[0xFFu8; 64]], true, &mut [&mut l, &mut r]);

    let mut fresh = DsdToPcmDecimator::new_32x();
    let mut fl = Vec::new();
    let mut fr = Vec::new();
    fresh.decimate_channels(
        &[&[0xFFu8; 64], &[0xFFu8; 64]],
        true,
        &mut [&mut fl, &mut fr],
    );
    assert_eq!(l, fl);
    assert_eq!(r, fr);
}

#[test]
fn test_dsf_format_info() {
    let path = temp_path("dsf");
    let block_size = 16u32;
    let frames = 32u64;
    write_bytes(
        &path,
        &build_dsf(
            2,
            block_size,
            1,
            2_822_400,
            frames,
            &[0u8; 32],
            Some(&[0u8; 32]),
        ),
    );
    let reader = DsdReader::open(&path).expect("open DSF");
    let info = reader.format_info();
    assert_eq!(info.container, "DSF");
    assert_eq!(info.sample_rate, 2_822_400);
    assert_eq!(info.channels, 2);
    assert!(info.is_dsd);
    assert!(info.is_lossless);
    // DSF duration derives from the bit count: padded frames × 8 bits / rate.
    let expected = (frames * 8) as f64 / 2_822_400.0;
    assert!((info.duration_secs.unwrap() - expected).abs() < 1e-9);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dsf_invalid_inputs() {
    // Unsupported channel count (7 is outside the DSF spec range).
    let path = temp_path("dsf");
    write_bytes(&path, &build_dsf(7, 16, 1, 2_822_400, 16, &[0u8; 16], None));
    assert!(matches!(
        DsdReader::open(&path),
        Err(DsdError::UnsupportedChannels(7))
    ));
    let _ = std::fs::remove_file(&path);

    // Unknown container magic.
    let path = temp_path("bin");
    write_bytes(&path, b"NOTA");
    assert!(matches!(
        DsdReader::open(&path),
        Err(DsdError::InvalidHeader(_))
    ));
    let _ = std::fs::remove_file(&path);

    // Unsupported rate.
    let path = temp_path("dsf");
    write_bytes(
        &path,
        &build_dsf(2, 16, 1, 48_000, 16, &[0u8; 16], Some(&[0u8; 16])),
    );
    assert!(matches!(
        DsdReader::open(&path),
        Err(DsdError::UnsupportedRate(48_000))
    ));
    let _ = std::fs::remove_file(&path);
}

// ── DFF ──────────────────────────────────────────────────────────────────

#[test]
fn test_dff_read_stereo_partial_blocks() {
    let path = temp_path("dff");
    let frames = 5000u64; // 1 full 4096-sample block + a partial one
    let ch0: Vec<u8> = (0..frames).map(|i| ((i * 3 + 1) % 256) as u8).collect();
    let ch1: Vec<u8> = (0..frames).map(|i| ((i * 5 + 2) % 256) as u8).collect();
    write_bytes(&path, &build_dff(2_822_400, 2, b"DSD ", &ch0, Some(&ch1)));

    let mut reader = DsdReader::open(&path).expect("open DFF");
    assert_eq!(reader.rate(), DsdRate::Dsd64);
    assert_eq!(reader.channels(), 2);
    assert!(reader.is_lsb_first());
    assert_eq!(reader.format_info().container, "DFF");
    assert_eq!(reader.total_dsd_frames(), frames);

    // Reads of 3000 frames straddle the 4096-sample block boundary, which
    // exercises the mid-block split (per-channel contiguity) logic.
    let mut got_frames = 0u64;
    let mut splits = 0usize;
    while let Some(block) = reader.read_dsd_block(3000).expect("read block") {
        assert!(block.frames <= 3000);
        let right = block.right().expect("stereo");
        let start = got_frames as usize;
        let end = start + block.frames as usize;
        assert_eq!(block.left(), &ch0[start..end], "left mismatch at {start}");
        assert_eq!(right, &ch1[start..end], "right mismatch at {start}");
        got_frames += block.frames as u64;
        if block.frames as u64 != 3000 && block.frames as u64 != frames - 4096 {
            splits += 1;
        }
    }
    assert_eq!(got_frames, frames);
    // Expect exactly the 4096-boundary split (and the final partial block).
    assert!(splits >= 1, "block-boundary split should have occurred");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dff_dst_rejected() {
    let path = temp_path("dff");
    write_bytes(
        &path,
        &build_dff(2_822_400, 2, b"DST ", &[0u8; 64], Some(&[0u8; 64])),
    );
    assert!(matches!(
        DsdReader::open(&path),
        Err(DsdError::UnsupportedCompression(_))
    ));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_dff_unsupported_channels() {
    let path = temp_path("dff");
    write_bytes(
        &path,
        &build_dff(2_822_400, 6, b"DSD ", &[0u8; 64], Some(&[0u8; 64])),
    );
    match DsdReader::open(&path) {
        Err(DsdError::UnsupportedChannels(6)) => {}
        Err(other) => panic!("expected UnsupportedChannels(6), got {other}"),
        Ok(_) => panic!("6-channel DFF should have been rejected"),
    }
    let _ = std::fs::remove_file(&path);
}

fn response_at(decimator: &DsdToPcmDecimator, frequency_hz: f32) -> f32 {
    let mut magnitude = 1.0f32;
    let input_rate = 2_822_400.0f32;
    for (stage_index, stage) in decimator.stages[0].iter().enumerate() {
        let stage_rate = input_rate / (1u32 << stage_index) as f32;
        let omega = 2.0 * std::f32::consts::PI * frequency_hz / stage_rate;
        let mut re = 0.0f32;
        let mut im = 0.0f32;
        for (tap, coefficient) in stage.coeffs.iter().enumerate() {
            let phase = omega * tap as f32;
            re += coefficient * phase.cos();
            im -= coefficient * phase.sin();
        }
        magnitude *= (re * re + im * im).sqrt();
    }
    magnitude
}

/// Complex response of the full cascade at an absolute frequency, given
/// the DSD input rate. The cascade is rate-scalable: the same five stages
/// applied to DSD64 / DSD128 produce identical *normalized* responses.
fn complex_response_at(
    decimator: &DsdToPcmDecimator,
    input_rate: f32,
    frequency_hz: f32,
) -> (f32, f32) {
    let mut cascade_re = 1.0f32;
    let mut cascade_im = 0.0f32;
    for (stage_index, stage) in decimator.stages[0].iter().enumerate() {
        let stage_rate = input_rate / (1u32 << stage_index) as f32;
        let omega = 2.0 * std::f32::consts::PI * frequency_hz / stage_rate;
        let mut re = 0.0f32;
        let mut im = 0.0f32;
        for (tap, coefficient) in stage.coeffs.iter().enumerate() {
            let phase = omega * tap as f32;
            re += coefficient * phase.cos();
            im -= coefficient * phase.sin();
        }
        let new_re = cascade_re * re - cascade_im * im;
        let new_im = cascade_re * im + cascade_im * re;
        cascade_re = new_re;
        cascade_im = new_im;
    }
    (cascade_re, cascade_im)
}

fn gain_db(re: f32, im: f32) -> f32 {
    20.0 * (re * re + im * im).sqrt().log10()
}

#[test]
fn test_dsd_rate_table_covers_supported_rates() {
    for (hz, expected) in [
        (2_822_400, DsdRate::Dsd64),
        (5_644_800, DsdRate::Dsd128),
        (11_289_600, DsdRate::Dsd256),
        (22_579_200, DsdRate::Dsd512),
        (45_158_400, DsdRate::Dsd1024),
    ] {
        assert_eq!(DsdRate::from_hz(hz), Some(expected));
        assert_eq!(expected.sample_rate_hz(), hz);
    }
    assert_eq!(DsdRate::from_hz(48_000), None);
}

#[test]
fn test_dsd_filter_has_unity_dc_and_symmetric_impulse_response() {
    let decimator = DsdToPcmDecimator::new_32x();
    for stage in &decimator.stages[0] {
        let dc: f32 = stage.coeffs.iter().sum();
        assert!((dc - 1.0).abs() < 1e-5, "stage DC gain was {dc}");
        for i in 0..stage.coeffs.len() / 2 {
            let mirror = stage.coeffs.len() - 1 - i;
            assert!(
                (stage.coeffs[i] - stage.coeffs[mirror]).abs() < 1e-6,
                "stage impulse response is not symmetric at {i}"
            );
        }
    }

    let mut stage = FirStage::new();
    let mut impulse = vec![0.0f32; 256];
    impulse[31] = 1.0;
    let mut filtered = Vec::new();
    stage.process(&impulse, &mut filtered);
    assert!(filtered.iter().all(|sample| sample.is_finite()));
    assert!(filtered.iter().any(|sample| sample.abs() > 1e-5));
}

#[test]
fn test_dsd_filter_passband_and_ultrasonic_rejection() {
    let decimator = DsdToPcmDecimator::new_32x();
    let passband = response_at(&decimator, 20_000.0);
    let stopband = response_at(&decimator, 100_000.0);
    assert!(passband > 0.90, "20 kHz passband gain was {passband}");
    assert!(stopband < 0.01, "100 kHz rejection was only {stopband}");
}

#[test]
fn test_dsd_cascade_quantitative_response() {
    let grid = [
        20.0f32, 1_000.0, 10_000.0, 20_000.0, 30_000.0, 40_000.0, 50_000.0, 100_000.0,
    ];
    let dsd64 = DsdToPcmDecimator::new_32x();

    // DC gain must be exactly unity (0 dB) with zero phase.
    let (dc_re, dc_im) = complex_response_at(&dsd64, 2_822_400.0, 0.0);
    assert!(
        (dc_re - 1.0).abs() < 1e-5 && dc_im.abs() < 1e-6,
        "DC response must be 1.0 + 0i, got {dc_re} + {dc_im}i"
    );

    let mut measured = Vec::new();
    for &freq in &grid {
        let (re, im) = complex_response_at(&dsd64, 2_822_400.0, freq);
        measured.push((freq, gain_db(re, im)));
    }

    // Passband ripple: everything through 20 kHz stays within ±0.1 dB.
    for (freq, db) in &measured {
        if *freq <= 20_000.0 {
            assert!(
                db.abs() < 0.1,
                "DSD64: {freq} Hz passband gain {db:.4} dB outside ±0.1 dB"
            );
        }
    }

    let db20 = measured[3].1;
    let db30 = measured[4].1;
    let db40 = measured[5].1;
    let db50 = measured[6].1;
    let db100 = measured[7].1;
    assert!(
        db20 > -1.0,
        "DSD64: 20 kHz gain {db20:.4} dB is too attenuated"
    );
    assert!(
        db100 < -40.0,
        "DSD64: 100 kHz rejection only {db100:.2} dB, want < -40 dB"
    );
    assert!(
        db40 < db30 && db50 < db40,
        "DSD64: rejection must deepen through the transition band ({db30:.2}, {db40:.2}, {db50:.2} dB)"
    );

    // Linear-phase cascade => constant group delay of 961 input samples,
    // i.e. 961 / 32 = 30.03125 samples at the 32× output rate.
    let gd_samples_at = |dec: &DsdToPcmDecimator, input_rate: f32, f: f32| -> f32 {
        let df = 10.0;
        let (r1, i1) = complex_response_at(dec, input_rate, f);
        let (r2, i2) = complex_response_at(dec, input_rate, f + df);
        let dphase = i2.atan2(r2) - i1.atan2(r1);
        (-dphase / (2.0 * std::f32::consts::PI * df)) * (input_rate / 32.0)
    };
    let gd = gd_samples_at(&dsd64, 2_822_400.0, 1_000.0);
    assert!(
        (gd - 30.03125).abs() < 0.05,
        "DSD64: group delay {gd:.5} samples, expected 30.03125"
    );

    // DSD128 reuses the same five stages at double the input rate, so the
    // response is rate-scalable: |H_128(2f)| == |H_64(f)|.
    let dsd128 = DsdToPcmDecimator::new_32x();
    for &freq in &grid {
        let (r64, i64_) = complex_response_at(&dsd64, 2_822_400.0, freq);
        let (r128, i128) = complex_response_at(&dsd128, 5_644_800.0, freq * 2.0);
        let db64 = gain_db(r64, i64_);
        let db128 = gain_db(r128, i128);
        assert!(
            (db64 - db128).abs() < 0.05,
            "DSD128: {freq} Hz -> {} Hz response mismatch ({db64:.4} vs {db128:.4} dB)",
            freq * 2.0
        );
    }
    let gd128 = gd_samples_at(&dsd128, 5_644_800.0, 2_000.0);
    assert!(
        (gd128 - 30.03125).abs() < 0.05,
        "DSD128: group delay {gd128:.5} samples, expected 30.03125"
    );

    // Emit the reference table (visible with `cargo test -- --nocapture`).
    println!("DSD64 cascade response (32x -> 88.2 kHz):");
    for (freq, db) in &measured {
        println!("  {freq:>6} Hz: {db:>8.3} dB");
    }
    println!("  group delay: {gd:.5} output samples");
}

#[test]
fn test_dsd_decimator_impulse_path_is_finite_and_bounded() {
    let mut bytes = vec![0u8; 256];
    bytes[0] = 0x01; // one positive DSD bit followed by negative bits
    let mut decimator = DsdToPcmDecimator::new_32x();
    let mut left = Vec::new();
    let mut right = Vec::new();
    decimator.decimate_channels(&[&bytes, &bytes], true, &mut [&mut left, &mut right]);
    assert!(!left.is_empty());
    assert_eq!(left.len(), right.len());
    assert!(left
        .iter()
        .all(|sample| sample.is_finite() && sample.abs() <= 1.0));
    assert!(left.iter().any(|sample| sample.abs() > 1e-5));
}

// ── Native DSD wire-format packing (§7) ─────────────────────────────────────

#[test]
fn test_native_dsd_packer_u8_stereo_interleave() {
    // Stereo DSD_U8: one byte per channel per 8-DSD-sample group,
    // channel-interleaved. ch0 = [0x01, 0x02, 0x03], ch1 = [0xA1, 0xA2, 0xA3].
    let ch0: Vec<u8> = vec![0x01, 0x02, 0x03];
    let ch1: Vec<u8> = vec![0xA1, 0xA2, 0xA3];
    let mut out = Vec::new();
    let words = NativeDsdPacker::pack(DsdWireFormat::U8, &[&ch0, &ch1], &mut out);
    assert_eq!(words, 3);
    assert_eq!(out, vec![0x01, 0xA1, 0x02, 0xA2, 0x03, 0xA3]);
}

#[test]
fn test_native_dsd_packer_u16_endianness() {
    // Stereo DSD_U16: each 16-bit word = 2 payload bytes (16 DSD samples).
    // ch0 word = [0x01, 0x02]; ch1 word = [0x11, 0x12].
    let ch0: Vec<u8> = vec![0x01, 0x02];
    let ch1: Vec<u8> = vec![0x11, 0x12];
    let mut out = Vec::new();
    let words = NativeDsdPacker::pack(DsdWireFormat::U16Le, &[&ch0, &ch1], &mut out);
    assert_eq!(words, 1);
    // LE: low byte first.
    assert_eq!(out, vec![0x01, 0x02, 0x11, 0x12]);

    out.clear();
    let words = NativeDsdPacker::pack(DsdWireFormat::U16Be, &[&ch0, &ch1], &mut out);
    assert_eq!(words, 1);
    // BE: high byte first.
    assert_eq!(out, vec![0x02, 0x01, 0x12, 0x11]);
}

#[test]
fn test_native_dsd_packer_u32_layouts() {
    // Stereo DSD_U32: each 32-bit word = 4 payload bytes (32 DSD samples).
    let ch0: Vec<u8> = vec![0x01, 0x02, 0x03, 0x04];
    let ch1: Vec<u8> = vec![0x11, 0x12, 0x13, 0x14];
    let mut out = Vec::new();
    let words = NativeDsdPacker::pack(DsdWireFormat::U32Le, &[&ch0, &ch1], &mut out);
    assert_eq!(words, 1);
    assert_eq!(out, vec![0x01, 0x02, 0x03, 0x04, 0x11, 0x12, 0x13, 0x14]);

    out.clear();
    let words = NativeDsdPacker::pack(DsdWireFormat::U32Be, &[&ch0, &ch1], &mut out);
    assert_eq!(words, 1);
    assert_eq!(out, vec![0x04, 0x03, 0x02, 0x01, 0x14, 0x13, 0x12, 0x11]);
}

#[test]
fn test_native_dsd_packer_clamps_to_whole_words() {
    // ch0 has 5 bytes, ch1 has 4: only 2 words (4 bytes) can be packed for
    // DSD_U16 (floor(5/2)=2, floor(4/2)=2).
    let ch0: Vec<u8> = vec![0x01, 0x02, 0x03, 0x04, 0x05];
    let ch1: Vec<u8> = vec![0x11, 0x12, 0x13, 0x14];
    let mut out = Vec::new();
    let words = NativeDsdPacker::pack(DsdWireFormat::U16Le, &[&ch0, &ch1], &mut out);
    assert_eq!(words, 2);
    assert_eq!(out.len(), 2 * 2 * 2); // 2 words × 2 channels × 2 bytes
                                      // No channel may be ahead of another in the wire stream.
    assert_eq!(out, vec![0x01, 0x02, 0x11, 0x12, 0x03, 0x04, 0x13, 0x14]);
}

#[test]
fn test_dsd_wire_format_frame_rates() {
    // ALSA frame rate = bit rate / samples per word.
    assert_eq!(DsdWireFormat::U8.frame_rate_hz(2_822_400), 352_800);
    assert_eq!(DsdWireFormat::U16Le.frame_rate_hz(2_822_400), 176_400);
    assert_eq!(DsdWireFormat::U32Le.frame_rate_hz(2_822_400), 88_200);
    assert_eq!(DsdWireFormat::U8.bytes_per_word(), 1);
    assert_eq!(DsdWireFormat::U32Be.bytes_per_word(), 4);
}

// ── Native DSD wire-format packer (§7 acceptance: byte packing) ────────

#[test]
fn test_native_pack_dsd_u8_interleaves_channels() {
    // Two channels, 2 bytes each. DSD_U8 wire = [ch0 b0][ch1 b0][ch0 b1][ch1 b1].
    let ch0 = vec![0xAA, 0x55];
    let ch1 = vec![0xF0, 0x0F];
    let refs = [ch0.as_slice(), ch1.as_slice()];
    let mut out = Vec::new();
    let words = NativeDsdPacker::pack(DsdWireFormat::U8, &refs, &mut out);
    assert_eq!(words, 2);
    assert_eq!(out, vec![0xAA, 0xF0, 0x55, 0x0F]);
}

#[test]
fn test_native_pack_dsd_u16_le_be() {
    let ch0 = vec![0xAB, 0xCD];
    let ch1 = vec![0x12, 0x34];
    let refs = [ch0.as_slice(), ch1.as_slice()];
    let mut out = Vec::new();
    // U16 LE: [ch0 b0 b1][ch1 b0 b1]
    let words = NativeDsdPacker::pack(DsdWireFormat::U16Le, &refs, &mut out);
    assert_eq!(words, 1);
    assert_eq!(out, vec![0xAB, 0xCD, 0x12, 0x34]);
    // U16 BE: bytes swapped per word: [ch0 b1 b0][ch1 b1 b0]
    out.clear();
    let words = NativeDsdPacker::pack(DsdWireFormat::U16Be, &refs, &mut out);
    assert_eq!(words, 1);
    assert_eq!(out, vec![0xCD, 0xAB, 0x34, 0x12]);
}

#[test]
fn test_native_pack_dsd_u32_endianness() {
    let ch0 = vec![0x00, 0x11, 0x22, 0x33];
    let ch1 = vec![0xAA, 0xBB, 0xCC, 0xDD];
    let refs = [ch0.as_slice(), ch1.as_slice()];
    let mut out = Vec::new();
    let words = NativeDsdPacker::pack(DsdWireFormat::U32Le, &refs, &mut out);
    assert_eq!(words, 1);
    assert_eq!(out, vec![0x00, 0x11, 0x22, 0x33, 0xAA, 0xBB, 0xCC, 0xDD]);
    out.clear();
    let words = NativeDsdPacker::pack(DsdWireFormat::U32Be, &refs, &mut out);
    assert_eq!(words, 1);
    assert_eq!(out, vec![0x33, 0x22, 0x11, 0x00, 0xDD, 0xCC, 0xBB, 0xAA]);
}

#[test]
fn test_native_pack_shortest_channel_wins_and_multiple_words() {
    // Channel 1 has only one full word: packing must not panic on the
    // shorter channel and must produce exactly one word per channel.
    let ch0 = vec![0x01, 0x02, 0x03, 0x04];
    let ch1 = vec![0x10, 0x20];
    let refs = [ch0.as_slice(), ch1.as_slice()];
    let mut out = Vec::new();
    let words = NativeDsdPacker::pack(DsdWireFormat::U16Le, &refs, &mut out);
    assert_eq!(words, 1);
    assert_eq!(out, vec![0x01, 0x02, 0x10, 0x20]);
    // Multi-word: 2 words per channel interleave [w0 ch0][w0 ch1][w1 ch0][w1 ch1].
    let ch0 = vec![0x01, 0x02, 0x03, 0x04];
    let ch1 = vec![0x05, 0x06, 0x07, 0x08];
    let refs = [ch0.as_slice(), ch1.as_slice()];
    let mut out = Vec::new();
    let words = NativeDsdPacker::pack(DsdWireFormat::U8, &refs, &mut out);
    assert_eq!(words, 4);
    assert_eq!(out, vec![0x01, 0x05, 0x02, 0x06, 0x03, 0x07, 0x04, 0x08]);
}

#[test]
fn test_native_frame_rates_match_alsa_dsd_contract() {
    // DSD64 = 2.8224 MHz.
    let rate = 2_822_400u32;
    // SND_PCM_FORMAT_DSD_U8: 1 byte = 8 samples -> 352.8 kHz frame rate.
    assert_eq!(DsdWireFormat::U8.frame_rate_hz(rate), 352_800);
    // SND_PCM_FORMAT_DSD_U16: 2 bytes = 16 samples -> 176.4 kHz.
    assert_eq!(DsdWireFormat::U16Le.frame_rate_hz(rate), 176_400);
    assert_eq!(DsdWireFormat::U16Be.frame_rate_hz(rate), 176_400);
    // SND_PCM_FORMAT_DSD_U32: 4 bytes = 32 samples -> 88.2 kHz.
    assert_eq!(DsdWireFormat::U32Le.frame_rate_hz(rate), 88_200);
    assert_eq!(DsdWireFormat::U32Be.frame_rate_hz(rate), 88_200);
}
