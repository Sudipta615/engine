//! Multi-track lane registry (Phase 4 S6).
//!
//! A lane is an independent playback stream mixed onto a mix-bus slot ≥ 2
//! (an "always-on track"). Lanes decode independently of the primary
//! stream and are fed to the graph as secondaries at each block boundary,
//! exactly like the primary's incoming stream is fed during a crossfade —
//! the graph owns the per-slot pre-mix chains, gain/pan/ducking, and
//! metering.
//!
//! Realtime contract: the decode side allocates (decoders allocate chunk
//! buffers, like the primary path); the graph-side feed is zero-alloc. Lane
//! slots are assigned from the first free bus slot ≥ 2; the bus grows on
//! demand via the glitch-free generation swap when a lane needs a slot the
//! current generation lacks.

use crate::decode::{DecodeError, Decoder};
use crate::dsp::graph::nodes::MAX_MIX_SLOTS;
use crate::source::AudioSource;

/// Maximum number of simultaneous lanes: the bus's spare slots.
pub const MAX_LANES: usize = MAX_MIX_SLOTS - 2;

/// Source frames fed to a lane's resampler per decode step. Small enough
/// that each resampler's output is drained frequently, large enough to
/// amortize the per-frame feed dispatch.
const LANE_FEED_BATCH: usize = 256;

/// A single playback lane: an independent decoder + resampler whose output
/// is mixed onto its bus slot at each block boundary. Constructed on the
/// control path (`AddTrack`); only [`Self::fill`] runs on the audio tick.
pub struct LaneTrack {
    /// Mix-bus slot (≥ 2).
    pub slot: usize,
    /// The source being played (for telemetry / re-arming).
    pub source: AudioSource,
    pub decoder: Decoder,
    #[cfg(feature = "resample")]
    pub resampler: Option<crate::dsp::resampler::GenericResampler>,
    #[cfg(not(feature = "resample"))]
    pub resampler: Option<()>,
    /// Output-domain (post-resample) frames waiting to be mixed. Preallocated
    /// at construction; the audio path only pushes within capacity.
    pub pending: std::collections::VecDeque<(f32, f32)>,
    /// Frames already mixed into the bus (for telemetry).
    pub frames_played: u64,
    /// User gain target in [0, 1] (re-applied on graph reconfig).
    pub gain: f32,
    /// User pan target in [-1, 1].
    pub pan: f32,
    /// Post-fader master-send gain in [0, 1] (Phase 5 S2).
    pub send_master_gain: f32,
    /// Post-fader aux-send gain in [0, 1] (Phase 5 S2).
    pub send_aux_gain: f32,
    /// Set on EndOfStream; the lane contributes silence until removed.
    pub finished: bool,
    /// Consecutive non-EOS decode errors (same circuit breaker as the
    /// primary path).
    pub consecutive_errors: u32,
}

impl LaneTrack {
    /// Open a lane for `source` on `slot`. Resampler construction mirrors the
    /// primary path's `build_resampler` helper. Control path.
    pub fn open(
        slot: usize,
        source: AudioSource,
        decoder: Decoder,
        #[cfg(feature = "resample")] quality: config::ResamplerQuality,
        #[cfg(feature = "resample")] output_rate: f32,
        #[cfg(feature = "resample")] speed: f32,
        #[cfg(feature = "resample")] precision: config::PrecisionMode,
    ) -> Self {
        #[cfg(feature = "resample")]
        let resampler = crate::engine::recovery::build_resampler(
            quality,
            decoder.info().sample_rate as f32,
            output_rate,
            speed,
            precision,
        );
        #[cfg(not(feature = "resample"))]
        let resampler = Some(());
        Self {
            slot,
            source,
            decoder,
            resampler,
            pending: std::collections::VecDeque::with_capacity(8192),
            frames_played: 0,
            gain: 1.0,
            pan: 0.0,
            send_master_gain: 1.0,
            send_aux_gain: 0.0,
            finished: false,
            consecutive_errors: 0,
        }
    }

    /// Bounded FIFO push (never grows past the preallocated capacity — a
    /// violation is an invariant failure, not a reason to allocate on the
    /// audio path).
    #[inline]
    fn push_pending(&mut self, frame: (f32, f32)) {
        if self.pending.len() < self.pending.capacity() {
            self.pending.push_back(frame);
        } else {
            log::error!("lane FIFO reached its realtime capacity; preserving the bound");
        }
    }

    /// Decode and resample until `pending` holds at least `need` output
    /// frames or the stream ends. The only allocations are the decoder's own
    /// chunk buffers. Mirrors the primary path's feed/drain discipline.
    pub fn fill(&mut self, need: usize, precision: config::PrecisionMode) {
        while self.pending.len() < need && !self.finished {
            match self.decoder.decode_next(LANE_FEED_BATCH) {
                Ok(chunk) => {
                    self.consecutive_errors = 0;
                    let ch = chunk.channels.max(1);
                    let frames_in_chunk = chunk.samples.len() / ch;
                    let mut idx = 0;
                    while idx < frames_in_chunk {
                        let (l, rv) = crate::engine::decode_loop::extract_stereo_frame(
                            &chunk.samples,
                            ch,
                            Some(&chunk.channel_layout),
                            idx * ch,
                        )
                        .unwrap_or((0.0, 0.0));
                        idx += 1;
                        #[cfg(feature = "resample")]
                        match &mut self.resampler {
                            Some(rs) => {
                                if precision == config::PrecisionMode::Quality {
                                    rs.feed_f64(l as f64, rv as f64);
                                } else {
                                    rs.feed_f32(l, rv);
                                }
                            }
                            None => self.push_pending((l, rv)),
                        }
                        #[cfg(not(feature = "resample"))]
                        self.push_pending((l, rv));
                    }
                    #[cfg(feature = "resample")]
                    if let Some(r) = &mut self.resampler {
                        while self.pending.len() < need {
                            let Some((ol, or_)) = r.read_f32() else {
                                break;
                            };
                            // Direct field access keeps `resampler` and
                            // `pending` as disjoint borrows of `self`.
                            if self.pending.len() < self.pending.capacity() {
                                self.pending.push_back((ol, or_));
                            }
                        }
                    }
                }
                Err(DecodeError::EndOfStream) => {
                    self.finished = true;
                    // Flush the resampler's tail into the FIFO so the last
                    // frames aren't lost.
                    #[cfg(feature = "resample")]
                    if let Some(r) = &mut self.resampler {
                        r.flush();
                        while let Some((ol, or_)) = r.read_f32() {
                            if self.pending.len() < self.pending.capacity() {
                                self.pending.push_back((ol, or_));
                            }
                        }
                    }
                }
                Err(_e) => {
                    self.consecutive_errors += 1;
                    if self.consecutive_errors >= 50 {
                        log::error!("Lane hit its decode error limit; marking finished");
                        self.finished = true;
                    }
                }
            }
        }
    }
}

impl super::AudioEngine {
    /// Decode each active lane until its pending FIFO holds at least `need`
    /// output frames, then copy the first `need` into the lane scratch
    /// buffers (zero-padded at startup/EOS so the bus never sees a gap).
    /// Returns the number of lanes fed (bounded by [`MAX_LANES`]). Audio
    /// tick path; the only allocations are the decoders' own chunk buffers.
    pub(super) fn fill_lane_scratch(&mut self, need: usize) -> usize {
        if need == 0 {
            return 0;
        }
        let precision = self.config.precision_mode;
        let mut count = 0usize;
        for idx in 0..self.lanes.len().min(MAX_LANES) {
            let lane = &mut self.lanes[idx];
            lane.fill(need, precision);
            let n_avail = lane.pending.len().min(need);
            let scratch_l = &mut self.scratch.lane_l[idx];
            let scratch_r = &mut self.scratch.lane_r[idx];
            for i in 0..n_avail {
                let Some((l, r)) = lane.pending.pop_front() else {
                    break;
                };
                scratch_l[i] = l;
                scratch_r[i] = r;
            }
            for i in n_avail..need {
                scratch_l[i] = 0.0;
                scratch_r[i] = 0.0;
            }
            lane.frames_played += need as u64;
            count += 1;
        }
        count
    }

    /// Find the first free bus slot ≥ 2, or `None` when the bus is full.
    pub(super) fn next_lane_slot(&self) -> Option<usize> {
        let mut used = [false; MAX_LANES + 2];
        for lane in &self.lanes {
            if lane.slot < used.len() {
                used[lane.slot] = true;
            }
        }
        (2..MAX_LANES + 2).find(|&s| !used[s])
    }
}
