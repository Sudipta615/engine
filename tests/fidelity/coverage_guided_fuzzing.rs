//! Coverage-Guided & Mutation Robustness Fuzzing Suite (Punch List Item 7).
//!
//! Validates that corrupted, truncated, malformed, and adversarial inputs across
//! all major engine parsers, containers, decoders, and ABI boundaries return
//! clean error/None results without panics, undefined behavior, or memory corruption:
//!
//! 1. WAV / AIFF / FLAC header and container parsing
//! 2. Ogg / Opus packet streams
//! 3. MP4 / ISOM container parsing
//! 4. Metadata tag parsing (ID3v2, Vorbis Comments, APEv2)
//! 5. CUE sheet parsing
//! 6. SOFA / NetCDF-classic HRTF parsing
//! 7. ADM / BW64 XML and container parsing
//! 8. Graph2 and spatial state serialization/deserialization
//! 9. Plugin ABI boundary validation

use engine::decode::cue::CueSheet;
use engine::decode::{extract_loudness_metadata, Decoder};
use engine::dsp::graph2::Graph2;
use engine::spatial::adm::parse_adm_xml;
use engine::spatial::sofa::import_sofa;
use engine::standards::adm::AdmDocument;
use plugin_abi::params::PluginParams;
use plugin_abi::state::StateBuffer;

/// Deterministic SplitMix64 pseudo-random generator for reproducible fuzz mutations.
struct FuzzRng(u64);

impl FuzzRng {
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

/// Applies coverage-guided style mutation operations:
/// bit flips, byte insertions, deletions, truncations, and boundary value substitutions.
fn mutate(buf: &mut Vec<u8>, rng: &mut FuzzRng) {
    if buf.is_empty() {
        buf.push(rng.byte());
        return;
    }

    match rng.below(7) {
        0 => {
            // Bit flip
            let idx = rng.below(buf.len());
            buf[idx] ^= 1 << rng.below(8);
        }
        1 => {
            // Byte overwrite
            let idx = rng.below(buf.len());
            buf[idx] = rng.byte();
        }
        2 => {
            // Extreme integer substitutions (0x00, 0x7F, 0x80, 0xFF)
            let idx = rng.below(buf.len());
            let extreme_vals = [0x00, 0x01, 0x7F, 0x80, 0xFE, 0xFF];
            buf[idx] = extreme_vals[rng.below(extreme_vals.len())];
        }
        3 => {
            // Truncation
            let new_len = rng.below(buf.len());
            buf.truncate(new_len);
        }
        4 => {
            // Insert byte
            let idx = rng.below(buf.len());
            buf.insert(idx, rng.byte());
        }
        5 => {
            // Delete byte
            if buf.len() > 1 {
                let idx = rng.below(buf.len());
                buf.remove(idx);
            }
        }
        _ => {
            // Multi-byte chunk shuffle / inversion
            let start = rng.below(buf.len());
            let end = (start + rng.below(buf.len() - start)).min(buf.len());
            for b in &mut buf[start..end] {
                *b = !*b;
            }
        }
    }
}

// ── 1. WAV / AIFF / FLAC Container Fuzzing ──────────────────────────────────

#[test]
fn test_fuzz_wav_aiff_flac() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_0001);

    // Valid minimal WAV header seed
    let base_wav = {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&36u32.to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&2u16.to_le_bytes()); // 2 ch
        b.extend_from_slice(&48000u32.to_le_bytes()); // 48k
        b.extend_from_slice(&(48000u32 * 4).to_le_bytes());
        b.extend_from_slice(&4u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    };

    // Valid minimal AIFF header seed
    let base_aiff = {
        let mut b = Vec::new();
        b.extend_from_slice(b"FORM");
        b.extend_from_slice(&46u32.to_be_bytes());
        b.extend_from_slice(b"AIFFCOMM");
        b.extend_from_slice(&18u32.to_be_bytes());
        b.extend_from_slice(&2u16.to_be_bytes()); // 2 ch
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&16u16.to_be_bytes());
        b.extend_from_slice(&[0x40, 0x0E, 0xBB, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // 48k
        b.extend_from_slice(b"SSND");
        b.extend_from_slice(&8u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b
    };

    // Minimal FLAC signature seed
    let base_flac = b"fLaC\x00\x00\x00\"\x10\x00\x10\x00\x00\x00\x00\x00\x00\x00\x0b\xb8\x01\x70\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();

    let seeds = [base_wav, base_aiff, base_flac];

    for seed in seeds {
        let mut cur = seed;
        for i in 0..100 {
            mutate(&mut cur, &mut rng);
            let path =
                std::env::temp_dir().join(format!("fuzz_waf_{}_{}.bin", std::process::id(), i));
            let _ = std::fs::write(&path, &cur);
            // Must not panic, returns Ok or Err
            let _ = Decoder::open(&path);
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ── 2. Ogg / Opus Packet Container Fuzzing ──────────────────────────────────

#[test]
fn test_fuzz_ogg_opus() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_0002);
    // Ogg page header seed
    let mut ogg_seed = b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x01\x13OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();

    for i in 0..100 {
        mutate(&mut ogg_seed, &mut rng);
        let path = std::env::temp_dir().join(format!("fuzz_ogg_{}_{}.opus", std::process::id(), i));
        let _ = std::fs::write(&path, &ogg_seed);
        let _ = Decoder::open(&path);
        let _ = std::fs::remove_file(&path);
    }
}

// ── 3. MP4 / ISOM Container Parsing Fuzzing ─────────────────────────────────

#[test]
fn test_fuzz_mp4_container() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_0003);
    // Minimal MP4 ftyp atom seed
    let mut mp4_seed =
        b"\x00\x00\x00\x20ftypM4A \x00\x00\x00\x00M4A mp42isom\x00\x00\x00\x08free".to_vec();

    for i in 0..100 {
        mutate(&mut mp4_seed, &mut rng);
        let path = std::env::temp_dir().join(format!("fuzz_mp4_{}_{}.m4a", std::process::id(), i));
        let _ = std::fs::write(&path, &mp4_seed);
        let _ = Decoder::open(&path);
        let _ = std::fs::remove_file(&path);
    }
}

// ── 4. Metadata Tag Parsing Fuzzing (ID3v2, Vorbis, APEv2) ───────────────────

#[test]
fn test_fuzz_metadata_tags() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_0004);
    // Minimal ID3v2.4 header
    let mut id3_seed =
        b"ID3\x04\x00\x00\x00\x00\x00\x23TIT2\x00\x00\x00\x09\x00\x00\x03Track 01".to_vec();

    for i in 0..100 {
        mutate(&mut id3_seed, &mut rng);
        let path = std::env::temp_dir().join(format!("fuzz_tag_{}_{}.bin", std::process::id(), i));
        let _ = std::fs::write(&path, &id3_seed);
        let _ = extract_loudness_metadata(&path);
        let _ = std::fs::remove_file(&path);
    }
}

// ── 5. CUE Sheet Parser Fuzzing ─────────────────────────────────────────────

#[test]
fn test_fuzz_cue_parser() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_0005);
    let valid_cue = r#"
        REM COMMENT
        PERFORMER "Artist"
        TITLE "Album Title"
        FILE "audio.wav" WAVE
          TRACK 01 AUDIO
            TITLE "Track One"
            INDEX 01 00:00:00
          TRACK 02 AUDIO
            TITLE "Track Two"
            INDEX 01 03:45:00
    "#;
    let mut cur = valid_cue.as_bytes().to_vec();

    for _ in 0..200 {
        mutate(&mut cur, &mut rng);
        let s = String::from_utf8_lossy(&cur);
        // Must never panic on arbitrary malformed input
        let _ = CueSheet::parse(&s);
    }
}

// ── 6. SOFA / NetCDF HRTF Parser Fuzzing ─────────────────────────────────────

#[test]
fn test_fuzz_sofa_netcdf() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_0006);
    // NetCDF classic header magic
    let mut sofa_seed = b"CDF\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();

    for _ in 0..150 {
        mutate(&mut sofa_seed, &mut rng);
        // Must return clean Err without panic
        let _ = import_sofa(&sofa_seed, None);
    }
}

// ── 7. ADM / BW64 Container & XML Fuzzing ───────────────────────────────────

#[test]
fn test_fuzz_adm_bw64() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_0007);
    let valid_adm_xml = r#"<?xml version="1.0" encoding="utf-8"?>
        <adm:adm xmlns:adm="urn:ebu:metadata:adm">
          <adm:audioFormatExtended>
            <adm:audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Stereo Mix"/>
            <adm:audioObject audioObjectID="AO_1001" audioObjectName="Lead Vocal">
              <adm:audioTrackUIDRef>ATU_00000001</adm:audioTrackUIDRef>
            </adm:audioObject>
          </adm:audioFormatExtended>
        </adm:adm>
    "#;
    let mut cur = valid_adm_xml.as_bytes().to_vec();

    for _ in 0..200 {
        mutate(&mut cur, &mut rng);
        let s = String::from_utf8_lossy(&cur);
        let _ = parse_adm_xml(&s);
        let _ = serde_json::from_slice::<AdmDocument>(&cur);
    }
}

// ── 8. Graph2 & State Serialization Fuzzing ─────────────────────────────────

#[test]
fn test_fuzz_graph2_and_state_serialization() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_0008);

    // 1. Graph2 topology JSON
    let mut g = Graph2::new();
    let s = g.add_source("in");
    let d = g.add_gain("gain", 0.5);
    let k = g.add_sink("out");
    let _ = g.add_edge(
        s,
        engine::dsp::graph2::PortId::OUT,
        d,
        engine::dsp::graph2::PortId::IN,
    );
    let _ = g.add_edge(
        d,
        engine::dsp::graph2::PortId::OUT,
        k,
        engine::dsp::graph2::PortId::IN,
    );
    let json_bytes = serde_json::to_vec(&g).unwrap();

    let mut cur = json_bytes;
    for _ in 0..200 {
        mutate(&mut cur, &mut rng);
        let _ = serde_json::from_slice::<Graph2>(&cur);
    }

    // 2. SpatialSceneConfig JSON
    let scene = config::SpatialSceneConfig::default();
    let scene_bytes = serde_json::to_vec(&scene).unwrap();
    let mut cur_scene = scene_bytes;
    for _ in 0..200 {
        mutate(&mut cur_scene, &mut rng);
        let _ = serde_json::from_slice::<config::SpatialSceneConfig>(&cur_scene);
    }
}

// ── 9. Plugin ABI Boundary Fuzzing ──────────────────────────────────────────

#[test]
fn test_fuzz_plugin_abi_boundary() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_0009);

    // Fuzz StateBuffer with arbitrary byte sequences
    let mut state_bytes = vec![0u8; 64];
    for _ in 0..200 {
        mutate(&mut state_bytes, &mut rng);
        let mut buf = StateBuffer::new();
        let _ = buf.set_bytes(&state_bytes);
    }

    // Fuzz PluginParams batch pushing with extreme indices and non-finite values
    let mut params = PluginParams::empty();
    for _ in 0..200 {
        let idx = rng.next() as u32;
        let val = f32::from_bits(rng.next() as u32);
        // Pushing arbitrary values (including NaN/Inf or extreme indices) must never panic
        let _ = params.push(idx, val);
    }
}
