//! CUE sheet parser and album image indexer.
//!
//! Provides parsing of standalone `.cue` files and embedded `CUESHEET` tags
//! (e.g. in FLAC Vorbis comments or APEv2 tags) for sample-accurate gapless
//! playback of single-file album images.
//!
//! Standard CUE frame rate is 75 frames per second (1 CD sector = 588 audio
//! samples at 44.1 kHz).

use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum CueParseError {
    #[error("Empty CUE sheet")]
    Empty,
    #[error("IO error: {0}")]
    Io(String),
    #[error("Syntax error at line {line}: {message}")]
    SyntaxError { line: usize, message: String },
    #[error("Invalid time format `{0}` (expected mm:ss:ff)")]
    InvalidTime(String),
}

/// A specific index position within a CUE track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueIndex {
    /// Index number (0 = pregap index, 1 = track start, 2..=99 = sub-indices).
    pub number: u8,
    pub minutes: u32,
    pub seconds: u32,
    /// 75 fps CD audio sectors (0..74).
    pub frames: u32,
}

impl CueIndex {
    /// Parse `mm:ss:ff` string into a `CueIndex`.
    pub fn parse(number: u8, time_str: &str) -> Result<Self, CueParseError> {
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() != 3 {
            return Err(CueParseError::InvalidTime(time_str.to_string()));
        }
        let minutes = parts[0]
            .parse::<u32>()
            .map_err(|_| CueParseError::InvalidTime(time_str.to_string()))?;
        let seconds = parts[1]
            .parse::<u32>()
            .map_err(|_| CueParseError::InvalidTime(time_str.to_string()))?;
        let frames = parts[2]
            .parse::<u32>()
            .map_err(|_| CueParseError::InvalidTime(time_str.to_string()))?;

        Ok(Self {
            number,
            minutes,
            seconds,
            frames,
        })
    }

    /// Offset from album start in whole seconds (fractional).
    pub fn total_seconds(&self) -> f64 {
        self.minutes as f64 * 60.0 + self.seconds as f64 + (self.frames as f64 / 75.0)
    }

    /// Offset from album start in audio samples at the specified sample rate.
    pub fn total_samples(&self, sample_rate: u32) -> u64 {
        let total_frames =
            self.minutes as u64 * 60 * 75 + self.seconds as u64 * 75 + self.frames as u64;
        (total_frames as f64 * (sample_rate as f64 / 75.0)).round() as u64
    }
}

/// A single track parsed from a CUE sheet.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CueTrack {
    /// 1-based track number.
    pub number: u32,
    /// Track datatype (e.g. "AUDIO").
    pub track_type: String,
    pub title: Option<String>,
    pub performer: Option<String>,
    pub songwriter: Option<String>,
    pub isrc: Option<String>,
    /// Referenced audio filename (if specified before or inside this track).
    pub file: Option<String>,
    /// Pregap time offset in fractional seconds if explicitly defined.
    pub pregap_secs: Option<f64>,
    /// Postgap time offset in fractional seconds if explicitly defined.
    pub postgap_secs: Option<f64>,
    /// List of indices (sorted by index number).
    pub indexes: Vec<CueIndex>,
}

impl CueTrack {
    /// Start time of the track in seconds. Prefers `INDEX 01` (track start);
    /// falls back to `INDEX 00` or 0.0 if missing.
    pub fn start_time_seconds(&self) -> f64 {
        if let Some(idx) = self.indexes.iter().find(|i| i.number == 1) {
            idx.total_seconds()
        } else if let Some(idx) = self.indexes.first() {
            idx.total_seconds()
        } else {
            0.0
        }
    }

    /// Start offset in audio samples at the specified sample rate.
    pub fn start_sample(&self, sample_rate: u32) -> u64 {
        if let Some(idx) = self.indexes.iter().find(|i| i.number == 1) {
            idx.total_samples(sample_rate)
        } else if let Some(idx) = self.indexes.first() {
            idx.total_samples(sample_rate)
        } else {
            0
        }
    }

    /// Pregap start time in seconds (from `INDEX 00`) if present.
    pub fn pregap_start_seconds(&self) -> Option<f64> {
        self.indexes
            .iter()
            .find(|i| i.number == 0)
            .map(|i| i.total_seconds())
    }
}

/// A complete parsed CUE sheet representing an album or compilation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CueSheet {
    pub title: Option<String>,
    pub performer: Option<String>,
    pub songwriter: Option<String>,
    pub genre: Option<String>,
    pub date: Option<String>,
    pub disc_id: Option<String>,
    pub comment: Option<String>,
    /// Referenced audio files.
    pub files: Vec<String>,
    /// Ordered list of tracks.
    pub tracks: Vec<CueTrack>,
}

impl CueSheet {
    /// Parse a CUE sheet from a string slice (supports CRLF and LF).
    pub fn parse(text: &str) -> Result<Self, CueParseError> {
        let mut sheet = CueSheet::default();
        let mut current_file: Option<String> = None;
        let mut current_track: Option<CueTrack> = None;

        for (line_idx, line) in text.lines().enumerate() {
            let line_num = line_idx + 1;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
                continue;
            }

            let mut tokens = tokenize_cue_line(trimmed);
            if tokens.is_empty() {
                continue;
            }

            let cmd = tokens.remove(0).to_uppercase();
            match cmd.as_str() {
                "REM" => {
                    if tokens.len() >= 2 {
                        let sub = tokens.remove(0).to_uppercase();
                        let val = tokens.join(" ");
                        match sub.as_str() {
                            "GENRE" => sheet.genre = Some(val),
                            "DATE" | "YEAR" => sheet.date = Some(val),
                            "DISCID" => sheet.disc_id = Some(val),
                            "COMMENT" => sheet.comment = Some(val),
                            _ => {}
                        }
                    }
                }
                "PERFORMER" => {
                    let val = tokens.join(" ");
                    if let Some(track) = current_track.as_mut() {
                        track.performer = Some(val);
                    } else {
                        sheet.performer = Some(val);
                    }
                }
                "TITLE" => {
                    let val = tokens.join(" ");
                    if let Some(track) = current_track.as_mut() {
                        track.title = Some(val);
                    } else {
                        sheet.title = Some(val);
                    }
                }
                "SONGWRITER" => {
                    let val = tokens.join(" ");
                    if let Some(track) = current_track.as_mut() {
                        track.songwriter = Some(val);
                    } else {
                        sheet.songwriter = Some(val);
                    }
                }
                "FILE" => {
                    if let Some(track) = current_track.take() {
                        sheet.tracks.push(track);
                    }
                    if !tokens.is_empty() {
                        let file_name = tokens[0].clone();
                        sheet.files.push(file_name.clone());
                        current_file = Some(file_name);
                    }
                }
                "TRACK" => {
                    if let Some(track) = current_track.take() {
                        sheet.tracks.push(track);
                    }
                    if tokens.is_empty() {
                        return Err(CueParseError::SyntaxError {
                            line: line_num,
                            message: "Missing track number".into(),
                        });
                    }
                    let num = tokens[0]
                        .parse::<u32>()
                        .map_err(|_| CueParseError::SyntaxError {
                            line: line_num,
                            message: format!("Invalid track number `{}`", tokens[0]),
                        })?;
                    let track_type = tokens.get(1).cloned().unwrap_or_else(|| "AUDIO".into());

                    current_track = Some(CueTrack {
                        number: num,
                        track_type,
                        file: current_file.clone(),
                        ..Default::default()
                    });
                }
                "INDEX" => {
                    let track =
                        current_track
                            .as_mut()
                            .ok_or_else(|| CueParseError::SyntaxError {
                                line: line_num,
                                message: "INDEX specified outside of TRACK".into(),
                            })?;
                    if tokens.len() < 2 {
                        return Err(CueParseError::SyntaxError {
                            line: line_num,
                            message: "INDEX requires index number and mm:ss:ff timestamp".into(),
                        });
                    }
                    let idx_num =
                        tokens[0]
                            .parse::<u8>()
                            .map_err(|_| CueParseError::SyntaxError {
                                line: line_num,
                                message: format!("Invalid index number `{}`", tokens[0]),
                            })?;
                    let index = CueIndex::parse(idx_num, &tokens[1])?;
                    track.indexes.push(index);
                }
                "PREGAP" => {
                    let track =
                        current_track
                            .as_mut()
                            .ok_or_else(|| CueParseError::SyntaxError {
                                line: line_num,
                                message: "PREGAP specified outside of TRACK".into(),
                            })?;
                    if let Some(time_str) = tokens.first() {
                        let idx = CueIndex::parse(0, time_str)?;
                        track.pregap_secs = Some(idx.total_seconds());
                    }
                }
                "POSTGAP" => {
                    let track =
                        current_track
                            .as_mut()
                            .ok_or_else(|| CueParseError::SyntaxError {
                                line: line_num,
                                message: "POSTGAP specified outside of TRACK".into(),
                            })?;
                    if let Some(time_str) = tokens.first() {
                        let idx = CueIndex::parse(0, time_str)?;
                        track.postgap_secs = Some(idx.total_seconds());
                    }
                }
                "ISRC" => {
                    if let Some(track) = current_track.as_mut() {
                        track.isrc = tokens.first().cloned();
                    }
                }
                _ => {
                    // Ignore unrecognized commands gracefully
                }
            }
        }

        if let Some(track) = current_track {
            sheet.tracks.push(track);
        }

        if sheet.tracks.is_empty() && sheet.files.is_empty() {
            return Err(CueParseError::Empty);
        }

        Ok(sheet)
    }

    /// Read and parse a `.cue` file from filesystem.
    pub fn parse_file(path: &Path) -> Result<Self, CueParseError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| CueParseError::Io(format!("{}: {}", path.display(), e)))?;
        Self::parse(&content)
    }

    /// Total number of tracks in the CUE sheet.
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// Find track by 1-based track number.
    pub fn get_track(&self, num: u32) -> Option<&CueTrack> {
        self.tracks.iter().find(|t| t.number == num)
    }

    /// Calculate track durations for single-file album images.
    ///
    /// Returns a list of `(track_number, start_seconds, duration_seconds)`.
    /// The last track's duration is computed from `total_duration_secs` if provided.
    pub fn calculate_track_durations(
        &self,
        total_duration_secs: Option<f64>,
    ) -> Vec<(u32, f64, Option<f64>)> {
        let mut results = Vec::with_capacity(self.tracks.len());
        for i in 0..self.tracks.len() {
            let cur = &self.tracks[i];
            let start = cur.start_time_seconds();
            let duration = if i + 1 < self.tracks.len() {
                let next_start = self.tracks[i + 1].start_time_seconds();
                Some((next_start - start).max(0.0))
            } else {
                total_duration_secs.map(|tot| (tot - start).max(0.0))
            };
            results.push((cur.number, start, duration));
        }
        results
    }
}

/// Tokenize a line from a CUE sheet respecting double quotes.
fn tokenize_cue_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for c in line.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c.is_whitespace() && !in_quotes {
            if !current.is_empty() {
                tokens.push(current.clone());
                current.clear();
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CUE: &str = r#"
REM GENRE "Progressive Rock"
REM DATE 1973
REM DISCID 8A0B1209
PERFORMER "Pink Floyd"
TITLE "The Dark Side of the Moon"
FILE "DarkSide.flac" WAVE
  TRACK 01 AUDIO
    TITLE "Speak to Me"
    PERFORMER "Pink Floyd"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Breathe"
    PERFORMER "Pink Floyd"
    INDEX 00 01:05:32
    INDEX 01 01:08:00
  TRACK 03 AUDIO
    TITLE "On the Run"
    PERFORMER "Pink Floyd"
    INDEX 01 03:57:25
"#;

    #[test]
    fn test_parse_cue_sheet() {
        let cue = CueSheet::parse(SAMPLE_CUE).expect("valid CUE sheet");
        assert_eq!(cue.performer.as_deref(), Some("Pink Floyd"));
        assert_eq!(cue.title.as_deref(), Some("The Dark Side of the Moon"));
        assert_eq!(cue.genre.as_deref(), Some("Progressive Rock"));
        assert_eq!(cue.date.as_deref(), Some("1973"));
        assert_eq!(cue.files, vec!["DarkSide.flac"]);
        assert_eq!(cue.track_count(), 3);

        let t1 = cue.get_track(1).unwrap();
        assert_eq!(t1.title.as_deref(), Some("Speak to Me"));
        assert_eq!(t1.start_time_seconds(), 0.0);
        assert_eq!(t1.start_sample(44100), 0);

        let t2 = cue.get_track(2).unwrap();
        assert_eq!(t2.title.as_deref(), Some("Breathe"));
        assert_eq!(t2.indexes.len(), 2);
        assert_eq!(t2.start_time_seconds(), 68.0); // 1 min 8 sec = 68.0 sec
        assert_eq!(t2.start_sample(44100), 68 * 44100);

        let t3 = cue.get_track(3).unwrap();
        assert_eq!(t3.title.as_deref(), Some("On the Run"));
        // 3 min 57 sec + 25 frames (25/75 = 1/3 sec)
        let expected_sec = 3.0 * 60.0 + 57.0 + (25.0 / 75.0);
        assert!((t3.start_time_seconds() - expected_sec).abs() < 1e-4);
    }

    #[test]
    fn test_track_durations() {
        let cue = CueSheet::parse(SAMPLE_CUE).expect("valid CUE sheet");
        let durations = cue.calculate_track_durations(Some(600.0));
        assert_eq!(durations.len(), 3);
        assert_eq!(durations[0].0, 1);
        assert_eq!(durations[0].1, 0.0);
        assert_eq!(durations[0].2, Some(68.0)); // 68.0 - 0.0

        assert_eq!(durations[1].0, 2);
        assert_eq!(durations[1].1, 68.0);
        let t3_start = 3.0 * 60.0 + 57.0 + (25.0 / 75.0);
        assert_eq!(durations[1].2, Some(t3_start - 68.0));
    }
}
