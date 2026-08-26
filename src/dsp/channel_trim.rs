//! Per-channel trim, routing matrix, and LFE gain — the multichannel
//! management stage (§5 incremental).
//!
//! This stage runs in the **multichannel passthrough path only** (`>2`
//! channels), on every channel, before the pre-mix chain. Stereo playback
//! keeps the pipeline's balance control; these entries are intentionally not
//! applied to the front L/R pair of a downmixed stereo stream.
//!
//! Per-frame order within the stage (documented signal path):
//!
//! ```text
//! in[ch] → routing matrix (Σ src·matrix[src][dst]) → gain → polarity
//!        → fractional delay → LFE gain → out[ch]
//! ```
//!
//! Every sub-stage is individually bypassable; when nothing is enabled the
//! stage is a pure passthrough and `process_planes` returns immediately.

use config::{
    BassManagementConfig, ChannelEqConfig, ChannelRoutingConfig, ChannelTrimConfig, EqBandConfig,
    FilterType as ConfigFilterType, LfeConfig,
};

use crate::buffer::MAX_CHANNELS;
use crate::dsp::biquad::{BiquadCoeffsF32, BiquadStateF32, FilterType as BiquadFilterType};

/// Maximum cascade length for one multichannel EQ channel. The value matches
/// the public stereo EQ limit while keeping the per-channel state bounded.
pub const MAX_CHANNEL_EQ_BANDS: usize = 64;

/// Maximum per-channel delay (ms). Enough for driver timing alignment while
/// bounding memory: at 192 kHz this is 19 200 samples (a 32 KiB plane per
/// channel), at 384 kHz 38 400 (64 KiB).
pub const MAX_CHANNEL_DELAY_MS: f32 = 100.0;

/// dB → linear gain.
fn db_to_linear(db: f32) -> f32 {
    if db.is_finite() {
        10.0f32.powf(db / 20.0)
    } else {
        1.0
    }
}

fn channel_eq_coeffs(sample_rate: f32, band: &EqBandConfig) -> BiquadCoeffsF32 {
    let filter_type = match band.filter_type {
        ConfigFilterType::Peaking => BiquadFilterType::Peaking,
        ConfigFilterType::LowShelf => BiquadFilterType::Lowshelf,
        ConfigFilterType::HighShelf => BiquadFilterType::Highshelf,
        ConfigFilterType::LowPass => BiquadFilterType::Lowpass,
        ConfigFilterType::HighPass => BiquadFilterType::Highpass,
        ConfigFilterType::Bandpass => BiquadFilterType::Bandpass,
        ConfigFilterType::Notch => BiquadFilterType::Notch,
        ConfigFilterType::AllPass => BiquadFilterType::Allpass,
    };
    filter_type.compute_coeffs::<f32>(sample_rate, band.frequency, band.gain_db, band.q)
}

/// Next power of two ≥ `n` (delay-line capacity must be a power of two so
/// the modulo is a mask).
fn next_pow2(n: usize) -> usize {
    let mut p = 1usize;
    while p < n {
        p <<= 1;
    }
    p
}

pub struct ChannelTrimmer {
    enabled: bool,
    sample_rate: f32,
    /// Linear gain per channel (1.0 = unity).
    gains: Vec<f32>,
    /// Fractional delay in samples per channel (0.0 = none).
    delay_samples: Vec<f32>,
    /// Polarity inversion per channel.
    invert: Vec<bool>,
    /// Per-channel delay lines (circular, power-of-two capacity).
    delay_cap: usize,
    delay_mask: usize,
    delay_bufs: Vec<Vec<f32>>,
    delay_pos: Vec<usize>,
    /// Routing matrix `[src][dst]`; empty when bypassed.
    routing_enabled: bool,
    routing_ok: bool,
    routing: Vec<Vec<f32>>,
    /// LFE gain applied to channels whose role is LFE.
    lfe_enabled: bool,
    lfe_gain: f32,
    lfe_channels: Vec<usize>,
    /// LFE low-pass (bass management): a second-order minimum-phase LP at
    /// `crossover_hz`, applied to LFE-role channels after the LFE gain.
    lfe_lp_enabled: bool,
    lfe_lp_coeffs: BiquadCoeffsF32,
    lfe_lp_states: Vec<BiquadStateF32>,
    /// Fixed flags avoid a `Vec::contains` scan in the audio loop.
    lfe_channel_flags: [bool; MAX_CHANNELS],
    /// Main-speaker bass-management high-pass. It is intentionally separate
    /// from the LFE low-pass so either half can be enabled explicitly.
    mains_hp_enabled: bool,
    mains_hp_coeffs: BiquadCoeffsF32,
    mains_hp_states: Vec<BiquadStateF32>,
    /// Per-channel EQ cascades. Configuration may allocate these vectors;
    /// processing only walks already-built state and never allocates.
    channel_eq_enabled: bool,
    channel_eq_coeffs: Vec<Vec<BiquadCoeffsF32>>,
    channel_eq_states: Vec<Vec<BiquadStateF32>>,
}

impl ChannelTrimmer {
    pub fn new(sample_rate: f32) -> Self {
        let mut t = Self {
            enabled: false,
            sample_rate,
            gains: Vec::new(),
            delay_samples: Vec::new(),
            invert: Vec::new(),
            delay_cap: 0,
            delay_mask: 0,
            delay_bufs: Vec::new(),
            delay_pos: Vec::new(),
            routing_enabled: false,
            routing_ok: false,
            routing: Vec::new(),
            lfe_enabled: false,
            lfe_gain: 1.0,
            lfe_channels: Vec::new(),
            lfe_lp_enabled: false,
            lfe_lp_coeffs: BiquadCoeffsF32::identity(),
            lfe_lp_states: (0..MAX_CHANNELS)
                .map(|_| BiquadStateF32::default())
                .collect(),
            lfe_channel_flags: [false; MAX_CHANNELS],
            mains_hp_enabled: false,
            mains_hp_coeffs: BiquadCoeffsF32::identity(),
            mains_hp_states: (0..MAX_CHANNELS)
                .map(|_| BiquadStateF32::default())
                .collect(),
            channel_eq_enabled: false,
            channel_eq_coeffs: (0..MAX_CHANNELS).map(|_| Vec::new()).collect(),
            channel_eq_states: (0..MAX_CHANNELS).map(|_| Vec::new()).collect(),
        };
        t.rebuild_delay_lines(MAX_CHANNELS);
        t
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if (self.sample_rate - sample_rate).abs() > 0.5 {
            self.sample_rate = sample_rate;
            self.rebuild_delay_lines(self.delay_bufs.len());
        }
    }

    fn rebuild_delay_lines(&mut self, channels: usize) {
        let samples = (MAX_CHANNEL_DELAY_MS * 0.001 * self.sample_rate).ceil() as usize + 2;
        let cap = next_pow2(samples.max(4));
        self.delay_cap = cap;
        self.delay_mask = cap - 1;
        self.delay_bufs = (0..channels.max(1)).map(|_| vec![0.0; cap]).collect();
        self.delay_pos = vec![0; channels.max(1)];
    }

    /// Apply the per-channel trim configuration (gain/delay/polarity).
    pub fn set_config(&mut self, config: &ChannelTrimConfig, sample_rate: f32) {
        self.set_sample_rate(sample_rate);
        self.enabled = config.enabled;
        self.gains.clear();
        self.delay_samples.clear();
        self.invert.clear();
        let max_ch = MAX_CHANNELS;
        for _ in 0..max_ch {
            self.gains.push(1.0);
            self.delay_samples.push(0.0);
            self.invert.push(false);
        }
        for entry in &config.entries {
            if entry.channel >= max_ch {
                log::warn!(
                    "ChannelTrim: entry for channel {} exceeds MAX_CHANNELS ({}); ignored",
                    entry.channel,
                    max_ch
                );
                continue;
            }
            let delay_ms = entry.delay_ms.clamp(0.0, MAX_CHANNEL_DELAY_MS);
            self.gains[entry.channel] = db_to_linear(entry.gain_db);
            self.delay_samples[entry.channel] = delay_ms * 0.001 * self.sample_rate;
            self.invert[entry.channel] = entry.invert;
        }
        if config.enabled {
            self.rebuild_delay_lines(self.delay_bufs.len());
        }
    }

    /// Compile per-channel EQ entries into bounded biquad cascades.
    ///
    /// This is a configuration-time operation. The realtime path only applies
    /// the preallocated coefficients/states, so a 16-channel block does not
    /// allocate or parse filter parameters while audio is flowing.
    pub fn set_channel_eq(&mut self, config: &ChannelEqConfig, sample_rate: f32) {
        self.channel_eq_enabled = config.enabled;
        self.channel_eq_coeffs = (0..MAX_CHANNELS).map(|_| Vec::new()).collect();
        self.channel_eq_states = (0..MAX_CHANNELS).map(|_| Vec::new()).collect();
        if !config.enabled {
            return;
        }

        for entry in &config.entries {
            if entry.channel >= MAX_CHANNELS {
                log::warn!(
                    "ChannelEq: entry for channel {} exceeds MAX_CHANNELS ({}); ignored",
                    entry.channel,
                    MAX_CHANNELS
                );
                continue;
            }
            let coeffs = &mut self.channel_eq_coeffs[entry.channel];
            let states = &mut self.channel_eq_states[entry.channel];
            for band in entry.bands.iter().take(MAX_CHANNEL_EQ_BANDS) {
                if !band.enabled {
                    continue;
                }
                coeffs.push(channel_eq_coeffs(sample_rate, band));
                states.push(BiquadStateF32::default());
            }
        }
    }

    /// Configure the mains high-pass portion of bass management.
    pub fn set_bass_management(&mut self, config: &BassManagementConfig, sample_rate: f32) {
        self.mains_hp_enabled = config.enabled && config.mains_highpass_enabled;
        if self.mains_hp_enabled {
            self.mains_hp_coeffs =
                BiquadCoeffsF32::highpass(sample_rate, config.crossover_hz, config.q);
            for state in &mut self.mains_hp_states {
                *state = BiquadStateF32::default();
            }
        }
    }

    /// Apply the routing matrix. The matrix must be square and its width must
    /// equal the active channel count at process time; any other shape is
    /// rejected here (a warning is logged) and routing is bypassed.
    pub fn set_routing(&mut self, config: &ChannelRoutingConfig) {
        self.routing_enabled = config.enabled;
        self.routing = config.matrix.clone();
        self.routing_ok = if !config.enabled {
            false
        } else if config.matrix.is_empty() {
            true // empty matrix = identity; enabled but no-op
        } else {
            let n = config.matrix.len();
            let square = config.matrix.iter().all(|row| row.len() == n);
            let bounded = n > 0 && n <= MAX_CHANNELS;
            if !(square && bounded) {
                log::warn!(
                    "ChannelRouting: matrix must be square with 1..={} rows, got {}x{}; routing bypassed",
                    MAX_CHANNELS,
                    n,
                    config.matrix.first().map_or(0, |r| r.len())
                );
            }
            square && bounded
        };
    }

    /// Apply LFE gain config and the optional LFE low-pass crossover.
    pub fn set_lfe(&mut self, config: &LfeConfig) {
        self.lfe_enabled = config.enabled;
        self.lfe_gain = if config.enabled {
            db_to_linear(config.gain_db)
        } else {
            1.0
        };
        // Bass-management LFE low-pass: second-order Butterworth at the
        // crossover frequency (spec §17/§34). `None` = full-band LFE.
        match config.crossover_hz {
            Some(hz) if hz.is_finite() && hz > 0.0 => {
                let q = std::f32::consts::FRAC_1_SQRT_2;
                self.lfe_lp_coeffs = BiquadCoeffsF32::lowpass(self.sample_rate, hz, q);
                self.lfe_lp_enabled = true;
                for state in &mut self.lfe_lp_states {
                    *state = BiquadStateF32::default();
                }
            }
            _ => {
                self.lfe_lp_enabled = false;
            }
        }
    }

    /// Declare which channel indices carry the LFE role (derived from the
    /// active [`crate::decode::ChannelLayout`]).
    pub fn set_lfe_channels(&mut self, channels: Vec<usize>) {
        self.lfe_channel_flags = [false; MAX_CHANNELS];
        for &channel in &channels {
            if channel < MAX_CHANNELS {
                self.lfe_channel_flags[channel] = true;
            }
        }
        self.lfe_channels = channels;
    }

    /// True when any sub-stage may alter the signal for `channels` channels.
    pub fn is_active(&self, channels: usize) -> bool {
        if !self.enabled
            && !self.routing_enabled
            && !self.lfe_enabled
            && !self.mains_hp_enabled
            && !self.channel_eq_enabled
        {
            return false;
        }
        if !self.enabled {
            // Routing, LFE management, bass management, and per-channel EQ
            // can all act independently of trim gain/delay/polarity.
            return self.routing_applies(channels)
                || self.lfe_applies()
                || self.mains_hp_enabled
                || self.channel_eq_enabled;
        }
        true
    }

    fn routing_applies(&self, channels: usize) -> bool {
        self.routing_enabled && self.routing_ok && self.routing.len() == channels
    }

    fn lfe_applies(&self) -> bool {
        self.lfe_enabled && self.lfe_channel_flags.iter().any(|active| *active)
    }

    /// Process `frames` of `channels` planar audio in place.
    pub fn process_planes(&mut self, planes: &mut [Vec<f32>], channels: usize, frames: usize) {
        if !self.is_active(channels) {
            return;
        }
        let ch = channels.min(planes.len());
        if ch == 0 || frames == 0 {
            return;
        }
        let routing = self.routing_applies(ch);
        let mut tmp = [0.0f32; MAX_CHANNELS];

        for i in 0..frames {
            // 1. Routing matrix: out[dst] = Σ_src matrix[src][dst] · in[src].
            if routing {
                for (dst, slot) in tmp.iter_mut().enumerate().take(ch) {
                    let mut acc = 0.0f32;
                    for (src, plane) in planes.iter().enumerate().take(ch) {
                        acc += self.routing[src][dst] * plane[i];
                    }
                    *slot = acc;
                }
                for (c, plane) in planes.iter_mut().enumerate().take(ch) {
                    plane[i] = tmp[c];
                }
            }

            // 2–4. Per-channel gain → polarity → delay, then LFE gain, then
            // the optional LFE low-pass (bass management).
            for (c, plane) in planes.iter_mut().enumerate().take(ch) {
                let is_lfe = self.lfe_enabled && self.lfe_channel_flags[c];
                let mut g = if self.enabled { self.gains[c] } else { 1.0 };
                if self.enabled && self.invert[c] {
                    g = -g;
                }
                if is_lfe {
                    g *= self.lfe_gain;
                }
                let d = if self.enabled {
                    self.delay_samples[c]
                } else {
                    0.0
                };
                let x = plane[i];
                if d > 0.0 {
                    let i0 = d.floor() as usize;
                    let frac = d - i0 as f32;
                    let pos = self.delay_pos[c];
                    // y = (1−frac)·x[n−i0] + frac·x[n−i0−1]; for i0 == 0 the
                    // "delayed by 0" sample is the current input itself.
                    let (a_val, b_val) = if i0 == 0 {
                        (
                            x,
                            self.delay_bufs[c][(pos + self.delay_cap - 1) & self.delay_mask],
                        )
                    } else {
                        (
                            self.delay_bufs[c][(pos + self.delay_cap - i0) & self.delay_mask],
                            self.delay_bufs[c][(pos + self.delay_cap - i0 - 1) & self.delay_mask],
                        )
                    };
                    let y = a_val * (1.0 - frac) + b_val * frac;
                    self.delay_bufs[c][pos] = x;
                    self.delay_pos[c] = (pos + 1) & self.delay_mask;
                    plane[i] = y * g;
                } else {
                    plane[i] = x * g;
                }
                // LFE low-pass runs after gain, only on LFE-role channels.
                if self.lfe_lp_enabled && is_lfe {
                    plane[i] = self.lfe_lp_states[c].process(plane[i], &self.lfe_lp_coeffs);
                }
                // Main high-pass is applied to every non-LFE speaker channel;
                // the LFE path has its own explicitly configured low-pass.
                if self.mains_hp_enabled && !is_lfe {
                    plane[i] = self.mains_hp_states[c].process(plane[i], &self.mains_hp_coeffs);
                }
                // Finally apply the channel-specific EQ cascade. EQ state is
                // independent per channel, so center/surround/height content
                // cannot leak into the front-pair stereo filters.
                if self.channel_eq_enabled {
                    for band in 0..self.channel_eq_coeffs[c].len() {
                        plane[i] = self.channel_eq_states[c][band]
                            .process(plane[i], &self.channel_eq_coeffs[c][band]);
                    }
                }
            }
        }
    }
}

impl Default for ChannelTrimmer {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{ChannelRoutingConfig, ChannelTrimConfig, ChannelTrimEntry, LfeConfig};

    fn planes(channels: usize, frames: usize, fill: impl Fn(usize, usize) -> f32) -> Vec<Vec<f32>> {
        (0..channels)
            .map(|c| (0..frames).map(|i| fill(c, i)).collect())
            .collect()
    }

    fn disabled() -> ChannelTrimConfig {
        ChannelTrimConfig {
            enabled: false,
            entries: Vec::new(),
        }
    }

    #[test]
    fn disabled_is_passthrough() {
        let mut t = ChannelTrimmer::new(48_000.0);
        t.set_config(&disabled(), 48_000.0);
        t.set_routing(&ChannelRoutingConfig::default());
        t.set_lfe(&LfeConfig::default());
        let mut p = planes(4, 8, |c, i| (c * 10 + i) as f32);
        let before = p.clone();
        t.process_planes(&mut p, 4, 8);
        assert_eq!(p, before);
    }

    #[test]
    fn gain_and_polarity() {
        let mut t = ChannelTrimmer::new(48_000.0);
        t.set_config(
            &ChannelTrimConfig {
                enabled: true,
                entries: vec![
                    ChannelTrimEntry {
                        channel: 0,
                        gain_db: -6.0206, // ≈ 0.5×
                        ..Default::default()
                    },
                    ChannelTrimEntry {
                        channel: 1,
                        gain_db: 0.0,
                        invert: true,
                        ..Default::default()
                    },
                ],
            },
            48_000.0,
        );
        let mut p = planes(4, 4, |_c, i| (i + 1) as f32);
        t.process_planes(&mut p, 4, 4);
        for (i, (a, b, c2, c3)) in p[0]
            .iter()
            .zip(p[1].iter())
            .zip(p[2].iter())
            .zip(p[3].iter())
            .map(|(((a, b), c2), c3)| (a, b, c2, c3))
            .enumerate()
        {
            let v = (i + 1) as f32;
            assert!((a - v * 0.5).abs() < 1e-3, "ch0 {a:.4} != {:.4}", v * 0.5);
            assert!((b + v).abs() < 1e-5, "ch1 {b:.4} != {:.4}", -v);
            assert!((c2 - v).abs() < 1e-5, "ch2 should be untouched");
            assert!((c3 - v).abs() < 1e-5, "ch3 should be untouched");
        }
    }

    #[test]
    fn integer_delay() {
        let mut t = ChannelTrimmer::new(1000.0); // 1 sample per ms
        t.set_config(
            &ChannelTrimConfig {
                enabled: true,
                entries: vec![ChannelTrimEntry {
                    channel: 0,
                    delay_ms: 3.0,
                    ..Default::default()
                }],
            },
            1000.0,
        );
        let mut p = planes(2, 10, |_c, i| (i + 1) as f32);
        t.process_planes(&mut p, 2, 10);
        // ch0 delayed by 3 samples: first 3 outputs are 0, then 1,2,3,...
        assert_eq!(p[0][0], 0.0);
        assert_eq!(p[0][1], 0.0);
        assert_eq!(p[0][2], 0.0);
        for (i, &v) in p[0].iter().enumerate().skip(3) {
            assert!((v - (i + 1 - 3) as f32).abs() < 1e-5);
        }
        // ch1 untouched.
        for (i, &v) in p[1].iter().enumerate().take(10) {
            assert!((v - (i + 1) as f32).abs() < 1e-5);
        }
    }

    #[test]
    fn fractional_delay_interpolates() {
        let mut t = ChannelTrimmer::new(1000.0);
        t.set_config(
            &ChannelTrimConfig {
                enabled: true,
                entries: vec![ChannelTrimEntry {
                    channel: 0,
                    delay_ms: 0.5,
                    ..Default::default()
                }],
            },
            1000.0,
        );
        // Ramp input: y[n] = 0.5·x[n] + 0.5·x[n−1] = n + 0.5.
        let mut p = planes(1, 16, |_c, i| (i + 1) as f32);
        t.process_planes(&mut p, 1, 16);
        for (i, &v) in p[0].iter().enumerate() {
            let expected = i as f32 + 0.5;
            assert!((v - expected).abs() < 1e-5, "y[{i}] = {v} != {expected}");
        }
    }

    #[test]
    fn fractional_delay_dc_converges() {
        let mut t = ChannelTrimmer::new(1000.0);
        t.set_config(
            &ChannelTrimConfig {
                enabled: true,
                entries: vec![ChannelTrimEntry {
                    channel: 0,
                    delay_ms: 0.3,
                    ..Default::default()
                }],
            },
            1000.0,
        );
        // Steady DC input: output must converge to the same DC value.
        let mut p = planes(1, 64, |_c, _i| 0.75);
        t.process_planes(&mut p, 1, 64);
        assert!((p[0][63] - 0.75).abs() < 1e-5);
    }

    #[test]
    fn routing_matrix_swaps_channels() {
        let mut t = ChannelTrimmer::new(48_000.0);
        t.set_config(&disabled(), 48_000.0);
        // Swap: dst0 = src1, dst1 = src0, dst2 = src2.
        t.set_routing(&ChannelRoutingConfig {
            enabled: true,
            matrix: vec![
                vec![0.0, 1.0, 0.0],
                vec![1.0, 0.0, 0.0],
                vec![0.0, 0.0, 1.0],
            ],
        });
        let mut p = planes(3, 4, |c, i| (c * 10 + i) as f32);
        let before = p.clone();
        t.process_planes(&mut p, 3, 4);
        for i in 0..4 {
            assert!((p[0][i] - before[1][i]).abs() < 1e-5);
            assert!((p[1][i] - before[0][i]).abs() < 1e-5);
            assert!((p[2][i] - before[2][i]).abs() < 1e-5);
        }
    }

    #[test]
    fn routing_mismatched_width_is_bypassed() {
        let mut t = ChannelTrimmer::new(48_000.0);
        t.set_config(&disabled(), 48_000.0);
        t.set_routing(&ChannelRoutingConfig {
            enabled: true,
            matrix: vec![vec![1.0, 0.0], vec![0.0, 1.0]], // 2×2, but 4 active channels
        });
        let mut p = planes(4, 4, |c, i| (c * 10 + i) as f32);
        let before = p.clone();
        t.process_planes(&mut p, 4, 4);
        assert_eq!(p, before, "non-matching matrix must be bypassed");
    }

    #[test]
    fn non_square_routing_is_rejected() {
        let mut t = ChannelTrimmer::new(48_000.0);
        t.set_routing(&ChannelRoutingConfig {
            enabled: true,
            matrix: vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]],
        });
        assert!(!t.routing_applies(2));
        assert!(!t.routing_applies(3));
    }

    #[test]
    fn lfe_gain_only_on_lfe_channels() {
        let mut t = ChannelTrimmer::new(48_000.0);
        t.set_config(&disabled(), 48_000.0);
        t.set_lfe(&LfeConfig {
            enabled: true,
            gain_db: 6.0, // ≈ 1.995×
            crossover_hz: None,
        });
        t.set_lfe_channels(vec![3]); // 5.1 layout LFE slot
        let mut p = planes(6, 4, |_c, _i| 0.5);
        t.process_planes(&mut p, 6, 4);
        for (i, &lfe) in p[3].iter().enumerate() {
            assert!((lfe - 0.5 * 1.995).abs() < 1e-3, "LFE ch must be boosted");
            for &v in [&p[0][i], &p[1][i], &p[2][i], &p[4][i], &p[5][i]] {
                assert!((v - 0.5).abs() < 1e-5, "non-LFE ch must be untouched");
            }
        }
    }

    #[test]
    fn lfe_low_pass_dc_unity_and_hf_rolloff() {
        // Bass-management LFE low-pass: DC passes at unity, a 10 kHz tone is
        // strongly attenuated (crossover at 120 Hz), and non-LFE channels are
        // untouched.
        let mut t = ChannelTrimmer::new(48_000.0);
        t.set_config(&disabled(), 48_000.0);
        t.set_lfe(&LfeConfig {
            enabled: true,
            gain_db: 0.0,
            crossover_hz: Some(120.0),
        });
        t.set_lfe_channels(vec![3]);

        // DC: after the filter settles, the LFE channel converges to 1.0.
        let mut p = planes(6, 4096, |c, _i| if c == 3 { 1.0 } else { 0.25 });
        t.process_planes(&mut p, 6, 4096);
        let lfe_tail = p[3][1024..].iter().copied().fold(0.0f32, f32::max);
        assert!(
            (lfe_tail - 1.0).abs() < 0.05,
            "LFE DC gain must be unity, got {lfe_tail}"
        );
        for c in [0, 1, 2, 4, 5] {
            assert!(
                (p[c][4095] - 0.25).abs() < 1e-5,
                "non-LFE ch must be untouched"
            );
        }

        // 10 kHz: far above the 120 Hz crossover → heavily attenuated.
        let mut t = ChannelTrimmer::new(48_000.0);
        t.set_config(&disabled(), 48_000.0);
        t.set_lfe(&LfeConfig {
            enabled: true,
            gain_db: 0.0,
            crossover_hz: Some(120.0),
        });
        t.set_lfe_channels(vec![3]);
        let mut p = planes(6, 4096, |c, i| {
            if c == 3 {
                (2.0 * std::f32::consts::PI * 10_000.0 * i as f32 / 48_000.0).sin()
            } else {
                0.0
            }
        });
        t.process_planes(&mut p, 6, 4096);
        let in_rms = 1.0 / 2.0f32.sqrt(); // sine RMS = 1/√2
        let out_rms =
            (p[3][1024..].iter().map(|s| s * s).sum::<f32>() / (4096 - 1024) as f32).sqrt();
        assert!(
            out_rms < in_rms * 0.1,
            "10 kHz tone must be rolled off by the 120 Hz LFE low-pass (in {in_rms:.3}, out {out_rms:.4})"
        );
    }
}
