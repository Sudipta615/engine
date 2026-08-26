//! Block signal-processing plans for [`DspGraph`]: stereo f32 / f64 and
//! multichannel, plus the pre-mix / post-mix / front-filter stage chains.

use super::*;

impl DspGraph {
    // ── Signal Processing Plans ───────────────────────────────────────────

    /// Process a block of stereo frames in-place.
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block(&mut left[start..end], &mut right[start..end]);
                start = end;
            }
            return;
        }
        if n == 0 || self.dop_bypass || self.bit_perfect {
            return;
        }

        match self.precision_mode {
            PrecisionMode::Performance => {
                self.process_outgoing_block(left, right);
                self.process_post_mix_block(left, right);
            }
            PrecisionMode::Quality => {
                let mut l64 = std::mem::take(&mut self.scratch.scratch_f64_l);
                let mut r64 = std::mem::take(&mut self.scratch.scratch_f64_r);
                for i in 0..n {
                    l64[i] = left[i] as f64;
                    r64[i] = right[i] as f64;
                }
                self.process_outgoing_block_f64(&mut l64[..n], &mut r64[..n]);
                self.process_post_mix_block_f64(&mut l64[..n], &mut r64[..n]);
                for i in 0..n {
                    left[i] = l64[i] as f32;
                    right[i] = r64[i] as f32;
                }
                self.scratch.scratch_f64_l = l64;
                self.scratch.scratch_f64_r = r64;
            }
        }
    }

    /// Process a block of stereo frames in f64 precision in-place.
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        let n = left.len().min(right.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block_f64(&mut left[start..end], &mut right[start..end]);
                start = end;
            }
            return;
        }
        if n == 0 || self.dop_bypass || self.bit_perfect {
            return;
        }
        self.process_outgoing_block_f64(left, right);
        self.process_post_mix_block_f64(left, right);
    }

    /// Process the pre-mix outgoing chain (preamp + loudness).
    #[inline]
    pub fn process_outgoing_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bit_perfect {
            return;
        }
        self.out_preamp.processor.process_block_stereo(left, right);
        self.out_loudness.normalizer.process_block(left, right);
    }

    #[inline]
    pub fn process_outgoing_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bit_perfect {
            return;
        }
        self.out_preamp
            .processor
            .process_block_stereo_f64(left, right);
        self.out_loudness.normalizer.process_block_f64(left, right);
    }

    /// Process the pre-mix incoming chain (preamp + loudness).
    #[inline]
    pub fn process_incoming_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bit_perfect {
            return;
        }
        self.in_preamp.processor.process_block_stereo(left, right);
        self.in_loudness.normalizer.process_block(left, right);
    }

    #[inline]
    pub fn process_incoming_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bit_perfect {
            return;
        }
        self.in_preamp
            .processor
            .process_block_stereo_f64(left, right);
        self.in_loudness.normalizer.process_block_f64(left, right);
    }

    /// Process the post-mix chain over stereo channels in f32.
    pub fn process_post_mix_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bit_perfect {
            return;
        }
        let n = left.len().min(right.len());
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_post_mix_block(&mut left[start..end], &mut right[start..end]);
                start = end;
            }
            return;
        }
        if n == 0 {
            return;
        }

        match self.precision_mode {
            PrecisionMode::Performance => self.process_post_mix_block_f32(left, right),
            PrecisionMode::Quality => {
                let mut l64 = std::mem::take(&mut self.scratch.scratch_f64_l);
                let mut r64 = std::mem::take(&mut self.scratch.scratch_f64_r);
                for i in 0..n {
                    l64[i] = left[i] as f64;
                    r64[i] = right[i] as f64;
                }
                self.process_post_mix_block_f64(&mut l64[..n], &mut r64[..n]);
                for i in 0..n {
                    left[i] = l64[i] as f32;
                    right[i] = r64[i] as f32;
                }
                self.scratch.scratch_f64_l = l64;
                self.scratch.scratch_f64_r = r64;
            }
        }
    }

    fn process_post_mix_block_f32(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if self.eq.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.eq.process(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.eq.process_block(left, right);
        }
        self.dynamics.compressor.process_block(left, right);
        self.convolution.engine.process_block(left, right);
        let bl = self.balance.balance_gain_l;
        let br = self.balance.balance_gain_r;
        for i in 0..n {
            left[i] *= bl;
            right[i] *= br;
        }
        self.crossfeed.crossfeed.process_block(left, right);
        self.stereo.enhancer.process_block(left, right);
        self.timestretch.stretcher.process_block(left, right);
        self.volume.processor.process_block_stereo(left, right);
        self.seek_fade.fade.process_block(left, right);
    }

    /// Process the post-mix chain in native f64.
    pub fn process_post_mix_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bit_perfect {
            return;
        }
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }
        if self.eq.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.eq.process_f64(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.eq.process_block_f64(left, right);
        }
        self.dynamics.compressor.process_block_f64(left, right);
        self.convolution.engine.process_block_f64(left, right);
        let bl = self.balance.balance_gain_l as f64;
        let br = self.balance.balance_gain_r as f64;
        for i in 0..n {
            left[i] *= bl;
            right[i] *= br;
        }
        self.crossfeed.crossfeed.process_block_f64(left, right);
        self.stereo.enhancer.process_block_f64(left, right);
        self.timestretch.stretcher.process_block_f64(left, right);
        self.volume.processor.process_block_stereo_f64(left, right);
        self.seek_fade.fade.process_block_f64(left, right);
    }

    /// Process an interleaved block of `channels`-channel frames in place.
    pub fn process_block_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        if channels == 0 || channels > MAX_CHANNELS {
            return;
        }
        let n = interleaved.len() / channels;
        if n == 0 {
            return;
        }
        if n > MAX_AUDIO_BLOCK_FRAMES {
            let mut start = 0;
            while start < n {
                let end = (start + MAX_AUDIO_BLOCK_FRAMES).min(n);
                self.process_block_multichannel(
                    &mut interleaved[start * channels..end * channels],
                    channels,
                );
                start = end;
            }
            return;
        }
        if self.dop_bypass || self.bit_perfect {
            return;
        }

        let mut planes = std::mem::take(&mut self.scratch.scratch_mc);

        if channels <= 2 {
            for (i, chunk) in interleaved[..n * channels]
                .chunks_exact(channels)
                .enumerate()
            {
                planes[0][i] = chunk[0];
                planes[1][i] = if channels == 2 { chunk[1] } else { chunk[0] };
            }
            {
                let (front, rest) = planes.split_at_mut(1);
                self.process_block(&mut front[0][..n], &mut rest[0][..n]);
            }
            for (i, chunk) in interleaved[..n * channels]
                .chunks_exact_mut(channels)
                .enumerate()
            {
                chunk[0] = planes[0][i];
                if channels == 2 {
                    chunk[1] = planes[1][i];
                }
            }
            self.scratch.scratch_mc = planes;
            return;
        }

        // Multichannel de-interleave: channel `ch` sits at indices
        // `ch, ch + channels, ch + 2*channels, …` in the interleaved block.
        for (ch, plane) in planes.iter_mut().enumerate().take(channels) {
            for (i, s) in interleaved
                .iter()
                .skip(ch)
                .step_by(channels)
                .take(n)
                .enumerate()
            {
                plane[i] = *s;
            }
        }

        if !self.bit_perfect {
            self.routing
                .trimmer
                .process_planes(&mut planes[..channels], channels, n);
        }

        if self.bit_perfect {
            self.volume
                .processor
                .process_planes(&mut planes[..channels], channels, n);
            self.seek_fade
                .fade
                .process_planes(&mut planes[..channels], channels, n);
        } else {
            self.out_preamp
                .processor
                .process_planes(&mut planes[..channels], channels, n);
            self.out_loudness
                .normalizer
                .process_planes(&mut planes[..channels], channels, n);

            // Stereo filter chain on front L/R
            {
                let (front, rest) = planes.split_at_mut(1);
                self.process_post_mix_front_filters(&mut front[0][..n], &mut rest[0][..n]);
            }

            self.volume
                .processor
                .process_planes(&mut planes[..channels], channels, n);
            self.seek_fade
                .fade
                .process_planes(&mut planes[..channels], channels, n);
        }

        // Re-interleave
        for ch in 0..channels {
            for i in 0..n {
                interleaved[i * channels + ch] = planes[ch][i];
            }
        }
        self.scratch.scratch_mc = planes;
    }

    fn process_post_mix_front_filters(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }
        if self.eq.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.eq.process(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.eq.process_block(left, right);
        }
        self.dynamics.compressor.process_block(left, right);
        self.convolution.engine.process_block(left, right);
        let bl = self.balance.balance_gain_l;
        let br = self.balance.balance_gain_r;
        for i in 0..n {
            left[i] *= bl;
            right[i] *= br;
        }
        self.crossfeed.crossfeed.process_block(left, right);
        self.stereo.enhancer.process_block(left, right);
        self.timestretch.stretcher.process_block(left, right);
    }
}
