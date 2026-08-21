//! Seeded mutation ("fuzz-style") robustness tests (spec §31).
//!
//! A desktop audio engine consumes untrusted files, so every parser entry
//! point must reject malformed input safely: never panic, never loop
//! unboundedly, never report `Ok` with absurd metadata. This file brings
//! cargo-fuzz-style byte mutation into the CI-runnable test suite:
//!
//! - Build a **valid** fixture per target (DSD/DSF, DSD/DFF, TTA, Ogg Opus,
//!   format probing, CUE sheets) entirely in code — no binary assets.
//! - Apply hundreds of **deterministic** mutations (bit flips at random
//!   offsets, truncations, extensions) from a fixed seed.
//! - Drive the real parser on each mutant. A panic fails the test; an
//!   unbounded decode loop is broken by a hard iteration cap; `Err` and
//!   `Ok` are both acceptable, but the parser must never crash.
//!
//! Targets map to the engine's own unit-test fixture builders so the base
//! files are genuinely parseable, not random garbage.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::decode::cue::CueSheet;
use engine::decode::dsd::DsdReader;
use engine::decode::opus::OpusSource;
use engine::decode::scanner::scan_track_loudness;
use engine::decode::tta::TtaDecoder;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(ext: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "engine-fuzz-mutation-{}-{n}.{ext}",
        std::process::id()
    ))
}

fn write_fixture(bytes: &[u8], ext: &str) -> PathBuf {
    let path = temp_path(ext);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// Deterministic PRNG (SplitMix64) so mutations are reproducible.
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
}

/// Apply `mutations` byte-level edits to `base`, returning a mutated copy:
/// a mix of single-bit flips, multi-byte flips, truncation, and extension
/// (all deterministic in the seed).
fn mutate(base: &[u8], seed: u64) -> Vec<u8> {
    let mut rng = Lcg::new(seed);
    let mut out = base.to_vec();
    let edits = 1 + rng.below(8);
    for _ in 0..edits {
        if out.is_empty() {
            break;
        }
        match rng.below(4) {
            0 => {
                // Single-bit flip.
                let pos = rng.below(out.len());
                out[pos] ^= 1 << rng.below(8);
            }
            1 => {
                // Random byte overwrite.
                let pos = rng.below(out.len());
                out[pos] = (rng.next() & 0xFF) as u8;
            }
            2 => {
                // Truncate (sometimes to zero).
                let cut = rng.below(out.len() + 1);
                out.truncate(cut);
            }
            _ => {
                // Extend with garbage.
                let extra = 1 + rng.below(64);
                for _ in 0..extra {
                    out.push((rng.next() & 0xFF) as u8);
                }
            }
        }
    }
    out
}

/// Drive a DSD reader on a mutated file: open if possible, then read blocks
/// under a hard iteration cap (a hostile file must never loop forever).
fn exercise_dsd(bytes: &[u8], ext: &str) {
    let path = write_fixture(bytes, ext);
    if let Ok(mut reader) = DsdReader::open(&path) {
        let mut iterations = 0u32;
        loop {
            match reader.read_dsd_block(512) {
                Ok(Some(_)) => {}
                _ => break,
            }
            iterations += 1;
            assert!(
                iterations < 100_000,
                "DSD reader must make progress on a mutated file"
            );
        }
    }
    let _ = std::fs::remove_file(&path);
}

fn exercise_tta(bytes: &[u8]) {
    let path = write_fixture(bytes, "tta");
    if let Ok(mut decoder) = TtaDecoder::open(&path) {
        let mut iterations = 0u32;
        loop {
            match decoder.decode_next(512) {
                Ok(chunk) if chunk.samples.len() > 0 => {}
                _ => break,
            }
            iterations += 1;
            assert!(
                iterations < 100_000,
                "TTA decoder must make progress on a mutated file"
            );
        }
    }
    let _ = std::fs::remove_file(&path);
}

fn exercise_opus(bytes: &[u8]) {
    let path = write_fixture(bytes, "opus");
    if let Ok(mut decoder) = OpusSource::open(&path) {
        let mut iterations = 0u32;
        loop {
            match decoder.decode_next(512) {
                Ok(chunk) if chunk.samples.len() > 0 => {}
                _ => break,
            }
            iterations += 1;
            assert!(
                iterations < 100_000,
                "Opus decoder must make progress on a mutated file"
            );
        }
    }
    let _ = std::fs::remove_file(&path);
}

// ── Fixture builders (mirror the engine's own test builders) ────────────────

/// Standard IEEE CRC-32 (matches the engine's TTA implementation).
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut crc = i as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
        *entry = crc;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc >> 8) ^ table[((crc ^ b as u32) & 0xFF) as usize];
    }
    crc ^ 0xFFFF_FFFF
}

/// Minimal valid mono DSF: 4096 DSD frames, one data block.
fn build_dsf() -> Vec<u8> {
    let block_size = 4096u32;
    let audio = vec![0xA5u8; 4096]; // one block of DSD bytes
    let mut out = Vec::new();
    out.extend_from_slice(b"DSD ");
    out.extend_from_slice(&28u64.to_le_bytes());
    let size_pos = out.len();
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&52u64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&2_822_400u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(4096u64 * 8).to_le_bytes());
    out.extend_from_slice(&block_size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&((audio.len() as u64) + 12).to_le_bytes());
    out.extend_from_slice(&audio);

    let total = out.len() as u64;
    out[size_pos..size_pos + 8].copy_from_slice(&total.to_le_bytes());
    out
}

/// Minimal valid mono DFF.
fn build_dff() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"FRM8");
    let size_pos = out.len();
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(b"DSD ");

    out.extend_from_slice(b"FVER");
    out.extend_from_slice(&4u64.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());

    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend_from_slice(b"FS");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(&2_822_400u32.to_be_bytes());
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&2u64.to_be_bytes());
    prop.extend_from_slice(&1u16.to_be_bytes());
    prop.extend_from_slice(b"CMPR");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(b"DSD ");
    out.extend_from_slice(b"PROP");
    out.extend_from_slice(&(prop.len() as u64).to_be_bytes());
    out.extend_from_slice(&prop);

    let audio = vec![0xA5u8; 4096];
    out.extend_from_slice(b"DSD ");
    out.extend_from_slice(&(audio.len() as u64).to_be_bytes());
    out.extend_from_slice(&audio);

    let total = out.len() as u64;
    out[size_pos..size_pos + 8].copy_from_slice(&(total - 12).to_be_bytes());
    out
}

/// Minimal valid TTA: 22-byte header + frame-size table + one zero frame.
fn build_tta() -> Vec<u8> {
    let payload = vec![0u8; 8];
    let frame = {
        let mut f = payload.clone();
        f.extend_from_slice(&crc32(&payload).to_le_bytes());
        f
    };
    let mut out = Vec::new();
    out.extend_from_slice(b"TTA1");
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(&44_100u32.to_le_bytes());
    out.extend_from_slice(&(8u32).to_le_bytes()); // data_length = payload bytes
    let header_crc = crc32(&out[..18]);
    out.extend_from_slice(&header_crc.to_le_bytes());
    // Frame-size table: one entry.
    out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    let table_crc = crc32(&out[22..26]);
    out.extend_from_slice(&table_crc.to_le_bytes());
    out.extend_from_slice(&frame);
    out
}

/// Ogg CRC-32 (polynomial 0x04C11DB7, init 0, no reflection, no final XOR).
fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Build one Ogg page from packets (lacing per the Ogg spec).
fn ogg_page(header_type: u8, granule: u64, serial: u32, seq: u32, packets: &[&[u8]]) -> Vec<u8> {
    let mut segs = Vec::new();
    let mut payload = Vec::new();
    for p in packets {
        let mut idx = 0usize;
        let mut rem = p.len();
        while rem >= 255 {
            segs.push(255);
            payload.extend_from_slice(&p[idx..idx + 255]);
            idx += 255;
            rem -= 255;
        }
        if rem > 0 {
            segs.push(rem as u8);
            payload.extend_from_slice(&p[idx..idx + rem]);
        } else if !segs.is_empty() && *segs.last().unwrap() == 255 {
            // Exact multiple of 255: terminate with a 0 lacing value.
            segs.push(0);
        }
    }
    let mut page = Vec::new();
    page.extend_from_slice(b"OggS");
    page.push(0); // version
    page.push(header_type);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&[0u8; 4]); // CRC placeholder
    page.push(segs.len() as u8);
    page.extend_from_slice(&segs);
    page.extend_from_slice(&payload);
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// Minimal valid Ogg-Opus file: one page with OpusHead + OpusTags packets.
fn build_ogg_opus() -> Vec<u8> {
    let mut head = Vec::new();
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(2); // channels
    head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&48_000u32.to_le_bytes()); // input rate
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family

    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    let vendor = b"engine-test";
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // no user comments

    ogg_page(0x02, 0, 0x1234_5678, 0, &[&head, &tags])
}

// ── Fuzz targets ────────────────────────────────────────────────────────────

#[test]
fn dsf_mutations_never_panic() {
    let base = build_dsf();
    for seed in 0..200u64 {
        let mutated = mutate(&base, seed);
        exercise_dsd(&mutated, "dsf");
    }
}

#[test]
fn dff_mutations_never_panic() {
    let base = build_dff();
    for seed in 0..200u64 {
        let mutated = mutate(&base, seed);
        exercise_dsd(&mutated, "dff");
    }
}

#[test]
fn tta_mutations_never_panic() {
    let base = build_tta();
    for seed in 0..200u64 {
        let mutated = mutate(&base, seed);
        exercise_tta(&mutated);
    }
}

#[test]
fn opus_mutations_never_panic() {
    let base = build_ogg_opus();
    for seed in 0..200u64 {
        let mutated = mutate(&base, seed);
        exercise_opus(&mutated);
    }
}

/// The loudness scanner (format probe / offline analysis path) must survive
/// arbitrary mutations of real DSD and TTA containers without panicking.
#[test]
fn loudness_scanner_mutations_never_panic() {
    let bases: [(&[u8], &str); 2] = [(&build_dsf(), "dsf"), (&build_tta(), "tta")];
    for (base, ext) in bases {
        for seed in 0..150u64 {
            let mutated = mutate(base, seed);
            let path = write_fixture(&mutated, ext);
            let _ = scan_track_loudness(&path); // must not panic
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// CUE sheets are free text parsed from untrusted tags: mutations of a valid
/// sheet and arbitrary garbage must never panic the parser.
#[test]
fn cue_sheet_mutations_never_panic() {
    let valid = "REM GENRE Classical\n\
                 PERFORMER \"Orchestra\"\n\
                 TITLE \"Symphony\"\n\
                 FILE \"disk.wav\" WAVE\n\
                   TRACK 01 AUDIO\n\
                     TITLE \"Movement I\"\n\
                     INDEX 01 00:00:00\n\
                   TRACK 02 AUDIO\n\
                     TITLE \"Movement II\"\n\
                     INDEX 01 04:30:12\n";

    // Mutated valid sheets.
    let bytes = valid.as_bytes();
    for seed in 0..300u64 {
        let mutated = mutate(bytes, seed);
        let text = String::from_utf8_lossy(&mutated);
        let _ = CueSheet::parse(&text); // must not panic
    }

    // Pure garbage: control chars, huge line counts, random bytes.
    let mut rng = Lcg::new(42);
    let mut garbage = String::new();
    for _ in 0..2000 {
        garbage.push(char::from_u32((rng.next() & 0xFF) as u32).unwrap_or(' '));
    }
    let _ = CueSheet::parse(&garbage);

    // Hostile index fields must fail cleanly, never panic.
    for time in [
        "99:99:99",
        "-1:00:00",
        "00:00:00",
        "1:2:3",
        "99999:99999:99999",
        "abc:def:ghi",
        "00;00;00",
    ] {
        let sheet = format!("FILE \"x.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 {time}\n");
        let _ = CueSheet::parse(&sheet);
    }
}

/// The generic `Decoder::open` dispatch (extension routing + Symphonia probe)
/// must never panic on mutated DSD/TTA/Opus files with recognized
/// extensions — the end-to-end entry point users actually hit.
#[test]
fn decoder_dispatch_mutations_never_panic() {
    use engine::decode::Decoder;

    let bases: [(&[u8], &str); 4] = [
        (&build_dsf(), "dsf"),
        (&build_dff(), "dff"),
        (&build_tta(), "tta"),
        (&build_ogg_opus(), "opus"),
    ];
    for (base, ext) in bases {
        for seed in 0..120u64 {
            let mutated = mutate(base, seed);
            let path = write_fixture(&mutated, ext);
            if let Ok(mut decoder) = Decoder::open(&path) {
                let mut iterations = 0u32;
                loop {
                    match decoder.decode_next(512) {
                        Ok(chunk) if chunk.samples.len() > 0 => {}
                        _ => break,
                    }
                    iterations += 1;
                    assert!(iterations < 100_000, "decoder must make progress");
                }
            }
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Ensure the valid fixtures actually parse (the mutation runs above only
/// mean something if the base is real).
#[test]
fn fixtures_are_genuinely_parseable() {
    let dsf = temp_path("dsf");
    std::fs::write(&dsf, build_dsf()).unwrap();
    let reader = DsdReader::open(&dsf).expect("DSF fixture must open");
    assert!(reader.total_dsd_frames() >= 4096);
    let _ = std::fs::remove_file(&dsf);

    let dff = temp_path("dff");
    std::fs::write(&dff, build_dff()).unwrap();
    assert!(DsdReader::open(&dff).is_ok(), "DFF fixture must open");
    let _ = std::fs::remove_file(&dff);

    let tta = temp_path("tta");
    std::fs::write(&tta, build_tta()).unwrap();
    assert!(TtaDecoder::open(&tta).is_ok(), "TTA fixture must open");
    let _ = std::fs::remove_file(&tta);

    let opus = temp_path("opus");
    std::fs::write(&opus, build_ogg_opus()).unwrap();
    assert!(OpusSource::open(&opus).is_ok(), "Opus fixture must open");
    let _ = std::fs::remove_file(&opus);

    let parsed = CueSheet::parse(
        "FILE \"x.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"T\"\n    INDEX 01 00:01:00\n",
    )
    .expect("CUE fixture must parse");
    assert_eq!(parsed.tracks.len(), 1);
}
