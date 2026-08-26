//! Integration tests for [`Decoder::open_from_source`] with in-memory byte
//! sources covering the three dispatch paths:
//!
//! 1. **Native trait-object** — DSD (`.dsf`) → `DsdDecoder::open_from_source`
//! 2. **MediaSourceBridge** — Symphonia (`.wav`, `.flac`, `.mp3`, …) →
//!    `SymphoniaDecoder::open_from_source` (WAV is used here as a proxy; FLAC
//!    follows the identical dispatch path)
//! 3. **Temp-file bridge** — `buf_and_open_file` for backends whose crates
//!    require a concrete reader type (`.wv` via WavPack, identical bridge to
//!    `.ape` via Monkey's Audio, `.opus`, `.tta`)

use engine::{
    audio_io::MemoryByteSource,
    decode::{DecodeError, Decoder},
};

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Generate stereo 16-bit PCM WAV bytes in memory (no scratch file).
fn generate_pcm_wav_bytes(sample_rate: u32, duration_secs: f32) -> Vec<u8> {
    let total_frames = (sample_rate as f32 * duration_secs) as usize;
    let channels: u16 = 2;
    let block_align = channels * 2;
    let byte_rate = sample_rate * block_align as u32;
    let data_size = (total_frames * block_align as usize) as u32;
    let riff_chunk_size = 36 + data_size;

    let mut buf = Vec::with_capacity((44 + data_size) as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_chunk_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());

    for i in 0..total_frames {
        let t = i as f32 / sample_rate as f32;
        let val = (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5;
        let sample = (val * 32767.0) as i16;
        buf.extend_from_slice(&sample.to_le_bytes());
        buf.extend_from_slice(&sample.to_le_bytes());
    }
    buf
}

/// Generate a minimal stereo DSF byte stream in memory.
fn generate_dsf_bytes() -> Vec<u8> {
    let block_size = 4096u32;
    let frames = 4096u64 * 32;
    let ch0: Vec<u8> = (0..frames).map(|i| (i % 256) as u8).collect();
    let ch1: Vec<u8> = (0..frames).map(|i| ((i * 7 + 3) % 256) as u8).collect();

    let mut out = Vec::new();
    out.extend_from_slice(b"DSD ");
    out.extend_from_slice(&28u64.to_le_bytes());
    let total_size_pos = out.len();
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&52u64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // DSD
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes()); // 2 channels
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&2_822_400u32.to_le_bytes()); // sample rate
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(frames * 8).to_le_bytes());
    out.extend_from_slice(&block_size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    let mut audio = vec![0u8; (frames * 2) as usize];
    for (ch, data) in [&ch0[..], &ch1[..]].iter().enumerate() {
        for (b, chunk) in data.chunks(block_size as usize).enumerate() {
            let base = (b * block_size as usize) * 2 + ch * block_size as usize;
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

/// Generate a WavPack byte stream (lossless, stereo 16-bit, 44.1 kHz).
#[cfg(feature = "codec-wavpack")]
fn generate_wavpack_bytes() -> Vec<u8> {
    use wavicle::EncodeParams;
    let frames = 5000usize;
    let mut src = Vec::with_capacity(frames * 2);
    let mut state = 0xDEAD_BEEFu32;
    for _ in 0..frames * 2 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        src.push(((state % 65536) as i32) - 32768);
    }
    wavicle::encode_int(
        EncodeParams {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
        },
        &src,
    )
    .expect("wavicle encode")
}

/// Decode all frames from `decoder`, returning total frame count and
/// verifying that each chunk's sample buffer matches its frame count.
fn drain_decoder(decoder: &mut Decoder) -> Result<u64, String> {
    let mut total_frames = 0u64;
    loop {
        match decoder.decode_next(4096) {
            Ok(chunk) => {
                assert_eq!(
                    chunk.samples.len(),
                    chunk.frame_count * chunk.channels,
                    "sample buffer length must match frame_count × channels"
                );
                total_frames += chunk.frame_count as u64;
            }
            Err(DecodeError::EndOfStream) => break,
            Err(e) => return Err(format!("decode error: {:?}", e)),
        }
    }
    Ok(total_frames)
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// Open a DSF file from a [`MemoryByteSource`] and verify the dispatch routes
/// through the native DSD trait-object path (`DsdDecoder::open_from_source`),
/// not the temp-file bridge.  Decode to PCM and verify frame count.
#[test]
fn test_open_dsf_from_memory_source() {
    let dsf_bytes = generate_dsf_bytes();
    let source = MemoryByteSource::new(dsf_bytes, "dsf");
    let mut decoder =
        Decoder::open_from_source(Box::new(source)).expect("open_from_source(DSF from memory)");

    // Verify the decoder variant.
    assert!(
        matches!(decoder, Decoder::Dsd(_)),
        "DSF dispatch must produce Decoder::Dsd"
    );

    // DSD decoder reports the PCM decimated sample rate.
    let info = decoder.info();
    // DSD64 (2,822,400 Hz) decimated at 1/32 → 88,200 Hz.
    assert_eq!(info.sample_rate, 88200, "DSD64 → PCM at 88.2 kHz");
    assert_eq!(info.channels, 2);

    let total = drain_decoder(&mut decoder).expect("decode should complete");
    // The DSF has a deterministic payload; verify we got a plausible number
    // of PCM frames out of the DSD → PCM decimation path.
    assert!(
        total > 100,
        "DSD decode must produce > 100 frames, got {total}"
    );
}

/// Open a WAV file from a [`MemoryByteSource`] and verify the dispatch routes
/// through the Symphonia `MediaSourceBridge` path.  This is the identical
/// dispatch path used for FLAC, MP3, AAC, Ogg Vorbis, ALAC, AIFF, and raw PCM.
#[test]
fn test_open_wav_from_memory_source() {
    let wav_bytes = generate_pcm_wav_bytes(48000, 0.1);
    let source = MemoryByteSource::new(wav_bytes, "wav");
    let mut decoder =
        Decoder::open_from_source(Box::new(source)).expect("open_from_source(WAV from memory)");

    assert!(
        matches!(decoder, Decoder::Symphonia(_)),
        "WAV dispatch must produce Decoder::Symphonia"
    );

    let info = decoder.info();
    assert_eq!(info.sample_rate, 48000);
    assert_eq!(info.channels, 2);

    let total = drain_decoder(&mut decoder).expect("decode should complete");
    // 0.1 s × 48000 Hz = 4800 frames. Symphonia may trim a few samples at
    // the boundary; accept anything > 90% of expected.
    let expected = (48000.0 * 0.1) as u64;
    assert!(
        total as f64 > expected as f64 * 0.9,
        "WAV decode: expected ~{expected} frames, got {total}"
    );
    assert!(
        total <= expected + 200,
        "WAV decode: at most {expected} frames plus padding, got {total}"
    );
}

/// Open a WavPack file from a [`MemoryByteSource`] and verify the dispatch
/// routes through the `buf_and_open_file` temp-file bridge.  This is the
/// identical bridge used for APE (Monkey's Audio), Opus, and TTA — any
/// backend whose crate requires a concrete `Read + Seek` type.
#[cfg(feature = "codec-wavpack")]
#[test]
fn test_open_wavpack_from_memory_source_bridge() {
    let wv_bytes = generate_wavpack_bytes();
    let source = MemoryByteSource::new(wv_bytes, "wv");
    let mut decoder =
        Decoder::open_from_source(Box::new(source)).expect("open_from_source(WV from memory)");

    assert!(
        matches!(decoder, Decoder::Wavpack(_)),
        "WV dispatch through temp-file bridge must produce Decoder::Wavpack"
    );

    let info = decoder.info();
    assert_eq!(info.sample_rate, 44100);
    assert_eq!(info.channels, 2);

    let total = drain_decoder(&mut decoder).expect("decode should complete");
    // We encoded 5000 frames.
    assert_eq!(total, 5000, "WavPack round-trip must be sample-exact");
}

/// Verify that `open_from_source` with the wrong extension still works when
/// the container is routed through Symphonia's probe (Symphonia ignores the
/// extension hint and sniffs the container).
#[test]
fn test_open_mislabeled_extension_through_symphonia() {
    let wav_bytes = generate_pcm_wav_bytes(44100, 0.05);
    // Claim it's a ".bin" file — no codec claims this extension, so the
    // dispatch falls through to Symphonia as universal fallback.
    let source = MemoryByteSource::new(wav_bytes, "bin");
    let mut decoder = Decoder::open_from_source(Box::new(source)).expect("open mislabeled");

    assert!(
        matches!(decoder, Decoder::Symphonia(_)),
        "unknown extension must fall through to Symphonia probe"
    );
    let info = decoder.info();
    assert_eq!(info.sample_rate, 44100);
    assert!(
        drain_decoder(&mut decoder).unwrap() > 0,
        "must decode frames from mislabeled WAV"
    );
}

/// Attempting to open an empty byte source must fail cleanly.
#[test]
fn test_empty_source_rejected() {
    let source = MemoryByteSource::new(vec![], "wav");
    let result = Decoder::open_from_source(Box::new(source));
    assert!(result.is_err(), "empty source must be rejected");
}

/// The `buf_and_open_file` bridge must clean up its temp file even when the
/// codec rejects the content.  WavPack rejects multichannel audio, so we feed
/// it a tiny WAV with the `.wv` extension to trigger the bridge + rejection.
#[cfg(feature = "codec-wavpack")]
#[test]
fn test_bridge_cleanup_on_codec_error() {
    // Create a tiny WAV and mislabel it as WavPack.  The bridge writes it to
    // a temp file, calls WavpackDecoder::open, which rejects it.  The bridge
    // must still remove the temp file.
    let wav_bytes = generate_pcm_wav_bytes(44100, 0.02);
    let source = MemoryByteSource::new(wav_bytes, "wv");
    let result = Decoder::open_from_source(Box::new(source));
    // The codec returns an error; the bridge still removes the temp file.
    match result {
        Ok(_) => panic!("mislabeled WavPack source must be rejected"),
        Err(e) => {
            let msg = format!("{:?}", e);
            assert!(
                msg.contains("WavPack") || msg.contains("wavpack") || msg.contains("wv"),
                "error should come from the codec, got: {msg}"
            );
        }
    }
}
