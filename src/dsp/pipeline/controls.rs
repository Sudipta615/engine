use super::{
    format::{
        BitPerfectReport, BitPerfectResult, EngineStats, LatencyReport, OutputSampleFormat,
        VolumePath,
    },
    DspNodeInfo, DspPipeline, PrecisionMode, PREAMP_RAMP_DURATION_MS,
};
use crate::dsp::{
    crossfade::TrackMixer,
    equalizer::EqBandParams,
    gain::{FadeState, GainProcessor},
    limiter::LimiterMode,
    loudness::{LoudnessMetadata, LoudnessMode},
    timestretch::TimeStretcher,
};

impl DspPipeline {
    pub fn set_volume(&mut self, volume: f32) {
        self.volume.set_gain(volume.clamp(0.0, 1.0));
    }

    /// Convert dB ([-60.0, 0.0]) to a linear scalar ([0.0, 1.0]).
    #[inline]
    pub fn volume_db_to_linear(db: f32) -> f32 {
        if !db.is_finite() || db <= -60.0 {
            0.0
        } else {
            10.0_f32.powf(db.clamp(-60.0, 0.0) / 20.0).clamp(0.0, 1.0)
        }
    }

    /// Convert linear scalar ([0.0, 1.0]) to dB ([-60.0, 0.0]).
    #[inline]
    pub fn volume_linear_to_db(linear: f32) -> f32 {
        if !linear.is_finite() || linear <= 1e-3 {
            -60.0
        } else {
            (20.0 * linear.clamp(0.0, 1.0).log10()).clamp(-60.0, 0.0)
        }
    }

    /// Set volume directly in dB. This is the perceptually-correct API: a UI
    /// slider mapped logarithmically to dB gives evenly-spaced perceived
    /// steps, whereas a linear 0.0–1.0 gain gives the well-known "all the
    /// volume change is in the top 20% of the slider" problem.
    ///
    /// Accepts values in the range [-60.0, 0.0] dB (full mute to unity).
    /// Values below -60 dB are clamped to -60 (effectively mute).
    pub fn set_volume_db(&mut self, db: f32) {
        if !db.is_finite() {
            log::warn!(
                "DspPipeline::set_volume_db: non-finite value {}; ignoring",
                db
            );
            return;
        }
        let linear = Self::volume_db_to_linear(db);
        self.volume.set_gain(linear);
    }

    /// Convert a UI percentage (0.0–100.0) to a dB value suitable for
    /// `set_volume_db`. Uses a logarithmic curve so 50% maps to roughly
    /// -6 dB (a perceptually mid-level volume), 25% to about -12 dB, etc.
    ///
    /// At 0% the function returns -60 dB (effectively mute).
    pub fn volume_percent_to_db(percent: f32) -> f32 {
        let p = percent.clamp(0.0, 100.0) / 100.0;
        if p <= 0.0001 {
            return -60.0;
        }
        // Map [0,1] → [-60, 0] dB logarithmically:
        // dB = 20 * log10(p) gives 0 dB at p=1, -20 dB at p=0.1, -40 dB at p=0.01.
        let scaled = p.max(1e-3);
        20.0 * scaled.log10()
    }

    /// Current volume as dB. Useful for UI display.
    pub fn volume_db(&self) -> f32 {
        Self::volume_linear_to_db(self.volume.current_gain())
    }

    pub fn volume(&self) -> &GainProcessor<f32> {
        &self.volume
    }

    pub fn set_balance(&mut self, balance: f32) {
        self.balance = balance.clamp(-1.0, 1.0);
        if self.balance >= 0.0 {
            self.balance_gain_l = 1.0 - self.balance;
            self.balance_gain_r = 1.0;
        } else {
            self.balance_gain_l = 1.0;
            self.balance_gain_r = 1.0 + self.balance;
        }
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed.clamp(0.25, 4.0);
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn begin_seek_fadeout(&mut self) {
        self.seek_fade.fade_out();
    }

    pub fn begin_seek_fadein(&mut self) {
        self.seek_fade.fade_in();
    }

    pub fn is_seek_fadeout_complete(&self) -> bool {
        self.seek_fade.is_faded_out()
    }

    pub fn apply_loudness_metadata_outgoing(&mut self, metadata: Option<LoudnessMetadata>) {
        self.out_loudness
            .set_track_metadata(&metadata.unwrap_or_default());
    }

    pub fn apply_loudness_metadata_incoming(&mut self, metadata: Option<LoudnessMetadata>) {
        self.in_loudness
            .set_track_metadata(&metadata.unwrap_or_default());
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn update_sample_rate(&mut self, sample_rate: f32) {
        if self.sample_rate == sample_rate {
            return;
        }
        let old_sample_rate = self.sample_rate;
        self.sample_rate = sample_rate;
        self.eq.set_sample_rate(sample_rate);
        self.out_loudness.set_sample_rate(sample_rate);
        self.in_loudness.set_sample_rate(sample_rate);
        self.multiband_compressor.set_sample_rate(sample_rate);
        self.convolution.set_sample_rate(sample_rate);
        self.crossfeed.set_sample_rate(sample_rate);
        self.limiter.set_sample_rate(sample_rate);
        self.stereo_enhancer.set_sample_rate(sample_rate);
        self.out_preamp
            .set_slew_rate(1.0 / (PREAMP_RAMP_DURATION_MS * 0.001 * sample_rate));
        self.in_preamp
            .set_slew_rate(1.0 / (PREAMP_RAMP_DURATION_MS * 0.001 * sample_rate));
        self.volume
            .set_slew_rate(1.0 / (self.volume_fade_ms * 0.001 * sample_rate));
        self.seek_fade.set_sample_rate(sample_rate);
        // Rescale the mixer's frame clock without resetting an active
        // transition. This preserves the normalized crossfade/fade progress
        // when a device changes rate in the middle of an overlap.
        self.mixer.rescale_sample_rate(old_sample_rate, sample_rate);
        // The stretcher's algorithms are sample-domain; update its reported
        // rate in place so its input/output FIFOs and phase remain intact.
        self.timestretcher.set_sample_rate(sample_rate);
    }

    pub fn reset(&mut self) {
        self.out_preamp.reset();
        self.out_loudness.reset();
        self.in_preamp.reset();
        self.in_loudness.reset();
        self.eq.reset();
        self.multiband_compressor.reset();
        self.convolution.reset();
        self.crossfeed.reset();
        self.stereo_enhancer.reset();
        self.timestretcher.reset();
        self.limiter.reset();
        self.seek_fade.reset();
        self.mixer.reset();
    }

    pub fn reset_filters_only(&mut self) {
        self.out_preamp.reset();
        self.out_loudness.reset();
        self.in_preamp.reset();
        self.in_loudness.reset();
        self.eq.reset();
        self.multiband_compressor.reset();
        self.convolution.reset();
        self.crossfeed.reset();
        self.stereo_enhancer.reset();
        self.timestretcher.reset();
        self.limiter.reset();
        self.seek_fade.reset();
    }

    pub fn timestretcher_mut(&mut self) -> &mut TimeStretcher {
        &mut self.timestretcher
    }

    pub fn timestretcher(&self) -> &TimeStretcher {
        &self.timestretcher
    }

    pub fn mixer_mut(&mut self) -> &mut TrackMixer {
        &mut self.mixer
    }

    pub fn mixer(&self) -> &TrackMixer {
        &self.mixer
    }

    /// Capacity reserved for the f64 promotion scratch used by realtime
    /// block processing. Exposed for diagnostics and allocation regression
    /// tests; it must not change during playback.
    pub fn realtime_scratch_capacity(&self) -> usize {
        self.scratch_f64_l
            .capacity()
            .min(self.scratch_f64_r.capacity())
    }

    pub fn set_limiter_enabled(&mut self, enabled: bool) {
        self.limiter.set_enabled(enabled);
    }

    // ── Precision & Bit-Perfect ───────────────────────────────────────────

    /// Set the DSP processing precision (f32 Performance vs f64 Quality).
    pub fn set_precision_mode(&mut self, mode: PrecisionMode) {
        self.precision_mode = mode;
    }

    pub fn precision_mode(&self) -> PrecisionMode {
        self.precision_mode
    }

    /// Enable or disable bit-perfect mode.
    ///
    /// In bit-perfect mode the entire sample-processing graph is bypassed:
    /// EQ, loudness, dynamics, convolution, balance, software volume, fades,
    /// and the final limiter. Hardware endpoint volume remains outside this
    /// graph; software volume commands are rejected by the engine while the
    /// mode is active.
    pub fn set_bit_perfect(&mut self, enabled: bool) {
        self.bit_perfect = enabled;
        if enabled {
            self.volume.set_gain(1.0);
            self.volume.snap();
            self.seek_fade.reset();
        }
    }

    pub fn is_bit_perfect(&self) -> bool {
        self.bit_perfect
    }

    /// Enable or disable the DoP (DSD-over-PCM) hard bypass.
    ///
    /// In DoP mode the samples are 24-bit DoP words and must reach the DAC
    /// unmodified, so the whole chain — including volume and seek fades — is
    /// skipped. Mutually exclusive with normal DSP; the engine manages this
    /// per-track.
    pub fn set_dop_bypass(&mut self, enabled: bool) {
        self.dop_bypass = enabled;
    }

    pub fn is_dop_bypass(&self) -> bool {
        self.dop_bypass
    }

    /// Compute the recommended EQ preamp to prevent the limiter from
    /// constantly working against a boosted EQ curve.
    ///
    /// Evaluates the true cascaded frequency response |H_total(f)| = \prod |H_i(f)|
    /// across 20 Hz – 20 kHz to find the true peak combined gain.
    ///
    /// Returns `-max(|H(f)|_dB)` or `0.0` if no boost is present or EQ is disabled.
    ///
    /// When `EqConfig::auto_headroom` is enabled this compensation is applied
    /// automatically by the EQ itself; this method remains available for
    /// diagnostics and manual gain staging.
    pub fn recommended_preamp_db(&self) -> f32 {
        if !self.eq.is_enabled() {
            return 0.0;
        }
        let max_boost = self.eq.combined_max_gain_db(self.sample_rate);
        if max_boost > 0.0 {
            -max_boost
        } else {
            0.0
        }
    }

    /// Current limiter gain reduction in dB (≤ 0; 0 = no reduction).
    pub fn limiter_gain_reduction_db(&self) -> f32 {
        self.limiter.gain_reduction_db()
    }

    /// Maximum true-peak observed by the limiter since the last reset.
    pub fn limiter_max_true_peak_dbtp(&self) -> f32 {
        self.limiter.max_true_peak_dbtp()
    }

    /// Set the limiter mode (Transparent brick-wall or Saturate soft-clip).
    pub fn set_limiter_mode(&mut self, mode: LimiterMode) {
        self.limiter.set_mode(mode);
    }

    /// Check if all DSP processing in the pipeline is completely bypassed / neutral
    /// for transparent bit-perfect output.
    pub fn is_dsp_transparent(&self) -> (bool, Option<String>) {
        if self.bit_perfect {
            return (true, None);
        }
        if self.eq.is_enabled() {
            return (false, Some("EQ is active".to_string()));
        }
        if self.midside_eq_enabled {
            return (false, Some("Mid/Side EQ processing is active".to_string()));
        }
        if self.multiband_compressor.is_enabled() {
            return (false, Some("Multiband compressor is active".to_string()));
        }
        if self.convolution.is_enabled() && self.convolution.is_ir_loaded() {
            return (
                false,
                Some("Convolution reverb/cabinet is active".to_string()),
            );
        }
        if self.crossfeed.is_enabled() {
            return (false, Some("Crossfeed is active".to_string()));
        }
        if self.stereo_enhancer.is_enabled() && (self.stereo_enhancer.width() - 1.0).abs() > 1e-4 {
            return (false, Some("Stereo enhancer is active".to_string()));
        }
        if (self.balance_gain_l - 1.0).abs() > 1e-4 || (self.balance_gain_r - 1.0).abs() > 1e-4 {
            return (false, Some("Stereo balance is not centered".to_string()));
        }
        if (self.volume.current_gain() - 1.0).abs() > 1e-4 {
            return (
                false,
                Some("Software volume is not 0 dB (unity)".to_string()),
            );
        }
        if self.seek_fade.state != FadeState::Idle || (self.seek_fade.gain() - 1.0).abs() > 1e-4 {
            return (false, Some("Seek / transition fade active".to_string()));
        }
        if self.limiter.is_enabled() {
            return (false, Some("Limiter is enabled in DSP path".to_string()));
        }
        if (self.out_preamp.current_gain() - 1.0).abs() > 1e-4
            || (self.in_preamp.current_gain() - 1.0).abs() > 1e-4
        {
            return (false, Some("Preamp gain is not unity (0 dB)".to_string()));
        }
        if self.out_loudness.is_enabled() || self.in_loudness.is_enabled() {
            return (false, Some("Loudness normalizer is active".to_string()));
        }
        if (self.speed - 1.0).abs() > 1e-4 {
            return (false, Some("Playback speed is not 1.0x".to_string()));
        }
        (true, None)
    }

    /// Evaluate the authoritative bit-perfect report against the ACTUAL
    /// environment (rates, depths, resampler and exclusivity).
    ///
    /// This is the single source of truth — `engine_stats()` and the engine
    /// tick derive `EngineStats.bit_perfect` from `is_bit_perfect` and never
    /// compute a second verdict.
    pub fn bit_perfect_report(
        &self,
        source_sample_rate: u32,
        output_sample_rate: u32,
        source_bit_depth: u32,
        output_bit_depth: u32,
        resampler_active: bool,
        output_exclusive: bool,
    ) -> BitPerfectReport {
        self.bit_perfect_report_with_format(
            source_sample_rate,
            output_sample_rate,
            source_bit_depth,
            output_bit_depth,
            OutputSampleFormat::from_bit_depth(output_bit_depth),
            resampler_active,
            output_exclusive,
        )
    }

    /// Output-format-aware bit-perfect report. Unlike the legacy depth-only
    /// API, this distinguishes integer containers from float output.
    // Report-shaped API: every argument is an independent field of the
    // verdict; grouping them would hide the caller-side meaning.
    #[allow(clippy::too_many_arguments)]
    pub fn bit_perfect_report_with_format(
        &self,
        source_sample_rate: u32,
        output_sample_rate: u32,
        source_bit_depth: u32,
        output_bit_depth: u32,
        output_format: OutputSampleFormat,
        resampler_active: bool,
        output_exclusive: bool,
    ) -> BitPerfectReport {
        let volume_unity = self.bit_perfect
            || ((self.volume.current_gain() - 1.0).abs() <= 1e-4
                && (self.volume.target_gain - 1.0).abs() <= 1e-4
                && self.seek_fade.state == FadeState::Idle
                && (self.seek_fade.gain() - 1.0).abs() <= 1e-4
                && (self.balance_gain_l - 1.0).abs() <= 1e-4
                && (self.balance_gain_r - 1.0).abs() <= 1e-4
                && (self.out_preamp.current_gain() - 1.0).abs() <= 1e-4
                && (self.out_preamp.target_gain - 1.0).abs() <= 1e-4
                && (self.in_preamp.current_gain() - 1.0).abs() <= 1e-4
                && (self.in_preamp.target_gain - 1.0).abs() <= 1e-4);

        let eq_bypassed = self.bit_perfect || (!self.eq.is_enabled() && !self.midside_eq_enabled);

        // Per-stage flags (spec §13 lists each stage explicitly) and their
        // aggregate. Splitting them means a diagnostic panel can say exactly
        // which stage invalidates bit-perfect instead of a bundled "DSP".
        let limiter_bypassed = self.bit_perfect || !self.limiter.is_enabled();
        let compressor_bypassed = self.bit_perfect || !self.multiband_compressor.is_enabled();
        let convolution_bypassed =
            self.bit_perfect || !self.convolution.is_enabled() || !self.convolution.is_ir_loaded();
        // The frozen pipeline oracle hosts no correction stage (Phase 7 S5
        // lives in the graph), so this term is always bypassed here; the
        // graph's twin builder reads the live CorrectionNode.
        let correction_bypassed = true;
        let crossfeed_bypassed = self.bit_perfect || !self.crossfeed.is_enabled();
        let stereo_bypassed = self.bit_perfect
            || !self.stereo_enhancer.is_enabled()
            || (self.stereo_enhancer.width() - 1.0).abs() <= 1e-4;
        let loudness_bypassed =
            self.bit_perfect || (!self.out_loudness.is_enabled() && !self.in_loudness.is_enabled());
        let dynamics_bypassed = limiter_bypassed
            && compressor_bypassed
            && convolution_bypassed
            && correction_bypassed
            && crossfeed_bypassed
            && stereo_bypassed
            && loudness_bypassed;

        let resampler_bypassed = !resampler_active && (self.speed - 1.0).abs() <= 1e-4;
        let sample_rate_matched =
            source_sample_rate > 0 && source_sample_rate == output_sample_rate;
        // NOT "matched" — the check is that no known source bit is truncated by
        // the output container (a 24-bit source in a 32-bit container is fine).
        // Unknown source or output precision cannot prove this condition.
        let format_bits = output_format.bit_depth().unwrap_or(output_bit_depth);
        let bit_depth_not_truncated = source_bit_depth > 0
            && format_bits > 0
            && source_bit_depth <= format_bits
            && output_format != OutputSampleFormat::Unknown;
        // Float output preserves known integer PCM through the engine's f32
        // precision boundary up to 24 bits. Integer containers have their
        // actual quantization width applied here, not an assumed 32-bit depth.
        let format_conversion_lossless = match output_format {
            OutputSampleFormat::F32
            | OutputSampleFormat::F64
            | OutputSampleFormat::I32
            | OutputSampleFormat::I24Le => source_bit_depth > 0 && source_bit_depth <= 24,
            OutputSampleFormat::I16 | OutputSampleFormat::U16 => {
                source_bit_depth > 0 && source_bit_depth <= 16
            }
            OutputSampleFormat::Unknown => false,
        };

        let mut reason = None;
        if !volume_unity {
            reason = Some("Volume / balance / preamp is not unity (0 dB)".to_string());
        } else if !eq_bypassed {
            reason = Some("EQ is active".to_string());
        } else if !dynamics_bypassed {
            reason = Some("Dynamics / DSP processor is active".to_string());
        } else if !resampler_bypassed {
            reason = Some("Resampler / speed modifier is active".to_string());
        } else if !sample_rate_matched {
            reason = Some(format!(
                "Sample rate mismatch: source {} Hz != output {} Hz",
                source_sample_rate, output_sample_rate
            ));
        } else if source_bit_depth == 0
            || output_bit_depth == 0
            || output_format == OutputSampleFormat::Unknown
        {
            reason = Some(format!(
                "Source or output precision is unknown (output format: {}); bit-perfect cannot be proven",
                output_format.label()
            ));
        } else if !bit_depth_not_truncated {
            reason = Some(format!(
                "Bit depth truncation: source {} bits > output {} bits",
                source_bit_depth, output_bit_depth
            ));
        } else if !format_conversion_lossless {
            reason = Some(format!(
                "Source precision ({} bits) is not lossless for {} output",
                source_bit_depth,
                output_format.label()
            ));
        } else if !output_exclusive {
            reason = Some("Output is not direct/exclusive hardware".to_string());
        }

        // ── Samples vs transport split (§13) ───────────────────────────────
        // Bit-perfect samples: every sample-domain condition holds — the
        // sequence reaching the transport is provably identical to the source.
        let bit_perfect_samples = volume_unity
            && eq_bypassed
            && dynamics_bypassed
            && resampler_bypassed
            && sample_rate_matched
            && bit_depth_not_truncated
            && format_conversion_lossless;
        // Bit-perfect transport: the digital transport itself is direct /
        // exclusive / bitstream with no hidden mixer. A shared or unverified
        // transport invalidates bit-perfect even when the sample path is
        // untouched.
        let bit_perfect_transport = output_exclusive;
        let is_bit_perfect = bit_perfect_samples && bit_perfect_transport;

        let provably_modified = !volume_unity
            || !eq_bypassed
            || !dynamics_bypassed
            || !resampler_bypassed
            || !sample_rate_matched;
        let result = if is_bit_perfect {
            BitPerfectResult::BitPerfect
        } else if provably_modified {
            BitPerfectResult::Dsp
        } else {
            BitPerfectResult::Unknown
        };

        // The legacy boolean-only API synthesizes an access state from the
        // transport flag. Callers that know the real negotiated state (the
        // engine tick) must use `bit_perfect_report_with_access` so the
        // requested/actual/verified/fallback fields reflect the backend's
        // own report rather than an inference.
        let access_requested = config::OutputAccessMode::Exclusive;
        let access_actual = if output_exclusive {
            config::OutputAccessMode::Exclusive
        } else {
            config::OutputAccessMode::Shared
        };
        let access_verified = output_exclusive;

        BitPerfectReport {
            is_bit_perfect,
            volume_unity,
            eq_bypassed,
            dynamics_bypassed,
            compressor_bypassed,
            convolution_bypassed,
            correction_bypassed,
            crossfeed_bypassed,
            stereo_bypassed,
            limiter_bypassed,
            loudness_bypassed,
            resampler_bypassed,
            sample_rate_matched,
            bit_depth_not_truncated,
            format_conversion_lossless,
            output_format,
            output_exclusive,
            bit_perfect_samples,
            bit_perfect_transport,
            access_requested,
            access_actual,
            access_verified,
            fallback_occurred: false,
            // Engine-owned fields; the engine tick fills these from the live
            // decoder / output state (the pipeline does not own them) and
            // re-derives the verdict with dither/crossfade invalidation.
            source_channels: 0,
            output_channels: 0,
            decoder_lossless: false,
            crossfade_active: false,
            dither_active: false,
            volume_path: VolumePath::None,
            result,
            reason,
        }
    }

    /// Output-format-aware bit-perfect report with the REAL negotiated access
    /// state (requested / actual / verified) and fallback flag.
    ///
    /// The transport verdict is derived from
    /// `access_state.is_bit_perfect() && !fallback_occurred` — verified
    /// direct/exclusive access with no silent downgrade — and the report's
    /// access fields are filled from the backend's own report, never inferred
    /// from device-name heuristics. This is the authoritative report the
    /// engine tick publishes.
    // Report-shaped API: one positional argument per report field; a struct
    // would add noise for a pure read-only aggregation.
    #[allow(clippy::too_many_arguments)]
    pub fn bit_perfect_report_with_access(
        &self,
        source_sample_rate: u32,
        output_sample_rate: u32,
        source_bit_depth: u32,
        output_bit_depth: u32,
        output_format: OutputSampleFormat,
        resampler_active: bool,
        access_state: config::OutputAccessState,
        fallback_occurred: bool,
    ) -> BitPerfectReport {
        let transport = access_state.is_bit_perfect() && !fallback_occurred;
        let mut report = self.bit_perfect_report_with_format(
            source_sample_rate,
            output_sample_rate,
            source_bit_depth,
            output_bit_depth,
            output_format,
            resampler_active,
            transport,
        );
        report.access_requested = access_state.requested;
        report.access_actual = access_state.actual;
        report.access_verified = access_state.verified;
        report.fallback_occurred = fallback_occurred;
        // Re-derive the coarse verdict now that the transport is authoritative
        // (a fallback can flip transport while leaving the samples untouched).
        report.is_bit_perfect = report.bit_perfect_samples && report.bit_perfect_transport;
        report.result = if report.is_bit_perfect {
            BitPerfectResult::BitPerfect
        } else if report.bit_perfect_samples {
            BitPerfectResult::Unknown
        } else {
            BitPerfectResult::Dsp
        };
        report
    }

    /// Compute the end-to-end latency report.
    ///
    /// The DSP-side terms (limiter and convolution) are computed from the
    /// live components; the output-domain terms (resampler group delay, ring
    /// buffer fill, and device buffer) are supplied by the engine, which owns
    /// those components. Every term is already in the output domain: the
    /// safety limiter runs after resampling, so its delay is measured at the
    /// output sample rate and summed directly with the other stages.
    pub fn latency_report(
        &self,
        resampler_latency_ms: f32,
        ring_buffer_latency_ms: f32,
        output_device_latency_ms: f32,
    ) -> LatencyReport {
        let limiter_lookahead_ms = if self.limiter.is_enabled() {
            self.limiter.lookahead_ms()
        } else {
            0.0
        };
        let limiter_detector_delay_ms = if self.limiter.is_enabled() {
            self.limiter.detector_delay_ms()
        } else {
            0.0
        };
        let convolution_latency_ms = if self.convolution.is_enabled() {
            self.convolution.latency_ms()
        } else {
            0.0
        };
        // The frozen pipeline oracle hosts no correction stage (Phase 7 S5
        // lives in the graph); the graph's twin builder reports the live
        // CorrectionNode latency.
        let correction_latency_ms = 0.0;
        let crossfeed_delay_ms = self.crossfeed.latency_ms();
        let timestretch_latency_ms = self.timestretcher.latency_ms();
        let total_latency_ms = limiter_lookahead_ms
            + convolution_latency_ms
            + correction_latency_ms
            + crossfeed_delay_ms
            + timestretch_latency_ms
            + resampler_latency_ms
            + ring_buffer_latency_ms
            + output_device_latency_ms;

        LatencyReport {
            limiter_lookahead_ms,
            limiter_detector_delay_ms,
            convolution_latency_ms,
            correction_latency_ms,
            crossfeed_delay_ms,
            timestretch_latency_ms,
            resampler_latency_ms,
            ring_buffer_latency_ms,
            output_device_latency_ms,
            total_latency_ms,
        }
    }

    /// Live DSP graph snapshot (spec §24): one row per stage in
    /// [`super::DSP_STAGE_CAPABILITIES`] order, merging the static capability
    /// metadata with the stage's current runtime state (active flag plus its
    /// deterministic latency and tail terms, spec §19).
    ///
    /// In bit-perfect or DoP bypass mode every user-DSP stage reports
    /// inactive — the graph really is a passthrough, and this snapshot says
    /// so rather than guessing.
    ///
    /// Stages the pipeline does not host are reported honestly: `resampler`
    /// and `dither` live in the engine/output layer, so they are inactive
    /// here and their latency terms enter [`LatencyReport`] through the
    /// engine's parameters. `channel_eq` / `bass_management` / `channel_mix`
    /// are consolidated under the `channel_trim` node in the current
    /// pipeline, so they report inactive and the implementing node carries
    /// the active state.
    pub fn graph_nodes(&self) -> Vec<DspNodeInfo> {
        use super::DSP_STAGE_CAPABILITIES;

        let bypassed = self.bit_perfect || self.dop_bypass;
        let mc_channels = self.multichannel_layout.channel_count();
        let mut nodes = Vec::with_capacity(DSP_STAGE_CAPABILITIES.len());
        for cap in DSP_STAGE_CAPABILITIES {
            let (active, latency_ms, tail_ms) = if bypassed {
                (false, 0.0, 0.0)
            } else {
                match cap.name {
                    "channel_trim" => (self.channel_trim.is_active(mc_channels), 0.0, 0.0),
                    "channel_eq" | "bass_management" | "channel_mix" => (false, 0.0, 0.0),
                    "out_preamp" => (self.out_preamp.current_gain() != 1.0, 0.0, 0.0),
                    "in_preamp" => (self.in_preamp.current_gain() != 1.0, 0.0, 0.0),
                    "out_loudness" => (self.out_loudness.is_enabled(), 0.0, 0.0),
                    "in_loudness" => (self.in_loudness.is_enabled(), 0.0, 0.0),
                    "mixer" => (self.mixer.is_enabled(), 0.0, 0.0),
                    "eq" => (self.eq.is_enabled(), 0.0, 0.0),
                    "multiband_compressor" => (self.multiband_compressor.is_enabled(), 0.0, 0.0),
                    "convolution" => {
                        if self.convolution.is_enabled() && self.convolution.is_ir_loaded() {
                            let latency_ms = self.convolution.latency_ms();
                            let ir_len =
                                self.convolution.num_partitions() * self.convolution.block_size();
                            let ir_len_ms = ir_len as f32 / self.sample_rate * 1000.0;
                            (true, latency_ms, (ir_len_ms - latency_ms).max(0.0))
                        } else {
                            (false, 0.0, 0.0)
                        }
                    }
                    "balance" => (self.balance != 0.0, 0.0, 0.0),
                    "crossfeed" => {
                        if self.crossfeed.is_enabled() {
                            let d = self.crossfeed.latency_ms();
                            (true, d, d)
                        } else {
                            (false, 0.0, 0.0)
                        }
                    }
                    "stereo_enhancer" => (self.stereo_enhancer.is_enabled(), 0.0, 0.0),
                    "timestretch" => {
                        let active = self.timestretcher.is_enabled();
                        let latency = if active {
                            self.timestretcher.latency_ms()
                        } else {
                            0.0
                        };
                        (active, latency, 0.0)
                    }
                    "volume" => (self.volume.current_gain() != 1.0, 0.0, 0.0),
                    "seek_fade" => (self.seek_fade.state != FadeState::Idle, 0.0, 0.0),
                    "limiter" => {
                        let active = self.limiter.is_enabled();
                        let lookahead = if active {
                            self.limiter.lookahead_ms()
                        } else {
                            0.0
                        };
                        let tail = if active {
                            self.limiter.release_ms()
                        } else {
                            0.0
                        };
                        (active, lookahead, tail)
                    }
                    "resampler" | "dither" => (false, 0.0, 0.0),
                    _ => (false, 0.0, 0.0),
                }
            };
            nodes.push(DspNodeInfo {
                name: cap.name,
                active,
                latency_ms,
                tail_ms,
            });
        }
        nodes
    }

    /// Snapshot the current DSP state into an [`EngineStats`] struct with default latencies.
    #[allow(clippy::too_many_arguments)]
    pub fn engine_stats(
        &self,
        source_sample_rate: u32,
        output_sample_rate: u32,
        source_bit_depth: u32,
        output_bit_depth: u32,
        resampler_active: bool,
        output_exclusive: bool,
    ) -> EngineStats {
        self.engine_stats_with_latency(
            source_sample_rate,
            output_sample_rate,
            source_bit_depth,
            output_bit_depth,
            resampler_active,
            output_exclusive,
            0.0,
            0.0,
            0.0,
        )
    }

    /// Snapshot the current DSP state into an [`EngineStats`] struct with measured pipeline latencies.
    #[allow(clippy::too_many_arguments)]
    pub fn engine_stats_with_latency(
        &self,
        source_sample_rate: u32,
        output_sample_rate: u32,
        source_bit_depth: u32,
        output_bit_depth: u32,
        resampler_active: bool,
        output_exclusive: bool,
        resampler_latency_ms: f32,
        ring_buffer_latency_ms: f32,
        output_device_latency_ms: f32,
    ) -> EngineStats {
        self.engine_stats_with_output_format(
            source_sample_rate,
            output_sample_rate,
            source_bit_depth,
            output_bit_depth,
            OutputSampleFormat::from_bit_depth(output_bit_depth),
            resampler_active,
            output_exclusive,
            resampler_latency_ms,
            ring_buffer_latency_ms,
            output_device_latency_ms,
        )
    }

    /// Snapshot diagnostics with the actual negotiated output sample format.
    #[allow(clippy::too_many_arguments)]
    pub fn engine_stats_with_output_format(
        &self,
        source_sample_rate: u32,
        output_sample_rate: u32,
        source_bit_depth: u32,
        output_bit_depth: u32,
        output_format: OutputSampleFormat,
        resampler_active: bool,
        output_exclusive: bool,
        resampler_latency_ms: f32,
        ring_buffer_latency_ms: f32,
        output_device_latency_ms: f32,
    ) -> EngineStats {
        let bp_report = self.bit_perfect_report_with_format(
            source_sample_rate,
            output_sample_rate,
            source_bit_depth,
            output_bit_depth,
            output_format,
            resampler_active,
            output_exclusive,
        );
        let latency_report = self.latency_report(
            resampler_latency_ms,
            ring_buffer_latency_ms,
            output_device_latency_ms,
        );

        EngineStats {
            dsp_precision: match self.precision_mode {
                PrecisionMode::Performance => "f32".to_string(),
                PrecisionMode::Quality => "f64".to_string(),
            },
            output_format: output_format.label().to_string(),
            eq_enabled: self.eq.is_enabled(),
            eq_auto_headroom: self.eq.is_auto_headroom(),
            compressor_enabled: self.multiband_compressor.is_enabled(),
            limiter_enabled: self.limiter.is_enabled(),
            limiter_gain_reduction_db: self.limiter_gain_reduction_db(),
            bit_perfect: bp_report.is_bit_perfect,
            bit_perfect_reason: bp_report.reason.clone(),
            bit_perfect_report: bp_report,
            output_latency_ms: latency_report.total_latency_ms,
            latency_report,
            ..Default::default()
        }
    }

    pub fn set_preamp_db(&mut self, db: f32) {
        self.eq.set_preamp_db(db);
    }

    pub fn set_bass_shelf(&mut self, gain_db: f32) {
        self.eq.set_bass_shelf(gain_db);
    }

    pub fn set_treble_shelf(&mut self, gain_db: f32) {
        self.eq.set_treble_shelf(gain_db);
    }

    pub fn set_eq_enabled(&mut self, enabled: bool) {
        self.eq.set_enabled(enabled);
    }

    pub fn set_eq_auto_headroom(&mut self, enabled: bool) {
        self.eq.set_auto_headroom(enabled);
    }

    pub fn set_eq_band(&mut self, index: usize, params: EqBandParams) {
        self.eq.set_band(index, params);
    }

    pub fn eq_num_bands(&self) -> usize {
        self.eq.num_bands()
    }

    pub fn set_midside_eq(&mut self, enabled: bool) {
        self.midside_eq_enabled = enabled;
    }

    pub fn is_midside_eq(&self) -> bool {
        self.midside_eq_enabled
    }

    pub fn set_convolution_wet_mix(&mut self, mix: f32) {
        self.convolution.set_wet_mix(mix);
    }

    pub fn convolution_ir_needs_reload(&self) -> bool {
        self.convolution.ir_needs_reload()
    }

    pub fn set_stereo_width(&mut self, width: f32) {
        let normalized = if width > 2.0 { width / 100.0 } else { width };
        self.stereo_enhancer.set_width(normalized);
        self.stereo_enhancer
            .set_enabled((normalized - 1.0).abs() > 0.001);
    }

    pub fn set_stereo_enhancer_enabled(&mut self, enabled: bool) {
        self.stereo_enhancer.set_enabled(enabled);
    }

    pub fn set_limiter_params(
        &mut self,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool,
    ) {
        self.limiter.set_lookahead(lookahead_ms);
        self.limiter.set_attack(attack_ms);
        self.limiter.set_release(release_ms);
        self.limiter.set_ceiling_db(ceiling_db);
        self.limiter.set_soft_clip(soft_clip);
    }

    /// Enable or disable true-peak (inter-sample peak) detection on the
    /// limiter. See `LookaheadLimiter::enable_true_peak` for details.
    pub fn set_limiter_true_peak(&mut self, enabled: bool) {
        self.limiter.enable_true_peak(enabled);
    }

    /// Whether true-peak detection is currently active on the limiter.
    pub fn limiter_true_peak_enabled(&self) -> bool {
        self.limiter.true_peak_enabled()
    }

    pub fn set_crossfeed_enabled(&mut self, enabled: bool) {
        self.crossfeed.set_enabled(enabled);
    }

    pub fn set_crossfeed_profile(&mut self, profile: config::types::enums::CrossfeedProfile) {
        self.crossfeed.set_profile(profile);
    }

    pub fn set_crossfeed_custom_params(&mut self, frequency_hz: f32, q: f32, delay_ms: f32) {
        self.crossfeed.set_custom_params(frequency_hz, q, delay_ms);
    }

    pub fn set_compressor_enabled(&mut self, enabled: bool) {
        self.multiband_compressor.set_enabled(enabled);
    }

    pub fn set_compressor_band_params(
        &mut self,
        band: usize,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    ) {
        self.multiband_compressor.set_band_params(
            band,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_gain_db,
        );
    }

    /// Set a compressor band's detector features: soft-knee width (dB),
    /// detector mode and stereo linking.
    pub fn set_compressor_band_features(
        &mut self,
        band: usize,
        knee_db: f32,
        detector: config::CompressorDetector,
        stereo_link: bool,
    ) {
        self.multiband_compressor
            .set_band_features(band, knee_db, detector, stereo_link);
    }

    pub fn set_loudness_mode(&mut self, mode: LoudnessMode) {
        self.out_loudness.set_mode(mode);
        self.in_loudness.set_mode(mode);
    }
}
