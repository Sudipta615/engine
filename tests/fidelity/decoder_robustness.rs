//! Decoder robustness tests (spec §31, §28): a desktop audio engine consumes
//! untrusted files, so malformed input must be rejected safely — never a
//! panic, never unbounded allocation — and unsupported codecs must fail with
//! an explicit, honest error rather than a silent fallback.
//!
//! These tests write small fixture files to the OS temp directory and drive
//! the real `Decoder::open` dispatch (extension routing + Symphonia probe).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::decode::{DecodeError, Decoder};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temp path for this test process.
fn temp_path(ext: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "engine-decoder-robustness-{}-{n}.{ext}",
        std::process::id()
    ))
}

fn write(path: &PathBuf, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write fixture");
}

#[test]
fn garbage_bytes_with_recognized_extension_are_rejected_not_panicked() {
    for ext in ["flac", "wav", "mp3", "aac", "ogg", "opus", "ape", "dsf"] {
        let path = temp_path(ext);
        // Non-zero garbage: looks like a file but no valid header/frames.
        write(&path, &[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33]);
        let result = Decoder::open(&path);
        assert!(result.is_err(), "{ext}: garbage bytes must not decode");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn empty_file_is_rejected() {
    let path = temp_path("wav");
    write(&path, &[]);
    let result = Decoder::open(&path);
    assert!(result.is_err(), "empty file must be rejected");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn truncated_wav_is_rejected_or_decodes_without_panic() {
    // A minimal 44.1 kHz / 16-bit / stereo WAV header (44 bytes) whose
    // declared data length far exceeds the actual bytes on disk. The decoder
    // must either fail cleanly or decode the available portion — never panic
    // and never allocate based on the lying chunk size.
    let mut header: Vec<u8> = Vec::new();
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes()[..]);
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes()[..]);
    header.extend_from_slice(&1u16.to_le_bytes()[..]); // PCM
    header.extend_from_slice(&2u16.to_le_bytes()[..]); // stereo
    header.extend_from_slice(&44_100u32.to_le_bytes()[..]);
    header.extend_from_slice(&(44_100u32 * 4).to_le_bytes()[..]); // byte rate
    header.extend_from_slice(&4u16.to_le_bytes()[..]); // block align
    header.extend_from_slice(&16u16.to_le_bytes()[..]); // 16-bit
    header.extend_from_slice(b"data");
    header.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes()[..]); // lying data size
    header.extend_from_slice(&[0u8; 32]); // a few real bytes, then EOF

    let path = temp_path("wav");
    write(&path, &header);

    // The exact outcome (Err vs partial decode) is implementation-defined;
    // the contract is: no panic, and the lying chunk size must not drive an
    // unbounded allocation or an absurd reported duration.
    match Decoder::open(&path) {
        Ok(dec) => {
            let duration = dec.info().duration_secs;
            assert!(
                duration < 3600.0,
                "lying header must not produce a >1h duration, got {duration}"
            );
        }
        Err(e) => {
            assert!(
                !matches!(e, DecodeError::EndOfStream),
                "a truncated header must not be reported as a clean EOS"
            );
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn declared_unavailable_codecs_fail_explicitly() {
    // TAK / Musepack are declared-but-unavailable in this build (and WavPack
    // too when `codec-wavpack` is off): the dispatch must reject them with a
    // named error, not a generic probe failure that hides which codec the
    // file was. (TTA is a supported, natively-decoded format — see
    // `garbage_bytes…` above for its robustness coverage and
    // `src/decode/tta.rs` for the decoder.)
    let mut codecs: Vec<(&str, &str)> = vec![("tak", "TAK"), ("mpc", "Musepack")];
    #[cfg(not(feature = "codec-wavpack"))]
    codecs.push(("wv", "WavPack"));
    for (ext, name) in codecs {
        let path = temp_path(ext);
        write(&path, &[0u8; 16]);
        match Decoder::open(&path) {
            Ok(_) => panic!("{ext}: must not decode (declared unavailable)"),
            Err(DecodeError::UnsupportedFormat(msg)) => {
                assert!(
                    msg.contains(name),
                    "{ext}: UnsupportedFormat must name {name}, got: {msg}"
                );
            }
            Err(other) => panic!(
                "{ext}: expected UnsupportedFormat naming {name}, got {}",
                other
            ),
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn tta_garbage_bytes_are_rejected_cleanly() {
    // TTA is supported: garbage bytes must reach the real TTA parser and be
    // rejected with a clean error (never a panic, never a silent decode).
    let path = temp_path("tta");
    write(&path, &[0u8; 16]);
    match Decoder::open(&path) {
        Ok(_) => panic!("16 zero bytes are not a valid TTA file"),
        Err(DecodeError::UnsupportedFormat(_)) | Err(DecodeError::FileOpen(_)) => {}
        Err(other) => panic!("expected a clean parse error, got {}", other),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn nonexistent_file_reports_io_error_not_panic() {
    let path = temp_path("flac");
    let _ = std::fs::remove_file(&path); // ensure absent
    match Decoder::open(&path) {
        Ok(_) => panic!("missing file must not decode"),
        Err(DecodeError::FileOpen(_)) | Err(DecodeError::Io(_)) => {}
        Err(other) => panic!("missing file should be FileOpen/Io, got {}", other),
    }
}
