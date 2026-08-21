//! Tests and validation for the TTA decoder.

use super::bitstream::BitReaderLsb;
use super::crc::crc32;
use super::decoder::{TtaDecoder, HEADER_SIZE, TTA1_MAGIC};
use super::filter::{pred, shift_1, shift_16, zigzag_decode, TtaFilter};
use super::rice::TtaRice;
use crate::decode::DecodeError;

// ── CRC-32 conformance ───────────────────────────────────────────────

#[test]
fn crc32_matches_standard_check_value() {
    // The universal CRC-32 check value for the ASCII string "123456789".
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0x0000_0000);
}

// ── Hand-traced reference vectors ────────────────────────────────────

#[test]
fn zigzag_decode_reference_mapping() {
    assert_eq!(zigzag_decode(0), 0);
    assert_eq!(zigzag_decode(1), 1);
    assert_eq!(zigzag_decode(2), -1);
    assert_eq!(zigzag_decode(3), 2);
    assert_eq!(zigzag_decode(4), -2);
    assert_eq!(zigzag_decode(5), 3);
}

#[test]
fn pred_matches_reference_macro() {
    // PRED(x,k) = (int32_t)((((uint64_t)(x) << k) - x) >> k)
    assert_eq!(pred(-1, 4), -1);
    assert_eq!(pred(100, 5), 96); // floor(100·31/32)
    assert_eq!(pred(-33, 5), -32);
    assert_eq!(pred(0, 4), 0);
    assert_eq!(pred(16, 4), 15); // 16·15/16
}

#[test]
fn filter_process_hand_traced_steps() {
    let mut f = TtaFilter {
        qm: [0; 8],
        dx: [0; 8],
        dl: [0; 8],
        error: 0,
        shift: 10,
        round: 512,
    };

    assert_eq!(f.process(100), 100);
    assert_eq!(&f.dl[4..], &[100, 100, 100, 100]);
    assert_eq!(&f.dx[4..], &[1, 2, 2, 4]);

    assert_eq!(f.process(-50), -49);
    assert_eq!(f.error, -50);
    assert_eq!(&f.qm[4..], &[1, 2, 2, 4]);
    assert_eq!(&f.dl[4..], &[-349, -249, -149, -49]);
}

#[test]
fn rice_adaptation_k_drop_and_growth_trace() {
    let mut r = TtaRice {
        k0: 10,
        k1: 10,
        sum0: shift_16(10),
        sum1: shift_16(10),
    };
    assert_eq!(r.k0, 10);

    r.adapt(0, 0);
    assert_eq!(r.k0, 9);
    for _ in 0..9 {
        r.adapt(0, 0);
    }
    assert_eq!(r.k0, 9);
    r.adapt(0, 0);
    assert_eq!(r.k0, 8);

    let mut r2 = TtaRice {
        k0: 10,
        k1: 10,
        sum0: shift_16(10),
        sum1: shift_16(10),
    };
    let mut seen = vec![r2.k0];
    for _ in 0..6 {
        r2.adapt(100_000, 0);
        seen.push(r2.k0);
    }
    assert_eq!(seen, vec![10, 11, 12, 13, 14, 14, 15]);
}

#[test]
fn bit_reader_lsb_order() {
    let mut br = BitReaderLsb::new(&[0b1010_0101, 0xFF]);
    assert_eq!(br.read_bit(), Some(1));
    assert_eq!(br.read_bit(), Some(0));
    assert_eq!(br.read_bit(), Some(1));
    assert_eq!(br.read_bit(), Some(0));
    assert_eq!(br.read_bits(3), Some(0b010));
    let mut br2 = BitReaderLsb::new(&[0b0000_0011]);
    assert_eq!(br2.read_unary(64), 2);
}

#[test]
fn rice_round_trip_through_reader() {
    let values: Vec<i32> = vec![0, 1, -1, 2, -2, 100, -100, 5000, -5000, 1 << 20];
    let mut enc_rice = TtaRice {
        k0: 10,
        k1: 10,
        sum0: shift_16(10),
        sum1: shift_16(10),
    };
    let mut enc_snapshots: Vec<(u32, u32)> = Vec::new();
    let bits: Vec<u8> = {
        let mut w = BitWriterLsb::default();
        for &v in &values {
            encode_rice_symbol(&mut w, &mut enc_rice, zigzag_encode(v));
            enc_snapshots.push((enc_rice.k0, enc_rice.k1));
        }
        w.finish()
    };
    let mut dec_rice = TtaRice {
        k0: 10,
        k1: 10,
        sum0: shift_16(10),
        sum1: shift_16(10),
    };
    let mut br = BitReaderLsb::new(&bits);
    for (i, &v) in values.iter().enumerate() {
        let decoded = zigzag_decode(dec_rice.decode(&mut br).unwrap());
        assert_eq!(decoded, v, "round trip mismatch for {v}");
        assert_eq!(dec_rice.k0, enc_snapshots[i].0, "k0 diverged at symbol {i}");
        assert_eq!(dec_rice.k1, enc_snapshots[i].1, "k1 diverged at symbol {i}");
    }
}

// ── Test-only TTA encoder (mirrors the decoder's inverse) ────────────

#[derive(Default)]
pub(crate) struct BitWriterLsb {
    bytes: Vec<u8>,
    bit_pos: usize,
}

impl BitWriterLsb {
    pub(crate) fn write_bit(&mut self, bit: u32) {
        if self.bit_pos % 8 == 0 {
            self.bytes.push(0);
        }
        if bit != 0 {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (self.bit_pos % 8);
        }
        self.bit_pos += 1;
    }

    pub(crate) fn write_bits(&mut self, value: u32, n: u32) {
        for i in 0..n {
            self.write_bit((value >> i) & 1);
        }
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        while self.bit_pos % 8 != 0 {
            self.write_bit(0);
        }
        self.bytes
    }
}

pub(crate) fn zigzag_encode(s: i32) -> u32 {
    if s > 0 {
        ((s as u32) << 1).wrapping_sub(1)
    } else {
        (s.wrapping_neg() as u32) << 1
    }
}

pub(crate) fn encode_rice_symbol(w: &mut BitWriterLsb, rice: &mut TtaRice, u: u32) {
    let threshold = shift_1(rice.k0);
    if u < threshold && rice.k0 < 31 {
        w.write_bit(0);
        w.write_bits(u, rice.k0);
        rice.adapt(u as i32, 0);
    } else {
        debug_assert!(u >= threshold || rice.k0 >= 31);
        let rem = u.wrapping_sub(shift_1(rice.k0));
        let unary = rem >> rice.k1;
        let low = rem & (shift_1(rice.k1).wrapping_sub(1));
        for _ in 0..=unary {
            w.write_bit(1);
        }
        w.write_bit(0);
        w.write_bits(low, rice.k1);
        let pre = ((unary << rice.k1) as u32).wrapping_add(low) as i32;
        rice.adapt(pre, 1);
    }
}

pub(crate) struct EncChannel {
    pub(crate) predictor: i32,
    pub(crate) filter: TtaFilter,
    pub(crate) rice: TtaRice,
}

impl EncChannel {
    pub(crate) fn new(bits_per_sample: u16) -> Self {
        let shift = match bits_per_sample.div_ceil(8) {
            1 => 10,
            2 => 9,
            _ => 10,
        };
        let mut filter = TtaFilter {
            qm: [0; 8],
            dx: [0; 8],
            dl: [0; 8],
            error: 0,
            shift,
            round: 0,
        };
        filter.init(shift);
        Self {
            predictor: 0,
            filter,
            rice: TtaRice {
                k0: 10,
                k1: 10,
                sum0: shift_16(10),
                sum1: shift_16(10),
            },
        }
    }
}

pub(crate) fn encode_frame(
    channels: usize,
    bits_per_sample: u16,
    interleaved: &[i32],
    states: &mut [EncChannel],
) -> Vec<u8> {
    for st in states.iter_mut() {
        st.predictor = 0;
        st.filter.init(st.filter.shift);
        st.rice.init();
    }

    let n = channels;
    let k_pred = if (bits_per_sample + 7) / 8 == 1 { 4 } else { 5 };
    let mut w = BitWriterLsb::default();

    for slot in 0..interleaved.len() / n {
        let base = slot * n;
        let mut coder_inputs = vec![0i32; n];
        for j in 0..n - 1 {
            coder_inputs[j] = interleaved[base + j + 1].wrapping_sub(interleaved[base + j]);
        }
        if n > 1 {
            coder_inputs[n - 1] =
                interleaved[base + n - 1].wrapping_sub(coder_inputs[n - 2] / 2);
        } else {
            coder_inputs[0] = interleaved[base];
        }

        for (j, &target) in coder_inputs.iter().enumerate() {
            let st = &mut states[j];
            let f = st.filter.peek_contribution();
            let v = target
                .wrapping_sub(pred(st.predictor, k_pred))
                .wrapping_sub(f);
            encode_rice_symbol(&mut w, &mut st.rice, zigzag_encode(v));
            let filtered = st.filter.process(v);
            let out = filtered.wrapping_add(pred(st.predictor, k_pred));
            debug_assert_eq!(out, target, "encoder inverse drifted");
            st.predictor = out;
        }
    }
    w.finish()
}

pub(crate) fn build_tta_file(
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
    data_length: u32,
    frames: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(TTA1_MAGIC);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&data_length.to_le_bytes());
    let crc = crc32(&out[..18]);
    out.extend_from_slice(&crc.to_le_bytes());

    for f in frames {
        out.extend_from_slice(&(f.len() as u32).to_le_bytes());
    }
    let table_crc = crc32(&out[HEADER_SIZE..HEADER_SIZE + 4 * frames.len()]);
    out.extend_from_slice(&table_crc.to_le_bytes());

    for f in frames {
        out.extend_from_slice(f);
    }
    out
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn sample(&mut self, amplitude: i32) -> i32 {
        (self.next() % (2 * amplitude as u64 + 1)) as i32 - amplitude
    }
}

fn temp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "engine_tta_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn write_fixture(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write tta fixture");
}

fn round_trip(channels: u16, bits: u16, rate: u32, frames_of_audio: usize, seed: u64) {
    let amplitude = match (bits + 7) / 8 {
        1 => 120,
        2 => 30_000,
        _ => 8_000_000,
    };
    let frame_len = 256u32 * rate / 245;
    let mut rng = Lcg(seed | 1);

    let mut frames_bytes = Vec::new();
    let mut expected_all: Vec<f32> = Vec::new();
    let mut enc_states: Vec<EncChannel> =
        (0..channels).map(|_| EncChannel::new(bits)).collect();
    let mut data_length = 0u32;

    for f in 0..frames_of_audio {
        let samples_in_frame = if f + 1 == frames_of_audio && frames_of_audio > 1 {
            (frame_len / 3).max(1)
        } else {
            frame_len
        };
        data_length += samples_in_frame;
        let mut interleaved = Vec::with_capacity(samples_in_frame as usize * channels as usize);
        for _ in 0..samples_in_frame {
            for _ in 0..channels {
                interleaved.push(rng.sample(amplitude));
            }
        }
        for &v in &interleaved {
            expected_all.push(match (bits + 7) / 8 {
                1 => v as f32 / 128.0,
                2 => (v as i16) as f32 / 32768.0,
                _ => v as f32 / 8_388_608.0,
            });
        }
        let payload = encode_frame(channels as usize, bits, &interleaved, &mut enc_states);
        let mut frame = payload;
        let crc = crc32(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        frames_bytes.push(frame);
    }

    let file = build_tta_file(channels, bits, rate, data_length, &frames_bytes);
    let path = temp_path("rt");
    write_fixture(&path, &file);

    let mut dec = TtaDecoder::open(&path).expect("open round-trip fixture");
    assert_eq!(dec.info().sample_rate, rate);
    assert_eq!(dec.info().channels, channels as usize);
    let expected_duration = data_length as f64 / rate as f64;
    assert!((dec.duration_secs() as f64 - expected_duration).abs() < 1e-4);

    let mut got: Vec<f32> = Vec::new();
    loop {
        match dec.decode_next(1 << 20) {
            Ok(chunk) => got.extend_from_slice(&chunk.samples),
            Err(DecodeError::EndOfStream) => break,
            Err(e) => panic!("decode failed: {e}"),
        }
    }
    assert_eq!(got.len(), expected_all.len(), "sample count mismatch");
    for (i, (g, e)) in got.iter().zip(expected_all.iter()).enumerate() {
        assert_eq!(g, e, "sample {i} differs (lossless violation)");
    }

    std::fs::remove_file(&path).ok();
}

#[test]
fn round_trip_stereo_16bit_multi_frame() {
    round_trip(2, 16, 44_100, 4, 0xDEADBEEF);
}

#[test]
fn round_trip_mono_8bit() {
    round_trip(1, 8, 48_000, 3, 0x12345678);
}

#[test]
fn round_trip_stereo_24bit() {
    round_trip(2, 24, 48_000, 3, 0xCAFEBABE);
}

#[test]
fn round_trip_six_channel_16bit() {
    round_trip(6, 16, 44_100, 3, 0xFEEDFACE);
}

#[test]
fn chunked_decoding_matches_single_call() {
    let channels = 2u16;
    let bits = 16u16;
    let rate = 44_100u32;
    let frame_len = 256u32 * rate / 245;
    let mut rng = Lcg(777);
    let mut frames_bytes = Vec::new();
    let mut enc_states: Vec<EncChannel> =
        (0..channels).map(|_| EncChannel::new(bits)).collect();
    let mut data_length = 0u32;
    for f in 0..3 {
        let n = if f == 2 { frame_len / 2 } else { frame_len };
        data_length += n;
        let mut inter = Vec::new();
        for _ in 0..n {
            for _ in 0..channels {
                inter.push(rng.sample(20_000));
            }
        }
        let payload = encode_frame(channels as usize, bits, &inter, &mut enc_states);
        let mut frame = payload;
        let crc = crc32(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        frames_bytes.push(frame);
    }
    let file = build_tta_file(channels, bits, rate, data_length, &frames_bytes);
    let path = temp_path("chunk");
    write_fixture(&path, &file);

    let collect = |max: usize| {
        let mut dec = TtaDecoder::open(&path).unwrap();
        let mut all = Vec::new();
        loop {
            match dec.decode_next(max) {
                Ok(c) => all.extend_from_slice(&c.samples),
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode: {e}"),
            }
        }
        all
    };
    let whole = collect(1 << 20);
    let one_by_one = collect(1);
    assert_eq!(whole, one_by_one, "chunked decode must be identical");

    std::fs::remove_file(&path).ok();
}

#[test]
fn seek_is_sample_accurate() {
    let channels = 2u16;
    let bits = 16u16;
    let rate = 48_000u32;
    let frame_len = 256u32 * rate / 245;
    let mut rng = Lcg(4242);
    let mut frames_bytes = Vec::new();
    let mut enc_states: Vec<EncChannel> =
        (0..channels).map(|_| EncChannel::new(bits)).collect();
    let mut data_length = 0u32;
    for f in 0..3 {
        let n = if f == 2 { frame_len / 2 } else { frame_len };
        data_length += n;
        let mut inter = Vec::new();
        for _ in 0..n {
            for _ in 0..channels {
                inter.push(rng.sample(25_000));
            }
        }
        let payload = encode_frame(channels as usize, bits, &inter, &mut enc_states);
        let mut frame = payload;
        let crc = crc32(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        frames_bytes.push(frame);
    }
    let file = build_tta_file(channels, bits, rate, data_length, &frames_bytes);
    let path = temp_path("seek");
    write_fixture(&path, &file);

    let mut reference: Vec<f32> = Vec::new();
    {
        let mut dec = TtaDecoder::open(&path).unwrap();
        loop {
            match dec.decode_next(1 << 20) {
                Ok(c) => reference.extend_from_slice(&c.samples),
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode: {e}"),
            }
        }
    }

    let target_sample = frame_len as u64 + frame_len as u64 / 2;
    let mut dec = TtaDecoder::open(&path).unwrap();
    dec.seek(target_sample as f32 / rate as f32).unwrap();
    let mut tail: Vec<f32> = Vec::new();
    loop {
        match dec.decode_next(4096) {
            Ok(c) => tail.extend_from_slice(&c.samples),
            Err(DecodeError::EndOfStream) => break,
            Err(e) => panic!("decode after seek: {e}"),
        }
    }
    let start = target_sample as usize * channels as usize;
    assert_eq!(
        tail,
        reference[start..],
        "post-seek samples must match the reference stream"
    );

    std::fs::remove_file(&path).ok();
}

fn base_header(channels: u16, bits: u16, rate: u32, length: u32, format: u16) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(TTA1_MAGIC);
    h.extend_from_slice(&format.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&bits.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&length.to_le_bytes());
    let crc = crc32(&h[..18]);
    h.extend_from_slice(&crc.to_le_bytes());
    h
}

#[test]
fn garbage_bytes_are_rejected() {
    let path = temp_path("garbage");
    write_fixture(&path, &[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33]);
    assert!(TtaDecoder::open(&path).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn bad_magic_is_rejected() {
    let path = temp_path("magic");
    let mut file = base_header(2, 16, 44_100, 1000, 1);
    file[0..4].copy_from_slice(b"TTA2");
    write_fixture(&path, &file);
    assert!(TtaDecoder::open(&path).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn header_crc_mismatch_is_rejected() {
    let path = temp_path("hcrc");
    let mut file = base_header(2, 16, 44_100, 1000, 1);
    file[10] ^= 0xFF;
    write_fixture(&path, &file);
    assert!(TtaDecoder::open(&path).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn encrypted_format_is_rejected_explicitly() {
    let path = temp_path("enc");
    let file = base_header(2, 16, 44_100, 1000, 2);
    write_fixture(&path, &file);
    let err = match TtaDecoder::open(&path) {
        Err(e) => e,
        Ok(_) => panic!("encrypted TTA must be rejected"),
    };
    assert!(matches!(err, DecodeError::UnsupportedFormat(ref m) if m.contains("encrypted")));
    std::fs::remove_file(&path).ok();
}

#[test]
fn invalid_header_fields_are_rejected() {
    for (channels, bits, rate, length) in [
        (0u16, 16u16, 44_100u32, 1000u32),
        (17, 16, 44_100, 1000),
        (2, 32, 44_100, 1000),
        (2, 16, 0, 1000),
        (2, 16, 2_000_000, 1000),
        (2, 16, 44_100, 0),
    ] {
        let path = temp_path("hdr");
        let file = base_header(channels, bits, rate, length, 1);
        write_fixture(&path, &file);
        assert!(
            TtaDecoder::open(&path).is_err(),
            "{channels}ch {bits}b {rate}Hz len {length}"
        );
        std::fs::remove_file(&path).ok();
    }
}

#[test]
fn oversized_frame_claim_is_rejected_without_allocation() {
    let path = temp_path("huge");
    let file = base_header(16, 16, 1_000_000, 100_000_000, 1);
    write_fixture(&path, &file);
    assert!(TtaDecoder::open(&path).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn truncated_size_table_is_rejected() {
    let path = temp_path("tbl");
    let mut file = base_header(2, 16, 44_100, 1_000_000, 1);
    file.extend_from_slice(&[0u8; 8]);
    write_fixture(&path, &file);
    assert!(TtaDecoder::open(&path).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn size_table_crc_mismatch_is_rejected() {
    let path = temp_path("tcrc");
    let mut file = base_header(2, 16, 44_100, 1000, 1);
    file.extend_from_slice(&64u32.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    write_fixture(&path, &file);
    assert!(TtaDecoder::open(&path).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn corrupted_frame_crc_errors_on_decode_not_open() {
    let channels = 2u16;
    let bits = 16u16;
    let rate = 44_100u32;
    let frame_len = 256u32 * rate / 245;
    let mut rng = Lcg(99);
    let mut enc_states: Vec<EncChannel> =
        (0..channels).map(|_| EncChannel::new(bits)).collect();
    let mut inter = Vec::new();
    for _ in 0..frame_len {
        for _ in 0..channels {
            inter.push(rng.sample(10_000));
        }
    }
    let payload = encode_frame(channels as usize, bits, &inter, &mut enc_states);
    let mut frame = payload.clone();
    let crc = crc32(&payload);
    frame.extend_from_slice(&crc.to_le_bytes());

    let mut file = build_tta_file(channels, bits, rate, frame_len, &[frame]);
    let data_start = HEADER_SIZE + 4 + 4;
    file[data_start + 8] ^= 0xAA;

    let path = temp_path("fcrc");
    write_fixture(&path, &file);
    let mut dec = TtaDecoder::open(&path).expect("header/table are valid");
    let err = dec.decode_next(1024).unwrap_err();
    assert!(matches!(err, DecodeError::Decode(ref m) if m.contains("CRC")));
    std::fs::remove_file(&path).ok();
}

#[test]
fn truncated_frame_payload_errors_cleanly() {
    let channels = 2u16;
    let bits = 16u16;
    let rate = 44_100u32;
    let frame_len = 256u32 * rate / 245;
    let mut rng = Lcg(31);
    let mut enc_states: Vec<EncChannel> =
        (0..channels).map(|_| EncChannel::new(bits)).collect();
    let mut inter = Vec::new();
    for _ in 0..frame_len {
        for _ in 0..channels {
            inter.push(rng.sample(10_000));
        }
    }
    let payload = encode_frame(channels as usize, bits, &inter, &mut enc_states);
    let mut frame = payload;
    let crc = crc32(&frame);
    frame.extend_from_slice(&crc.to_le_bytes());

    let mut file = build_tta_file(channels, bits, rate, frame_len, &[frame]);
    file.truncate(file.len() - 40);

    let path = temp_path("trunc");
    write_fixture(&path, &file);
    let mut dec = TtaDecoder::open(&path).expect("structure parses");
    assert!(dec.decode_next(1024).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn per_symbol_encoder_decoder_parity_mono_8bit() {
    let n = 32usize;
    let mut rng = Lcg(7);
    let interleaved: Vec<i32> = (0..n).map(|_| rng.sample(120)).collect();

    let mut st = EncChannel::new(8);
    let mut w = BitWriterLsb::default();
    let mut enc_trace = Vec::new();
    let k_pred = 4u32;
    for &target in &interleaved {
        let f = st.filter.peek_contribution();
        let v = target
            .wrapping_sub(pred(st.predictor, k_pred))
            .wrapping_sub(f);
        let u = zigzag_encode(v);
        encode_rice_symbol(&mut w, &mut st.rice, u);
        enc_trace.push((v, u, st.rice.k0));
        let filtered = st.filter.process(v);
        let out = filtered.wrapping_add(pred(st.predictor, k_pred));
        assert_eq!(out, target);
        st.predictor = out;
    }
    let payload = w.finish();

    let mut dec = super::decoder::ChannelState::new(8);
    dec.reset();
    let mut br = BitReaderLsb::new(&payload);
    for (i, &(v_enc, u_enc, k0e)) in enc_trace.iter().enumerate() {
        let raw = dec
            .rice
            .decode(&mut br)
            .unwrap_or_else(|e| panic!("sym {i}: {e}"));
        let v_dec = zigzag_decode(raw);
        if v_dec != v_enc || dec.rice.k0 != k0e {
            panic!(
                "sym {i}: enc(v={v_enc},u={u_enc},k0={k0e}) dec(v={v_dec},raw={raw},k0={})",
                dec.rice.k0
            );
        }
        let mut value = v_dec;
        value = dec.filter.process(value);
        value = value.wrapping_add(pred(dec.predictor, k_pred));
        dec.predictor = value;
        if value != interleaved[i] {
            panic!("sym {i}: output {value} != target {}", interleaved[i]);
        }
    }
}

#[test]
fn id3v2_prefix_is_skipped() {
    let channels = 2u16;
    let bits = 16u16;
    let rate = 44_100u32;
    let frame_len = 256u32 * rate / 245;
    let mut rng = Lcg(555);
    let mut enc_states: Vec<EncChannel> =
        (0..channels).map(|_| EncChannel::new(bits)).collect();
    let mut inter = Vec::new();
    for _ in 0..frame_len {
        for _ in 0..channels {
            inter.push(rng.sample(12_345));
        }
    }
    let payload = encode_frame(channels as usize, bits, &inter, &mut enc_states);
    let mut frame = payload;
    let crc = crc32(&frame);
    frame.extend_from_slice(&crc.to_le_bytes());
    let body = build_tta_file(channels, bits, rate, frame_len, &[frame]);

    let mut file = Vec::new();
    file.extend_from_slice(b"ID3");
    file.extend_from_slice(&[3, 0, 0]);
    file.extend_from_slice(&[0, 0, 0, 100]);
    file.extend_from_slice(&vec![0u8; 100]);
    file.extend_from_slice(&body);

    let path = temp_path("id3");
    write_fixture(&path, &file);
    let mut dec = TtaDecoder::open(&path).expect("ID3v2 prefix must be skipped");
    let chunk = dec.decode_next(1 << 20).expect("decodes after tag");
    assert_eq!(chunk.frame_count, frame_len as usize);
    std::fs::remove_file(&path).ok();
}
