//! Transport and tempo clock state for plugins (Item 28).

/// Realtime transport and musical clock information provided to plugins.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportInfo {
    /// Current sample rate in Hz.
    pub sample_rate: f64,
    /// Musical tempo in beats per minute (BPM).
    pub tempo_bpm: f64,
    /// Musical position in quarter-notes / beats from project start.
    pub ppq_pos: f64,
    /// Time signature numerator (e.g. 4 for 4/4).
    pub time_sig_num: u16,
    /// Time signature denominator (e.g. 4 for 4/4).
    pub time_sig_den: u16,
    /// Transport is currently rolling / playing.
    pub is_playing: bool,
    /// Transport is recording.
    pub is_recording: bool,
    /// Loop playback is enabled.
    pub is_looping: bool,
    /// Loop start position in PPQ.
    pub loop_start_ppq: f64,
    /// Loop end position in PPQ.
    pub loop_end_ppq: f64,
}

impl Default for TransportInfo {
    fn default() -> Self {
        Self {
            sample_rate: 48000.0,
            tempo_bpm: 120.0,
            ppq_pos: 0.0,
            time_sig_num: 4,
            time_sig_den: 4,
            is_playing: true,
            is_recording: false,
            is_looping: false,
            loop_start_ppq: 0.0,
            loop_end_ppq: 0.0,
        }
    }
}
