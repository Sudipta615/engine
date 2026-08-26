use super::{DspPipeline, PrecisionMode};
use crate::buffer::{validate_audio_block, AudioBlockError, MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};

impl DspPipeline {
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.dop_bypass {
            // DoP bitstream: no stage may touch the samples (not even volume).
            return (left, right);
        }
        if self.bit_perfect {
            // Bit-perfect mode is a hard transport contract: software volume,
            // seek fades, and every DSP stage are bypassed. The caller must
            // use hardware volume or remain at unity.
            return (left, right);
        }
        match self.precision_mode {
            PrecisionMode::Performance => {
                let (l, r) = self.process_outgoing(left, right);
                self.process_post_mix(l, r)
            }
            PrecisionMode::Quality => {
                // f64 path: promote -> outgoing (preamp + loudness) -> post-mix -> demote
                let (out_l, out_r) = self.process_outgoing_f64(left as f64, right as f64);
                let (l64, r64) = self.process_post_mix_f64(out_l, out_r);
                (l64 as f32, r64 as f32)
            }
        }
    }

    /// Process a stereo pair in f64 precision (Quality mode).
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if self.dop_bypass {
            return (left, right);
        }
        if self.bit_perfect {
            return (left, right);
        }
        let (out_l, out_r) = self.process_outgoing_f64(left, right);
        self.process_post_mix_f64(out_l, out_r)
    }

    /// Process a block of stereo frames in place. The bit-perfect check and
    /// the precision-mode dispatch run once per block instead of once per
    /// frame; the whole chain is then applied stage-by-stage over the block,
    /// which lets each stage's enabled check be hoisted out of its loop.
    ///
    /// Semantics are identical to calling [`Self::process`] per frame.
    /// Process a block, splitting oversized caller buffers into bounded
    /// realtime blocks. Each DSP stage therefore sees at most
    /// `MAX_AUDIO_BLOCK_FRAMES` frames and can keep fixed scratch storage.
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
        if n == 0 {
            return;
        }
        if self.dop_bypass {
            // DoP bitstream: pure passthrough.
            return;
        }
        if self.bit_perfect {
            // Hard bypass: no software volume or fade is allowed to alter
            // the sample sequence in Bit-Perfect mode.
            return;
        }
        match self.precision_mode {
            PrecisionMode::Performance => {
                self.process_outgoing_block(left, right);
                self.process_post_mix_block(left, right);
            }
            PrecisionMode::Quality => {
                // Promote the block to f64, run the f64 chain, demote back.
                // The scratch Vecs are moved out of `self` (O(1)) so the
                // chain calls below can borrow `self` mutably without
                // aliasing the scratch buffers; the allocation is retained
                // when they are put back.
                let mut l64 = std::mem::take(&mut self.scratch_f64_l);
                let mut r64 = std::mem::take(&mut self.scratch_f64_r);
                debug_assert!(n <= MAX_AUDIO_BLOCK_FRAMES);
                l64[..n].fill(0.0);
                r64[..n].fill(0.0);
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
                self.scratch_f64_l = l64;
                self.scratch_f64_r = r64;
            }
        }
    }

    /// Process an interleaved block of `channels`-channel frames in place.
    ///
    /// - `channels <= 2`: identical to [`Self::process_block`] on the front
    ///   L/R pair (a mono source is duplicated to both channels), preserving
    ///   the stereo path's Quality-mode f64 promotion.
    /// - `channels > 2`: a multichannel-capable path:
    ///   1. The channel-agnostic gain stages (out preamp, out loudness) run
    ///      on **every** channel.
    ///   2. The stereo filter stages (EQ, multiband compressor, convolution,
    ///      balance, crossfeed, stereo enhancer, timestretch) run on the
    ///      **front L/R pair only** — those stages own stereo-linked or
    ///      stereo-only state and are not yet N-channel.
    ///   3. Volume and seek-fade run on **every** channel.
    ///
    /// The >2-channel path runs in f32 regardless of [`PrecisionMode`]; it is
    /// not f64-promoted, mirroring the engine's other documented f32
    /// boundaries. The final safety limiter is applied separately by the
    /// caller (it is stereo-linked).
    pub fn process_block_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        if channels == 0 {
            return;
        }
        if channels > MAX_CHANNELS {
            log::warn!(
                "process_block_multichannel: {} channels exceed MAX_CHANNELS ({})",
                channels,
                MAX_CHANNELS
            );
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
            // Both raw DSD/DoP and Bit-Perfect transport bypass every sample
            // transform, including multichannel trim and software volume.
            return;
        }

        // Take the reusable de-interleave planes out of `self` so the stage
        // calls below can borrow `self` mutably without aliasing the scratch.
        let mut planes = std::mem::take(&mut self.scratch_mc);

        if channels <= 2 {
            // Delegate to the stereo block path, which keeps the Quality-mode
            // f64 promotion and the exact stereo chain order.
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
            self.scratch_mc = planes;
            return;
        }

        // De-interleave every channel into its own plane.
        for ch in 0..channels {
            for i in 0..n {
                planes[ch][i] = interleaved[i * channels + ch];
            }
        }

        // Channel management stage: per-channel gain/delay/polarity trim,
        // routing matrix, and LFE gain. Applied on every channel before the
        // pre-mix chain; bypassed in bit-perfect / DoP modes like every
        // other user DSP stage.
        if !self.bit_perfect {
            self.channel_trim
                .process_planes(&mut planes[..channels], channels, n);
        }

        if self.bit_perfect {
            // Bit-perfect mode: only volume and seek fade are permitted, and
            // both are channel-agnostic planar scalars.
            self.volume
                .process_planes(&mut planes[..channels], channels, n);
            self.seek_fade
                .process_planes(&mut planes[..channels], channels, n);
        } else {
            // 1. Pre-filter gains on every channel.
            self.out_preamp
                .process_planes(&mut planes[..channels], channels, n);
            self.out_loudness
                .process_planes(&mut planes[..channels], channels, n);

            // 2. Stereo filter chain on the front L/R pair only.
            {
                let (front, rest) = planes.split_at_mut(1);
                self.process_post_mix_front_filters(&mut front[0][..n], &mut rest[0][..n]);
            }

            // 3. Post-filter gains on every channel.
            self.volume
                .process_planes(&mut planes[..channels], channels, n);
            self.seek_fade
                .process_planes(&mut planes[..channels], channels, n);
        }

        // Re-interleave back into the caller's buffer.
        for ch in 0..channels {
            for i in 0..n {
                interleaved[i * channels + ch] = planes[ch][i];
            }
        }
        self.scratch_mc = planes;
    }

    /// Stereo filter stages for the >2-channel multichannel path: EQ →
    /// multiband compressor → convolution → balance → crossfeed → stereo
    /// enhancer → timestretch. Volume and seek-fade are intentionally NOT
    /// applied here (they run on every channel in
    /// [`Self::process_block_multichannel`]).
    fn process_post_mix_front_filters(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }
        if self.midside_eq_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.process(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.process_block(left, right);
        }
        self.multiband_compressor.process_block(left, right);
        self.convolution.process_block(left, right);
        let bl = self.balance_gain_l;
        let br = self.balance_gain_r;
        for i in 0..n {
            left[i] *= bl;
            right[i] *= br;
        }
        self.crossfeed.process_block(left, right);
        self.stereo_enhancer.process_block(left, right);
        self.timestretcher.process_block(left, right);
    }

    /// Validate and process a block without silently splitting it.
    pub fn try_process_block(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), AudioBlockError> {
        validate_audio_block(left.len().min(right.len()))?;
        self.process_block(left, right);
        Ok(())
    }

    /// Validate and process an f64 block without silently splitting it.
    pub fn try_process_block_f64(
        &mut self,
        left: &mut [f64],
        right: &mut [f64],
    ) -> Result<(), AudioBlockError> {
        validate_audio_block(left.len().min(right.len()))?;
        self.process_block_f64(left, right);
        Ok(())
    }

    /// Process a block of stereo frames in f64 precision in place.
    /// Semantics are identical to calling [`Self::process_f64`] per frame.
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
        if n == 0 {
            return;
        }
        if self.dop_bypass {
            return;
        }
        if self.bit_perfect {
            return;
        }
        self.process_outgoing_block_f64(left, right);
        self.process_post_mix_block_f64(left, right);
    }

    /// Process the pre-mix chain (preamp + loudness) for a block of stereo
    /// frames in place. Equivalent to calling [`Self::process_outgoing`] per
    /// frame.
    #[inline]
    pub fn process_outgoing_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        debug_assert!(left.len().min(right.len()) <= MAX_AUDIO_BLOCK_FRAMES);
        if self.bit_perfect {
            return;
        }
        self.out_preamp.process_block_stereo(left, right);
        self.out_loudness.process_block(left, right);
    }

    /// f64 variant of [`Self::process_outgoing_block`].
    #[inline]
    pub fn process_outgoing_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bit_perfect {
            return;
        }
        self.out_preamp.process_block_stereo_f64(left, right);
        self.out_loudness.process_block_f64(left, right);
    }

    /// Process the incoming pre-mix chain (preamp + loudness) for a block of
    /// stereo frames in place. Equivalent to [`Self::process_incoming`] per
    /// frame.
    #[inline]
    pub fn process_incoming_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bit_perfect {
            return;
        }
        self.in_preamp.process_block_stereo(left, right);
        self.in_loudness.process_block(left, right);
    }

    /// f64 variant of [`Self::process_incoming_block`].
    #[inline]
    pub fn process_incoming_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bit_perfect {
            return;
        }
        self.in_preamp.process_block_stereo_f64(left, right);
        self.in_loudness.process_block_f64(left, right);
    }

    /// Process the post-mix chain (EQ → compressor → convolution → balance →
    /// crossfeed → stereo → volume → seek fade) for a block of stereo frames
    /// in place, dispatching on precision mode once per block.
    ///
    /// The final safety limiter is **not** part of this chain: it runs in the
    /// output domain, after resampling/mixing, via
    /// [`Self::process_final_limiter_block`].
    /// Equivalent to calling [`Self::process_post_mix`] per frame.
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
                let mut l64 = std::mem::take(&mut self.scratch_f64_l);
                let mut r64 = std::mem::take(&mut self.scratch_f64_r);
                debug_assert!(n <= MAX_AUDIO_BLOCK_FRAMES);
                l64[..n].fill(0.0);
                r64[..n].fill(0.0);
                for i in 0..n {
                    l64[i] = left[i] as f64;
                    r64[i] = right[i] as f64;
                }
                self.process_post_mix_block_f64(&mut l64[..n], &mut r64[..n]);
                for i in 0..n {
                    left[i] = l64[i] as f32;
                    right[i] = r64[i] as f32;
                }
                self.scratch_f64_l = l64;
                self.scratch_f64_r = r64;
            }
        }
    }

    /// f32 post-mix chain over a block (Performance mode).
    fn process_post_mix_block_f32(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if self.midside_eq_enabled {
            // Niche path: keep per-frame mid/side decomposition (no scratch).
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.process(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.process_block(left, right);
        }
        self.multiband_compressor.process_block(left, right);
        self.convolution.process_block(left, right);
        let bl = self.balance_gain_l;
        let br = self.balance_gain_r;
        for i in 0..n {
            left[i] *= bl;
            right[i] *= br;
        }
        self.crossfeed.process_block(left, right);
        self.stereo_enhancer.process_block(left, right);
        self.timestretcher.process_block(left, right);
        self.volume.process_block_stereo(left, right);
        self.seek_fade.process_block(left, right);
    }

    /// Process the post-mix chain for a block of stereo frames in native f64
    /// precision. Equivalent to calling [`Self::process_post_mix_f64`] per
    /// frame.
    pub fn process_post_mix_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bit_perfect {
            return;
        }
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }
        if self.midside_eq_enabled {
            // Niche path: keep per-frame mid/side decomposition (no scratch).
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.process_f64(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.process_block_f64(left, right);
        }
        self.multiband_compressor.process_block_f64(left, right);
        self.convolution.process_block_f64(left, right);
        let bl = self.balance_gain_l as f64;
        let br = self.balance_gain_r as f64;
        for i in 0..n {
            left[i] *= bl;
            right[i] *= br;
        }
        self.crossfeed.process_block_f64(left, right);
        self.stereo_enhancer.process_block_f64(left, right);
        self.timestretcher.process_block_f64(left, right);
        self.volume.process_block_stereo_f64(left, right);
        self.seek_fade.process_block_f64(left, right);
    }

    /// Internal f64 post-mix chain.
    #[inline]
    pub fn process_post_mix_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if self.bit_perfect {
            return (left, right);
        }
        // EQ
        let (l, r) = if self.midside_eq_enabled {
            let mid = (left + right) * 0.5;
            let side = (left - right) * 0.5;
            let (eq_mid, eq_side) = self.eq.process_f64(mid, side);
            (eq_mid + eq_side, eq_mid - eq_side)
        } else {
            self.eq.process_f64(left, right)
        };

        // Compressor (native f64)
        let (l_c, r_c) = self.multiband_compressor.process_f64(l, r);

        // Convolution (native f64 accumulation)
        let (l_cv, r_cv) = self.convolution.process_f64(l_c, r_c);

        // Balance
        let (l_b, r_b) = (
            l_cv * (self.balance_gain_l as f64),
            r_cv * (self.balance_gain_r as f64),
        );

        // Crossfeed (native f64 biquad and delay lines)
        let (l_x, r_x) = self.crossfeed.process_f64(l_b, r_b);

        // Stereo enhancer (native f64 mid/side)
        let (l_s, r_s) = self.stereo_enhancer.process_f64(l_x, r_x);

        // Volume (native f64 gain with smooth dezipper/ramp advancement)
        let (l_v, r_v) = self.volume.process_stereo_f64(l_s, r_s);

        // Seek fade (f64 cosine curve)
        let (l_f, r_f) = self.seek_fade.process_f64(l_v, r_v);
        (l_f, r_f)
    }

    #[inline]
    pub fn process_outgoing(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.bit_perfect {
            return (left, right);
        }
        let (l, r) = self.out_preamp.process_stereo(left, right);
        self.out_loudness.process(l, r)
    }

    #[inline]
    pub fn process_outgoing_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if self.bit_perfect {
            return (left, right);
        }
        let (l, r) = self.out_preamp.process_stereo_f64(left, right);
        self.out_loudness.process_f64(l, r)
    }

    #[inline]
    pub fn process_incoming(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.bit_perfect {
            return (left, right);
        }
        let (l, r) = self.in_preamp.process_stereo(left, right);
        self.in_loudness.process(l, r)
    }

    #[inline]
    pub fn process_incoming_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if self.bit_perfect {
            return (left, right);
        }
        let (l, r) = self.in_preamp.process_stereo_f64(left, right);
        self.in_loudness.process_f64(l, r)
    }

    #[inline]
    pub fn process_post_mix(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.bit_perfect {
            return (left, right);
        }
        let (mut l, mut r) = (left, right);
        if self.midside_eq_enabled {
            let mid = (l + r) * 0.5;
            let side = (l - r) * 0.5;
            let (eq_mid, eq_side) = self.eq.process(mid, side);
            l = eq_mid + eq_side;
            r = eq_mid - eq_side;
        } else {
            let (eq_l, eq_r) = self.eq.process(l, r);
            l = eq_l;
            r = eq_r;
        }
        let (l_c, r_c) = self.multiband_compressor.process(l, r);
        let (l_cv, r_cv) = self.convolution.process(l_c, r_c);
        let (l_b, r_b) = (l_cv * self.balance_gain_l, r_cv * self.balance_gain_r);
        let (l_x, r_x) = self.crossfeed.process(l_b, r_b);
        let (l_s, r_s) = self.stereo_enhancer.process(l_x, r_x);
        let (l_v, r_v) = self.volume.process_stereo(l_s, r_s);
        let (l_f, r_f) = self.seek_fade.process(l_v, r_v);
        // NOTE: Dither is NOT applied here. It must be applied at the final
        // sample-format conversion step in the cpal callback (see
        // `audio_callback_i16` / `audio_callback_u16`). Applying dither inside
        // the DSP pipeline followed by another f32 → integer conversion in
        // the callback would double-quantize and lose the dither's effect.
        // The `self.dither` field is retained for backward API compatibility
        (l_f, r_f)
    }

    /// Apply the final safety limiter to a stereo sample pair.
    ///
    /// This is the output-domain peak-protection stage: it runs *after* the
    /// resampler/mixer, at the output sample rate, and after the user volume
    /// and seek fades, so the ceiling is enforced on the final signal that
    /// reaches the DAC. Skipped in bit-perfect and DoP bypass modes.
    #[inline]
    pub fn process_final_limiter(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.bit_perfect || self.dop_bypass {
            return (left, right);
        }
        self.limiter.process(left, right)
    }

    /// Block form of [`Self::process_final_limiter`].
    pub fn process_final_limiter_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bit_perfect || self.dop_bypass {
            return;
        }
        self.limiter.process_block(left, right);
    }

    /// Block form of [`Self::process_final_limiter`] for multichannel interleaved streams.
    pub fn process_final_limiter_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        if self.bit_perfect || self.dop_bypass {
            return;
        }
        self.limiter
            .process_block_multichannel(interleaved, channels);
    }

    /// Flush the final safety limiter's lookahead tail (output-domain samples).
    ///
    /// Called at end-of-stream so the last `lookahead` output samples are
    /// emitted instead of being stranded in the limiter's delay line.  Empty
    /// in bit-perfect and DoP bypass modes, where the limiter is not applied.
    pub fn flush_final_limiter(&mut self) -> Vec<(f32, f32)> {
        if self.bit_perfect || self.dop_bypass {
            return Vec::new();
        }
        self.limiter.flush()
    }

    /// Multichannel flush for final safety limiter.
    pub fn flush_final_limiter_multichannel(&mut self, channels: usize) -> Vec<f32> {
        if self.bit_perfect || self.dop_bypass {
            return Vec::new();
        }
        self.limiter.flush_multichannel(channels)
    }
}
