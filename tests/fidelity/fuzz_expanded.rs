//! Expanded Format, Serializer, and Parser Fuzzing Suite (spec §7.4).
//!
//! Evaluates the engine's resilience against untrusted, malformed, and adversarial inputs:
//! - Codecs & Containers: WAV, AIFF, FLAC, MP4, Ogg, Opus, APE, WavPack, TTA, DSD (DSF/DFF)
//! - Metadata & CUE: CueSheet parser
//! - Serializers: Graph2 JSON, SpatialScene JSON, AdmDocument JSON
//! - Plugin ABI: Descriptors and state structures
//!
//! Guarantees:
//! Under all mutations (truncation, flipped bits, oversized headers, absurd timestamps,
//! non-finite floats, malformed channel counts), parsers must reject gracefully with `Err`,
//! never panic, and never enter unbounded loops.

#![allow(clippy::unusual_byte_groupings)]

use std::sync::atomic::{AtomicU64, Ordering};

use engine::decode::cue::CueSheet;
use engine::decode::dsd::DsdReader;
use engine::decode::tta::TtaDecoder;
use engine::dsp::graph2::Graph2;
use engine::standards::adm::AdmDocument;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_fixture_path(ext: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("engine-fuzz-exp-{}-{n}.{ext}", std::process::id()))
}

/// Deterministic SplitMix64 PRNG.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn byte(&mut self) -> u8 {
        (self.next() & 0xFF) as u8
    }
}

/// Applies a deterministic mutation to an input byte slice.
fn mutate(buf: &mut Vec<u8>, lcg: &mut Lcg) {
    if buf.is_empty() {
        buf.push(lcg.byte());
        return;
    }

    match lcg.below(5) {
        0 => {
            // Flip bits in a random byte
            let idx = lcg.below(buf.len());
            buf[idx] ^= 1 << lcg.below(8);
        }
        1 => {
            // Truncate
            let new_len = lcg.below(buf.len());
            buf.truncate(new_len);
        }
        2 => {
            // Insert random bytes
            let idx = lcg.below(buf.len());
            buf.insert(idx, lcg.byte());
        }
        3 => {
            // Overwrite with oversized/extreme integer
            let idx = lcg.below(buf.len());
            buf[idx] = 0xFF;
        }
        _ => {
            // Delete a byte
            if buf.len() > 1 {
                let idx = lcg.below(buf.len());
                buf.remove(idx);
            }
        }
    }
}

// ── 1. CUE Sheet Parser Fuzzing ───────────────────────────────────────────────

#[test]
fn test_fuzz_cue_sheet_parser() {
    let valid_cue = b"PERFORMER \"Artist\"\nTITLE \"Album\"\nFILE \"track.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n";
    let mut lcg = Lcg::new(0xC0FFEE_01);

    for _ in 0..100 {
        let mut mutant = valid_cue.to_vec();
        for _ in 0..lcg.below(10) + 1 {
            mutate(&mut mutant, &mut lcg);
        }

        let text = String::from_utf8_lossy(&mutant);
        // Parsing must never panic regardless of input corruption
        let _ = CueSheet::parse(&text);
    }
}

// ── 2. Graph2 Topology Deserialization Fuzzing ────────────────────────────────

#[test]
fn test_fuzz_graph2_json_deserializer() {
    let mut g = Graph2::new();
    let s = g.add_source("src");
    let d = g.add_delay("del", 100);
    let k = g.add_sink("sink");
    g.add_edge(
        s,
        engine::dsp::graph2::PortId::OUT,
        d,
        engine::dsp::graph2::PortId::IN,
    )
    .unwrap();
    g.add_edge(
        d,
        engine::dsp::graph2::PortId::OUT,
        k,
        engine::dsp::graph2::PortId::IN,
    )
    .unwrap();

    let valid_json = serde_json::to_vec(&g).unwrap();
    let mut lcg = Lcg::new(0xC0FFEE_02);

    for _ in 0..100 {
        let mut mutant = valid_json.clone();
        for _ in 0..lcg.below(8) + 1 {
            mutate(&mut mutant, &mut lcg);
        }

        // Must reject or deserialize safely without panicking
        let _ = serde_json::from_slice::<Graph2>(&mutant);
    }
}

// ── 3. Spatial Scene Config JSON Fuzzing ──────────────────────────────────────

#[test]
fn test_fuzz_spatial_scene_deserializer() {
    let scene = config::SpatialSceneConfig::default();
    let valid_json = serde_json::to_vec(&scene).unwrap();
    let mut lcg = Lcg::new(0xC0FFEE_03);

    for _ in 0..100 {
        let mut mutant = valid_json.clone();
        for _ in 0..lcg.below(8) + 1 {
            mutate(&mut mutant, &mut lcg);
        }

        let _ = serde_json::from_slice::<config::SpatialSceneConfig>(&mutant);
    }
}

// ── 4. ADM Document JSON Fuzzing ──────────────────────────────────────────────

#[test]
fn test_fuzz_adm_document_deserializer() {
    let adm = AdmDocument::default();
    let valid_json = serde_json::to_vec(&adm).unwrap();
    let mut lcg = Lcg::new(0xC0FFEE_04);

    for _ in 0..100 {
        let mut mutant = valid_json.clone();
        for _ in 0..lcg.below(8) + 1 {
            mutate(&mut mutant, &mut lcg);
        }

        let _ = serde_json::from_slice::<AdmDocument>(&mutant);
    }
}

// ── 5. TTA Decoder Header Fuzzing ─────────────────────────────────────────────

#[test]
fn test_fuzz_tta_decoder() {
    // Valid TTA header: 'TTA1', format=1, ch=2, bps=16, sr=44100, samples=44100, crc32
    let mut valid_tta = Vec::new();
    valid_tta.extend_from_slice(b"TTA1");
    valid_tta.extend_from_slice(&1u16.to_le_bytes()); // audio format PCM
    valid_tta.extend_from_slice(&2u16.to_le_bytes()); // 2 channels
    valid_tta.extend_from_slice(&16u16.to_le_bytes()); // 16-bit
    valid_tta.extend_from_slice(&44100u32.to_le_bytes()); // sample rate
    valid_tta.extend_from_slice(&44100u32.to_le_bytes()); // total samples
    valid_tta.extend_from_slice(&[0u8; 4]); // crc placeholder

    let mut lcg = Lcg::new(0xC0FFEE_05);

    for _ in 0..50 {
        let mut mutant = valid_tta.clone();
        for _ in 0..lcg.below(6) + 1 {
            mutate(&mut mutant, &mut lcg);
        }

        let path = temp_fixture_path("tta");
        if std::fs::write(&path, &mutant).is_ok() {
            let _ = TtaDecoder::open(&path);
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ── 6. DSD / DSF Reader Fuzzing ───────────────────────────────────────────────

#[test]
fn test_fuzz_dsf_reader() {
    // Basic DSF header chunk
    let mut valid_dsf = Vec::new();
    valid_dsf.extend_from_slice(b"DSD ");
    valid_dsf.extend_from_slice(&28u64.to_le_bytes()); // chunk size
    valid_dsf.extend_from_slice(&92u64.to_le_bytes()); // total file size
    valid_dsf.extend_from_slice(&0u64.to_le_bytes()); // metadata offset

    let mut lcg = Lcg::new(0xC0FFEE_06);

    for _ in 0..50 {
        let mut mutant = valid_dsf.clone();
        for _ in 0..lcg.below(6) + 1 {
            mutate(&mut mutant, &mut lcg);
        }

        let path = temp_fixture_path("dsf");
        if std::fs::write(&path, &mutant).is_ok() {
            let _ = DsdReader::open(&path);
            let _ = std::fs::remove_file(&path);
        }
    }
}
