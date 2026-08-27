//! Multi-endpoint output routing (roadmap Phase 5: the routing matrix).
//!
//! The engine produces ONE master stereo mix in the primary endpoint's rate
//! domain (the decode loop runs the graph chain + the primary final
//! limiter). [`EndpointTransport`] carries that master block to one
//! additional output device:
//!
//! - its own lock-free output ring (the SPSC boundary the backend's
//!   realtime callback drains — the same pattern as the primary
//!   `output_buffer`);
//! - a resampler into the endpoint's own rate domain (`None` when the
//!   rates match: pass-through);
//! - its own final safety limiter, sized for the endpoint rate
//!   (lookahead/attack/release are frame counts at that rate), applied to
//!   the resampled frames only — same-rate endpoints reuse the master's
//!   already-limited block untouched;
//! - a per-endpoint level (`gain`) applied after resampling.
//!
//! The fan-out runs on the decode loop (a control thread, never a realtime
//! callback), so the per-endpoint chain may use the same bounded, prebuilt
//! DSP objects the decode loops already use. Clock drift between
//! independent devices is deliberately NOT corrected here: each endpoint
//! resamples against its own nominal clock, which is the correct first-cut
//! contract (drift correction is a dedicated follow-up).

use std::collections::VecDeque;
use std::sync::Arc;

use config::{EndpointConfig, PrecisionMode, ResamplerQuality};

use crate::buffer::{FixedFrameBuffer, OUTPUT_BUFFER_FRAMES};
use crate::dsp::limiter::LookaheadLimiter;
use crate::dsp::resampler::{GenericResampler, ResamplerError};
use crate::output::{Output, OutputError};

/// Maximum frames an endpoint may buffer ahead of its ring (resampler +
/// limiter output not yet accepted). Bounded so a stuck endpoint can never
/// grow the decode loop's memory; the oldest frames are dropped first.
pub const MAX_ENDPOINT_PENDING_FRAMES: usize = OUTPUT_BUFFER_FRAMES * 2;

/// One additional output endpoint: its own ring + backend, plus the
/// per-endpoint resampler and final limiter that bring the master block
/// (already output-domain at the primary rate) into this endpoint's rate
/// domain.
pub struct EndpointTransport {
    pub(crate) config: EndpointConfig,
    pub(crate) output: Box<dyn Output>,
    /// The endpoint's SPSC ring: the decode loop pushes, the backend's
    /// realtime callback drains.
    pub(crate) ring: Arc<FixedFrameBuffer>,
    /// Resamples the master-rate block to this endpoint's rate. `None` when
    /// the rates match (pass-through).
    pub(crate) resampler: Option<GenericResampler>,
    /// Final safety limiter in the endpoint's output domain. Only processes
    /// resampled frames (same-rate endpoints reuse the master's limited
    /// block unchanged).
    pub(crate) limiter: LookaheadLimiter,
    /// Endpoint sample rate (the device's negotiated rate).
    pub(crate) rate: u32,
    /// Per-endpoint level in [0, 1] applied after resampling.
    pub(crate) gain: f32,
    /// Resampler/limiter output not yet accepted by the ring, in
    /// chronological order (partial-write preservation, mirroring the
    /// engine's `pending_output_frames`).
    pub(crate) pending: VecDeque<(f32, f32)>,
}

impl EndpointTransport {
    /// Open an endpoint transport: build the resampler (master → endpoint
    /// rate, when they differ) and the rate-matched final limiter. The
    /// caller owns the ring and hands it to BOTH the backend
    /// (`create_output`) and the transport, so the backend drains exactly
    /// what the transport feeds. Control path — allocation is legal here;
    /// the audio-side feed is allocation-free. `limiter_cfg` is the
    /// engine's master limiter settings, re-instantiated at the endpoint's
    /// rate.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        config: EndpointConfig,
        output: Box<dyn Output>,
        ring: Arc<FixedFrameBuffer>,
        master_rate: u32,
        resampler_quality: ResamplerQuality,
        precision: PrecisionMode,
        limiter_cfg: &config::LimiterConfig,
    ) -> Result<Self, OutputError> {
        let rate = output.sample_rate();
        let resampler = if rate != master_rate && master_rate > 0 {
            match precision {
                PrecisionMode::Performance => crate::dsp::resampler::AudioResampler::<f32>::new(
                    resampler_quality,
                    master_rate as f32,
                    rate as f32,
                )
                .map(GenericResampler::F32),
                PrecisionMode::Quality => crate::dsp::resampler::AudioResampler::<f64>::new(
                    resampler_quality,
                    master_rate as f32,
                    rate as f32,
                )
                .map(GenericResampler::F64),
            }
            .map_err(endpoint_resampler_error)?
            .into()
        } else {
            None
        };
        let limiter = LookaheadLimiter::new_with_params(
            rate as f32,
            limiter_cfg.lookahead_ms,
            limiter_cfg.attack_ms,
            limiter_cfg.release_ms,
            limiter_cfg.ceiling_db,
            limiter_cfg.soft_clip,
        );
        Ok(Self {
            config,
            output,
            ring,
            resampler,
            limiter,
            rate,
            gain: 1.0,
            pending: VecDeque::new(),
        })
    }

    /// Test/headless constructor: `rate` is the endpoint's sample rate.
    #[cfg(test)]
    pub(crate) fn open_for_test(
        config: EndpointConfig,
        output: Box<dyn Output>,
        master_rate: u32,
        quality: ResamplerQuality,
        precision: PrecisionMode,
        limiter_cfg: &config::LimiterConfig,
    ) -> Self {
        let ring = Arc::new(FixedFrameBuffer::new(OUTPUT_BUFFER_FRAMES).expect("endpoint ring"));
        Self::open(
            config,
            output,
            ring,
            master_rate,
            quality,
            precision,
            limiter_cfg,
        )
        .expect("endpoint transport")
    }

    /// Feed one master-domain stereo frame into the endpoint's chain
    /// (resample → endpoint limiter → gain). Appends to `pending`; call
    /// [`Self::drain`] to flush to the ring. Allocation-free.
    pub(crate) fn feed_frame(&mut self, l: f32, r: f32) {
        if let Some(rs) = &mut self.resampler {
            rs.feed_f32(l, r);
            while let Some((ol, or_)) = rs.read_f32() {
                let (ol, or_) = self.limiter.process(ol, or_);
                self.pending.push_back((ol * self.gain, or_ * self.gain));
            }
        } else if self.gain == 1.0 {
            self.pending.push_back((l, r));
        } else {
            self.pending.push_back((l * self.gain, r * self.gain));
        }
        self.trim_pending();
    }

    /// Feed a whole master-domain block. Allocation-free.
    pub(crate) fn feed_block(&mut self, left: &[f32], right: &[f32]) {
        let n = left.len().min(right.len());
        for i in 0..n {
            self.feed_frame(left[i], right[i]);
        }
    }

    /// Bound the pending queue: a stuck endpoint must not grow memory; the
    /// oldest frames are dropped first (the endpoint already underran).
    fn trim_pending(&mut self) {
        if self.pending.len() > MAX_ENDPOINT_PENDING_FRAMES {
            let over = self.pending.len() - MAX_ENDPOINT_PENDING_FRAMES;
            self.pending.drain(..over);
        }
    }

    /// Flush as many pending frames as the ring accepts, in chronological
    /// order. Returns the number of frames written.
    pub(crate) fn drain(&mut self) -> usize {
        let mut written = 0usize;
        let mut batch = [0.0f32; 512 * 2];
        loop {
            let len = self.pending.len();
            if len == 0 {
                break;
            }
            let m = len.min(512);
            for (k, (l, r)) in self.pending.iter().take(m).enumerate() {
                batch[k * 2] = *l;
                batch[k * 2 + 1] = *r;
            }
            let accepted = self.ring.push_block_interleaved(&batch[..m * 2]);
            let frames = accepted / 2;
            if frames == 0 {
                break;
            }
            self.pending.drain(..frames);
            written += frames;
            if frames < m {
                break;
            }
        }
        written
    }

    /// Number of frames buffered ahead of the ring (for telemetry).
    pub fn pending_frames(&self) -> usize {
        self.pending.len()
    }
}

fn endpoint_resampler_error(e: ResamplerError) -> OutputError {
    OutputError::StreamError(format!("endpoint resampler: {e}"))
}

impl super::AudioEngine {
    /// Fan a master-domain stereo block out to every additional endpoint
    /// (resample → endpoint limiter → gain → ring). Decode-loop path;
    /// allocation-free. A missing/skipped endpoint feeds nothing.
    pub(crate) fn fanout_endpoint_block(&mut self, left: &[f32], right: &[f32]) {
        fanout_block(&mut self.extra_endpoints, left, right);
    }

    /// Fan one master-domain stereo frame out to every additional endpoint
    /// (the per-frame resampled path). The flush to each endpoint's ring
    /// happens in [`Self::drain_endpoints`], called at the batch drain
    /// points. Allocation-free.
    pub(crate) fn fanout_endpoint_frame(&mut self, l: f32, r: f32) {
        for ep in &mut self.extra_endpoints {
            ep.feed_frame(l, r);
        }
    }

    /// Flush every endpoint's pending frames to its ring.
    pub(crate) fn drain_endpoints(&mut self) {
        drain_endpoints(&mut self.extra_endpoints);
    }
}

/// Free-function form so the crossfade flush can feed `self.scratch.mix_*`
/// while borrowing `self.extra_endpoints` mutably (disjoint fields).
pub(crate) fn fanout_block(endpoints: &mut [EndpointTransport], left: &[f32], right: &[f32]) {
    if endpoints.is_empty() {
        return;
    }
    for ep in endpoints {
        ep.feed_block(left, right);
        ep.drain();
    }
}

/// Free-function form of [`EndpointTransport::drain`] over a slice.
pub(crate) fn drain_endpoints(endpoints: &mut [EndpointTransport]) {
    for ep in endpoints {
        ep.drain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::LimiterConfig;

    use crate::engine::tests::FakeEndpointOutput;

    fn limiter_cfg() -> LimiterConfig {
        LimiterConfig::default()
    }

    fn make(rate: u32, master: u32) -> EndpointTransport {
        let config = EndpointConfig {
            device: "fake-endpoint".to_string(),
            ..EndpointConfig::default()
        };
        let out = FakeEndpointOutput { rate };
        EndpointTransport::open_for_test(
            config,
            Box::new(out),
            master,
            ResamplerQuality::Balanced,
            PrecisionMode::Performance,
            &limiter_cfg(),
        )
    }

    #[test]
    fn same_rate_endpoint_passes_through_gain() {
        let mut ep = make(44_100, 44_100);
        ep.gain = 0.5;
        let n = 1000;
        let left = vec![0.25f32; n];
        let right = vec![-0.25f32; n];
        ep.feed_block(&left, &right);
        ep.drain();
        assert!(ep.pending.is_empty(), "same-rate: no resampler latency");
        // Without a started backend nobody drains the ring; pop it ourselves.
        let mut out = vec![0.0f32; n * 2];
        let got = ep.ring.pop_block_interleaved(&mut out);
        assert_eq!(got, n * 2, "all frames accepted by the ring");
        // Master block was already limited; the endpoint applies gain only.
        for i in 0..n {
            assert!((out[i * 2] - 0.125).abs() < 1e-6, "L at {i}");
            assert!((out[i * 2 + 1] + 0.125).abs() < 1e-6, "R at {i}");
        }
    }

    #[test]
    fn rate_mismatch_resamples_and_limits() {
        let master = 44_100u32;
        let endpoint_rate = 48_000u32;
        let mut ep = make(endpoint_rate, master);
        assert!(ep.resampler.is_some(), "rate mismatch builds a resampler");

        // ~0.09 s of a 0.5-amplitude 440 Hz sine at the master rate. The
        // chunked FFT resampler holds partial chunks internally (and its
        // filter has a long startup), so the emitted count is
        // chunk-quantized; the assertions below check pitch/peak/finiteness
        // over the steady-state region rather than an exact length.
        let n = 4000usize;
        let left: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / master as f32).sin() * 0.5)
            .collect();
        ep.feed_block(&left, &left);
        ep.drain();

        let mut out = vec![0.0f32; 8192 * 2];
        let got = ep.ring.pop_block_interleaved(&mut out);
        let frames = got / 2;
        let expect = (n as f64 * endpoint_rate as f64 / master as f64) as usize;
        assert!(
            frames >= expect / 2 && frames <= expect + 2048,
            "resampled frame count {frames} ≈ {expect}"
        );
        assert!(
            out[..frames * 2].iter().any(|&v| v.abs() > 1e-3),
            "resampled output is silence"
        );

        // Content sanity over the steady-state region only: the filter's
        // startup (~first 1600 frames) rings with extra zero crossings.
        let mut peak = 0.0f32;
        let mut zero_crossings = 0usize;
        let mut prev: Option<f32> = None;
        let start = 1600.min(frames);
        for i in start..frames {
            let v = out[i * 2];
            assert!(v.is_finite());
            peak = peak.max(v.abs());
            if let Some(p) = prev {
                if (p < 0.0 && v >= 0.0) || (p >= 0.0 && v < 0.0) {
                    zero_crossings += 1;
                }
            }
            prev = Some(v);
        }
        assert!(
            (peak - 0.5).abs() < 0.05,
            "resampled peak ≈ 0.5, got {peak}"
        );
        let seconds = (frames - start) as f32 / endpoint_rate as f32;
        if seconds > 0.02 {
            let hz = zero_crossings as f32 / (2.0 * seconds);
            assert!(
                (hz - 440.0).abs() < 60.0,
                "resampled tone ≈ 440 Hz, got {hz}"
            );
        }
    }

    #[test]
    fn partial_ring_write_preserves_chronological_pending() {
        let mut ep = make(44_100, 44_100); // Fill the physical ring completely (the backing store reserves
                                           // frames × MAX_CHANNELS(16) samples = 131072 samples here).
        let filler = vec![0.0f32; 131_072];
        let accepted = ep.ring.push_block_interleaved(&filler);
        assert_eq!(accepted, 131_072, "ring physically filled");
        let probe = vec![0.5f32; 512 * 2];
        assert_eq!(
            ep.ring.push_block_interleaved(&probe),
            0,
            "no room after the fill"
        );
        let n = 512;
        let left = vec![0.3f32; n];
        let right = vec![0.3f32; n];
        ep.feed_block(&left, &right);
        let written = ep.drain();
        assert_eq!(written, 0, "ring full: nothing accepted");
        assert_eq!(ep.pending.len(), n, "all frames stay pending");
        // Free room and drain again: pending flushes in order.
        let mut drain = vec![0.0f32; 4096];
        ep.ring.pop_block_interleaved(&mut drain);
        let written = ep.drain();
        assert!(written > 0);
        assert!(ep.pending.is_empty(), "pending fully flushed");
    }

    #[test]
    fn pending_queue_is_bounded_for_stuck_endpoint() {
        let mut ep = make(44_100, 44_100);
        // Feed far more than the bound without ever draining.
        let n = MAX_ENDPOINT_PENDING_FRAMES * 3;
        let left = vec![0.2f32; n];
        let right = vec![0.2f32; n];
        ep.feed_block(&left, &right);
        assert!(
            ep.pending.len() <= MAX_ENDPOINT_PENDING_FRAMES,
            "pending bounded ({} ≤ {})",
            ep.pending.len(),
            MAX_ENDPOINT_PENDING_FRAMES
        );
    }
}
