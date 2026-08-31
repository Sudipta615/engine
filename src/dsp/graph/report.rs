//! Latency & graph introspection for [`DspGraph`], plus the engine-facing
//! telemetry reports (latency / bit-perfect / engine stats).

use crate::diagnostics::BitPerfectCause;
use crate::dsp::pipeline::{
    BitPerfectReport, BitPerfectResult, EngineStats, LatencyReport, OutputSampleFormat, VolumePath,
};

use super::*;

impl DspGraph {
    /// Snapshot dynamic graph nodes for diagnostics and UI telemetry.
    pub fn graph_nodes(&self) -> Vec<DspNodeInfo> {
        let bypassed = self.bit_perfect || self.dop_bypass;
        let mc_channels = self.multichannel_layout.channel_count();
        let mut nodes = Vec::with_capacity(DSP_STAGE_CAPABILITIES.len());

        for cap in DSP_STAGE_CAPABILITIES {
            let (active, latency_ms, tail_ms) = if bypassed {
                (false, 0.0, 0.0)
            } else {
                match cap.name {
                    "channel_trim" => (self.routing().trimmer.is_active(mc_channels), 0.0, 0.0),
                    "channel_eq" | "bass_management" | "channel_mix" => (false, 0.0, 0.0),
                    "out_preamp" => (self.out_preamp().is_active(), 0.0, 0.0),
                    "in_preamp" => (self.in_preamp().is_active(), 0.0, 0.0),
                    "out_loudness" => (self.out_loudness().is_active(), 0.0, 0.0),
                    "in_loudness" => (self.in_loudness().is_active(), 0.0, 0.0),
                    // Mirrors the pipeline's `mixer.is_enabled()`.
                    "mixer" => (self.mix().crossfade_enabled, 0.0, 0.0),
                    "eq" => (self.eq().is_active(), 0.0, 0.0),
                    "multiband_compressor" => (self.dynamics().is_active(), 0.0, 0.0),
                    "convolution" => {
                        if self.convolution().is_active() {
                            let latency_ms = self.convolution().engine.latency_ms();
                            let ir_len = self.convolution().engine.num_partitions()
                                * self.convolution().engine.block_size();
                            let ir_len_ms = ir_len as f32 / self.sample_rate * 1000.0;
                            (true, latency_ms, (ir_len_ms - latency_ms).max(0.0))
                        } else {
                            (false, 0.0, 0.0)
                        }
                    }
                    "correction" => {
                        let node = self.correction();
                        if node.is_active() {
                            let latency_ms = node.latency_ms(self.sample_rate);
                            let ir_len_ms = node.tail_samples() as f32 / self.sample_rate * 1000.0;
                            (true, latency_ms, ir_len_ms.max(0.0))
                        } else {
                            (false, 0.0, 0.0)
                        }
                    }
                    "balance" => (self.balance().is_active(), 0.0, 0.0),
                    "crossfeed" => {
                        if self.crossfeed().is_active() {
                            let d = self.crossfeed().crossfeed.latency_ms();
                            (true, d, d)
                        } else {
                            (false, 0.0, 0.0)
                        }
                    }
                    "stereo_enhancer" => (self.stereo().is_active(), 0.0, 0.0),
                    "timestretch" => {
                        let active = self.timestretch().is_active();
                        let latency = if active {
                            self.timestretch().stretcher.latency_ms()
                        } else {
                            0.0
                        };
                        (active, latency, 0.0)
                    }
                    "volume" => (self.volume().is_active(), 0.0, 0.0),
                    "seek_fade" => (self.seek_fade().is_active(), 0.0, 0.0),
                    "limiter" => {
                        let active = self.limiter().is_active();
                        let lookahead = if active {
                            self.limiter().limiter.lookahead_ms()
                        } else {
                            0.0
                        };
                        let tail = if active {
                            self.limiter().limiter.release_ms()
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

    /// Total deterministic graph latency in milliseconds (output domain).
    pub fn total_latency_ms(&self) -> f32 {
        if self.bit_perfect || self.dop_bypass {
            return 0.0;
        }
        let mut total = 0.0;
        if self.crossfeed().is_active() {
            total += self.crossfeed().crossfeed.latency_ms();
        }
        if self.timestretch().is_active() {
            total += self.timestretch().stretcher.latency_ms();
        }
        if self.limiter().is_active() {
            total +=
                self.limiter().limiter.lookahead_ms() + self.limiter().limiter.detector_delay_ms();
        }
        if self.convolution().is_active() {
            total += self.convolution().engine.latency_ms();
        }
        if self.correction().is_active() {
            total += self.correction().latency_ms(self.sample_rate);
        }
        total
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Engine-facing telemetry (Phase 3 S3/S4): the pipeline's report surface
// rebuilt from the graph's node state, so the engine can own the graph
// end-to-end without the pipeline.
// ─────────────────────────────────────────────────────────────────────────────

impl DspGraph {
    /// Whether a convolution IR load is still pending (the engine polls this
    /// to report IR-load progress). Mirrors the pipeline's API.
    pub fn convolution_ir_needs_reload(&self) -> bool {
        self.convolution().engine.ir_needs_reload()
    }

    /// Compute the end-to-end latency report. The DSP-side terms (limiter
    /// and convolution) come from the live nodes; the output-domain terms
    /// (resampler group delay, ring fill, device buffer) are supplied by the
    /// engine, which owns those components. Every term is already in the
    /// output domain.
    pub fn latency_report(
        &self,
        resampler_latency_ms: f32,
        ring_buffer_latency_ms: f32,
        output_device_latency_ms: f32,
    ) -> LatencyReport {
        let limiter_lookahead_ms = if self.limiter().limiter.is_enabled() {
            self.limiter().limiter.lookahead_ms()
        } else {
            0.0
        };
        let limiter_detector_delay_ms = if self.limiter().limiter.is_enabled() {
            self.limiter().limiter.detector_delay_ms()
        } else {
            0.0
        };
        let convolution_latency_ms = if self.convolution().engine.is_enabled() {
            self.convolution().engine.latency_ms()
        } else {
            0.0
        };
        let correction_latency_ms = if self.correction().is_active() {
            self.correction().latency_ms(self.sample_rate)
        } else {
            0.0
        };
        let crossfeed_delay_ms = self.crossfeed().crossfeed.latency_ms();
        let timestretch_latency_ms = self.timestretch().stretcher.latency_ms();
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

    /// Output-format-aware bit-perfect report — the graph-side twin of the
    /// pipeline's builder, reading the same per-stage state from the graph's
    /// nodes.
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
        let vol = &self.volume().processor;
        let out_pre = &self.out_preamp().processor;
        let in_pre = &self.in_preamp().processor;
        let balance = self.balance();
        let volume_unity = self.bit_perfect
            || ((vol.current_gain() - 1.0).abs() <= 1e-4
                && (vol.target_gain - 1.0).abs() <= 1e-4
                && self.seek_fade().fade.state == crate::dsp::gain::FadeState::Idle
                && (self.seek_fade().fade.gain() - 1.0).abs() <= 1e-4
                && (balance.balance_gain_l - 1.0).abs() <= 1e-4
                && (balance.balance_gain_r - 1.0).abs() <= 1e-4
                && (out_pre.current_gain() - 1.0).abs() <= 1e-4
                && (out_pre.target_gain - 1.0).abs() <= 1e-4
                && (in_pre.current_gain() - 1.0).abs() <= 1e-4
                && (in_pre.target_gain - 1.0).abs() <= 1e-4);

        let eq_bypassed =
            self.bit_perfect || (!self.eq().eq.is_enabled() && !self.eq().midside_enabled);

        let limiter_bypassed = self.bit_perfect || !self.limiter().limiter.is_enabled();
        let compressor_bypassed = self.bit_perfect || !self.dynamics().compressor.is_enabled();
        let convolution_bypassed = self.bit_perfect
            || !self.convolution().engine.is_enabled()
            || !self.convolution().engine.is_ir_loaded();
        let correction_bypassed = self.bit_perfect || !self.correction().is_active();
        let crossfeed_bypassed = self.bit_perfect || !self.crossfeed().crossfeed.is_enabled();
        let stereo_bypassed = self.bit_perfect
            || !self.stereo().enhancer.is_enabled()
            || (self.stereo().enhancer.width() - 1.0).abs() <= 1e-4;
        let loudness_bypassed = self.bit_perfect
            || (!self.mix().inputs[0].loudness.normalizer.is_enabled()
                && !self.mix().inputs[1].loudness.normalizer.is_enabled());
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
        let format_bits = output_format.bit_depth().unwrap_or(output_bit_depth);
        let bit_depth_not_truncated = source_bit_depth > 0
            && format_bits > 0
            && source_bit_depth <= format_bits
            && output_format != OutputSampleFormat::Unknown;
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

        let mut cause = None;
        let mut reason = None;
        if !volume_unity {
            cause = Some(BitPerfectCause::VolumeNotUnity);
            reason = Some("Volume / balance / preamp is not unity (0 dB)".to_string());
        } else if !eq_bypassed {
            cause = Some(BitPerfectCause::EqActive);
            reason = Some("EQ is active".to_string());
        } else if !dynamics_bypassed {
            cause = Some(BitPerfectCause::DynamicsActive);
            reason = Some("Dynamics / DSP processor is active".to_string());
        } else if !resampler_bypassed {
            cause = Some(BitPerfectCause::SpeedOrResampleActive);
            reason = Some("Resampler / speed modifier is active".to_string());
        } else if !sample_rate_matched {
            cause = Some(BitPerfectCause::SampleRateMismatch);
            reason = Some(format!(
                "Sample rate mismatch: source {} Hz != output {} Hz",
                source_sample_rate, output_sample_rate
            ));
        } else if source_bit_depth == 0
            || output_bit_depth == 0
            || output_format == OutputSampleFormat::Unknown
        {
            cause = Some(BitPerfectCause::UnknownPrecision);
            reason = Some(format!(
                "Source or output precision is unknown (output format: {}); bit-perfect cannot be proven",
                output_format.label()
            ));
        } else if !bit_depth_not_truncated {
            cause = Some(BitPerfectCause::BitDepthTruncation);
            reason = Some(format!(
                "Bit depth truncation: source {} bits > output {} bits",
                source_bit_depth, output_bit_depth
            ));
        } else if !format_conversion_lossless {
            cause = Some(BitPerfectCause::FormatConversionLossy);
            reason = Some(format!(
                "Source precision ({} bits) is not lossless for {} output",
                source_bit_depth,
                output_format.label()
            ));
        } else if !output_exclusive {
            cause = Some(BitPerfectCause::OutputNotDirectExclusive);
            reason = Some("Output is not direct/exclusive hardware".to_string());
        }

        let bit_perfect_samples = volume_unity
            && eq_bypassed
            && dynamics_bypassed
            && resampler_bypassed
            && sample_rate_matched
            && bit_depth_not_truncated
            && format_conversion_lossless;
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
            compressor_bypassed,
            dynamics_bypassed,
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
            source_channels: 0,
            output_channels: 0,
            decoder_lossless: false,
            crossfade_active: false,
            dither_active: false,
            volume_path: VolumePath::None,
            result,
            reason,
            cause,
        }
    }

    /// Output-format-aware bit-perfect report with the REAL negotiated access
    /// state (requested / actual / verified) and fallback flag — the
    /// authoritative report the engine tick publishes.
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

    /// Build the engine telemetry snapshot — the graph-side twin of the
    /// pipeline's builder.
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
            eq_enabled: self.eq().eq.is_enabled(),
            eq_auto_headroom: self.eq().eq.is_auto_headroom(),
            compressor_enabled: self.dynamics().compressor.is_enabled(),
            limiter_enabled: self.limiter().limiter.is_enabled(),
            limiter_gain_reduction_db: self.limiter_gain_reduction_db(),
            bit_perfect: bp_report.is_bit_perfect,
            bit_perfect_reason: bp_report.reason.clone(),
            bit_perfect_cause: bp_report.cause,
            bit_perfect_report: bp_report,
            output_latency_ms: latency_report.total_latency_ms,
            latency_report,
            ..Default::default()
        }
    }
}
