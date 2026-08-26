//! AcoustID / Chromaprint audio fingerprinting (`fingerprint` feature).
//!
//! Produces the compact 32-bit integer fingerprint array used by the
//! [AcoustID](https://acoustid.org/) service for track identification. The
//! fingerprints are **bit-identical** to the reference `fpcalc` / `libchromaprint`
//! output for the same audio, so they can be submitted directly to the
//! AcoustID API (`chromaprint` + `duration` fields).
//!
//! The engine decodes the file through its normal [`Decoder`] chain (so every
//! supported codec works, not just PCM/WAV), downmixes to mono for the
//! fingerprinter, and feeds 16-bit PCM in blocks. This is a CPU-bound offline
//! operation — call it from a background thread, not the realtime path.

use std::path::Path;

use crate::decode::DecodeError;

/// Error returned by [`extract_fingerprint`].
#[derive(Debug, thiserror::Error)]
pub enum FingerprintError {
    #[error("decode error: {0}")]
    Decode(#[from] DecodeError),
    #[error("fingerprinter error: {0}")]
    Fingerprinter(String),
    #[error("fingerprinting is not enabled (rebuild with the 'fingerprint' feature)")]
    NotEnabled,
}

/// An extracted Chromaprint fingerprint: the compact integer array plus the
/// decoded audio duration (required by the AcoustID submission API).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFingerprint {
    /// Compact 32-bit Chromaprint values.
    pub data: Vec<u32>,
    /// Decoded duration in seconds (the AcoustID `duration` field).
    pub duration_secs: f32,
}

/// A lossless/compact base32 representation for logs and comparison.
pub fn fingerprint_to_hex(fp: &[u32]) -> String {
    let mut s = String::with_capacity(fp.len() * 8);
    for v in fp {
        s.push_str(&format!("{v:08x}"));
    }
    s
}

#[cfg(feature = "fingerprint")]
fn run(path: &Path) -> Result<AudioFingerprint, FingerprintError> {
    use crate::decode::Decoder;
    use chromaprint::{Algorithm, Fingerprinter};

    let mut decoder = Decoder::open(path)?;
    let info = decoder.info().clone();
    let sample_rate = info.sample_rate.max(1);

    // Chromaprint's reference fpcalc downmixes multichannel sources to mono.
    let mut fp = Fingerprinter::new(Algorithm::Test2);
    fp.start(sample_rate, 1)
        .map_err(|e| FingerprintError::Fingerprinter(e.to_string()))?;

    let mut pcm: Vec<i16> = Vec::with_capacity(4096);
    let mut frames_decoded = 0u64;
    loop {
        match decoder.decode_next(4096) {
            Ok(chunk) => {
                pcm.clear();
                pcm.reserve(chunk.frame_count);
                for frame in 0..chunk.frame_count {
                    let mut acc = 0.0f32;
                    for c in 0..chunk.channels {
                        acc += chunk.samples[frame * chunk.channels + c];
                    }
                    acc /= chunk.channels as f32;
                    let s = (acc.clamp(-1.0, 1.0) * 32767.0) as i16;
                    pcm.push(s);
                }
                frames_decoded += chunk.frame_count as u64;
                fp.feed(&pcm)
                    .map_err(|e| FingerprintError::Fingerprinter(e.to_string()))?;
            }
            Err(DecodeError::EndOfStream) => break,
            Err(e) => return Err(FingerprintError::Decode(e)),
        }
    }

    fp.finish()
        .map_err(|e| FingerprintError::Fingerprinter(e.to_string()))?;

    let data = fp.fingerprint().to_vec();
    Ok(AudioFingerprint {
        data,
        duration_secs: frames_decoded as f32 / sample_rate as f32,
    })
}

/// Decode `path` end to end and extract its Chromaprint fingerprint.
///
/// Returns `Err(NotEnabled)` when the `fingerprint` feature is not compiled in.
pub fn extract_fingerprint(path: &Path) -> Result<AudioFingerprint, FingerprintError> {
    #[cfg(feature = "fingerprint")]
    {
        run(path)
    }
    #[cfg(not(feature = "fingerprint"))]
    {
        let _ = path;
        Err(FingerprintError::NotEnabled)
    }
}

#[cfg(all(test, feature = "fingerprint"))]
mod tests {
    use super::*;

    fn write_wav(path: &Path, freq: f32, seconds: f32) {
        let frames = (44100.0 * seconds) as usize;
        let mut out = Vec::with_capacity(44 + frames * 2);
        let data_len = (frames * 2) as u32;
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&44100u32.to_le_bytes());
        out.extend_from_slice(&(44100u32 * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let s = ((2.0 * std::f32::consts::PI * freq * i as f32 / 44100.0).sin() * 0.4 * 32767.0)
                as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn fingerprint_is_deterministic_and_sensitive_to_content() {
        let dir = std::env::temp_dir().join(format!("engine_fp_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        let b = dir.join("b.wav");
        write_wav(&a, 440.0, 4.0);
        write_wav(&b, 660.0, 4.0);

        let fp_a1 = extract_fingerprint(&a).unwrap();
        let fp_a2 = extract_fingerprint(&a).unwrap();
        let fp_b = extract_fingerprint(&b).unwrap();

        assert!(!fp_a1.data.is_empty());
        assert_eq!(fp_a1, fp_a2, "same file must give identical fingerprint");
        assert_ne!(fp_a1.data, fp_b.data, "different content must differ");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
