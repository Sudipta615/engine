//! Dedicated Bass Management Layer (spec §56, §57).
//!
//! Manages multi-channel bass redirection between mains channels and subwoofer/LFE:
//! - Multi-slope crossover (12, 24, 48 dB/oct) with Linkwitz-Riley and Butterworth topologies.
//! - High-pass filtering on mains channels.
//! - Monophonic bass extraction and low-pass filtering.
//! - Subwoofer delay compensation (0..50 ms) and phase alignment (0..180° + polarity).
//! - Zero allocation in the audio processing hot path.

use super::filter::{CrossoverFilter, SubwooferDelay, SubwooferPhase};
use config::{BassIntent, BassManagementConfig};

/// Maximum channels supported by the bass manager (e.g. 9.1.6 arrays).
pub const MAX_BASS_CHANNELS: usize = 16;

/// Dedicated bass management processor.
#[derive(Debug, Clone)]
pub struct BassManager {
    config: BassManagementConfig,
    sample_rate: f32,
    mains_filters: Vec<CrossoverFilter>,
    sub_filter: CrossoverFilter,
    sub_delay: SubwooferDelay,
    sub_phase: SubwooferPhase,
    bass_sum: Vec<f32>,
}

impl BassManager {
    /// Construct a new bass manager for the given configuration and sample rate.
    pub fn new(config: &BassManagementConfig, sample_rate: f32, channel_count: usize) -> Self {
        let n_mains = channel_count.clamp(1, MAX_BASS_CHANNELS);
        let sr = sample_rate.max(1.0);
        let fc = config.crossover_hz;
        let slope = config.slope;
        let ft = config.filter_type;

        let mut mains_filters = Vec::with_capacity(n_mains);
        for _ in 0..n_mains {
            mains_filters.push(CrossoverFilter::new(sr, fc, slope, ft, true));
        }

        let sub_filter = CrossoverFilter::new(sr, fc, slope, ft, false);
        let sub_delay = SubwooferDelay::new(sr, config.sub_delay_ms);
        let sub_phase =
            SubwooferPhase::new(sr, fc, config.sub_phase_degrees, config.sub_polarity_invert);

        Self {
            config: config.clone(),
            sample_rate: sr,
            mains_filters,
            sub_filter,
            sub_delay,
            sub_phase,
            bass_sum: vec![0.0; 4096],
        }
    }

    /// Update bass management configuration.
    pub fn update_config(&mut self, config: &BassManagementConfig, sample_rate: f32) {
        self.config = config.clone();
        self.sample_rate = sample_rate.max(1.0);
        let fc = config.crossover_hz;
        let slope = config.slope;
        let ft = config.filter_type;

        for filter in &mut self.mains_filters {
            filter.update(self.sample_rate, fc, slope, ft);
        }
        self.sub_filter.update(self.sample_rate, fc, slope, ft);
        self.sub_delay
            .set_delay_ms(config.sub_delay_ms, self.sample_rate);
        self.sub_phase.set_parameters(
            self.sample_rate,
            fc,
            config.sub_phase_degrees,
            config.sub_polarity_invert,
        );
    }

    /// Ensure capacity for the given block size and channel count.
    pub fn ensure_capacity(&mut self, block_size: usize, channel_count: usize) {
        if self.bass_sum.len() < block_size {
            self.bass_sum.resize(block_size, 0.0);
        }
        let needed_mains = channel_count.clamp(1, MAX_BASS_CHANNELS);
        while self.mains_filters.len() < needed_mains {
            self.mains_filters.push(CrossoverFilter::new(
                self.sample_rate,
                self.config.crossover_hz,
                self.config.slope,
                self.config.filter_type,
                true,
            ));
        }
    }

    /// Process mains channels and route filtered bass to subwoofer / LFE.
    ///
    /// - `mains`: Slice of mutable audio channel buffers.
    /// - `sub_out`: Target subwoofer buffer receiving the redirected bass and sub energy.
    /// - `direct_lfe`: Optional dedicated LFE channel input (summed directly into subwoofer).
    /// - `object_bass`: Optional external object bass send.
    pub fn process(
        &mut self,
        mains: &mut [&mut [f32]],
        sub_out: &mut [f32],
        direct_lfe: Option<&[f32]>,
        object_bass: Option<&[f32]>,
    ) {
        let block_size = sub_out.len();
        self.ensure_capacity(block_size, mains.len());

        if !self.config.enabled {
            // Bass management disabled: passthrough mains untouched
            sub_out.fill(0.0);
            if let Some(lfe) = direct_lfe {
                let n = block_size.min(lfe.len());
                sub_out[..n].copy_from_slice(&lfe[..n]);
            }
            if let Some(ob) = object_bass {
                let n = block_size.min(ob.len());
                for (s, o) in sub_out[..n].iter_mut().zip(ob[..n].iter()) {
                    *s += *o;
                }
            }
            return;
        }

        // Accumulate bass redirect from mains
        let bass_buf = &mut self.bass_sum[..block_size];
        bass_buf.fill(0.0);

        let n_channels = mains.len().min(self.mains_filters.len());
        for (ch_idx, ch) in mains[..n_channels].iter_mut().enumerate() {
            let n = block_size.min(ch.len());
            if self.config.mains_highpass_enabled {
                // Sum input signal into bass redirect accumulator before highpass
                for i in 0..n {
                    bass_buf[i] += ch[i];
                }
                // Highpass the mains channel in-place
                self.mains_filters[ch_idx].process_block(&mut ch[..n]);
            }
        }

        // Add external object bass send (if any)
        if let Some(ob) = object_bass {
            let n = block_size.min(ob.len());
            for i in 0..n {
                bass_buf[i] += ob[i];
            }
        }

        // Lowpass filter the extracted bass
        self.sub_filter.process_block(bass_buf);

        // Sum lowpassed redirected bass with direct LFE (if provided)
        for i in 0..block_size {
            let lfe_sample = direct_lfe.and_then(|l| l.get(i).copied()).unwrap_or(0.0);
            sub_out[i] = bass_buf[i] + lfe_sample;
        }

        // Apply delay compensation and phase alignment to subwoofer output
        self.sub_delay.process_block(sub_out);
        self.sub_phase.process_block(sub_out);
    }

    /// Process a single object with bass intent.
    /// Returns `(mains_sample, sub_sample)`.
    #[inline]
    pub fn route_object_intent(
        &self,
        sample: f32,
        intent: BassIntent,
        bass_send: f32,
    ) -> (f32, f32) {
        match intent {
            BassIntent::FullRangeManaged => {
                // Object stays in mains, send scaled portion to bass manager
                (sample, sample * bass_send)
            }
            BassIntent::DirectLfe => {
                // Direct LFE routing: remove from mains, route completely to LFE/sub
                (0.0, sample * bass_send.max(1.0))
            }
            BassIntent::SubBassOnly => {
                // Band-limited sub-bass only
                (0.0, sample * bass_send.max(1.0))
            }
            BassIntent::Bypass => {
                // Unmanaged in mains, zero sub send
                (sample, 0.0)
            }
        }
    }

    /// Reset internal filter and delay states.
    pub fn reset(&mut self) {
        for filter in &mut self.mains_filters {
            filter.reset();
        }
        self.sub_filter.reset();
        self.sub_delay.reset();
        self.sub_phase.reset();
        self.bass_sum.fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{CrossoverFilterType, CrossoverSlope};

    #[test]
    fn bass_manager_redirects_mains_low_frequencies() {
        let config = BassManagementConfig {
            enabled: true,
            mains_highpass_enabled: true,
            crossover_hz: 80.0,
            q: std::f32::consts::FRAC_1_SQRT_2,
            slope: CrossoverSlope::Db24,
            filter_type: CrossoverFilterType::LinkwitzRiley,
            sub_delay_ms: 0.0,
            sub_phase_degrees: 0.0,
            sub_polarity_invert: false,
        };

        let mut mgr = BassManager::new(&config, 48000.0, 2);

        // 40 Hz tone (below crossover)
        let block_len = 1024;
        let mut left = vec![0.0f32; block_len];
        let mut right = vec![0.0f32; block_len];
        for i in 0..block_len {
            let t = i as f32 / 48000.0;
            let val = (2.0 * std::f32::consts::PI * 40.0 * t).sin();
            left[i] = val;
            right[i] = val;
        }

        let mut sub = vec![0.0f32; block_len];
        let mut mains_slices = [&mut left[..], &mut right[..]];

        mgr.process(&mut mains_slices, &mut sub, None, None);

        // Subwoofer should receive low frequency energy
        let sub_energy: f32 = sub[500..].iter().map(|s| s * s).sum();
        assert!(sub_energy > 1.0, "Subwoofer did not receive 40 Hz energy");

        // Mains channels should be attenuated by highpass
        let mains_energy: f32 = mains_slices[0][500..].iter().map(|s| s * s).sum();
        let original_energy: f32 = (500..block_len)
            .map(|i| {
                let t = i as f32 / 48000.0;
                let val = (2.0 * std::f32::consts::PI * 40.0 * t).sin();
                val * val
            })
            .sum();

        assert!(
            mains_energy < original_energy * 0.3,
            "Mains were not highpassed properly: mains {} vs orig {}",
            mains_energy,
            original_energy
        );
    }
}
