//! Offline loudness scanning.
//!
//! Decodes a file end to end on a background thread and measures its
//! loudness with the **same** [`LoudnessMeter`] used everywhere else in the
//! engine — full BS.1770-4 K-weighting, absolute + relative gating,
//! short-term/LRA, and the shared 4× polyphase FIR true-peak detector.
//!
//! The metadata the scanner produces (`integrated_lufs`, `lra`, `dbtp`) is
//! therefore identical in definition to what the playback chain measures,
//! not a separate ungated estimate.

use std::path::Path;

use crate::decode::{DecodeError, Decoder};
use crate::dsp::LoudnessMeter;

/// Measured loudness of a scanned track.
#[derive(Debug, Clone, PartialEq)]
pub struct LoudnessScanResult {
    /// EBU R128 integrated loudness in LUFS (dual-threshold gated per
    /// BS.1770-4 §3.2, via [`LoudnessMeter::snapshot`]).
    pub ebu_r128_loudness: Option<f32>,
    /// True peak in dBTP — the shared 4× oversampled FIR estimate, same
    /// detector the limiter and the loudness meter use.  Never a plain
    /// sample peak.
    pub ebu_r128_peak_dbtp: Option<f32>,
    /// ReplayGain 2.0 track gain in dB (-18.0 - integrated_lufs).
    pub replaygain_track_db: Option<f32>,
    /// ReplayGain track peak (linear amplitude).
    pub replaygain_track_peak: Option<f32>,
    /// Loudness range in LU (10th–95th percentile of gated blocks).
    pub lra_lu: Option<f32>,
    /// Frames of audio actually decoded and measured.
    pub frames_scanned: u64,
}

/// Decode `path` end to end and measure its loudness.
///
/// Convenience wrapper around [`scan_decoder`] that opens a [`Decoder`] from
/// the filesystem. Prefer [`scan_decoder`] directly if you already hold a
/// [`Decoder`] (e.g. from a network stream or memory buffer) to avoid
/// re-opening the file.
///
/// Returns `None` if the file cannot be opened or yields no measurable
/// audio (e.g. a DSD file, which the Symphonia path does not decode).
///
/// Measures the native channel stream directly per ITU-R BS.1770-4 / EBU R128
/// with semantic channel weighting rather than losing surround weighting by
/// downmixing before measurement.
pub fn scan_track_loudness(path: &Path) -> Option<LoudnessScanResult> {
    let mut decoder = Decoder::open(path).ok()?;
    scan_decoder(&mut decoder)
}

/// Decode `decoder` end to end and measure its loudness.
///
/// Returns `None` if the decoder yields no measurable audio frames.
///
/// This is the primary entry point — it works with any [`Decoder`] variant
/// (Symphonia, DSD/PCM, APE, Opus, WavPack, TTA) and any byte source the
/// decoder was opened from (file, memory buffer, network stream). Call
/// [`scan_track_loudness`] if you only have a filesystem path and want the
/// single-call convenience.
///
/// # Panics
///
/// Never panics. Returns `None` on decode errors or empty streams.
pub fn scan_decoder(decoder: &mut Decoder) -> Option<LoudnessScanResult> {
    let sample_rate = decoder.info().sample_rate as f32;
    let src_channels = decoder.info().channels;
    let layout = decoder.format_info().channel_layout.clone();
    let mut meter = LoudnessMeter::new(sample_rate, src_channels);
    meter.set_channel_layout(&layout);
    let mut frames_scanned = 0u64;

    const CHUNK_FRAMES: usize = 8192;
    loop {
        match decoder.decode_next(CHUNK_FRAMES) {
            Ok(chunk) => {
                let channels = chunk.channels.max(1);
                meter.set_channel_layout(&chunk.channel_layout);
                meter.process_interleaved(&chunk.samples, channels);
                frames_scanned += chunk.frame_count as u64;
            }
            Err(DecodeError::EndOfStream) => break,
            // Stop on any other decode error; the frames measured so far are
            // still a usable estimate of the track's loudness.
            Err(_) => break,
        }
    }

    if frames_scanned == 0 {
        return None;
    }

    let m = meter.snapshot();
    let ebu_r128_loudness = if m.integrated_lufs.is_finite() {
        Some(m.integrated_lufs)
    } else {
        None
    };
    let ebu_r128_peak_dbtp = if m.true_peak_linear > 0.0 {
        Some(m.true_peak_dbtp())
    } else {
        None
    };
    // ReplayGain 2.0 target is -18.0 LUFS per BS.1770 specification
    let replaygain_track_db = ebu_r128_loudness.map(|lufs| -18.0 - lufs);
    let replaygain_track_peak = if m.true_peak_linear > 0.0 {
        Some(m.true_peak_linear)
    } else {
        None
    };

    Some(LoudnessScanResult {
        ebu_r128_loudness,
        ebu_r128_peak_dbtp,
        replaygain_track_db,
        replaygain_track_peak,
        lra_lu: if m.lra_lu.is_finite() && m.lra_lu > 0.0 {
            Some(m.lra_lu)
        } else {
            None
        },
        frames_scanned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::SymphoniaDecoder;
    use std::io::Write;

    /// Write a stereo 16-bit PCM WAV containing a `freq` Hz sine at `amplitude`.
    fn write_sine_wav(path: &Path, sample_rate: u32, seconds: usize, freq: f32, amplitude: f32) {
        let n_frames = sample_rate as usize * seconds;
        let mut data = Vec::with_capacity(n_frames * 2 * 2);
        for i in 0..n_frames {
            let s = amplitude
                * (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin();
            let v = (s * 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&v.to_le_bytes());
        }
        let byte_rate: u32 = sample_rate * 2 * 2; // 2 channels × 16-bit
        let block_align: u16 = 2 * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&wav).unwrap();
    }

    fn temp_wav_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "engine_scan_test_{}_{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn test_scan_full_scale_1khz_sine() {
        let path = temp_wav_path();
        write_sine_wav(&path, 48000, 3, 1000.0, 1.0);

        let result = scan_track_loudness(&path).expect("scan should succeed");
        let _ = std::fs::remove_file(&path);

        // Stereo full-scale 1 kHz sine: per-channel mean square 0.5, summed
        // over 2 channels = 1.0, plus +0.67 dB K-weight at 1 kHz, gives
        // -0.691 + 10*log10(10^0.067) ≈ -0.02 LUFS.
        let lufs = result.ebu_r128_loudness.expect("loudness measured");
        assert!(
            (lufs - (-0.02)).abs() < 0.5,
            "expected ≈ -0.02 LUFS, got {lufs:.2}"
        );

        // Full-scale i16 sine peaks at 32767/32768 ≈ -0.0003 dBTP.
        let peak = result.ebu_r128_peak_dbtp.expect("peak measured");
        assert!(
            (peak - 0.0).abs() < 0.1,
            "peak should be ≈ 0 dBTP, got {peak:.3}"
        );

        assert!(
            result.frames_scanned >= 48000 * 3 - 10,
            "should scan the full track, scanned {}",
            result.frames_scanned
        );
    }

    #[test]
    fn test_decode_next_packet_tail_not_dropped() {
        // Regression: decode_next(4096) used to silently drop the tail of the
        // packet straddling each call boundary (4096 mod 1152 = 640, so 512 of
        // every 1152-frame WAV packet was lost — 12.5% of all audio). The full
        // track must be delivered across calls.
        let path = temp_wav_path();
        write_sine_wav(&path, 48000, 3, 1000.0, 0.5);

        let mut decoder = SymphoniaDecoder::open(&path).unwrap();
        let mut total = 0u64;
        let mut last_frame_count = 0usize;
        while let Ok(c) = decoder.decode_next(4096) {
            last_frame_count = c.frame_count;
            total += c.frame_count as u64;
            assert_eq!(
                c.samples.len(),
                c.frame_count * c.channels,
                "frame_count must match sample count"
            );
        }
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            total,
            48000 * 3,
            "all frames must be delivered, got {total}"
        );
        assert!(last_frame_count > 0, "last chunk should not be empty");
    }

    #[test]
    fn test_scan_quiet_sine_measures_lower() {
        let path = temp_wav_path();
        // -20 dB amplitude (0.1): should measure ~20 LU quieter than full scale.
        write_sine_wav(&path, 44100, 2, 1000.0, 0.1);

        let result = scan_track_loudness(&path).expect("scan should succeed");
        let _ = std::fs::remove_file(&path);

        let lufs = result.ebu_r128_loudness.expect("loudness measured");
        // Full-scale stereo 1 kHz ≈ -0.02 LUFS; -20 dB amplitude → ≈ -20.02 LUFS.
        assert!(
            (lufs - (-20.02)).abs() < 0.6,
            "expected ≈ -20.02 LUFS for -20 dB sine, got {lufs:.2}"
        );
        let peak = result.ebu_r128_peak_dbtp.expect("peak measured");
        assert!(
            (peak - (-20.0)).abs() < 0.2,
            "peak should be ≈ -20 dBTP, got {peak:.3}"
        );
    }
}
