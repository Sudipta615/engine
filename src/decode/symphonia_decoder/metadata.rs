//! Track metadata extraction (title/artist/album/duration) and loudness tags.

use std::fs::File;
use std::path::Path;

use symphonia::core::{
    formats::probe::Hint,
    formats::FormatOptions,
    io::MediaSourceStream,
    meta::{MetadataOptions, StandardTag},
    units::Timestamp,
};

pub fn extract_track_metadata(path: &Path) -> (String, String, String, f64, String) {
    let default_title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown Track")
        .to_string();
    let mut title = default_title.clone();
    let mut artist = "Unknown Artist".to_string();
    let mut album = "Unknown Album".to_string();
    let mut duration_secs = 0.0;

    if let Ok(file) = File::open(path) {
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let metadata_opts = MetadataOptions::default();
        let format_opts = FormatOptions::default();

        if let Ok(mut format_reader) =
            symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts)
        {
            if let Some(track) = format_reader.tracks().first() {
                if let Some(tb) = track.time_base {
                    if let Some(n_frames) = track.num_frames {
                        if let Some(time) = tb.calc_time(Timestamp::new(n_frames as i64)) {
                            duration_secs = time.as_secs_f64();
                        }
                    }
                }
            }

            if let Some(current) = format_reader.metadata().current() {
                for tag in &current.media.tags {
                    if let Some(std) = &tag.std {
                        match std {
                            StandardTag::TrackTitle(val) if !val.is_empty() => {
                                title = val.to_string();
                            }
                            StandardTag::Artist(val) if !val.is_empty() => {
                                artist = val.to_string();
                            }
                            StandardTag::Album(val) if !val.is_empty() => {
                                album = val.to_string();
                            }
                            _ => {}
                        }
                    } else {
                        let key_str = tag.raw.key.to_lowercase();
                        let val_str = tag.raw.value.to_string();
                        if (key_str.contains("title") || key_str == "tracktitle")
                            && !val_str.is_empty()
                        {
                            title = val_str;
                        } else if key_str.contains("artist") && !val_str.is_empty() {
                            artist = val_str;
                        } else if key_str.contains("album") && !val_str.is_empty() {
                            album = val_str;
                        }
                    }
                }
            }
        }
    }

    let duration_str = if duration_secs > 0.0 {
        format!(
            "{}:{:02}",
            (duration_secs as i32) / 60,
            (duration_secs as i32) % 60
        )
    } else {
        "0:00".to_string()
    };

    (title, artist, album, duration_secs, duration_str)
}

/// Extract ReplayGain / EBU R128 loudness metadata from file tags, for
/// Symphonia-probeable formats. Ogg Opus is handled by
/// `decode::extract_loudness_metadata` (OpusTags cannot be read by
/// Symphonia's probe), which dispatches here for everything else.
pub fn extract_loudness_metadata_symphonia(path: &Path) -> crate::dsp::LoudnessMetadata {
    use crate::dsp::LoudnessMetadata;

    let mut meta = LoudnessMetadata::default();

    let parse_f32 = |s: &str| -> Option<f32> {
        // Tags often look like "-6.34 dB" — strip non-numeric prefix/suffix.
        let trimmed: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
            .collect();
        trimmed.parse::<f32>().ok().filter(|v| v.is_finite())
    };

    // R128 tag values are integer LUFS × 100 (per the EBU R128 tag spec).
    // Some encoders write the value as a plain float LUFS string; we detect
    // both forms by attempting the integer-÷-100 conversion first.
    let parse_r128 = |s: &str| -> Option<f32> {
        let trimmed: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
            .collect();
        if let Ok(v) = trimmed.parse::<f32>() {
            if v.is_finite() {
                // Heuristic: if |v| > 200 it's almost certainly the encoded
                // integer form (a typical track is -23 LUFS = -2300 encoded).
                // Otherwise treat it as a plain LUFS value.
                if v.abs() > 200.0 {
                    return Some(v / 100.0);
                }
                return Some(v);
            }
        }
        None
    };

    if let Ok(file) = File::open(path) {
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let metadata_opts = MetadataOptions::default();
        let format_opts = FormatOptions::default();

        if let Ok(mut format_reader) =
            symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts)
        {
            if let Some(current) = format_reader.metadata().current() {
                for tag in &current.media.tags {
                    if let Some(std) = &tag.std {
                        match std {
                            StandardTag::ReplayGainTrackGain(v) => {
                                meta.replaygain_track_db = parse_f32(v);
                            }
                            StandardTag::ReplayGainAlbumGain(v) => {
                                meta.replaygain_album_db = parse_f32(v);
                            }
                            StandardTag::ReplayGainTrackPeak(v) => {
                                meta.replaygain_track_peak = parse_f32(v);
                            }
                            StandardTag::ReplayGainAlbumPeak(v) => {
                                meta.replaygain_album_peak = parse_f32(v);
                            }
                            _ => {}
                        }
                    }
                    let key = tag.raw.key.to_lowercase();
                    let value = tag.raw.value.to_string();
                    if value.is_empty() {
                        continue;
                    }
                    if key == "replaygain_track_gain" && meta.replaygain_track_db.is_none() {
                        meta.replaygain_track_db = parse_f32(&value);
                    } else if key == "replaygain_album_gain" && meta.replaygain_album_db.is_none() {
                        meta.replaygain_album_db = parse_f32(&value);
                    } else if key == "replaygain_track_peak" && meta.replaygain_track_peak.is_none()
                    {
                        meta.replaygain_track_peak = parse_f32(&value);
                    } else if key == "replaygain_album_peak" && meta.replaygain_album_peak.is_none()
                    {
                        meta.replaygain_album_peak = parse_f32(&value);
                    } else if key == "r128_track_gain" {
                        meta.ebu_r128_loudness = parse_r128(&value);
                    } else if key == "r128_album_gain" {
                        // Reuse the same field — AlbumReplayGain mode reads
                        // replaygain_album_db, but if only R128 tags are
                        // present we treat them as the track loudness.
                        if meta.ebu_r128_loudness.is_none() {
                            meta.ebu_r128_loudness = parse_r128(&value);
                        }
                    }
                }
            }
        }
    }

    meta
}
